//! Step 4 编译管线专用事务接口（kernel 子模块；spec §8.2–§8.4，决策 D2/D5）。
//! Step 4 compile-pipeline transactional API (kernel submodule; spec §8.2–§8.4,
//! decisions D2/D5).
//!
//! 锁纪律（§8.2 硬要求）：
//! - 每个公开方法取 `conn` Mutex **一次**，用 `SqliteConnection::immediate_transaction`
//!   （BEGIN IMMEDIATE）管理事务；内部辅助函数只接 `&mut SqliteConnection`。
//! - 不把 user/model 数据插值拼接 SQL：DML 一律 `diesel::sql_query` + 按序 bind
//!   （IN 列表只拼 `?` 占位标记）或 diesel table DSL；全同步方法，无 guard 跨 await。
//! - 锁 poison → `Error::Internal`；不在本层新增 `unwrap`。
//!
//! Lock discipline (§8.2 hard requirements):
//! - Every public method takes the `conn` Mutex **once** and manages the
//!   transaction with `SqliteConnection::immediate_transaction` (BEGIN IMMEDIATE);
//!   internal helpers only take `&mut SqliteConnection`.
//! - User/model data is never interpolated into SQL: DML goes through
//!   `diesel::sql_query` with ordered binds (IN lists only splice `?` marks) or
//!   the diesel table DSL; all methods are synchronous, no guard held across await.
//! - Mutex poison → `Error::Internal`; no new `unwrap` in this layer.

use crate::compile::compatibility::{PersistedPageIdentity, PersistedTaskSnapshot};
use crate::compile::config::{
    Admission, CommitOutcome, CompilePolicy, FailureDisposition, PreparedSource, TaskLease,
};
use crate::compile::consistency::{ConsistencyReport, CONSISTENCY_CONFLICT, CONSISTENCY_SCORE};
use crate::compile::contract::{CompileEvidence, CompileFailure};
use crate::compile::hash::{content_hash, schema_to_value, HashDependencies};
use crate::compile::quality::ScoreReport;
use crate::db_schema::{
    page_quality as page_quality_t, page_sections as page_sections_t, pages as pages_t,
    qug_edges as edges_t,
};
use crate::schema::facts;
use crate::traits::EntitySchema;
use crate::types::error::{Error, Result};
use crate::types::{
    CompileContext, CompiledPage, FactValue, Facts, PageMetadata, QualityScore, QugEdge, RawEntity,
    Section, WikiPage,
};
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteKernel;

/// 租约窗口（§8.3：claim / recover / heartbeat 统一延长至 now+300 秒）。
/// Lease window (§8.3: claim / recover / heartbeat uniformly extend to now+300s).
const LEASE_WINDOW_SECONDS: i64 = 300;

/// artifact_json 体积上限（§8.1：完整 CompiledPage 仅本地、≤256 KiB）。
/// artifact_json size cap (§8.1: full CompiledPage is local-only, ≤256 KiB).
const MAX_ARTIFACT_BYTES: usize = 256 * 1024;

/// `compile_tasks.dependencies_json` 的持久化形状（§7 步骤 3：context/policy/schema）。
/// claim 反序列化还原编译上下文与预算参数；publish 还原 artifact_version 等。
/// `pub(super)`：Step6 批4 审核批准把同一形状作为 supplemental subject 的
/// `dependencies_json` 契约复用（step6 spec §8，禁止双实现漂移）。
/// Persisted shape of `compile_tasks.dependencies_json` (§7 item 3:
/// context/policy/schema). Claim deserializes the compile context and budget
/// params; publish restores artifact_version etc. `pub(super)`: Step6 batch 4
/// review approval reuses the same shape as the supplemental subject's
/// `dependencies_json` contract (step6 spec §8; no drifting duplicates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredDependencies {
    pub(super) context: CompileContext,
    pub(super) policy: CompilePolicy,
    pub(super) schema: serde_json::Value,
}

/// `pages.frontmatter_json` 的读取形状（§8.1：aliases/tags/refs/质量策略）。
/// 读取路径只需要 aliases/tags；refs/质量策略为后续构图与审计保留。
/// Read shape of `pages.frontmatter_json` (§8.1: aliases/tags/refs/quality
/// policy). The read path only needs aliases/tags; refs/quality policy are kept
/// for later graph construction and audit.
#[derive(Debug, Clone, Default, Deserialize)]
struct StoredFrontmatter {
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// 过期租约回收统计（Step8 §6.3/D8/A11）：`recover_compile_leases` 的结构化
/// 返回。字段为 spec §6.3 逐字五计数，另附扫描行数；`dead_quarantined` 当前
/// 回收路径不产生（回收 dead 分支恒为 `failed`），字段照 spec 保留以稳定观测面。
/// Expired-lease recovery stats (Step8 §6.3/D8/A11): the structured return of
/// `recover_compile_leases`. The five fields match spec §6.3 verbatim, plus the
/// scanned row count; `dead_quarantined` is never produced by the current
/// recovery path (the recovery dead branch is always `failed`) and is kept per
/// spec to stabilize the observability surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    /// 回收后重排为 pending（未超限，退避 `next_attempt_at`）的任务数。
    /// Tasks requeued as pending after recovery (under the limit, with a backoff
    /// `next_attempt_at`).
    pub recovered_pending: u64,
    /// 回收后进入 dead/failed（重试耗尽）的任务数。
    /// Tasks recovered into dead/failed (retries exhausted).
    pub dead_failed: u64,
    /// 回收后进入 dead/quarantined 的任务数（当前回收路径恒为 0）。
    /// Tasks recovered into dead/quarantined (always 0 on the current path).
    pub dead_quarantined: u64,
    /// 因 source head 前进而以 succeeded/superseded 收尾的任务数。
    /// Tasks finalized as succeeded/superseded because the source head moved on.
    pub superseded: u64,
    /// 本次回收实际新插入的审核行数（死信等；`ON CONFLICT DO NOTHING` 幂等
    /// 跳过不计入，A10）。
    /// Review rows actually inserted by this recovery pass (dead letters etc.;
    /// rows skipped by the idempotent `ON CONFLICT DO NOTHING` do not count, A10).
    pub review_inserted: u64,
    /// 扫描到的过期 running 任务总数（可选总扫描数）。
    /// Total expired running tasks scanned (the optional scan count).
    pub scanned: u64,
}

impl RecoveryStats {
    /// 被回收终结/重排的任务总数（不含审核行与扫描计数）。
    /// Total tasks recovered into a terminal/requeued state (excluding review
    /// rows and the scan count).
    pub fn recovered_tasks(&self) -> u64 {
        self.recovered_pending + self.dead_failed + self.dead_quarantined + self.superseded
    }
}

// ===== `diesel::sql_query` 行映射（QueryableByName）=====
// ===== `diesel::sql_query` row mappings (QueryableByName) =====

#[derive(QueryableByName)]
struct SourceHeadRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    source_revision: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    snapshot_hash: String,
}

#[derive(QueryableByName)]
struct AcceptedPageRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    artifact_version: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    source_revision: i64,
}

#[derive(QueryableByName)]
struct ExistingTaskRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    desired_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
}

#[derive(QueryableByName)]
struct TaskEpochRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
}

#[derive(QueryableByName)]
struct ClaimCandidate {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    desired_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    source_json: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    dependencies_json: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_token_budget: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    reserved_tokens: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    attempt_count: i64,
}

#[derive(QueryableByName)]
struct BudgetRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    token_limit: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    reserved_tokens: i64,
}

#[derive(QueryableByName)]
struct PublishTaskRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    source_revision: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    desired_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    result: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    lease_token: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    lease_expires_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    dependencies_json: String,
}

#[derive(QueryableByName)]
struct FailureTaskRow {
    /// Step8 批 B3：死信/冲突诊断需要任务身份（entity_id/desired_hash）。
    /// Step8 batch B3: the dead-letter/conflict diagnostics need the task
    /// identity (entity_id/desired_hash).
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    desired_hash: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    lease_token: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    lease_expires_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    retry_count: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    max_retries: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    recompile_count: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    dependencies_json: String,
}

#[derive(QueryableByName)]
struct ExpiredLeaseRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    epoch: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    desired_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    lease_token: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    retry_count: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    max_retries: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    attempt_count: i64,
}

#[derive(QueryableByName)]
struct GenerationRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct TextOnlyRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

#[derive(QueryableByName)]
struct AcceptedPageFullRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    source_revision: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    title: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    domain_pack_version: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    compiled_at: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    model_version: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    embedding_model: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    frontmatter_json: String,
}

#[derive(QueryableByName)]
struct QualityRow {
    #[diesel(sql_type = diesel::sql_types::Double)]
    coverage: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    citation: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    schema_compliance: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    density: f64,
    /// Step8：发布时写入的一致性值（旧行/未仲裁行为 NULL）。
    /// Step8: the consistency value written at publish (NULL for legacy rows or
    /// un-arbitrated pages).
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Double>)]
    consistency: Option<f64>,
}

/// top_k_related_pages 的质量行（Step8 §6.1：额外读取 consistency 列；独立行
/// 结构，不改变 load_accepted_pages 的既有读取形状）。
/// Quality row for `top_k_related_pages` (Step8 §6.1: also reads the
/// consistency column; a separate row struct so the existing
/// `load_accepted_pages` read shape is untouched).
#[derive(QueryableByName)]
struct RelatedQualityRow {
    #[diesel(sql_type = diesel::sql_types::Double)]
    coverage: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    citation: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    schema_compliance: f64,
    #[diesel(sql_type = diesel::sql_types::Double)]
    density: f64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Double>)]
    consistency: Option<f64>,
}

#[derive(QueryableByName)]
struct SectionRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    heading: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content: String,
}

#[derive(QueryableByName)]
struct EdgeJsonRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    edge_json: String,
}

#[derive(QueryableByName)]
struct TaskIdRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
}

#[derive(QueryableByName)]
struct ArtifactJsonRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    artifact_json: String,
}

/// compatibility_page_identities 行（Step8 批 B5 §6.4：身份四列只读投影）。
/// Row for `compatibility_page_identities` (Step8 batch B5 §6.4: a read-only
/// projection of the four identity columns).
#[derive(QueryableByName)]
struct CompatPageRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    domain_pack_version: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    artifact_version: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    frontmatter_json: String,
}

/// compatibility_task_snapshots 行（Step8 批 B5 §6.4：快照三列只读投影）。
/// Row for `compatibility_task_snapshots` (Step8 batch B5 §6.4: a read-only
/// projection of the three snapshot columns).
#[derive(QueryableByName)]
struct CompatTaskRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    task_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    dependencies_json: String,
}

/// 页面向量构建行（真实嵌入实验：accepted 页的 embedding 载荷所需六列）。
/// Row for vector-index building (real-embedding experiment: the six columns an
/// embedding payload needs from an accepted page).
#[derive(QueryableByName)]
struct AcceptedPageVectorRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    title: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
}

/// 页面向量构建行的公开载体（含生成代数，供 `wiktor vector build` 使用）。
/// Public carrier for vector-build rows (with the generation, consumed by
/// `wiktor vector build`).
pub struct AcceptedPageVector {
    pub page_id: String,
    pub entity_id: String,
    pub title: String,
    pub content: String,
    pub content_hash: String,
    pub generation: u64,
}

/// list_published_pages 行（Step 4 §10：向量 worker 的稳定键读取接口）。
/// Row for `list_published_pages` (Step 4 §10: the stable-key reader for the
/// future vector worker).
#[derive(QueryableByName)]
struct PublishedPageRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    embedding_model: String,
}

/// validate_vector_payloads 返回行（§10 必要边界修复：RRF 前批量校验）。
/// Row for `validate_vector_payloads` (§10 boundary fix: bulk pre-RRF validation).
#[derive(QueryableByName)]
struct ValidPageIdRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
}

// ===== 纯函数辅助 =====
// ===== Pure helpers =====

/// canonical JSON 文本：递归按 UTF-8 字节序排键、紧凑输出（§8.1：TEXT JSON 均
/// canonical UTF-8）。`serde_json::to_value` 默认 BTreeMap 保键序。
/// Canonical JSON text: recursively key-sorted by UTF-8 byte order, compact
/// (§8.1: TEXT JSON is canonical UTF-8). `serde_json::to_value` uses BTreeMap,
/// preserving key order.
fn canonical_text<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&value)?)
}

/// revision 限制 1..=i64::MAX（§3.1：禁止 `as i64` 高位截断/静默回退 1）。
/// Revision bounds 1..=i64::MAX (§3.1: no `as i64` truncation, no silent
/// fallback to 1).
fn revision_to_i64(revision: u64) -> Result<i64> {
    if revision == 0 || revision > i64::MAX as u64 {
        return Err(Error::Validation(format!(
            "source_revision {revision} outside 1..=i64::MAX"
        )));
    }
    Ok(revision as i64)
}

/// 退避秒数 `min(2^(retry_count-1), 60)`（§8.3；无随机抖动，便于可重复验收）。
/// retry_count<=0 时取 1 秒；移位上限 6 防 i64 溢出。
/// Backoff seconds `min(2^(retry_count-1), 60)` (§8.3; no jitter, deterministic
/// acceptance). retry_count<=0 yields 1s; the shift is capped at 6 to avoid i64
/// overflow.
fn backoff_seconds(retry_count: i64) -> i64 {
    if retry_count <= 1 {
        1
    } else {
        let shift = (retry_count - 1).min(6);
        (1i64 << shift).min(60)
    }
}

// ===== Step8 批 B3：死信审核与一致性诊断（§5.2/§6.3/D5/D6/D7）=====
// ===== Step8 batch B3: dead-letter review and consistency diagnostics
// (§5.2/§6.3/D5/D6/D7) =====

/// consistency_json / 死信 reason_json 的体积上限（§5.2：canonical JSON
/// ≤64 KiB；超限 fail-closed，不截断、不猜删）。
/// Size cap for consistency_json / dead-letter reason_json (§5.2: canonical JSON
/// ≤64 KiB; overruns fail closed — never truncated, never guessed away).
const MAX_CONSISTENCY_JSON_BYTES: usize = 64 * 1024;

/// findings 的诊断编码（§5.2 canonical 形状）：`key` 序列化为
/// `{entity_id,pointer}`（ClaimKey 全键，STEP8-019），`old_hash`=证据值摘要、
/// `new_hash`=候选值摘要（BLAKE3 hex）；原文不落任何诊断列。
/// The findings diagnostic encoding (§5.2 canonical shape): `key` serializes as
/// `{entity_id,pointer}` (the full ClaimKey, STEP8-019), `old_hash` = the
/// evidence value digest, `new_hash` = the candidate value digest (BLAKE3 hex);
/// raw values never reach any diagnostic column.
fn consistency_findings_json(report: &ConsistencyReport) -> Vec<serde_json::Value> {
    report
        .findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "key": {"entity_id": f.key.entity_id, "pointer": f.key.pointer},
                "old_hash": f.evidence_value_hash,
                "new_hash": f.candidate_value_hash,
            })
        })
        .collect()
}

/// Step8 §5.2 canonical 诊断/理由 JSON：`code` + 任务身份（task_id/epoch/
/// attempt_no/entity_id/desired_hash）+ findings；kernel `canonical_text` 序列化
/// （键序稳定紧凑，与 quality_json 同一 canonical 纪律）；超 64 KiB → Validation。
/// The Step8 §5.2 canonical diagnostic/reason JSON: `code` + task identity
/// (task_id/epoch/attempt_no/entity_id/desired_hash) + findings; serialized via
/// the kernel `canonical_text` (stable compact key order, the same canonical
/// discipline as quality_json); over 64 KiB → Validation.
fn consistency_diagnostic_json(
    code: &str,
    task_id: i64,
    epoch: i64,
    attempt_no: i64,
    entity_id: &str,
    desired_hash: &str,
    findings: Vec<serde_json::Value>,
) -> Result<String> {
    let text = canonical_text(&serde_json::json!({
        "code": code,
        "task_id": task_id,
        "epoch": epoch,
        "attempt_no": attempt_no,
        "entity_id": entity_id,
        "desired_hash": desired_hash,
        "findings": findings,
    }))?;
    if text.len() > MAX_CONSISTENCY_JSON_BYTES {
        return Err(Error::Validation(format!(
            "consistency diagnostic exceeds {MAX_CONSISTENCY_JSON_BYTES} bytes"
        )));
    }
    Ok(text)
}

/// 发布诊断（Step8 §5.2「数量」域）：`code=CONSISTENCY_SCORE`、任务身份、
/// score/compared_claims/candidate_count 与 findings 摘要（score=None → null，
/// 即「有仲裁、无可比较证据」）；与冲突诊断同一体积上限。
/// The publish diagnostic (Step8 §5.2 "counts" fields): `code=CONSISTENCY_SCORE`
/// with task identity, score/compared_claims/candidate_count and the findings
/// summary (score=None → null, i.e. "arbitrated but no comparable evidence");
/// the same size cap as the conflict diagnostic.
fn publish_consistency_json(
    report: &ConsistencyReport,
    task_id: i64,
    epoch: i64,
    attempt_no: i64,
    entity_id: &str,
    desired_hash: &str,
) -> Result<String> {
    let text = canonical_text(&serde_json::json!({
        "code": CONSISTENCY_SCORE,
        "task_id": task_id,
        "epoch": epoch,
        "attempt_no": attempt_no,
        "entity_id": entity_id,
        "desired_hash": desired_hash,
        "score": report.score,
        "compared_claims": report.compared_claims,
        "candidate_count": report.candidate_count,
        "findings": consistency_findings_json(report),
    }))?;
    if text.len() > MAX_CONSISTENCY_JSON_BYTES {
        return Err(Error::Validation(format!(
            "consistency diagnostic exceeds {MAX_CONSISTENCY_JSON_BYTES} bytes"
        )));
    }
    Ok(text)
}

/// 审核行插入主体（D6：死信与一致性冲突共享 review_queue；仅可在调用方已持有
/// 的 BEGIN IMMEDIATE 事务内调用）。`source_log_ids_json` 对编译审核固定为
/// `[]`（§5.2）；`ON CONFLICT(domain,action,subject_json) DO NOTHING` 是幂等
/// 保护（A10），返回实际插入行数（0 = 已存在被跳过）。
/// The review-row insert body (D6: dead letters and consistency conflicts share
/// review_queue; callable only inside a BEGIN IMMEDIATE transaction already held
/// by the caller). `source_log_ids_json` is fixed to `[]` for compile audits
/// (§5.2); `ON CONFLICT(domain,action,subject_json) DO NOTHING` is the
/// idempotency guard (A10); returns the rows actually inserted (0 = skipped as
/// existing).
fn insert_review_on_conn(
    tx: &mut SqliteConnection,
    domain: &str,
    action: &str,
    subject_json: &str,
    reason_json: &str,
    compile_task_id: Option<i64>,
    now: i64,
) -> Result<usize> {
    let affected = diesel::sql_query(
        "INSERT INTO review_queue
            (domain, action, source_log_ids_json, subject_json, reason_json, created_at,
             compile_task_id)
         VALUES (?, ?, '[]', ?, ?, ?, ?)
         ON CONFLICT (domain, action, subject_json) DO NOTHING",
    )
    .bind::<diesel::sql_types::Text, _>(domain)
    .bind::<diesel::sql_types::Text, _>(action)
    .bind::<diesel::sql_types::Text, _>(subject_json)
    .bind::<diesel::sql_types::Text, _>(reason_json)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(compile_task_id)
    .execute(tx)?;
    Ok(affected)
}

/// 死信入队（§6.3/D7 签名；仅可在调用方已持有的 BEGIN IMMEDIATE 事务内调用，
/// 不得自行取连接/开事务——§8 的质量终态与回收终态共用外层事务）。
/// Dead-letter enqueue (§6.3/D7 signature; callable only inside a BEGIN
/// IMMEDIATE transaction already held by the caller — it never takes the
/// connection or opens its own transaction, sharing the outer quality/recovery
/// terminal transaction of §8).
///
/// subject 固定 `{"task_id":N}`；UNIQUE(domain,action,subject_json) + DO NOTHING
/// 保证同 task 重复 finish/recover 只有一条（A10）；`compile_task_id` 回填
/// （D7）。`epoch` 仅用于插入成功时的结构化日志（subject/reason 已含身份）。
/// 返回实际插入行数（0 = 已存在被跳过；偏差 STEP8-025：spec §6.3 字面为
/// `Result<()>`，为 `RecoveryStats.review_inserted` 精确计数改为 `Result<usize>`，
/// 语义不变）。
/// The subject is fixed `{"task_id":N}`; UNIQUE(domain,action,subject_json) with
/// DO NOTHING keeps exactly one row across repeated finish/recover of the same
/// task (A10); `compile_task_id` is backfilled (D7). `epoch` only feeds the
/// structured log emitted on a fresh insert (subject/reason already carry the
/// identity). Returns the rows actually inserted (0 = skipped as existing;
/// deviation STEP8-025: spec §6.3 literally says `Result<()>`, widened to
/// `Result<usize>` for an exact `RecoveryStats.review_inserted` count, semantics
/// unchanged).
pub(super) fn enqueue_dead_letter_on_conn(
    tx: &mut SqliteConnection,
    task_id: i64,
    domain: &str,
    epoch: i64,
    reason_json: &str,
    now: i64,
) -> Result<usize> {
    let subject = canonical_text(&serde_json::json!({ "task_id": task_id }))?;
    let inserted = insert_review_on_conn(
        tx,
        domain,
        "compile_dead_letter",
        &subject,
        reason_json,
        Some(task_id),
        now,
    )?;
    if inserted == 1 {
        tracing::debug!(task_id, epoch, "compile dead letter enqueued");
    }
    Ok(inserted)
}

/// 从实体键解析 review 域（compile_tasks 无 domain 列；实体键
/// `domain:type:slug` 的首段即域，解析失败 → Validation fail-closed）。
/// Resolves the review domain from an entity key (compile_tasks has no domain
/// column; the leading segment of `domain:type:slug` is the domain; a parse
/// failure → Validation fail-closed).
fn review_domain_of(entity_id: &str) -> Result<String> {
    Ok(crate::types::EntityId::from_key(entity_id)?.domain)
}

/// 保守预留 B = system UTF8 bytes + input_json UTF8 bytes + 256 +
/// max_output_tokens（§8.4：版本化预算上界估算，非供应商账单）。
/// `pub(crate)`：executor 统计 `reserved_tokens` 需复用同一公式，禁止双实现漂移。
/// Conservative reservation B = system UTF8 bytes + input_json UTF8 bytes + 256
/// plus max_output_tokens (§8.4: a versioned budget upper-bound estimate, not a
/// provider bill). `pub(crate)`: the executor's `reserved_tokens` statistic
/// reuses the same formula — never fork a drifting duplicate.
pub(crate) fn estimate_budget_units(
    system: &str,
    input_json: &str,
    max_output_tokens: u32,
) -> Result<i64> {
    let total = (system.len() as i64)
        .checked_add(input_json.len() as i64)
        .and_then(|v| v.checked_add(256))
        .and_then(|v| v.checked_add(i64::from(max_output_tokens)))
        .ok_or_else(|| Error::Internal("budget estimate overflow".into()))?;
    if total <= 0 {
        return Err(Error::Internal("budget estimate must be positive".into()));
    }
    Ok(total)
}

/// 单级预算检查 `reserved + B <= limit`（checked arithmetic，§8.4）。
/// Single-level budget check `reserved + B <= limit` (checked arithmetic, §8.4).
fn fits(reserved: i64, b: i64, limit: i64) -> Result<bool> {
    if reserved < 0 || limit < 0 {
        return Err(Error::Internal(
            "budget columns must be non-negative".into(),
        ));
    }
    let total = reserved
        .checked_add(b)
        .ok_or_else(|| Error::Internal("budget addition overflow".into()))?;
    Ok(total <= limit)
}

