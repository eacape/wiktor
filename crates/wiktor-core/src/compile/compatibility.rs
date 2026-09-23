//! 兼容 preflight（Step 8 spec §2/§5.1/§6.4/§7/§8，决策 D10/D11）。
//! Compatibility preflight (Step 8 spec §2/§5.1/§6.4/§7/§8, decisions D10/D11).
//!
//! 契约要点：
//! - [`CompatibilityChecker`]（§6.4 签名）+ 默认
//!   [`StandardCompatibilityChecker`]：确定性、只读、无网络——扫描全部 accepted
//!   页的 `domain_pack_version/artifact_version/frontmatter quality_policy` 与
//!   全部 pending/running/dead 任务的 `dependencies_json` 快照；已 superseded 的
//!   历史 attempt 不算当前产物，损坏 JSON 仍计为 violation（A14）。
//! - 版本缺失、非法 semver、artifact 不在显式允许列表均为不兼容；semver 范围用
//!   `VersionReq::matches` 正式比较，绝不按字符串排序（A13/D10）。
//! - [`check_domain_compatibility`] 是 domain check（B6 CLI 接线）与 executor
//!   首次 admission 前 preflight（D11）共用的同一入口：`spec=None` 时按 §5.1
//!   legacy 只读容忍返回空兼容报告（偏差 STEP8-028：executor 不对「缺失矩阵 +
//!   已有 Step4 数据」强制配置错误，保持 338 基线与 Step4/6 行为，留上层拍板）。
//! - 诊断只存版本串/稳定 code/主体标识，绝不落源明文；`compatible=false` 时
//!   compile 拒绝 admission、不改 facts、不建任务，并在真实 run 中幂等插入
//!   `compatibility_conflict` 审核（A16）。
//!
//! Contract highlights:
//! - [`CompatibilityChecker`] (the §6.4 signature) plus the default
//!   [`StandardCompatibilityChecker`]: deterministic, read-only and offline — it
//!   scans every accepted page's `domain_pack_version/artifact_version/
//!   frontmatter quality_policy` and every pending/running/dead task's
//!   `dependencies_json` snapshot; explicitly superseded historical attempts do
//!   not count as current artifacts while corrupt JSON still counts as a
//!   violation (A14).
//! - Missing versions, invalid semver and artifacts outside the explicit
//!   allowlist are all incompatible; semver ranges compare via
//!   `VersionReq::matches` — never string-sorted (A13/D10).
//! - [`check_domain_compatibility`] is the single entry shared by `domain check`
//!   (CLI wiring lands in B6) and the executor's pre-first-admission preflight
//!   (D11): with `spec=None` it returns an empty compatible report per the §5.1
//!   legacy read-only tolerance (deviation STEP8-028: the executor does not
//!   enforce "missing matrix + existing Step4 data = config error", preserving
//!   the 338-test baseline and Step4/6 behavior; left for an upstream ruling).
//! - Diagnostics carry version strings/stable codes/subject identities only,
//!   never source plaintext; with `compatible=false` a compile refuses
//!   admission, touches no facts, creates no task, and (on a real run)
//!   idempotently inserts a `compatibility_conflict` review (A16).

use crate::compile::config::{CompatibilitySpec, CompilePolicy, DomainIdentity};
use crate::kernel::SqliteKernel;
use crate::types::error::{Error, Result};
use crate::types::CompileContext;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// 稳定违规码（§6.4「版本缺失」）：字段空缺/未记录（含 legacy 快照缺版本）。
/// Stable violation code (§6.4 "missing version"): the field is absent or never
/// recorded (including legacy snapshots without a version).
pub const COMPATIBILITY_VERSION_MISSING: &str = "VERSION_MISSING";

/// 稳定违规码（A13）：观察值不是 strict semver。
/// Stable violation code (A13): the observed value is not strict semver.
pub const COMPATIBILITY_VERSION_INVALID: &str = "VERSION_INVALID";

/// 稳定违规码（A13）：合法 semver 但不在声明的 `VersionReq` 范围内。
/// Stable violation code (A13): a legal semver outside the declared `VersionReq`
/// range.
pub const COMPATIBILITY_VERSION_RANGE: &str = "VERSION_RANGE";

/// 稳定违规码（A13/D10）：artifact 不在显式允许列表内。
/// Stable violation code (A13/D10): the artifact is outside the explicit
/// allowlist.
pub const COMPATIBILITY_ARTIFACT_NOT_ALLOWED: &str = "ARTIFACT_NOT_ALLOWED";

/// 稳定违规码（A14）：`dependencies_json` 快照解析失败——不静默跳过。
/// Stable violation code (A14): the `dependencies_json` snapshot fails to parse
/// — never silently skipped.
pub const COMPATIBILITY_CORRUPT_SNAPSHOT: &str = "CORRUPT_SNAPSHOT";

/// 稳定违规码（A14）：`frontmatter_json` 解析失败或 `quality_policy` 形状损坏。
/// Stable violation code (A14): `frontmatter_json` fails to parse or the
/// `quality_policy` shape is corrupt.
pub const COMPATIBILITY_CORRUPT_FRONTMATTER: &str = "CORRUPT_FRONTMATTER";

/// 兼容 preflight 拒绝的稳定错误前缀（§7 CLI 退出码分类惯例）：executor 把
/// 不兼容结果包成 `Error::InvalidConfig("<前缀>…")`，CLI 用 `contains` 匹配后
/// 以退出码 3（配置/迁移错误语义）结束 run，绝不解析其余正文。
/// Stable error prefix of a compatibility-preflight rejection (the §7 CLI
/// exit-code classification convention): the executor wraps the incompatible
/// result as `Error::InvalidConfig("<prefix>…")`; the CLI matches via `contains`
/// and ends the run with exit code 3 (config/migration-error semantics), never
/// parsing the rest of the message.
pub const COMPATIBILITY_REJECTED_PREFIX: &str = "compatibility preflight rejected";

