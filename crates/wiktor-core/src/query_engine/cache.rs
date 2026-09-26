//! 查询热点缓存（P1，MASTER-PLAN §5.4/§5.5 落地）：generation-aware、按实体 ID
//! 精准失效的检索结果缓存。**缓存命中集合而非整条 QueryResult**——`log_id` 与
//! `latency_ms` 属每次查询，绝不入缓存（反馈闭环依赖每次查询的真实 log_id）。
//! Query hot-cache (P1, delivering MASTER-PLAN §5.4/§5.5): a generation-aware,
//! entity-ID-invalidated retrieval-result cache. It caches the **hit set, not the
//! whole QueryResult** — `log_id` and `latency_ms` are per-query and never cached
//! (the feedback loop depends on the real log_id of every query).
//!
//! 键与失效：
//! - **键** = 规范化查询签名（text / filters / top_k / domain / generation）的
//!   BLAKE3 摘要。generation 每次 search 现读（`current_published_generation`），
//!   编译发布使 generation 递增 → 键变化 → 旧条目自然失效（无需主动清缓存）。
//! - **实体失效** = 缓存额外维护 `entity_page_id → 键` 反向索引；事实平面直写
//!   （不经编译、不 bump generation 的 ETL 路径）调用
//!   [`QueryCache::invalidate_entity_ids`]，精确删掉含该实体的条目——不做整页
//!   TTL 赌博（§5.5 契约 #5）。
//!
//! Key & invalidation:
//! - **Key** = BLAKE3 of a normalized query signature (text / filters / top_k /
//!   domain / generation). generation is read fresh on each search via
//!   `current_published_generation`; a compile publish bumps it → the key changes
//!   → old entries naturally go stale (no explicit flush needed).
//! - **Entity invalidation** = the cache keeps an `entity_page_id → keys` reverse
//!   index; a fact-plane direct write (an ETL path that bypasses compile and does
//!   not bump generation) calls [`QueryCache::invalidate_entity_ids`] to precisely
//!   drop entries referencing that entity — no whole-page TTL gambling (§5.5 #5).

use crate::query_engine::QueryDiagnostics;
use crate::types::{RewrittenQuery, SearchHit};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 缓存条目：检索融合后的命中集合 + 改写 + 诊断。**不含 log_id / latency_ms**
/// （每次查询独立，反馈引用依赖真实 log_id）。
/// Cache entry: the fused hit set + rewrite + diagnostics. **Excludes log_id /
/// latency_ms** (per-query; feedback references the real log_id).
#[derive(Debug, Clone)]
pub struct CachedSearch {
    pub hits: Vec<SearchHit>,
    pub rewritten: Option<RewrittenQuery>,
    pub rewrite_failure: bool,
    pub diagnostics: QueryDiagnostics,
    /// 缓存命中的日志行需复现 `candidate_empty_initial`（不在 QueryDiagnostics，
    /// 反馈分析器依赖——故随条目携带）。
    /// Cache-hit log rows must reproduce `candidate_empty_initial` (absent from
    /// QueryDiagnostics; the feedback analyzer depends on it — so it rides along).
    pub candidate_empty_initial: bool,
}

/// 查询热点缓存（线程安全；内部 `moka::future::Cache` 键 = 规范化签名摘要）。
/// Query hot-cache (thread-safe; the inner `moka::future::Cache` key = the
/// normalized-signature digest).
#[derive(Clone)]
pub struct QueryCache {
    inner: moka::future::Cache<[u8; 32], Arc<CachedSearch>>,
    /// `entity_page_id → 该实体命中的缓存键集合`（实体失效反向索引）。
    /// `entity_page_id → cache keys whose hits reference that entity` (the
    /// entity-invalidation reverse index).
    entity_index: Arc<tokio::sync::RwLock<HashMap<String, HashSet<[u8; 32]>>>>,
}

