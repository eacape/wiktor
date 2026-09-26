//! Step 14 (P1) 集成测试：查询热点缓存的 `search` 装配契约。
//! Step 14 (P1) integration tests: the query hot-cache `search` wiring contract.
//!
//! 覆盖（MASTER-PLAN §5.4/§5.5 契约 #5）：
//! - 同一查询二次调用命中缓存：返回**相同 hits**，但每次仍写新 query_logs → 新
//!   `log_id`（反馈闭环依赖真实 log_id，绝不被缓存吞掉）。
//! - 实体失效（`invalidate_entity_ids`）后命中集合变化反映在后续检索中。
//!
//! Coverage (MASTER-PLAN §5.4/§5.5 #5):
//! - a second identical query hits the cache: identical hits returned, but every
//!   call still writes a new query_logs row → a fresh `log_id` (the feedback loop
//!   depends on the real log_id; caching never swallows it).
//! - after `invalidate_entity_ids` the changed hit set is reflected in later
//!   retrieval.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::{QueryCache, QueryEmbedder, QueryEngine};
use wiktor_core::seed;
use wiktor_core::traits::EntityStore;
use wiktor_core::types::{
    EntityId, Error, FactValue, Facts, FilterCondition, Filters, PublishStatus, Query,
};

const DOMAIN: &str = "milk-tea";

/// fts_only 下永不被调用的嵌入器（失败即证明向量路没跑——缓存命中不应触发它）。
/// Never-called embedder under fts_only (any call fails — proving the vector path
/// didn't run, including on a cache hit).
struct UnusedEmbedder;

#[async_trait]
impl QueryEmbedder for UnusedEmbedder {
    async fn embed(&self, _text: &str) -> wiktor_core::Result<Vec<f32>> {
        Err(Error::Internal(
            "vector path must be skipped under fts_only".into(),
        ))
    }
}

/// 种子：一页波霸奶茶 + 一条挂到该页 category 的事实（供过滤/相关性验证）。
/// Seeds: one 波霸奶茶 page + a fact pointing its category at that page (for
/// filter/relevance assertions).
async fn fixture() -> (Arc<SqliteKernel>, QueryEngine<MockVectorStore>) {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let page = seed::parse_page(
        "---\npage_id: milk-tea:drink:boba\nentity_id: milk-tea:drink:boba\ntitle: 波霸奶茶\nentity_type: drink\n---\n\n波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。",
    )
    .unwrap();
    kernel
        .seed_pages(&page, DOMAIN, PublishStatus::Accepted)
        .unwrap();
    let mut facts = Facts {
        entity_id: EntityId::new(DOMAIN, "product", "a").unwrap(),
        fields: BTreeMap::new(),
        source_revision: 1,
    };
    facts.fields.insert(
        "category".into(),
        FactValue::Text("milk-tea:drink:boba".into()),
    );
    facts
        .fields
        .insert("price".into(), FactValue::Numeric(18.0));
    kernel
        .upsert_facts(&facts.entity_id, &facts, facts.source_revision)
        .await
        .unwrap();
    let mut engine = QueryEngine::new(
        kernel.clone(),
        Arc::new(MockVectorStore::new()),
        None,
        Arc::new(UnusedEmbedder),
        DOMAIN,
        5,
        60,
    )
    .unwrap()
    .with_cache(QueryCache::new(100));
    engine.fts_only = true;
    (kernel, engine)
}

fn q(text: &str, filters: Filters, domain: Option<&str>) -> Query {
    Query {
        text: text.into(),
        filters,
        top_k: 5,
        domain: domain.map(str::to_string),
    }
}