/// 审核 reason_json / subject_json 的体积上限（§5.2 canonical JSON ≤64 KiB 同一
/// 纪律；超限 fail-closed，不截断、不猜删）。
/// Size cap for the review reason_json / subject_json (the same §5.2 discipline
/// of canonical JSON ≤64 KiB; overruns fail closed — never truncated, never
/// guessed away).
const MAX_COMPATIBILITY_JSON_BYTES: usize = 64 * 1024;

/// 一条兼容违规（§6.4 诊断形状）：主体（`page:<id>` / `task:<id>`）、稳定
/// code、字段与观察值/期望（仅版本串与描述，不含源明文）。
/// One compatibility violation (the §6.4 diagnostic shape): the subject
/// (`page:<id>` / `task:<id>`), the stable code, the field and the observed/
/// expected pair (version strings and descriptions only, never source
/// plaintext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityViolation {
    /// 违规主体标识：`page:<page_id>` 或 `task:<task_id>`。
    /// Violation subject: `page:<page_id>` or `task:<task_id>`.
    pub subject: String,
    /// 稳定违规码（[`COMPATIBILITY_VERSION_MISSING`] 等）。
    /// Stable violation code ([`COMPATIBILITY_VERSION_MISSING`] etc.).
    pub code: String,
    /// 违规字段（domain_pack_version / artifact_version / schema_version /
    /// prompt_version / quality_policy.artifact_version / dependencies_json）。
    /// The violated field (domain_pack_version / artifact_version /
    /// schema_version / prompt_version / quality_policy.artifact_version /
    /// dependencies_json).
    pub field: String,
    /// 观察值（版本串或 "absent"/"unparseable"；不含源明文）。
    /// The observed value (a version string or "absent"/"unparseable"; never
    /// source plaintext).
    pub observed: String,
    /// 期望（`VersionReq` 文本 / artifact 允许列表 / "strict semver"）。
    /// The expectation (the `VersionReq` text / the artifact allowlist / "strict
    /// semver").
    pub expected: String,
}

/// 稳定告警码（B6/STEP8-028 补偿）：domain.yaml 缺失 `compatibility` 段——
/// legacy 只读容忍（无矩阵可查）继续成立，但 `domain check` 必须显式告警而非
/// 静默直通。告警不是违规：不翻转 `compatible`。
/// Stable warning code (B6 / the STEP8-028 compensation): the domain.yaml lacks
/// a `compatibility` section — the legacy read-only tolerance (nothing to check)
/// stands, but `domain check` must surface an explicit warning instead of a
/// silent pass-through. A warning is not a violation: it never flips
/// `compatible`.
pub const COMPATIBILITY_MATRIX_MISSING: &str = "MISSING_COMPATIBILITY_MATRIX";

/// 一条非阻断告警（B6）：稳定 code + 当前 domain 身份（与 compatibility_conflict
/// 的 canonical subject 同源——domain 加当前四版本，缺失版本为 null）。
/// One non-blocking warning (B6): a stable code plus the current domain identity
/// (same source as the compatibility_conflict canonical subject — the domain and
/// the current four versions, absent ones as null).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityWarning {
    /// 稳定告警码（[`COMPATIBILITY_MATRIX_MISSING`] 等）。
    /// Stable warning code ([`COMPATIBILITY_MATRIX_MISSING`] etc.).
    pub code: String,
    /// 当前 domain 名。
    /// The current domain name.
    pub domain: String,
    /// 当前领域包版本。
    /// The current domain-pack version.
    pub domain_pack_version: String,
    /// 当前 schema 版本（缺失为 null）。
    /// The current schema version (null when absent).
    pub schema_version: Option<String>,
    /// 当前 prompt 版本（缺失为 null）。
    /// The current prompt version (null when absent).
    pub prompt_version: Option<String>,
    /// 当前 artifact 版本。
    /// The current artifact version.
    pub artifact_version: String,
}

/// 矩阵缺失告警构造（B6；字段与 [`compatibility_conflict_payloads`] 的 canonical
/// subject 逐项同源，保证两处告警身份一致）。
/// Builds the matrix-missing warning (B6; the fields mirror the canonical
/// subject of [`compatibility_conflict_payloads`] item for item, keeping the two
/// warning identities aligned).
pub fn missing_matrix_warning(current: &DomainIdentity) -> CompatibilityWarning {
    CompatibilityWarning {
        code: COMPATIBILITY_MATRIX_MISSING.into(),
        domain: current.domain.clone(),
        domain_pack_version: current.version.to_string(),
        schema_version: current.schema_version.as_ref().map(Version::to_string),
        prompt_version: current.prompt_version.as_ref().map(Version::to_string),
        artifact_version: current.artifact_version.clone(),
    }
}

/// 兼容 preflight 报告（§6.4 契约）：`--json` 直接渲染本对象（§7）。
/// The compatibility-preflight report (the §6.4 contract); `--json` renders this
/// object directly (§7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    /// 全部检查通过为 true；任一 violation 即 false。
    /// True when every check passes; any violation makes it false.
    pub compatible: bool,
    /// 已检查的 accepted 页数量（本 domain）。
    /// Number of accepted pages checked (this domain).
    pub checked_pages: u64,
    /// 已检查的 pending/running/dead 任务数量（本 domain）。
    /// Number of pending/running/dead tasks checked (this domain).
    pub checked_tasks: u64,
    /// 全部违规（空 = 兼容）。
    /// Every violation (empty = compatible).
    pub violations: Vec<CompatibilityViolation>,
    /// 非阻断告警（B6/STEP8-028 补偿；如矩阵缺失）。空时整个字段从 JSON 省略，
    /// 使无告警输出保持 §6.4 形状（§7「stdout 只有 CompatibilityReport」）；
    /// `serde(default)` 让缺字段的旧 JSON 仍可读。
    /// Non-blocking warnings (B6 / the STEP8-028 compensation; e.g. a missing
    /// matrix). When empty the whole field is omitted from the JSON so a
    /// warning-free output keeps the §6.4 shape (§7's "stdout carries only the
    /// CompatibilityReport"); `serde(default)` keeps old JSON without the field
    /// readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<CompatibilityWarning>,
}

