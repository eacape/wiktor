//! Step 5 批3：运行时加载与 fallback 接线验证（spec `step5-qug-build.md` §4.4、
//! §7 A7/A8）。
//! Step 5 batch 3: runtime loading and fallback wiring checks (spec
//! `step5-qug-build.md` §4.4, §7 A7/A8).
//!
//! 覆盖语义：
//! - hash 一致：`with_persistent_qug` 注入持久化 active 图，rewrite 正常生效；
//! - 页变化（hash 不一致）：reload 后 `qug=None`，查询诊断写 `stale`，显式走混合
//!   fallback 且仍有命中（绝不静默用旧图或内存重建图顶替）；
//! - 重建 + 显式 reload：图恢复注入，诊断回到 `applied`；
//! - 无 active build：诊断 `disabled`，混合 fallback 照常；
//! - 边 JSON 损坏：`load_active_qug` 返回 Internal（exit 4 语义，eval 等强一致
//!   路径必须失败）；普通查询引擎吸收为 fallback，诊断 `disabled`；
//! - A8 抽查：seed 与 compiled accepted 页共存时注入的图同时承载两类页边。
//!
//! Covered semantics:
//! - consistent hash: `with_persistent_qug` injects the persisted active graph
//!   and the rewrite takes effect;
//! - a page change (hash mismatch): after reload `qug=None`, queries report
//!   `stale` and hybrid retrieval is the explicit fallback that still returns
//!   hits (never silently substituted by the stale or a rebuilt in-memory graph);
//! - rebuild + explicit reload: the graph is re-injected and the diagnosis
//!   returns to `applied`;
//! - no active build: the diagnosis is `disabled` and the hybrid fallback works;
//! - corrupt edge JSON: `load_active_qug` returns Internal (the exit-4 semantics;
//!   strongly-consistent paths such as eval must fail); the ordinary query engine
//!   absorbs it into fallback with the `disabled` diagnosis;
//! - an A8 spot check: with legacy seed and Step4 accepted compiled pages
//!   coexisting, the injected graph carries page edges of both kinds.

use async_trait::async_trait;
use std::sync::Arc;
use wiktor_core::kernel::qug_store::{build_and_publish_qug, load_active_qug, QUG_STALE_PREFIX};
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::qug::qug_build::{parse_intents, QugBuildOutcome};
use wiktor_core::query_engine::RewriteStatus as Status;
use wiktor_core::traits::{DistanceMetric, DomainConfig, VectorStore};
use wiktor_core::types::error::Error;
use wiktor_core::types::{PublishStatus, Query};
use wiktor_core::{seed, Filters, QueryEmbedder, QueryEngine};

/// 测试向量维度（仅测试；不宣称语义质量）。
/// Test vector dimension (test-only; no semantic-quality claims).
const DIM: usize = 8;

