use crate::types::entity::EntityId;
use crate::types::Filters;
use serde::{Deserialize, Serialize};

/// 用户查询请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,
    pub filters: Filters,
    pub top_k: usize,
    pub domain: Option<String>,
}

/// QUG 改写后的查询（rewrite 返回 None 表示 QUG 无法处理，需 fallback 混合检索）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenQuery {
    pub expanded_terms: Vec<String>,
    pub filters: Filters,
    pub boost_entities: Vec<EntityId>,
}

/// 检索命中（查询层融合结果；向量层的中间命中是 VectorHit）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub page_id: String,
    pub entity_id: EntityId,
    pub score: f32,
}

/// 查询日志（反馈层输入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLog {
    pub query: Query,
    pub rewritten: Option<RewrittenQuery>,
    pub hits: Vec<SearchHit>,
    pub rewrite_failure: bool,
    pub latency_ms: u64,
    /// Unix 时间戳（秒）。
    pub timestamp: i64,
}

/// 数据源分页游标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub offset: usize,
    pub batch_size: usize,
}