/// 兼容 preflight 的 accepted 页身份行（kernel 只读面返回；frontmatter_json 为
/// 原文，解析在 checker 侧）。
/// Accepted-page identity row for the preflight (returned by the kernel
/// read-only surface; frontmatter_json is raw and parsed checker-side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedPageIdentity {
    pub page_id: String,
    pub domain_pack_version: String,
    pub artifact_version: String,
    pub frontmatter_json: String,
}

/// 兼容 preflight 的任务快照行（kernel 只读面返回；仅 pending/running/dead）。
/// Task-snapshot row for the preflight (kernel read-only surface;
/// pending/running/dead only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTaskSnapshot {
    pub task_id: i64,
    pub entity_id: String,
    pub dependencies_json: String,
}

/// 兼容检查器（§6.4 签名）：确定性、只读、无网络；未来实现只能收窄/替换，
/// 不能绕过「全量扫描 + 损坏 JSON 仍 violation」契约。
/// The compatibility checker (§6.4 signature): deterministic, read-only and
/// offline; future implementations may only narrow/replace it, never bypass the
/// "full scan + corrupt JSON still a violation" contract.
pub trait CompatibilityChecker: Send + Sync {
    fn check(
        &self,
        current: &DomainIdentity,
        spec: &CompatibilitySpec,
        db: &SqliteKernel,
    ) -> Result<CompatibilityReport>;
}

/// 默认兼容检查器（D10/D11）：kernel SQL 读取 + 纯 semver/allowlist 评估。
/// The default compatibility checker (D10/D11): kernel SQL reads plus pure
/// semver/allowlist evaluation.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardCompatibilityChecker;

impl StandardCompatibilityChecker {
    /// 构造默认检查器（零字段，保留构造函数稳定调用面）。
    /// Builds the default checker (zero fields; the constructor keeps a stable
    /// call surface).
    pub fn new() -> Self {
        Self
    }
}

impl CompatibilityChecker for StandardCompatibilityChecker {
    /// 全量 preflight（§6.4 逐字）：accepted 页身份 + pending/running/dead 任务
    /// 快照；任务按实体键首段（`domain:type:slug`）过滤到当前 domain——compile_
    /// tasks 无 domain 列（Step4 先例），避免 LIKE 通配转义交给 Rust 侧精确
    /// 比较。
    /// The full preflight (§6.4 verbatim): accepted-page identities plus
    /// pending/running/dead task snapshots; tasks are scoped to the current
    /// domain by the leading segment of the entity key (`domain:type:slug`) —
    /// compile_tasks has no domain column (a Step4 precedent), so the filter is
    /// an exact Rust-side comparison instead of LIKE-escape juggling.
    fn check(
        &self,
        current: &DomainIdentity,
        spec: &CompatibilitySpec,
        db: &SqliteKernel,
    ) -> Result<CompatibilityReport> {
        let pages = db.compatibility_page_identities(&current.domain)?;
        let tasks: Vec<PersistedTaskSnapshot> = db
            .compatibility_task_snapshots()?
            .into_iter()
            .filter(|t| entity_domain(&t.entity_id) == current.domain.as_str())
            .collect();
        let mut violations = evaluate_page_identities(&pages, spec);
        violations.extend(evaluate_task_snapshots(&tasks, spec));
        Ok(CompatibilityReport {
            compatible: violations.is_empty(),
            checked_pages: pages.len() as u64,
            checked_tasks: tasks.len() as u64,
            violations,
            warnings: Vec::new(),
        })
    }
}

/// 库级兼容 check 入口（D11「同一 preflight」）：domain check（B6 CLI）与
/// executor 首次 admission 前的 preflight 共用同一 checker 与同一报告形状。
/// spec=None → §5.1 legacy 只读容忍：无矩阵可查，返回空兼容报告，绝不猜测
/// （偏差 STEP8-028：executor 侧不对「缺失矩阵 + 已有 Step4 数据」强制配置
/// 错误，保持 338 基线；留上层拍板）。空库 + Some(spec) → checker 空扫描，
/// 空 report 视为 compatible（A15）。
/// The library-level compatibility-check entry (D11 "the same preflight"): the
/// domain check (B6 CLI) and the executor's pre-first-admission preflight share
/// this checker and report shape. spec=None → the §5.1 legacy read-only
/// tolerance: with no matrix there is nothing to check, so an empty compatible
/// report is returned and nothing is guessed (deviation STEP8-028: the executor
/// does not enforce "missing matrix + existing Step4 data = config error",
/// preserving the 338 baseline; left for an upstream ruling). An empty database
/// with Some(spec) → an empty checker scan, and an empty report counts as
/// compatible (A15).
pub fn check_domain_compatibility(
    db: &SqliteKernel,
    checker: &dyn CompatibilityChecker,
    current: &DomainIdentity,
    spec: Option<&CompatibilitySpec>,
) -> Result<CompatibilityReport> {
    match spec {
        None => Ok(CompatibilityReport {
            compatible: true,
            checked_pages: 0,
            checked_tasks: 0,
            violations: Vec::new(),
            warnings: Vec::new(),
        }),
        Some(spec) => checker.check(current, spec, db),
    }
}