impl QueryCache {
    /// 构造缓存（`capacity` = 最多缓存条目数；仅作软上限，命中超限按 moka 策略
    /// 淘汰最久未用）。
    /// Builds the cache (`capacity` = max cached entries; a soft cap evicted by
    /// moka's least-recently-used policy once exceeded).
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: moka::future::Cache::builder()
                .max_capacity(capacity)
                .build(),
            entity_index: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// 计算规范化查询签名摘要（BLAKE3）。
    /// Computes the normalized query-signature digest (BLAKE3).
    ///
    /// `text`/`filters`/`top_k`/`domain` 构成检索语义；`generation` 使缓存键随
    /// 编译发布递增而自然失效。
    /// `text`/`filters`/`top_k`/`domain` form the retrieval semantics; `generation`
    /// makes the key naturally stale when a compile publish bumps it.
    pub fn key(
        text: &str,
        filters: &crate::types::Filters,
        top_k: usize,
        domain: &str,
        generation: Option<i64>,
    ) -> [u8; 32] {
        // 键 = 签名元组的 serde 确定性 JSON 的 BLAKE3 摘要（Filters 含 f64 无法
        // derive Hash/Eq，故用 canonical JSON 作键标识，而非手写哈希）。
        // Key = BLAKE3 of the signature tuple's deterministic serde JSON (Filters
        // holds f64 and can't derive Hash/Eq, so canonical JSON serves as the key
        // identity instead of a hand-rolled hash).
        let sig = serde_json::to_vec(&(text, filters, top_k, domain, generation))
            .expect("cache key signature serializes");
        let mut h = blake3::Hasher::new();
        h.update(&sig);
        *h.finalize().as_bytes()
    }

    /// 命中检查。返回缓存条目时，调用方仍须**重写 query_logs 并取新 log_id**
    /// （反馈闭环需要），并用本次真实 latency 覆盖——条目本身不含这些。
    /// Lookup. On a hit, the caller must **still write query_logs and take the new
    /// log_id** (the feedback loop needs it) and override with the real latency —
    /// the entry carries neither.
    pub async fn get(&self, key: &[u8; 32]) -> Option<Arc<CachedSearch>> {
        self.inner.get(key).await
    }

    /// 插入缓存并登记实体反向索引（每 hit 的 page_id → 键）。
    /// Inserts into the cache and registers the entity reverse index (each hit's
    /// page_id → key).
    pub async fn insert(&self, key: [u8; 32], entry: CachedSearch) {
        let page_ids: HashSet<String> = entry.hits.iter().map(|h| h.page_id.clone()).collect();
        self.inner.insert(key, Arc::new(entry)).await;
        let mut idx = self.entity_index.write().await;
        for page_id in page_ids {
            idx.entry(page_id).or_default().insert(key);
        }
    }

    /// 精确失效：从缓存与反向索引中删除所有命中过这些 page_id 的条目
    /// （事实平面直写路径用；§5.5 契约 #5——按实体 ID 精准失效，不做整页 TTL）。
    /// Precise invalidation: removes every cached entry whose hits referenced
    /// these page_ids, from both the cache and the reverse index (for the
    /// fact-plane direct-write path; §5.5 #5 — invalidate by entity ID, never a
    /// whole-page TTL gamble).
    pub async fn invalidate_entity_ids(&self, page_ids: &[String]) {
        if page_ids.is_empty() {
            return;
        }
        let mut idx = self.entity_index.write().await;
        let mut keys_to_drop: HashSet<[u8; 32]> = HashSet::new();
        for page_id in page_ids {
            if let Some(keys) = idx.get(page_id) {
                keys_to_drop.extend(keys.iter().copied());
            }
        }
        for key in &keys_to_drop {
            self.inner.invalidate(key).await;
        }
        for page_id in page_ids {
            idx.remove(page_id);
        }
    }

    /// 全量清空（测试 / 运维用）。
    /// Full clear (tests / ops).
    pub async fn clear(&self) {
        self.inner.invalidate_all();
        let mut idx = self.entity_index.write().await;
        idx.clear();
    }

    /// 当前条目数（测试/诊断）。
    /// Current entry count (tests / diagnostics).
    pub fn len(&self) -> u64 {
        self.inner.entry_count()
    }

