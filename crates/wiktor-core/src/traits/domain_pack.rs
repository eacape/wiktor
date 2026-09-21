use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::{Error, Result};
use crate::types::{FieldDefinition, FieldType, Filters};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// 领域包（YAML 配置 + Prompt 模板 + 页面模板三件套）。
/// Domain pack (a trio: YAML config + Prompt templates + page templates).
pub trait DomainPack: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn config(&self) -> &DomainConfig;
    fn compiler(&self) -> Result<Box<dyn Compiler>>;
    fn qug_builder(&self) -> Result<Box<dyn QugBuilder>>;
    fn reranker(&self) -> Result<Option<Box<dyn Reranker>>>;
}

/// 领域包配置（从 domain.yaml 解析）。
/// Domain-pack configuration (parsed from domain.yaml).
///
/// YAML 形如：
/// YAML shape:
/// ```yaml
/// name: milk-tea
/// version: "0.1.0"
/// entities: [...]
/// compile:
///   quality_threshold: 0.75
///   max_recompiles: 2
/// query:
///   filters: [price, sugar_level, size, ingredient_ids]
/// qug:
///   enabled: true
///   max_depth: 2
///   candidate_multiplier: 5
///   intent_templates: intents.yaml
/// ```
/// 自定义 `Deserialize` 把嵌套的 `compile` / `query` / `qug` 段拍平到本结构，
/// 保持既有字段（quality_threshold / max_recompiles）不变，仅新增 query_filters
/// 与 qug；缺少 `qug` 段时使用 Step 2 兼容默认值（enabled=false）。
/// A custom `Deserialize` flattens the nested `compile` / `query` / `qug` sections
/// into this struct, keeping existing fields (quality_threshold / max_recompiles)
/// unchanged and adding query_filters and qug; a missing `qug` section falls back to
/// Step 2-compatible defaults (enabled=false).
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: String,
    pub version: String,
    pub entities: Vec<EntityConfig>,
    pub quality_threshold: f32,
    pub max_recompiles: usize,
    /// `query.filters` 过滤白名单（Step 2 仅解析提示，不强制消费）。
    /// Whitelist of `query.filters` (Step 2 only parses this as a hint, does not enforce it).
    pub query_filters: Vec<String>,
    /// `qug` 段（Step 3：QUG 开关/深度/候选倍率/意图模板文件）。
    /// The `qug` section (Step 3: QUG toggle/depth/candidate multiplier/intent template file).
    pub qug: QugConfig,
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
            #[serde(default)]
            qug: QugConfig,
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
            qug: r.qug,
        })
    }
}

impl DomainConfig {
    /// 查询 field 在 `entities[].fields` 中声明的类型（意图校验用）。
    /// Looks up the declared type of a field in `entities[].fields` (used by intent validation).
    pub fn field_type(&self, name: &str) -> Option<FieldType> {
        self.entities
            .iter()
            .flat_map(|e| e.fields.iter())
            .find(|f| f.name == name)
            .map(|f| f.field_type)
    }
}

/// QUG 段配置（`qug:`，缺失时全部走 Step 2 兼容默认值）。
/// QUG section config (`qug:`; missing section falls back to Step 2-compatible defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct QugConfig {
    /// QUG 总开关；关闭时引擎不调用 rewrite（诊断状态为 disabled）。
    /// Master switch; when off the engine skips rewrite (diagnostics report `disabled`).
    #[serde(default = "default_qug_enabled")]
    pub enabled: bool,
    /// 图遍历深度上限（默认 2，硬上限 4，超过上限在构图时报配置错误）。
    /// Graph traversal depth cap (default 2, hard cap 4; exceeding the cap is a config error at build time).
    #[serde(default = "default_qug_max_depth")]
    pub max_depth: usize,
    /// 候选倍率：`candidate_k = min(max(top_k * multiplier, 50), 500)`，限制 1..=20。
    /// Candidate multiplier: `candidate_k = min(max(top_k * multiplier, 50), 500)`, bounded to 1..=20.
    #[serde(default = "default_qug_candidate_multiplier")]
    pub candidate_multiplier: usize,
    /// 意图模板文件路径（相对 domain.yaml 所在目录解析；None = 无意图模板）。
    /// Intent-template file path (resolved relative to the domain.yaml directory; None = no intent templates).
    #[serde(default)]
    pub intent_templates: Option<String>,
}

fn default_qug_enabled() -> bool {
    false
}

fn default_qug_max_depth() -> usize {
    2
}

fn default_qug_candidate_multiplier() -> usize {
    5
}

impl Default for QugConfig {
    /// Step 2 domain.yaml 无 `qug` 段时的兼容默认值。
    /// Step 2-compatible defaults used when a domain.yaml has no `qug` section.
    fn default() -> Self {
        Self {
            enabled: false,
            max_depth: 2,
            candidate_multiplier: 5,
            intent_templates: None,
        }
    }
}

/// 意图模板文件（`intents.yaml`）根结构。
/// Root structure of an intent-template file (`intents.yaml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentConfig {
    pub version: String,
    pub intents: Vec<IntentEntry>,
}