/// intents.yaml 原文（与 kernel 单测 fixture 同构；批3 以原始 bytes 冻结传入）。
/// Raw intents.yaml (isomorphic to the kernel unit-test fixture; batch 3 freezes
/// and passes the raw bytes).
const INTENTS_YAML: &str = r#"version: "0.1.0"
intents:
  - id: low_sugar
    phrases: ["不甜的", "少糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
"#;

/// 进程内确定性嵌入器（同一文本恒定输出）。
/// In-process deterministic embedder (the same text always yields the same vector).
struct TestEmbedder;

#[async_trait]
impl QueryEmbedder for TestEmbedder {
    async fn embed(&self, _text: &str) -> wiktor_core::Result<Vec<f32>> {
        let mut vec = vec![0.0_f32; DIM];
        vec[0] = 1.0;
        Ok(vec)
    }
}

/// fixture 领域配置（qug enabled、max_depth=2、multiplier=5）。
/// The fixture domain config (qug enabled, max_depth=2, multiplier=5).
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

/// seed 一页 accepted 页（alias=波霸、tag=奶茶；frontmatter 按 STEP5-001 落库）。
/// Seeds one accepted page (alias=波霸, tag=奶茶; frontmatter persisted per
/// STEP5-001).
fn seed_boba(kernel: &SqliteKernel) {
    let md = "---\npage_id: milk-tea:drink:boba\nentity_id: milk-tea:drink:boba\n\
             entity_type: drink\ntitle: 珍珠奶茶\naliases: [波霸]\ntags: [奶茶]\n---\n\n\
             珍珠奶茶是经典饮品。\n\n## 概述\n\n- 茶底\n";
    let page = seed::parse_page(md).unwrap();
    kernel
        .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
        .unwrap();
}

/// 发布一次 QUG 构建（accepted 页清单从 DB 现场组装），返回参与页数。
/// Publishes one QUG build (the accepted page list is assembled from the DB) and
/// returns the participating page count.
fn publish_build(kernel: &SqliteKernel, config: &DomainConfig) -> usize {
    let snapshot = kernel
        .assemble_qug_snapshot(
            "milk-tea",
            "0.1.0",
            serde_json::to_string(&config.qug).unwrap(),
            INTENTS_YAML.as_bytes().to_vec(),
        )
        .unwrap();
    let page_count = snapshot.pages.len();
    let intents = parse_intents(&snapshot.intents_bytes).unwrap();
    let outcome = build_and_publish_qug(kernel, &snapshot, config, &intents, false).unwrap();
    assert!(
        matches!(outcome, QugBuildOutcome::Published(_)),
        "expected published, got {outcome:?}"
    );
    page_count
}

/// 组装共享依赖（kernel + 向量库 + 嵌入器；向量集合已建、保持空置——向量路 0
/// 命中，RRF 只剩 FTS，足以验证 fallback 检索路径）。
/// Assembles the shared dependencies (kernel + vector store + embedder; the
/// collection exists but stays empty — 0 vector hits, RRF reduces to FTS, which
/// is enough to exercise the fallback retrieval path).
async fn shared_deps() -> (Arc<SqliteKernel>, Arc<MockVectorStore>, Arc<TestEmbedder>) {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .unwrap();
    (kernel, store, Arc::new(TestEmbedder))
}

fn query(text: &str) -> Query {
    Query {
        text: text.to_string(),
        filters: Filters::empty(),
        top_k: 5,
        domain: Some("milk-tea".into()),
    }
}

// A7：hash 一致加载 → rewrite 生效；页变化 → stale 诊断 + 混合 fallback 仍有
// 命中；重建 + 显式 reload → 恢复 applied。
// A7: a consistent hash loads and the rewrite applies; a page change → the stale
// diagnosis with a hybrid fallback that still hits; rebuild + explicit reload →
// applied again.
#[tokio::test]
async fn a7_engine_stale_reload_and_fallback_flow() {
    let (kernel, store, embedder) = shared_deps().await;
    seed_boba(&kernel);
    let config = fixture_config();
    assert_eq!(publish_build(&kernel, &config), 1);

    let mut engine = QueryEngine::with_persistent_qug(
        kernel.clone(),
        store.clone(),
        &config,
        INTENTS_YAML.as_bytes(),
        embedder.clone(),
        "milk-tea",
        60,
    )
    .unwrap();
    assert!(
        engine.qug.is_some(),
        "consistent hash must inject the graph"
    );
    assert!(!engine.qug_stale);

    let res = engine.search(&query("珍珠奶茶")).await.unwrap();
    assert_eq!(res.diagnostics.rewrite_status, Status::Applied);
    assert!(res.rewritten.is_some(), "hyponym expansion must apply");
    assert!(!res.rewrite_failure);

    // 页变化（content_hash 改动）→ hash 不一致 → reload 后 stale。
    // A page change (content_hash updated) → hash mismatch → stale after reload.
    kernel
        .execute_batch(
            "UPDATE pages SET content_hash = 'h-changed'
             WHERE page_id = 'milk-tea:drink:boba'",
        )
        .unwrap();
    let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
    assert!(
        err.to_string().contains(QUG_STALE_PREFIX),
        "expected stale prefix, got {err}"
    );
    engine.reload_persistent_qug(&config, INTENTS_YAML.as_bytes());
    assert!(engine.qug.is_none(), "the stale graph must never be served");
    assert!(engine.qug_stale);

    let res = engine.search(&query("珍珠奶茶")).await.unwrap();
    assert_eq!(res.diagnostics.rewrite_status, Status::Stale);
    assert!(res.rewritten.is_none());
    assert!(!res.rewrite_failure, "no rewrite was attempted when stale");
    assert!(
        !res.hits.is_empty(),
        "the hybrid fallback must still retrieve via FTS"
    );

    // 重建（新 hash）+ 显式 reload → 恢复注入、诊断 applied。
    // Rebuild (new hash) + explicit reload → re-injected and the diagnosis is
    // applied again.
    publish_build(&kernel, &config);
    engine.reload_persistent_qug(&config, INTENTS_YAML.as_bytes());
    assert!(engine.qug.is_some());
    assert!(!engine.qug_stale);
    let res = engine.search(&query("珍珠奶茶")).await.unwrap();
    assert_eq!(res.diagnostics.rewrite_status, Status::Applied);
}

// A7：无 active build → disabled 诊断 + 混合 fallback 照常检索；qug.enabled=false
// 同样落到 disabled（显式关闭语义）。
// A7: no active build → the disabled diagnosis and a working hybrid fallback; a
// config with qug.enabled=false also lands on disabled (explicitly off).
#[tokio::test]
async fn a7_engine_without_active_build_reports_disabled() {
    let (kernel, store, embedder) = shared_deps().await;
    seed_boba(&kernel);
    let config = fixture_config();

    let engine = QueryEngine::with_persistent_qug(
        kernel.clone(),
        store.clone(),
        &config,
        INTENTS_YAML.as_bytes(),
        embedder,
        "milk-tea",
        60,
    )
    .unwrap();
    assert!(engine.qug.is_none());
    assert!(!engine.qug_stale, "missing active is disabled, not stale");

    let res = engine.search(&query("珍珠奶茶")).await.unwrap();
    assert_eq!(res.diagnostics.rewrite_status, Status::Disabled);
    assert!(res.rewritten.is_none());
    assert!(!res.hits.is_empty(), "the hybrid fallback must retrieve");

    let yaml = r#"name: milk-tea
version: "0.1.0"
entities:
  - name: drink
    source: jsonl://fixture
    id_field: id
    type_field: type
    fields: []
query:
  filters: []
qug:
  enabled: false
"#;
    let off: DomainConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let engine = QueryEngine::with_persistent_qug(
        kernel,
        store,
        &off,
        INTENTS_YAML.as_bytes(),
        Arc::new(TestEmbedder),
        "milk-tea",
        60,
    )
    .unwrap();
    assert!(engine.qug.is_none());
    assert!(!engine.qug_stale);
}

// A7：边 JSON 损坏 → load_active_qug 返回 Internal（exit 4 语义）；查询引擎把
// 加载失败吸收为 fallback，诊断 disabled 且仍有命中，绝不返回旧图。
// A7: corrupt edge JSON → load_active_qug returns Internal (the exit-4
// semantics); the query engine absorbs the load failure into fallback with the
// disabled diagnosis and hits — never serving the stale graph.
#[tokio::test]
async fn a7_engine_corrupt_payload_falls_back_and_loader_reports_internal() {
    let (kernel, store, embedder) = shared_deps().await;
    seed_boba(&kernel);
    let config = fixture_config();
    publish_build(&kernel, &config);

    // 损坏前：加载成功。
    // Before corruption: the load succeeds.
    assert!(load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
        .unwrap()
        .is_some());
    kernel
        .execute_batch("UPDATE qug_page_snapshots SET edge_json = 'not json'")
        .unwrap();
    // 损坏后：Internal，不返回任何图（旧图也不行）。
    // After corruption: Internal, no graph at all (the stale graph included).
    let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
    assert!(matches!(err, Error::Internal(_)), "got {err:?}");

    let engine = QueryEngine::with_persistent_qug(
        kernel,
        store,
        &config,
        INTENTS_YAML.as_bytes(),
        embedder,
        "milk-tea",
        60,
    )
    .unwrap();
    assert!(
        engine.qug.is_none(),
        "corrupt payload must not inject a graph"
    );
    assert!(!engine.qug_stale, "corruption is internal, not stale");

    let res = engine.search(&query("珍珠奶茶")).await.unwrap();
    assert_eq!(res.diagnostics.rewrite_status, Status::Disabled);
    assert!(!res.hits.is_empty(), "the hybrid fallback must retrieve");
}

// A8 引擎侧抽查：seed 与 compiled accepted 页共存时，注入的持久化图同时承载
// 两类页面的边（kernel 侧并集已由 qug_store 单测覆盖，这里验证注入路径）。
// An engine-side spot check for A8: with legacy seed and Step4 accepted compiled
// pages coexisting, the injected persisted graph carries edges from both page
// kinds (the kernel-side union is covered by qug_store unit tests; this verifies
// the injection path).
#[tokio::test]
async fn a8_engine_injects_graph_over_seed_and_compiled_pages() {
    let (kernel, store, embedder) = shared_deps().await;
    seed_boba(&kernel);
    // Step4 compiled 页形态直接落库（generation=2, artifact_version='wiki-v1'）；
    // execute_batch 内联 SQL：载荷不含单引号，无注入面。
    // The Step4 compiled-page shape is written straight to the DB (generation=2,
    // artifact_version='wiki-v1'); inline SQL via execute_batch: the payload has
    // no single quotes, so there is no injection surface.
    kernel
        .execute_batch(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at,
                source_revision, artifact_version, frontmatter_json)
             VALUES ('milk-tea:drink:fruit', 'milk-tea:drink:fruit', 'milk-tea', 'drink',
                '水果茶', '水果茶内容。', 'h-fruit', 2, 'accepted', '0.1.0', 1,
                'seed', 'none', 1, 1, 0, 'wiki-v1',
                '{\"title\":\"水果茶\",\"aliases\":[\"fruit\"],\"tags\":[\"果茶\"]}')",
        )
        .unwrap();
    let config = fixture_config();
    assert_eq!(publish_build(&kernel, &config), 2);

    let engine = QueryEngine::with_persistent_qug(
        kernel,
        store,
        &config,
        INTENTS_YAML.as_bytes(),
        embedder,
        "milk-tea",
        60,
    )
    .unwrap();
    let graph = engine.qug.expect("the graph must be injected");
    // 两页各 1 alias + 1 tag → 4 条页面边 + 3 条配置边 = 7。
    // Each page contributes 1 alias + 1 tag → 4 page edges + 3 config edges = 7.
    assert_eq!(graph.graph.edge_count(), 7);
}
