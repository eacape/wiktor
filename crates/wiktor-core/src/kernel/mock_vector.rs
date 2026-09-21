use crate::traits::{DistanceMetric, VectorHit, VectorMetadata, VectorPoint, VectorStore};
use crate::types::error::{Error, Result};
use crate::types::EntityId;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// 内存稀疏向量库：无网络、暴力余弦扫描，供单测与测试环境使用。
/// In-memory sparse vector store: offline brute-force cosine scan for unit tests and test environments.
#[derive(Clone, Default)]
pub struct MockVectorStore {
    inner: Arc<RwLock<HashMap<String, MockCollection>>>,
}

#[derive(Default)]
struct MockCollection {
    points: HashMap<String, MockPoint>,
}

struct MockPoint {
    vector: Vec<f32>,
    metadata: VectorMetadata,
}

impl MockVectorStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum();
        let norm_b: f32 = b.iter().map(|x| x * x).sum();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a.sqrt() * norm_b.sqrt())
        }
    }
}

#[async_trait]
impl VectorStore for MockVectorStore {
    async fn ensure_collection(
        &self,
        collection: &str,
        _dimension: usize,
        _distance: DistanceMetric,
    ) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        inner.entry(collection.to_string()).or_default();
        Ok(())
    }

    async fn upsert(&self, collection: &str, points: &[VectorPoint]) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        let coll = inner.entry(collection.to_string()).or_default();
        for p in points {
            coll.points.insert(
                p.id.clone(),
                MockPoint {
                    vector: p.vector.clone(),
                    metadata: p.metadata.clone(),
                },
            );
        }
        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<VectorHit>> {
        let inner = self.inner.read().unwrap();
        let coll = inner
            .get(collection)
            .ok_or_else(|| Error::VectorStore(format!("collection not found: {collection}")))?;

        let candidate_keys: Option<Vec<String>> =
            candidate_ids.map(|ids| ids.iter().map(|e| e.to_key()).collect());

        let mut scored: Vec<(String, f32, VectorMetadata)> = Vec::new();
        for (id, point) in &coll.points {
            if let Some(keys) = &candidate_keys {
                if !keys.contains(&point.metadata.entity_id) {
                    continue;
                }
            }
            let score = Self::cosine_similarity(query_vector, &point.vector);
            scored.push((id.clone(), score, point.metadata.clone()));
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(id, score, metadata)| VectorHit {
                id,
                score,
                metadata,
            })
            .collect())
    }

    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        if let Some(coll) = inner.get_mut(collection) {
            for id in ids {
                coll.points.remove(id);
            }
        }
        Ok(())
    }

    async fn recreate_collection(&self, collection: &str, _dimension: usize) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        inner.insert(collection.to_string(), MockCollection::default());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(entity: &str) -> VectorMetadata {
        VectorMetadata {
            entity_id: entity.to_string(),
            page_id: format!("page/{entity}"),
            chunk_type: crate::traits::ChunkType::Summary,
            content_hash: "hash".into(),
            generation: 1,
        }
    }

    #[tokio::test]
    async fn upsert_search_top_k_ordering() {
        let store = MockVectorStore::new();
        store
            .ensure_collection("c", 3, DistanceMetric::Cosine)
            .await
            .unwrap();
        store
            .upsert(
                "c",
                &[
                    VectorPoint {
                        id: "1".into(),
                        vector: vec![1.0, 0.0, 0.0],
                        metadata: meta("a"),
                    },
                    VectorPoint {
                        id: "2".into(),
                        vector: vec![0.0, 1.0, 0.0],
                        metadata: meta("b"),
                    },
                    VectorPoint {
                        id: "3".into(),
                        vector: vec![0.0, 0.0, 1.0],
                        metadata: meta("c"),
                    },
                ],
            )
            .await
            .unwrap();

        let hits = store.search("c", &[0.9, 0.1, 0.0], 2, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "1");
        assert_eq!(hits[1].id, "2");
    }

    #[tokio::test]
    async fn search_respects_candidate_ids() {
        let store = MockVectorStore::new();
        store
            .ensure_collection("c", 2, DistanceMetric::Cosine)
            .await
            .unwrap();
        let key_a = "dom:prod:a".to_string();
        let key_b = "dom:prod:b".to_string();
        store
            .upsert(
                "c",
                &[
                    VectorPoint {
                        id: "1".into(),
                        vector: vec![1.0, 0.0],
                        metadata: meta(&key_a),
                    },
                    VectorPoint {
                        id: "2".into(),
                        vector: vec![0.0, 1.0],
                        metadata: meta(&key_b),
                    },
                ],
            )
            .await
            .unwrap();

        let a = EntityId::from_key(&key_a).unwrap();
        let hits = store
            .search("c", &[1.0, 0.0], 10, Some(&[a]))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.entity_id, key_a);
    }

    #[tokio::test]
    async fn delete_removes_point() {
        let store = MockVectorStore::new();
        store
            .ensure_collection("c", 2, DistanceMetric::Cosine)
            .await
            .unwrap();
        store
            .upsert(
                "c",
                &[VectorPoint {
                    id: "1".into(),
                    vector: vec![1.0, 0.0],
                    metadata: meta("a"),
                }],
            )
            .await
            .unwrap();
        store.delete("c", &["1".into()]).await.unwrap();
        let hits = store.search("c", &[1.0, 0.0], 10, None).await.unwrap();
        assert!(hits.is_empty());
    }
}
