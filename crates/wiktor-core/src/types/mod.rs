//! 领域类型模块。
//! Domain types module.
//!
//! 承载两平面模型与查询/反馈路径所需的公共数据类型，无外部依赖：
//! 实体（`entity`）、页面（`page`）、查询（`query`）、QUG（`qug`）与
//! 错误（`error`）。模块内同时聚合部分与事实平面过滤下推相关的载荷
//! 类型（`FactValue` / `Facts` / `Filters` / `FilterCondition`）。
//! Holds the shared data types for the two-plane model and the query/feedback paths,
//! with no external dependencies: entity (`entity`), page (`page`), query (`query`),
//! QUG (`qug`) and error (`error`). It also aggregates payload types related to
//! fact-plane filter pushdown (`FactValue` / `Facts` / `Filters` / `FilterCondition`).

mod entity;
pub mod error;
mod page;
mod query;
mod qug;

pub use entity::{EntityId, RawEntity};
pub use error::{Error, Result};
pub use page::{
    CompileContext, CompiledPage, PageMetadata, PublishStatus, QualityScore, Section, WikiPage,
};
pub use query::{Cursor, Query, QueryLog, RewrittenQuery, SearchHit};
pub use qug::{QugEdge, QugPath};

/// 反馈层建议的补充编译任务（进人工审核队列）。
/// Supplementary compile task suggested by the feedback layer (goes to the human review queue).
#[derive(Debug, Clone)]
pub struct CompileTask {
    pub entity_id: EntityId,
    pub source_revision: u64,
    pub domain_pack_version: String,
}

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 事实平面中某个 field 的值（filterable 字段）。
/// Value of a field in the fact plane (filterable fields).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FactValue {
    Numeric(f64),
    Text(String),
    Boolean(bool),
    RefList(Vec<String>),
    Timestamp(i64),
}

/// 实体事实（事实平面写入载荷）。
/// Entity facts (fact-plane write payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Facts {
    pub entity_id: EntityId,
    pub fields: BTreeMap<String, FactValue>,
    pub source_revision: u64,
}

/// 过滤条件（事实平面下推 + 向量候选域共用）。
/// Filter conditions (shared by fact-plane pushdown and the vector candidate scope).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    pub conditions: Vec<FilterCondition>,
}

impl Filters {
    pub fn empty() -> Self {
        Self {
            conditions: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }
}

/// 单条过滤条件。
/// A single filter condition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterCondition {
    NumericRange {
        field: String,
        min: Option<f64>,
        max: Option<f64>,
    },
    TextEquals {
        field: String,
        value: String,
    },
    RefContains {
        field: String,
        refs: Vec<String>,
    },
    RefExcludes {
        field: String,
        refs: Vec<String>,
    },
}

/// 源数据字段定义（DataSource::schema 返回）。
/// Source-data field definition (returned by DataSource::schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: FieldType,
    pub filterable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Numeric,
    Text,
    Boolean,
    /// 事实表 CHECK 约束用 'reflist'，故 serde 显式 rename（rename_all 会产出 ref_list）。
    /// The facts table CHECK constraint uses 'reflist', so serde renames it explicitly
    /// (rename_all would yield `ref_list`).
    #[serde(rename = "reflist")]
    RefList,
    Timestamp,
}