/// 兼容冲突审核的 canonical payload（§5.2/A16）：subject 必含当前 domain 与
/// 当前 domain/schema/prompt/artifact 版本（缺失版本为 null）；reason 携带稳定
/// code 与全部 violation。canonical 紧凑 JSON（键序稳定），超 64 KiB →
/// Validation（不截断）。
/// Canonical payloads of the compatibility-conflict review (§5.2/A16): the
/// subject must carry the current domain plus the current domain/schema/prompt/
/// artifact versions (absent versions serialize as null); the reason carries the
/// stable code and every violation. Canonical compact JSON (stable key order);
/// over 64 KiB → Validation (never truncated).
pub fn compatibility_conflict_payloads(
    current: &DomainIdentity,
    report: &CompatibilityReport,
) -> Result<(String, String)> {
    let subject = canonical_json_text(&serde_json::json!({
        "artifact_version": current.artifact_version,
        "domain": current.domain,
        "domain_pack_version": current.version.to_string(),
        "prompt_version": current.prompt_version.as_ref().map(Version::to_string),
        "schema_version": current.schema_version.as_ref().map(Version::to_string),
    }))?;
    let reason = canonical_json_text(&serde_json::json!({
        "code": "COMPATIBILITY_PREFLIGHT_FAILED",
        "violations": report.violations,
    }))?;
    for (name, text) in [("subject_json", &subject), ("reason_json", &reason)] {
        if text.len() > MAX_COMPATIBILITY_JSON_BYTES {
            return Err(Error::Validation(format!(
                "compatibility_conflict {name} exceeds {MAX_COMPATIBILITY_JSON_BYTES} bytes"
            )));
        }
    }
    Ok((subject, reason))
}

/// canonical 紧凑 JSON 文本（serde_json BTreeMap 保键序；与 kernel quality_json
/// 同一 canonical 纪律）。
/// Canonical compact JSON text (serde_json BTreeMap keeps key order; the same
/// canonical discipline as the kernel's quality_json).
fn canonical_json_text(value: &Value) -> Result<String> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&value)?)
}

/// 实体键（`domain:type:slug`）首段即域；无分隔符时整串视为域（保守匹配）。
/// The leading segment of an entity key (`domain:type:slug`) is the domain;
/// without a separator the whole string counts as the domain (conservative
/// matching).
fn entity_domain(entity_id: &str) -> &str {
    match entity_id.split_once(':') {
        Some((domain, _)) => domain,
        None => entity_id,
    }
}

/// 单一 semver 字段检查（A13）：缺失/非法/越界分别对应 MISSING/INVALID/RANGE；
/// `VersionReq::matches` 正式比较（不按字符串排序）。返回 None = 通过。
/// A single semver-field check (A13): missing/invalid/out-of-range map to
/// MISSING/INVALID/RANGE respectively; `VersionReq::matches` compares formally
/// (never string-sorted). None = pass.
fn check_semver_field(
    subject: &str,
    field: &str,
    observed: &str,
    req: &semver::VersionReq,
) -> Option<CompatibilityViolation> {
    let trimmed = observed.trim();
    if trimmed.is_empty() {
        return Some(CompatibilityViolation {
            subject: subject.to_string(),
            code: COMPATIBILITY_VERSION_MISSING.into(),
            field: field.into(),
            observed: "absent".into(),
            expected: req.to_string(),
        });
    }
    match Version::parse(trimmed) {
        Err(_) => Some(CompatibilityViolation {
            subject: subject.to_string(),
            code: COMPATIBILITY_VERSION_INVALID.into(),
            field: field.into(),
            observed: trimmed.into(),
            expected: "strict semver".into(),
        }),
        Ok(version) if !req.matches(&version) => Some(CompatibilityViolation {
            subject: subject.to_string(),
            code: COMPATIBILITY_VERSION_RANGE.into(),
            field: field.into(),
            observed: trimmed.into(),
            expected: req.to_string(),
        }),
        Ok(_) => None,
    }
}

/// artifact 允许列表检查（A13/D10）：缺失 → MISSING；不在列表 → NOT_ALLOWED。
/// The artifact-allowlist check (A13/D10): absent → MISSING; outside the list →
/// NOT_ALLOWED.
fn check_artifact_field(
    subject: &str,
    field: &str,
    observed: &str,
    allow: &BTreeSet<String>,
) -> Option<CompatibilityViolation> {
    let trimmed = observed.trim();
    if trimmed.is_empty() {
        return Some(CompatibilityViolation {
            subject: subject.to_string(),
            code: COMPATIBILITY_VERSION_MISSING.into(),
            field: field.into(),
            observed: "absent".into(),
            expected: allowlist_text(allow),
        });
    }
    if !allow.contains(trimmed) {
        return Some(CompatibilityViolation {
            subject: subject.to_string(),
            code: COMPATIBILITY_ARTIFACT_NOT_ALLOWED.into(),
            field: field.into(),
            observed: trimmed.into(),
            expected: allowlist_text(allow),
        });
    }
    None
}

/// 允许列表的稳定文本（BTreeSet 字节序，报告确定性）。
/// Stable text of an allowlist (BTreeSet byte order; deterministic reports).
fn allowlist_text(allow: &BTreeSet<String>) -> String {
    let items: Vec<&str> = allow.iter().map(String::as_str).collect();
    format!("[{}]", items.join(","))
}