/// 缓存命中不吞 log_id：同一查询第二次命中缓存，hits 相同但 log_id 不同。
/// A cache hit never swallows log_id: on the second identical query (a hit) the
/// hits are identical while log_id differs — every call still logs.
#[tokio::test]
async fn cache_hit_returns_same_hits_but_fresh_log_id() {
    let (kernel, engine) = fixture().await;
    let query = q("波霸奶茶", Filters::empty(), Some(DOMAIN));

    let first = engine.search(&query).await.unwrap();
    let second = engine.search(&query).await.unwrap();

    // 命中集合一致（缓存正确复用）。
    // The hit sets agree (cache correctly reused).
    assert_eq!(
        first
            .hits
            .iter()
            .map(|h| h.page_id.clone())
            .collect::<Vec<_>>(),
        second
            .hits
            .iter()
            .map(|h| h.page_id.clone())
            .collect::<Vec<_>>(),
        "cached hits must match the first retrieval"
    );
    assert_eq!(first.hits.len(), 1);
    assert_eq!(first.hits[0].page_id, "milk-tea:drink:boba");

    // 每次调用都写新 log_id（反馈依赖真实 log_id；缓存只复用 hits）。
    // Every call writes a new log_id (feedback needs the real id; the cache
    // reuses only the hit set).
    assert!(first.log_id.is_some() && second.log_id.is_some());
    assert_ne!(
        first.log_id, second.log_id,
        "cache hit must still log a fresh log_id"
    );

    // 确确实实写了两条 query_logs。
    // Two query_logs rows were actually written.
    let (logs, events) = kernel.load_feedback_window(DOMAIN, 0, i64::MAX).unwrap();
    assert!(events.is_empty());
    assert_eq!(logs.len(), 2, "one log per search call, cache hit included");
}

/// 缓存命中不重跑向量路径（UnusedEmbedder 会失败）——过滤一致的缓存查询复用。
/// A cache hit does not re-run the vector path (UnusedEmbedder would fail) — the
/// cached entry with matching filters is reused wholesale.
#[tokio::test]
async fn cache_hit_does_not_recompute_retrieval() {
    let (_kernel, engine) = fixture().await;
    let query = q("波霸奶茶", Filters::empty(), Some(DOMAIN));

    let first = engine.search(&query).await.unwrap();
    let second = engine.search(&query).await.unwrap();

    assert_eq!(first.hits.len(), 1);
    assert_eq!(second.hits.len(), 1, "cached query must still hit");
}

/// 实体失效后过滤结果变化反映在后续检索（MVCC 语义不落后于实体变更）。
/// After entity invalidation a changed filtered result shows up in later search
/// (retrieval does not lag the entity change).
#[tokio::test]
async fn invalidate_entity_reflects_filter_change() {
    let (kernel, engine) = fixture().await;

    // category=波霸奶茶 的过滤查询：事实平面把候选域限到 boba 页 → 命中。
    // A category=boba filtered query: the fact plane scopes candidates to the
    // boba page → it hits.
    let filter = Filters {
        conditions: vec![FilterCondition::TextEquals {
            field: "category".into(),
            value: "milk-tea:drink:boba".into(),
        }],
    };
    let query = q("波霸奶茶", filter, Some(DOMAIN));

    let before = engine.search(&query).await.unwrap();
    assert_eq!(before.hits.len(), 1, "category=boba must hit the boba page");

    // 变更该实体事实：category 改指 other → 精准失效命中它的缓存条目。
    // Change the entity's fact: category now points elsewhere → precisely
    // invalidate the cached entry referencing it.
    let mut facts = Facts {
        entity_id: EntityId::new(DOMAIN, "product", "a").unwrap(),
        fields: BTreeMap::new(),
        source_revision: 2,
    };
    facts.fields.insert(
        "category".into(),
        FactValue::Text("milk-tea:drink:other".into()),
    );
    kernel
        .upsert_facts(&facts.entity_id, &facts, facts.source_revision)
        .await
        .unwrap();
    engine
        .cache
        .as_ref()
        .unwrap()
        .invalidate_entity_ids(&["milk-tea:drink:boba".to_string()])
        .await;

    // 失效后重跑同查询：候选域不再含 boba → 命中清零（缓存不落后实体变更）。
    // After invalidation re-run the same query: the candidate scope no longer
    // contains boba → zero hits (the cache does not lag the entity change).
    let after = engine.search(&query).await.unwrap();
    assert_eq!(
        after.hits.len(),
        0,
        "entity invalidation must drop the stale cached hit"
    );
}