/// 事实平面 CAS 写入（§7 admit 事务步骤 1 / [`crate::traits::EntityStore::upsert_facts`]
/// 复用同一实现）：
/// CAS 生效判断 —— 仅当本 revision 真正覆盖/新插入 facts 行（影响行数==1）时，
/// 才允许重写派生行 fact_refs，否则旧 revision 会绕过 CAS 污染 reflist。
/// CAS 是核心语义（excluded.source_revision > facts.source_revision），走 raw SQL
/// 逃生（diesel on_conflict do_update 的 WHERE 表达力不足）；本 SQL 是 CAS 唯一真相。
/// Fact-plane CAS write (§7 admit transaction item 1 / reused by
/// [`crate::traits::EntityStore::upsert_facts`]): the CAS effect check — only when
/// this revision truly overwrites/inserts a facts row (affected rows == 1) may the
/// derived fact_refs rows be rewritten, otherwise an older revision would bypass
/// CAS and pollute the reflist. CAS is core semantics
/// (excluded.source_revision > facts.source_revision) via the raw-SQL escape
/// hatch (diesel's on_conflict do_update WHERE is not expressive enough); this SQL
/// is the single source of truth for CAS.
pub(super) fn write_facts_cas(
    tx: &mut SqliteConnection,
    facts: &Facts,
    source_revision: u64,
    now: i64,
) -> Result<()> {
    let revision = revision_to_i64(source_revision)?;
    let entity_key = facts.entity_id.to_key();
    use crate::db_schema::fact_refs as fact_refs_t;
    for (field_name, value) in &facts.fields {
        let (field_type, numeric, text, boolean, timestamp) = facts::fact_columns(value);
        let applied = diesel::sql_query(
            "INSERT INTO facts (entity_id, field_name, field_type, value_numeric, value_text, value_boolean, value_timestamp, source_revision, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(entity_id, field_name) DO UPDATE SET
                 field_type      = excluded.field_type,
                 value_numeric   = excluded.value_numeric,
                 value_text      = excluded.value_text,
                 value_boolean   = excluded.value_boolean,
                 value_timestamp = excluded.value_timestamp,
                 source_revision = excluded.source_revision,
                 updated_at      = excluded.updated_at
             WHERE excluded.source_revision > facts.source_revision",
        )
        .bind::<diesel::sql_types::Text, _>(&entity_key)
        .bind::<diesel::sql_types::Text, _>(field_name)
        .bind::<diesel::sql_types::Text, _>(&field_type)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Double>, _>(numeric)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(text)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(boolean)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(timestamp)
        .bind::<diesel::sql_types::BigInt, _>(revision)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .execute(tx)?
            == 1;

        // reflist 拆行到 fact_refs（同一事务，先删后插；仅 CAS 生效时执行）。
        // Split reflist into fact_refs rows (same transaction, delete-then-insert;
        // only runs when CAS applied).
        if applied {
            if let FactValue::RefList(refs) = value {
                diesel::delete(
                    fact_refs_t::table
                        .filter(fact_refs_t::entity_id.eq(&entity_key))
                        .filter(fact_refs_t::field_name.eq(field_name)),
                )
                .execute(tx)?;
                for r in refs {
                    diesel::insert_into(fact_refs_t::table)
                        .values((
                            fact_refs_t::entity_id.eq(&entity_key),
                            fact_refs_t::field_name.eq(field_name),
                            fact_refs_t::ref_value.eq(r),
                        ))
                        .execute(tx)?;
                }
            }
        }
    }
    Ok(())
}

/// 兜底 legacy generation 哨兵（§8.1）：迁移只在「迁移时已有 legacy 页」时插入；
/// 迁移后才 seed 的库在首次发布前补插，保证 AUTOINCREMENT 不与 legacy
/// generation=1 碰撞。
/// Fallback legacy generation sentinel (§8.1): the migration only inserts it when
/// legacy pages already exist at migration time; databases seeded after the
/// migration get it before their first publish, so AUTOINCREMENT never collides
/// with the legacy generation=1.
fn ensure_generation_sentinel(tx: &mut SqliteConnection) -> Result<()> {
    diesel::sql_query(
        "INSERT INTO generations (generation, domain_pack, domain_pack_version, status, created_at)
         SELECT 1, '__legacy__', 'seed', 'published', 0
         WHERE NOT EXISTS (SELECT 1 FROM generations)
           AND EXISTS (SELECT 1 FROM pages WHERE generation = 1)",
    )
    .execute(tx)?;
    Ok(())
}

/// artifact_json = canonical(完整 CompiledPage)（§8.1：≤256 KiB）。
/// artifact_json = canonical(full CompiledPage) (§8.1: ≤256 KiB).
fn serialized_artifact(page: &CompiledPage) -> Result<String> {
    let json = canonical_text(page)?;
    if json.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::Compilation(format!(
            "artifact_json exceeds {MAX_ARTIFACT_BYTES} bytes (got {})",
            json.len()
        )));
    }
    Ok(json)
}

/// frontmatter JSON（§8.1：title/aliases/tags/完整 refs/质量策略；不混入正文）。
/// STEP5-001（additive）：补写 title——Synonym/Hyponym 需要锚点；title 不参与
/// content_hash/artifact_version 口径，故本次写入不改变任何既有 hash 语义。
/// Frontmatter JSON (§8.1: title/aliases/tags/full refs/quality policy; never
/// the body). STEP5-001 (additive): the title is now written — Synonym/Hyponym
/// edges need the anchor; the title takes no part in the content_hash /
/// artifact_version formulas, so this write changes no existing hash semantics.
fn build_frontmatter_json(page: &CompiledPage, deps: &StoredDependencies) -> Result<String> {
    let evidence: &CompileEvidence = page
        .evidence
        .as_ref()
        .ok_or_else(|| Error::Compilation("evidence required for publish".into()))?;
    let refs: Vec<&crate::compile::contract::SourceRef> = evidence
        .sections
        .iter()
        .flat_map(|s| s.refs.iter())
        .collect();
    let value = serde_json::json!({
        "title": page.wiki.title,
        "aliases": page.wiki.aliases,
        "tags": page.wiki.tags,
        "refs": refs,
        "quality_policy": {
            "scorer_version": deps.policy.scorer_version,
            "artifact_version": deps.policy.artifact_version,
            "quality_threshold": deps.context.quality_threshold,
            "require_source_refs": deps.context.require_source_refs,
            "min_coverage": deps.policy.min_coverage,
            "min_density": deps.policy.min_density,
        },
    });
    canonical_text(&value)
}

/// compile_source_heads upsert（§7 步骤 4）。单写事务（BEGIN IMMEDIATE）内的
/// 读-改-写天然原子，提交即 CAS；新 revision / 新配置 admission 即 fence 旧 worker。
/// compile_source_heads upsert (§7 item 4). Read-modify-write inside one write
/// transaction (BEGIN IMMEDIATE) is atomic by construction, so commit acts as the
/// CAS; a new revision / new config fences the old worker immediately at
/// admission.
// 内部事务辅助：参数按 head 行形状逐一绑定，收敛为结构体反而失真。
// Internal transaction helper: parameters bind the head-row shape one-to-one;
// collapsing them into a struct would blur that mapping.
#[allow(clippy::too_many_arguments)]
fn upsert_source_head(
    tx: &mut SqliteConnection,
    entity_key: &str,
    revision: i64,
    snapshot_hash: &str,
    desired_hash: &str,
    task_id: i64,
    epoch: i64,
    now: i64,
) -> Result<()> {
    diesel::sql_query(
        "INSERT INTO compile_source_heads
            (entity_id, source_revision, snapshot_hash, desired_hash, task_id, epoch, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (entity_id) DO UPDATE SET
            source_revision = excluded.source_revision,
            snapshot_hash   = excluded.snapshot_hash,
            desired_hash    = excluded.desired_hash,
            task_id         = excluded.task_id,
            epoch           = excluded.epoch,
            updated_at      = excluded.updated_at",
    )
    .bind::<diesel::sql_types::Text, _>(entity_key)
    .bind::<diesel::sql_types::BigInt, _>(revision)
    .bind::<diesel::sql_types::Text, _>(snapshot_hash)
    .bind::<diesel::sql_types::Text, _>(desired_hash)
    .bind::<diesel::sql_types::BigInt, _>(task_id)
    .bind::<diesel::sql_types::BigInt, _>(epoch)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(tx)?;
    Ok(())
}

/// skip 路径的任务记录（§7 步骤 2：输入 revision 更新但内容已接受 → 记
/// succeeded/skipped，保留已接受页及其原 revision 证据）。
/// Task record for the skip path (§7 item 2: newer input revision with identical
/// accepted content → record succeeded/skipped, keeping the accepted page and its
/// original revision evidence).
// 内部事务辅助：参数按 compile_tasks 插入行形状逐一绑定（见上条说明）。
// Internal transaction helper: parameters bind the compile_tasks insert-row shape
// one-to-one (see the note above).
#[allow(clippy::too_many_arguments)]
fn record_skipped_task(
    tx: &mut SqliteConnection,
    entity_key: &str,
    revision: i64,
    domain_pack_version: &str,
    desired_hash: &str,
    source_json: &str,
    deps_json: &str,
    snapshot_hash: &str,
    max_retries: i64,
    task_token_budget: i64,
    now: i64,
) -> Result<TaskEpochRow> {
    diesel::sql_query(
        "INSERT INTO compile_tasks
            (entity_id, source_revision, domain_pack_version, status, result, desired_hash, epoch,
             source_json, dependencies_json, snapshot_hash, max_retries, task_token_budget,
             created_at, updated_at)
         VALUES (?, ?, ?, 'succeeded', 'skipped', ?, 1, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (entity_id, source_revision, domain_pack_version) DO UPDATE SET
            status = 'succeeded',
            result = 'skipped',
            error_message = NULL,
            lease_token = NULL,
            lease_expires_at = 0,
            next_attempt_at = 0,
            updated_at = ?
         RETURNING task_id, epoch",
    )
    .bind::<diesel::sql_types::Text, _>(entity_key)
    .bind::<diesel::sql_types::BigInt, _>(revision)
    .bind::<diesel::sql_types::Text, _>(domain_pack_version)
    .bind::<diesel::sql_types::Text, _>(desired_hash)
    .bind::<diesel::sql_types::Text, _>(source_json)
    .bind::<diesel::sql_types::Text, _>(deps_json)
    .bind::<diesel::sql_types::Text, _>(snapshot_hash)
    .bind::<diesel::sql_types::BigInt, _>(max_retries)
    .bind::<diesel::sql_types::BigInt, _>(task_token_budget)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .get_result(tx)
    .map_err(Error::Database)
}

/// admit 事务主体（§7 步骤 1-4）。`pub(super)`：Step6 批4 approve_review 在
/// **同一个** BEGIN IMMEDIATE 内复用本主体（D12；diesel 2.3 嵌套 BEGIN 报
/// AlreadyInTransaction，故复用主体而非嵌套事务）；调用方必须已在
/// `immediate_transaction` 中，本函数不再自行开启事务。
/// The admit transaction body (§7 items 1-4). `pub(super)`: Step6 batch 4's
/// approve_review reuses this body inside the **same** BEGIN IMMEDIATE (D12;
/// diesel 2.3 rejects nested BEGINs with AlreadyInTransaction, so the body is
/// reused rather than nesting transactions); callers must already run inside
/// `immediate_transaction` — this function never opens its own.
pub(super) fn admit_compile_on_conn(
    conn: &mut SqliteConnection,
    prepared: &PreparedSource,
    ctx: &CompileContext,
    policy: &CompilePolicy,
    schema: &EntitySchema,
    force: bool,
) -> Result<Admission> {
    let entity_key = prepared.full.id.to_key();
    let revision = revision_to_i64(prepared.full.source_revision)?;
    let now = super::sqlite::unix_now();

    // —— §7 步骤 1：比较实体 source head ——
    // —— §7 item 1: compare the entity source head ——
    let head: Option<SourceHeadRow> = diesel::sql_query(
        "SELECT source_revision, snapshot_hash
         FROM compile_source_heads WHERE entity_id = ?",
    )
    .bind::<diesel::sql_types::Text, _>(&entity_key)
    .get_result(conn)
    .optional()?;

    if let Some(h) = &head {
        if revision < h.source_revision {
            // 低 revision → skipped(stale_source)：不写 facts、不排队（§7.1）。
            // Lower revision → skipped(stale_source): no facts write, no queueing.
            return Ok(Admission::Skipped);
        }
        if revision == h.source_revision && prepared.snapshot_hash != h.snapshot_hash {
            // 同 revision 不同 snapshot → rejected(source_revision_conflict)，
            // 不自动采信（§7.1/§13：force 也不能覆盖事实 CAS）。
            // Same revision with a different snapshot →
            // rejected(source_revision_conflict); never auto-trusted (§7.1/§13:
            // even force cannot override the fact CAS).
            return Ok(Admission::Rejected("source_revision_conflict".into()));
        }
        if revision > h.source_revision {
            // 高 revision：先以事实 CAS 写 facts（仅 CAS 成功才替换，§7.1/§8.2）。
            // Higher revision: write facts via CAS first (replacement only when
            // CAS applies, §7.1/§8.2).
            write_facts_cas(conn, &prepared.facts, prepared.full.source_revision, now)?;
        }
        // 同 revision 同 snapshot → 幂等重放，不重复写 facts。
        // Same revision and snapshot → idempotent replay, no facts rewrite.
    } else {
        write_facts_cas(conn, &prepared.facts, prepared.full.source_revision, now)?;
    }

    // desired_hash = content_hash(knowledge, ctx, policy, schema)（§7）。
    // desired_hash = content_hash(knowledge, ctx, policy, schema) (§7).
    let desired_hash = content_hash(HashDependencies {
        source: &prepared.knowledge,
        context: ctx,
        policy,
        source_schema: schema,
    })?;
    let deps = StoredDependencies {
        context: ctx.clone(),
        policy: policy.clone(),
        schema: schema_to_value(schema),
    };
    let deps_json = canonical_text(&deps)?;
    let source_json = canonical_text(&prepared.knowledge)?;

    // —— §7 步骤 2：desired_hash 与已接受页 hash/版本一致且非 force → skipped ——
    // —— §7 item 2: identical accepted-page hash/version without force → skipped ——
    let accepted: Option<AcceptedPageRow> = diesel::sql_query(
        "SELECT content_hash, artifact_version, source_revision FROM pages
         WHERE entity_id = ? AND status = 'accepted'
         ORDER BY generation DESC, updated_at DESC LIMIT 1",
    )
    .bind::<diesel::sql_types::Text, _>(&entity_key)
    .get_result(conn)
    .optional()?;

    if let Some(p) = &accepted {
        if !force && p.content_hash == desired_hash && p.artifact_version == policy.artifact_version
        {
            if revision > p.source_revision {
                // 输入 revision 更新：记录本任务 succeeded/skipped；已接受页、其原
                // revision 证据与 generation 均保持不变（§7.2）。
                // Newer input revision: record succeeded/skipped; the accepted page,
                // its original revision evidence and generation stay untouched.
                let row = record_skipped_task(
                    conn,
                    &entity_key,
                    revision,
                    &ctx.domain_pack_version,
                    &desired_hash,
                    &source_json,
                    &deps_json,
                    &prepared.snapshot_hash,
                    i64::from(policy.max_retries),
                    policy.task_token_budget as i64,
                    now,
                )?;
                upsert_source_head(
                    conn,
                    &entity_key,
                    revision,
                    &prepared.snapshot_hash,
                    &desired_hash,
                    row.task_id,
                    row.epoch,
                    now,
                )?;
            }
            return Ok(Admission::Skipped);
        }
    }

    // —— §7 步骤 3：三元 (entity_id, source_revision, domain_pack_version) 任务
    //    INSERT ON CONFLICT 语义 ——
    // —— §7 item 3: triple (entity_id, source_revision, domain_pack_version)
    //    INSERT ON CONFLICT semantics ——
    let existing: Option<ExistingTaskRow> = diesel::sql_query(
        "SELECT task_id, epoch, desired_hash, status FROM compile_tasks
         WHERE entity_id = ? AND source_revision = ? AND domain_pack_version = ?",
    )
    .bind::<diesel::sql_types::Text, _>(&entity_key)
    .bind::<diesel::sql_types::BigInt, _>(revision)
    .bind::<diesel::sql_types::Text, _>(&ctx.domain_pack_version)
    .get_result(conn)
    .optional()?;

    let (task_id, epoch) = match existing {
        Some(t) if t.desired_hash == desired_hash && !force => {
            if matches!(t.status.as_str(), "pending" | "running") {
                // 相同 hash 的 pending/running 合并：刷新快照但不重置计数、
                // 不 bump epoch（§7.3）。
                // Same-hash pending/running merge: refresh snapshots without
                // resetting counters or bumping the epoch (§7.3).
                diesel::sql_query(
                    "UPDATE compile_tasks
                     SET source_json = ?, dependencies_json = ?, snapshot_hash = ?, updated_at = ?
                     WHERE task_id = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&source_json)
                .bind::<diesel::sql_types::Text, _>(&deps_json)
                .bind::<diesel::sql_types::Text, _>(&prepared.snapshot_hash)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(t.task_id)
                .execute(conn)?;
                (t.task_id, t.epoch)
            } else {
                // 相同 hash 的 dead/failed/succeeded 不自动重启，计入现有 terminal
                // 结果（§7.3）。
                // Same-hash dead/failed/succeeded never auto-restarts; the existing
                // terminal result stands (§7.3).
                upsert_source_head(
                    conn,
                    &entity_key,
                    revision,
                    &prepared.snapshot_hash,
                    &desired_hash,
                    t.task_id,
                    t.epoch,
                    now,
                )?;
                return Ok(Admission::Skipped);
            }
        }
        Some(t) => {
            // desired hash 改变（或 force 同 hash）：epoch+1、替换知识与依赖快照、
            // 计数归零、重排 pending；旧 attempts 保留，旧 lease 因 epoch fence 失效；
            // reserved_tokens 保留（force 预算仍生效，§7.3/§8.3/§9）。
            // Desired hash changed (or force on same hash): epoch+1, replace
            // knowledge/dependency snapshots, reset counters, requeue as pending;
            // old attempts are kept, old leases are fenced by the new epoch, and
            // reserved_tokens stay (force keeps budgets effective,
            // §7.3/§8.3/§9).
            let row: TaskEpochRow = diesel::sql_query(
                "UPDATE compile_tasks SET
                    status = 'pending',
                    desired_hash = ?, epoch = epoch + 1,
                    source_json = ?, dependencies_json = ?, snapshot_hash = ?,
                    recompile_count = 0, attempt_count = 0, retry_count = 0,
                    result = NULL, next_attempt_at = 0,
                    lease_token = NULL, lease_expires_at = NULL, error_message = NULL,
                    max_retries = ?, task_token_budget = ?, updated_at = ?
                 WHERE task_id = ?
                 RETURNING task_id, epoch",
            )
            .bind::<diesel::sql_types::Text, _>(&desired_hash)
            .bind::<diesel::sql_types::Text, _>(&source_json)
            .bind::<diesel::sql_types::Text, _>(&deps_json)
            .bind::<diesel::sql_types::Text, _>(&prepared.snapshot_hash)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(policy.max_retries))
            .bind::<diesel::sql_types::BigInt, _>(policy.task_token_budget as i64)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(t.task_id)
            .get_result(conn)?;
            (row.task_id, row.epoch)
        }
        None => {
            // 新任务：status='pending'、desired_hash、epoch=1（§7.3）。
            // New task: status='pending', desired_hash, epoch=1 (§7.3).
            let row: TaskEpochRow = diesel::sql_query(
                "INSERT INTO compile_tasks
                    (entity_id, source_revision, domain_pack_version, status, desired_hash, epoch,
                     source_json, dependencies_json, snapshot_hash, max_retries, task_token_budget,
                     created_at, updated_at)
                 VALUES (?, ?, ?, 'pending', ?, 1, ?, ?, ?, ?, ?, ?, ?)
                 RETURNING task_id, epoch",
            )
            .bind::<diesel::sql_types::Text, _>(&entity_key)
            .bind::<diesel::sql_types::BigInt, _>(revision)
            .bind::<diesel::sql_types::Text, _>(&ctx.domain_pack_version)
            .bind::<diesel::sql_types::Text, _>(&desired_hash)
            .bind::<diesel::sql_types::Text, _>(&source_json)
            .bind::<diesel::sql_types::Text, _>(&deps_json)
            .bind::<diesel::sql_types::Text, _>(&prepared.snapshot_hash)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(policy.max_retries))
            .bind::<diesel::sql_types::BigInt, _>(policy.task_token_budget as i64)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .get_result(conn)?;
            (row.task_id, row.epoch)
        }
    };

    // —— §7 步骤 4：source_heads 保存 latest desired task_id/epoch/hash ——
    // —— §7 item 4: source_heads stores the latest desired task_id/epoch/hash ——
    upsert_source_head(
        conn,
        &entity_key,
        revision,
        &prepared.snapshot_hash,
        &desired_hash,
        task_id,
        epoch,
        now,
    )?;

    Ok(Admission::Queued(task_id))
}