/// accepted 页身份评估（§6.4：domain_pack_version / artifact_version /
/// frontmatter quality_policy）。frontmatter 解析失败或 quality_policy 形状
/// 损坏 → CORRUPT_FRONTMATTER；缺 quality_policy（legacy seed 页）→ MISSING
/// （「版本缺失…均为不兼容」，fail-closed）。
/// Accepted-page identity evaluation (§6.4: domain_pack_version /
/// artifact_version / frontmatter quality_policy). An unparseable frontmatter or
/// corrupt quality_policy shape → CORRUPT_FRONTMATTER; an absent
/// quality_policy (legacy seed pages) → MISSING ("missing versions are always
/// incompatible", fail-closed).
pub(crate) fn evaluate_page_identities(
    pages: &[PersistedPageIdentity],
    spec: &CompatibilitySpec,
) -> Vec<CompatibilityViolation> {
    let mut out = Vec::new();
    for page in pages {
        let subject = format!("page:{}", page.page_id);
        if let Some(v) = check_semver_field(
            &subject,
            "domain_pack_version",
            &page.domain_pack_version,
            &spec.domain_pack,
        ) {
            out.push(v);
        }
        if let Some(v) = check_artifact_field(
            &subject,
            "artifact_version",
            &page.artifact_version,
            &spec.artifact,
        ) {
            out.push(v);
        }
        // frontmatter：解析失败不静默跳过（A14「损坏 JSON 仍 violation」）。
        // Frontmatter: parse failures are never silently skipped (A14 "corrupt
        // JSON is still a violation").
        let frontmatter: Value = match serde_json::from_str(&page.frontmatter_json) {
            Ok(v) => v,
            Err(_) => {
                out.push(CompatibilityViolation {
                    subject: subject.clone(),
                    code: COMPATIBILITY_CORRUPT_FRONTMATTER.into(),
                    field: "frontmatter_json".into(),
                    observed: "unparseable".into(),
                    expected: "JSON object".into(),
                });
                continue;
            }
        };
        match frontmatter.get("quality_policy") {
            Some(Value::Object(policy)) => {
                let observed = policy
                    .get("artifact_version")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(v) = check_artifact_field(
                    &subject,
                    "quality_policy.artifact_version",
                    observed,
                    &spec.artifact,
                ) {
                    out.push(v);
                }
            }
            Some(_) => out.push(CompatibilityViolation {
                subject: subject.clone(),
                code: COMPATIBILITY_CORRUPT_FRONTMATTER.into(),
                field: "quality_policy".into(),
                observed: "not an object".into(),
                expected: "JSON object".into(),
            }),
            None => out.push(CompatibilityViolation {
                subject: subject.clone(),
                code: COMPATIBILITY_VERSION_MISSING.into(),
                field: "quality_policy.artifact_version".into(),
                observed: "absent".into(),
                expected: allowlist_text(&spec.artifact),
            }),
        }
    }
    out
}

/// `dependencies_json` 快照的读取形状（§7 步骤 3 载体 {context, policy, schema};
/// 本镜像只取检查所需两段，schema 段忽略；任一必需字段缺失/类型不符 → 解析
/// 失败 = CORRUPT_SNAPSHOT）。
/// The read shape of a `dependencies_json` snapshot (the §7 item-3 carrier
/// {context, policy, schema}; this mirror takes only the two checked sections
/// and ignores the schema section; any missing/ill-typed required field → parse
/// failure = CORRUPT_SNAPSHOT).
#[derive(Deserialize)]
struct TaskDependenciesMirror {
    context: CompileContext,
    policy: CompilePolicy,
}

