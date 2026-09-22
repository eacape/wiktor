//! 编译管线配置、源投影与运行期 DTO（Step 4 spec §3 / §3.1）。
//! Compile-pipeline config, source projection and runtime DTOs (Step 4 spec §3 / §3.1).
//!
//! 契约要点：
//! - [`CompilePolicy`] 全字段 + 默认值对齐 §3.1 YAML；`validate()` 是发布前的硬校验。
//! - [`prepare_source`] 产出两平面投影：`full` 本地校验原件、`knowledge` 只含允许
//!   字段（进任务/Prompt）、`facts` 覆盖全部已声明字段（敏感字段允许本地存储）。
//! - revision 严格限制 `1..=i64::MAX`，禁止 `as i64` 溢出与静默回退 1。
//!
//! Contract highlights:
//! - [`CompilePolicy`] carries all fields with defaults aligned to the §3.1 YAML;
//!   `validate()` is the hard pre-publish check.
//! - [`prepare_source`] produces the two-plane projection: `full` is the locally
//!   validated original, `knowledge` keeps only allowed fields (goes into the
//!   task/prompt), `facts` covers every declared field (sensitive fields may be
//!   stored locally).
//! - Revisions are strictly `1..=i64::MAX`; no `as i64` overflow and no silent
//!   fallback to 1.

use crate::data::jsonl::raw_to_facts;
use crate::traits::EntitySchema;
use crate::types::error::{Error, Result};
use crate::types::{CompileContext, EntityId, Facts, RawEntity};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// 默认编译温度（OpenAI 适配器固定 temperature=0，进入 quality_policy 哈希域）。
/// Default compile temperature (OpenAI adapter pins temperature=0; goes into the
/// quality_policy hash domain).
pub const COMPILE_TEMPERATURE: f32 = 0.0;

/// 输出 envelope 的唯一支持版本（§5.1）。
/// The only supported envelope version (§5.1).
pub const ENVELOPE_SCHEMA_VERSION: &str = "source-ref-v1";

/// 编译策略：所有参与发布决策与哈希的配置快照（§3 契约字段全量）。
/// Compile policy: the full config snapshot participating in publish decisions
/// and hashing (all fields of the §3 contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompilePolicy {
    pub compiler_version: String,
    pub artifact_version: String,
    pub scorer_version: String,
    /// 允许进入知识平面的字段（缺省空 = 自动选 `text && !filterable` 且非敏感字段）。
    /// Fields allowed into the knowledge plane (empty default = auto-select
    /// `text && !filterable` non-sensitive fields).
    pub knowledge_fields: Vec<String>,
    /// 敏感字段：允许本地事实存储，但不得进入任务快照 / Prompt / 日志。
    /// Sensitive fields: allowed in local facts, never into task snapshots/prompts/logs.
    pub sensitive_fields: Vec<String>,
    /// 必需章节标题（领域展示字符串，非空且唯一）。
    /// Required section headings (domain display strings, non-empty and unique).
    pub required_headings: Vec<String>,
    pub min_coverage: f32,
    pub min_density: f32,
    pub max_recompiles: u32,
    pub max_retries: u32,
    pub task_token_budget: u64,
    pub batch_token_budget: u64,
    pub daily_token_budget: Option<u64>,
    pub max_output_tokens: u32,
    pub lease_seconds: u32,
    pub heartbeat_seconds: u32,
}

impl Default for CompilePolicy {
    /// 默认值按 spec §3.1 YAML；lease/心跳取 §8.3 的 300s/30s；
    /// compiler/artifact 版本为本步固定标识（参与 content_hash，改动即失效）。
    /// Defaults follow the §3.1 YAML; lease/heartbeat use §8.3's 300s/30s;
    /// compiler/artifact versions are fixed identifiers for this step (they join
    /// the content_hash, so changing them invalidates artifacts).
    fn default() -> Self {
        Self {
            compiler_version: "compile-v1".to_string(),
            artifact_version: "wiki-v1".to_string(),
            scorer_version: "rules-v1".to_string(),
            knowledge_fields: Vec::new(),
            sensitive_fields: Vec::new(),
            required_headings: vec!["概述".to_string()],
            min_coverage: 0.60,
            min_density: 0.40,
            max_recompiles: 2,
            max_retries: 3,
            task_token_budget: 65536,
            batch_token_budget: 262144,
            daily_token_budget: None,
            max_output_tokens: 2048,
            lease_seconds: 300,
            heartbeat_seconds: 30,
        }
    }
}

