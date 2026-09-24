//! Step 5 批5：A/B/C 三档评测运行器（spec `step5-qug-build.md` §3 D6/D7、
//! §4.3 末段、§7 A10–A12）。
//! Step 5 batch 5: the A/B/C three-tier evaluation runner (spec
//! `step5-qug-build.md` §3 D6/D7, §4.3 last paragraph, §7 A10–A12).
//!
//! 三档定义（D6）：
//! - **A = 纯 FTS5 BM25**：引擎 `qug=None` + `fts_only=true`（跳过整条向量路；
//!   RRF 的 `1/(k+r)` 对名次单调，最终排序等价 BM25 名次，tie-break page_id）；
//! - **B = FTS+Vector+RRF**（Step3 混合）：`qug=None` + `fts_only=false`；
//! - **C = 启用 QUG**：图**直调** [`load_active_qug`] 取得（§4.3：eval 必须直调
//!   并对任何错误失败，绝不走吸收错误的 `reload_persistent_qug`），随后
//!   rewrite + 过滤下推走同一混合检索；QUG 无匹配显式 fallback（计入
//!   fallback_count）。
//!
//! Tier definitions (D6):
//! - **A = pure FTS5 BM25**: engine `qug=None` + `fts_only=true` (the whole
//!   vector path is skipped; RRF's `1/(k+r)` is monotonic in the rank, so the
//!   final order equals the BM25 ranking with a deterministic page_id
//!   tie-break);
//! - **B = FTS+Vector+RRF** (Step3 hybrid): `qug=None` + `fts_only=false`;
//! - **C = QUG enabled**: the graph comes from a **direct** [`load_active_qug`]
//!   call (§4.3: eval must call it directly and fail on any error, never via
//!   the error-absorbing `reload_persistent_qug`), then rewrite + filter
//!   pushdown feed the same hybrid retrieval; a QUG miss falls back explicitly
//!   (counted in fallback_count).
//!
//! 公平性契约：三档共用同一 `Arc<SqliteKernel>`（同一数据库快照、同一
//! accepted 代次、同一 facts）、同一 VectorStore 实例（同一向量输入）、同一
//! embedder、同一 candidate_multiplier 与 rrf_k；仅 QUG 与向量路开关不同。
//! Fairness contract: all tiers share one `Arc<SqliteKernel>` (one database
//! snapshot, one accepted generation, one fact plane), one VectorStore instance
//! (one vector input), one embedder, and one candidate_multiplier/rrf_k; only
//! the QUG and vector-path switches differ.
//!
//! 失败语义（§6）：单条 query 错误不吞掉——继续跑完剩余样本后汇总为运行失败
//! （[`Error::Query`] 列出全部失败样本）；`load_active_qug` 的任何错误（stale/
//! 损坏/数据库）立即失败。`Ok(None)`（无 active 构建）不是错误：C 以 `qug=None`
//! 运行并按 D6 视为无增益 → disabled。
//! Failure semantics (§6): per-query errors are never swallowed — the remaining
//! samples still run, then everything is summarized into one run failure
//! ([`Error::Query`] listing every failed sample); any `load_active_qug` error
//! (stale/corrupt/database) fails immediately. `Ok(None)` (no active build) is
//! not an error: C runs with `qug=None` and, per D6, counts as no gain →
//! disabled.

use crate::eval::metrics::{decide, evaluate_variant, Decision, SampleRun, VariantMetrics};
use crate::eval::{GoldenFilterCondition, GoldenFilters, GoldenQuery, GoldenSet};
use crate::kernel::qug_store::{load_active_qug, QugStore};
use crate::kernel::SqliteKernel;
use crate::query_engine::hybrid::RRF_K_DEFAULT;
use crate::query_engine::QueryEmbedder;
use crate::traits::{DomainConfig, VectorStore};
use crate::types::error::{Error, Result};
use crate::types::{FilterCondition, Filters, Query};
use std::sync::Arc;

/// 三档评测输入配置（报告契约里的 vector_backend / 运行命令由此带入）。
/// Evaluation input config (vector_backend / run command of the report
/// contract come from here).
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// 评测 top-k（D7：默认 10，范围 10..=100；@1/@5/@10 取其前缀）。
    /// Evaluation top-k (D7: default 10, range 10..=100; @1/@5/@10 are
    /// prefixes of it).
    pub top_k: usize,
    pub rrf_k: u32,
    /// 向量 collection 名（三档一致）。
    /// Vector collection name (identical across tiers).
    pub collection: String,
    /// 向量后端标签（"mock" / "qdrant"），仅入报告。
    /// Vector-backend label ("mock" / "qdrant"), report-only.
    pub vector_backend: String,
    /// 触发本次评测的命令行（报告审计字段）。
    /// The command line that triggered this evaluation (report audit field).
    pub command: String,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            top_k: crate::eval::EVAL_TOP_K_DEFAULT,
            rrf_k: RRF_K_DEFAULT,
            collection: "milk-tea".into(),
            vector_backend: "mock".into(),
            command: String::new(),
        }
    }
}

/// 一次评测的完整产物（报告模型 [`crate::eval::EvaluationReport`] 的输入）。
/// The complete product of one evaluation (input to the report model
/// [`crate::eval::EvaluationReport`]).
#[derive(Debug, Clone)]
pub struct EvalOutcome {
    pub eval_top_k: usize,
    pub tier_a: VariantMetrics,
    pub tier_b: VariantMetrics,
    pub tier_c: VariantMetrics,
    pub decision: Decision,
    /// active published 构建的 source_hash；无 active 时 None（D2）。
    /// The active published build's source_hash; None without an active build
    /// (D2).
    pub source_hash: Option<String>,
    pub golden_total: usize,
    /// golden 集按 kind 计数（报告概览用）。
    /// Golden per-kind counts (report overview).
    pub kind_counts: std::collections::BTreeMap<String, usize>,
}

