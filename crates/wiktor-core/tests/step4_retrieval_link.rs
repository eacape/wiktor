//! Step 4 §10/A21 检索衔接：向量过期 payload 防护与稳定键读取接口。
//!
//! - 旧 hash/generation、缺版本 metadata、不存在页的向量 hit 在 RRF 融合前被拒；
//! - quarantine 页的向量（hash/generation 一致但非 accepted）同样被拒；
//! - `list_published_pages` 提供向量 worker 的 `(page_id, generation,
//!   content_hash, embedding_model)` 稳定键。
//!
//! Step 4 §10/A21 retrieval link: the stale vector-payload guard and the
//! stable-key reader interface.
//!
//! - Vector hits with a stale hash/generation, missing version metadata or a
//!   missing page are rejected before RRF fusion;
//! - a quarantined page's vector (matching hash/generation but not accepted) is
//!   rejected too;
//! - `list_published_pages` provides the vector worker's `(page_id, generation,
//!   content_hash, embedding_model)` stable keys.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::traits::{ChunkType, DistanceMetric, VectorMetadata, VectorPoint, VectorStore};
use wiktor_core::types::{Filters, PublishStatus, Query};
use wiktor_core::{seed, QueryEmbedder, QueryEngine};

/// 测试向量维度。
/// Test vector dimension.
const DIM: usize = 64;

/// 进程内确定性嵌入器（仅测试；不宣称语义质量）。
/// In-process deterministic embedder (test-only; no semantic-quality claims).
struct TestEmbedder;

#[async_trait]
impl QueryEmbedder for TestEmbedder {
    async fn embed(&self, text: &str) -> wiktor_core::Result<Vec<f32>> {
        Ok(deterministic_embed(text))
    }
}

/// 纯同步确定性嵌入（同一文本恒定输出；避免在异步测试里嵌套 runtime）。
/// Pure synchronous deterministic embedding (the same text always yields the
/// same vector; avoids a nested runtime inside async tests).
fn deterministic_embed(text: &str) -> Vec<f32> {
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
    vec
}

/// 解析一页 seed Wiki（drink 实体）。
/// Parses one seed wiki page (a drink entity).
fn page(slug: &str, title: &str, body: &str) -> wiktor_core::types::WikiPage {
    seed::parse_page(&format!(
        "---\npage_id: milk-tea:drink:{slug}\nentity_id: milk-tea:drink:{slug}\ntitle: {title}\nentity_type: drink\n---\n\n{body}"
    ))
    .unwrap()
}

/// 构造一个向量点（payload 即被测 metadata）。
/// Builds one vector point (its payload is the metadata under test).
fn point(
    point_id: &str,
    page_id: &str,
    generation: u64,
    content_hash: &str,
    query: &str,
) -> VectorPoint {
    // 向量本身与查询文本相关即可（只关心融合前的校验，不关心相似度排序）。
    // The vector only needs to be query-related (the test cares about pre-fusion
    // validation, not similarity ordering).
    VectorPoint {
        id: point_id.to_string(),
        vector: deterministic_embed(query),
        metadata: VectorMetadata {
            entity_id: page_id.to_string(),
            page_id: page_id.to_string(),
            chunk_type: ChunkType::Summary,
            content_hash: content_hash.to_string(),
            generation,
        },
    }
}