impl CompilePolicy {
    /// 硬校验（§3.1）：阈值有限且在 [0,1]、max_recompiles 0..=10、
    /// max_retries 1..=100、token 预算非零、max_output_tokens 1..=8192、
    /// required_headings 非空唯一、knowledge_fields 无重复非空。
    /// Hard validation (§3.1): thresholds finite within [0,1], max_recompiles
    /// 0..=10, max_retries 1..=100, non-zero token budgets, max_output_tokens
    /// 1..=8192, required_headings non-empty and unique, knowledge_fields
    /// duplicate-free and non-empty.
    pub fn validate(&self) -> Result<()> {
        for (name, v) in [
            ("min_coverage", self.min_coverage),
            ("min_density", self.min_density),
        ] {
            // NaN 与所有比较均为 false，因此取反的范围判断同时拒绝 NaN/Inf。
            // NaN fails every comparison, so the negated range test rejects NaN/Inf too.
            if !(0.0..=1.0).contains(&v) {
                return Err(Error::InvalidConfig(format!(
                    "compile policy {name} must be finite within [0,1], got {v}"
                )));
            }
        }
        if self.max_recompiles > 10 {
            return Err(Error::InvalidConfig(format!(
                "compile policy max_recompiles must be within 0..=10, got {}",
                self.max_recompiles
            )));
        }
        if self.max_retries < 1 || self.max_retries > 100 {
            return Err(Error::InvalidConfig(format!(
                "compile policy max_retries must be within 1..=100, got {}",
                self.max_retries
            )));
        }
        if self.task_token_budget == 0 {
            return Err(Error::InvalidConfig(
                "compile policy task_token_budget must be non-zero".into(),
            ));
        }
        if self.batch_token_budget == 0 {
            return Err(Error::InvalidConfig(
                "compile policy batch_token_budget must be non-zero".into(),
            ));
        }
        if self.daily_token_budget == Some(0) {
            return Err(Error::InvalidConfig(
                "compile policy daily_token_budget must be non-zero when enabled".into(),
            ));
        }
        if self.max_output_tokens < 1 || self.max_output_tokens > 8192 {
            return Err(Error::InvalidConfig(format!(
                "compile policy max_output_tokens must be within 1..=8192, got {}",
                self.max_output_tokens
            )));
        }
        if self.lease_seconds == 0 || self.heartbeat_seconds == 0 {
            return Err(Error::InvalidConfig(
                "compile policy lease_seconds/heartbeat_seconds must be non-zero".into(),
            ));
        }
        if self.heartbeat_seconds > self.lease_seconds {
            return Err(Error::InvalidConfig(
                "compile policy heartbeat_seconds must not exceed lease_seconds".into(),
            ));
        }
        if self.required_headings.is_empty() {
            return Err(Error::InvalidConfig(
                "compile policy required_headings must be non-empty".into(),
            ));
        }
        check_unique_non_empty(&self.required_headings, "required_headings")?;
        check_unique_non_empty(&self.knowledge_fields, "knowledge_fields")?;
        check_unique_non_empty(&self.sensitive_fields, "sensitive_fields")?;
        Ok(())
    }

