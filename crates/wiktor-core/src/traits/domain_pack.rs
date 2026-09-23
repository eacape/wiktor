use crate::compile::config::{CompatibilitySpec, CompilePolicy, ConsistencyPolicy, DomainIdentity};
use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::{Error, Result};
use crate::types::{FieldDefinition, FieldType, Filters};
use semver::Version;
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
///   knowledge_fields: [name, description]
///   required_headings: [概述]
/// query:
///   filters: [price, sugar_level, size, ingredient_ids]
/// qug:
///   enabled: true
///   max_depth: 2
///   candidate_multiplier: 5
///   intent_templates: intents.yaml
/// ```
/// 自定义 `Deserialize` 把嵌套的 `compile` / `query` / `qug` 段拍平到本结构，
/// 保持既有字段（quality_threshold / max_recompiles）不变，新增 query_filters、
/// qug 与 compile_policy；缺少 `qug` 段时使用 Step 2 兼容默认值（enabled=false）。
/// 缺少整个 `compile` 段时必须实际产生阈值 0.75 / max_recompiles=2（修正历史
/// derive(Default) 零值 bug），`compile` 段内部严格拒绝未知字段。
/// Step 8 起解析 `compile.consistency` / `compile.artifact_version` /
/// `compatibility` 段（§5.1），且 `version` / `schema_version` /
/// `prompt_version` 必须是 strict semver（解析期 fail-closed，A13 纯解析部分；
/// 缺失的 schema/prompt 记 None = legacy 只读容忍，真实 compile 的强制在 B5）。
/// A custom `Deserialize` flattens the nested `compile` / `query` / `qug` sections
/// into this struct, keeping existing fields (quality_threshold / max_recompiles)
/// unchanged and adding query_filters, qug and compile_policy; a missing `qug`
/// section falls back to Step 2-compatible defaults (enabled=false). A missing
/// `compile` section must actually yield threshold 0.75 / max_recompiles=2
/// (fixing the historical derive(Default) zero-value bug); the `compile` section
/// strictly rejects unknown fields. Since Step 8 the `compile.consistency` /
/// `compile.artifact_version` / `compatibility` sections are parsed (§5.1) and
/// `version` / `schema_version` / `prompt_version` must be strict semver
/// (fail-closed at parse time, the A13 pure-parsing part; missing schema/prompt
/// are recorded as None = legacy read-only tolerance, enforcement for real
/// compiles lands in B5).
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: String,
    pub version: String,
    pub entities: Vec<EntityConfig>,
    pub quality_threshold: f32,
    pub max_recompiles: usize,
    /// Step 4 编译策略快照（§3 契约；run 启动时冻结并参与哈希）。
    /// The Step 4 compile-policy snapshot (§3 contract; frozen at run start and
    /// hashed).
    pub compile_policy: CompilePolicy,
    /// `compile.prompt`（可省略，使用内置 source-ref-v1 模板）。
    /// `compile.prompt` (optional; falls back to the built-in source-ref-v1 template).
    pub compile_prompt: Option<String>,
    /// `compile.output_contract`（默认 require_source_refs）。
    /// `compile.output_contract` (defaults to require_source_refs).
    pub compile_output_contract: String,
    /// `query.filters` 过滤白名单（Step 2 仅解析提示，不强制消费）。
    /// Whitelist of `query.filters` (Step 2 only parses this as a hint, does not enforce it).
    pub query_filters: Vec<String>,
    /// `qug` 段（Step 3：QUG 开关/深度/候选倍率/意图模板文件）。
    /// The `qug` section (Step 3: QUG toggle/depth/candidate multiplier/intent template file).
    pub qug: QugConfig,
    /// Step 8 §5.1：strict semver 的 schema 版本（缺省 None = legacy 容忍）。
    /// Step 8 §5.1: the strict-semver schema version (None = legacy tolerance).
    pub schema_version: Option<Version>,
    /// Step 8 §5.1：strict semver 的 prompt 版本（缺省 None = legacy 容忍）。
    /// Step 8 §5.1: the strict-semver prompt version (None = legacy tolerance).
    pub prompt_version: Option<Version>,
}