    /// 是否为空（配对 len() 满足 clippy；纯诊断用）。
    /// Whether the cache is empty (paired with len() to satisfy clippy; used for
    /// diagnostics only).
    pub fn is_empty(&self) -> bool {
        self.inner.entry_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_engine::RewriteStatus;
    use crate::types::{EntityId, Filters};

    fn hit(page_id: &str) -> SearchHit {
        // page_id 需是合法 entity key（domain:type:id）以通过 from_key。
        // page_id must be a valid entity key (domain:type:id) to pass from_key.
        SearchHit {
            page_id: page_id.to_string(),
            entity_id: EntityId::from_key(page_id).expect("valid entity key in test"),
            score: 0.5,
            title: String::new(),
        }
    }

    fn entry(page_ids: &[&str]) -> CachedSearch {
        CachedSearch {
            hits: page_ids.iter().map(|p| hit(p)).collect(),
            rewritten: None,
            rewrite_failure: false,
            candidate_empty_initial: false,
            diagnostics: QueryDiagnostics {
                rewrite_status: RewriteStatus::Disabled,
                applied_filters: Filters::empty(),
                candidate_count: 0,
                fts_count: 0,
                vector_count: 0,
                rrf_k: 60,
                relaxation_attempted: false,
                relaxation_succeeded: false,
            },
        }
    }

    #[tokio::test]
    async fn key_changes_with_generation() {
        let f = Filters::empty();
        let k0 = QueryCache::key("a", &f, 5, "d", Some(1));
        let k1 = QueryCache::key("a", &f, 5, "d", Some(2));
        assert_ne!(k0, k1, "generation bump must change the key");
        // 相同输入恒同键（确定性）。
        let k0b = QueryCache::key("a", &f, 5, "d", Some(1));
        assert_eq!(k0, k0b, "same input must yield the same key");
    }

    #[tokio::test]
    async fn get_miss_then_insert_hit() {
        let c = QueryCache::new(100);
        let f = Filters::empty();
        let k = QueryCache::key("q", &f, 5, "d", Some(1));
        assert!(c.get(&k).await.is_none(), "empty cache must miss");
        c.insert(k, entry(&["d:t:p1", "d:t:p2"])).await;
        let got = c.get(&k).await.expect("inserted entry must hit");
        assert_eq!(got.hits.len(), 2);
        // entry_count() 是近似值且刚插入可能滞后；以 get 语义为准。
        // entry_count() is approximate and may lag right after insert; the get
        // semantics are the source of truth.
        let _ = c.len();
    }

    #[tokio::test]
    async fn invalidate_entity_drops_only_referencing_entries() {
        let c = QueryCache::new(100);
        let f = Filters::empty();
        let ka = QueryCache::key("a", &f, 5, "d", Some(1));
        let kb = QueryCache::key("b", &f, 5, "d", Some(1));
        c.insert(ka, entry(&["d:t:p1", "d:t:p2"])).await;
        c.insert(kb, entry(&["d:t:p3"])).await;
        // 失效 p2 → 只应删含 p2 的条目（ka），kb 保留。
        // Invalidate p2 → only the entry referencing p2 (ka) should drop; kb stays.
        c.invalidate_entity_ids(&["d:t:p2".to_string()]).await;
        assert!(
            c.get(&ka).await.is_none(),
            "ka references p2, must be dropped"
        );
        assert!(
            c.get(&kb).await.is_some(),
            "kb does not reference p2, must survive"
        );
        // 反向索引应已清掉失效条目（kb 幸存 → 仍可命中）。
        // The reverse index should have dropped the invalidated entry (kb
        // survives → still retrievable).
        let _ = c.len();
    }

    #[tokio::test]
    async fn invalidate_unknown_entity_is_noop() {
        let c = QueryCache::new(100);
        let f = Filters::empty();
        let k = QueryCache::key("a", &f, 5, "d", Some(1));
        c.insert(k, entry(&["d:t:p1"])).await;
        c.invalidate_entity_ids(&["d:t:does-not-exist".to_string()])
            .await;
        assert!(
            c.get(&k).await.is_some(),
            "unrelated invalidation must be a no-op"
        );
    }
}