    /// 解析知识字段允许列表（§3.1）：缺省（空）选 `text && !filterable` 且非敏感
    /// 字段；显式列表必须命中 schema、无重复，不得包含 filterable、numeric、
    /// boolean、timestamp 或 sensitive 字段（非 filterable text/reflist 允许）。
    /// Resolves the knowledge-field allowlist (§3.1): empty default selects
    /// `text && !filterable` non-sensitive fields; an explicit list must hit the
    /// schema without duplicates and must not include filterable, numeric,
    /// boolean, timestamp or sensitive fields (non-filterable text/reflist are OK).
    pub fn resolve_knowledge_fields(&self, schema: &EntitySchema) -> Result<Vec<String>> {
        if self.knowledge_fields.is_empty() {
            return Ok(schema
                .fields
                .iter()
                .filter(|f| {
                    f.field_type == crate::types::FieldType::Text
                        && !f.filterable
                        && !self.sensitive_fields.iter().any(|s| s == &f.name)
                })
                .map(|f| f.name.clone())
                .collect());
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for name in &self.knowledge_fields {
            if name.trim().is_empty() {
                return Err(Error::InvalidConfig(
                    "compile policy knowledge_fields entries must be non-empty".into(),
                ));
            }
            if !seen.insert(name.as_str()) {
                return Err(Error::InvalidConfig(format!(
                    "compile policy knowledge_fields has duplicate entry {name:?}"
                )));
            }
            let fd = schema
                .fields
                .iter()
                .find(|f| &f.name == name)
                .ok_or_else(|| {
                    Error::InvalidConfig(format!(
                        "knowledge field {name:?} not declared in source schema"
                    ))
                })?;
            if fd.filterable {
                return Err(Error::InvalidConfig(format!(
                    "knowledge field {name:?} must not be filterable"
                )));
            }
            let type_ok = matches!(
                fd.field_type,
                crate::types::FieldType::Text | crate::types::FieldType::RefList
            );
            if !type_ok {
                return Err(Error::InvalidConfig(format!(
                    "knowledge field {name:?} must be text or reflist, got {:?}",
                    fd.field_type
                )));
            }
            if self.sensitive_fields.iter().any(|s| s == name) {
                return Err(Error::InvalidConfig(format!(
                    "knowledge field {name:?} is declared sensitive"
                )));
            }
        }
        Ok(self.knowledge_fields.clone())
    }
}

fn check_unique_non_empty(values: &[String], field: &str) -> Result<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    for v in values {
        if v.trim().is_empty() {
            return Err(Error::InvalidConfig(format!(
                "compile policy {field} entries must be non-empty"
            )));
        }
        if !seen.insert(v.as_str()) {
            return Err(Error::InvalidConfig(format!(
                "compile policy {field} has duplicate entry {v:?}"
            )));
        }
    }
    Ok(())
}

/// 投影后的源（§3.1）：`full` 本地校验原件；`knowledge` 只含允许字段，是进
/// 任务/Prompt/验证器的唯一知识快照；`facts` 覆盖全部已声明字段；`snapshot_hash`
/// 为完整原件的 BLAKE3，用于同 revision 内容冲突检测。
/// Projected source (§3.1): `full` is the locally validated original; `knowledge`
/// keeps only allowed fields and is the sole knowledge snapshot entering the
/// task/prompt/validator; `facts` covers every declared field; `snapshot_hash` is
/// the BLAKE3 over the full original, detecting same-revision content conflicts.
#[derive(Debug, Clone)]
pub struct PreparedSource {
    pub full: RawEntity,
    pub knowledge: RawEntity,
    pub facts: Facts,
    pub snapshot_hash: String,
}

