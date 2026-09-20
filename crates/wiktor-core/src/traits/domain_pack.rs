use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::Result;
use crate::types::FieldDefinition;
use serde::Deserialize;

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
///
/// YAML 形如：
/// ```yaml
/// name: milk-tea
/// version: "0.1.0"
/// entities: [...]
/// compile:
///   quality_threshold: 0.75
///   max_recompiles: 2
/// query:
///   filters: [price, sugar_level, size, ingredient_ids]
/// ```
/// 自定义 `Deserialize` 把嵌套的 `compile` / `query` 段拍平到本结构，
/// 保持既有字段（quality_threshold / max_recompiles）不变，仅新增 query_filters。
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: String,
    pub version: String,
    pub entities: Vec<EntityConfig>,
    pub quality_threshold: f32,
    pub max_recompiles: usize,
    /// `query.filters` 过滤白名单（Step 2 仅解析提示，不强制消费）。
    pub query_filters: Vec<String>,
}

impl<'de> Deserialize<'de> for DomainConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "snake_case")]
        struct CompileSection {
            #[serde(default = "default_threshold")]
            quality_threshold: f32,
            #[serde(default)]
            max_recompiles: usize,
        }
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "snake_case")]
        struct QuerySection {
            #[serde(default)]
            filters: Vec<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        struct Repr {
            name: String,
            version: String,
            #[serde(default)]
            entities: Vec<EntityConfig>,
            #[serde(default)]
            compile: CompileSection,
            #[serde(default)]
            query: QuerySection,
        }
        fn default_threshold() -> f32 {
            0.75
        }

        let r = Repr::deserialize(deserializer)?;
        Ok(DomainConfig {
            name: r.name,
            version: r.version,
            entities: r.entities,
            quality_threshold: r.compile.quality_threshold,
            max_recompiles: r.compile.max_recompiles,
            query_filters: r.query.filters,
        })
    }
}

/// 实体配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EntityConfig {
    pub name: String,
    /// 数据源 URI（jsonl:// 或 postgres://）。
    pub source: String,
    pub id_field: String,
    pub type_field: String,
    pub fields: Vec<FieldDefinition>,
}