/// claim 事务主体（§8.3/§8.4）。
/// The claim transaction body (§8.3/§8.4).
fn claim_in_transaction(
    tx: &mut SqliteConnection,
    task_ids: &[i64],
    run_id: &str,
    now: i64,
) -> Result<Option<TaskLease>> {
    // 参数化领取 SQL（§8.3）：只查当前 run 已 admission 的 task_ids（JSON 数组
    // 经内建 json_each 展开）、pending、due、retry_count<max_retries，按
    // (next_attempt_at, task_id) 取 1；领取不增加 retry_count。
    // Parameterized claim SQL (§8.3): only tasks admitted into this run (the JSON
    // array expanded by the built-in json_each), pending, due, with
    // retry_count<max_retries, one row ordered by (next_attempt_at, task_id);
    // claiming never increments retry_count.
    let task_ids_json = serde_json::to_string(task_ids)?;
    let Some(cand) = diesel::sql_query(crate::schema::tasks::SQL_CLAIM_NEXT)
        .bind::<diesel::sql_types::Text, _>(&task_ids_json)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .get_result::<ClaimCandidate>(tx)
        .optional()?
    else {
        return Ok(None);
    };

    // 任务快照还原：知识源 + 冻结依赖（context/policy/schema）。
    // Snapshot restore: knowledge source + frozen dependencies
    // (context/policy/schema).
    let source: RawEntity = serde_json::from_str(&cand.source_json)?;
    let deps: StoredDependencies = serde_json::from_str(&cand.dependencies_json)?;

    // —— 预算熔断（§8.4）：task/run/day 各 reserved+B<=limit。不足 → Ok(None)，
    //    任务保持 pending（§8.4：预算不足不是质量失败，由 executor 计 deferred）。
    // —— Budget circuit breaker (§8.4): task/run/day each need reserved+B<=limit.
    //    Insufficient → Ok(None); the task stays pending (not a quality failure;
    //    the executor counts it as deferred).
    let b = estimate_budget_units(
        &deps.context.prompt_template,
        &cand.source_json,
        deps.policy.max_output_tokens,
    )?;
    if !fits(cand.reserved_tokens, b, cand.task_token_budget)? {
        return Ok(None);
    }

    // run 行：不存在则按该任务冻结的 batch 预算插入；已存在保留其 limit。
    // Run row: insert with the task's frozen batch budget when missing; an
    // existing row keeps its own limit.
    diesel::sql_query(
        "INSERT INTO compile_runs (run_id, token_limit, reserved_tokens, created_at)
         VALUES (?, ?, 0, ?) ON CONFLICT (run_id) DO NOTHING",
    )
    .bind::<diesel::sql_types::Text, _>(run_id)
    .bind::<diesel::sql_types::BigInt, _>(deps.policy.batch_token_budget as i64)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(tx)?;
    let run_budget: BudgetRow =
        diesel::sql_query("SELECT token_limit, reserved_tokens FROM compile_runs WHERE run_id = ?")
            .bind::<diesel::sql_types::Text, _>(run_id)
            .get_result(tx)?;
    if !fits(run_budget.reserved_tokens, b, run_budget.token_limit)? {
        return Ok(None);
    }

    // 日预算：配置了才创建（冲突取当天已存 limit 与本次 limit 的最小值，§8.4）；
    // 省略日配置不能绕过已存在当天限额 → 有行则仍检查并预留。
    // Daily budget: only created when configured (on conflict the stored limit
    // meets the new one at their minimum, §8.4); omitting the daily config must
    // not bypass an existing day row, which is still checked and reserved.
    let utc_day = now.div_euclid(86_400);
    let day_row: Option<BudgetRow> = match deps.policy.daily_token_budget {
        Some(limit) => {
            diesel::sql_query(
                "INSERT INTO compile_daily_budget (utc_day, token_limit, reserved_tokens)
                 VALUES (?, ?, 0)
                 ON CONFLICT (utc_day) DO UPDATE SET token_limit = MIN(token_limit, excluded.token_limit)",
            )
            .bind::<diesel::sql_types::BigInt, _>(utc_day)
            .bind::<diesel::sql_types::BigInt, _>(limit as i64)
            .execute(tx)?;
            diesel::sql_query(
                "SELECT token_limit, reserved_tokens FROM compile_daily_budget WHERE utc_day = ?",
            )
            .bind::<diesel::sql_types::BigInt, _>(utc_day)
            .get_result(tx)
            .optional()?
        }
        None => diesel::sql_query(
            "SELECT token_limit, reserved_tokens FROM compile_daily_budget WHERE utc_day = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(utc_day)
        .get_result(tx)
        .optional()?,
    };
    if let Some(day) = &day_row {
        if !fits(day.reserved_tokens, b, day.token_limit)? {
            return Ok(None);
        }
    }

    // —— 预算同时预留（task/run/day；completed/crash 均不退还，§8.4）。
    // —— Reserve budgets simultaneously (task/run/day; never refunded on
    //    completion or crash, §8.4).
    diesel::sql_query(
        "UPDATE compile_tasks SET reserved_tokens = reserved_tokens + ? WHERE task_id = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(b)
    .bind::<diesel::sql_types::BigInt, _>(cand.task_id)
    .execute(tx)?;
    diesel::sql_query(
        "UPDATE compile_runs SET reserved_tokens = reserved_tokens + ? WHERE run_id = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(b)
    .bind::<diesel::sql_types::Text, _>(run_id)
    .execute(tx)?;
    if day_row.is_some() {
        diesel::sql_query("UPDATE compile_daily_budget SET reserved_tokens = reserved_tokens + ? WHERE utc_day = ?")
            .bind::<diesel::sql_types::BigInt, _>(b)
            .bind::<diesel::sql_types::BigInt, _>(utc_day)
            .execute(tx)?;
    }

    // reserved attempt 记账（含预留归属 run/utc_day，§8.1/§8.4）。
    // Reserved-attempt bookkeeping (records the run/utc_day of the reservation,
    // §8.1/§8.4).
    let lease_token = uuid::Uuid::new_v4().to_string();
    let attempt_no = cand
        .attempt_count
        .checked_add(1)
        .ok_or_else(|| Error::Internal("attempt_count overflow".into()))?;
    diesel::sql_query(
        "INSERT INTO compile_attempts
            (task_id, epoch, attempt_no, lease_token, status, run_id, utc_day, issues_json, reserved_tokens, created_at)
         VALUES (?, ?, ?, ?, 'reserved', ?, ?, '[]', ?, ?)",
    )
    .bind::<diesel::sql_types::BigInt, _>(cand.task_id)
    .bind::<diesel::sql_types::BigInt, _>(cand.epoch)
    .bind::<diesel::sql_types::BigInt, _>(attempt_no)
    .bind::<diesel::sql_types::Text, _>(&lease_token)
    .bind::<diesel::sql_types::Text, _>(run_id)
    .bind::<diesel::sql_types::BigInt, _>(utc_day)
    .bind::<diesel::sql_types::BigInt, _>(b)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(tx)?;

    // 最终 CAS：affected rows=1 才返回租约（§8.3）。0 行属不可能状态 → 报错回滚
    // （预算预留随之撤销）。claim 不增加 retry_count。
    // Final CAS: affected rows=1 yields the lease (§8.3). Zero rows is impossible
    // here → error to roll back (budget reservations included). Claim never
    // increments retry_count.
    let affected = diesel::sql_query(
        "UPDATE compile_tasks
         SET status = 'running', lease_expires_at = ?, lease_token = ?,
             attempt_count = attempt_count + 1
         WHERE task_id = ? AND status = 'pending' AND epoch = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(now + LEASE_WINDOW_SECONDS)
    .bind::<diesel::sql_types::Text, _>(&lease_token)
    .bind::<diesel::sql_types::BigInt, _>(cand.task_id)
    .bind::<diesel::sql_types::BigInt, _>(cand.epoch)
    .execute(tx)?;
    if affected != 1 {
        return Err(Error::Internal(format!(
            "claim lost task {} between select and update",
            cand.task_id
        )));
    }

    Ok(Some(TaskLease {
        task_id: cand.task_id,
        epoch: cand.epoch,
        lease_token,
        desired_hash: cand.desired_hash,
        attempt_no: u32::try_from(attempt_no)
            .map_err(|_| Error::Internal("attempt_no overflow".into()))?,
        source,
        context: deps.context,
    }))
}

/// publish 事务主体（§8.2 接受事务步骤 1-10；Step8 批 B3：consistency 为
/// `Some` 时在**同一事务**写 `page_quality.consistency` 与
/// `compile_attempts.consistency_json`（`None` → SQL NULL / `'{}'`，Step8
/// 「consistency 值与非 NULL consistency_json 在 publish 事务内写入」）。
/// The publish transaction body (§8.2 accept transaction items 1-10; Step8
/// batch B3: with a `Some` consistency the **same transaction** writes
/// `page_quality.consistency` and `compile_attempts.consistency_json`
/// (`None` → SQL NULL / `'{}'`, per Step8 "the consistency value and the
/// non-NULL consistency_json are written inside the publish transaction").
fn publish_in_transaction(
    tx: &mut SqliteConnection,
    lease: &TaskLease,
    page: &CompiledPage,
    report: &ScoreReport,
    consistency: Option<&ConsistencyReport>,
    now: i64,
) -> Result<CommitOutcome> {
    let task: Option<PublishTaskRow> = diesel::sql_query(
        "SELECT epoch, entity_id, source_revision, desired_hash, status, result,
                lease_token, lease_expires_at, dependencies_json
         FROM compile_tasks WHERE task_id = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
    .get_result(tx)
    .optional()?;

    // —— 幂等重放（§8.2）：该 task 已 succeeded/accepted 且本次 lease 与已完成的
    //    accepted attempt 匹配 → 返回既有 generation，不重复分配。
    // —— Idempotent replay (§8.2): the task already succeeded/accepted and this
    //    lease matches the completed accepted attempt → return the existing
    //    generation without allocating another.
    if let Some(t) = &task {
        if t.status == "succeeded" && t.result.as_deref() == Some("accepted") {
            let replay: Option<CountRow> = diesel::sql_query(
                "SELECT COUNT(*) AS n FROM compile_attempts
                 WHERE task_id = ? AND epoch = ? AND lease_token = ?
                   AND status = 'completed' AND publish_status = 'accepted'",
            )
            .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
            .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
            .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
            .get_result(tx)
            .optional()?;
            if replay.is_some_and(|c| c.n > 0) {
                let gen: Option<GenerationRow> = diesel::sql_query(
                    "SELECT generation FROM pages
                     WHERE entity_id = ? AND status = 'accepted'
                     ORDER BY generation DESC LIMIT 1",
                )
                .bind::<diesel::sql_types::Text, _>(&t.entity_id)
                .get_result(tx)
                .optional()?;
                return match gen {
                    Some(g) => Ok(CommitOutcome::Accepted {
                        generation: g.generation,
                    }),
                    None => Err(Error::Internal(format!(
                        "accepted attempt for task {} has no accepted page",
                        lease.task_id
                    ))),
                };
            }
        }
    }

    // —— 步骤 1：fencing 校验 tuple（task/epoch/token/status/未过期租约）。
    // —— Item 1: fencing tuple validation (task/epoch/token/status/unexpired lease).
    let Some(t) = &task else {
        return Ok(CommitOutcome::Stale);
    };
    let live = t.status == "running"
        && t.epoch == lease.epoch
        && t.lease_token.as_deref() == Some(lease.lease_token.as_str())
        && t.lease_expires_at.is_some_and(|e| e > now);
    if !live {
        return Ok(CommitOutcome::Stale);
    }

    // source head 未变：新 admission 在提交前即 fence 旧 worker（§8.2）。
    // Source head unchanged: a new admission fences the old worker before commit.
    let head: Option<TextOnlyRow> = diesel::sql_query(
        "SELECT desired_hash AS value FROM compile_source_heads WHERE entity_id = ?",
    )
    .bind::<diesel::sql_types::Text, _>(&t.entity_id)
    .get_result(tx)
    .optional()?;
    if head.as_ref().map(|h| h.value.as_str()) != Some(t.desired_hash.as_str()) {
        // head 已前进：本次 publish 放弃；任务由 recover 以 superseded 收尾。
        // The head moved on: this publish yields; recover finalizes the task as
        // superseded.
        return Ok(CommitOutcome::Stale);
    }

    // —— 步骤 2：Evidence=None 视为 schema 失败（§4）。
    // —— Item 2: Evidence=None counts as a schema failure (§4).
    if page.evidence.is_none() {
        return Err(Error::Compilation("evidence required for publish".into()));
    }

    // executor 契约：页实体与 content_hash 必须与任务一致（同一 hash 函数、同一
    // 输入重算；防止发布与任务身份脱钩）。
    // Executor contract: page entity and content_hash must match the task (the
    // same hash function over the same inputs; keeps publish identity-bound).
    if page.wiki.entity_id.to_key() != t.entity_id {
        return Err(Error::Compilation(format!(
            "page entity {} does not match task entity {}",
            page.wiki.entity_id.to_key(),
            t.entity_id
        )));
    }
    if page.content_hash != lease.desired_hash {
        return Err(Error::ContentHashMismatch {
            expected: lease.desired_hash.clone(),
            actual: page.content_hash.clone(),
        });
    }

    // artifact/quality 载荷先于任何写入计算（canonical、≤256 KiB，§8.1）。
    // artifact/quality payloads computed before any write (canonical, ≤256 KiB,
    // §8.1).
    let artifact_json = serialized_artifact(page)?;
    let quality_json = canonical_text(report)?;
    let deps: StoredDependencies = serde_json::from_str(&t.dependencies_json)?;
    let frontmatter_json = build_frontmatter_json(page, &deps)?;

    // 兜底 legacy 哨兵（§8.1），随后 generations building → 新 generation
    // （禁止重用 legacy 1）。
    // Fallback legacy sentinel (§8.1), then generations building → a new
    // generation (never reusing legacy 1).
    ensure_generation_sentinel(tx)?;
    diesel::sql_query(
        "INSERT INTO generations (domain_pack, domain_pack_version, status, created_at)
         VALUES (?, ?, 'building', ?)",
    )
    .bind::<diesel::sql_types::Text, _>(&page.wiki.entity_id.domain)
    .bind::<diesel::sql_types::Text, _>(&deps.context.domain_pack_version)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(tx)?;
    let generation = diesel::sql_query("SELECT last_insert_rowid() AS generation")
        .get_result::<GenerationRow>(tx)?
        .generation;

    // —— 步骤 4：upsert pages（accepted + 新 generation + frontmatter + 来源/产物
    //    版本；created_at 首插 now、upsert 保留原值）。
    // —— Item 4: upsert pages (accepted + new generation + frontmatter + source/
    //    artifact versions; created_at=now on first insert, preserved on update).
    let entity_key = page.wiki.entity_id.to_key();
    diesel::insert_into(pages_t::table)
        .values((
            pages_t::page_id.eq(&page.wiki.page_id),
            pages_t::entity_id.eq(&entity_key),
            pages_t::domain.eq(&page.wiki.entity_id.domain),
            pages_t::entity_type.eq(&page.wiki.entity_id.entity_type),
            pages_t::title.eq(&page.wiki.title),
            pages_t::content.eq(&page.wiki.content),
            pages_t::content_hash.eq(&page.content_hash),
            pages_t::generation.eq(generation),
            pages_t::status.eq("accepted"),
            pages_t::domain_pack_version.eq(&page.wiki.metadata.domain_pack_version),
            pages_t::compiled_at.eq(now),
            pages_t::model_version.eq(&page.wiki.metadata.model_version),
            pages_t::embedding_model.eq(&page.wiki.metadata.embedding_model),
            pages_t::source_revision.eq(t.source_revision),
            pages_t::artifact_version.eq(&deps.policy.artifact_version),
            pages_t::frontmatter_json.eq(&frontmatter_json),
            pages_t::created_at.eq(now),
            pages_t::updated_at.eq(now),
        ))
        .on_conflict(pages_t::page_id)
        .do_update()
        .set((
            pages_t::entity_id.eq(&entity_key),
            pages_t::domain.eq(&page.wiki.entity_id.domain),
            pages_t::entity_type.eq(&page.wiki.entity_id.entity_type),
            pages_t::title.eq(&page.wiki.title),
            pages_t::content.eq(&page.wiki.content),
            pages_t::content_hash.eq(&page.content_hash),
            pages_t::generation.eq(generation),
            pages_t::status.eq("accepted"),
            pages_t::domain_pack_version.eq(&page.wiki.metadata.domain_pack_version),
            pages_t::compiled_at.eq(now),
            pages_t::model_version.eq(&page.wiki.metadata.model_version),
            pages_t::embedding_model.eq(&page.wiki.metadata.embedding_model),
            pages_t::source_revision.eq(t.source_revision),
            pages_t::artifact_version.eq(&deps.policy.artifact_version),
            pages_t::frontmatter_json.eq(&frontmatter_json),
            pages_t::updated_at.eq(now),
        ))
        .execute(tx)?;

    // —— 步骤 5：删除并替换 page_sections（section_id = {page_id}#{index}）。
    // —— Item 5: delete-and-replace page_sections (section_id = {page_id}#{index}).
    diesel::delete(page_sections_t::table.filter(page_sections_t::page_id.eq(&page.wiki.page_id)))
        .execute(tx)?;
    for (i, s) in page.wiki.sections.iter().enumerate() {
        diesel::insert_into(page_sections_t::table)
            .values((
                page_sections_t::section_id.eq(format!("{}#{}", page.wiki.page_id, i)),
                page_sections_t::page_id.eq(&page.wiki.page_id),
                page_sections_t::heading.eq(&s.heading),
                page_sections_t::content.eq(&s.content),
                page_sections_t::section_index.eq(i as i64),
            ))
            .execute(tx)?;
    }

    // —— 步骤 6：upsert page_quality（四维实际值 + overall + 一致性列；Step8：
    //    一致性值以仲裁报告参数为权威源——score=None 或未仲裁 → SQL NULL，
    //    Some(s) → 精确落库 0/1/分数，A6；executor 管线中该值与
    //    report.quality.consistency 同源）。
    // —— Item 6: upsert page_quality (actual four dimensions + overall + the
    //    consistency column; Step8: the arbitration-report parameter is the
    //    authoritative source of the consistency value — score=None or
    //    un-arbitrated → SQL NULL, Some(s) → the exact 0/1/score, A6; in the
    //    executor pipeline it mirrors report.quality.consistency).
    let quality_values = (
        page_quality_t::coverage.eq(f64::from(report.quality.coverage)),
        page_quality_t::citation.eq(f64::from(report.quality.citation)),
        page_quality_t::schema_compliance.eq(f64::from(report.quality.schema_compliance)),
        page_quality_t::density.eq(f64::from(report.quality.density)),
        page_quality_t::consistency.eq(consistency.and_then(|c| c.score).map(f64::from)),
        page_quality_t::overall.eq(f64::from(report.quality.overall())),
    );
    diesel::insert_into(page_quality_t::table)
        .values((
            page_quality_t::page_id.eq(&page.wiki.page_id),
            quality_values,
        ))
        .on_conflict(page_quality_t::page_id)
        .do_update()
        .set(quality_values)
        .execute(tx)?;

    // —— 步骤 7：删除替换该页 qug_edges（本步默认空；非空按 canonical edge JSON
    //    的 BLAKE3 做 edge_hash，generation/hash 来自当前接受事务）。
    // —— Item 7: delete-and-replace the page's qug_edges (empty by default in
    //    this step; non-empty payloads hash canonical edge JSON with BLAKE3, with
    //    generation/hash from this accept transaction).
    diesel::delete(edges_t::table.filter(edges_t::page_id.eq(&page.wiki.page_id))).execute(tx)?;
    for edge in &page.qug_edges {
        let edge_json = canonical_text(edge)?;
        let edge_hash = blake3::hash(edge_json.as_bytes()).to_hex().to_string();
        diesel::insert_into(edges_t::table)
            .values((
                edges_t::page_id.eq(&page.wiki.page_id),
                edges_t::edge_hash.eq(&edge_hash),
                edges_t::edge_json.eq(&edge_json),
                edges_t::generation.eq(generation),
                edges_t::content_hash.eq(&page.content_hash),
            ))
            .on_conflict((edges_t::page_id, edges_t::edge_hash))
            .do_update()
            .set((
                edges_t::edge_json.eq(&edge_json),
                edges_t::generation.eq(generation),
                edges_t::content_hash.eq(&page.content_hash),
            ))
            .execute(tx)?;
    }

    // —— 步骤 8：FTS 由 trigger 同步（仅 accepted 插入，§8.1）——pages upsert 已
    //    在同事务内触发。
    // —— Item 8: FTS sync via triggers (accepted-only inserts, §8.1) — the pages
    //    upsert already fired them inside this transaction.

    // —— 步骤 9：generations published → attempt completed/accepted → task
    //    succeeded/accepted 清 lease。Step8：consistency Some 时同一事务写
    //    attempt 的 consistency_json（发布诊断：score/计数/findings 摘要；
    //    None → 保持 '{}' 默认）。
    // —— Item 9: generations published → attempt completed/accepted → task
    //    succeeded/accepted with lease cleared. Step8: with a Some consistency
    //    the attempt's consistency_json is written in the same transaction (the
    //    publish diagnostic: score/counts/findings summary; None keeps the
    //    '{}' default).
    let consistency_json = match consistency {
        Some(c) => publish_consistency_json(
            c,
            lease.task_id,
            lease.epoch,
            i64::from(lease.attempt_no),
            &t.entity_id,
            &t.desired_hash,
        )?,
        None => "{}".to_string(),
    };
    diesel::sql_query("UPDATE generations SET status = 'published' WHERE generation = ?")
        .bind::<diesel::sql_types::BigInt, _>(generation)
        .execute(tx)?;
    let affected = diesel::sql_query(
        "UPDATE compile_attempts
         SET status = 'completed', publish_status = 'accepted',
             artifact_json = ?, quality_json = ?, consistency_json = ?, finished_at = ?
         WHERE task_id = ? AND epoch = ? AND attempt_no = ? AND lease_token = ?",
    )
    .bind::<diesel::sql_types::Text, _>(&artifact_json)
    .bind::<diesel::sql_types::Text, _>(&quality_json)
    .bind::<diesel::sql_types::Text, _>(&consistency_json)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
    .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
    .bind::<diesel::sql_types::BigInt, _>(i64::from(lease.attempt_no))
    .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
    .execute(tx)?;
    if affected != 1 {
        return Err(Error::Internal(
            "publish: reserved attempt not found".into(),
        ));
    }
    let affected = diesel::sql_query(
        "UPDATE compile_tasks
         SET status = 'succeeded', result = 'accepted',
             lease_token = NULL, lease_expires_at = 0, updated_at = ?
         WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
    )
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
    .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
    .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
    .execute(tx)?;
    if affected != 1 {
        return Err(Error::Internal(
            "publish: task update lost the lease".into(),
        ));
    }

    // —— 步骤 10：commit 由 immediate_transaction 完成；任一步失败整事务回滚，
    //    generation/hash/task 不会提前成功（§8.2）。
    // —— Item 10: commit happens via immediate_transaction; any failure rolls the
    //    whole transaction back so generation/hash/task never succeed early (§8.2).
    Ok(CommitOutcome::Accepted { generation })
}

/// 失败处置事务主体（§8.3；Step8 批 B3：dead 终态与死信/一致性审核行同一
/// 事务——§8 质量终态原子性，任一步失败整体回滚）。
/// The failure-disposition transaction body (§8.3; Step8 batch B3: a dead
/// terminal and its dead-letter/consistency review rows share one transaction —
/// the §8 quality-terminal atomicity; any failure rolls everything back).
///
/// `consistency` 为本轮仲裁报告（executor 在 validate→score 之间产生）；仅当
/// 报告携带 VALUE_DIVERGENCE findings 时视为「该轮有一致性冲突」：attempt 写入
/// canonical 冲突诊断（consistency_json），刹车到顶的 dead 分支在同一事务插入
/// compile_dead_letter + consistency_conflict 各一条（D5）；无冲突数据时
/// consistency_json 保持 `'{}'`（§5.2「无可写数据时」）。
/// `consistency` is this round's arbitration report (produced by the executor
/// between validate and score); the round counts as "having a consistency
/// conflict" only when the report carries VALUE_DIVERGENCE findings: the attempt
/// stores the canonical conflict diagnostic (consistency_json), and the
/// brake-exhausted dead branch inserts one compile_dead_letter plus one
/// consistency_conflict in the same transaction (D5); without conflict data
/// consistency_json stays `'{}'` (§5.2 "no writable data").
fn finish_failure_in_transaction(
    tx: &mut SqliteConnection,
    lease: &TaskLease,
    failure: &CompileFailure,
    candidate: Option<&CompiledPage>,
    report: &ScoreReport,
    consistency: Option<&ConsistencyReport>,
    now: i64,
) -> Result<FailureDisposition> {
    let task: Option<FailureTaskRow> = diesel::sql_query(
        "SELECT entity_id, desired_hash, epoch, status, lease_token, lease_expires_at,
                retry_count, max_retries, recompile_count, dependencies_json
         FROM compile_tasks WHERE task_id = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
    .get_result(tx)
    .optional()?;

    let Some(t) = &task else {
        // fencing：0 行匹配 = 失去所有权（§8.3：只返回 stale，不改现任 worker 的
        // 状态/计数）。以 Ok(Failed) 表达，调用方仅计入失败统计。
        // Fencing: zero matching rows means ownership was lost (§8.3: report stale
        // only, never mutate the current owner's state/counters). Expressed as
        // Ok(Failed); the caller just counts a failure.
        return Ok(FailureDisposition::Failed);
    };
    let live = t.status == "running"
        && t.epoch == lease.epoch
        && t.lease_token.as_deref() == Some(lease.lease_token.as_str())
        && t.lease_expires_at.is_some_and(|e| e > now);
    if !live {
        return Ok(FailureDisposition::Failed);
    }
    let deps: StoredDependencies = serde_json::from_str(&t.dependencies_json)?;

    match failure {
        CompileFailure::InvalidOutput { code, .. } => {
            // —— 质量类失败：recompile_count+1（不占 retry_count）；attempt 置
            //    completed，候选存在则 publish_status='candidate' 并保存受限产物
            //    （§8.2 低质量事务：不动 pages/sections/quality/edges/generations）。
            // —— Quality-class failure: recompile_count+1 (retry_count untouched);
            //    the attempt completes with publish_status='candidate' plus the
            //    bounded artifact when a candidate exists (§8.2 low-quality
            //    transaction: pages/sections/quality/edges/generations untouched).
            let new_recompiles = t
                .recompile_count
                .checked_add(1)
                .ok_or_else(|| Error::Internal("recompile_count overflow".into()))?;
            let artifact_json = match candidate {
                Some(p) => Some(serialized_artifact(p)?),
                None => None,
            };
            let issues_json = canonical_text(&report.issues)?;
            let quality_json = canonical_text(report)?;
            // Step8 §5.2：该轮有 VALUE_DIVERGENCE findings 才生成冲突诊断，否则
            // 保持 '{}'（无可写数据）；attempt 与死信 reason 复用同一份。
            // Step8 §5.2: the conflict diagnostic is built only when this round
            // carries VALUE_DIVERGENCE findings, otherwise '{}' (no writable
            // data); the attempt and the dead-letter reason share one copy.
            let conflict = consistency.filter(|c| !c.findings.is_empty());
            let conflict_json = match conflict {
                Some(c) => Some(consistency_diagnostic_json(
                    CONSISTENCY_CONFLICT,
                    lease.task_id,
                    lease.epoch,
                    i64::from(lease.attempt_no),
                    &t.entity_id,
                    &t.desired_hash,
                    consistency_findings_json(c),
                )?),
                None => None,
            };
            let consistency_json = conflict_json.clone().unwrap_or_else(|| "{}".into());
            let affected = diesel::sql_query(
                "UPDATE compile_attempts
                 SET status = 'completed', publish_status = ?, artifact_json = ?,
                     quality_json = ?, issues_json = ?, error_code = ?, finished_at = ?,
                     consistency_json = ?
                 WHERE task_id = ? AND epoch = ? AND attempt_no = ? AND lease_token = ?",
            )
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                if candidate.is_some() {
                    Some("candidate")
                } else {
                    None
                },
            )
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(
                artifact_json.as_deref(),
            )
            .bind::<diesel::sql_types::Text, _>(&quality_json)
            .bind::<diesel::sql_types::Text, _>(&issues_json)
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::Text, _>(&consistency_json)
            .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
            .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(lease.attempt_no))
            .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
            .execute(tx)?;
            if affected != 1 {
                return Err(Error::Internal(
                    "failure: reserved attempt not found".into(),
                ));
            }
            if new_recompiles > i64::from(deps.policy.max_recompiles) {
                // 页级刹车到顶 → dead/quarantined 终态，进入人工队列（§8.3：
                // max_recompiles=0 首个差输出即 terminal）。
                // Page-level brake exhausted → dead/quarantined terminal, entering
                // the human queue (§8.3: max_recompiles=0 terminal on first bad
                // output).
                let affected = diesel::sql_query(
                    "UPDATE compile_tasks
                     SET status = 'dead', result = 'quarantined', recompile_count = ?,
                         lease_token = NULL, lease_expires_at = 0, error_message = ?, updated_at = ?
                     WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
                )
                .bind::<diesel::sql_types::BigInt, _>(new_recompiles)
                .bind::<diesel::sql_types::Text, _>(code)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
                .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
                .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
                .execute(tx)?;
                if affected != 1 {
                    return Err(Error::Internal(
                        "failure: task update lost the lease".into(),
                    ));
                }
                // —— Step8 D5/D7（A9/A10）：dead 终态与审核入队同一事务。死信恒
                //    插（有冲突时 reason 即冲突诊断，code=CONSISTENCY_CONFLICT；
                //    否则 code=失败码、findings 空）；consistency_conflict 行仅在
                //    该轮确有 VALUE_DIVERGENCE 冲突时插入。DO NOTHING 幂等保证
                //    重复 finish 不产生第二条（A10）。
                // —— Step8 D5/D7 (A9/A10): the dead terminal and the review
                //    inserts share one transaction. The dead letter always lands
                //    (reason = the conflict diagnostic with
                //    code=CONSISTENCY_CONFLICT when conflicting, otherwise the
                //    failure code with empty findings); the consistency_conflict
                //    row lands only when this round truly diverged. DO NOTHING
                //    idempotency keeps repeated finishes at one row (A10).
                let domain = review_domain_of(&t.entity_id)?;
                let reason = match &conflict_json {
                    Some(text) => text.clone(),
                    None => consistency_diagnostic_json(
                        code,
                        lease.task_id,
                        lease.epoch,
                        i64::from(lease.attempt_no),
                        &t.entity_id,
                        &t.desired_hash,
                        Vec::new(),
                    )?,
                };
                enqueue_dead_letter_on_conn(tx, lease.task_id, &domain, lease.epoch, &reason, now)?;
                if conflict.is_some() {
                    let subject = canonical_text(&serde_json::json!({ "task_id": lease.task_id }))?;
                    insert_review_on_conn(
                        tx,
                        &domain,
                        "consistency_conflict",
                        &subject,
                        &reason,
                        Some(lease.task_id),
                        now,
                    )?;
                }
                Ok(FailureDisposition::Quarantined)
            } else {
                // 仍有页级刹车空间 → pending/next_attempt_at=now+退避（§8.3）。
                // Page-level brake has room → pending with backoff (§8.3).
                let at = now + backoff_seconds(t.retry_count);
                let affected = diesel::sql_query(
                    "UPDATE compile_tasks
                     SET status = 'pending', recompile_count = ?, next_attempt_at = ?,
                         lease_token = NULL, lease_expires_at = 0, updated_at = ?
                     WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
                )
                .bind::<diesel::sql_types::BigInt, _>(new_recompiles)
                .bind::<diesel::sql_types::BigInt, _>(at)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
                .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
                .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
                .execute(tx)?;
                if affected != 1 {
                    return Err(Error::Internal(
                        "failure: task update lost the lease".into(),
                    ));
                }
                Ok(FailureDisposition::RetryAt(at))
            }
        }
        CompileFailure::Retryable {
            code,
            retry_after_seconds,
        } => {
            // —— 传输类失败：只加 retry_count，不加 recompile_count；attempt 置
            //    completed（结果已知）并保留 error_code（§8.3）。
            // —— Transport-class failure: retry_count+1 only, recompile_count
            //    untouched; the attempt completes (outcome known) with error_code
            //    (§8.3).
            let new_retry = t
                .retry_count
                .checked_add(1)
                .ok_or_else(|| Error::Internal("retry_count overflow".into()))?;
            let affected = diesel::sql_query(
                "UPDATE compile_attempts
                 SET status = 'completed', error_code = ?, finished_at = ?
                 WHERE task_id = ? AND epoch = ? AND attempt_no = ? AND lease_token = ?",
            )
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
            .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(lease.attempt_no))
            .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
            .execute(tx)?;
            if affected != 1 {
                return Err(Error::Internal(
                    "failure: reserved attempt not found".into(),
                ));
            }
            if new_retry >= t.max_retries {
                // 纯可重试传输耗尽 → dead/failed（§8.3：达到 max_retries 即停止）。
                // Purely retryable transport exhausted → dead/failed (§8.3: stop at
                // max_retries).
                let affected = diesel::sql_query(
                    "UPDATE compile_tasks
                     SET status = 'dead', result = 'failed', retry_count = ?,
                         lease_token = NULL, lease_expires_at = 0, error_message = ?, updated_at = ?
                     WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
                )
                .bind::<diesel::sql_types::BigInt, _>(new_retry)
                .bind::<diesel::sql_types::Text, _>(code)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
                .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
                .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
                .execute(tx)?;
                if affected != 1 {
                    return Err(Error::Internal(
                        "failure: task update lost the lease".into(),
                    ));
                }
                // —— Step8 D7（A10）：传输耗尽 dead 与死信入队同一事务（仅死信
                //    一条；无一致性冲突行——该路径从未仲裁）。
                // —— Step8 D7 (A10): the transport-exhausted dead terminal and
                //    its dead letter share one transaction (a single dead letter
                //    and no consistency row — this path never arbitrated).
                let domain = review_domain_of(&t.entity_id)?;
                let reason = consistency_diagnostic_json(
                    code,
                    lease.task_id,
                    lease.epoch,
                    i64::from(lease.attempt_no),
                    &t.entity_id,
                    &t.desired_hash,
                    Vec::new(),
                )?;
                enqueue_dead_letter_on_conn(tx, lease.task_id, &domain, lease.epoch, &reason, now)?;
                Ok(FailureDisposition::Failed)
            } else {
                // Retry-After 钳制 0..=300 秒（§4）；无值用 min(2^(retry-1),60)（§8.3）。
                // Retry-After clamped to 0..=300s (§4); otherwise
                // min(2^(retry-1),60) (§8.3).
                let delay = match retry_after_seconds {
                    Some(s) => (i64::from(*s)).clamp(0, 300),
                    None => backoff_seconds(new_retry),
                };
                let at = now + delay;
                let affected = diesel::sql_query(
                    "UPDATE compile_tasks
                     SET status = 'pending', retry_count = ?, next_attempt_at = ?,
                         lease_token = NULL, lease_expires_at = 0, updated_at = ?
                     WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
                )
                .bind::<diesel::sql_types::BigInt, _>(new_retry)
                .bind::<diesel::sql_types::BigInt, _>(at)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
                .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
                .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
                .execute(tx)?;
                if affected != 1 {
                    return Err(Error::Internal(
                        "failure: task update lost the lease".into(),
                    ));
                }
                Ok(FailureDisposition::RetryAt(at))
            }
        }
        CompileFailure::Permanent { code } => {
            // —— 永久传输错误 → failed/failed 终态（§8.3：不伪装为低质量）。
            // —— Permanent transport error → failed/failed terminal (§8.3: never
            //    disguised as a low-quality candidate).
            let affected = diesel::sql_query(
                "UPDATE compile_attempts
                 SET status = 'completed', error_code = ?, finished_at = ?
                 WHERE task_id = ? AND epoch = ? AND attempt_no = ? AND lease_token = ?",
            )
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
            .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(lease.attempt_no))
            .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
            .execute(tx)?;
            if affected != 1 {
                return Err(Error::Internal(
                    "failure: reserved attempt not found".into(),
                ));
            }
            let affected = diesel::sql_query(
                "UPDATE compile_tasks
                 SET status = 'failed', result = 'failed',
                     lease_token = NULL, lease_expires_at = 0, error_message = ?, updated_at = ?
                 WHERE task_id = ? AND epoch = ? AND lease_token = ? AND status = 'running'",
            )
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
            .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
            .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
            .execute(tx)?;
            if affected != 1 {
                return Err(Error::Internal(
                    "failure: task update lost the lease".into(),
                ));
            }
            Ok(FailureDisposition::Failed)
        }
    }
}

/// 按单行 pages 记录组装完整 CompiledPage（§8.1）：sections/qug_edges 逐页读取，
/// evidence 从最新 accepted attempt 的 artifact_json 还原。quality 由调用方按各自
/// 读取形状传入（load_accepted_pages 读四维、top_k_related_pages 额外读
/// consistency 列）。
/// Assembles a full CompiledPage from one pages row (§8.1): sections/qug_edges
/// are read per page and evidence is restored from the newest accepted attempt's
/// artifact_json. The caller supplies quality per its own read shape
/// (load_accepted_pages reads the four dimensions; top_k_related_pages
/// additionally reads the consistency column).
fn assemble_compiled_page(
    tx: &mut SqliteConnection,
    row: AcceptedPageFullRow,
    quality: QualityScore,
) -> Result<CompiledPage> {
    let entity_id = crate::types::EntityId::from_key(&row.entity_id)?;
    let frontmatter: StoredFrontmatter =
        serde_json::from_str(&row.frontmatter_json).unwrap_or_default();
    let sections: Vec<SectionRow> = diesel::sql_query(
        "SELECT heading, content FROM page_sections
         WHERE page_id = ? ORDER BY section_index",
    )
    .bind::<diesel::sql_types::Text, _>(&row.page_id)
    .load(tx)?;
    let edges: Vec<EdgeJsonRow> =
        diesel::sql_query("SELECT edge_json FROM qug_edges WHERE page_id = ? ORDER BY edge_hash")
            .bind::<diesel::sql_types::Text, _>(&row.page_id)
            .load(tx)?;
    let qug_edges: Vec<QugEdge> = edges
        .iter()
        .map(|e| serde_json::from_str(&e.edge_json))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let evidence = load_page_evidence(
        tx,
        &row.entity_id,
        row.source_revision,
        &row.domain_pack_version,
    )?;
    Ok(CompiledPage {
        wiki: WikiPage {
            page_id: row.page_id,
            entity_id,
            title: row.title,
            content: row.content,
            sections: sections
                .into_iter()
                .map(|s| Section {
                    heading: s.heading,
                    content: s.content,
                })
                .collect(),
            metadata: PageMetadata {
                domain_pack_version: row.domain_pack_version,
                compiled_at: row.compiled_at,
                model_version: row.model_version,
                embedding_model: row.embedding_model,
            },
            aliases: frontmatter.aliases,
            tags: frontmatter.tags,
        },
        quality,
        qug_edges,
        content_hash: row.content_hash,
        evidence,
    })
}

/// 从最新 accepted attempt 的 artifact_json 还原 evidence（§8.1：pages 只存
/// frontmatter，完整载荷在 attempts）。损坏载荷不阻塞读取（返回 None，
/// 不伪造证据——§10：旧 seed 页不能伪造回填）。
/// Restores evidence from the newest accepted attempt's artifact_json (§8.1: pages
/// store only frontmatter, the full payload lives in attempts). A corrupted
/// payload never blocks reads (returns None; evidence is never fabricated — §10:
/// legacy seed pages cannot be fabricated back).
fn load_page_evidence(
    tx: &mut SqliteConnection,
    entity_key: &str,
    source_revision: i64,
    domain_pack_version: &str,
) -> Result<Option<CompileEvidence>> {
    let task: Option<TaskIdRow> = diesel::sql_query(
        "SELECT task_id FROM compile_tasks
         WHERE entity_id = ? AND source_revision = ? AND domain_pack_version = ?",
    )
    .bind::<diesel::sql_types::Text, _>(entity_key)
    .bind::<diesel::sql_types::BigInt, _>(source_revision)
    .bind::<diesel::sql_types::Text, _>(domain_pack_version)
    .get_result(tx)
    .optional()?;
    let Some(task) = task else {
        return Ok(None);
    };
    let artifact: Option<ArtifactJsonRow> = diesel::sql_query(
        "SELECT artifact_json FROM compile_attempts
         WHERE task_id = ? AND status = 'completed' AND publish_status = 'accepted'
           AND artifact_json IS NOT NULL
         ORDER BY epoch DESC, attempt_no DESC LIMIT 1",
    )
    .bind::<diesel::sql_types::BigInt, _>(task.task_id)
    .get_result(tx)
    .optional()?;
    let Some(artifact) = artifact else {
        return Ok(None);
    };
    Ok(
        serde_json::from_str::<CompiledPage>(&artifact.artifact_json)
            .ok()
            .and_then(|p| p.evidence),
    )
}

impl SqliteKernel {
    /// admission 事务（§7/§8.2）：事实 CAS → desired_hash 比对 → 三元任务
    /// upsert → source head 更新，单次 BEGIN IMMEDIATE 提交。
    /// The admission transaction (§7/§8.2): fact CAS → desired_hash comparison →
    /// triple task upsert → source head update, committed in one BEGIN IMMEDIATE.
    ///
    /// 偏差说明（§8.2 签名）：spec 签名不含 `source_schema`，但 content_hash（§7）
    /// 与 dependencies_json 的 knowledge_schema 域都需要冻结的源 schema（executor
    /// 持有它），故显式传入；语义与其余签名一致。
    /// Deviation note (§8.2 signature): the spec signature has no
    /// `source_schema`, but content_hash (§7) and the dependencies_json
    /// knowledge_schema domain both need the frozen source schema (held by the
    /// executor), so it is passed explicitly; semantics are otherwise identical.
    pub fn admit_compile(
        &self,
        prepared: &PreparedSource,
        ctx: &CompileContext,
        policy: &CompilePolicy,
        schema: &EntitySchema,
        force: bool,
    ) -> Result<Admission> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            admit_compile_on_conn(tx, prepared, ctx, policy, schema, force)
        })
    }

    /// 领取事务（§8.3/§8.4）：在当前 run 已 admission 的任务中按
    /// `(next_attempt_at, task_id)` 领取 1 个，重查预算并预留，生成 UUID
    /// lease_token，attempt_count+1，插 reserved attempt，CAS 置 running。
    /// The claim transaction (§8.3/§8.4): picks one task among this run's admitted
    /// ids ordered by `(next_attempt_at, task_id)`, re-checks and reserves
    /// budgets, generates a UUID lease_token, increments attempt_count, inserts a
    /// reserved attempt, and CASes the task to running.
    pub fn claim_compile(
        &self,
        task_ids: &[i64],
        run_id: &str,
        now: i64,
    ) -> Result<Option<TaskLease>> {
        if task_ids.is_empty() {
            return Ok(None);
        }
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| claim_in_transaction(tx, task_ids, run_id, now))
    }

    /// 心跳保活（§8.3）：WHERE task_id/epoch/token/status=running 且
    /// lease_expires_at>now；affected=0（stale）返回 false 且不改任何状态。
    /// Heartbeat (§8.3): WHERE task_id/epoch/token/status=running and
    /// lease_expires_at>now; zero affected rows (stale) returns false and mutates
    /// nothing.
    pub fn heartbeat_compile(&self, lease: &TaskLease, now: i64) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        let affected = diesel::sql_query(
            "UPDATE compile_tasks SET lease_expires_at = ?
             WHERE task_id = ? AND epoch = ? AND lease_token = ?
               AND status = 'running' AND lease_expires_at > ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(now + LEASE_WINDOW_SECONDS)
        .bind::<diesel::sql_types::BigInt, _>(lease.task_id)
        .bind::<diesel::sql_types::BigInt, _>(lease.epoch)
        .bind::<diesel::sql_types::Text, _>(&lease.lease_token)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .execute(&mut *conn)?;
        Ok(affected == 1)
    }

    /// 过期租约回收（§8.3；Step8 §6.3/D8 返回结构化 [`RecoveryStats`]）：扫描
    /// `running AND lease_expires_at<=now`，事务内以旧 token CAS 将 reserved
    /// attempt 置 abandoned、retry_count+1（保留全部 token 预留）、清 lease；
    /// 新 source head → succeeded/superseded；超限 → dead/failed 且同一事务死信
    /// 入队（Step8 D7/A10）；否则 pending/退避。重复回收不重复计数、不重复插行。
    /// 五分支计数（recovered_pending/dead_failed/dead_quarantined/superseded/
    /// review_inserted）+ 扫描数随 [`RecoveryStats`] 返回。
    /// Expired-lease recovery (§8.3; Step8 §6.3/D8 returns the structured
    /// [`RecoveryStats`]): scans `running AND lease_expires_at<=now`, then per
    /// task inside one transaction: the reserved attempt is abandoned via
    /// old-token CAS, retry_count+1 (all token reservations kept), lease cleared;
    /// a moved source head → succeeded/superseded; retries exhausted → dead/failed
    /// with a dead letter enqueued in the same transaction (Step8 D7/A10);
    /// otherwise pending with backoff. Repeated recovery never double-counts nor
    /// double-inserts. The five per-branch counters (recovered_pending/
    /// dead_failed/dead_quarantined/superseded/review_inserted) plus the scan
    /// count are returned via [`RecoveryStats`].
    pub fn recover_compile_leases(&self, now: i64) -> Result<RecoveryStats> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let rows: Vec<ExpiredLeaseRow> = diesel::sql_query(
                "SELECT task_id, epoch, entity_id, desired_hash, lease_token, retry_count,
                        max_retries, attempt_count
                 FROM compile_tasks
                 WHERE status = 'running'
                   AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?",
            )
            .bind::<diesel::sql_types::BigInt, _>(now)
            .load(tx)?;
            let mut stats = RecoveryStats::default();
            for t in rows {
                stats.scanned += 1;
                // reserved attempt → abandoned（保留 reserved_tokens，不退还，§8.3）。
                // Reserved attempt → abandoned (reserved_tokens kept, §8.3).
                diesel::sql_query(
                    "UPDATE compile_attempts SET status = 'abandoned', finished_at = ?
                     WHERE task_id = ? AND epoch = ? AND attempt_no = ? AND lease_token = ?
                       AND status = 'reserved'",
                )
                .bind::<diesel::sql_types::BigInt, _>(now)
                .bind::<diesel::sql_types::BigInt, _>(t.task_id)
                .bind::<diesel::sql_types::BigInt, _>(t.epoch)
                .bind::<diesel::sql_types::BigInt, _>(t.attempt_count)
                .bind::<diesel::sql_types::Text, _>(&t.lease_token)
                .execute(tx)?;

                // 新 source head 使任务过时 → succeeded/superseded，不发布（§8.3）。
                // A newer source head makes the task stale → succeeded/superseded,
                // nothing published (§8.3).
                let head: Option<TextOnlyRow> = diesel::sql_query(
                    "SELECT desired_hash AS value FROM compile_source_heads WHERE entity_id = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&t.entity_id)
                .get_result(tx)
                .optional()?;
                let superseded = matches!(&head, Some(h) if h.value != t.desired_hash);
                let new_retry = t
                    .retry_count
                    .checked_add(1)
                    .ok_or_else(|| Error::Internal("retry_count overflow".into()))?;
                // 本行 dead 分支实际插入的审核行数（仅 dead 分支入队，Step8 D7）。
                // Review rows actually inserted by this row's dead branch (only the
                // dead branch enqueues, Step8 D7).
                let mut review_rows = 0u64;
                let affected = if superseded {
                    diesel::sql_query(
                        "UPDATE compile_tasks
                         SET status = 'succeeded', result = 'superseded',
                             lease_token = NULL, lease_expires_at = 0, updated_at = ?
                         WHERE task_id = ? AND lease_token = ? AND status = 'running'",
                    )
                    .bind::<diesel::sql_types::BigInt, _>(now)
                    .bind::<diesel::sql_types::BigInt, _>(t.task_id)
                    .bind::<diesel::sql_types::Text, _>(&t.lease_token)
                    .execute(tx)?
                } else if new_retry >= t.max_retries {
                    let affected = diesel::sql_query(
                        "UPDATE compile_tasks
                         SET status = 'dead', result = 'failed', retry_count = ?,
                             lease_token = NULL, lease_expires_at = 0,
                             error_message = 'lease_expired_retry_exhausted', updated_at = ?
                         WHERE task_id = ? AND lease_token = ? AND status = 'running'",
                    )
                    .bind::<diesel::sql_types::BigInt, _>(new_retry)
                    .bind::<diesel::sql_types::BigInt, _>(now)
                    .bind::<diesel::sql_types::BigInt, _>(t.task_id)
                    .bind::<diesel::sql_types::Text, _>(&t.lease_token)
                    .execute(tx)?;
                    // —— Step8 D7（A10）：回收 dead 终态与死信入队同一事务
                    //    （§8 回收终态原子性）；DO NOTHING 保证重复回收不重复
                    //    插行。attempt_no 记被弃的预留 attempt。
                    // —— Step8 D7 (A10): the recovery dead terminal and its dead
                    //    letter share one transaction (the §8 recovery-terminal
                    //    atomicity); DO NOTHING keeps repeated recoveries at one
                    //    row. attempt_no records the abandoned reserved attempt.
                    if affected == 1 {
                        let domain = review_domain_of(&t.entity_id)?;
                        let reason = consistency_diagnostic_json(
                            "LEASE_EXPIRED_RETRY_EXHAUSTED",
                            t.task_id,
                            t.epoch,
                            t.attempt_count,
                            &t.entity_id,
                            &t.desired_hash,
                            Vec::new(),
                        )?;
                        review_rows += enqueue_dead_letter_on_conn(
                            tx, t.task_id, &domain, t.epoch, &reason, now,
                        )? as u64;
                    }
                    affected
                } else {
                    let at = now + backoff_seconds(new_retry);
                    diesel::sql_query(
                        "UPDATE compile_tasks
                         SET status = 'pending', retry_count = ?, next_attempt_at = ?,
                             lease_token = NULL, lease_expires_at = 0, updated_at = ?
                         WHERE task_id = ? AND lease_token = ? AND status = 'running'",
                    )
                    .bind::<diesel::sql_types::BigInt, _>(new_retry)
                    .bind::<diesel::sql_types::BigInt, _>(at)
                    .bind::<diesel::sql_types::BigInt, _>(now)
                    .bind::<diesel::sql_types::BigInt, _>(t.task_id)
                    .bind::<diesel::sql_types::Text, _>(&t.lease_token)
                    .execute(tx)?
                };
                // WHERE lease_token=旧 token：重复回收不重复计数（§8.3）；Step8
                // §6.3 按分支落 RecoveryStats 计数。
                // WHERE lease_token=<old token>: repeated recovery never
                // double-counts (§8.3); Step8 §6.3 lands the per-branch
                // RecoveryStats counters.
                if affected == 1 {
                    if superseded {
                        stats.superseded += 1;
                    } else if new_retry >= t.max_retries {
                        stats.dead_failed += 1;
                        stats.review_inserted += review_rows;
                    } else {
                        stats.recovered_pending += 1;
                    }
                }
            }
            Ok(stats)
        })
    }

    /// 接受事务（§8.2 步骤 1-10）：校验 tuple 与 head → 插 generations building →
    /// upsert pages/sections/quality/edges（FTS 由 trigger 同步）→ published →
    /// attempt completed/accepted → task succeeded/accepted；任一步失败整事务
    /// 回滚；幂等重放返回既有 generation。Step8 批 B3：`consistency` 为 Some 时
    /// 同一事务写 `page_quality.consistency` 与 attempt 的 `consistency_json`。
    /// The accept transaction (§8.2 items 1-10): validate tuple and head → insert
    /// a building generation → upsert pages/sections/quality/edges (FTS synced by
    /// triggers) → published → attempt completed/accepted → task succeeded/
    /// accepted; any failure rolls everything back; idempotent replay returns the
    /// existing generation. Step8 batch B3: with a `Some` consistency the same
    /// transaction writes `page_quality.consistency` and the attempt's
    /// `consistency_json`.
    pub fn publish_compile(
        &self,
        lease: &TaskLease,
        page: &CompiledPage,
        report: &ScoreReport,
        consistency: Option<&ConsistencyReport>,
        now: i64,
    ) -> Result<CommitOutcome> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            publish_in_transaction(tx, lease, page, report, consistency, now)
        })
    }

    /// 失败处置事务（§8.3；Step8 批 B3）：质量类消耗 recompile_count、传输类消耗
    /// retry_count、永久错误直接终态；所有更新绑定 lease tuple（fencing），不动
    /// 发布面。dead 终态（页级刹车/传输耗尽）在同一事务死信入队（D7），刹车到顶
    /// 且本轮有 VALUE_DIVERGENCE 冲突时另插一条 consistency_conflict（D5）；
    /// attempt 按 §5.2 写 consistency_json（无冲突数据保持 '{}'）。
    /// The failure-disposition transaction (§8.3; Step8 batch B3): quality
    /// failures consume recompile_count, transport failures retry_count,
    /// permanent errors terminate; every update binds the lease tuple (fencing)
    /// and never touches the publish surface. A dead terminal (page brake /
    /// transport exhaustion) enqueues its dead letter in the same transaction
    /// (D7), and a brake exhaustion whose round truly diverged additionally
    /// inserts one consistency_conflict row (D5); the attempt stores
    /// consistency_json per §5.2 (staying '{}' without conflict data).
    pub fn finish_compile_failure(
        &self,
        lease: &TaskLease,
        failure: &CompileFailure,
        candidate: Option<&CompiledPage>,
        report: &ScoreReport,
        consistency: Option<&ConsistencyReport>,
        now: i64,
    ) -> Result<FailureDisposition> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            finish_failure_in_transaction(tx, lease, failure, candidate, report, consistency, now)
        })
    }

    /// preflight 隔离（§8.3：空知识/输入超限，无 LLM、不占 token）。
    ///
    /// 偏差说明：spec §8.3 将 preflight quarantine 记在 finish_compile_failure 名
    /// 下，但 §4 的 CompileFailure 枚举固定为三变体（传输/永久/低质），preflight
    /// 发生在模型请求前（通常领取前、无 lease），无法用该枚举表达，故独立成方法；
    /// 行为与 spec 一致：合成 completed attempt_no=0、不占 token、任务
    /// dead/quarantined。task 已终结时返回 Failed（幂等重放）。
    /// Preflight quarantine (§8.3: empty knowledge / oversize input; no LLM, no
    /// tokens).
    ///
    /// Deviation note: spec §8.3 lists preflight quarantine under
    /// finish_compile_failure, but the §4 CompileFailure enum is fixed to three
    /// variants (transport/permanent/low-quality) and preflight happens before any
    /// model request (usually pre-claim, without a lease), so it cannot be
    /// expressed there; this dedicated method keeps the spec behavior: a synthetic
    /// completed attempt_no=0, no tokens, task dead/quarantined. Returns Failed
    /// when the task is already terminal (idempotent replay).
    pub fn quarantine_compile_preflight(
        &self,
        task_id: i64,
        epoch: i64,
        code: &str,
        now: i64,
    ) -> Result<FailureDisposition> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let lease_token = uuid::Uuid::new_v4().to_string();
            // 合成 attempt_no=0 记录（不占 token；与领取 attempt_no>=1 无碰撞）。
            // Synthetic attempt_no=0 record (no tokens; never collides with
            // claimed attempt_no>=1).
            diesel::sql_query(
                "INSERT INTO compile_attempts
                    (task_id, epoch, attempt_no, lease_token, status, publish_status,
                     issues_json, reserved_tokens, error_code, created_at, finished_at)
                 VALUES (?, ?, 0, ?, 'completed', 'quarantined', '[]', 0, ?, ?, ?)
                 ON CONFLICT (task_id, epoch, attempt_no) DO NOTHING",
            )
            .bind::<diesel::sql_types::BigInt, _>(task_id)
            .bind::<diesel::sql_types::BigInt, _>(epoch)
            .bind::<diesel::sql_types::Text, _>(&lease_token)
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .execute(tx)?;
            let affected = diesel::sql_query(
                "UPDATE compile_tasks
                 SET status = 'dead', result = 'quarantined',
                     lease_token = NULL, lease_expires_at = 0, error_message = ?, updated_at = ?
                 WHERE task_id = ? AND epoch = ? AND status IN ('pending', 'running')",
            )
            .bind::<diesel::sql_types::Text, _>(code)
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::BigInt, _>(task_id)
            .bind::<diesel::sql_types::BigInt, _>(epoch)
            .execute(tx)?;
            if affected == 1 {
                Ok(FailureDisposition::Quarantined)
            } else {
                // 任务已终结（重复 preflight）→ 幂等返回 Failed。
                // The task is already terminal (repeated preflight) → idempotent
                // Failed.
                Ok(FailureDisposition::Failed)
            }
        })
    }

    /// 读取某 domain 全部 accepted 页（§8.2）：按 page_id 序组装 CompiledPage
    /// （quality 四维 + overall + 一致性列——Step8 批 B3 起该列由 publish 写入，
    /// 旧行/未仲裁行为 NULL）；evidence 从该任务最新 accepted attempt 的
    /// artifact_json 还原——legacy/seed 页无 attempt 载荷，evidence 为 None
    /// （§10：不能伪造回填）。
    /// Reads every accepted page of a domain (§8.2), assembling CompiledPage in
    /// page_id order (quality four dimensions + overall + the consistency column,
    /// written by publish since Step8 batch B3 and NULL for legacy/un-arbitrated
    /// rows); evidence is restored from the task's newest accepted attempt's
    /// artifact_json — legacy/seed pages have no attempt payload, so their
    /// evidence is None (§10: never fabricated back).
    pub fn load_accepted_pages(&self, domain: &str) -> Result<Vec<CompiledPage>> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let rows: Vec<AcceptedPageFullRow> = diesel::sql_query(
                "SELECT page_id, entity_id, source_revision, title, content, content_hash,
                        domain_pack_version, compiled_at, model_version,
                        embedding_model, frontmatter_json
                 FROM pages
                 WHERE domain = ? AND status = 'accepted'
                 ORDER BY page_id",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .load(tx)?;

            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let quality: Option<QualityRow> = diesel::sql_query(
                    "SELECT coverage, citation, schema_compliance, density, consistency
                     FROM page_quality WHERE page_id = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&r.page_id)
                .get_result(tx)
                .optional()?;
                let quality = quality
                    .map(|q| QualityScore {
                        coverage: q.coverage as f32,
                        citation: q.citation as f32,
                        schema_compliance: q.schema_compliance as f32,
                        density: q.density as f32,
                        consistency: q.consistency.map(|c| c as f32),
                    })
                    .unwrap_or(QualityScore {
                        coverage: 0.0,
                        citation: 0.0,
                        schema_compliance: 0.0,
                        density: 0.0,
                        consistency: None,
                    });
                out.push(assemble_compiled_page(tx, r, quality)?);
            }
            Ok(out)
        })
    }

    /// 一致性相关页有界召回（Step8 §6.1 provider 段）：FTS 命中的 accepted 页按
    /// bm25、page_id 稳定排序并 LIMIT `limit`，随后只装载这些页的载荷（绝不先
    /// 全量 SELECT 再内存截断）。`exclude_page_id` 为候选页自身（其旧 generation
    /// 行也不得计入，A8）；SQL 构造（含引号转义）全部落 kernel，锁一次不跨 await。
    /// Bounded related-page recall for consistency (Step8 §6.1 provider
    /// paragraph): FTS-matched accepted pages, stably ordered by bm25/page_id and
    /// LIMIT `limit`; only those pages' payloads are loaded (never a full SELECT
    /// followed by in-memory truncation). `exclude_page_id` is the candidate
    /// itself (its old-generation row must not count either, A8); SQL construction
    /// (with quote escaping) lives entirely in the kernel — one lock, never across
    /// await.
    ///
    /// `terms` 为空或 `limit == 0` 直接返回空集合，不发起 SQL；`limit` 再钳制到
    /// 1..=32（A4 防御边界，与 provider 侧一致）。
    /// Empty `terms` or `limit == 0` returns an empty set without SQL; `limit` is
    /// additionally clamped to 1..=32 (the A4 defensive edge, matching the
    /// provider side).
    pub fn top_k_related_pages(
        &self,
        terms: &[String],
        exclude_page_id: &str,
        limit: u32,
    ) -> Result<Vec<CompiledPage>> {
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, crate::compile::consistency::MAX_CONSISTENCY_TOP_K) as i64;
        // MATCH 词组双引号转义（与 kernel search 的 FTS 惯例一致），OR 连接多词。
        // Double-quote escaping for MATCH phrases (same FTS convention as the
        // kernel search), multiple terms joined with OR.
        let match_expr = terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let rows: Vec<AcceptedPageFullRow> = diesel::sql_query(
                "SELECT p.page_id, p.entity_id, p.source_revision, p.title, p.content,
                        p.content_hash, p.domain_pack_version, p.compiled_at,
                        p.model_version, p.embedding_model, p.frontmatter_json,
                        -bm25(pages_fts) AS score
                 FROM pages_fts f
                 JOIN pages p ON p.page_id = f.page_id
                 WHERE f.pages_fts MATCH ? AND p.status = 'accepted' AND p.page_id != ?
                 ORDER BY score DESC, p.page_id ASC
                 LIMIT ?",
            )
            .bind::<diesel::sql_types::Text, _>(&match_expr)
            .bind::<diesel::sql_types::Text, _>(exclude_page_id)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .load(tx)?;

            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                let quality: Option<RelatedQualityRow> = diesel::sql_query(
                    "SELECT coverage, citation, schema_compliance, density, consistency
                     FROM page_quality WHERE page_id = ?",
                )
                .bind::<diesel::sql_types::Text, _>(&r.page_id)
                .get_result(tx)
                .optional()?;
                let quality = quality
                    .map(|q| QualityScore {
                        coverage: q.coverage as f32,
                        citation: q.citation as f32,
                        schema_compliance: q.schema_compliance as f32,
                        density: q.density as f32,
                        consistency: q.consistency.map(|c| c as f32),
                    })
                    .unwrap_or(QualityScore {
                        coverage: 0.0,
                        citation: 0.0,
                        schema_compliance: 0.0,
                        density: 0.0,
                        consistency: None,
                    });
                out.push(assemble_compiled_page(tx, r, quality)?);
            }
            Ok(out)
        })
    }

    /// 列出某 domain 全部 accepted+published 页的稳定键（§10）：向量 worker 按
    /// `(page_id, generation, content_hash, embedding_model)` 扫描，generation
    /// 逐页版本。只提供读取接口，不发通知（通知不作为唯一恢复依据）。
    /// Lists stable keys of every accepted+published page of a domain (§10): the
    /// future vector worker scans by `(page_id, generation, content_hash,
    /// embedding_model)`, where generation is per-page. Read interface only — no
    /// notifications are sent (notifications are never the sole recovery path).
    pub fn list_published_pages(&self, domain: &str) -> Result<Vec<(String, i64, String, String)>> {
        let mut conn = self.lock_conn()?;
        let rows: Vec<PublishedPageRow> = diesel::sql_query(
            "SELECT page_id, generation, content_hash, embedding_model
             FROM pages
             WHERE domain = ? AND status = 'accepted'
             ORDER BY page_id",
        )
        .bind::<diesel::sql_types::Text, _>(domain)
        .load(&mut *conn)?;
        Ok(rows
            .into_iter()
            .map(|r| (r.page_id, r.generation, r.content_hash, r.embedding_model))
            .collect())
    }

    /// 向量 payload 批量校验（§10 必要边界修复）：返回仍有效的 page_id 集合。
    /// 页有效 ⇔ 传入的该页**全部** payload 都与 accepted head 一致（status=
    /// accepted、content_hash、generation 三者逐条相等）；同页混入旧代/陈旧
    /// chunk（部分匹配）时整页判无效——杜绝旧代向量借同页有效 payload 冒充。
    /// 调用方（QueryEngine）在 RRF 融合与截取 top_k 前丢弃无效页与缺版本
    /// metadata 的 hit。
    /// Bulk vector-payload validation (§10 boundary fix): returns the set of
    /// page_ids that remain valid. A page is valid iff **every** payload provided
    /// for it matches its accepted head (status=accepted, content_hash and
    /// generation all equal, per row); a page mixing stale chunks with a current
    /// one (partial match) is invalid as a whole — a stale-generation vector can
    /// never ride on a sibling's valid payload. The caller (QueryEngine) drops
    /// invalid pages and hits without version metadata before RRF fusion and
    /// top_k truncation.
    pub fn validate_vector_payloads(
        &self,
        hits: &[(String, String, u64)],
    ) -> Result<std::collections::HashSet<String>> {
        if hits.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        // hit 列表经 JSON 参数传入（json_each 展开），无插值拼接；同一 page_id
        // 的多条 payload 按 COUNT/SUM 聚合出「全部一致才有效」的语义。
        // The hit list is passed as a JSON parameter (expanded by json_each) — no
        // SQL interpolation; multiple payloads of one page_id aggregate via
        // COUNT/SUM into the "valid only when all match" semantics.
        let payload: Vec<serde_json::Value> = hits
            .iter()
            .map(|(page_id, content_hash, generation)| {
                serde_json::json!({
                    "page_id": page_id,
                    "hash": content_hash,
                    "generation": generation,
                })
            })
            .collect();
        let payload = serde_json::to_string(&payload)?;
        let mut conn = self.lock_conn()?;
        let rows: Vec<ValidPageIdRow> = diesel::sql_query(
            "SELECT t.pid AS page_id
             FROM (
                 SELECT json_extract(v.value, '$.page_id') AS pid,
                        COUNT(*) AS total,
                        SUM(CASE WHEN p.page_id IS NOT NULL THEN 1 ELSE 0 END) AS matched
                 FROM json_each(?) v
                 LEFT JOIN pages p
                     ON p.page_id = json_extract(v.value, '$.page_id')
                    AND p.status = 'accepted'
                    AND p.content_hash = json_extract(v.value, '$.hash')
                    AND p.generation = json_extract(v.value, '$.generation')
                 GROUP BY pid
             ) t
             WHERE t.total = t.matched",
        )
        .bind::<diesel::sql_types::Text, _>(&payload)
        .load(&mut *conn)?;
        Ok(rows.into_iter().map(|r| r.page_id).collect())
    }

    /// 页面向量索引构建读取面（真实嵌入实验）：某 domain 全部 accepted 页的
    /// embedding 载荷六列（page_id/entity_id/title/content/content_hash/
    /// generation）。纯 SELECT，一次连接锁、无事务、无写入；`page_id` 序稳定，
    /// 向量索引确定性可复现。generation 取自 pages 表自身，与
    /// [`validate_vector_payloads`] 的 staleness 校验同源（payload 的
    /// generation 必须等于该页的 `pages.generation` 才不会被判 stale）。
    /// Vector-index build read surface (real-embedding experiment): the six
    /// embedding-payload columns (page_id/entity_id/title/content/content_hash/
    /// generation) of every accepted page of a domain. A pure SELECT — one
    /// connection lock, no transaction, no writes; stable `page_id` order keeps
    /// the vector index deterministically reproducible. The generation comes
    /// from the pages table itself, the same source as the
    /// [`validate_vector_payloads`] staleness check (a payload's generation must
    /// equal the page's `pages.generation` or it is dropped as stale).
    pub fn accepted_page_vectors(&self, domain: &str) -> Result<Vec<AcceptedPageVector>> {
        let mut conn = self.lock_conn()?;
        let rows: Vec<AcceptedPageVectorRow> = diesel::sql_query(
            "SELECT page_id, entity_id, title, content, content_hash, generation
             FROM pages
             WHERE domain = ? AND status = 'accepted'
             ORDER BY page_id",
        )
        .bind::<diesel::sql_types::Text, _>(domain)
        .load(&mut *conn)?;
        Ok(rows
            .into_iter()
            .map(|r| AcceptedPageVector {
                page_id: r.page_id,
                entity_id: r.entity_id,
                title: r.title,
                content: r.content,
                content_hash: r.content_hash,
                generation: r.generation.max(0) as u64,
            })
            .collect())
    }

    // ===== Step8 批 B5：兼容 preflight 读取面（§6.4/D10/D11；只读）=====
    // ===== Step8 batch B5: compatibility-preflight read surface (§6.4/D10/D11;
    // read-only) =====

    /// 兼容 preflight：读取某 domain 全部 accepted 页的身份四列（§6.4；纯
    /// SELECT，一次连接锁、无事务、无写入）。`page_id` 序稳定，报告确定性。
    /// Compatibility preflight: reads the four identity columns of every
    /// accepted page of a domain (§6.4; a pure SELECT — one connection lock, no
    /// transaction, no writes). Stable `page_id` order keeps reports
    /// deterministic.
    pub fn compatibility_page_identities(
        &self,
        domain: &str,
    ) -> Result<Vec<PersistedPageIdentity>> {
        let mut conn = self.lock_conn()?;
        let rows: Vec<CompatPageRow> = diesel::sql_query(
            "SELECT page_id, domain_pack_version, artifact_version, frontmatter_json
             FROM pages
             WHERE domain = ? AND status = 'accepted'
             ORDER BY page_id",
        )
        .bind::<diesel::sql_types::Text, _>(domain)
        .load(&mut *conn)?;
        Ok(rows
            .into_iter()
            .map(|r| PersistedPageIdentity {
                page_id: r.page_id,
                domain_pack_version: r.domain_pack_version,
                artifact_version: r.artifact_version,
                frontmatter_json: r.frontmatter_json,
            })
            .collect())
    }

    /// 兼容 preflight：读取全部 pending/running/dead 任务的 dependencies_json
    /// 快照（§6.4「所有 pending/running/dead tasks 的 dependencies_json 快照」；
    /// succeeded/superseded 历史不算当前产物）。compile_tasks 无 domain 列
    /// （Step4 先例），域过滤由 checker 按实体键首段精确比较，不做 LIKE 通配
    /// 转义。task_id 序稳定。
    /// Compatibility preflight: reads every pending/running/dead task's
    /// dependencies_json snapshot (§6.4 "all pending/running/dead tasks'
    /// dependencies_json snapshots"; succeeded/superseded history is not a
    /// current artifact). compile_tasks has no domain column (a Step4
    /// precedent), so the checker scopes the domain by exact comparison of the
    /// entity key's leading segment instead of LIKE-escape juggling. Stable
    /// task_id order.
    pub fn compatibility_task_snapshots(&self) -> Result<Vec<PersistedTaskSnapshot>> {
        let mut conn = self.lock_conn()?;
        let rows: Vec<CompatTaskRow> = diesel::sql_query(
            "SELECT task_id, entity_id, dependencies_json
             FROM compile_tasks
             WHERE status IN ('pending', 'running', 'dead')
             ORDER BY task_id",
        )
        .load(&mut *conn)?;
        Ok(rows
            .into_iter()
            .map(|r| PersistedTaskSnapshot {
                task_id: r.task_id,
                entity_id: r.entity_id,
                dependencies_json: r.dependencies_json,
            })
            .collect())
    }

    /// `--skip-compatibility-check` 的 fail-closed 判定（§7）：数据库是否存在旧
    /// artifact——任意 domain 的 accepted 页，或任意 pending/running/dead 任务。
    /// 「空数据库」按字面全局口径：任何旧数据都拒绝跳过（A15 fail-closed）。
    /// 只读，一次连接锁。
    /// The fail-closed decision behind `--skip-compatibility-check` (§7): does
    /// the database hold any old artifact — an accepted page of any domain, or
    /// any pending/running/dead task. "Empty database" is taken literally and
    /// globally: any old data refuses the skip (A15 fail-closed). Read-only, one
    /// connection lock.
    pub fn has_existing_compile_data(&self) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        let row: CountRow = diesel::sql_query(
            "SELECT CASE
                 WHEN EXISTS(SELECT 1 FROM pages WHERE status = 'accepted') THEN 1
                 WHEN EXISTS(SELECT 1 FROM compile_tasks
                             WHERE status IN ('pending', 'running', 'dead')) THEN 1
                 ELSE 0 END AS n",
        )
        .get_result(&mut *conn)?;
        Ok(row.n > 0)
    }

    /// 兼容告警入队（§5.2/§6.4/D11/A16）：独立 BEGIN IMMEDIATE 写事务——
    /// `subject_json`/`reason_json` 必须为 JSON 对象（B3 宽松校验同纪律），表内
    /// `UNIQUE(domain,action,subject_json)` 冲突 `DO NOTHING` 幂等跳过；返回实际
    /// 插入行数（0 = 已存在被跳过，重复 run 不新增告警）。`compile_task_id`
    /// 恒为 NULL（admission 被拒，无任务可回填）。
    /// Compatibility-alert enqueue (§5.2/§6.4/D11/A16): one standalone BEGIN
    /// IMMEDIATE write transaction — `subject_json`/`reason_json` must be JSON
    /// objects (the same relaxed B3 validation), and the table's
    /// `UNIQUE(domain,action,subject_json)` conflict is idempotently skipped via
    /// `DO NOTHING`; returns the rows actually inserted (0 = skipped as
    /// existing, so repeated runs never add alerts). `compile_task_id` stays
    /// NULL (admission was refused — no task to backfill).
    pub fn insert_compatibility_conflict_review(
        &self,
        domain: &str,
        subject_json: &str,
        reason_json: &str,
        now: i64,
    ) -> Result<usize> {
        // 纯校验先于写锁（任一非法 → Validation，不取锁、不落行）。
        // Pure validation precedes the write lock (any illegal input →
        // Validation, no lock taken, no rows written).
        let subject_is_object = serde_json::from_str::<serde_json::Value>(subject_json)
            .map(|v| v.is_object())
            .unwrap_or(false);
        if !subject_is_object {
            return Err(Error::Validation(
                "compatibility_conflict subject_json must be a JSON object".into(),
            ));
        }
        if serde_json::from_str::<serde_json::Value>(reason_json).is_err() {
            return Err(Error::Validation(
                "compatibility_conflict reason_json must be parseable JSON".into(),
            ));
        }
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            insert_review_on_conn(
                tx,
                domain,
                "compatibility_conflict",
                subject_json,
                reason_json,
                None,
                now,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compatibility::{
        CompatibilityChecker, StandardCompatibilityChecker, COMPATIBILITY_ARTIFACT_NOT_ALLOWED,
        COMPATIBILITY_CORRUPT_SNAPSHOT, COMPATIBILITY_VERSION_MISSING, COMPATIBILITY_VERSION_RANGE,
    };
    use crate::compile::config::{prepare_source, CompatibilitySpec, DomainIdentity};
    use crate::compile::contract::{
        render_canonical_markdown, Assertion, DefaultSourceRefValidator, EvidenceSection,
        OutputWiki, SourceRef, SourceRefValidator,
    };
    use crate::compile::quality::{RuleBasedScorer, RuleScorer};
    use crate::types::{EntityId, FieldDefinition, FieldType, PublishStatus};
    use std::collections::BTreeMap;

    // ===== 测试夹具 =====
    // ===== Test fixtures =====

    fn schema() -> EntitySchema {
        EntitySchema {
            entity_type: "drink".into(),
            fields: vec![
                FieldDefinition {
                    name: "name".into(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "description".into(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "category".into(),
                    field_type: FieldType::Text,
                    filterable: true,
                },
                FieldDefinition {
                    name: "price".into(),
                    field_type: FieldType::Numeric,
                    filterable: true,
                },
            ],
        }
    }

    fn ctx() -> CompileContext {
        CompileContext {
            domain_pack_version: "0.1.0".into(),
            prompt_template: "SYSTEM source-ref-v1\nTEMPLATE".into(),
            model_version: "mock-v1".into(),
            embedding_model: "none".into(),
            quality_threshold: 0.75,
            require_source_refs: true,
            schema_version: None,
            prompt_version: None,
        }
    }

    fn policy() -> CompilePolicy {
        CompilePolicy {
            knowledge_fields: vec!["name".into(), "description".into()],
            ..CompilePolicy::default()
        }
    }

    fn raw(revision: u64, price: f64) -> RawEntity {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        fields.insert(
            "category".to_string(),
            serde_json::json!("milk-tea:drink:boba"),
        );
        fields.insert("price".to_string(), serde_json::json!(price));
        RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: revision,
        }
    }

    fn prepared(revision: u64) -> PreparedSource {
        prepare_source(&raw(revision, 19.0), &schema(), &policy()).unwrap()
    }

    fn prepared_with(revision: u64, policy: &CompilePolicy) -> PreparedSource {
        prepare_source(&raw(revision, 19.0), &schema(), policy).unwrap()
    }

    /// §5.1 形状的合法证据：两断言两 refs，标题命中 quote。
    /// Legal evidence of §5.1 shape: two assertions, two refs, title inside quotes.
    fn evidence_ok(p: &PreparedSource) -> CompileEvidence {
        let revision = p.knowledge.source_revision;
        CompileEvidence {
            schema_version: "source-ref-v1".into(),
            wiki: OutputWiki {
                title: "啵啵".into(),
                aliases: vec![],
                tags: vec![],
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".into(),
                assertions: vec![
                    Assertion {
                        text: "啵啵".into(),
                        ref_ids: vec!["r1".into()],
                    },
                    Assertion {
                        text: "珍珠奶茶".into(),
                        ref_ids: vec!["r2".into()],
                    },
                ],
                refs: vec![
                    SourceRef {
                        id: "r1".into(),
                        entity_id: "milk-tea:drink:boba".into(),
                        source_revision: revision,
                        pointer: "/fields/name".into(),
                        value: serde_json::json!("啵啵"),
                        quote: "啵啵".into(),
                    },
                    SourceRef {
                        id: "r2".into(),
                        entity_id: "milk-tea:drink:boba".into(),
                        source_revision: revision,
                        pointer: "/fields/description".into(),
                        value: serde_json::json!("珍珠奶茶"),
                        quote: "珍珠奶茶".into(),
                    },
                ],
            }],
            usage: None,
        }
    }

    fn build_page(
        p: &PreparedSource,
        mut evidence: CompileEvidence,
        policy: &CompilePolicy,
    ) -> CompiledPage {
        evidence.wiki.markdown = render_canonical_markdown(&evidence);
        let desired_hash = content_hash(HashDependencies {
            source: &p.knowledge,
            context: &ctx(),
            policy,
            source_schema: &schema(),
        })
        .unwrap();
        let markdown = evidence.wiki.markdown.clone();
        CompiledPage {
            wiki: WikiPage {
                page_id: "milk-tea:drink:boba".into(),
                entity_id: p.knowledge.id.clone(),
                title: "啵啵".into(),
                content: markdown.clone(),
                sections: vec![Section {
                    heading: "概述".into(),
                    content: markdown,
                }],
                metadata: PageMetadata {
                    domain_pack_version: "0.1.0".into(),
                    compiled_at: 0,
                    model_version: "mock-v1".into(),
                    embedding_model: "none".into(),
                },
                aliases: vec![],
                tags: vec![],
            },
            quality: QualityScore {
                coverage: 1.0,
                citation: 1.0,
                schema_compliance: 1.0,
                density: 1.0,
                consistency: None,
            },
            qug_edges: vec![],
            content_hash: desired_hash,
            evidence: Some(evidence),
        }
    }

    /// accepted 产物：真实 scorer 四维全 1 且通过门槛。
    /// Accepted artifact: real scorer gives all-four dimensions 1 and passes gates.
    fn accepted_page_with(
        p: &PreparedSource,
        policy: &CompilePolicy,
    ) -> (CompiledPage, ScoreReport) {
        let evidence = evidence_ok(p);
        let page = build_page(p, evidence, policy);
        let refs =
            DefaultSourceRefValidator.validate(&p.knowledge, page.evidence.as_ref().unwrap(), true);
        assert!(
            refs.issues.is_empty(),
            "fixture refs must be valid: {:?}",
            refs.issues
        );
        let report = RuleBasedScorer.score(&p.knowledge, Some(&page), &refs, true, &ctx(), policy);
        assert!(report.accepted, "fixture page must be accepted");
        (page, report)
    }

    fn accepted_page(p: &PreparedSource) -> (CompiledPage, ScoreReport) {
        accepted_page_with(p, &policy())
    }

    /// 低质候选：同 quote 重复十次 → density≈0.25，被门槛拒绝（§6）。
    /// Low-quality candidate: the same quote repeated ten times → density≈0.25,
    /// rejected by the gates (§6).
    fn low_quality_page(p: &PreparedSource, policy: &CompilePolicy) -> (CompiledPage, ScoreReport) {
        let mut evidence = evidence_ok(p);
        evidence.sections[0].assertions[0].text = ["啵啵"; 10].join(" ");
        evidence.sections[0].assertions[0].ref_ids = vec!["r1".into(); 10];
        let page = build_page(p, evidence, policy);
        let refs =
            DefaultSourceRefValidator.validate(&p.knowledge, page.evidence.as_ref().unwrap(), true);
        let report = RuleBasedScorer.score(&p.knowledge, Some(&page), &refs, true, &ctx(), policy);
        assert!(!report.accepted, "fixture candidate must be rejected");
        (page, report)
    }

    fn seed_wiki_page() -> WikiPage {
        crate::seed::parse_page(
            "---\npage_id: milk-tea:drink:legacy\nentity_id: milk-tea:drink:legacy\ntitle: 乌龙奶茶\nentity_type: drink\n---\n\n乌龙奶茶是经典茶底。\n\n## 概述\n\n- 乌龙茶底\n",
        )
        .unwrap()
    }

    // ===== 查询辅助 =====
    // ===== Query helpers =====

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    #[derive(QueryableByName)]
    struct TaskStateRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        status: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        result: Option<String>,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        retry_count: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        attempt_count: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        recompile_count: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        epoch: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        reserved_tokens: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        next_attempt_at: i64,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        lease_token: Option<String>,
    }

    fn count(kernel: &SqliteKernel, table: &str) -> i64 {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(format!("SELECT COUNT(*) AS n FROM {table}"))
            .get_result::<CountRow>(&mut *conn)
            .unwrap()
            .n
    }

    fn scalar_i64(kernel: &SqliteKernel, sql: &str) -> i64 {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(sql)
            .get_result::<CountRow>(&mut *conn)
            .unwrap()
            .n
    }

    fn task_state(kernel: &SqliteKernel, task_id: i64) -> TaskStateRow {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "SELECT status, result, retry_count, attempt_count, recompile_count, epoch,
                    reserved_tokens, next_attempt_at, lease_token
             FROM compile_tasks WHERE task_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .get_result(&mut *conn)
        .unwrap()
    }

    fn queued_with(
        kernel: &SqliteKernel,
        p: &PreparedSource,
        policy: &CompilePolicy,
        force: bool,
    ) -> i64 {
        match kernel
            .admit_compile(p, &ctx(), policy, &schema(), force)
            .unwrap()
        {
            Admission::Queued(id) => id,
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn queued(kernel: &SqliteKernel, p: &PreparedSource) -> i64 {
        queued_with(kernel, p, &policy(), false)
    }

    // ===== A15–A18 =====

    // A16（部分）+ A2 + A1 基础链路：admit → claim → publish 全链路、generation
    // 不重用 legacy 1、幂等重放不重复分配、同输入重跑 skip、load_accepted_pages
    // 往返（含 evidence 从 attempts 还原）。
    // A16 (partial) + A2 + A1 base loop: full admit → claim → publish cycle, no
    // legacy generation-1 reuse, idempotent replay without re-allocation, rerun
    // skip, and load_accepted_pages round-trip (evidence restored from attempts).
    #[test]
    fn full_cycle_publish_replay_skip_and_load() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        // legacy seed 页（generation=1）→ 兜底哨兵保证新发布不重用 1（§8.1）。
        // Legacy seed page (generation=1) → the fallback sentinel keeps new
        // publishes from reusing 1 (§8.1).
        kernel
            .seed_pages(&seed_wiki_page(), "milk-tea", PublishStatus::Accepted)
            .unwrap();

        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        assert_eq!(lease.epoch, 1);
        assert_eq!(lease.attempt_no, 1);
        assert_eq!(lease.source.id, p.knowledge.id);
        assert_eq!(lease.context.prompt_template, ctx().prompt_template);

        let (page, report) = accepted_page(&p);
        let outcome = kernel
            .publish_compile(&lease, &page, &report, None, 1000)
            .unwrap();
        let generation = match outcome {
            CommitOutcome::Accepted { generation } => generation,
            other => panic!("expected accepted, got {other:?}"),
        };
        assert!(
            generation >= 2,
            "must not reuse legacy generation 1, got {generation}"
        );

        // legacy seed 页 + 编译页各 1（seed 页导语+概述 2 节 + 编译页 1 节 = 3 节）。
        // Legacy seed page + compiled page (seed has intro+overview = 2 sections,
        // compiled 1 → 3 total).
        assert_eq!(count(&kernel, "pages"), 2);
        assert_eq!(count(&kernel, "page_sections"), 3);
        assert_eq!(count(&kernel, "generations"), 2, "sentinel + published");
        assert_eq!(count(&kernel, "pages_fts"), 2);
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "succeeded");
        assert_eq!(t.result.as_deref(), Some("accepted"));
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE status='completed' AND publish_status='accepted'"
            ),
            1
        );

        // 幂等重放：同 lease 重复 publish 返回既有 generation（A16）。
        // Idempotent replay: re-publishing with the same lease returns the existing
        // generation (A16).
        let replay = kernel
            .publish_compile(&lease, &page, &report, None, 1001)
            .unwrap();
        assert_eq!(replay, CommitOutcome::Accepted { generation });
        assert_eq!(
            count(&kernel, "generations"),
            2,
            "replay must not allocate a generation"
        );

        // A2：同输入重跑 → skip，无新任务可领。
        // A2: rerunning the same input → skip, no claimable task.
        assert_eq!(
            kernel
                .admit_compile(&p, &ctx(), &policy(), &schema(), false)
                .unwrap(),
            Admission::Skipped
        );
        assert!(kernel
            .claim_compile(&[task_id], "run-1", 1002)
            .unwrap()
            .is_none());

        // load_accepted_pages（§8.2）：正文/章节/评分/evidence 完整往返。
        // load_accepted_pages (§8.2): content/sections/quality/evidence fully
        // round-trip.
        let loaded = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(loaded.len(), 2, "legacy seed page + compiled page");
        let compiled = loaded
            .iter()
            .find(|l| l.wiki.page_id == "milk-tea:drink:boba")
            .expect("compiled page must be loaded");
        assert_eq!(compiled.wiki.content, page.wiki.content);
        assert_eq!(compiled.wiki.sections.len(), 1);
        assert_eq!(compiled.quality.overall(), 1.0);
        assert!(
            compiled.evidence.is_some(),
            "evidence must round-trip via attempts artifact_json"
        );
        assert!(kernel.load_accepted_pages("other").unwrap().is_empty());
    }

    // A15：租约 fencing —— worker A 超时、B 回收领取后 A 不能发布/续约/改计数；
    // 反复回收只记一次失败和一次 reservation；预留不退还。
    // A15: lease fencing — after worker A times out and B recovers and claims, A
    // can neither publish, renew, nor mutate counters; repeated recovery counts one
    // failure and one reservation; reservations are never refunded.
    #[test]
    fn a15_lease_fencing_blocks_stale_worker() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);

        let lease_a = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        // worker A 停摆到租约过期（1000+300）。
        // Worker A stalls past its lease (1000+300).
        let recovery = kernel.recover_compile_leases(1301).unwrap();
        assert_eq!(recovery.recovered_pending, 1);
        assert_eq!(recovery.scanned, 1);
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "pending");
        assert_eq!(t.retry_count, 1);
        assert_eq!(t.next_attempt_at, 1302, "backoff(1)=1s");
        assert!(t.lease_token.is_none());

        // 反复回收不重复计数（§8.3）。
        // Repeated recovery never double-counts (§8.3).
        assert_eq!(
            kernel.recover_compile_leases(1301).unwrap(),
            RecoveryStats::default()
        );
        assert_eq!(task_state(&kernel, task_id).retry_count, 1);
        // 预留不退还（§8.3/§8.4）。
        // Reservations are never refunded (§8.3/§8.4).
        assert!(task_state(&kernel, task_id).reserved_tokens > 0);

        // worker B 领取（attempt_no=2）。
        // Worker B claims (attempt_no=2).
        let lease_b = kernel
            .claim_compile(&[task_id], "run-1", 1302)
            .unwrap()
            .unwrap();
        assert_eq!(lease_b.attempt_no, 2);
        assert_ne!(lease_b.lease_token, lease_a.lease_token);

        // A 的旧租约：不能续约。
        // A's stale lease: cannot renew.
        assert!(!kernel.heartbeat_compile(&lease_a, 1302).unwrap());
        // 不能发布（Stale），且不留任何发布痕迹。
        // Cannot publish (Stale), leaving no publish traces.
        let (page, report) = accepted_page(&p);
        assert_eq!(
            kernel
                .publish_compile(&lease_a, &page, &report, None, 1302)
                .unwrap(),
            CommitOutcome::Stale
        );
        // 不能改计数（failure 走 stale 路径返回 Failed）。
        // Cannot mutate counters (failure takes the stale path, returning Failed).
        let (low, low_report) = low_quality_page(&p, &policy());
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease_a,
                    &CompileFailure::invalid("LOW", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1302
                )
                .unwrap(),
            FailureDisposition::Failed
        );
        let after = task_state(&kernel, task_id);
        assert_eq!(after.attempt_count, 2);
        assert_eq!(after.retry_count, 1);
        assert_eq!(after.recompile_count, 0);
        assert_eq!(count(&kernel, "pages"), 0);
        assert_eq!(count(&kernel, "generations"), 0);
        // B 的 attempt 仍 reserved。
        // B's attempt is still reserved.
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE status='reserved'"
            ),
            1
        );

        // B 正常发布成功。
        // B publishes successfully.
        assert_eq!(
            kernel
                .publish_compile(&lease_b, &page, &report, None, 1302)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
    }

    // A12（Step8 批 B4）：心跳续租——claim 后多次心跳推进注入时间，租约窗口
    // 跨过 300s 仍恒为 now+300（未来）；status/计数不被心跳改动。
    // A12 (Step8 batch B4): heartbeat renewal — multiple heartbeats advance the
    // injected time past the claim; the lease window stays now+300 (in the
    // future) across the 300s mark; status/counters are untouched by the
    // heartbeat.
    #[test]
    fn a12_heartbeat_renewal_keeps_lease_alive_past_window() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();

        // 心跳跨两个窗口推进：expires 恒 = 本次心跳 now+300（A12「等待超过
        // 300 秒仍保持 lease」的 kernel 面）。
        // Heartbeats advance across two windows: expires stays = this heartbeat's
        // now+300 (the kernel face of A12 "waiting past 300 seconds keeps the
        // lease").
        for now in [1200i64, 1400, 1600] {
            assert!(kernel.heartbeat_compile(&lease, now).unwrap());
            let expires = scalar_i64(
                &kernel,
                &format!(
                    "SELECT lease_expires_at AS n FROM compile_tasks WHERE task_id = {task_id}"
                ),
            );
            assert_eq!(expires, now + 300, "heartbeat at {now}");
            assert!(expires > now);
        }

        // 心跳只续租：状态、attempt/retry/recompile 计数全部不变。
        // The heartbeat only renews: status and attempt/retry/recompile counters
        // all unchanged.
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "running");
        assert_eq!(t.attempt_count, 1);
        assert_eq!(t.retry_count, 0);
        assert_eq!(t.recompile_count, 0);
    }

    // A12（Step8 批 B4）：新 epoch 接管 fencing——另一 worker 经 force admission
    // 抬升 epoch 并领取后，worker A 的旧租约不能续约/发布/改计数（affected=0）。
    // A12 (Step8 batch B4): new-epoch takeover fencing — after another worker
    // bumps the epoch via a force admission and claims, worker A's stale lease can
    // neither renew, publish, nor mutate counters (affected=0).
    #[test]
    fn a12_epoch_takeover_blocks_stale_worker_writes() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease_a = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        assert_eq!(lease_a.epoch, 1);

        // force admission：同 hash 强制重排 → epoch+1、计数归零、旧租约清空。
        // Force admission: a forced requeue on the same hash → epoch+1, counters
        // reset, old lease cleared.
        let _ = queued_with(&kernel, &p, &policy(), true);
        let lease_b = kernel
            .claim_compile(&[task_id], "run-1", 1001)
            .unwrap()
            .unwrap();
        assert_eq!(lease_b.epoch, 2, "takeover worker runs on the new epoch");
        assert_ne!(lease_b.lease_token, lease_a.lease_token);

        // A 旧租约：不能续约。
        // A's stale lease: cannot renew.
        assert!(!kernel.heartbeat_compile(&lease_a, 1100).unwrap());
        // 不能发布（Stale），且不留任何发布痕迹。
        // Cannot publish (Stale), leaving no publish traces.
        let (page, report) = accepted_page(&p);
        assert_eq!(
            kernel
                .publish_compile(&lease_a, &page, &report, None, 1100)
                .unwrap(),
            CommitOutcome::Stale
        );
        // 不能改计数（failure 走 stale 路径返回 Failed）。
        // Cannot mutate counters (failure takes the stale path, returning Failed).
        let (low, low_report) = low_quality_page(&p, &policy());
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease_a,
                    &CompileFailure::invalid("LOW", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1100
                )
                .unwrap(),
            FailureDisposition::Failed
        );

        // 计数只反映接管 worker B：attempt=1、retry=0、recompile=0；无页发布。
        // Counters reflect only the takeover worker B: attempt=1, retry=0,
        // recompile=0; no page published.
        let after = task_state(&kernel, task_id);
        assert_eq!(after.attempt_count, 1);
        assert_eq!(after.retry_count, 0);
        assert_eq!(after.recompile_count, 0);
        assert_eq!(count(&kernel, "pages"), 0);
        assert_eq!(count(&kernel, "generations"), 0);
    }

    // A16：原子发布 —— 注入失败（hash 不匹配 / artifact 超限）无半套写入；重试
    // 提交（同 lease 重放）只增长一次 generation。
    // A16: atomic publish — injected failures (hash mismatch / artifact over cap)
    // leave no partial writes; retry-committing (same-lease replay) grows the
    // generation exactly once.
    #[test]
    fn a16_atomic_publish_injected_failures() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (page, report) = accepted_page(&p);

        // 注入失败 1：content_hash 与任务 desired_hash 不一致 → Err。
        // Injected failure 1: content_hash diverges from the task's desired_hash.
        let mut bad = page.clone();
        bad.content_hash = "deadbeef".into();
        assert!(matches!(
            kernel.publish_compile(&lease, &bad, &report, None, 1000),
            Err(Error::ContentHashMismatch { .. })
        ));
        assert_no_partial_publish(&kernel);

        // 注入失败 2：artifact_json 超 256 KiB → Err。
        // Injected failure 2: artifact_json exceeds 256 KiB.
        let mut big = page.clone();
        big.wiki.content = "长".repeat(90_000);
        big.wiki.sections[0].content = big.wiki.content.clone();
        assert!(matches!(
            kernel.publish_compile(&lease, &big, &report, None, 1000),
            Err(Error::Compilation(_))
        ));
        assert_no_partial_publish(&kernel);

        // 正常发布 → generation=1；重放复用同一 generation。
        // Normal publish → generation=1; replay reuses the same generation.
        assert_eq!(
            kernel
                .publish_compile(&lease, &page, &report, None, 1000)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
        assert_eq!(
            kernel
                .publish_compile(&lease, &page, &report, None, 1001)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
        assert_eq!(count(&kernel, "generations"), 1);
    }

    fn assert_no_partial_publish(kernel: &SqliteKernel) {
        assert_eq!(count(kernel, "pages"), 0, "pages must roll back");
        assert_eq!(count(kernel, "page_sections"), 0, "sections must roll back");
        assert_eq!(count(kernel, "page_quality"), 0, "quality must roll back");
        assert_eq!(
            count(kernel, "generations"),
            0,
            "generations must roll back"
        );
        assert_eq!(count(kernel, "qug_edges"), 0, "edges must roll back");
        assert_eq!(count(kernel, "pages_fts"), 0, "FTS must roll back");
        assert_eq!(
            scalar_i64(
                kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE status='reserved'"
            ),
            1,
            "reserved attempt must survive for retry"
        );
    }

    // A17：旧版本保留 —— 已接受页后新低质版本隔离/重试，旧正文、评分与 FTS 保持
    // 不变；页级刹车到顶后 dead/quarantined 仍不抹掉上一代。
    // A17: prior version retained — after an accepted page, new low-quality
    // versions are quarantined/retried while the old body, score and FTS stay
    // untouched; even after the page-level brake trips, the previous generation is
    // never overwritten.
    #[test]
    fn a17_prior_accepted_version_retained() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease1 = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (page, report) = accepted_page(&p);
        assert_eq!(
            kernel
                .publish_compile(&lease1, &page, &report, None, 1000)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );

        // 新配置（scorer_version 变 → desired_hash 变）→ 同三元 epoch+1 重排。
        // New config (scorer_version changes → desired_hash changes) → same triple
        // requeued with epoch+1.
        let mut policy2 = policy();
        policy2.scorer_version = "rules-v2".into();
        let p2 = prepared_with(1, &policy2);
        assert_eq!(
            kernel
                .admit_compile(&p2, &ctx(), &policy2, &schema(), false)
                .unwrap(),
            Admission::Queued(task_id)
        );
        let lease2 = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        assert_eq!(lease2.epoch, 2);

        // 低质候选 1 → RetryAt；旧 accepted 版本原样保留。
        // Low-quality candidate 1 → RetryAt; the prior accepted version is intact.
        let (low, low_report) = low_quality_page(&p2, &policy2);
        assert!(matches!(
            kernel
                .finish_compile_failure(
                    &lease2,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1000
                )
                .unwrap(),
            FailureDisposition::RetryAt(_)
        ));
        assert_retained(&kernel, &page);

        // 候选 2 → RetryAt；候选 3（recompile=3 > max_recompiles=2）→ Quarantined。
        // Candidate 2 → RetryAt; candidate 3 (recompile=3 > max_recompiles=2) →
        // Quarantined.
        let lease3 = kernel
            .claim_compile(&[task_id], "run-1", 1001)
            .unwrap()
            .unwrap();
        assert!(matches!(
            kernel
                .finish_compile_failure(
                    &lease3,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1001
                )
                .unwrap(),
            FailureDisposition::RetryAt(_)
        ));
        let lease4 = kernel
            .claim_compile(&[task_id], "run-1", 1002)
            .unwrap()
            .unwrap();
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease4,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1002
                )
                .unwrap(),
            FailureDisposition::Quarantined
        );

        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "dead");
        assert_eq!(t.result.as_deref(), Some("quarantined"));
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE publish_status='candidate'"
            ),
            3,
            "exactly max_recompiles+1 quality candidates"
        );
        // 隔离后旧正文仍检索可见，新正文不在 pages（§8.2/§8.3）。
        // After quarantine the old body stays searchable and the new body never
        // reached pages (§8.2/§8.3).
        assert_retained(&kernel, &page);
    }

    fn assert_retained(kernel: &SqliteKernel, page: &CompiledPage) {
        assert_eq!(count(kernel, "pages"), 1);
        assert_eq!(count(kernel, "page_sections"), 1);
        assert_eq!(count(kernel, "generations"), 1);
        assert_eq!(count(kernel, "pages_fts"), 1);
        let loaded = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(loaded[0].wiki.content, page.wiki.content);
        assert_eq!(loaded[0].quality.overall(), 1.0);
    }

    // A18：CAS/幂等 —— 同三元同 hash 合并不重置计数、同 revision 不同 snapshot
    // 冲突拒绝、低 revision stale skip 且 facts 不回退、新配置 epoch fence 旧
    // worker。
    // A18: CAS/idempotency — same-triple same-hash merges without resetting
    // counters, same-revision different-snapshot conflicts are rejected, lower
    // revisions skip stale without rolling facts back, and a new config fences the
    // old worker via a new epoch.
    #[test]
    fn a18_cas_idempotency_and_epoch_fence() {
        let kernel = SqliteKernel::open_in_memory().unwrap();

        // 同三元同 hash 重复 admit → 合并：不 bump epoch、不重置计数（§7.3）。
        // Re-admitting the same triple and hash → merge: no epoch bump, no counter
        // reset (§7.3).
        let p1 = prepared(1);
        let task_id = queued(&kernel, &p1);
        let _lease1 = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        assert_eq!(task_state(&kernel, task_id).attempt_count, 1);
        assert_eq!(
            kernel
                .admit_compile(&p1, &ctx(), &policy(), &schema(), false)
                .unwrap(),
            Admission::Queued(task_id)
        );
        let t = task_state(&kernel, task_id);
        assert_eq!(t.epoch, 1, "merge must not bump epoch");
        assert_eq!(t.attempt_count, 1, "merge must not reset counters");

        // 同 revision 不同 snapshot → Rejected(source_revision_conflict)（§7.1）。
        // Same revision, different snapshot → Rejected(source_revision_conflict).
        let conflict = prepare_source(&raw(1, 25.0), &schema(), &policy()).unwrap();
        assert_eq!(
            kernel
                .admit_compile(&conflict, &ctx(), &policy(), &schema(), false)
                .unwrap(),
            Admission::Rejected("source_revision_conflict".into())
        );

        // 高 revision：新三元新任务，facts CAS 前进到 rev2（§7.1）。
        // Higher revision: a new triple/task; facts CAS advances to rev2 (§7.1).
        let p2 = prepared(2);
        let t2 = queued(&kernel, &p2);
        assert_ne!(t2, task_id, "new revision is a new task");
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT source_revision AS n FROM facts WHERE entity_id='milk-tea:drink:boba' AND field_name='price'"
            ),
            2
        );

        // 低 revision：stale skip，且 facts 不回退（A18/A19 语义）。
        // Lower revision: stale skip, facts never roll back (A18/A19 semantics).
        assert_eq!(
            kernel
                .admit_compile(&p1, &ctx(), &policy(), &schema(), false)
                .unwrap(),
            Admission::Skipped
        );
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT source_revision AS n FROM facts WHERE entity_id='milk-tea:drink:boba' AND field_name='price'"
            ),
            2
        );

        // 新配置 epoch fence 旧 worker：t2 领取后新配置 admit → epoch+1、计数归零，
        // 旧 lease 的 publish/heartbeat 全部失效（§7.3/§8.3）。
        // New config fences the old worker via epoch: after t2 is claimed, a new
        // config admit bumps the epoch and resets counters; the old lease's
        // publish/heartbeat all fail (§7.3/§8.3).
        let lease_old = kernel.claim_compile(&[t2], "run-1", 1000).unwrap().unwrap();
        let mut policy2 = policy();
        policy2.scorer_version = "rules-v2".into();
        let p2v2 = prepared_with(2, &policy2);
        assert_eq!(
            kernel
                .admit_compile(&p2v2, &ctx(), &policy2, &schema(), false)
                .unwrap(),
            Admission::Queued(t2)
        );
        let t = task_state(&kernel, t2);
        assert_eq!(t.epoch, 2);
        assert_eq!(t.attempt_count, 0, "epoch reset");
        assert_eq!(t.retry_count, 0);
        assert_eq!(t.recompile_count, 0);
        assert!(t.result.is_none());
        assert!(!kernel.heartbeat_compile(&lease_old, 1000).unwrap());
        let (page, report) = accepted_page_with(&p2v2, &policy2);
        assert_eq!(
            kernel
                .publish_compile(&lease_old, &page, &report, None, 1000)
                .unwrap(),
            CommitOutcome::Stale
        );
        // 新 epoch 领取 + 发布成功。
        // The new epoch claims and publishes successfully.
        let lease_new = kernel.claim_compile(&[t2], "run-1", 1000).unwrap().unwrap();
        assert_eq!(lease_new.epoch, 2);
        assert_eq!(
            kernel
                .publish_compile(&lease_new, &page, &report, None, 1000)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
    }

    // 预算熔断（§8.4）：task/run 任一额度不足 → 任务保持 pending、无预留、无租约。
    // Budget circuit breaker (§8.4): insufficient task/run budget → the task stays
    // pending with no reservation and no lease.
    #[test]
    fn claim_defers_on_budget_shortage() {
        let kernel = SqliteKernel::open_in_memory().unwrap();

        // task 级：B 必超 task_token_budget=16。
        // Task level: B always exceeds task_token_budget=16.
        let mut small_task = policy();
        small_task.task_token_budget = 16;
        let p = prepared_with(1, &small_task);
        let task_id = queued_with(&kernel, &p, &small_task, false);
        assert!(kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .is_none());
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "pending");
        assert_eq!(t.reserved_tokens, 0);

        // run 级：batch_token_budget=16 → 新任务（同 hash 合并不改行内预算）带
        // 冻结 batch 预算 16，run 行按其创建后 claim 仍 None。
        // Run level: batch_token_budget=16 → a fresh task (a same-hash merge keeps
        // the row budgets) carries the frozen batch budget 16; the run row is
        // created from it and claim still yields None.
        let mut small_run = policy();
        small_run.batch_token_budget = 16;
        let p2 = prepared_with(2, &small_run);
        let task2 = queued_with(&kernel, &p2, &small_run, false);
        assert!(kernel
            .claim_compile(&[task2], "run-1", 1000)
            .unwrap()
            .is_none());
        assert_eq!(task_state(&kernel, task2).reserved_tokens, 0);
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT token_limit AS n FROM compile_runs WHERE run_id='run-1'"
            ),
            16,
            "run row created with the task's frozen batch budget"
        );
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT reserved_tokens AS n FROM compile_runs WHERE run_id='run-1'"
            ),
            0
        );

        // 新 run 无限额约束 → 可领取，预留同时入 task/run 账。（head 已在 rev2，
        // 更低 revision 会被 stale skip，故用 rev3 的新任务。）
        // A fresh run has no limiting row → claimable; reservations hit task/run.
        // (The head is at rev2 already — a lower revision would stale-skip, so use
        // a new rev3 task.)
        let p3 = prepared_with(3, &policy());
        let task3 = queued_with(&kernel, &p3, &policy(), false);
        let lease = kernel
            .claim_compile(&[task3], "run-2", 1000)
            .unwrap()
            .unwrap();
        assert!(task_state(&kernel, task3).reserved_tokens > 0);
        assert!(
            scalar_i64(
                &kernel,
                "SELECT reserved_tokens AS n FROM compile_runs WHERE run_id='run-2'"
            ) > 0
        );
        let _ = lease;
    }

    // preflight 隔离（§8.3）：合成 attempt_no=0、不占 token、任务 dead/quarantined。
    // Preflight quarantine (§8.3): synthetic attempt_no=0, no tokens, task
    // dead/quarantined.
    #[test]
    fn preflight_quarantine_synthesizes_attempt_zero() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        assert_eq!(
            kernel
                .quarantine_compile_preflight(task_id, 1, "EMPTY_KNOWLEDGE", 1000)
                .unwrap(),
            FailureDisposition::Quarantined
        );
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "dead");
        assert_eq!(t.result.as_deref(), Some("quarantined"));
        assert_eq!(t.reserved_tokens, 0, "preflight consumes no tokens");
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE attempt_no=0 AND publish_status='quarantined'"
            ),
            1
        );
        assert!(kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .is_none());
    }

    // §8.3 回收的 superseded 分支：新 source head 使过期任务 succeeded/superseded，
    // 不发布。
    // The superseded branch of recovery (§8.3): a moved source head finalizes the
    // expired task as succeeded/superseded without publishing.
    #[test]
    fn recover_marks_superseded_when_head_moved() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p1 = prepared(1);
        let task_id = queued(&kernel, &p1);
        let _lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();

        // 新 revision + 新配置推进 head（desired_hash 变化才会 supersede）。
        // A new revision with a new config moves the head (only a desired_hash
        // change supersedes).
        let mut policy2 = policy();
        policy2.scorer_version = "rules-v2".into();
        let p2 = prepared_with(2, &policy2);
        let _ = queued_with(&kernel, &p2, &policy2, false);

        let recovery = kernel.recover_compile_leases(1301).unwrap();
        assert_eq!(recovery.superseded, 1);
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "succeeded");
        assert_eq!(t.result.as_deref(), Some("superseded"));
        assert_eq!(count(&kernel, "pages"), 0);
    }

    // 传输失败语义（A12 基础）：Retryable 消耗 retry_count、退避重排；耗尽 →
    // dead/failed；Permanent 一次终态 → failed/failed；均不动 recompile_count。
    // Transport-failure semantics (A12 groundwork): Retryable consumes
    // retry_count and requeues with backoff; exhaustion → dead/failed; Permanent
    // terminates once as failed/failed; neither touches recompile_count.
    #[test]
    fn transport_failures_count_retries_only() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);

        // 前两次可重试 → RetryAt（退避 1s/2s）；第三次 → dead/failed。
        // First two retryable failures → RetryAt (backoffs 1s/2s); the third →
        // dead/failed.
        for now in [1000i64, 1001, 1003] {
            let lease = kernel
                .claim_compile(&[task_id], "run-1", now)
                .unwrap()
                .unwrap();
            let disposition = kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::Retryable {
                        code: "TIMEOUT".into(),
                        retry_after_seconds: None,
                    },
                    None,
                    &ScoreReport {
                        quality: QualityScore {
                            coverage: 0.0,
                            citation: 0.0,
                            schema_compliance: 0.0,
                            density: 0.0,
                            consistency: None,
                        },
                        issues: vec![],
                        accepted: false,
                    },
                    None,
                    now,
                )
                .unwrap();
            if now < 1003 {
                assert!(matches!(disposition, FailureDisposition::RetryAt(_)));
            } else {
                assert_eq!(disposition, FailureDisposition::Failed);
            }
        }
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "dead");
        assert_eq!(t.result.as_deref(), Some("failed"));
        assert_eq!(t.retry_count, 3);
        assert_eq!(
            t.recompile_count, 0,
            "transport failures never consume recompiles"
        );

        // Permanent 首次即 failed/failed。
        // Permanent terminates immediately as failed/failed.
        let p2 = prepared(2);
        let task2 = queued(&kernel, &p2);
        let lease2 = kernel
            .claim_compile(&[task2], "run-1", 2000)
            .unwrap()
            .unwrap();
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease2,
                    &CompileFailure::Permanent {
                        code: "TLS_ERROR".into()
                    },
                    None,
                    &ScoreReport {
                        quality: QualityScore {
                            coverage: 0.0,
                            citation: 0.0,
                            schema_compliance: 0.0,
                            density: 0.0,
                            consistency: None
                        },
                        issues: vec![],
                        accepted: false,
                    },
                    None,
                    2000,
                )
                .unwrap(),
            FailureDisposition::Failed
        );
        let t2 = task_state(&kernel, task2);
        assert_eq!(t2.status, "failed");
        assert_eq!(t2.result.as_deref(), Some("failed"));
    }

    // ===== Step8 批 B3：A9/A10（死信与一致性审核原子接入）=====
    // ===== Step8 batch B3: A9/A10 (atomic dead-letter and consistency-review
    // wiring) =====

    use crate::compile::consistency::{
        ClaimKey, ConsistencyFinding, CONSISTENCY_CONFLICT, CONSISTENCY_SCORE, VALUE_DIVERGENCE,
    };

    #[derive(QueryableByName)]
    struct ReviewProbe {
        #[diesel(sql_type = diesel::sql_types::Text)]
        subject_json: String,
        #[diesel(sql_type = diesel::sql_types::Text)]
        reason_json: String,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
        compile_task_id: Option<i64>,
    }

    fn reviews(kernel: &SqliteKernel, action: &str) -> Vec<ReviewProbe> {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "SELECT review_id, subject_json, reason_json, compile_task_id
             FROM review_queue WHERE action = ? ORDER BY review_id",
        )
        .bind::<diesel::sql_types::Text, _>(action)
        .load(&mut *conn)
        .unwrap()
    }

    fn attempt_consistency_json(kernel: &SqliteKernel, task_id: i64, attempt_no: i64) -> String {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "SELECT consistency_json AS value FROM compile_attempts
             WHERE task_id = ? AND attempt_no = ? ORDER BY epoch DESC LIMIT 1",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .bind::<diesel::sql_types::BigInt, _>(attempt_no)
        .get_result::<TextOnlyRow>(&mut *conn)
        .unwrap()
        .value
    }

    /// 一条 VALUE_DIVERGENCE 冲突报告（哈希为固定 64 位 hex 占位）。
    /// One VALUE_DIVERGENCE conflict report (fixed 64-char hex hash placeholders).
    fn conflict_report() -> ConsistencyReport {
        ConsistencyReport {
            score: Some(0.0),
            compared_claims: 1,
            findings: vec![ConsistencyFinding {
                code: VALUE_DIVERGENCE.into(),
                key: ClaimKey {
                    entity_id: "milk-tea:ingredient:pearl".into(),
                    pointer: "/fields/name".into(),
                },
                candidate_value_hash: "a".repeat(64),
                evidence_value_hash: "b".repeat(64),
            }],
            candidate_count: 1,
        }
    }

    // A9：max_recompiles=0 首个低质候选即刹车到顶 → dead/quarantined；同一事务
    // 一条 compile_dead_letter（subject 恰为 {"task_id":N}、compile_task_id 回填）
    // + 一条 consistency_conflict；attempt consistency_json 与死信 reason 为同一
    // 份 canonical 冲突诊断（§5.2：code/task 身份/findings[key={entity_id,pointer},
    // old_hash=证据摘要,new_hash=候选摘要]）。
    // A9: with max_recompiles=0 the first low-quality candidate trips the brake →
    // dead/quarantined; the same transaction holds one compile_dead_letter
    // (subject exactly {"task_id":N}, compile_task_id backfilled) plus one
    // consistency_conflict; the attempt's consistency_json and the dead-letter
    // reason are the same canonical conflict diagnostic (§5.2:
    // code/task-identity/findings[key={entity_id,pointer},
    // old_hash=evidence digest, new_hash=candidate digest]).
    #[test]
    fn a9_conflict_brake_enqueues_dead_letter_and_consistency_review_atomically() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let mut p0 = policy();
        p0.max_recompiles = 0;
        let p = prepared_with(1, &p0);
        let task_id = queued_with(&kernel, &p, &p0, false);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (low, low_report) = low_quality_page(&p, &p0);
        let conflict = conflict_report();
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    Some(&conflict),
                    1000
                )
                .unwrap(),
            FailureDisposition::Quarantined
        );
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "dead");
        assert_eq!(t.result.as_deref(), Some("quarantined"));

        // —— A9：一条死信 + 一条一致性冲突审核，subject/回填精确 ——
        // —— A9: one dead letter + one consistency-conflict review with exact
        //    subject/backfill ——
        let dead = reviews(&kernel, "compile_dead_letter");
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].subject_json, format!(r#"{{"task_id":{task_id}}}"#));
        assert_eq!(dead[0].compile_task_id, Some(task_id));
        let conflict_rows = reviews(&kernel, "consistency_conflict");
        assert_eq!(conflict_rows.len(), 1);
        assert_eq!(conflict_rows[0].compile_task_id, Some(task_id));

        // reason = canonical 冲突诊断（§5.2 形状；旧/新值仅哈希）。
        // reason = the canonical conflict diagnostic (§5.2 shape; digests only).
        let reason: serde_json::Value =
            serde_json::from_str(&conflict_rows[0].reason_json).unwrap();
        assert_eq!(reason["code"], CONSISTENCY_CONFLICT);
        assert_eq!(reason["task_id"], task_id);
        assert_eq!(reason["epoch"], 1);
        assert_eq!(reason["attempt_no"], 1);
        assert_eq!(reason["entity_id"], "milk-tea:drink:boba");
        let finding = &reason["findings"][0];
        assert_eq!(finding["key"]["entity_id"], "milk-tea:ingredient:pearl");
        assert_eq!(finding["key"]["pointer"], "/fields/name");
        assert_eq!(finding["old_hash"], "b".repeat(64));
        assert_eq!(finding["new_hash"], "a".repeat(64));
        assert_eq!(conflict_rows[0].reason_json, dead[0].reason_json);

        // attempt consistency_json 与 reason 同一份；无 64 KiB 越界。
        // The attempt consistency_json equals the reason; within the 64 KiB cap.
        assert_eq!(
            attempt_consistency_json(&kernel, task_id, 1),
            conflict_rows[0].reason_json
        );

        // —— A10：同 lease 重复 finish 被 fencing 拦截 → 不产生第二行 ——
        // —— A10: a repeated finish with the same lease is fenced → no second
        //    row ——
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    Some(&conflict),
                    1001
                )
                .unwrap(),
            FailureDisposition::Failed
        );
        assert_eq!(reviews(&kernel, "compile_dead_letter").len(), 1);
        assert_eq!(reviews(&kernel, "consistency_conflict").len(), 1);
    }

    // A9（无冲突对照）：刹车到顶但本轮无一致性 findings → 仅死信一条（reason
    // code = 失败码、findings 空），无 consistency_conflict 行；attempt
    // consistency_json 保持 '{}'。
    // A9 (no-conflict control): brake exhausted without this round's consistency
    // findings → exactly one dead letter (reason code = the failure code with
    // empty findings), no consistency_conflict row; the attempt's
    // consistency_json stays '{}'.
    #[test]
    fn a9_brake_without_conflict_enqueues_dead_letter_only() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let mut p0 = policy();
        p0.max_recompiles = 0;
        let p = prepared_with(1, &p0);
        let task_id = queued_with(&kernel, &p, &p0, false);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (low, low_report) = low_quality_page(&p, &p0);
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    None,
                    1000
                )
                .unwrap(),
            FailureDisposition::Quarantined
        );
        let dead = reviews(&kernel, "compile_dead_letter");
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].compile_task_id, Some(task_id));
        let reason: serde_json::Value = serde_json::from_str(&dead[0].reason_json).unwrap();
        assert_eq!(reason["code"], "LOW_DENSITY");
        assert_eq!(reason["task_id"], task_id);
        assert_eq!(reason["findings"], serde_json::json!([]));
        assert!(reviews(&kernel, "consistency_conflict").is_empty());
        assert_eq!(attempt_consistency_json(&kernel, task_id, 1), "{}");
    }

    // A10：传输耗尽（max_retries=1）→ dead/failed + 一条死信（code=传输错误码）；
    // 重复 finish 不加行；enqueue_dead_letter_on_conn 直接连插两次 DO NOTHING 仍
    // 一行；回收 dead 分支同样一条死信且重复回收（0 行）不重复插。
    // A10: transport exhaustion (max_retries=1) → dead/failed with one dead
    // letter (code = the transport code); repeated finishes add no rows; calling
    // enqueue_dead_letter_on_conn twice directly still yields one row via
    // DO NOTHING; the recovery dead branch likewise enqueues once and repeated
    // recovery (0 rows) never re-inserts.
    #[test]
    fn a10_dead_letter_is_idempotent_across_finish_enqueue_and_recover() {
        let kernel = SqliteKernel::open_in_memory().unwrap();

        // —— 传输耗尽 → 死信一条 ——
        // —— Transport exhaustion → one dead letter ——
        let mut p0 = policy();
        p0.max_retries = 1;
        let p = prepared_with(1, &p0);
        let task_id = queued_with(&kernel, &p, &p0, false);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::Retryable {
                        code: "TIMEOUT".into(),
                        retry_after_seconds: None,
                    },
                    None,
                    &ScoreReport {
                        quality: QualityScore {
                            coverage: 0.0,
                            citation: 0.0,
                            schema_compliance: 0.0,
                            density: 0.0,
                            consistency: None,
                        },
                        issues: vec![],
                        accepted: false,
                    },
                    None,
                    1000
                )
                .unwrap(),
            FailureDisposition::Failed
        );
        assert_eq!(task_state(&kernel, task_id).status, "dead");
        let dead = reviews(&kernel, "compile_dead_letter");
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].subject_json, format!(r#"{{"task_id":{task_id}}}"#));
        assert_eq!(dead[0].compile_task_id, Some(task_id));
        let reason: serde_json::Value = serde_json::from_str(&dead[0].reason_json).unwrap();
        assert_eq!(reason["code"], "TIMEOUT");
        assert!(reviews(&kernel, "consistency_conflict").is_empty());

        // 重复 finish（fencing）不加行。
        // A repeated finish (fenced) adds no rows.
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::Retryable {
                        code: "TIMEOUT".into(),
                        retry_after_seconds: None,
                    },
                    None,
                    &ScoreReport {
                        quality: QualityScore {
                            coverage: 0.0,
                            citation: 0.0,
                            schema_compliance: 0.0,
                            density: 0.0,
                            consistency: None,
                        },
                        issues: vec![],
                        accepted: false,
                    },
                    None,
                    1001
                )
                .unwrap(),
            FailureDisposition::Failed
        );
        assert_eq!(reviews(&kernel, "compile_dead_letter").len(), 1);

        // DO NOTHING 直证：同 (domain,action,subject) 连插两次仍一行。
        // Direct DO NOTHING proof: two back-to-back inserts on the same
        // (domain,action,subject) still yield one row.
        {
            let mut conn = kernel.lock_conn().unwrap();
            conn.immediate_transaction(|tx| {
                enqueue_dead_letter_on_conn(tx, task_id, "milk-tea", 1, r#"{"code":"X"}"#, 2000)?;
                enqueue_dead_letter_on_conn(tx, task_id, "milk-tea", 1, r#"{"code":"X"}"#, 2000)
            })
            .unwrap();
        }
        assert_eq!(reviews(&kernel, "compile_dead_letter").len(), 1);

        // —— 回收 dead 分支：一条死信；重复回收不重复插 ——
        // —— The recovery dead branch: one dead letter; repeated recovery never
        //    re-inserts ——
        let mut p1 = policy();
        p1.max_retries = 1;
        let p2 = prepared_with(2, &p1);
        let task2 = queued_with(&kernel, &p2, &p1, false);
        let _ = kernel
            .claim_compile(&[task2], "run-1", 1000)
            .unwrap()
            .unwrap();
        let recovery = kernel.recover_compile_leases(1301).unwrap();
        assert_eq!(recovery.dead_failed, 1);
        assert_eq!(recovery.review_inserted, 1);
        assert_eq!(task_state(&kernel, task2).status, "dead");
        let dead_after_recover = reviews(&kernel, "compile_dead_letter");
        assert_eq!(dead_after_recover.len(), 2, "one row per dead task");
        let recovered = dead_after_recover
            .iter()
            .find(|r| r.compile_task_id == Some(task2))
            .unwrap();
        assert_eq!(recovered.subject_json, format!(r#"{{"task_id":{task2}}}"#));
        let reason2: serde_json::Value = serde_json::from_str(&recovered.reason_json).unwrap();
        assert_eq!(reason2["code"], "LEASE_EXPIRED_RETRY_EXHAUSTED");
        assert_eq!(
            kernel.recover_compile_leases(1301).unwrap(),
            RecoveryStats::default()
        );
        assert_eq!(reviews(&kernel, "compile_dead_letter").len(), 2);
    }

    // Step8 批 B3（publish 侧）：consistency Some → page_quality.consistency 精确
    // 落库、attempt consistency_json 为发布诊断（code=CONSISTENCY_SCORE + 计数）、
    // load_accepted_pages 往返读回；consistency None → SQL NULL + '{}'。
    // Step8 batch B3 (publish side): a Some consistency lands the exact
    // page_quality.consistency value, the attempt consistency_json carries the
    // publish diagnostic (code=CONSISTENCY_SCORE plus counts), and
    // load_accepted_pages round-trips it; None → SQL NULL + '{}'.
    #[test]
    fn publish_writes_consistency_value_and_json() {
        // —— Some(1.0)：五维值与发布诊断精确落库 ——
        // —— Some(1.0): the five-dim value and publish diagnostic land exactly ——
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let p = prepared(1);
        let task_id = queued(&kernel, &p);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (page, _) = accepted_page(&p);
        let consistency = ConsistencyReport {
            score: Some(1.0),
            compared_claims: 2,
            findings: vec![],
            candidate_count: 3,
        };
        // 报告经五维组合器产生（与 executor 管线同源：quality.consistency =
        // 仲裁得分，accepted=true）。
        // The report comes from the five-dim combiner (same source as the
        // executor pipeline: quality.consistency = the arbitration score,
        // accepted=true).
        let refs =
            DefaultSourceRefValidator.validate(&p.knowledge, page.evidence.as_ref().unwrap(), true);
        let report = RuleBasedScorer.score_with_consistency(
            &p.knowledge,
            Some(&page),
            &refs,
            true,
            &consistency,
            &ctx(),
            &policy(),
        );
        assert!(report.accepted, "fixture page must be accepted");
        assert_eq!(
            kernel
                .publish_compile(&lease, &page, &report, Some(&consistency), 1000)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT CAST(consistency * 1000000 AS INTEGER) AS n FROM page_quality
                 WHERE page_id = 'milk-tea:drink:boba'"
            ),
            1_000_000,
            "the consistency value lands exactly"
        );
        let published: serde_json::Value =
            serde_json::from_str(&attempt_consistency_json(&kernel, task_id, 1)).unwrap();
        assert_eq!(published["code"], CONSISTENCY_SCORE);
        assert_eq!(published["task_id"], task_id);
        assert_eq!(published["score"], 1.0);
        assert_eq!(published["compared_claims"], 2);
        assert_eq!(published["candidate_count"], 3);
        assert_eq!(
            kernel.load_accepted_pages("milk-tea").unwrap()[0]
                .quality
                .consistency,
            Some(1.0),
            "the value round-trips through load_accepted_pages"
        );

        // —— None：SQL NULL + '{}'（既有 Step4 行为不变）——
        // —— None: SQL NULL + '{}' (the existing Step4 behavior unchanged) ——
        let kernel2 = SqliteKernel::open_in_memory().unwrap();
        let p2 = prepared(1);
        let task2 = queued(&kernel2, &p2);
        let lease2 = kernel2
            .claim_compile(&[task2], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (page2, report2) = accepted_page(&p2);
        assert_eq!(
            kernel2
                .publish_compile(&lease2, &page2, &report2, None, 1000)
                .unwrap(),
            CommitOutcome::Accepted { generation: 1 }
        );
        assert_eq!(
            scalar_i64(
                &kernel2,
                "SELECT COUNT(*) AS n FROM page_quality WHERE consistency IS NULL"
            ),
            1,
            "None stays SQL NULL"
        );
        assert_eq!(attempt_consistency_json(&kernel2, task2, 1), "{}");
        assert_eq!(
            kernel2.load_accepted_pages("milk-tea").unwrap()[0]
                .quality
                .consistency,
            None
        );
    }

    // ===== Step8 批 B5（A13/A14/A15 前置）：兼容 preflight 读取面 =====
    // ===== Step8 batch B5 (A13/A14/A15 prerequisites): the
    // compatibility-preflight read surface =====

    /// B5 矩阵：§5.1 示例形状（artifact 只认 wiki-v2）。
    /// The B5 matrix: the §5.1 sample shape (artifact admits wiki-v2 only).
    fn b5_spec() -> CompatibilitySpec {
        serde_yaml_ng::from_str(
            "domain_pack: \">=1.0.0,<2.0.0\"\n\
             schema: \">=2.0.0,<3.0.0\"\n\
             prompt: \">=3.0.0,<4.0.0\"\n\
             artifact: [\"wiki-v2\"]",
        )
        .unwrap()
    }

    /// 直插 accepted/candidate/quarantined 页（绕过发布管线，聚焦读取面过滤）。
    /// Plants a page directly (bypassing the publish pipeline, focusing the read
    /// surface's filters).
    fn plant_page(
        kernel: &SqliteKernel,
        page_id: &str,
        domain: &str,
        status: &str,
        domain_pack_version: &str,
        artifact_version: &str,
        frontmatter_json: &str,
    ) {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                                content_hash, generation, status, domain_pack_version,
                                compiled_at, model_version, embedding_model,
                                artifact_version, frontmatter_json, created_at, updated_at)
             VALUES (?, ?, ?, 'drink', 't', 'c', 'h', 1, ?, ?, 0, 'mock-v1', 'none', ?, ?, 0, 0)",
        )
        .bind::<diesel::sql_types::Text, _>(page_id)
        .bind::<diesel::sql_types::Text, _>(format!("{domain}:drink:x"))
        .bind::<diesel::sql_types::Text, _>(domain)
        .bind::<diesel::sql_types::Text, _>(status)
        .bind::<diesel::sql_types::Text, _>(domain_pack_version)
        .bind::<diesel::sql_types::Text, _>(artifact_version)
        .bind::<diesel::sql_types::Text, _>(frontmatter_json)
        .execute(&mut *conn)
        .unwrap();
    }

    /// 直插任务行（依赖快照由调用方给足；status 默认 pending）。
    /// Plants a task row directly (the caller supplies the dependency snapshot;
    /// status defaults to pending).
    fn plant_task(kernel: &SqliteKernel, entity_id: &str, deps_json: &str) -> i64 {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "INSERT INTO compile_tasks
                (entity_id, source_revision, domain_pack_version, dependencies_json,
                 created_at, updated_at)
             VALUES (?, 1, '1.2.0', ?, 0, 0)
             RETURNING task_id",
        )
        .bind::<diesel::sql_types::Text, _>(entity_id)
        .bind::<diesel::sql_types::Text, _>(deps_json)
        .get_result::<TaskIdRow>(&mut *conn)
        .unwrap()
        .task_id
    }

    fn set_task_status(kernel: &SqliteKernel, task_id: i64, status: &str) {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query("UPDATE compile_tasks SET status = ? WHERE task_id = ?")
            .bind::<diesel::sql_types::Text, _>(status)
            .bind::<diesel::sql_types::BigInt, _>(task_id)
            .execute(&mut *conn)
            .unwrap();
    }

    /// 与 compatibility.rs 测试同一 deps 形状（context/policy 双段；B1 旧形状
    /// policy，serde default 兜底 Step8 字段）。
    /// The same deps shape as the compatibility.rs tests (both the context and
    /// policy sections; a B1 legacy-shape policy whose Step8 fields fall back to
    /// serde defaults).
    fn b5_deps_json(
        domain_pack_version: &str,
        schema_version: Option<&str>,
        prompt_version: Option<&str>,
        artifact_version: &str,
    ) -> String {
        serde_json::to_string(&serde_json::json!({
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

    // 读取面：页按 domain+accepted 过滤；任务按 pending/running/dead 过滤；
    // has_existing_compile_data 的全局 fail-closed 判定（§7 skip 语义前置）。
    // Read surfaces: pages filtered by domain+accepted; tasks filtered by
    // pending/running/dead; the global fail-closed verdict of
    // has_existing_compile_data (the §7 skip-semantics prerequisite).
    #[test]
    fn b5_read_surfaces_scope_and_status_filters() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        // 空库：无旧 artifact，has_existing=false（§7 skip 允许面）。
        // Empty database: no old artifact, has_existing=false (the §7 skip
        // allowance).
        assert!(!kernel.has_existing_compile_data().unwrap());
        assert!(kernel
            .compatibility_page_identities("milk-tea")
            .unwrap()
            .is_empty());
        assert!(kernel.compatibility_task_snapshots().unwrap().is_empty());

        plant_page(
            &kernel,
            "p-own",
            "milk-tea",
            "accepted",
            "1.2.0",
            "wiki-v2",
            r#"{"quality_policy":{"artifact_version":"wiki-v2"}}"#,
        );
        plant_page(
            &kernel,
            "p-other",
            "other-tea",
            "accepted",
            "9.9.9",
            "wiki-x",
            "{}",
        );
        // quarantined 页不是当前产物，不入检查面。
        // A quarantined page is not a current artifact and stays outside the
        // check surface.
        plant_page(
            &kernel,
            "p-quarantined",
            "milk-tea",
            "quarantined",
            "9.9.9",
            "wiki-x",
            "{}",
        );
        let pages = kernel.compatibility_page_identities("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_id, "p-own");
        assert_eq!(pages[0].domain_pack_version, "1.2.0");
        assert_eq!(pages[0].artifact_version, "wiki-v2");
        assert!(pages[0].frontmatter_json.contains("quality_policy"));
        // 任意 accepted 页存在 → has_existing=true（全局口径）。
        // Any accepted page → has_existing=true (the global reading).
        assert!(kernel.has_existing_compile_data().unwrap());

        // 任务：pending/running/dead 都在面内；succeeded/他域不算当前产物。
        // Tasks: pending/running/dead are all in-surface; succeeded/other-domain
        // are not current artifacts.
        let t1 = plant_task(
            &kernel,
            "milk-tea:drink:boba",
            &b5_deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2"),
        );
        let t2 = plant_task(
            &kernel,
            "milk-tea:drink:lemon",
            &b5_deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2"),
        );
        let t3 = plant_task(
            &kernel,
            "milk-tea:drink:cheese",
            &b5_deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2"),
        );
        let t4 = plant_task(
            &kernel,
            "milk-tea:drink:cocoa",
            &b5_deps_json("1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2"),
        );
        let other = plant_task(
            &kernel,
            "other-tea:drink:boba",
            &b5_deps_json("0.1.0", None, None, "seed-v1"),
        );
        set_task_status(&kernel, t2, "running");
        set_task_status(&kernel, t3, "dead");
        set_task_status(&kernel, t4, "succeeded");
        let tasks = kernel.compatibility_task_snapshots().unwrap();
        let mut ids: Vec<i64> = tasks.iter().map(|t| t.task_id).collect();
        ids.sort_unstable();
        // succeeded 出面；他域 live 行保留在快照面内（域过滤在 checker 侧）。
        // succeeded leaves the surface; other-domain live rows stay in the
        // snapshot surface (domain filtering happens checker-side).
        assert_eq!(ids, vec![t1, t2, t3, other]);
        assert!(tasks
            .iter()
            .all(|t| t.dependencies_json.contains("context")));
    }

    // 检查器全量扫描（A14）：本域页/任务违规全部出现、计数准确、损坏 JSON 为
    // violation（不静默跳过）、他域行不进本域检查面。
    // Checker full scan (A14): every in-domain page/task violation appears,
    // counts are exact, corrupt JSON is a violation (never silently skipped),
    // and other-domain rows stay outside this domain's check surface.
    #[test]
    fn b5_checker_full_scan_violations_counts_and_domain_scope() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        // 本域页：dpv 越界 + artifact 列外 + 无 quality_policy → 3 条违规。
        // In-domain page: dpv out of range + artifact outside the list + no
        // quality_policy → 3 violations.
        plant_page(
            &kernel, "p-1", "milk-tea", "accepted", "0.9.9", "seed-v1", "{}",
        );
        // 他域页不进本域检查面。
        // Other-domain pages stay outside this domain's check surface.
        plant_page(
            &kernel,
            "p-2",
            "other-tea",
            "accepted",
            "0.9.9",
            "seed-v1",
            "{}",
        );
        // 本域 pending 任务：legacy deps（schema/prompt 缺失）→ 2 条 MISSING。
        // In-domain pending task: legacy deps (schema/prompt absent) → 2
        // MISSING violations.
        plant_task(
            &kernel,
            "milk-tea:drink:boba",
            &b5_deps_json("1.2.0", None, None, "wiki-v2"),
        );
        // 损坏快照 → CORRUPT_SNAPSHOT（A14 不静默跳过）。
        // A corrupt snapshot → CORRUPT_SNAPSHOT (A14: never silently skipped).
        plant_task(&kernel, "milk-tea:drink:cheese", "not json at all");
        // 他域任务（缺版本也不报）——矩阵按域生效。
        // An other-domain task (its missing versions are not reported) — the
        // matrix applies per domain.
        plant_task(
            &kernel,
            "other-tea:drink:boba",
            &b5_deps_json("0.1.0", None, None, "seed-v1"),
        );

        let identity =
            DomainIdentity::parse("milk-tea", "1.2.0", Some("2.1.0"), Some("3.0.0"), "wiki-v2")
                .unwrap();
        let report = StandardCompatibilityChecker::new()
            .check(&identity, &b5_spec(), &kernel)
            .unwrap();
        assert!(!report.compatible);
        assert_eq!(report.checked_pages, 1, "only this domain's accepted pages");
        assert_eq!(
            report.checked_tasks, 2,
            "only this domain's live/dead tasks"
        );
        let mut hits: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|v| (v.field.as_str(), v.code.as_str()))
            .collect();
        hits.sort_unstable();
        assert_eq!(
            hits,
            vec![
                ("artifact_version", COMPATIBILITY_ARTIFACT_NOT_ALLOWED),
                ("dependencies_json", COMPATIBILITY_CORRUPT_SNAPSHOT),
                ("domain_pack_version", COMPATIBILITY_VERSION_RANGE),
                ("prompt_version", COMPATIBILITY_VERSION_MISSING),
                (
                    "quality_policy.artifact_version",
                    COMPATIBILITY_VERSION_MISSING
                ),
                ("schema_version", COMPATIBILITY_VERSION_MISSING),
            ]
        );
    }

    // 兼容告警入队（A16 前置）：UNIQUE(domain,action,subject_json) DO NOTHING
    // 幂等；compile_task_id 恒 NULL；非对象 subject / 非法 reason → Validation
    // 零行落库。
    // Compatibility-alert enqueue (the A16 prerequisite): UNIQUE
    // (domain,action,subject_json) DO NOTHING idempotency; compile_task_id stays
    // NULL; a non-object subject / illegal reason → Validation with zero rows.
    #[test]
    fn b5_compatibility_conflict_review_is_idempotent_and_validated() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let subject = r#"{"domain":"milk-tea","domain_pack_version":"1.2.0"}"#;
        let inserted = kernel
            .insert_compatibility_conflict_review("milk-tea", subject, r#"{"code":"X"}"#, 7)
            .unwrap();
        assert_eq!(inserted, 1);
        let again = kernel
            .insert_compatibility_conflict_review("milk-tea", subject, r#"{"code":"X"}"#, 8)
            .unwrap();
        assert_eq!(again, 0, "repeated runs never add a second alert");
        let other = kernel
            .insert_compatibility_conflict_review(
                "milk-tea",
                r#"{"domain":"milk-tea","domain_pack_version":"2.0.0"}"#,
                r#"{"code":"X"}"#,
                9,
            )
            .unwrap();
        assert_eq!(other, 1, "a new subject is a new alert");

        let rows = reviews(&kernel, "compatibility_conflict");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.compile_task_id.is_none()));

        // 非 JSON 对象 subject → Validation（零行落库）。
        // A non-object subject → Validation (zero rows written).
        let err = kernel
            .insert_compatibility_conflict_review("milk-tea", "[]", r#"{}"#, 10)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        assert_eq!(reviews(&kernel, "compatibility_conflict").len(), 2);
        // 非法 reason JSON → Validation。
        // An illegal reason JSON → Validation.
        let err = kernel
            .insert_compatibility_conflict_review("milk-tea", r#"{}"#, "not json", 11)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    // ===== Step8 批 B7：故障注入（§8 事务原子性）=====
    // ===== Step8 batch B7: fault injection (§8 transaction atomicity) =====
    //
    // 注入手段：SQL 级 RAISE(ABORT) 触发器，不引入故障开关库。触发器中段制造
    // 语句失败 → diesel 的 immediate_transaction 整体回滚；断言前置写入零残留。
    // Injection means: SQL-level RAISE(ABORT) triggers, no fault-toggle library.
    // A mid-transaction statement failure makes diesel's immediate_transaction
    // roll the whole thing back; the assertions prove zero residue from earlier
    // writes in the same transaction.

    // §8/§7 admit 原子性：admit = 「facts CAS → 三元任务 upsert → source head
    // upsert」多步写事务。注入最后一步（head upsert）失败：facts/fact_refs/
    // tasks/heads 全部零残留——前置步骤不得提前提交或泄漏。
    // §8/§7 admit atomicity: admit is the multi-step write transaction "facts
    // CAS → triple task upsert → source head upsert". Injecting a failure at the
    // last step (the head upsert) leaves zero residue on facts/fact_refs/tasks/
    // heads — earlier steps never commit or leak.
    #[test]
    fn admit_mid_transaction_failure_rolls_back_everything() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        kernel
            .execute_batch(
                "CREATE TRIGGER inject_head_fail AFTER INSERT ON compile_source_heads
                 WHEN NEW.entity_id = 'milk-tea:drink:boba'
                 BEGIN SELECT RAISE(ABORT, 'injected: head upsert failure'); END;",
            )
            .unwrap();
        let p = prepared(1);
        let outcome = kernel.admit_compile(&p, &ctx(), &policy(), &schema(), false);
        assert!(
            outcome.is_err(),
            "a head-upsert failure must propagate, got {outcome:?}"
        );
        assert_eq!(count(&kernel, "compile_tasks"), 0);
        assert_eq!(count(&kernel, "compile_source_heads"), 0);
        assert_eq!(count(&kernel, "facts"), 0);
        assert_eq!(count(&kernel, "fact_refs"), 0);

        // 拆除注入后同一 admit 成功——核对故障确实来自注入点而非其它路径。
        // The same admit succeeds after dropping the injection — proving the
        // fault came from the injection point, not another path.
        kernel
            .execute_batch("DROP TRIGGER inject_head_fail;")
            .unwrap();
        assert!(queued(&kernel, &p) > 0);
    }

    // §8 质量终态原子性：finish_compile_failure 的 dead 分支 = 「attempt marks
    // completed → task dead/quarantined → 死信 + 一致性冲突审核」同一事务。注入
    // 死信插入失败：attempt 不完成、任务不终态、审核零行（两条审核一起回滚）。
    // §8 quality-terminal atomicity: the dead branch of finish_compile_failure is
    // one transaction over "attempt completed → task dead/quarantined → dead
    // letter + consistency-conflict reviews". Injecting a dead-letter insert
    // failure: the attempt does not complete, the task never reaches a terminal,
    // and zero review rows survive (both reviews roll back together).
    #[test]
    fn finish_failure_dead_branch_failure_rolls_back_terminal_atomically() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        kernel
            .execute_batch(
                "CREATE TRIGGER inject_dead_letter_fail AFTER INSERT ON review_queue
                 WHEN NEW.action = 'compile_dead_letter'
                 BEGIN SELECT RAISE(ABORT, 'injected: dead-letter insert failure'); END;",
            )
            .unwrap();
        let mut p0 = policy();
        p0.max_recompiles = 0;
        let p = prepared_with(1, &p0);
        let task_id = queued_with(&kernel, &p, &p0, false);
        let lease = kernel
            .claim_compile(&[task_id], "run-1", 1000)
            .unwrap()
            .unwrap();
        let (low, low_report) = low_quality_page(&p, &p0);
        let conflict = conflict_report();
        let err = kernel
            .finish_compile_failure(
                &lease,
                &CompileFailure::invalid("LOW_DENSITY", "{}"),
                Some(&low),
                &low_report,
                Some(&conflict),
                1000,
            )
            .unwrap_err();
        assert!(
            matches!(err, Error::Database(_) | Error::Internal(_)),
            "got {err:?}"
        );
        // 整事务回滚：任务保持 running + 原 lease、attempt 仍 reserved、审核零行。
        // The whole transaction rolled back: the task stays running with its
        // original lease, the attempt stays reserved, zero review rows.
        let t = task_state(&kernel, task_id);
        assert_eq!(t.status, "running");
        assert!(t.lease_token.is_some(), "lease must be retained");
        assert_eq!(count(&kernel, "review_queue"), 0);
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE status = 'reserved'"
            ),
            1
        );

        // 拆除注入后正常失败：dead 终态 + 一条死信 + 一条一致性冲突（对照）。
        // After dropping the injection the normal failure lands: dead terminal +
        // one dead letter + one consistency conflict (the control case).
        kernel
            .execute_batch("DROP TRIGGER inject_dead_letter_fail;")
            .unwrap();
        assert_eq!(
            kernel
                .finish_compile_failure(
                    &lease,
                    &CompileFailure::invalid("LOW_DENSITY", "{}"),
                    Some(&low),
                    &low_report,
                    Some(&conflict),
                    1000,
                )
                .unwrap(),
            FailureDisposition::Quarantined
        );
        assert_eq!(task_state(&kernel, task_id).status, "dead");
        assert_eq!(reviews(&kernel, "compile_dead_letter").len(), 1);
        assert_eq!(reviews(&kernel, "consistency_conflict").len(), 1);
    }

    // §8 回收终态原子性：recover 的每个过期任务在同一 BEGIN IMMEDIATE 内处理
    // （abandon → dead/pending + 死信）。注入死信插入失败：整个 pass 回滚——
    // 其它任务已做的 pending/退避更新也一并撤销，两个任务保持 running、原
    // lease/retry_count/attempt 计数全不变、零审核行。
    // §8 recovery-terminal atomicity: every expired task is handled inside the
    // same BEGIN IMMEDIATE (abandon → dead/pending + dead letter). Injecting a
    // dead-letter insert failure rolls back the whole pass — pending/backoff
    // updates for other tasks are undone too; both tasks stay running with their
    // original lease/retry_count/attempt counts and zero review rows.
    #[test]
    fn recover_mid_pass_failure_rolls_back_whole_pass_atomically() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        kernel
            .execute_batch(
                "CREATE TRIGGER inject_recover_dead_fail AFTER INSERT ON review_queue
                 WHEN NEW.action = 'compile_dead_letter'
                 BEGIN SELECT RAISE(ABORT, 'injected: recovery dead-letter failure'); END;",
            )
            .unwrap();
        // 任务 A：max_retries=1 → 回收走 dead/failed + 死信入队（触发注入）。
        // Task A: max_retries=1 → recovery takes the dead/failed branch with the
        // dead-letter insert (the injected trigger).
        let mut p_a = policy();
        p_a.max_retries = 1;
        let a = prepared_with(2, &p_a);
        let task_a = queued_with(&kernel, &a, &p_a, false);
        kernel
            .claim_compile(&[task_a], "run-1", 1000)
            .unwrap()
            .unwrap();
        // 任务 B：默认 max_retries=3 → 回收走 pending/退避分支（无审核插入）。
        // Task B: default max_retries=3 → recovery takes the pending/backoff
        // branch (no review insert).
        let b = prepared_with(3, &policy());
        let task_b = queued(&kernel, &b);
        kernel
            .claim_compile(&[task_b], "run-1", 1000)
            .unwrap()
            .unwrap();

        // 注入下回收（租约 1000+300=1300 已过期）→ 整个 pass 回滚。
        // Recovery under injection (leases expired at 1300) → the whole pass
        // rolls back.
        let err = kernel.recover_compile_leases(1301).unwrap_err();
        assert!(
            matches!(err, Error::Database(_) | Error::Internal(_)),
            "got {err:?}"
        );
        for id in [task_a, task_b] {
            let t = task_state(&kernel, id);
            assert_eq!(t.status, "running", "task {id} must stay running");
            assert!(t.lease_token.is_some(), "task {id} lease must be retained");
            assert_eq!(t.retry_count, 0, "task {id} retry must not be bumped");
        }
        assert_eq!(count(&kernel, "review_queue"), 0);
        assert_eq!(
            scalar_i64(
                &kernel,
                "SELECT COUNT(*) AS n FROM compile_attempts WHERE status = 'reserved'"
            ),
            2
        );

        // 拆除注入后正常回收：A dead/failed + 一条死信，B pending/退避。
        // After dropping the injection: A ends dead/failed with one dead letter,
        // B goes pending with backoff.
        kernel
            .execute_batch("DROP TRIGGER inject_recover_dead_fail;")
            .unwrap();
        let recovery = kernel.recover_compile_leases(1301).unwrap();
        assert_eq!(recovery.dead_failed, 1);
        assert_eq!(recovery.recovered_pending, 1);
        assert_eq!(recovery.review_inserted, 1);
        assert_eq!(task_state(&kernel, task_a).status, "dead");
        assert_eq!(task_state(&kernel, task_b).status, "pending");
        assert_eq!(count(&kernel, "review_queue"), 1);
    }
}