/// 源投影入口：策略校验 → revision 严格校验 → 知识字段解析 → 事实转换 → 快照哈希。
/// Source-projection entry: policy validation → strict revision check → knowledge
/// field resolution → fact conversion → snapshot hash.
pub fn prepare_source(
    raw: &RawEntity,
    schema: &EntitySchema,
    policy: &CompilePolicy,
) -> Result<PreparedSource> {
    policy.validate()?;
    // revision 必须落在 1..=i64::MAX；禁止 `as i64` 溢出（u64 高位截断会变负数）。
    // Revision must lie in 1..=i64::MAX; `as i64` overflow (u64 truncation turning
    // negative) is forbidden.
    if raw.source_revision == 0 || raw.source_revision > i64::MAX as u64 {
        return Err(Error::Validation(format!(
            "entity {} source_revision {} outside 1..=i64::MAX",
            raw.id.to_key(),
            raw.source_revision
        )));
    }
    let allowed = policy.resolve_knowledge_fields(schema)?;
    // 事实平面覆盖全部 schema 声明字段（含敏感字段，允许本地存储，不进任务快照）。
    // The fact plane covers every schema-declared field (including sensitive ones,
    // allowed locally, never entering the task snapshot).
    let facts = raw_to_facts(raw, schema)?;
    let mut knowledge_fields: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for name in &allowed {
        if let Some(value) = raw.fields.get(name) {
            knowledge_fields.insert(name.clone(), value.clone());
        }
    }
    let knowledge = RawEntity {
        id: raw.id.clone(),
        fields: knowledge_fields,
        source_revision: raw.source_revision,
    };
    let snapshot_hash = crate::compile::hash::snapshot_hash(raw)?;
    Ok(PreparedSource {
        full: raw.clone(),
        knowledge,
        facts,
        snapshot_hash,
    })
}

/// 一次 run 的执行选项（§3 / §9：limit 默认 1000 上限 10000，batch_size 1..=128）。
/// Run execution options (§3 / §9: limit defaults to 1000 with cap 10000,
/// batch_size 1..=128).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOptions {
    pub limit: usize,
    pub batch_size: usize,
    pub force: bool,
    pub dry_run: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            limit: 1000,
            batch_size: 32,
            force: false,
            dry_run: false,
        }
    }
}

impl RunOptions {
    /// 范围校验：limit 1..=10000、batch_size 1..=128（§3.1）。
    /// Range validation: limit 1..=10000, batch_size 1..=128 (§3.1).
    pub fn validate(&self) -> Result<()> {
        if self.limit < 1 || self.limit > 10000 {
            return Err(Error::InvalidConfig(format!(
                "run options limit must be within 1..=10000, got {}",
                self.limit
            )));
        }
        if self.batch_size < 1 || self.batch_size > 128 {
            return Err(Error::InvalidConfig(format!(
                "run options batch_size must be within 1..=128, got {}",
                self.batch_size
            )));
        }
        Ok(())
    }
}

/// 一次 CLI run 的最终统计（§3；`--json` 输出单对象）。
/// Final statistics of one CLI run (§3; the single `--json` object).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompileStats {
    pub run_id: String,
    pub scanned: u64,
    pub accepted: u64,
    pub quarantined: u64,
    pub failed: u64,
    pub skipped: u64,
    pub deferred: u64,
    pub would_compile: u64,
    pub attempts: u64,
    pub reserved_tokens: u64,
    pub reported_tokens: u64,
    pub circuit_open: bool,
    pub dry_run: bool,
}

/// admission 事务结果（§3）。
/// Outcome of the admission transaction (§3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Queued(i64),
    Skipped,
    Deferred,
    Rejected(String),
}

/// 发布事务结果（§3）：接受并返回新 generation，或租约/前置校验过期。
/// Publish-transaction outcome (§3): accepted with a new generation, or stale
/// lease/precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Accepted { generation: i64 },
    Stale,
}

/// 失败处置（§3）：退避重试、隔离或终态失败。
/// Failure disposition (§3): retry with backoff, quarantine, or terminal failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    RetryAt(i64),
    Quarantined,
    Failed,
}

/// 已领取任务的租约（§3 契约）；所有后续更新必须绑定 `lease_token`。
/// Lease of a claimed task (§3 contract); every later update must bind `lease_token`.
#[derive(Debug, Clone)]
pub struct TaskLease {
    pub task_id: i64,
    pub epoch: i64,
    pub lease_token: String,
    pub desired_hash: String,
    pub attempt_no: u32,
    pub source: RawEntity,
    pub context: CompileContext,
}

/// 时钟抽象（验收用注入 Clock；§3）。
/// Clock abstraction (injected Clock for acceptance tests; §3).
pub trait Clock: Send + Sync {
    fn unix_seconds(&self) -> i64;
}

/// 真实系统时钟。
/// Real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> i64 {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(_) => 0,
        }
    }
}