/// golden `filters` → 事实平面 [`Filters`]（**逐条透传字段名**，不再硬编码
/// price→price / sugar→sugar_level / ingredients→ingredient_ids 映射，STEP10
/// D3；tech-docs 的 topic/level/format/audience_years/tags 同样适用）。
/// Golden `filters` → fact-plane [`Filters`] (**field names pass through**; the
/// price→price / sugar→sugar_level / ingredients→ingredient_ids mapping is
/// gone, STEP10 D3; tech-docs's topic/level/format/audience_years/tags work too).
pub fn golden_filters_to_filters(f: &GoldenFilters) -> Filters {
    Filters {
        conditions: f
            .conditions
            .iter()
            .map(|c| match c {
                GoldenFilterCondition::NumericRange { field, min, max } => {
                    FilterCondition::NumericRange {
                        field: field.clone(),
                        min: *min,
                        max: *max,
                    }
                }
                GoldenFilterCondition::TextEquals { field, value } => FilterCondition::TextEquals {
                    field: field.clone(),
                    value: value.clone(),
                },
                GoldenFilterCondition::RefContains { field, refs } => {
                    FilterCondition::RefContains {
                        field: field.clone(),
                        refs: refs.clone(),
                    }
                }
                GoldenFilterCondition::RefExcludes { field, refs } => {
                    FilterCondition::RefExcludes {
                        field: field.clone(),
                        refs: refs.clone(),
                    }
                }
            })
            .collect(),
    }
}

/// 执行三档评测（spec §8 批5；不写 CLI，CLI 接线属批6）。
/// Runs the three-tier evaluation (spec §8 batch 5; no CLI here — CLI wiring
/// belongs to batch 6).
///
/// 顺序固定 A → B → C；三档共用同一快照与依赖实例，仅在 QUG 与向量路开关上
/// 不同（D6 公平性契约）。
/// Fixed order A → B → C; the tiers share one snapshot and one set of
/// dependency instances, differing only in the QUG and vector-path switches
/// (the D6 fairness contract).
pub async fn run_evaluation<V: VectorStore>(
    kernel: Arc<SqliteKernel>,
    vector_store: Arc<V>,
    embedder: Arc<dyn QueryEmbedder>,
    domain: &DomainConfig,
    intents_bytes: &[u8],
    golden: &GoldenSet,
    config: &EvalConfig,
) -> Result<EvalOutcome> {
    // D7：top_k 范围 10..=100；越界 = 用法/配置错误（Validation）。
    // D7: top_k range 10..=100; out of range = a usage/config error
    // (Validation).
    if !(10..=100).contains(&config.top_k) {
        return Err(Error::Validation(format!(
            "eval top_k {} out of range 10..=100 (D7)",
            config.top_k
        )));
    }

    let multiplier = domain.qug.candidate_multiplier;
    let collection = config.collection.clone();

    // —— A 档：纯 FTS5 BM25（qug=None + fts_only=true）。
    // —— Tier A: pure FTS5 BM25 (qug=None + fts_only=true).
    let mut engine_a = crate::query_engine::QueryEngine::new(
        kernel.clone(),
        vector_store.clone(),
        None,
        embedder.clone(),
        collection.clone(),
        multiplier,
        config.rrf_k,
    )?;
    engine_a.fts_only = true;
    let samples_a = run_tier(&engine_a, domain, golden, config.top_k).await?;
    let tier_a = evaluate_variant(&samples_a, config.top_k);

    // —— B 档：Step3 混合（qug=None + 向量路开启）。
    // —— Tier B: the Step3 hybrid (qug=None + vector path on).
    let engine_b = crate::query_engine::QueryEngine::new(
        kernel.clone(),
        vector_store.clone(),
        None,
        embedder.clone(),
        collection.clone(),
        multiplier,
        config.rrf_k,
    )?;
    let samples_b = run_tier(&engine_b, domain, golden, config.top_k).await?;
    let tier_b = evaluate_variant(&samples_b, config.top_k);

    // —— C 档：QUG 启用。图直调 load_active_qug（§4.3 末段）：任何错误
    //    （stale/损坏/Internal）立即运行失败，绝不吸收为 disabled；Ok(None)
    //    （无 active 构建）不是错误——C 以 qug=None 运行，D6 视为无增益。
    // —— Tier C: QUG enabled. The graph is loaded via a direct
    //    load_active_qug call (§4.3 last paragraph): any error
    //    (stale/corrupt/Internal) fails the run immediately and is never
    //    absorbed into disabled; Ok(None) (no active build) is not an error —
    //    C runs with qug=None and counts as no gain per D6.
    let graph = load_active_qug(&kernel, domain, intents_bytes)?;
    let source_hash = kernel
        .active_build_identity(&domain.name, &domain.version)?
        .map(|(_, hash)| hash);
    let engine_c = crate::query_engine::QueryEngine::new(
        kernel,
        vector_store,
        graph,
        embedder,
        collection,
        multiplier,
        config.rrf_k,
    )?;
    let samples_c = run_tier(&engine_c, domain, golden, config.top_k).await?;
    let tier_c = evaluate_variant(&samples_c, config.top_k);
    let active_qug_samples = samples_c.iter().filter(|s| s.rewrite_applied).count();
    let has_active_graph = engine_c.qug.is_some();
    let decision = decide(&tier_b, &tier_c, has_active_graph, active_qug_samples);

    Ok(EvalOutcome {
        eval_top_k: config.top_k,
        tier_a,
        tier_b,
        tier_c,
        decision,
        source_hash,
        golden_total: golden.len(),
        kind_counts: golden.kind_counts().clone(),
    })
}

