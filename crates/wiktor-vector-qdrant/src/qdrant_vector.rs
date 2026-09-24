use async_trait::async_trait;
use qdrant_client::qdrant::point_id::PointIdOptions;
use qdrant_client::qdrant::value::Kind;
use qdrant_client::qdrant::{
    with_payload_selector, CollectionExistsRequest, Condition, CreateCollectionBuilder,
    DeletePointsBuilder, Distance, Filter, PointId, PointStruct, SearchPointsBuilder,
    UpsertPointsBuilder, Value, VectorParamsBuilder,
};
use qdrant_client::Qdrant;
use wiktor_core::traits::{DistanceMetric, VectorHit, VectorMetadata, VectorPoint, VectorStore};
use wiktor_core::types::error::{Error, Result};
use wiktor_core::types::EntityId;

/// Qdrant 向量后端（v3.2 起为 Wiktor 默认向量服务；本 crate 从 wiktor-core 拆出）。
/// Qdrant vector backend (the default Wiktor vector service since v3.2; split out
/// of wiktor-core into this plugin crate, STEP10 B3).
///
/// 设计要点：
/// Design points:
/// - 集合命名由调用方（QueryEngine）负责，形如 `{prefix}_{domain}_{version}_gen{generation}`；
/// - Collection naming is the caller's (QueryEngine) responsibility, shaped `{prefix}_{domain}_{version}_gen{generation}`;
/// - point id 必须是合法 UUID，由 `point_id()` 从 BLAKE3 确定性派生，同一
/// - point ids must be valid UUIDs, deterministically derived from BLAKE3 by `point_id()`, so the same
///   (entity, chunk_type, generation) 幂等覆盖；
///   (entity, chunk_type, generation) is idempotently overwritten;
/// - 候选 ID 过滤按 **payload.entity_id** 做关键词匹配（不能用 `Condition::has_id`，
/// - candidate-ID filtering uses keyword matching on **payload.entity_id** (not `Condition::has_id`,
///   那是按 point id 过滤，候选是实体 ID）。
///   which filters by point id; candidates are entity IDs).
pub struct QdrantVectorStore {
    client: Qdrant,
    default_dimension: usize,
}

impl QdrantVectorStore {
    /// 连接 qdrant（URL 默认指向 gRPC 端口，如 `http://127.0.0.1:6334`）。
    /// Connects to qdrant (the URL defaults to the gRPC port, e.g. `http://127.0.0.1:6334`).
    pub fn from_config(url: &str, api_key: Option<&str>, dimension: usize) -> Result<Self> {
        let mut cfg = qdrant_client::config::QdrantConfig::from_url(url);
        if let Some(key) = api_key {
            cfg = cfg.api_key(key);
        }
        let client = cfg
            .build()
            .map_err(|e| Error::QdrantConnection(e.to_string()))?;
        Ok(Self {
            client,
            default_dimension: dimension,
        })
    }

    /// 便捷构造（无 API key）。
    /// Convenience constructor (no API key).
    pub fn connect(url: &str, dimension: usize) -> Result<Self> {
        Self::from_config(url, None, dimension)
    }

    /// 确定性 point id：BLAKE3 前 16 字节 → UUID。
    /// Deterministic point id: first 16 bytes of BLAKE3 → UUID.
    pub fn point_id(entity_id: &EntityId, chunk_type: &str, generation: u64) -> String {
        let digest = blake3::hash(
            format!("{}|{}|{}", entity_id.to_key(), chunk_type, generation).as_bytes(),
        );
        let b = digest.as_bytes();
        uuid::Uuid::from_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15],
        ])
        .to_string()
    }

    fn map_distance(d: DistanceMetric) -> Distance {
        match d {
            DistanceMetric::Cosine => Distance::Cosine,
            DistanceMetric::Euclidean => Distance::Euclid,
            DistanceMetric::DotProduct => Distance::Dot,
        }
    }

    /// 集合命名（MVP：调用方也可自行命名，此为主推约定）。
    /// Collection naming (MVP: callers may name it themselves; this is the recommended convention).
    pub fn collection_name(prefix: &str, domain: &str, version: &str, generation: u64) -> String {
        let v = version.replace('.', "_");
        format!("{prefix}_{domain}_{v}_gen{generation}")
    }

    pub fn client(&self) -> &Qdrant {
        &self.client
    }
}

#[async_trait]
impl VectorStore for QdrantVectorStore {
    async fn ensure_collection(
        &self,
        collection: &str,
        dimension: usize,
        distance: DistanceMetric,
    ) -> Result<()> {
        let exists = self
            .client
            .collection_exists(CollectionExistsRequest {
                collection_name: collection.to_string(),
            })
            .await
            .map_err(|e| Error::VectorStore(e.to_string()))?;

        if !exists {
            self.client
                .create_collection(CreateCollectionBuilder::new(collection).vectors_config(
                    VectorParamsBuilder::new(dimension as u64, Self::map_distance(distance)),
                ))
                .await
                .map_err(|e| Error::VectorStore(e.to_string()))?;
        }
        Ok(())
    }