/// 构造编译上下文（执行器把领域配置 + 策略映射为参与哈希的 `CompileContext`）。
/// Builds the compile context (the executor maps domain config + policy into the
/// hash-participating `CompileContext`).
pub fn build_context(
    domain_pack_version: &str,
    prompt_template: &str,
    model_version: &str,
    embedding_model: &str,
    quality_threshold: f32,
    require_source_refs: bool,
) -> CompileContext {
    CompileContext {
        domain_pack_version: domain_pack_version.to_string(),
        prompt_template: prompt_template.to_string(),
        model_version: model_version.to_string(),
        embedding_model: embedding_model.to_string(),
        quality_threshold,
        require_source_refs,
    }
}

/// 把实体引用规约为稳定键（测试与诊断辅助）。
/// Reduces an entity reference to its stable key (test/diagnostic helper).
pub fn entity_key(id: &EntityId) -> String {
    id.to_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FieldDefinition, FieldType};

    fn schema() -> EntitySchema {
        EntitySchema {
            entity_type: "drink".to_string(),
            fields: vec![
                FieldDefinition {
                    name: "name".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "description".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "category".to_string(),
                    field_type: FieldType::Text,
                    filterable: true,
                },
                FieldDefinition {
                    name: "price".to_string(),
                    field_type: FieldType::Numeric,
                    filterable: true,
                },
                FieldDefinition {
                    name: "on_sale".to_string(),
                    field_type: FieldType::Boolean,
                    filterable: false,
                },
                FieldDefinition {
                    name: "ingredients".to_string(),
                    field_type: FieldType::RefList,
                    filterable: false,
                },
            ],
        }
    }

    fn raw(revision: u64) -> RawEntity {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        fields.insert(
            "category".to_string(),
            serde_json::json!("milk-tea:drink:boba"),
        );
        fields.insert("price".to_string(), serde_json::json!(19.0));
        fields.insert("on_sale".to_string(), serde_json::json!(false));
        fields.insert(
            "ingredients".to_string(),
            serde_json::json!(["milk-tea:ingredient:pearl"]),
        );
        RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: revision,
        }
    }

    // A23（部分）：revision 边界 1..=i64::MAX，非法值拒绝。
    // A23 (partial): revision bounds 1..=i64::MAX, invalid values rejected.
    #[test]
    fn revision_bounds_are_strict() {
        let policy = CompilePolicy::default();
        assert!(prepare_source(&raw(1), &schema(), &policy).is_ok());
        assert!(prepare_source(&raw(i64::MAX as u64), &schema(), &policy).is_ok());
        let err = prepare_source(&raw(0), &schema(), &policy).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        let mut overflow = raw(1);
        overflow.source_revision = i64::MAX as u64 + 1;
        let err = prepare_source(&overflow, &schema(), &policy).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    // 投影：knowledge 只含允许字段；facts 覆盖全部声明字段；snapshot_hash 稳定。
    // Projection: knowledge keeps only allowed fields; facts cover all declared
    // fields; snapshot_hash is stable.
    #[test]
    fn projection_splits_planes() {
        let policy = CompilePolicy::default();
        let prepared = prepare_source(&raw(3), &schema(), &policy).unwrap();
        let names: Vec<&str> = prepared
            .knowledge
            .fields
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, vec!["description", "name"]);
        assert_eq!(prepared.facts.fields.len(), 6);
        assert_eq!(prepared.facts.source_revision, 3);
        assert_eq!(prepared.knowledge.source_revision, 3);
        assert_eq!(prepared.snapshot_hash.len(), 64);
        // 同 revision 同内容 → snapshot_hash 相同。
        // Same revision and content → identical snapshot_hash.
        let again = prepare_source(&raw(3), &schema(), &policy).unwrap();
        assert_eq!(prepared.snapshot_hash, again.snapshot_hash);
    }

    // 显式知识字段列表：命中 schema、拒绝 filterable/numeric/sensitive。
    // Explicit knowledge list: must hit the schema; rejects filterable/numeric/sensitive.
    #[test]
    fn explicit_knowledge_fields_are_validated() {
        let schema = schema();
        let ok = CompilePolicy {
            knowledge_fields: vec!["name".into(), "ingredients".into()],
            ..CompilePolicy::default()
        };
        assert_eq!(
            ok.resolve_knowledge_fields(&schema).unwrap(),
            vec!["name".to_string(), "ingredients".to_string()]
        );
        let filterable = CompilePolicy {
            knowledge_fields: vec!["category".into()],
            ..CompilePolicy::default()
        };
        assert!(filterable.resolve_knowledge_fields(&schema).is_err());
        let numeric = CompilePolicy {
            knowledge_fields: vec!["price".into()],
            ..CompilePolicy::default()
        };
        assert!(numeric.resolve_knowledge_fields(&schema).is_err());
        let unknown = CompilePolicy {
            knowledge_fields: vec!["nope".into()],
            ..CompilePolicy::default()
        };
        assert!(unknown.resolve_knowledge_fields(&schema).is_err());
        let dup = CompilePolicy {
            knowledge_fields: vec!["name".into(), "name".into()],
            ..CompilePolicy::default()
        };
        assert!(dup.validate().is_err());
        let sensitive = CompilePolicy {
            knowledge_fields: vec!["name".into()],
            sensitive_fields: vec!["name".into()],
            ..CompilePolicy::default()
        };
        assert!(sensitive.resolve_knowledge_fields(&schema).is_err());
        // 敏感字段不进入缺省知识选择。
        // Sensitive fields are excluded from the default knowledge selection.
        let hidden = CompilePolicy {
            sensitive_fields: vec!["description".into()],
            ..CompilePolicy::default()
        };
        let default_sel = hidden.resolve_knowledge_fields(&schema).unwrap();
        assert!(!default_sel.iter().any(|s| s == "description"));
    }

    // 策略校验反例（A9/A23 依赖的策略面）。
    // Policy-validation negatives (the policy face A9/A23 rely on).
    #[test]
    fn policy_validate_rejects_bad_values() {
        let base = CompilePolicy::default();
        assert!(base.validate().is_ok());
        let cases = [
            CompilePolicy {
                min_coverage: 1.5,
                ..base.clone()
            },
            CompilePolicy {
                min_coverage: f32::NAN,
                ..base.clone()
            },
            CompilePolicy {
                min_density: -0.1,
                ..base.clone()
            },
            CompilePolicy {
                max_recompiles: 11,
                ..base.clone()
            },
            CompilePolicy {
                max_retries: 0,
                ..base.clone()
            },
            CompilePolicy {
                max_retries: 101,
                ..base.clone()
            },
            CompilePolicy {
                task_token_budget: 0,
                ..base.clone()
            },
            CompilePolicy {
                batch_token_budget: 0,
                ..base.clone()
            },
            CompilePolicy {
                daily_token_budget: Some(0),
                ..base.clone()
            },
            CompilePolicy {
                max_output_tokens: 0,
                ..base.clone()
            },
            CompilePolicy {
                max_output_tokens: 8193,
                ..base.clone()
            },
            CompilePolicy {
                required_headings: Vec::new(),
                ..base.clone()
            },
            CompilePolicy {
                required_headings: vec!["概述".into(), "概述".into()],
                ..base.clone()
            },
        ];
        for policy in cases {
            assert!(policy.validate().is_err(), "expected rejection: {policy:?}");
        }
    }

    // RunOptions 范围。
    // RunOptions ranges.
    #[test]
    fn run_options_ranges() {
        assert!(RunOptions::default().validate().is_ok());
        assert!(RunOptions {
            limit: 0,
            ..RunOptions::default()
        }
        .validate()
        .is_err());
        assert!(RunOptions {
            limit: 10001,
            ..RunOptions::default()
        }
        .validate()
        .is_err());
        assert!(RunOptions {
            batch_size: 0,
            ..RunOptions::default()
        }
        .validate()
        .is_err());
        assert!(RunOptions {
            batch_size: 129,
            ..RunOptions::default()
        }
        .validate()
        .is_err());
    }
}
