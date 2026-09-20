use crate::types::{Query, SearchHit};
use async_trait::async_trait;

/// 重排器（cross_encoder，默认关闭）。
#[async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(&self, query: &Query, hits: Vec<SearchHit>) -> Vec<SearchHit>;
}
