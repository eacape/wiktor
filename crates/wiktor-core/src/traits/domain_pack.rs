use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::Result;
use crate::types::FieldDefinition;

/// 领域包（YAML 配置 + Prompt 模板 + 页面模板三件套）。
pub trait DomainPack: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn config(&self) -> &DomainConfig;
    fn compiler(&self) -> Result<Box<dyn Compiler>>;
    fn qug_builder(&self) -> Result<Box<dyn QugBuilder>>;
    fn reranker(&self) -> Result<Option<Box<dyn Reranker>>>;
}

/// 领域包配置（从 domain.yaml 解析）。
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: String,
    pub version: String,
    pub entities: Vec<EntityConfig>,
    pub quality_threshold: f32,
    pub max_recompiles: usize,
}

/// 实体配置。
#[derive(Debug, Clone)]
pub struct EntityConfig {
    pub name: String,
    /// 数据源 URI（jsonl:// 或 postgres://）。
    pub source: String,
    pub id_field: String,
    pub type_field: String,
    pub fields: Vec<FieldDefinition>,
}