/// 单档执行：逐条 golden 查询走引擎全链路；单条错误收集后汇总为运行失败。
/// One tier's execution: every golden query runs the full engine pipeline;
/// per-query errors are collected and summarized into one run failure.
async fn run_tier<V: VectorStore>(
    engine: &crate::query_engine::QueryEngine<V>,
    domain: &DomainConfig,
    golden: &GoldenSet,
    top_k: usize,
) -> Result<Vec<SampleRun>> {
    let mut samples = Vec::with_capacity(golden.len());
    let mut failures: Vec<(String, String)> = Vec::new();
    for q in golden.queries() {
        match run_sample(engine, domain, q, top_k).await {
            Ok(sample) => samples.push(sample),
            Err(e) => failures.push((q.id.clone(), e.to_string())),
        }
    }
    if failures.is_empty() {
        return Ok(samples);
    }
    // §6：单条 query error 不吞掉，汇总为运行失败（Error），不得伪装成
    // disabled；错误清单带样本 id 便于定位。
    // §6: per-query errors are never swallowed and summarize into one run
    // failure (Error), never disguised as disabled; the list carries sample
    // ids for triage.
    let listed: String = failures
        .iter()
        .map(|(id, err)| format!("\n  - {id}: {err}"))
        .collect();
    Err(Error::Query(format!(
        "eval: {} of {} golden queries failed:{}",
        failures.len(),
        golden.len(),
        listed
    )))
}

