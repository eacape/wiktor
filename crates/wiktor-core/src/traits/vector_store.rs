use crate::types::error::Result;
use crate::types::EntityId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 向量库适配器（插件点 3：默认 qdrant；Mock 供无网络单测）。
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// 幂等确保 collection 存在（不存在则创建）。
    async fn ensure_collection(
        &self,
        collection: &str,
        dimension: usize,
        distance: DistanceMetric,
    ) -> Result<()>;

    /// 批量插入或更新向量（按点 id 幂等覆盖）。
    async fn upsert(&self, collection: &str, points: &[VectorPoint]) -> Result<()>;

    /// 向量检索；`candidate_ids` 是事实平面预筛的候选实体域（按 payload.entity_id 过滤）。
    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<VectorHit>>;

    /// 删除向量（按点 id）。
    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()>;

    /// 可重建语义：删库重建（派生索引，不涉及数据迁移）。
    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()>;
}

/// 向量点（写入载荷）。
#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub id: String,
    pub vector: Vec<f32>,
    pub metadata: VectorMetadata,
}

/// 向量点元数据（存储于 qdrant payload；也是可重建/recreate 的依据）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMetadata {
    /// EntityId.to_key()。
    pub entity_id: String,
    pub page_id: String,
    pub chunk_type: ChunkType,
    /// BLAKE3 哈希（用于重建验证）。
    pub content_hash: String,
    /// SQLite generation（用于对齐）。
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkType {
    /// 页面摘要级。
    Summary,
    /// 章节级。
    Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    DotProduct,
}

/// 向量检索命中（向量层中间结果；查询层融合后为 SearchHit）。
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: String,
    pub score: f32,
    pub metadata: VectorMetadata,
}