/// pending/running/dead 任务快照评估（§6.4）：domain_pack/schema/prompt 版本取
/// 自 context（STEP8-011 载体），artifact_version 取自 policy；损坏 JSON →
/// CORRUPT_SNAPSHOT，不静默跳过（A14）。缺版本（legacy None）→ MISSING。
/// Pending/running/dead task-snapshot evaluation (§6.4): domain_pack/schema/
/// prompt versions come from the context (the STEP8-011 carrier) and
/// artifact_version from the policy; corrupt JSON → CORRUPT_SNAPSHOT, never
/// silently skipped (A14). Missing versions (legacy None) → MISSING.
pub(crate) fn evaluate_task_snapshots(
    tasks: &[PersistedTaskSnapshot],
    spec: &CompatibilitySpec,
) -> Vec<CompatibilityViolation> {
    let mut out = Vec::new();
    for task in tasks {
        let subject = format!("task:{}", task.task_id);
        let deps: TaskDependenciesMirror = match serde_json::from_str(&task.dependencies_json) {
            Ok(deps) => deps,
            Err(_) => {
                // 观察值只记描述，不回显快照原文（可能含知识源字段，§5.2 纪律）。
                // The observed value records a description only — the raw
                // snapshot is never echoed (it may carry knowledge-source
                // fields, the §5.2 discipline).
                out.push(CompatibilityViolation {
                    subject,
                    code: COMPATIBILITY_CORRUPT_SNAPSHOT.into(),
                    field: "dependencies_json".into(),
                    observed: "unparseable".into(),
                    expected: "JSON object {context, policy, schema}".into(),
                });
                continue;
            }
        };
        if let Some(v) = check_semver_field(
            &subject,
            "domain_pack_version",
            &deps.context.domain_pack_version,
            &spec.domain_pack,
        ) {
            out.push(v);
        }
        for (field, observed, req) in [
            (
                "schema_version",
                deps.context.schema_version.as_deref(),
                &spec.schema,
            ),
            (
                "prompt_version",
                deps.context.prompt_version.as_deref(),
                &spec.prompt,
            ),
        ] {
            let violation = match observed {
                None => CompatibilityViolation {
                    subject: subject.clone(),
                    code: COMPATIBILITY_VERSION_MISSING.into(),
                    field: field.into(),
                    observed: "absent".into(),
                    expected: req.to_string(),
                },
                Some(text) => match check_semver_field(&subject, field, text, req) {
                    Some(v) => v,
                    None => continue,
                },
            };
            out.push(violation);
        }
        if let Some(v) = check_artifact_field(
            &subject,
            "artifact_version",
            &deps.policy.artifact_version,
            &spec.artifact,
        ) {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// spec §5.1 YAML 示例矩阵。
    /// The §5.1 YAML sample matrix.
    fn spec() -> CompatibilitySpec {
        serde_yaml_ng::from_str(
            "domain_pack: \">=1.0.0,<2.0.0\"\n\
             schema: \">=2.0.0,<3.0.0\"\n\
             prompt: \">=3.0.0,<4.0.0\"\n\
             artifact: [\"wiki-v1\", \"wiki-v2\"]",
        )
        .unwrap()
    }

    fn page(
        page_id: &str,
        domain_pack_version: &str,
        artifact_version: &str,
        frontmatter_json: &str,
    ) -> PersistedPageIdentity {
        PersistedPageIdentity {
            page_id: page_id.into(),
            domain_pack_version: domain_pack_version.into(),
            artifact_version: artifact_version.into(),
            frontmatter_json: frontmatter_json.into(),
        }
    }

    fn task(task_id: i64, entity_id: &str, dependencies_json: &str) -> PersistedTaskSnapshot {
        PersistedTaskSnapshot {
            task_id,
            entity_id: entity_id.into(),
            dependencies_json: dependencies_json.into(),
        }
    }

    /// 合法页身份（quality_policy 与列值一致且均在允许列表内）。
    /// A legal page identity (quality_policy agrees with the columns and both
    /// are inside the allowlist).
    fn ok_page(page_id: &str, domain_pack_version: &str) -> PersistedPageIdentity {
        page(
            page_id,
            domain_pack_version,
            "wiki-v2",
            r#"{"quality_policy":{"artifact_version":"wiki-v2"},"title":"t"}"#,
        )
    }

    fn deps_json(
        domain_pack_version: &str,
        schema_version: Option<&str>,
        prompt_version: Option<&str>,
        artifact_version: &str,
    ) -> String {
        // 与 compile_tasks.dependencies_json 同形状（§7 步骤 3：context/policy/
        // schema）；policy 段为 B1 旧形状（无 Step8 字段，serde default 兜底），
        // 与 config.rs 旧快照可读性测试逐字段一致。
        // The compile_tasks.dependencies_json shape (§7 item 3: context/policy/
        // schema); the policy section is the B1 legacy shape (no Step8 fields,
        // serde defaults backfill), field-for-field identical to the old-snapshot
        // readability test in config.rs.
        canonical_json_text(&serde_json::json!({
            "context": {
                "domain_pack_version": domain_pack_version,
                "prompt_template": "SYSTEM",
                "model_version": "mock-v1",
                "embedding_model": "none",
                "quality_threshold": 0.75,
                "require_source_refs": true,
                "schema_version": schema_version,
                "prompt_version": prompt_version,
            },
            "policy": {
                "compiler_version": "compile-v1",
                "artifact_version": artifact_version,
                "scorer_version": "rules-v1",
                "knowledge_fields": [],
                "sensitive_fields": [],
                "required_headings": ["概述"],
                "min_coverage": 0.6,
                "min_density": 0.4,
                "max_recompiles": 2,
                "max_retries": 3,
                "task_token_budget": 65536,
                "batch_token_budget": 262144,
                "daily_token_budget": null,
                "max_output_tokens": 2048,
                "lease_seconds": 300,
                "heartbeat_seconds": 30,
            },
        }))
        .unwrap()
    }

    // A13：semver 矩阵 —— 合法范围接受、非法 semver 拒绝、不按字符串排序
    // 误判（1.10.0 在 >=1.0.0,<2.0.0 内；0.9.9/2.0.0 越界；字符串序下
    // "1.10.0" < "1.2.0" 会误拒合法版本，正式 VersionReq 比较不会）。
    // A13: the semver matrix — legal ranges accept, invalid semver rejects, and
    // string-sorting never misjudges (1.10.0 is inside >=1.0.0,<2.0.0 while
    // 0.9.9/2.0.0 are outside; lexicographic order would place "1.10.0" before
    // "1.2.0" and wrongly reject a legal version — formal VersionReq matching
    // does not).
    #[test]
    fn a13_semver_matrix_accepts_and_rejects_formally() {
        let spec = spec();
        let pages = vec![
            ok_page("p-1", "1.2.0"),
            ok_page("p-2", "1.10.0"),
            ok_page("p-3", "0.9.9"),
            ok_page("p-4", "2.0.0"),
            ok_page("p-5", "test-v1"),
            ok_page("p-6", ""),
        ];
        let violations = evaluate_page_identities(&pages, &spec);
        let domain_violations: Vec<(&str, &str)> = violations
            .iter()
            .filter(|v| v.field == "domain_pack_version")
            .map(|v| (v.subject.as_str(), v.code.as_str()))
            .collect();
        assert_eq!(
            domain_violations,
            vec![
                ("page:p-3", COMPATIBILITY_VERSION_RANGE),
                ("page:p-4", COMPATIBILITY_VERSION_RANGE),
                ("page:p-5", COMPATIBILITY_VERSION_INVALID),
                ("page:p-6", COMPATIBILITY_VERSION_MISSING),
            ]
        );
    }

    // A13：artifact 显式允许列表 —— 列内接受、列外/缺失拒绝（列与
    // quality_policy.artifact_version 双份口径一致）。
    // A13: the explicit artifact allowlist — inside accepts, outside/absent
    // rejects (column and quality_policy.artifact_version agree).
    #[test]
    fn a13_artifact_allowlist_is_enforced() {
        let spec = spec();
        let pages = vec![
            ok_page("p-ok", "1.2.0"),
            page(
                "p-out",
                "1.2.0",
                "wiki-v3",
                r#"{"quality_policy":{"artifact_version":"wiki-v3"}}"#,
            ),
            page(
                "p-mixed",
                "1.2.0",
                "wiki-v2",
                r#"{"quality_policy":{"artifact_version":"wiki-v9"}}"#,
            ),
            page("p-legacy-seed", "1.2.0", "seed-v1", r#"{"title":"t"}"#),
            page("p-no-policy", "1.2.0", "wiki-v2", r#"{"title":"t"}"#),
        ];
        let violations = evaluate_page_identities(&pages, &spec);
        let artifact_violations: Vec<(&str, &str, &str)> = violations
            .iter()
            .filter(|v| v.code == COMPATIBILITY_ARTIFACT_NOT_ALLOWED)
            .map(|v| (v.subject.as_str(), v.field.as_str(), v.observed.as_str()))
            .collect();
        assert_eq!(
            artifact_violations,
            vec![
                ("page:p-out", "artifact_version", "wiki-v3"),
                ("page:p-out", "quality_policy.artifact_version", "wiki-v3"),
                ("page:p-mixed", "quality_policy.artifact_version", "wiki-v9"),
                ("page:p-legacy-seed", "artifact_version", "seed-v1"),
            ]
        );
        // 缺 quality_policy（legacy seed）也是 violation（版本缺失不兼容）。
        // An absent quality_policy (legacy seed) is a violation too (missing
        // versions are incompatible).
        assert!(violations.iter().any(|v| v.subject == "page:p-no-policy"
            && v.field == "quality_policy.artifact_version"
            && v.code == COMPATIBILITY_VERSION_MISSING));
        // 损坏 frontmatter → CORRUPT_FRONTMATTER（不静默跳过）。
        // A corrupt frontmatter → CORRUPT_FRONTMATTER (never silently skipped).
        let corrupt = vec![page("p-corrupt", "1.2.0", "wiki-v2", "not json at all")];
        let violations = evaluate_page_identities(&corrupt, &spec);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, COMPATIBILITY_CORRUPT_FRONTMATTER);
        assert_eq!(violations[0].field, "frontmatter_json");
    }

    // A13：任务快照的 schema/prompt 版本矩阵 —— 合法接受、越界拒绝、缺失
    // （legacy None）拒绝；domain_pack 与 artifact 同矩阵约束。
    // A13: the schema/prompt version matrix of task snapshots — legal accepts,
    // out-of-range rejects, missing (legacy None) rejects; domain_pack and
    // artifact follow the same matrix constraints.
    #[test]
    fn a13_task_snapshot_schema_prompt_matrix() {
        let spec = spec();
        let tasks = vec![
            task(
                1,
                "milk-tea:drink:boba",
                &deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2"),
            ),
            task(
                2,
                "milk-tea:drink:lemon",
                &deps_json("1.2.0", Some("3.0.0"), Some("3.0.0"), "wiki-v2"),
            ),
            task(
                3,
                "milk-tea:drink:cheese",
                &deps_json("1.2.0", None, Some("3.0.0"), "wiki-v2"),
            ),
            task(
                4,
                "milk-tea:drink:cocoa",
                &deps_json("0.9.0", Some("2.1.0"), Some("3.0.0"), "wiki-v9"),
            ),
        ];
        let violations = evaluate_task_snapshots(&tasks, &spec);
        let hits: Vec<(i64, &str, &str)> = violations
            .iter()
            .map(|v| {
                let id: i64 = v.subject.trim_start_matches("task:").parse().unwrap();
                (id, v.field.as_str(), v.code.as_str())
            })
            .collect();
        assert_eq!(
            hits,
            vec![
                (2, "schema_version", COMPATIBILITY_VERSION_RANGE),
                (3, "schema_version", COMPATIBILITY_VERSION_MISSING),
                (4, "domain_pack_version", COMPATIBILITY_VERSION_RANGE),
                (4, "artifact_version", COMPATIBILITY_ARTIFACT_NOT_ALLOWED),
            ]
        );
    }

    // A14：损坏 dependencies_json 为 violation（CORRUPT_SNAPSHOT），不静默
    // 跳过；观察值不回显快照原文。
    // A14: a corrupt dependencies_json is a violation (CORRUPT_SNAPSHOT), never
    // silently skipped; the observed value never echoes the raw snapshot.
    #[test]
    fn a14_corrupt_dependencies_json_is_a_violation() {
        let spec = spec();
        for corrupt in ["not json", "[]", r#"{"context":{}}"#, r#"{"policy":1}"#] {
            let violations =
                evaluate_task_snapshots(&[task(7, "milk-tea:drink:boba", corrupt)], &spec);
            assert_eq!(violations.len(), 1, "corrupt input: {corrupt}");
            assert_eq!(violations[0].code, COMPATIBILITY_CORRUPT_SNAPSHOT);
            assert_eq!(violations[0].field, "dependencies_json");
            assert_eq!(violations[0].observed, "unparseable");
            assert!(
                !violations[0].observed.contains(corrupt),
                "raw snapshot bytes must never be echoed"
            );
        }
    }

    // A14：全量扫描计数准确 —— 每页/每任务各计一次，compatible=false 当且仅
    // 当存在 violation；全兼容输入产生空 violations。
    // A14: full-scan counts are exact — every page/task counts once, and
    // compatible=false iff a violation exists; fully compatible inputs yield an
    // empty violations list.
    #[test]
    fn a14_full_scan_counts_and_compatible_flag() {
        let spec = spec();
        let pages = vec![ok_page("p-1", "1.2.0"), ok_page("p-2", "1.9.9")];
        let tasks = vec![task(
            1,
            "milk-tea:drink:boba",
            &deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v1"),
        )];
        let mut violations = evaluate_page_identities(&pages, &spec);
        violations.extend(evaluate_task_snapshots(&tasks, &spec));
        assert!(violations.is_empty(), "all inputs satisfy the matrix");
        let report = CompatibilityReport {
            compatible: violations.is_empty(),
            checked_pages: pages.len() as u64,
            checked_tasks: tasks.len() as u64,
            violations,
            warnings: Vec::new(),
        };
        assert!(report.compatible);
        assert_eq!(report.checked_pages, 2);
        assert_eq!(report.checked_tasks, 1);

        // 一条违规即 compatible=false。
        // A single violation flips compatible=false.
        let bad = vec![ok_page("p-3", "9.9.9")];
        let violations = evaluate_page_identities(&bad, &spec);
        assert_eq!(violations.len(), 1);
        assert!(
            !CompatibilityReport {
                compatible: violations.is_empty(),
                checked_pages: 1,
                checked_tasks: 0,
                violations,
                warnings: Vec::new(),
            }
            .compatible
        );
    }

    // §6.4：已 superseded 的历史 attempt 不进入检查面（评估器只消费
    // pending/running/dead 快照行——kernel 读取面的 SQL 过滤在 kernel 测试锁定）。
    // §6.4: explicitly superseded historical attempts stay outside the check
    // surface (the evaluator consumes pending/running/dead snapshot rows only —
    // the kernel read-side SQL filter is pinned by kernel tests).

    // 兼容冲突 payload：subject 含 domain 与当前四版本（缺失为 null）、字节级
    // 确定性（同输入同串，A16 幂等键），reason 携带全部 violation。
    // Compatibility-conflict payloads: the subject carries the domain and the
    // current four versions (absent ones as null), byte-deterministic (same
    // input, same string — the A16 idempotency key), and the reason carries
    // every violation.
    #[test]
    fn compatibility_conflict_payloads_are_canonical_and_deterministic() {
        let identity =
            DomainIdentity::parse("milk-tea", "1.2.0", Some("2.1.0"), None, "wiki-v2").unwrap();
        let report = CompatibilityReport {
            compatible: false,
            checked_pages: 1,
            checked_tasks: 0,
            violations: vec![CompatibilityViolation {
                subject: "page:p-1".into(),
                code: COMPATIBILITY_VERSION_RANGE.into(),
                field: "domain_pack_version".into(),
                observed: "0.9.0".into(),
                expected: ">=1.0.0,<2.0.0".into(),
            }],
            warnings: Vec::new(),
        };
        let (subject, reason) = compatibility_conflict_payloads(&identity, &report).unwrap();
        assert_eq!(
            subject,
            r#"{"artifact_version":"wiki-v2","domain":"milk-tea","domain_pack_version":"1.2.0","prompt_version":null,"schema_version":"2.1.0"}"#
        );
        assert!(reason.contains(r#""code":"COMPATIBILITY_PREFLIGHT_FAILED""#));
        assert!(reason.contains(r#""subject":"page:p-1""#));
        let (subject_again, reason_again) =
            compatibility_conflict_payloads(&identity, &report).unwrap();
        assert_eq!(subject, subject_again, "A16: the subject is the UNIQUE key");
        assert_eq!(reason, reason_again);
    }

    // check_domain_compatibility：spec=None → legacy 只读容忍空报告（不触碰
    // db）；报告可 serde 往返（--json 渲染契约）。
    // check_domain_compatibility: spec=None → the legacy read-only tolerant
    // empty report (the db is untouched); the report serde round-trips (the
    // --json rendering contract).
    #[test]
    fn none_spec_is_legacy_tolerant_and_report_serializes() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let identity = DomainIdentity::parse("d", "1.0.0", None, None, "wiki-v1").unwrap();
        let report = check_domain_compatibility(
            &kernel,
            &StandardCompatibilityChecker::new(),
            &identity,
            None,
        )
        .unwrap();
        assert!(report.compatible);
        assert_eq!(report.checked_pages, 0);
        assert_eq!(report.checked_tasks, 0);
        assert!(report.violations.is_empty());

        let json = serde_json::to_string(&report).unwrap();
        let back: CompatibilityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }

    // Step8 批 B6 / STEP8-028 补偿：矩阵缺失告警的稳定形状 —— code 固定、身份
    // 四版本与 compatibility_conflict 的 canonical subject 逐项同源（缺失版本为
    // null）；空 warnings 从 JSON 省略（§6.4 形状不变），带告警的 JSON 可读回。
    // Step8 batch B6 / the STEP8-028 compensation: the matrix-missing warning's
    // stable shape — fixed code, the identity four versions mirroring the
    // compatibility_conflict canonical subject item for item (absent ones as
    // null); empty warnings are omitted from the JSON (the §6.4 shape stays
    // intact) and a warning-bearing JSON reads back.
    #[test]
    fn b6_missing_matrix_warning_shape_and_serde_omission() {
        let identity =
            DomainIdentity::parse("milk-tea", "1.2.0", Some("2.1.0"), None, "wiki-v2").unwrap();
        let warning = missing_matrix_warning(&identity);
        assert_eq!(warning.code, COMPATIBILITY_MATRIX_MISSING);
        assert_eq!(warning.domain, "milk-tea");
        assert_eq!(warning.domain_pack_version, "1.2.0");
        assert_eq!(warning.schema_version.as_deref(), Some("2.1.0"));
        assert_eq!(warning.prompt_version, None);
        assert_eq!(warning.artifact_version, "wiki-v2");

        // 与 compatibility_conflict canonical subject 的身份字段同源。
        // Identity fields shared with the compatibility_conflict canonical
        // subject.
        let (subject, _) = compatibility_conflict_payloads(
            &identity,
            &CompatibilityReport {
                compatible: true,
                checked_pages: 0,
                checked_tasks: 0,
                violations: Vec::new(),
                warnings: Vec::new(),
            },
        )
        .unwrap();
        assert!(subject.contains(r#""domain":"milk-tea""#));
        assert!(subject.contains(r#""domain_pack_version":"1.2.0""#));
        assert!(subject.contains(r#""schema_version":"2.1.0""#));
        assert!(subject.contains(r#""prompt_version":null"#));
        assert!(subject.contains(r#""artifact_version":"wiki-v2""#));

        // 空 warnings：字段整体省略（§6.4 形状）；带告警：字段出现且可读回。
        // Empty warnings: the whole field is omitted (the §6.4 shape); with a
        // warning: the field appears and reads back.
        let mut report = check_domain_compatibility(
            &SqliteKernel::open_in_memory().unwrap(),
            &StandardCompatibilityChecker::new(),
            &identity,
            None,
        )
        .unwrap();
        let bare = serde_json::to_string(&report).unwrap();
        assert!(!bare.contains("warnings"), "bare JSON: {bare}");
        report.warnings.push(warning);
        let with_warning = serde_json::to_string(&report).unwrap();
        assert!(with_warning.contains(r#""code":"MISSING_COMPATIBILITY_MATRIX""#));
        let back: CompatibilityReport = serde_json::from_str(&with_warning).unwrap();
        assert_eq!(back, report);
        assert!(report.compatible, "a warning never flips compatible");
    }
}