// A21：旧向量 hash/generation、缺版本 metadata、不存在页的 hit 在融合前被拒；
// 有效 hit 保留（vector_count 只计有效 hit）。kernel 语义：页有效 ⇔ 该页全部
// payload 与 accepted head 一致。
// A21: hits with stale hash/generation, missing version metadata or a missing
// page are rejected before fusion; valid hits survive (vector_count only counts
// valid hits). Kernel semantics: a page is valid iff every payload provided for
// it matches its accepted head.
#[tokio::test]
async fn a21_stale_vector_hits_rejected_before_fusion() {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    kernel
        .seed_pages(
            &page("oolong", "乌龙奶茶", "乌龙奶茶是经典茶底，香气清雅。"),
            "milk-tea",
            PublishStatus::Accepted,
        )
        .unwrap();
    kernel
        .seed_pages(
            &page("osmanthus", "桂花乌龙", "桂花乌龙以桂花窨制乌龙茶底。"),
            "milk-tea",
            PublishStatus::Accepted,
        )
        .unwrap();
    kernel
        .seed_pages(
            &page("cheese", "芝士奶盖茶", "芝士奶盖茶以咸香芝士奶盖著称。"),
            "milk-tea",
            PublishStatus::Accepted,
        )
        .unwrap();

    // 稳定键读取接口（§10）：accepted head 即 (page_id, generation, hash)。
    // Stable-key reader (§10): accepted heads are (page_id, generation, hash).
    let published = kernel.list_published_pages("milk-tea").unwrap();
    assert_eq!(published.len(), 3);
    let mut heads: std::collections::HashMap<String, (i64, String)> =
        std::collections::HashMap::new();
    for (page_id, generation, hash, _model) in &published {
        heads.insert(page_id.clone(), (*generation, hash.clone()));
    }
    let oolong_id = "milk-tea:drink:oolong".to_string();
    let (oolong_gen, oolong_hash) = heads[&oolong_id].clone();
    let osman_id = "milk-tea:drink:osmanthus".to_string();
    let (osman_gen, _osman_hash) = heads[&osman_id].clone();
    let cheese_id = "milk-tea:drink:cheese".to_string();
    let (cheese_gen, cheese_hash) = heads[&cheese_id].clone();

    // —— kernel 批量校验接口直接断言（§10）——
    // —— Direct assertions on the kernel bulk-validation API (§10) ——
    // 单页单 payload 与 head 一致 → 有效。
    // One page, one payload matching the head → valid.
    assert_eq!(
        kernel
            .validate_vector_payloads(&[(
                oolong_id.clone(),
                oolong_hash.clone(),
                oolong_gen as u64
            )])
            .unwrap(),
        HashSet::from([oolong_id.clone()])
    );
    // 旧 hash / 旧 generation / 不存在页 → 无效。
    // Stale hash / stale generation / nonexistent page → invalid.
    assert!(kernel
        .validate_vector_payloads(&[(oolong_id.clone(), "deadbeef".into(), oolong_gen as u64)])
        .unwrap()
        .is_empty());
    assert!(kernel
        .validate_vector_payloads(&[(
            oolong_id.clone(),
            oolong_hash.clone(),
            oolong_gen as u64 + 1
        )])
        .unwrap()
        .is_empty());
    assert!(kernel
        .validate_vector_payloads(&[("milk-tea:drink:ghost".into(), "whatever".into(), 1)])
        .unwrap()
        .is_empty());
    // 同页混入旧代 payload（部分匹配）→ 整页判无效，杜绝旧代 chunk 冒充。
    // A page mixing a stale payload (partial match) → invalid as a whole, so a
    // stale chunk can never ride on its sibling's valid payload.
    assert!(kernel
        .validate_vector_payloads(&[
            (oolong_id.clone(), oolong_hash.clone(), oolong_gen as u64),
            (oolong_id.clone(), "deadbeef".into(), oolong_gen as u64),
        ])
        .unwrap()
        .is_empty());
    // 多页各配一致 payload → 全部有效。
    // Multiple pages with uniform matching payloads → all valid.
    assert_eq!(
        kernel
            .validate_vector_payloads(&[
                (oolong_id.clone(), oolong_hash.clone(), oolong_gen as u64),
                (cheese_id.clone(), cheese_hash.clone(), cheese_gen as u64),
            ])
            .unwrap(),
        HashSet::from([oolong_id.clone(), cheese_id.clone()])
    );

    // —— 引擎融合前校验（RRF/top_k 之前丢弃）——
    // —— Engine pre-fusion validation (dropped before RRF/top_k) ——
    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .unwrap();
    store
        .upsert(
            "milk-tea",
            &[
                // 有效：两 chunk 同 payload，与 accepted head 一致。
                // Valid: two chunks sharing one payload matching the head.
                point(
                    "a-chunk1",
                    &oolong_id,
                    oolong_gen as u64,
                    &oolong_hash,
                    "乌龙",
                ),
                point(
                    "a-chunk2",
                    &oolong_id,
                    oolong_gen as u64,
                    &oolong_hash,
                    "乌龙",
                ),
                // 旧 hash → 该页唯一 payload 陈旧 → 整页被拒。
                // Stale hash → the page's only payload is stale → rejected.
                point(
                    "b-stale-hash",
                    &osman_id,
                    osman_gen as u64,
                    "deadbeef",
                    "乌龙",
                ),
                // 缺版本 metadata（空 hash / generation=0）→ 先被丢弃。
                // Missing version metadata (empty hash / generation=0) → dropped first.
                point("b-no-meta", &osman_id, 0, "", "乌龙"),
                // 不存在页。
                // Nonexistent page.
                point("c-ghost", "milk-tea:drink:ghost", 1, "whatever", "乌龙"),
                // 旧 generation。
                // Stale generation.
                point(
                    "d-wrong-gen",
                    &cheese_id,
                    cheese_gen as u64 + 1,
                    &cheese_hash,
                    "乌龙",
                ),
            ],
        )
        .await
        .unwrap();

    let engine = QueryEngine::new(
        kernel.clone(),
        store,
        None,
        Arc::new(TestEmbedder),
        "milk-tea",
        5,
        60,
    )
    .unwrap();
    let query = Query {
        text: "乌龙奶茶".to_string(),
        filters: Filters::empty(),
        top_k: 10,
        domain: Some("milk-tea".into()),
    };
    let result = engine.search(&query).await.unwrap();
    // vector_count = 融合前的有效向量数（4 个无效 hit 已被丢弃）。
    // vector_count = valid vectors before fusion (the 4 invalid hits were
    // dropped).
    assert_eq!(result.diagnostics.vector_count, 2);
    let hit_pages: HashSet<String> = result.hits.iter().map(|h| h.page_id.clone()).collect();
    assert!(
        hit_pages.contains(&oolong_id),
        "valid vectors survive, got {hit_pages:?}"
    );
    assert!(!hit_pages.contains(&osman_id), "stale-hash page rejected");
    assert!(
        !hit_pages.contains(&cheese_id),
        "stale-generation page rejected"
    );
    assert!(!hit_pages.contains("milk-tea:drink:ghost"));
}

