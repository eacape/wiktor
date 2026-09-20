use crate::types::error::Result;
use crate::types::{CompiledPage, Query, QugEdge, QugPath, RewrittenQuery};
use async_trait::async_trait;

/// 查询理解图（编译时构建、查询时只读遍历）。
#[async_trait]
pub trait QueryUnderstandingGraph: Send + Sync {
    /// 查询改写；返回 None = QUG 无法处理，调用方必须 fallback 到混合检索。
    async fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>>;
    fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath>;
}

/// QUG 构建器（从编译产物与领域包配置构建图）。
#[async_trait]
pub trait QugBuilder: Send + Sync {
    async fn extract_edges(&self, pages: &[CompiledPage]) -> Result<Vec<QugEdge>>;
    async fn build_graph(&self, edges: Vec<QugEdge>) -> Result<Box<dyn QueryUnderstandingGraph>>;
}
