use crate::types::{FilterCondition, Query};
use serde::{Deserialize, Serialize};

/// QUG 五类语义边（边类型通用，具体边由领域包实例化）。
/// Five QUG semantic edge types (generic edge types instantiated by domain packs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QugEdge {
    /// 同义边：查询时直接替换。
    /// Synonym edge: replace directly during query processing.
    Synonym { from: String, to: Vec<String> },
    /// 上下位边：品类扩展召回。
    /// Hyponym edge: expand category recall.
    Hyponym { child: String, parent: String },
    /// 属性传播边：转为结构化过滤，落事实平面。
    /// Attribute-propagation edge: convert to a structured filter in the fact plane.
    AttributePropagation {
        phrase: String,
        filter: FilterCondition,
    },
    /// 意图模板边：展开为复合查询。
    /// Intent-template edge: expand into a compound query.
    IntentTemplate { phrase: String, expansion: Query },
    /// 否定边：生成排除过滤器。
    /// Negation edge: generate an exclusion filter.
    Negation {
        phrase: String,
        exclusion: FilterCondition,
    },
}

/// QUG 图遍历路径。
/// QUG graph traversal path.
#[derive(Debug, Clone)]
pub struct QugPath {
    pub nodes: Vec<String>,
    pub edges: Vec<QugEdge>,
    pub depth: usize,
}