// A21：quarantine 页的向量（hash/generation 仍一致）在融合前被拒。
// A21: a quarantined page's vector (hash/generation still matching) is rejected
// before fusion.
#[tokio::test]
async fn a21_quarantined_vector_payload_rejected() {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    kernel
        .seed_pages(
            &page("cheese", "芝士奶盖茶", "芝士奶盖茶以咸香芝士奶盖著称。"),
            "milk-tea",
            PublishStatus::Accepted,
        )
        .unwrap();
    let published = kernel.list_published_pages("milk-tea").unwrap();
    let (page_id, generation, hash, _model) = published[0].clone();

    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .unwrap();
    store
        .upsert(
            "milk-tea",
            &[point("v1", &page_id, generation as u64, &hash, "芝士")],
        )
        .await
        .unwrap();

    let engine = QueryEngine::new(
        kernel.clone(),
        store,
        None,
        Arc::new(TestEmbedder),
        "milk-tea",
        5,
        60,
    )
    .unwrap();
    let query = Query {
        text: "芝士奶盖".to_string(),
        filters: Filters::empty(),
        top_k: 10,
        domain: Some("milk-tea".into()),
    };
    let result = engine.search(&query).await.unwrap();
    assert_eq!(
        result.diagnostics.vector_count, 1,
        "accepted payload is valid"
    );
    assert!(!result.hits.is_empty());

    // accepted → quarantined：向量虽 hash/generation 一致，也必须在融合前被拒。
    // accepted → quarantined: even with a matching hash/generation the vector
    // must be rejected before fusion.
    kernel
        .execute_batch(&format!(
            "UPDATE pages SET status = 'quarantined' WHERE page_id = '{}'",
            page_id.replace('\'', "''")
        ))
        .unwrap();
    let result = engine.search(&query).await.unwrap();
    assert_eq!(
        result.diagnostics.vector_count, 0,
        "a quarantined page's vector must be rejected"
    );
    assert!(
        result.hits.is_empty(),
        "FTS also drops the quarantined page"
    );
}
