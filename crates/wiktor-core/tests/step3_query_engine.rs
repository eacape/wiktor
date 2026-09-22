//! Step 3 golden 评测：三档通过率对比。
//! Step 3 golden evaluation: three-tier pass-rate comparison (English + Chinese).
//!
//! Runs the same seed database and the same golden-query set through three tiers:
//!   A. pure FTS         (`search_candidates`, original text only, no vector)
//!   B. hybrid           (QueryEngine without QUG + MockVectorStore)
//!   C. QUG full path    (QueryEngine with QUG graph + MockVectorStore)
//! 同库同查询集跑三档：
//!   A. 纯 FTS      （search_candidates，仅原文，无向量）
//!   B. 混合        （无 QUG 的 QueryEngine + MockVectorStore）
//!   C. QUG 完整路径（带 QUG 图的 QueryEngine + MockVectorStore）
//!
//! Exit rule (Step 3 §9): if C's absolute pass-rate gain over B is ≥5 points, C is
//! considered enabled and must pass; if the gain is <5 points with no regression,
//! the test prints `QUG disabled` and still passes; a regression fails the test.
//! 退出判定（Step 3 §9）：C 相对 B 的绝对通过率提升 ≥5 个百分点 → C 视为启用且必须
//! 通过；提升 <5 且无回退 → 输出 `QUG disabled` 仍判通过；有回退 → 测试失败。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::qug::build_qug_from_wiki;
use wiktor_core::traits::{
    ChunkType, DataSource, DistanceMetric, DomainConfig, EntityStore, IntentConfig, VectorMetadata,
    VectorPoint, VectorStore,
};
use wiktor_core::types::{Cursor, FilterCondition, Filters, PublishStatus, Query};
use wiktor_core::{seed, QueryEmbedder, QueryEngine};

/// 测试向量维度（与 CLI 基线同构）。
/// Test vector dimension (isomorphic to the CLI baseline).
const DIM: usize = 768;

/// golden 查询记录（与 Step 2 schema 一致）。
/// Golden query record (mirrors Step 2's schema).
///
/// Step5 批 4 起新格式记录（expected_entity_ids，无 expected_hits）由
/// Step5 评测器执行；此处期望设默认值并在加载时过滤跳过。
/// Since Step5 batch 4, new-format records (expected_entity_ids, no
/// expected_hits) belong to the Step5 evaluator; expectations default here and
/// such records are skipped at load time.
#[derive(Debug, Deserialize)]
struct Golden {
    query: String,
    #[serde(default)]
    expected_hits: Vec<String>,
    #[serde(default)]
    filters: GoldenFilters,
}

/// 扁平 golden 过滤表示 → FilterCondition。
/// Flat golden filter representation → `FilterCondition`.
#[derive(Debug, Default, Deserialize)]
struct GoldenFilters {
    #[serde(default)]
    price_max: Option<f64>,
    #[serde(default)]
    price_min: Option<f64>,
    #[serde(default)]
    sugar_max: Option<f64>,
    #[serde(default)]
    sugar_min: Option<f64>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    ingredients: Vec<String>,
    #[serde(default)]
    exclude_ingredients: Vec<String>,
}

impl GoldenFilters {
    fn to_filters(&self) -> Filters {
        let mut conditions = Vec::new();
        if self.price_min.is_some() || self.price_max.is_some() {
            conditions.push(FilterCondition::NumericRange {
                field: "price".into(),
                min: self.price_min,
                max: self.price_max,
            });
        }
        if self.sugar_min.is_some() || self.sugar_max.is_some() {
            conditions.push(FilterCondition::NumericRange {
                field: "sugar_level".into(),
                min: self.sugar_min,
                max: self.sugar_max,
            });
        }
        if let Some(size) = &self.size {
            conditions.push(FilterCondition::TextEquals {
                field: "size".into(),
                value: size.clone(),
            });
        }
        if !self.ingredients.is_empty() {
            conditions.push(FilterCondition::RefContains {
                field: "ingredient_ids".into(),
                refs: self.ingredients.clone(),
            });
        }
        if !self.exclude_ingredients.is_empty() {
            conditions.push(FilterCondition::RefExcludes {
                field: "ingredient_ids".into(),
                refs: self.exclude_ingredients.clone(),
            });
        }
        Filters { conditions }
    }
}