/// 单样本执行：golden 记录 → [`Query`] → 引擎检索 → [`SampleRun`]。
/// One sample: golden record → [`Query`] → engine search → [`SampleRun`].
async fn run_sample<V: VectorStore>(
    engine: &crate::query_engine::QueryEngine<V>,
    domain: &DomainConfig,
    q: &GoldenQuery,
    top_k: usize,
) -> Result<SampleRun> {
    let query = Query {
        text: q.query.clone(),
        filters: golden_filters_to_filters(&q.filters),
        top_k,
        domain: Some(domain.name.clone()),
    };
    let result = engine.search(&query).await?;
    Ok(SampleRun {
        query_id: q.id.clone(),
        kind: q.kind,
        expected: q.expected_entity_ids.clone(),
        must_exclude: q.must_exclude_entity_ids.clone(),
        hit_entity_ids: result.hits.iter().map(|h| h.entity_id.to_key()).collect(),
        rewrite_failure: result.rewrite_failure,
        rewrite_applied: result.diagnostics.rewrite_status
            == crate::query_engine::RewriteStatus::Applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{GoldenKind, QugDecision};
    use crate::kernel::qug_store::build_and_publish_qug;
    use crate::kernel::MockVectorStore;
    use crate::query_engine::qug::qug_build::{parse_intents, QugBuildOutcome};
    use crate::seed;
    use crate::traits::{DistanceMetric, EntityStore, VectorMetadata, VectorPoint};
    use crate::types::{EntityId, FactValue, Facts, PublishStatus};
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    /// 测试向量维度（仅 fixture；不宣称语义质量）。
    /// Test vector dimension (fixture only; no semantic-quality claims).
    const DIM: usize = 8;
    const COLLECTION: &str = "milk-tea";

    /// intents.yaml 原文：少糖 → sugar_level ≤ 30；不要珍珠 → 排除珍珠。
    /// Raw intents.yaml: 少糖 → sugar_level ≤ 30; 不要珍珠 → exclude pearl.
    const INTENTS_YAML: &str = r#"version: "0.1.0"
intents:
  - id: low_sugar
    phrases: ["少糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
"#;

    /// fixture 嵌入器：文本 → one-hot（索引 = Unicode 字符数 % DIM）。
    /// 确定性且测试可手算：与 point 同索引 → 余弦 1.0，否则 0.0。
    /// Fixture embedder: text → one-hot (index = Unicode char count % DIM).
    /// Deterministic and hand-computable: same index as a point → cosine 1.0,
    /// else 0.0.
    struct CountingEmbedder;

    #[async_trait]
    impl QueryEmbedder for CountingEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let mut vec = vec![0.0_f32; DIM];
            vec[text.chars().count() % DIM] = 1.0;
            Ok(vec)
        }
    }

    /// 错误注入嵌入器：文本含 "boom" 即报错（单条 query 错误冒泡测试）。
    /// Error-injecting embedder: fails when the text contains "boom" (the
    /// per-query error bubbling test).
    struct BoomEmbedder;

    #[async_trait]
    impl QueryEmbedder for BoomEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            if text.contains("boom") {
                return Err(Error::Query("embedder exploded".into()));
            }
            CountingEmbedder.embed(text).await
        }
    }

    fn fixture_config() -> DomainConfig {
        let yaml = r#"name: milk-tea
version: "0.1.0"
entities:
  - name: drink
    source: jsonl://fixture
    id_field: id
    type_field: type
    fields:
      - name: sugar_level
        field_type: numeric
        filterable: true
      - name: ingredient_ids
        field_type: reflist
        filterable: true
query:
  filters: [sugar_level, ingredient_ids]
qug:
  enabled: true
  max_depth: 2
  candidate_multiplier: 5
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    /// fixture 页面清单：(name, title, aliases, tags, 正文)。title 字符数决定
    /// 向量索引（珍珠奶茶/芋头奶茶/芝士奶茶 = 4，柠檬茶 = 3）；boba 的 alias
    /// 波霸是 C 档同义改写的 QUG 边来源。
    /// Fixture pages: (name, title, aliases, tags, body). The title char count
    /// decides the vector index (珍珠奶茶/芋头奶茶/芝士奶茶 = 4, 柠檬茶 = 3);
    /// boba's alias 波霸 feeds the tier-C synonym-rewrite QUG edge.
    const PAGES: [(&str, &str, &str, &str, &str); 4] = [
        (
            "boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
            "珍珠奶茶是经典饮品，少糖也好喝。",
        ),
        ("taro", "芋头奶茶", "[]", "[奶茶]", "芋头奶茶香浓顺滑。"),
        ("lemon", "柠檬茶", "[]", "[果茶]", "柠檬茶清爽解腻。"),
        ("cheese", "芝士奶茶", "[]", "[奶茶]", "芝士奶茶绵密咸香。"),
    ];

    /// seed 一页 accepted 页，返回 (page_id, content_hash)（与 seed_pages 同式
    /// 的 BLAKE3，向量点 metadata 用，绕过 pub(super) 的 lock_conn）。
    /// Seeds one accepted page and returns (page_id, content_hash) — the same
    /// BLAKE3 formula as seed_pages, used by the vector-point metadata because
    /// lock_conn is pub(super).
    fn seed_page(
        kernel: &SqliteKernel,
        name: &str,
        title: &str,
        aliases: &str,
        tags: &str,
        content: &str,
    ) -> (String, String) {
        let page_id = format!("milk-tea:drink:{name}");
        let md = format!(
            "---\npage_id: {page_id}\nentity_id: {page_id}\n\
             entity_type: drink\ntitle: {title}\naliases: {aliases}\ntags: {tags}\n---\n\n{content}\n\n## 概述\n\n- 茶底\n"
        );
        let page = seed::parse_page(&md).unwrap();
        // content_hash 与 kernel::seed_pages 完全同式（title\0content）。
        // content_hash mirrors kernel::seed_pages exactly (title\0content).
        let content_hash = blake3::hash(format!("{}\0{}", page.title, page.content).as_bytes())
            .to_hex()
            .to_string();
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();
        (page_id, content_hash)
    }

    /// 为一个 drink 写一条 SKU 事实（category 锚点 + 过滤字段）。
    /// Writes one SKU fact row for a drink (category anchor + filter fields).
    async fn upsert_sku(
        kernel: &SqliteKernel,
        name: &str,
        category: &str,
        sugar: f64,
        ingredients: &[&str],
    ) {
        let mut fields: BTreeMap<String, FactValue> = BTreeMap::new();
        fields.insert("category".into(), FactValue::Text(category.into()));
        fields.insert("sugar_level".into(), FactValue::Numeric(sugar));
        fields.insert(
            "ingredient_ids".into(),
            FactValue::RefList(ingredients.iter().map(|s| s.to_string()).collect()),
        );
        let entity = EntityId::new("milk-tea", "sku", name).unwrap();
        let facts = Facts {
            entity_id: entity.clone(),
            fields,
            source_revision: 1,
        };
        kernel.upsert_facts(&entity, &facts, 1).await.unwrap();
    }

    /// 组装共享快照（页面 + 事实 + 向量点）并可发布 QUG 构建；返回 kernel 与
    /// 向量库（三档共用同一实例 = 同一快照/同一向量输入）。boba 故意**没有**
    /// 向量点：向量索引子集是合法状态，并让 B 档无法靠向量命中"波霸"。
    /// Assembles the shared snapshot (pages + facts + vector points), optionally
    /// publishing the QUG build; returns the kernel and vector store (all three
    /// tiers share the instances = one snapshot / one vector input). boba
    /// deliberately has **no** vector point: a partial vector index is a legal
    /// state, and it stops tier B from recalling 波霸 via vectors.
    async fn fixture(publish_qug: bool) -> (Arc<SqliteKernel>, Arc<MockVectorStore>) {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let mut points: Vec<VectorPoint> = Vec::new();
        for (name, title, aliases, tags, content) in PAGES {
            let (page_id, content_hash) = seed_page(&kernel, name, title, aliases, tags, content);
            if name == "boba" {
                continue; // boba 无向量点 / boba has no vector point
            }
            let mut vector = vec![0.0_f32; DIM];
            vector[title.chars().count() % DIM] = 1.0;
            points.push(VectorPoint {
                id: page_id.clone(),
                vector,
                metadata: VectorMetadata {
                    entity_id: page_id.clone(),
                    page_id,
                    chunk_type: crate::traits::ChunkType::Summary,
                    content_hash,
                    generation: 1,
                },
            });
        }

        upsert_sku(
            &kernel,
            "boba-1",
            "milk-tea:drink:boba",
            100.0,
            &["milk-tea:ingredient:pearl"],
        )
        .await;
        upsert_sku(
            &kernel,
            "taro-1",
            "milk-tea:drink:taro",
            10.0,
            &["milk-tea:ingredient:taro-ball"],
        )
        .await;
        upsert_sku(
            &kernel,
            "lemon-1",
            "milk-tea:drink:lemon",
            20.0,
            &["milk-tea:ingredient:coconut-jelly"],
        )
        .await;
        // cheese 无 SKU：事实平面无锚点，过滤查询永不命中（不影响无过滤查询）。
        // cheese has no SKU: no fact anchor, filtered queries never hit it
        // (unfiltered queries are unaffected).

        let store = Arc::new(MockVectorStore::new());
        store
            .ensure_collection(COLLECTION, DIM, DistanceMetric::Cosine)
            .await
            .unwrap();
        store.upsert(COLLECTION, &points).await.unwrap();

        if publish_qug {
            let config = fixture_config();
            let snapshot = kernel
                .assemble_qug_snapshot(
                    "milk-tea",
                    "0.1.0",
                    serde_json::to_string(&config.qug).unwrap(),
                    INTENTS_YAML.as_bytes().to_vec(),
                )
                .unwrap();
            let intents = parse_intents(&snapshot.intents_bytes).unwrap();
            let outcome =
                build_and_publish_qug(&*kernel, &snapshot, &config, &intents, false).unwrap();
            assert!(matches!(outcome, QugBuildOutcome::Published(_)));
        }
        (kernel, store)
    }

    // ===== golden fixture =====
    // ===== golden fixtures =====

    /// 快捷 golden 记录。
    /// Golden-record shortcut.
    fn gq(
        id: &str,
        query: &str,
        kind: GoldenKind,
        expected: &[&str],
        must_exclude: &[&str],
        filters: GoldenFilters,
    ) -> GoldenQuery {
        GoldenQuery {
            id: id.into(),
            line: 0,
            query: query.into(),
            normalized_query: crate::query_engine::qug::normalize(query).unwrap(),
            kind,
            expected_entity_ids: expected.iter().map(|s| s.to_string()).collect(),
            must_exclude_entity_ids: must_exclude.iter().map(|s| s.to_string()).collect(),
            filters,
            notes: None,
        }
    }

    fn eval_config() -> EvalConfig {
        EvalConfig {
            top_k: 10,
            rrf_k: 60,
            collection: COLLECTION.into(),
            vector_backend: "mock".into(),
            command: "wiktor eval --fixture".into(),
        }
    }

    fn domain() -> DomainConfig {
        fixture_config()
    }

    async fn run(
        kernel: &Arc<SqliteKernel>,
        store: &Arc<MockVectorStore>,
        golden: &GoldenSet,
        embedder: Arc<dyn QueryEmbedder>,
    ) -> Result<EvalOutcome> {
        run_evaluation(
            kernel.clone(),
            store.clone(),
            embedder,
            &domain(),
            INTENTS_YAML.as_bytes(),
            golden,
            &eval_config(),
        )
        .await
    }

    /// 完整六样本 golden 集（手算指标见 exact-metrics 测试）。
    /// The full six-sample golden set (hand-derived metrics in the
    /// exact-metrics test).
    fn full_golden() -> GoldenSet {
        GoldenSet::from_queries(vec![
            gq(
                "syn-01",
                "波霸",
                GoldenKind::Synonym,
                &["milk-tea:drink:boba"],
                &[],
                GoldenFilters::default(),
            ),
            gq(
                "int-01",
                "少糖",
                GoldenKind::Intent,
                &["milk-tea:drink:boba"],
                &[],
                GoldenFilters::default(),
            ),
            gq(
                "neg-01",
                "不要珍珠",
                GoldenKind::Negation,
                &[],
                &["milk-tea:drink:boba"],
                GoldenFilters::default(),
            ),
            gq(
                "neg-02",
                "珍珠",
                GoldenKind::Negative,
                &[],
                &["milk-tea:drink:boba"],
                GoldenFilters::default(),
            ),
            gq(
                "neg-03",
                "果茶",
                GoldenKind::Negative,
                &[],
                &["milk-tea:drink:cheese"],
                GoldenFilters::default(),
            ),
            gq(
                "att-01",
                "奶茶",
                GoldenKind::AttributeFilter,
                &["milk-tea:drink:taro"],
                &[],
                GoldenFilters {
                    conditions: vec![GoldenFilterCondition::NumericRange {
                        field: "sugar_level".into(),
                        min: None,
                        max: Some(10.0),
                    }],
                },
            ),
        ])
    }

    // ===== A10：同一 snapshot 连续两次运行结果逐位一致 =====
    // ===== A10: two consecutive runs over the same snapshot are bitwise
    // identical =====

    #[tokio::test]
    async fn a10_consecutive_runs_are_bitwise_reproducible() {
        let (kernel, store) = fixture(true).await;
        let golden = full_golden();
        let first = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();
        let second = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();
        assert_eq!(first.tier_a, second.tier_a, "tier A must reproduce bitwise");
        assert_eq!(first.tier_b, second.tier_b, "tier B must reproduce bitwise");
        assert_eq!(first.tier_c, second.tier_c, "tier C must reproduce bitwise");
        assert_eq!(first.decision, second.decision);
    }

    // ===== A10/A11：mock 向量下三档指标精确值 + disabled 判定路径 =====
    // ===== A10/A11: exact per-tier metrics under the mock vectors + the
    // disabled decision path =====
    //
    // 手算依据（fixture 见上；评测 top_k=10，共 4 页）：
    // Hand derivation (fixture above; eval top_k=10, 4 pages):
    // - 向量点：taro(idx4)、lemon(idx3)、cheese(idx4)；boba 无点。2 字查询
    //   （波霸/少糖/奶茶/珍珠/果茶 → idx2）对全部点余弦 0.0 但仍全部返回
    //   （mock 暴力扫描），排序 (score desc, id asc) → [cheese, lemon, taro]；
    //   "不要珍珠"（4 字 → idx4）→ cheese 1.0、taro 1.0、lemon 0.0 →
    //   [cheese, taro, lemon]。
    //   Vector points: taro(idx4), lemon(idx3), cheese(idx4); boba none.
    //   2-char queries (波霸/少糖/奶茶/珍珠/果茶 → idx2) score 0.0 against
    //   every point yet all points are still returned (mock brute force),
    //   ordered (score desc, id asc) → [cheese, lemon, taro]; "不要珍珠"
    //   (4 chars → idx4) → cheese 1.0, taro 1.0, lemon 0.0 →
    //   [cheese, taro, lemon].
    // - A（纯 FTS）：波霸→0 命中（alias 不入 FTS）；少糖→boba（正文含少糖）；
    //   不要珍珠→0 命中（trigram MATCH 无子串）；珍珠→boba（LIKE）；果茶→0；
    //   奶茶+sugar≤10→taro（scope 过滤）。recall = (0+1+1)/3；np = (1+0+1)/3。
    //   A (pure FTS): 波霸→0 hits (alias not in FTS); 少糖→boba (body contains
    //   少糖); 不要珍珠→0 hits (trigram MATCH, no substring); 珍珠→boba (LIKE);
    //   果茶→0; 奶茶+sugar≤10→taro (scope filter). recall = (0+1+1)/3;
    //   np = (1+0+1)/3.
    // - B：cheese 经向量路进入"果茶"查询命中 → np 掉到 (1+0+0)/3；boba 无
    //   向量点 → 波霸仍 0 命中 → recall 不变 2/3。
    //   B: cheese rides the vector path into the 果茶 query → np drops to
    //   (1+0+0)/3; boba has no vector point → 波霸 still 0 hits → recall stays
    //   2/3.
    // - C：波霸→珍珠奶茶 同义改写命中（synonym @1 = 1）；少糖属性过滤
    //   sugar≤30 把 boba(100) 滤出候选域（intent @10 = 0）；珍珠 无匹配节点、
    //   奶茶/果茶 是 Hyponym 父端（tag 节点无出边，Step3 语义 rewrite →
    //   None）→ 三者显式 fallback（fallback_count = 3）；neg-03 即便 fallback
    //   与否，cheese 都随向量路（0 分点也返回）留在 top-10 → 违规。
    //   C: 波霸→珍珠奶茶 synonym rewrite hits (synonym @1 = 1); 少糖's
    //   attribute filter sugar≤30 pushes boba(100) out of the scope (intent
    //   @10 = 0); 珍珠 matches no node, while 奶茶/果茶 sit on the Hyponym
    //   parent side (tag nodes have no outgoing edges, so the Step3 rewrite
    //   yields None) → three explicit fallbacks (fallback_count = 3); in
    //   neg-03 cheese rides the vector path (0-score points are returned too)
    //   into the top-10 regardless → violation.
    #[tokio::test]
    async fn mock_vector_exact_metrics_and_disabled_decision() {
        let (kernel, store) = fixture(true).await;
        let golden = full_golden();
        let out = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();

        let two_thirds = (0.0 + 1.0 + 1.0) / 3.0;
        // —— A 档精确值。
        // —— Tier A exact values.
        assert_eq!(out.tier_a.recall_at_1, Some(two_thirds));
        assert_eq!(out.tier_a.recall_at_5, Some(two_thirds));
        assert_eq!(out.tier_a.recall_at_10, Some(two_thirds));
        assert_eq!(out.tier_a.negative_precision, Some((1.0 + 0.0 + 1.0) / 3.0));
        assert_eq!(out.tier_a.fallback_count, 0);
        assert_eq!(out.tier_a.by_kind["synonym"].recall_at_10, Some(0.0));
        assert_eq!(out.tier_a.by_kind["intent"].recall_at_10, Some(1.0));
        assert_eq!(
            out.tier_a.by_kind["attribute_filter"].recall_at_10,
            Some(1.0)
        );
        assert_eq!(out.tier_a.by_kind["negation"].negative_precision, Some(1.0));
        assert_eq!(
            out.tier_a.by_kind["negative"].negative_precision,
            Some((0.0 + 1.0) / 2.0)
        );
        assert_eq!(out.tier_a.by_kind["negative"].recall_at_10, None);

        // —— B 档：recall 同 A，cheese 经向量进入排除查询 → np = 1/3。
        // —— Tier B: recall equals A; cheese rides the vector path into an
        //      exclusion query → np = 1/3.
        assert_eq!(out.tier_b.recall_at_10, Some(two_thirds));
        assert_eq!(out.tier_b.negative_precision, Some((1.0 + 0.0 + 0.0) / 3.0));
        assert_eq!(out.tier_b.by_kind["negative"].negative_precision, Some(0.0));
        assert_eq!(out.tier_b.fallback_count, 0);

        // —— C 档：改写生效样本 3（syn/int/neg-01），neg-02/neg-03/att-01
        //      显式 fallback（att/neg-03 命中 Hyponym 父端节点，无出边）。
        // —— Tier C: 3 rewrite-applied samples (syn/int/neg-01); neg-02/
        //      neg-03/att-01 fall back explicitly (att/neg-03 match Hyponym
        //      parent nodes that have no outgoing edges).
        assert_eq!(out.tier_c.recall_at_1, Some(two_thirds));
        assert_eq!(out.tier_c.recall_at_10, Some(two_thirds));
        // neg-01 合规（boba 被下推滤出）、neg-02 fallback 后 boba 命中违规、
        // neg-03 fallback 后 cheese 随向量路违规 → np = 1/3（与 B 持平，无
        // 回退标注）。
        // neg-01 compliant (boba pushed out), neg-02 falls back and boba hits
        // (violation), neg-03 falls back and cheese rides the vector path
        // (violation) → np = 1/3 (level with B, no regression flag).
        assert_eq!(out.tier_c.negative_precision, Some((1.0 + 0.0 + 0.0) / 3.0));
        assert_eq!(out.tier_c.fallback_count, 3);
        assert_eq!(out.tier_c.by_kind["synonym"].recall_at_1, Some(1.0));
        assert_eq!(out.tier_c.by_kind["intent"].recall_at_10, Some(0.0));
        assert_eq!(out.tier_c.by_kind["negative"].negative_precision, Some(0.0));

        // 判定：gain = 0（C@10 = B@10 = 2/3）→ disabled；无回退标注；
        // active 图存在、source_hash 记录；有效样本 3（syn/int/neg-01），
        // neg-02/neg-03/att-01 显式 fallback。
        // Decision: gain = 0 (C@10 = B@10 = 2/3) → disabled; no regression
        // flags; an active graph exists and source_hash is recorded; 3
        // effective samples (syn/int/neg-01), neg-02/neg-03/att-01 fall back
        // explicitly.
        assert_eq!(out.decision.gain_pp, Some(0.0));
        assert_eq!(out.decision.qug_decision, QugDecision::Disabled);
        assert!(!out.decision.recall_regression);
        assert!(!out.decision.negative_precision_regression);
        assert_eq!(out.decision.active_qug_samples, 3);
        assert!(out.decision.has_active_graph);
        assert!(out.source_hash.is_some());
    }

    // ===== A11：enabled 判定路径（C−B ≥ 5pp）=====
    // ===== A11: the enabled decision path (C−B ≥ 5pp) =====

    #[tokio::test]
    async fn enabled_decision_when_gain_meets_threshold() {
        let (kernel, store) = fixture(true).await;
        // 波霸：B 靠向量/FTS 均不可达 → 0；C 经 QUG 同义改写命中 → 1。
        // 奶茶（过滤）：两档都命中 taro → 1。gain = (1.0 − 0.5) × 100 = 50pp。
        // 波霸: B reaches it neither via vectors nor FTS → 0; C hits via the
        // QUG synonym rewrite → 1. 奶茶 (filtered): both recall taro → 1.
        // gain = (1.0 − 0.5) × 100 = 50pp.
        let golden = GoldenSet::from_queries(vec![
            gq(
                "syn-01",
                "波霸",
                GoldenKind::Synonym,
                &["milk-tea:drink:boba"],
                &[],
                GoldenFilters::default(),
            ),
            gq(
                "att-01",
                "奶茶",
                GoldenKind::AttributeFilter,
                &["milk-tea:drink:taro"],
                &[],
                GoldenFilters {
                    conditions: vec![GoldenFilterCondition::NumericRange {
                        field: "sugar_level".into(),
                        min: None,
                        max: Some(10.0),
                    }],
                },
            ),
        ]);
        let out = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();
        assert_eq!(out.tier_b.recall_at_10, Some((0.0 + 1.0) / 2.0));
        assert_eq!(out.tier_c.recall_at_10, Some((1.0 + 1.0) / 2.0));
        assert_eq!(out.decision.gain_pp, Some(50.0));
        assert_eq!(out.decision.qug_decision, QugDecision::Enabled);
        assert!(!out.decision.recall_regression);
        // 无排除样本 → np 双双未定义，回退标注为否。
        // No exclusion samples → np undefined on both sides, no regression flag.
        assert!(!out.decision.negative_precision_regression);
    }

    // ===== A12：C 低于 B → 回退标注，但运行仍成功（disabled 是合格交付）=====
    // ===== A12: C below B → regression flag, yet the run still succeeds
    // (disabled is a valid delivery) =====

    #[tokio::test]
    async fn c_below_b_flags_regression_but_run_succeeds() {
        let (kernel, store) = fixture(true).await;
        // 少糖：B 靠 FTS 内容命中 boba（recall 1）；C 的 QUG 属性过滤
        // sugar≤30 把 boba（sugar=100）滤出候选域 → recall 0。gain = −100pp。
        // 少糖: B recalls boba via FTS content (recall 1); C's QUG attribute
        // filter sugar≤30 pushes boba (sugar=100) out of the scope → recall 0.
        // gain = −100pp.
        let golden = GoldenSet::from_queries(vec![gq(
            "int-01",
            "少糖",
            GoldenKind::Intent,
            &["milk-tea:drink:boba"],
            &[],
            GoldenFilters::default(),
        )]);
        let out = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();
        assert_eq!(out.tier_b.recall_at_10, Some(1.0));
        assert_eq!(out.tier_c.recall_at_10, Some(0.0));
        assert_eq!(out.decision.gain_pp, Some(-100.0));
        assert_eq!(out.decision.qug_decision, QugDecision::Disabled);
        assert!(out.decision.recall_regression, "C recall@10 < B must flag");
        // 运行成功（Ok）即本测试走到这里——disabled 未被改判为失败。
        // Reaching this point means the run returned Ok — disabled was never
        // re-judged as a failure.
    }

    // ===== 无 active QUG 构建：Ok(None) 非错误，C=B，判定走"无增益"disabled =====
    // ===== No active QUG build: Ok(None) is not an error, C == B, and the
    // decision takes the no-gain disabled path =====

    #[tokio::test]
    async fn no_active_build_is_no_gain_disabled_not_a_failure() {
        let (kernel, store) = fixture(false).await;
        let golden = full_golden();
        let out = run(&kernel, &store, &golden, Arc::new(CountingEmbedder))
            .await
            .unwrap();
        // C 无图 → 与 B 完全同指标（fallback 0：未尝试改写）。
        // C has no graph → metrics identical to B (fallback 0: no rewrite was
        // attempted).
        assert_eq!(out.tier_c, out.tier_b);
        assert_eq!(out.tier_c.fallback_count, 0);
        assert!(out.source_hash.is_none());
        assert!(!out.decision.has_active_graph);
        assert_eq!(out.decision.active_qug_samples, 0);
        assert_eq!(out.decision.qug_decision, QugDecision::Disabled);
        assert_eq!(out.decision.gain_pp, None);
    }

    // ===== 单条 query 错误冒泡：不吞掉、不伪装 disabled，汇总为运行失败 =====
    // ===== Per-query error bubbling: never swallowed, never disguised as
    // disabled, summarized into one run failure =====

    #[tokio::test]
    async fn per_query_error_bubbles_up_as_run_failure() {
        let (kernel, store) = fixture(true).await;
        let golden = GoldenSet::from_queries(vec![
            gq(
                "err-01",
                "boom",
                GoldenKind::Synonym,
                &["milk-tea:drink:boba"],
                &[],
                GoldenFilters::default(),
            ),
            gq(
                "ok-01",
                "少糖",
                GoldenKind::Intent,
                &["milk-tea:drink:boba"],
                &[],
                GoldenFilters::default(),
            ),
        ]);
        let err = run(&kernel, &store, &golden, Arc::new(BoomEmbedder))
            .await
            .expect_err("the embedder failure must bubble up as a run failure");
        let msg = err.to_string();
        assert!(
            msg.contains("err-01"),
            "failed sample id must be listed: {msg}"
        );
        assert!(msg.contains("embedder exploded"));
        assert!(
            !msg.contains("ok-01"),
            "healthy samples must not be listed: {msg}"
        );
        assert!(matches!(err, Error::Query(_)), "got {err:?}");
    }

    // ===== filters 映射、EvalConfig 默认值与 top_k 校验 =====
    // ===== Filter mapping, EvalConfig defaults, and the top_k check =====

    #[test]
    fn golden_filters_map_to_fact_plane_conditions() {
        // 字段名直接透传（STEP10 D3）：不再有 price→price / sugar→sugar_level /
        // ingredients→ingredient_ids 的硬编码映射；任何领域字段都按原样表达。
        // Field names pass through (STEP10 D3): the price→price / sugar→sugar_level
        // / ingredients→ingredient_ids mapping is gone; any domain field is kept
        // as-is.
        let f = GoldenFilters {
            conditions: vec![
                GoldenFilterCondition::NumericRange {
                    field: "price".into(),
                    min: Some(5.0),
                    max: Some(20.0),
                },
                GoldenFilterCondition::NumericRange {
                    field: "sugar_level".into(),
                    min: Some(0.0),
                    max: Some(30.0),
                },
                GoldenFilterCondition::TextEquals {
                    field: "size".into(),
                    value: "中杯".into(),
                },
                GoldenFilterCondition::RefContains {
                    field: "ingredient_ids".into(),
                    refs: vec!["milk-tea:ingredient:pearl".into()],
                },
                GoldenFilterCondition::RefExcludes {
                    field: "ingredient_ids".into(),
                    refs: vec!["milk-tea:ingredient:cheese-foam".into()],
                },
            ],
        };
        let filters = golden_filters_to_filters(&f);
        assert_eq!(filters.conditions.len(), 5);
        assert!(matches!(
            &filters.conditions[0],
            FilterCondition::NumericRange { field, min: Some(5.0), max: Some(20.0) }
                if field == "price"
        ));
        assert!(matches!(
            &filters.conditions[1],
            FilterCondition::NumericRange { field, min: Some(0.0), max: Some(30.0) }
                if field == "sugar_level"
        ));
        assert!(matches!(
            &filters.conditions[2],
            FilterCondition::TextEquals { field, value }
                if field == "size" && value == "中杯"
        ));
        assert!(matches!(
            &filters.conditions[3],
            FilterCondition::RefContains { field, refs }
                if field == "ingredient_ids" && refs.len() == 1
        ));
        assert!(matches!(
            &filters.conditions[4],
            FilterCondition::RefExcludes { field, refs }
                if field == "ingredient_ids" && refs.len() == 1
        ));
        assert!(golden_filters_to_filters(&GoldenFilters::default()).is_empty());
    }

    #[test]
    fn tech_docs_style_filters_pass_field_names_through() {
        // 证明第二领域（tech-docs）的过滤字段（level/format/audience_years）也能
        // 通用表达，无需任何 milk-tea 特化（STEP10 A3）。
        // Proves a second domain (tech-docs) filter fields (level/format/
        // audience_years) are expressible generically without any milk-tea
        // specialization (STEP10 A3).
        let f = GoldenFilters {
            conditions: vec![
                GoldenFilterCondition::TextEquals {
                    field: "level".into(),
                    value: "beginner".into(),
                },
                GoldenFilterCondition::NumericRange {
                    field: "audience_years".into(),
                    min: Some(0.0),
                    max: Some(3.0),
                },
                GoldenFilterCondition::RefExcludes {
                    field: "tags".into(),
                    refs: vec!["tech-docs:technology:sqlite".into()],
                },
            ],
        };
        let filters = golden_filters_to_filters(&f);
        assert_eq!(filters.conditions.len(), 3);
        assert!(matches!(
            &filters.conditions[0],
            FilterCondition::TextEquals { field, value }
                if field == "level" && value == "beginner"
        ));
        assert!(matches!(
            &filters.conditions[1],
            FilterCondition::NumericRange { field, min: Some(0.0), max: Some(3.0) }
                if field == "audience_years"
        ));
        assert!(matches!(
            &filters.conditions[2],
            FilterCondition::RefExcludes { field, refs }
                if field == "tags" && refs == &["tech-docs:technology:sqlite"]
        ));
    }

    #[tokio::test]
    async fn top_k_out_of_range_is_a_validation_error() {
        let (kernel, store) = fixture(false).await;
        let golden = GoldenSet::from_queries(vec![gq(
            "s-1",
            "波霸",
            GoldenKind::Synonym,
            &["milk-tea:drink:boba"],
            &[],
            GoldenFilters::default(),
        )]);
        let mut cfg = eval_config();
        cfg.top_k = 5;
        let err = run_evaluation(
            kernel.clone(),
            store.clone(),
            Arc::new(CountingEmbedder),
            &domain(),
            INTENTS_YAML.as_bytes(),
            &golden,
            &cfg,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        let mut cfg = eval_config();
        cfg.top_k = 101;
        let err = run_evaluation(
            kernel,
            store,
            Arc::new(CountingEmbedder),
            &domain(),
            INTENTS_YAML.as_bytes(),
            &golden,
            &cfg,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // GoldenSet::from_queries：kind_counts 正确、dataset_hash 同输入稳定且
    // 不同输入互异。
    // GoldenSet::from_queries: correct kind_counts; dataset_hash stable for
    // identical input and distinct across different inputs.
    #[test]
    fn from_queries_computes_counts_and_stable_hash() {
        let a = full_golden();
        assert_eq!(a.len(), 6);
        assert_eq!(a.count_of(GoldenKind::Synonym), 1);
        assert_eq!(a.count_of(GoldenKind::Negative), 2);
        assert_eq!(a.count_of(GoldenKind::AttributeFilter), 1);
        let b = full_golden();
        assert_eq!(
            a.dataset_hash(),
            b.dataset_hash(),
            "same records → same hash"
        );
        let c = GoldenSet::from_queries(vec![gq(
            "x-1",
            "波霸",
            GoldenKind::Synonym,
            &["milk-tea:drink:boba"],
            &[],
            GoldenFilters::default(),
        )]);
        assert_ne!(
            a.dataset_hash(),
            c.dataset_hash(),
            "different records → different hash"
        );
    }
}
