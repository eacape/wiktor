//! qdrant 集成测试（默认 `#[ignore]`，需本机/远程 qdrant 服务）。
//! qdrant integration tests (ignored by default; require a local or remote qdrant service).
//!
//! 运行方式：
//! Run with:
//!   WIKTOR_QDRANT_URL=http://127.0.0.1:6334 cargo test --features vector-qdrant -- --include-ignored
//!
//! 连接失败时 skip 而非失败（CI 可能无 qdrant）。
//! Connection failures skip rather than fail (CI may not provide qdrant).

use wiktor_core::kernel::QdrantVectorStore;
use wiktor_core::traits::{ChunkType, DistanceMetric, VectorMetadata, VectorPoint, VectorStore};
use wiktor_core::types::EntityId;
use wiktor_core::Error;

fn env_url() -> String {
    std::env::var("WIKTOR_QDRANT_URL").unwrap_or_else(|_| "http://127.0.0.1:6334".to_string())
}

fn api_key() -> Option<String> {
    std::env::var("WIKTOR_QDRANT_API_KEY").ok()
}

fn meta(entity: &str, generation: u64) -> VectorMetadata {
    VectorMetadata {
        entity_id: entity.to_string(),
        page_id: format!("page/{entity}"),
        chunk_type: ChunkType::Summary,
        content_hash: "hash".into(),
        generation,
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test_{nanos}")
}

#[tokio::test]
#[ignore]
async fn qdrant_ensure_upsert_search_delete_roundtrip() {
    let store = match QdrantVectorStore::from_config(&env_url(), api_key().as_deref(), 4) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: cannot connect to qdrant at {}: {e}", env_url());
            return;
        }
    };
    let collection = rand_suffix();
    store
        .ensure_collection(&collection, 4, DistanceMetric::Cosine)
        .await
        .unwrap();

    let a = EntityId::new("ecommerce", "product", "a").unwrap();
    let b = EntityId::new("ecommerce", "product", "b").unwrap();
    store
        .upsert(
            &collection,
            &[
                VectorPoint {
                    id: QdrantVectorStore::point_id(&a, "summary", 1),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                    metadata: meta(&a.to_key(), 1),
                },
                VectorPoint {
                    id: QdrantVectorStore::point_id(&b, "summary", 1),
                    vector: vec![0.0, 1.0, 0.0, 0.0],
                    metadata: meta(&b.to_key(), 1),
                },
            ],
        )
        .await
        .unwrap();

    // 无候选：top1 应为 a
    // No candidates: top1 should be a
    let hits = store
        .search(&collection, &[1.0, 0.0, 0.0, 0.0], 1, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].metadata.entity_id, a.to_key());

    // 候选域只含 b：应只返回 b
    // Candidate scope contains only b: only b should be returned
    let hits = store
        .search(
            &collection,
            &[1.0, 0.0, 0.0, 0.0],
            10,
            Some(std::slice::from_ref(&b)),
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].metadata.entity_id, b.to_key());

    // 删除后为空
    // Empty after deletion
    store
        .delete(
            &collection,
            &[QdrantVectorStore::point_id(&a, "summary", 1)],
        )
        .await
        .unwrap();
    let hits = store
        .search(&collection, &[1.0, 0.0, 0.0, 0.0], 10, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);

    // 清理
    // Cleanup
    store.recreate_collection(&collection, 4).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn qdrant_missing_collection_search_errors() {
    let store = match QdrantVectorStore::from_config(&env_url(), api_key().as_deref(), 4) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: cannot connect to qdrant at {}: {e}", env_url());
            return;
        }
    };
    let res = store
        .search(
            &format!("nonexistent_{}", rand_suffix()),
            &[1.0; 4],
            1,
            None,
        )
        .await;
    assert!(matches!(res, Err(Error::VectorStore(_))));
}