/// 进程内确定性嵌入器（仅测试；不宣称语义质量）——同一文本恒定输出。
/// In-process deterministic embedder (test-only; no semantic-quality claims).
struct TestEmbedder;

#[async_trait]
impl QueryEmbedder for TestEmbedder {
    async fn embed(&self, text: &str) -> wiktor_core::Result<Vec<f32>> {
        let mut vec = vec![0.0_f32; DIM];
        for ch in text.chars() {
            let mut h = DefaultHasher::new();
            ch.hash(&mut h);
            let bucket = (h.finish() as usize) % DIM;
            let mut h2 = DefaultHasher::new();
            (ch, text.len()).hash(&mut h2);
            vec[bucket] += ((h2.finish() >> 8) % 1000) as f32 / 1000.0;
        }
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        Ok(vec)
    }
}

/// `examples/milk-tea` 目录（相对 crate 根）。
/// The `examples/milk-tea` directory (relative to the crate root).
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("milk-tea")
}

/// 从 seed-wiki 页面解析出 WikiPage 列表（构建 QUG 图 + 向量灌入用）。
/// Parses the seed-wiki pages into WikiPage list (for QUG graph + vector loading).
fn load_wiki_pages(domain_dir: &Path) -> Vec<wiktor_core::types::WikiPage> {
    let mut md_files: Vec<PathBuf> = std::fs::read_dir(domain_dir.join("seed-wiki"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();
    md_files
        .iter()
        .map(|p| {
            let content = std::fs::read_to_string(p).unwrap();
            seed::parse_page(&content).unwrap()
        })
        .collect()
}

/// 向内存 kernel 导入页面 + 事实平面，返回 (config, wiki_pages)。
/// Seeds pages + facts into the in-memory kernel; returns (config, wiki_pages).
async fn seed_all(
    kernel: &SqliteKernel,
    domain_dir: &Path,
) -> (DomainConfig, Vec<wiktor_core::types::WikiPage>) {
    let yaml_text = std::fs::read_to_string(domain_dir.join("domain.yaml")).unwrap();
    let config: DomainConfig = serde_yaml_ng::from_str(&yaml_text).unwrap();

    let wiki_pages = load_wiki_pages(domain_dir);
    for mut page in wiki_pages.clone() {
        page.metadata.domain_pack_version = config.version.clone();
        kernel
            .seed_pages(&page, &config.name, PublishStatus::Accepted)
            .unwrap();
    }

    if let Some(entity) = config
        .entities
        .iter()
        .find(|e| e.source.starts_with("jsonl://"))
    {
        let source = JsonlDataSource::from_config(entity, domain_dir).unwrap();
        let mut cursor: Option<Cursor> = None;
        loop {
            let batch = source.fetch(cursor.clone()).await.unwrap();
            if batch.is_empty() {
                break;
            }
            for raw in &batch {
                let facts = source.raw_to_facts(raw).unwrap();
                kernel
                    .upsert_facts(&raw.id, &facts, raw.source_revision)
                    .await
                    .unwrap();
            }
            let offset = cursor.map(|c| c.offset).unwrap_or(0) + batch.len();
            cursor = Some(Cursor {
                offset,
                batch_size: wiktor_core::data::DEFAULT_BATCH_SIZE,
            });
        }
    }
    (config, wiki_pages)
}

/// 把页面灌入 MockVectorStore（Summary 块，确定性嵌入）。
/// Step 4 §10：向量 payload 必须携带与 accepted head 一致的版本 metadata
/// （content_hash/generation），否则引擎在 RRF 前会把 hit 当过期向量丢弃——
/// 因此这里从 kernel 读真实 `(page_id, generation, content_hash)`。
/// Loads pages into the MockVectorStore (Summary chunks, deterministic embeddings).
/// Step 4 §10: vector payloads must carry version metadata matching the accepted
/// head (content_hash/generation), or the engine drops the hit as stale before
/// RRF — so the real `(page_id, generation, content_hash)` comes from the kernel.
async fn populate_mock(
    store: &MockVectorStore,
    embedder: &TestEmbedder,
    wiki_pages: &[wiktor_core::types::WikiPage],
    published: &[(String, i64, String, String)],
) {
    store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .unwrap();
    let heads: HashMap<&str, (i64, &str)> = published
        .iter()
        .map(|(page_id, generation, hash, _)| (page_id.as_str(), (*generation, hash.as_str())))
        .collect();
    let mut points = Vec::new();
    for page in wiki_pages {
        let Some((generation, hash)) = heads.get(page.page_id.as_str()) else {
            continue;
        };
        let vec = embedder
            .embed(&format!("{}\n{}", page.title, page.content))
            .await
            .unwrap();
        points.push(VectorPoint {
            id: page.page_id.clone(),
            vector: vec,
            metadata: VectorMetadata {
                entity_id: page.entity_id.to_key(),
                page_id: page.page_id.clone(),
                chunk_type: ChunkType::Summary,
                content_hash: hash.to_string(),
                generation: u64::try_from(*generation).unwrap_or(0),
            },
        });
    }
    store.upsert("milk-tea", &points).await.unwrap();
}

/// 命中实体集与期望集交集非空即通过（与 Step 2 一致）。
/// Non-empty intersection between hit entities and expected hits (same as Step 2).
fn passed(hits: &[wiktor_core::types::SearchHit], expected: &[String]) -> bool {
    let hit_ids: HashSet<String> = hits.iter().map(|h| h.entity_id.to_key()).collect();
    expected.iter().any(|e| hit_ids.contains(e))
}

/// 加载 golden 查询集。
/// Loads the golden-query set.
fn load_goldens(domain_dir: &Path) -> Vec<Golden> {
    std::fs::read_to_string(domain_dir.join("golden-queries.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Golden>(l).unwrap())
        .filter(|g| !g.expected_hits.is_empty())
        .collect()
}

/// 主评测：三档 + 退出判定。
/// Main evaluation: three tiers + exit rule.
#[tokio::test]
async fn golden_three_tier_qug_gate() {
    let domain_dir = examples_dir();
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let (config, wiki_pages) = seed_all(&kernel, &domain_dir).await;

    let counts = kernel.row_counts().unwrap();
    assert_eq!(counts["pages"], 20, "expect 20 seed pages");
    assert!(counts["facts"] >= 100 * 8, "expect ≥100 SKUs × 8 fields");

    let goldens = load_goldens(&domain_dir);
    assert!(
        goldens.len() >= 20,
        "need ≥20 goldens, got {}",
        goldens.len()
    );

    // 共享向量库与嵌入器；灌入页面向量（payload 版本 metadata 与 accepted head
    // 一致，Step 4 §10）。
    // Shared vector store + embedder; load page vectors (payload version metadata
    // matches the accepted head, Step 4 §10).
    let vector_store = Arc::new(MockVectorStore::new());
    let embedder = Arc::new(TestEmbedder);
    populate_mock(
        &vector_store,
        &embedder,
        &wiki_pages,
        &kernel.list_published_pages(&config.name).unwrap(),
    )
    .await;

    // ---- Tier A: pure FTS（仅原文，无向量、无 QUG、无候选域限制）----
    // ---- Tier A: pure FTS (original text only; no vector, no QUG, no candidate scope) ----
    let a_rate = tier_fts(&kernel, &goldens).await;

    // ---- Tier B: hybrid（QueryEngine，QUG 关闭）----
    // ---- Tier B: hybrid (QueryEngine, QUG disabled) ----
    let engine_b = QueryEngine::new(
        kernel.clone(),
        vector_store.clone(),
        None,
        embedder.clone(),
        "milk-tea",
        config.qug.candidate_multiplier,
        60,
    )
    .unwrap();
    let b_rate = tier_engine(&engine_b, &goldens, "B").await;

    // ---- Tier C: QUG 完整路径 ----
    // ---- Tier C: QUG full path ----
    let intents_yaml = std::fs::read_to_string(domain_dir.join("intents.yaml")).unwrap();
    let intents: IntentConfig = serde_yaml_ng::from_str(&intents_yaml).unwrap();
    let built = build_qug_from_wiki(&wiki_pages, &config, &intents).unwrap();
    let engine_c = QueryEngine::new(
        kernel.clone(),
        vector_store.clone(),
        Some(built.graph),
        embedder.clone(),
        "milk-tea",
        config.qug.candidate_multiplier,
        60,
    )
    .unwrap();
    let c_rate = tier_engine(&engine_c, &goldens, "C").await;

    // ---- 退出判定：C 相对 B 的增益 ----
    // ---- Exit rule: C's gain over B ----
    let gain_points = (c_rate - b_rate) * 100.0;
    let regression = c_rate + 1e-9 < b_rate;
    println!("=== result: A={a_rate:.4} B={b_rate:.4} C={c_rate:.4} gain={gain_points:+.2}pp ===");
    if gain_points >= 5.0 {
        println!("[Gate] QUG gain {gain_points:.2}pp ≥ 5pp → QUG ENABLED (pass)");
        assert!(
            c_rate >= 0.8,
            "QUG-enabled tier C must still reach ≥80% pass rate, got {c_rate:.2}"
        );
    } else if regression {
        panic!("QUG regression: C {c_rate:.4} < B {b_rate:.4}; fix before merge");
    } else {
        println!(
            "[Gate] QUG gain {gain_points:.2}pp < 5pp and no regression → QUG DISABLED (pass)"
        );
    }
}

/// Tier A：纯 FTS（search_candidates，仅原文，无过滤下推之外的候选域）。
/// Tier A: pure FTS (search_candidates, original text only).
async fn tier_fts(kernel: &SqliteKernel, goldens: &[Golden]) -> f64 {
    let mut passed_count = 0usize;
    let mut failed: Vec<(String, Vec<String>)> = Vec::new();
    for g in goldens {
        let hits = kernel
            .search_candidates(
                std::slice::from_ref(&g.query),
                &g.filters.to_filters(),
                10,
                None,
                None,
            )
            .unwrap();
        if passed(&hits, &g.expected_hits) {
            passed_count += 1;
        } else {
            failed.push((
                g.query.clone(),
                hits.iter().map(|h| h.entity_id.to_key()).collect(),
            ));
        }
    }
    let rate = passed_count as f64 / goldens.len() as f64;
    println!(
        "[Tier A] pure FTS: {passed_count}/{} = {:.2}%",
        goldens.len(),
        rate * 100.0
    );
    for (q, hits) in &failed {
        println!("  [A fail] {q:?} hits={hits:?}");
    }
    rate
}

/// Tier B/C：经 QueryEngine 跑全部 golden。
/// Tier B/C: run all goldens through the QueryEngine.
async fn tier_engine(
    engine: &QueryEngine<MockVectorStore>,
    goldens: &[Golden],
    label: &str,
) -> f64 {
    let mut passed_count = 0usize;
    let mut failed: Vec<(
        String,
        Vec<String>,
        wiktor_core::query_engine::QueryDiagnostics,
    )> = Vec::new();
    for g in goldens {
        let q = Query {
            text: g.query.clone(),
            filters: g.filters.to_filters(),
            top_k: 10,
            domain: Some("milk-tea".into()),
        };
        let res = engine.search(&q).await.unwrap();
        if passed(&res.hits, &g.expected_hits) {
            passed_count += 1;
        } else {
            failed.push((
                g.query.clone(),
                res.hits.iter().map(|h| h.entity_id.to_key()).collect(),
                res.diagnostics.clone(),
            ));
        }
    }
    let rate = passed_count as f64 / goldens.len() as f64;
    println!(
        "[Tier {label}] pass rate: {passed_count}/{} = {:.2}%",
        goldens.len(),
        rate * 100.0
    );
    for (q, hits, diag) in &failed {
        println!(
            "  [{label} fail] {q:?} hits={hits:?} status={:?} fts={} vec={} cand={} filters={}",
            diag.rewrite_status,
            diag.fts_count,
            diag.vector_count,
            diag.candidate_count,
            diag.applied_filters.conditions.len()
        );
    }
    rate
}