/// 单条意图模板：短语 + 恰好一个规则（expansion / attribute / negation）。
/// A single intent template: phrases + exactly one rule (expansion / attribute / negation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentEntry {
    pub id: String,
    pub phrases: Vec<String>,
    #[serde(default)]
    pub expansion: Option<TemplateExpansion>,
    #[serde(default)]
    pub attribute: Option<AttributeRule>,
    #[serde(default)]
    pub negation: Option<NegationRule>,
}

/// 意图展开：追加检索词与过滤条件。
/// Intent expansion: appends search terms and filter conditions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateExpansion {
    pub text: String,
    #[serde(default)]
    pub filters: Filters,
}

/// 属性传播：转为结构化数值/文本过滤，落事实平面。
/// Attribute propagation: becomes a structured numeric/text filter in the fact plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeRule {
    pub field: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub equals: Option<String>,
}

/// 否定规则：对 reflist 字段生成排除过滤。
/// Negation rule: produces an exclusion filter over a reflist field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegationRule {
    pub field: String,
    pub refs: Vec<String>,
}

impl IntentConfig {
    /// 校验意图配置（Step 3 §4）：每个 entry 恰好一个 rule；attribute 需
    /// min/max/equals 之一；negation 的 field 必须声明为 reflist；所有 field
    /// 必须在 `query.filters` 白名单内。非法配置返回 `Error::InvalidConfig`，
    /// 错误信息带文件、entry id 与字段。
    /// Validates the intent config (Step 3 §4): each entry has exactly one rule;
    /// attribute needs one of min/max/equals; a negation field must be declared as
    /// reflist; every field must be in the `query.filters` whitelist. Invalid configs
    /// return `Error::InvalidConfig` carrying the file, entry id and field name.
    pub fn validate(&self, source: &str, config: &DomainConfig) -> Result<()> {
        let allowed: HashSet<&str> = config.query_filters.iter().map(String::as_str).collect();
        for entry in &self.intents {
            let ctx = format!("{source} entry {}", entry.id);
            // 至少一个非空短语
            // At least one non-empty phrase
            if entry.phrases.iter().all(|p| p.trim().is_empty()) {
                return Err(Error::InvalidConfig(format!(
                    "{ctx}: phrases must be non-empty"
                )));
            }
            // 恰好一个 rule
            // Exactly one rule
            let rule_count = [
                entry.expansion.is_some(),
                entry.attribute.is_some(),
                entry.negation.is_some(),
            ]
            .into_iter()
            .filter(|b| *b)
            .count();
            if rule_count != 1 {
                return Err(Error::InvalidConfig(format!(
                    "{ctx}: exactly one of expansion/attribute/negation required, got {rule_count}"
                )));
            }

            // 所有引用到的 field 校验
            // Validate every referenced field
            if let Some(attr) = &entry.attribute {
                if attr.field.trim().is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: attribute field is empty"
                    )));
                }
                if attr.min.is_none() && attr.max.is_none() && attr.equals.is_none() {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: attribute {:?} needs at least one of min/max/equals",
                        attr.field
                    )));
                }
                if !allowed.contains(attr.field.as_str()) {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: attribute field {:?} not in query.filters whitelist",
                        attr.field
                    )));
                }
            }
            if let Some(neg) = &entry.negation {
                if neg.field.trim().is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: negation field is empty"
                    )));
                }
                if neg.refs.is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: negation {:?} refs must be non-empty",
                        neg.field
                    )));
                }
                // 否定仅允许 reflist 字段（排除语义需要 fact_refs 行）
                // Negation is only allowed on reflist fields (exclusion needs fact_refs rows)
                if config.field_type(&neg.field) != Some(FieldType::RefList) {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: negation field {:?} must be declared as reflist",
                        neg.field
                    )));
                }
                if !allowed.contains(neg.field.as_str()) {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: negation field {:?} not in query.filters whitelist",
                        neg.field
                    )));
                }
            }
            if let Some(exp) = &entry.expansion {
                if exp.text.trim().is_empty() {
                    return Err(Error::InvalidConfig(format!(
                        "{ctx}: expansion text is empty"
                    )));
                }
                for cond in &exp.filters.conditions {
                    let field = match cond {
                        crate::types::FilterCondition::NumericRange { field, .. }
                        | crate::types::FilterCondition::TextEquals { field, .. }
                        | crate::types::FilterCondition::RefContains { field, .. }
                        | crate::types::FilterCondition::RefExcludes { field, .. } => field,
                    };
                    if !allowed.contains(field.as_str()) {
                        return Err(Error::InvalidConfig(format!(
                            "{ctx}: expansion filter field {:?} not in query.filters whitelist",
                            field
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// 实体配置。
/// Entity configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EntityConfig {
    pub name: String,
    /// 数据源 URI（jsonl:// 或 postgres://）。
    /// Data source URI (jsonl:// or postgres://).
    pub source: String,
    pub id_field: String,
    pub type_field: String,
    pub fields: Vec<FieldDefinition>,
}