impl<'de> Deserialize<'de> for DomainConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // compile 段：deny_unknown_fields 拒绝拼写错误；各字段默认值以
        // CompilePolicy::default() 为唯一事实来源（§3.1 YAML）。
        // The compile section: deny_unknown_fields rejects typos; per-field
        // defaults take CompilePolicy::default() as the single source of truth
        // (§3.1 YAML).
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        struct CompileSection {
            #[serde(default = "default_threshold")]
            quality_threshold: f32,
            #[serde(default = "default_max_recompiles")]
            max_recompiles: usize,
            #[serde(default)]
            prompt: Option<String>,
            #[serde(default = "default_output_contract")]
            output_contract: String,
            #[serde(default)]
            knowledge_fields: Vec<String>,
            #[serde(default)]
            sensitive_fields: Vec<String>,
            #[serde(default = "default_required_headings")]
            required_headings: Vec<String>,
            #[serde(default = "default_scorer_version")]
            scorer_version: String,
            #[serde(default = "default_min_coverage")]
            min_coverage: f32,
            #[serde(default = "default_min_density")]
            min_density: f32,
            #[serde(default = "default_max_retries")]
            max_retries: u32,
            #[serde(default = "default_task_token_budget")]
            task_token_budget: u64,
            #[serde(default = "default_batch_token_budget")]
            batch_token_budget: u64,
            #[serde(default)]
            daily_token_budget: Option<u64>,
            #[serde(default = "default_max_output_tokens")]
            max_output_tokens: u32,
            // Step 8 §5.1 新增：一致性策略 / 产物版本 / 回收间隔 / preflight 开关。
            // Added in Step 8 §5.1: consistency policy / artifact version /
            // reaper interval / preflight toggle.
            #[serde(default)]
            artifact_version: Option<String>,
            #[serde(default)]
            consistency: ConsistencyPolicy,
            #[serde(default = "default_lease_reaper_interval")]
            lease_reaper_interval_seconds: u32,
            #[serde(default = "default_compatibility_preflight")]
            compatibility_preflight: bool,
        }
        impl Default for CompileSection {
            /// 缺省 compile 段（历史 bug：derive(Default) 产生零值；§3.1 修正为
            /// 0.75 / 2 及各新字段默认）。
            /// Defaults for a missing compile section (historical bug:
            /// derive(Default) produced zeros; §3.1 fixes them to 0.75 / 2 plus
            /// the new-field defaults).
            fn default() -> Self {
                Self {
                    quality_threshold: default_threshold(),
                    max_recompiles: default_max_recompiles(),
                    prompt: None,
                    output_contract: default_output_contract(),
                    knowledge_fields: Vec::new(),
                    sensitive_fields: Vec::new(),
                    required_headings: default_required_headings(),
                    scorer_version: default_scorer_version(),
                    min_coverage: default_min_coverage(),
                    min_density: default_min_density(),
                    max_retries: default_max_retries(),
                    task_token_budget: default_task_token_budget(),
                    batch_token_budget: default_batch_token_budget(),
                    daily_token_budget: None,
                    max_output_tokens: default_max_output_tokens(),
                    artifact_version: None,
                    consistency: ConsistencyPolicy::default(),
                    lease_reaper_interval_seconds: default_lease_reaper_interval(),
                    compatibility_preflight: default_compatibility_preflight(),
                }
            }
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
            // Step 8 §5.1：strict semver 身份版本与兼容矩阵段（均可缺省）。
            // Step 8 §5.1: strict-semver identity versions and the
            // compatibility section (all optional).
            #[serde(default)]
            schema_version: Option<String>,
            #[serde(default)]
            prompt_version: Option<String>,
            #[serde(default)]
            compatibility: Option<CompatibilitySpec>,
        }
        fn default_threshold() -> f32 {
            0.75
        }
        fn default_max_recompiles() -> usize {
            2
        }
        fn default_output_contract() -> String {
            "require_source_refs".to_string()
        }
        fn default_required_headings() -> Vec<String> {
            CompilePolicy::default().required_headings
        }
        fn default_scorer_version() -> String {
            CompilePolicy::default().scorer_version
        }
        fn default_min_coverage() -> f32 {
            CompilePolicy::default().min_coverage
        }
        fn default_min_density() -> f32 {
            CompilePolicy::default().min_density
        }
        fn default_max_retries() -> u32 {
            CompilePolicy::default().max_retries
        }
        fn default_task_token_budget() -> u64 {
            CompilePolicy::default().task_token_budget
        }
        fn default_batch_token_budget() -> u64 {
            CompilePolicy::default().batch_token_budget
        }
        fn default_max_output_tokens() -> u32 {
            CompilePolicy::default().max_output_tokens
        }
        fn default_lease_reaper_interval() -> u32 {
            crate::compile::config::DEFAULT_LEASE_REAPER_INTERVAL_SECONDS
        }
        fn default_compatibility_preflight() -> bool {
            true
        }

        let r = Repr::deserialize(deserializer)?;
        // max_recompiles 保持公开字段为 usize（Step 2 兼容），策略内为 u32；
        // 越界值留给 CompilePolicy::validate() 报错。
        // max_recompiles stays usize in the public field (Step 2 compatibility)
        // and u32 inside the policy; out-of-range values are left to
        // CompilePolicy::validate().
        let compile_policy = CompilePolicy {
            scorer_version: r.compile.scorer_version,
            knowledge_fields: r.compile.knowledge_fields,
            sensitive_fields: r.compile.sensitive_fields,
            required_headings: r.compile.required_headings,
            min_coverage: r.compile.min_coverage,
            min_density: r.compile.min_density,
            max_recompiles: u32::try_from(r.compile.max_recompiles).unwrap_or(u32::MAX),
            max_retries: r.compile.max_retries,
            task_token_budget: r.compile.task_token_budget,
            batch_token_budget: r.compile.batch_token_budget,
            daily_token_budget: r.compile.daily_token_budget,
            max_output_tokens: r.compile.max_output_tokens,
            // Step 8 §5.1：artifact_version 缺省沿用策略默认 wiki-v1；一致性、
            // 回收间隔、preflight 开关与兼容矩阵快照照抄 YAML。
            // Step 8 §5.1: artifact_version falls back to the policy default
            // wiki-v1; consistency, reaper interval, preflight toggle and the
            // compatibility snapshot are copied verbatim from the YAML.
            artifact_version: r
                .compile
                .artifact_version
                .unwrap_or_else(|| CompilePolicy::default().artifact_version),
            consistency: r.compile.consistency,
            lease_reaper_interval_seconds: r.compile.lease_reaper_interval_seconds,
            compatibility_preflight: r.compile.compatibility_preflight,
            compatibility: r.compatibility,
            ..CompilePolicy::default()
        };
        // Step 8 §5.1：version/schema_version/prompt_version 必须 strict semver
        // （解析期 fail-closed，A13）；artifact_version 用策略解析后的值，保证
        // 身份五元组与策略一致。缺失 schema/prompt 记 None（legacy 只读容忍）。
        // Step 8 §5.1: version/schema_version/prompt_version must be strict
        // semver (fail-closed at parse time, A13); artifact_version uses the
        // resolved policy value so the identity five-tuple and the policy agree.
        // Missing schema/prompt are recorded as None (legacy read-only
        // tolerance).
        let identity = DomainIdentity::parse(
            &r.name,
            &r.version,
            r.schema_version.as_deref(),
            r.prompt_version.as_deref(),
            &compile_policy.artifact_version,
        )
        .map_err(serde::de::Error::custom)?;
        Ok(DomainConfig {
            name: r.name,
            version: r.version,
            entities: r.entities,
            quality_threshold: r.compile.quality_threshold,
            max_recompiles: r.compile.max_recompiles,
            compile_policy,
            compile_prompt: r.compile.prompt,
            compile_output_contract: r.compile.output_contract,
            query_filters: r.query.filters,
            qug: r.qug,
            schema_version: identity.schema_version,
            prompt_version: identity.prompt_version,
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

    /// 构造领域身份五元组（Step8 §6.4；B5 兼容 preflight 与 CLI 消费）。
    /// YAML 路径的 version/schema/prompt 已在解析期验证 strict semver；非 YAML
    /// 直接构造的配置（测试夹具）若 version 非法在此报 `InvalidConfig`。
    /// artifact_version 与 compile 策略同源，保证身份与哈希输入一致。
    /// Builds the domain-identity five-tuple (Step8 §6.4; consumed by B5's
    /// compatibility preflight and the CLI). YAML-parsed configs already
    /// validated version/schema/prompt as strict semver at parse time; configs
    /// built directly (test fixtures) fail with `InvalidConfig` here when
    /// version is invalid. artifact_version shares its source with the compile
    /// policy, keeping the identity and the hash inputs consistent.
    pub fn identity(&self) -> Result<DomainIdentity> {
        let schema = self.schema_version.as_ref().map(ToString::to_string);
        let prompt = self.prompt_version.as_ref().map(ToString::to_string);
        DomainIdentity::parse(
            &self.name,
            &self.version,
            schema.as_deref(),
            prompt.as_deref(),
            &self.compile_policy.artifact_version,
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    // 缺省 compile 段必须实际产生 0.75/2（历史零值 bug 的回归测试）。
    // A missing compile section must actually yield 0.75/2 (regression test for
    // the historical zero-value bug).
    #[test]
    fn missing_compile_section_defaults_to_spec() {
        let config: DomainConfig =
            serde_yaml_ng::from_str("name: d\nversion: \"0.1.0\"\n").unwrap();
        assert_eq!(config.quality_threshold, 0.75);
        assert_eq!(config.max_recompiles, 2);
        let policy = &config.compile_policy;
        assert_eq!(policy.min_coverage, 0.60);
        assert_eq!(policy.min_density, 0.40);
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.task_token_budget, 65536);
        assert_eq!(policy.batch_token_budget, 262144);
        assert_eq!(policy.max_output_tokens, 2048);
        assert_eq!(policy.required_headings, vec!["概述".to_string()]);
        assert_eq!(config.compile_output_contract, "require_source_refs");
        assert!(config.compile_prompt.is_none());
        assert!(policy.validate().is_ok());
    }

    // compile 段部分提供时其余字段仍取默认。
    // A partially provided compile section keeps the remaining fields at defaults.
    #[test]
    fn partial_compile_section_keeps_defaults() {
        let yaml = "name: d\nversion: \"0.1.0\"\ncompile:\n  max_recompiles: 1\n";
        let config: DomainConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.quality_threshold, 0.75);
        assert_eq!(config.max_recompiles, 1);
    }

    // compile 段未知字段严格拒绝；query/qug 段维持兼容（不启用严格模式）。
    // Unknown compile fields are strictly rejected; query/qug sections stay
    // compatible (no strict mode there).
    #[test]
    fn compile_section_rejects_unknown_fields() {
        let yaml = "name: d\nversion: \"0.1.0\"\ncompile:\n  quality_threshlod: 0.9\n";
        assert!(serde_yaml_ng::from_str::<DomainConfig>(yaml).is_err());
        // 既有段不拒绝未知字段（Step 2/3 兼容性）。
        // Existing sections do not reject unknown fields (Step 2/3 compatibility).
        let yaml = "name: d\nversion: \"0.1.0\"\nquery:\n  legacy_key: 1\nqug:\n  legacy_key: 1\n";
        assert!(serde_yaml_ng::from_str::<DomainConfig>(yaml).is_ok());
    }

    // Step8 §5.1（A13 纯解析）：consistency/artifact_version 段解析进策略；
    // 缺省时一致性关闭、artifact_version 取策略默认。
    // Step8 §5.1 (A13 pure parsing): the consistency/artifact_version sections
    // feed the policy; defaults keep consistency disabled and artifact_version
    // at the policy default.
    #[test]
    fn compile_section_step8_fields_parse_into_policy() {
        let yaml = "name: d\nversion: \"0.1.0\"\n";
        let config: DomainConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!config.compile_policy.consistency.enabled);
        assert_eq!(config.compile_policy.artifact_version, "wiki-v1");
        assert_eq!(config.compile_policy.lease_reaper_interval_seconds, 30);
        assert!(config.compile_policy.compatibility_preflight);
        assert!(config.compile_policy.compatibility.is_none());
        assert!(config.schema_version.is_none());
        assert!(config.prompt_version.is_none());

        let yaml = concat!(
            "name: d\n",
            "version: \"1.2.0\"\n",
            "schema_version: \"2.1.0\"\n",
            "prompt_version: \"3.0.0\"\n",
            "compile:\n",
            "  artifact_version: wiki-v2\n",
            "  lease_reaper_interval_seconds: 15\n",
            "  compatibility_preflight: false\n",
            "  consistency:\n",
            "    enabled: true\n",
            "    top_k: 16\n",
            "    min_consistency: 0.9\n",
            "    compare_pointers: [\"/fields/name\", \"/fields/description\"]\n",
            "compatibility:\n",
            "  domain_pack: \">=1.0.0,<2.0.0\"\n",
            "  schema: \">=2.0.0,<3.0.0\"\n",
            "  prompt: \">=3.0.0,<4.0.0\"\n",
            "  artifact: [\"wiki-v1\", \"wiki-v2\"]\n",
        );
        let config: DomainConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let policy = &config.compile_policy;
        assert_eq!(policy.artifact_version, "wiki-v2");
        assert!(policy.consistency.enabled);
        assert_eq!(policy.consistency.top_k, 16);
        assert!((policy.consistency.min_consistency - 0.9).abs() < 1e-6);
        assert_eq!(
            policy.consistency.compare_pointers,
            vec![
                "/fields/name".to_string(),
                "/fields/description".to_string()
            ]
        );
        assert_eq!(policy.lease_reaper_interval_seconds, 15);
        assert!(!policy.compatibility_preflight);
        let spec = policy.compatibility.as_ref().unwrap();
        assert_eq!(spec.artifact.len(), 2);
        assert!(policy.validate().is_ok());
        // 身份五元组：strict semver + 与策略同源的 artifact_version。
        // Identity five-tuple: strict semver with policy-sourced
        // artifact_version.
        let identity = config.identity().unwrap();
        assert_eq!(identity.domain, "d");
        assert_eq!(identity.version.to_string(), "1.2.0");
        assert_eq!(
            identity.schema_version.as_ref().unwrap().to_string(),
            "2.1.0"
        );
        assert_eq!(
            identity.prompt_version.as_ref().unwrap().to_string(),
            "3.0.0"
        );
        assert_eq!(identity.artifact_version, "wiki-v2");
    }

    // Step8 §5.1（A13）：version/schema_version/prompt_version 非 strict semver
    // 在解析期拒绝；compile 段未知字段（含 consistency 内部）仍严格拒绝。
    // Step8 §5.1 (A13): non-strict-semver version/schema_version/
    // prompt_version is rejected at parse time; unknown fields (including
    // inside consistency) remain strictly rejected.
    #[test]
    fn step8_identity_versions_are_strict_semver() {
        assert!(
            serde_yaml_ng::from_str::<DomainConfig>("name: d\nversion: \"test-v1\"\n").is_err()
        );
        assert!(serde_yaml_ng::from_str::<DomainConfig>(
            "name: d\nversion: \"0.1.0\"\nschema_version: \"2.0\"\n"
        )
        .is_err());
        assert!(serde_yaml_ng::from_str::<DomainConfig>(
            "name: d\nversion: \"0.1.0\"\nprompt_version: \"\"\n"
        )
        .is_err());
        // consistency 段未知字段拒绝。
        // Unknown consistency fields are rejected.
        assert!(serde_yaml_ng::from_str::<DomainConfig>(
            "name: d\nversion: \"0.1.0\"\ncompile:\n  consistency:\n    topk: 8\n"
        )
        .is_err());
        // compatibility 段未知字段拒绝。
        // Unknown compatibility fields are rejected.
        assert!(serde_yaml_ng::from_str::<DomainConfig>(
            "name: d\nversion: \"0.1.0\"\ncompatibility:\n  domain: \">=1.0.0\"\n"
        )
        .is_err());
    }
}