    async fn upsert(&self, collection: &str, points: &[VectorPoint]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let structs: Vec<PointStruct> = points
            .iter()
            .map(|p| {
                PointStruct::new(
                    p.id.clone(),
                    p.vector.clone(),
                    metadata_to_payload(&p.metadata),
                )
            })
            .collect();

        self.client
            .upsert_points(UpsertPointsBuilder::new(collection, structs))
            .await
            .map_err(|e| Error::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<VectorHit>> {
        let mut builder = SearchPointsBuilder::new(collection, query_vector.to_vec(), top_k as u64)
            .with_payload(with_payload_selector::SelectorOptions::Enable(true));

        if let Some(ids) = candidate_ids {
            let keys: Vec<String> = ids.iter().map(|e| e.to_key()).collect();
            // payload.entity_id ∈ keys（Keywords 匹配，qdrant 会建 OR 组）
            // payload.entity_id ∈ keys (Keywords match; qdrant builds an OR group)
            let cond = Condition::matches("entity_id", keys);
            builder = builder.filter(Filter::must([cond]));
        }

        let resp = self
            .client
            .search_points(builder)
            .await
            .map_err(|e| Error::VectorStore(e.to_string()))?;

        Ok(resp
            .result
            .into_iter()
            .map(|point| {
                let payload = &point.payload;
                let id = point
                    .id
                    .and_then(|id| id.point_id_options)
                    .map(|opt| match opt {
                        PointIdOptions::Uuid(u) => u,
                        PointIdOptions::Num(n) => n.to_string(),
                    })
                    .unwrap_or_default();
                VectorHit {
                    id,
                    score: point.score,
                    metadata: payload_to_metadata(payload),
                }
            })
            .collect())
    }

    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let point_ids: Vec<PointId> = ids
            .iter()
            .map(|s| PointId {
                point_id_options: Some(PointIdOptions::Uuid(s.clone())),
            })
            .collect();

        let mut b = DeletePointsBuilder::new(collection);
        b = b.points(point_ids);
        self.client
            .delete_points(b)
            .await
            .map_err(|e| Error::VectorStore(e.to_string()))?;
        Ok(())
    }

    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()> {
        let _ = self.client.delete_collection(collection).await;
        self.ensure_collection(collection, dimension, DistanceMetric::Cosine)
            .await
    }
}

/// VectorMetadata → qdrant payload（字符串/整数 value；关键词可被 Condition::matches 命中）。
/// VectorMetadata → qdrant payload (string/integer values; keywords matchable by Condition::matches).
fn metadata_to_payload(meta: &VectorMetadata) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();
    map.insert(
        "entity_id".to_string(),
        Value {
            kind: Some(Kind::StringValue(meta.entity_id.clone())),
        },
    );
    map.insert(
        "page_id".to_string(),
        Value {
            kind: Some(Kind::StringValue(meta.page_id.clone())),
        },
    );
    map.insert(
        "chunk_type".to_string(),
        Value {
            kind: Some(Kind::StringValue(
                format!("{:?}", meta.chunk_type).to_lowercase(),
            )),
        },
    );
    map.insert(
        "content_hash".to_string(),
        Value {
            kind: Some(Kind::StringValue(meta.content_hash.clone())),
        },
    );
    map.insert(
        "generation".to_string(),
        Value {
            kind: Some(Kind::IntegerValue(meta.generation as i64)),
        },
    );
    map
}

/// qdrant payload → VectorMetadata（缺失字段给默认值，避免解析失败）。
/// qdrant payload → VectorMetadata (missing fields get defaults to avoid parse failures).
fn payload_to_metadata(payload: &std::collections::HashMap<String, Value>) -> VectorMetadata {
    fn str_of(v: Option<&Value>) -> String {
        match v.and_then(|v| v.kind.as_ref()) {
            Some(Kind::StringValue(s)) => s.clone(),
            _ => String::new(),
        }
    }
    fn u64_of(v: Option<&Value>) -> u64 {
        match v.and_then(|v| v.kind.as_ref()) {
            Some(Kind::IntegerValue(i)) => *i as u64,
            _ => 0,
        }
    }
    VectorMetadata {
        entity_id: str_of(payload.get("entity_id")),
        page_id: str_of(payload.get("page_id")),
        chunk_type: match str_of(payload.get("chunk_type")).as_str() {
            "section" => wiktor_core::traits::ChunkType::Section,
            _ => wiktor_core::traits::ChunkType::Summary,
        },
        content_hash: str_of(payload.get("content_hash")),
        generation: u64_of(payload.get("generation")),
    }
}

// 保留 default_dimension 供后续默认集合创建使用。
// Keep default_dimension for later default-collection creation.
#[allow(dead_code)]
impl QdrantVectorStore {
    fn default_dimension(&self) -> usize {
        self.default_dimension
    }
}
