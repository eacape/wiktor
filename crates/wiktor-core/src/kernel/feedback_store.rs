//! Step 6 批1：反馈闭环存储层（spec `step6-feedback-loop.md` §3 D3/D4/D5、§5 DDL、
//! §6 类型契约、§10 A2–A4、§11 批1）；批4 追加审核转换（§3 D12、§8、§10 A15/A16、
//! §11 批4）；批5 追加 HTTP 面所需的批量原子插入 / accepted 页存在性 / 413 拒绝
//! 审计 / pending 计数（§7、§10 A6–A8）。
//! Step 6 batch 1: the feedback-loop storage layer (spec `step6-feedback-loop.md`
//! §3 D3/D4/D5, §5 DDL, §6 type contract, §10 A2–A4, §11 batch 1); batch 4 adds
//! the review transitions (§3 D12, §8, §10 A15/A16, §11 batch 4); batch 5 adds
//! the HTTP-facing batch-atomic insert / accepted-page existence / 413 rejection
//! audit / pending count (§7, §10 A6–A8).
//!
//! 职责边界（上层口径：不引入第二连接栈，偏差 STEP6-003——SQLite 实现按仓库既有
//! 模式落在本文件 `impl SqliteKernel`；`wiktor-feedback` 只持有 `FeedbackStore`
//! trait 与本模块类型的 re-export，批3 起分析器/报告通过持有 kernel 句柄委托调用；
//! 批1 实现 insert/load/list，批4 追加 approve_review/ignore_review）：
//! - `insert_feedback_idempotent`：单事务 BEGIN IMMEDIATE——先校验 log_id 存在且
//!   domain 一致（不一致 → Validation），再 `INSERT ... ON CONFLICT(domain,
//!   idempotency_key) DO NOTHING`；命中冲突即回读原行，返回 `replayed=true` 与
//!   原 `event_id`/`received_at`，绝不更新载荷（D5）；新行返回 `replayed=false`；
//! - `load_feedback_window`：同一读事务（BEGIN DEFERRED，spec §9：读窗口用只读
//!   事务）读 [from, to] 窗口内该 domain 的 query_logs（含 0005 三状态列）与
//!   feedback_events 两组快照，按 id 升序输出（分析器哈希的确定性输入序）；
//! - `list_reviews`：按 domain（可选 status）读 review_queue，`created_at` 升序，
//!   limit 1..=1000（CLI §8 契约同款边界，不做静默截断）；
//! - `insert_review_suggestions`（批3）：单事务 BEGIN IMMEDIATE 批量插入审核
//!   建议，UNIQUE(domain,action,subject_json) 冲突跳过不报错，返回实际新插入的
//!   review_id（批6 CLI analyze 调用；分析器本身不写库）；
//! - `approve_review`（批4，D12）：单事务 BEGIN IMMEDIATE——读 review 行并校验
//!   status=pending（非 pending → Validation「已审核」）；action=supplemental_
//!   compile 校验 subject_json 五必需字段（entity_id/source_revision/domain_pack_
//!   version/source_json/dependencies_json，缺 → Validation，不猜造）后在**同一
//!   事务**内复用 `compile_store::admit_compile_on_conn` 插入 compile_tasks，
//!   成功回填 compile_task_id + status='approved' + 审计字段，admission 未真正
//!   排队或任一步失败整体回滚（review 保持 pending、无孤儿任务）；action=query_
//!   template 仅写 status='approved' + 审计字段，不碰 compile_tasks（A16）；
//! - `ignore_review`（批4）：pending → status='ignored' + 审计字段的纯审计转换，
//!   不校验 subject、不触碰 compile_tasks；非 pending 拒绝。
//!
//! 类型单源：`FeedbackKind`/`FeedbackEventInput`/`FeedbackIngested`/`FeedbackEvent`/
//! `QueryLogSnapshot`/`ReviewItem`/`ReviewStatus`/`ReviewOutcome` 定义在本模块
//! （core），由 `wiktor-feedback` 全量 re-export 为 spec §6 的公开契约面——core
//! 不得反向依赖 feedback，故契约类型只能单源落 core。
//!
//! Responsibility boundary (upstream decision: no second connection stack,
//! deviation STEP6-003 — the SQLite implementation follows the repo's existing
//! pattern here as `impl SqliteKernel`; `wiktor-feedback` only carries the
//! `FeedbackStore` trait plus re-exports of this module's types, and from batch 3
//! the analyzer/report delegate through a kernel handle; batch 1 implemented
//! insert/load/list, batch 4 adds approve_review/ignore_review):
//! - `insert_feedback_idempotent`: one BEGIN IMMEDIATE transaction — first
//!   validate that the log_id exists with a matching domain (mismatch →
//!   Validation), then `INSERT ... ON CONFLICT(domain, idempotency_key)
//!   DO NOTHING`; on conflict the original row is read back and returned as
//!   `replayed=true` with the original `event_id`/`received_at`, and the payload
//!   is never overwritten (D5); new rows return `replayed=false`;
//! - `load_feedback_window`: one read transaction (BEGIN DEFERRED; spec §9:
//!   window reads use read-only transactions) fetching both snapshots — the
//!   domain's query_logs (with the three 0005 state columns) and feedback_events
//!   — inside [from, to], ordered by id ascending (the analyzer's deterministic
//!   hash-input order);
//! - `list_reviews`: reads review_queue by domain (optionally by status),
//!   ordered by `created_at` ascending, limit 1..=1000 (same bounds as the CLI
//!   §8 contract; never silently truncated);
//! - `insert_review_suggestions` (batch 3): bulk-inserts review suggestions in
//!   one BEGIN IMMEDIATE transaction, skipping UNIQUE(domain,action,subject_json)
//!   conflicts without error and returning the actually newly inserted
//!   review_ids (called by the batch-6 CLI analyze; the analyzer itself never
//!   writes);
//! - `approve_review` (batch 4, D12): one BEGIN IMMEDIATE transaction — reads
//!   the review row and validates status=pending (non-pending → Validation
//!   "already reviewed"); for action=supplemental_compile it validates the five
//!   required subject_json fields (entity_id/source_revision/domain_pack_version/
//!   source_json/dependencies_json, missing → Validation, never fabricated) and
//!   then reuses `compile_store::admit_compile_on_conn` inside the **same**
//!   transaction to insert the compile task; success backfills compile_task_id +
//!   status='approved' + audit fields, while an admission that fails to queue or
//!   any other failure rolls the whole thing back (the review stays pending, no
//!   orphan tasks); for action=query_template it only writes status='approved'
//!   plus audit fields and never touches compile_tasks (A16);
//! - `ignore_review` (batch 4): a pure audit transition from pending to
//!   status='ignored' plus audit fields; the subject is not validated and
//!   compile_tasks is never touched; non-pending rows are rejected.
//!
//! Single type source: `FeedbackKind`/`FeedbackEventInput`/`FeedbackIngested`/
//! `FeedbackEvent`/`QueryLogSnapshot`/`ReviewItem`/`ReviewStatus`/`ReviewOutcome`
//! are defined here (core) and fully re-exported by `wiktor-feedback` as the
//! public §6 contract surface — core must never depend back on feedback, so the
//! contract types can only live in core.

use super::compile_store::{admit_compile_on_conn, StoredDependencies};
use crate::compile::config::{prepare_source, Admission};
use crate::compile::hash::schema_from_value;
use crate::types::error::{Error, Result};
use crate::types::RawEntity;
use diesel::prelude::*;
use serde::{Deserialize, Serialize};

// ===== 输入预算常量（spec §5/§7：数据模型层规则，kernel 先行校验，HTTP 层复述）=====
// ===== Input-budget constants (spec §5/§7: data-model rules; the kernel
// validates first and the HTTP layer restates them) =====

/// idempotency_key：ASCII 可见字符，1..=128（spec §6 字段契约）。
/// idempotency_key: ASCII visible characters, 1..=128 (spec §6 field contract).
const MAX_KEY_CHARS: usize = 128;
/// domain：1..=128 Unicode scalar（spec §7）。
/// domain: 1..=128 Unicode scalars (spec §7).
const MAX_DOMAIN_CHARS: usize = 128;
/// click/adopt 的 page_id：非空且 ≤512（spec §7）。
/// click/adopt page_id: non-empty and ≤512 (spec §7).
const MAX_PAGE_ID_CHARS: usize = 512;
/// metadata_json：只允许对象且 ≤4 KiB（spec §5）。
/// metadata_json: object only, ≤4 KiB (spec §5).
const MAX_METADATA_BYTES: usize = 4 * 1024;
/// list_reviews 单页上限（spec §8 CLI `--limit 1..=1000` 同款边界）。
/// list_reviews page cap (same bounds as the spec §8 CLI `--limit 1..=1000`).
const MAX_REVIEW_LIMIT: u32 = 1000;
/// reviewed_by 审计边界：操作者名 1..=128 字符（审计不允许空操作者）。
/// reviewed_by audit bound: operator names are 1..=128 chars (audits never have
/// an empty operator).
const MAX_REVIEWER_CHARS: usize = 128;

// ===== 公开契约类型（spec §6；serde 风格对齐仓库：rename_all + deny_unknown_fields）=====
// ===== Public contract types (spec §6; serde style aligned with the repo:
// rename_all + deny_unknown_fields) =====

/// 反馈事件种类（D3：仅 click/adopt/rate；`hit` 不作为 API 事件，命中数已在
/// query log 中）。
/// Feedback event kinds (D3: click/adopt/rate only; `hit` is never an API event
/// because hit counts already live in the query log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FeedbackKind {
    Click,
    Adopt,
    Rate,
}

impl FeedbackKind {
    /// DDL 存储串（`feedback_events.kind` CHECK 的三个合法值）。
    /// The DDL storage string (one of the three legal `feedback_events.kind`
    /// CHECK values).
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackKind::Click => "click",
            FeedbackKind::Adopt => "adopt",
            FeedbackKind::Rate => "rate",
        }
    }

    /// 从 DDL 存储串解析（库内出现未知 kind = 数据损坏 → Internal）。
    /// Parses from the DDL storage string (an unknown kind in the DB means
    /// corruption → Internal).
    fn from_db(value: &str) -> Result<Self> {
        match value {
            "click" => Ok(FeedbackKind::Click),
            "adopt" => Ok(FeedbackKind::Adopt),
            "rate" => Ok(FeedbackKind::Rate),
            other => Err(Error::Internal(format!(
                "corrupt feedback_events.kind: {other:?}"
            ))),
        }
    }
}

/// 幂等插入输入（spec §6 类型契约；kind/rating/page 组合合法性见
/// [`validate_feedback_input`]）。
/// Idempotent-insert input (spec §6 type contract; kind/rating/page legality is
/// enforced by [`validate_feedback_input`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackEventInput {
    /// ASCII/可见字符，1..=128。
    /// ASCII/visible characters, 1..=128.
    pub idempotency_key: String,
    pub domain: String,
    /// 必须指向存在的 query_logs 行，且其 domain 与本字段一致。
    /// Must point to an existing query_logs row whose domain matches this field.
    pub log_id: i64,
    pub kind: FeedbackKind,
    /// click/adopt 必填；rate 可选。
    /// Required for click/adopt; optional for rate.
    pub page_id: Option<String>,
    /// 仅 rate 使用，1..=5；click/adopt 必须为 None。
    /// Only for rate, 1..=5; must be None for click/adopt.
    pub rating: Option<u8>,
    /// 只允许不超过 4 KiB 的 JSON 对象（spec §5）。
    /// A JSON object of at most 4 KiB only (spec §5).
    pub metadata: serde_json::Value,
}

/// 幂等插入结果（D5：replayed=true 时 event_id/received_at 为首次写入的原值）。
/// Idempotent-insert outcome (D5: with replayed=true, event_id/received_at are
/// the original first-write values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackIngested {
    pub event_id: i64,
    pub replayed: bool,
    pub received_at: i64,
}

/// `feedback_events` 行读取形状（load_window 的反馈组；metadata 保持原始串，
/// 解析/判定属分析器批次）。
/// Read shape of a `feedback_events` row (the feedback group of load_window;
/// metadata stays the raw string — parsing/judging belongs to the analyzer
/// batch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEvent {
    pub event_id: i64,
    pub idempotency_key: String,
    pub domain: String,
    pub log_id: i64,
    pub kind: FeedbackKind,
    pub page_id: Option<String>,
    pub rating: Option<u8>,
    pub metadata_json: String,
    pub received_at: i64,
}

/// `query_logs` 行读取形状（load_window 的查询日志组；0005 三状态列随行携带，
/// legacy 行为 `__legacy__`/false/false/false，STEP6-002）。
/// Read shape of a `query_logs` row (the query-log group of load_window; the
/// three 0005 state columns travel with the row; legacy rows read
/// `__legacy__`/false/false/false, STEP6-002).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLogSnapshot {
    pub log_id: i64,
    pub query_text: String,
    pub query_json: String,
    pub rewritten_json: Option<String>,
    pub rewrite_failure: bool,
    pub hit_count: i64,
    pub latency_ms: i64,
    pub timestamp: i64,
    pub domain: String,
    pub candidate_empty_initial: bool,
    pub relaxation_attempted: bool,
    pub relaxation_succeeded: bool,
}

/// 审核项状态（`review_queue.status` CHECK 的四个合法值）。
/// Review-item status (the four legal `review_queue.status` CHECK values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewStatus {
    Pending,
    Approved,
    Ignored,
    Failed,
}

impl ReviewStatus {
    /// DDL 存储串。
    /// The DDL storage string.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewStatus::Pending => "pending",
            ReviewStatus::Approved => "approved",
            ReviewStatus::Ignored => "ignored",
            ReviewStatus::Failed => "failed",
        }
    }

    /// 从 DDL 存储串解析（未知值 = 数据损坏 → Internal）。
    /// Parses from the DDL storage string (unknown values mean corruption →
    /// Internal).
    fn from_db(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(ReviewStatus::Pending),
            "approved" => Ok(ReviewStatus::Approved),
            "ignored" => Ok(ReviewStatus::Ignored),
            "failed" => Ok(ReviewStatus::Failed),
            other => Err(Error::Internal(format!(
                "corrupt review_queue.status: {other:?}"
            ))),
        }
    }
}

/// `review_queue` 行读取形状（批1 只读列表；action 保持 DDL 原串，approve/ignore
/// 批4 引入动作枚举与状态转换）。
/// Read shape of a `review_queue` row (batch 1 is read-only listing; `action`
/// keeps the raw DDL string — the action enum and status transitions arrive
/// with approve/ignore in batch 4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub review_id: i64,
    pub domain: String,
    pub action: String,
    pub status: ReviewStatus,
    pub source_log_ids_json: String,
    pub subject_json: String,
    pub reason_json: String,
    pub created_at: i64,
    pub reviewed_at: Option<i64>,
    pub reviewed_by: Option<String>,
    pub compile_task_id: Option<i64>,
}

/// 审核建议插入输入（批3；分析器产物经调用方映射进 kernel，`domain` 列由
/// [`SqliteKernel::insert_review_suggestions`] 的同名参数提供，不随行携带）。
/// Review-suggestion insert input (batch 3; analyzer output mapped by the
/// caller into the kernel — the `domain` column comes from the same-named
/// parameter of [`SqliteKernel::insert_review_suggestions`], not from the row).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSuggestionInput {
    /// DDL 三个合法值之一：`supplemental_compile` / `query_template` / `ignore`。
    /// One of the three legal DDL values: `supplemental_compile` /
    /// `query_template` / `ignore`.
    pub action: String,
    /// 来源 query log id 的 JSON 数组串（如 `"[1,3]"`；trace 用途）。
    /// JSON array string of source query-log ids (e.g. `"[1,3]"`; for trace).
    pub source_log_ids_json: String,
    /// 建议 subject 的 JSON 串（分析器确定性生成，UNIQUE(domain,action,subject)
    /// 的幂等键成分）。
    /// The suggestion's subject JSON string (deterministically produced by the
    /// analyzer; part of the UNIQUE(domain,action,subject) idempotency key).
    pub subject_json: String,
    /// 判定理由的 JSON 串（信号名与阈值上下文，审计用）。
    /// The reason JSON string (signal name plus threshold context; audit).
    pub reason_json: String,
    /// 创建时间（unix 秒；由调用方提供——报告模型保持无时间戳的确定性）。
    /// Creation time (unix seconds; caller-supplied — the report model stays
    /// deterministic with no wall-clock inside).
    pub created_at: i64,
}

/// 审核转换结果（spec §6 `ReviewOutcome`；批4）。
/// Review-transition outcome (spec §6 `ReviewOutcome`; batch 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub review_id: i64,
    /// 转换后的状态：approve → Approved；（ignore_review 返回 `()`，不经本类型）。
    /// The post-transition status: approve → Approved (ignore_review returns `()`
    /// and does not go through this type).
    pub status: ReviewStatus,
    /// 仅 supplemental_compile 批准成功时为新 compile task id；query_template 与
    /// 其他路径恒为 `None`（A16：不写 compile_tasks）。
    /// The new compile-task id only for a successful supplemental_compile
    /// approval; always `None` for query_template and other paths (A16: no
    /// compile_tasks writes).
    pub compile_task_id: Option<i64>,
}

// ===== 纯函数校验（spec §5/§6/§7 数据模型规则；CHECK 约束为兜底背书）=====
// ===== Pure validation (spec §5/§6/§7 data-model rules; CHECK constraints act
// as the backstop) =====

/// 413 拒绝审计原因（批5；`feedback_rejections.reason` CHECK 的三个合法值，
/// spec §5/§7：载荷、事件数或字段预算超限写计数审计行，400/401/403/422/429 不写）。
/// Rejection-audit reasons for 413 (batch 5; the three legal
/// `feedback_rejections.reason` CHECK values, spec §5/§7: payload, event-count
/// and field-budget overruns write an audit row, while 400/401/403/422/429 do
/// not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackRejectionReason {
    PayloadTooLarge,
    EventCountTooLarge,
    FieldTooLarge,
}

impl FeedbackRejectionReason {
    /// DDL 存储串。
    /// The DDL storage string.
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackRejectionReason::PayloadTooLarge => "payload_too_large",
            FeedbackRejectionReason::EventCountTooLarge => "event_count_too_large",
            FeedbackRejectionReason::FieldTooLarge => "field_too_large",
        }
    }
}

/// kind/rating/page/key/domain/metadata 合法性（非法输入 → Validation；DB CHECK
/// 仅作约束兜底，错误面更清晰）。
/// Validates kind/rating/page/key/domain/metadata legality (illegal input →
/// Validation; DB CHECKs stay as constraint backstops with a clearer error
/// surface).
fn validate_feedback_input(input: &FeedbackEventInput) -> Result<()> {
    let key_chars = input.idempotency_key.chars().count();
    if key_chars == 0 || key_chars > MAX_KEY_CHARS {
        return Err(Error::Validation(format!(
            "idempotency_key must be 1..={MAX_KEY_CHARS} chars (got {key_chars})"
        )));
    }
    // ASCII 可见字符（0x21..=0x7E）：排除空格与控制符（spec §6 注释）。
    // ASCII visible characters (0x21..=0x7E): no spaces, no control characters
    // (spec §6 comment).
    if !input.idempotency_key.chars().all(|c| c.is_ascii_graphic()) {
        return Err(Error::Validation(
            "idempotency_key must contain ASCII visible characters only".into(),
        ));
    }
    let domain_chars = input.domain.chars().count();
    if domain_chars == 0 || domain_chars > MAX_DOMAIN_CHARS {
        return Err(Error::Validation(format!(
            "domain must be 1..={MAX_DOMAIN_CHARS} chars (got {domain_chars})"
        )));
    }
    if input.log_id <= 0 {
        return Err(Error::Validation(format!(
            "log_id must be > 0 (got {})",
            input.log_id
        )));
    }
    match input.kind {
        FeedbackKind::Rate => {
            // rate：rating 必填且 1..=5（DDL：rating BETWEEN 1 AND 5）。
            // rate: rating required and 1..=5 (DDL: rating BETWEEN 1 AND 5).
            match input.rating {
                Some(r) if (1..=5).contains(&r) => {}
                other => {
                    return Err(Error::Validation(format!(
                        "rate events require rating 1..=5 (got {other:?})"
                    )));
                }
            }
        }
        FeedbackKind::Click | FeedbackKind::Adopt => {
            // click/adopt：rating 必须缺省、page_id 必填且 ≤512（D3）。
            // click/adopt: rating absent, page_id required and ≤512 (D3).
            if input.rating.is_some() {
                return Err(Error::Validation(format!(
                    "{} events must not carry a rating",
                    input.kind.as_str()
                )));
            }
            let page = input.page_id.as_deref().unwrap_or_default();
            let page_chars = page.chars().count();
            if page_chars == 0 || page_chars > MAX_PAGE_ID_CHARS {
                return Err(Error::Validation(format!(
                    "{} events require page_id of 1..={MAX_PAGE_ID_CHARS} chars",
                    input.kind.as_str()
                )));
            }
        }
    }
    // metadata：只允许 JSON 对象且序列化后 ≤4 KiB（spec §5；MVP 不参与判定）。
    // metadata: JSON object only, ≤4 KiB serialized (spec §5; not used for
    // judgment in the MVP).
    if !input.metadata.is_object() {
        return Err(Error::Validation("metadata must be a JSON object".into()));
    }
    let text = serde_json::to_string(&input.metadata)?;
    if text.len() > MAX_METADATA_BYTES {
        return Err(Error::Validation(format!(
            "metadata must not exceed {MAX_METADATA_BYTES} bytes (got {})",
            text.len()
        )));
    }
    Ok(())
}

/// 审核建议合法性（批3；非法输入 → Validation，DB CHECK 仅兜底）：domain 边界、
/// action 枚举、三个 JSON 字段必须可解析（CHECK 无法验证 JSON 形状，应用层先行）。
/// Review-suggestion legality (batch 3; illegal input → Validation, the DB CHECK
/// stays a backstop): domain bounds, the action enum, and the three JSON fields
/// must parse (CHECKs cannot verify JSON shape, so the application layer goes
/// first).
fn validate_review_suggestion(domain: &str, suggestion: &ReviewSuggestionInput) -> Result<()> {
    let domain_chars = domain.chars().count();
    if domain_chars == 0 || domain_chars > MAX_DOMAIN_CHARS {
        return Err(Error::Validation(format!(
            "review suggestion domain must be 1..={MAX_DOMAIN_CHARS} chars (got {domain_chars})"
        )));
    }
    match suggestion.action.as_str() {
        "supplemental_compile" | "query_template" | "ignore" => {}
        other => {
            return Err(Error::Validation(format!(
                "review suggestion action must be one of \
                 supplemental_compile/query_template/ignore (got {other:?})"
            )));
        }
    }
    for (name, text) in [
        ("source_log_ids_json", &suggestion.source_log_ids_json),
        ("subject_json", &suggestion.subject_json),
        ("reason_json", &suggestion.reason_json),
    ] {
        if serde_json::from_str::<serde_json::Value>(text).is_err() {
            return Err(Error::Validation(format!(
                "review suggestion {name} must be a parseable JSON string"
            )));
        }
    }
    Ok(())
}

// ===== 批4：审核转换的纯校验（spec §8 口径；CHECK 约束为兜底背书）=====
// ===== Batch 4: pure validation for review transitions (spec §8; CHECK
// constraints stay as the backstop) =====

/// supplemental_compile subject 的还原产物：完整源实体 + 与
/// compile_tasks.dependencies_json 同形状的冻结依赖（禁止双实现漂移）。
/// The reconstructed supplemental_compile subject: the full raw entity plus the
/// frozen dependencies in the exact `compile_tasks.dependencies_json` shape (no
/// drifting duplicates).
#[derive(Debug, Clone)]
struct SupplementalSubject {
    raw: RawEntity,
    deps: StoredDependencies,
}

/// 解析并交叉校验 supplemental_compile 的 subject_json（spec §8）：五个必需字段
/// entity_id/source_revision/domain_pack_version/source_json/dependencies_json
/// 缺一或类型不符 → Validation，**不猜造**任务；再校验 subject 自身的内部一致
/// 性（entity_id/source_revision 与 source_json 一致、domain_pack_version 与
/// dependencies_json.context 一致）。
/// Parses and cross-validates the supplemental_compile subject_json (spec §8):
/// the five required fields entity_id/source_revision/domain_pack_version/
/// source_json/dependencies_json are each mandatory with strict types — missing
/// or ill-typed → Validation, tasks are **never fabricated**; then the subject's
/// own coherence is enforced (entity_id/source_revision must match source_json,
/// domain_pack_version must match dependencies_json.context).
fn parse_supplemental_subject(subject_json: &str) -> Result<SupplementalSubject> {
    let value: serde_json::Value = serde_json::from_str(subject_json)
        .map_err(|_| Error::Validation("review subject_json must be a JSON object".into()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| Error::Validation("review subject_json must be a JSON object".into()))?;
    let entity_id = require_subject_str(obj, "entity_id")?;
    let source_revision = require_subject_revision(obj)?;
    let domain_pack_version = require_subject_str(obj, "domain_pack_version")?;
    let source_json = require_subject_str(obj, "source_json")?;
    let dependencies_json = require_subject_str(obj, "dependencies_json")?;

    let raw: RawEntity = serde_json::from_str(source_json).map_err(|e| {
        Error::Validation(format!(
            "review subject source_json does not parse as a RawEntity: {e}"
        ))
    })?;
    let deps: StoredDependencies = serde_json::from_str(dependencies_json).map_err(|e| {
        Error::Validation(format!(
            "review subject dependencies_json does not parse as {{context, policy, schema}}: {e}"
        ))
    })?;

    // 一致性交叉校验（fail-closed）：subject 声明与载荷矛盾时拒绝，不做任何代填。
    // Coherence cross-checks (fail-closed): a subject contradicting its payload
    // is rejected, never silently reconciled.
    if raw.id.to_key() != entity_id {
        return Err(Error::Validation(format!(
            "review subject entity_id {entity_id:?} does not match source_json entity {:?}",
            raw.id.to_key()
        )));
    }
    if raw.source_revision != source_revision {
        return Err(Error::Validation(format!(
            "review subject source_revision {source_revision} does not match \
             source_revision {} in source_json",
            raw.source_revision
        )));
    }
    if deps.context.domain_pack_version != domain_pack_version {
        return Err(Error::Validation(format!(
            "review subject domain_pack_version {domain_pack_version:?} does not match \
             dependencies_json context {:?}",
            deps.context.domain_pack_version
        )));
    }
    Ok(SupplementalSubject { raw, deps })
}

/// 读取 subject 的非空字符串字段（缺字段/类型不符 → Validation，spec §8「缺字段
/// 返回校验错误不猜造」）。
/// Reads a required non-empty string field from the subject (missing field or
/// wrong type → Validation; spec §8 "missing fields return a validation error,
/// never a fabricated task").
fn require_subject_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<&'a str> {
    match obj.get(name) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(s.as_str()),
        Some(other) => Err(Error::Validation(format!(
            "review subject field {name:?} must be a non-empty string (got {other:?})"
        ))),
        None => Err(Error::Validation(format!(
            "review subject is missing required field {name:?} (spec step6 §8: \
             entity_id/source_revision/domain_pack_version/source_json/dependencies_json)"
        ))),
    }
}

/// 读取 subject 的 source_revision（正整数；0 → Validation，对齐 revision
/// 1..=i64::MAX 契约）。
/// Reads the subject's source_revision (a positive integer; 0 → Validation,
/// aligned with the 1..=i64::MAX revision contract).
fn require_subject_revision(obj: &serde_json::Map<String, serde_json::Value>) -> Result<u64> {
    let value = obj.get("source_revision").ok_or_else(|| {
        Error::Validation("review subject is missing required field \"source_revision\"".into())
    })?;
    let revision = value.as_u64().ok_or_else(|| {
        Error::Validation(format!(
            "review subject field \"source_revision\" must be a non-negative integer \
             (got {value:?})"
        ))
    })?;
    if revision == 0 {
        return Err(Error::Validation(
            "review subject field \"source_revision\" must be >= 1".into(),
        ));
    }
    Ok(revision)
}

/// reviewer 审计边界：1..=128 字符（空操作者会让 reviewed_by 失去可审计性）。
/// Reviewer audit bound: 1..=128 chars (an empty operator would make
/// reviewed_by unauditable).
fn validate_reviewer(reviewer: &str) -> Result<()> {
    let chars = reviewer.chars().count();
    if chars == 0 || chars > MAX_REVIEWER_CHARS {
        return Err(Error::Validation(format!(
            "reviewer must be 1..={MAX_REVIEWER_CHARS} chars (got {chars})"
        )));
    }
    Ok(())
}

// ===== `diesel::sql_query` 行映射（QueryableByName）=====
// ===== `diesel::sql_query` row mappings (QueryableByName) =====

/// 单列文本行（log domain 读取）。
/// Single text-column row (log-domain lookup).
#[derive(QueryableByName)]
struct LogDomainRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

/// 单列 ID 行（last_insert_rowid）。
/// Single ID column row (last_insert_rowid).
#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// 幂等回放行（原 event_id/received_at，D5）。
/// Replay row (original event_id/received_at, D5).
#[derive(QueryableByName)]
struct ReplayRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    event_id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    received_at: i64,
}

/// query_logs 窗口行。
/// query_logs window row.
#[derive(QueryableByName)]
struct QueryLogRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    log_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    query_text: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    query_json: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    rewritten_json: Option<String>,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    rewrite_failure: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    hit_count: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    latency_ms: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    timestamp: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    domain: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    candidate_empty_initial: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    relaxation_attempted: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    relaxation_succeeded: i64,
}

/// feedback_events 窗口行。
/// feedback_events window row.
#[derive(QueryableByName)]
struct FeedbackEventRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    event_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    idempotency_key: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    domain: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    log_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    kind: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    page_id: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    rating: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    metadata_json: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    received_at: i64,
}

/// review_queue 行。
/// review_queue row.
#[derive(QueryableByName)]
struct ReviewRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    review_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    domain: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    action: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    status: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    source_log_ids_json: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    subject_json: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    reason_json: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    created_at: i64,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    reviewed_at: Option<i64>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    reviewed_by: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
    compile_task_id: Option<i64>,
}

impl FeedbackEventRow {
    /// 行 → 契约类型（kind/rating 损坏 → Internal，绝不静默丢弃事件）。
    /// Row → contract type (corrupt kind/rating → Internal; events are never
    /// silently dropped).
    fn into_contract(self) -> Result<FeedbackEvent> {
        let kind = FeedbackKind::from_db(&self.kind)?;
        let rating = match self.rating {
            None => None,
            Some(v) => {
                let r = u8::try_from(v).map_err(|_| {
                    Error::Internal(format!(
                        "corrupt feedback_events.rating {} for event {}",
                        v, self.event_id
                    ))
                })?;
                if !(1..=5).contains(&r) {
                    return Err(Error::Internal(format!(
                        "corrupt feedback_events.rating {r} for event {}",
                        self.event_id
                    )));
                }
                Some(r)
            }
        };
        Ok(FeedbackEvent {
            event_id: self.event_id,
            idempotency_key: self.idempotency_key,
            domain: self.domain,
            log_id: self.log_id,
            kind,
            page_id: self.page_id,
            rating,
            metadata_json: self.metadata_json,
            received_at: self.received_at,
        })
    }
}

impl ReviewRow {
    /// 行 → 契约类型（status 损坏 → Internal）。
    /// Row → contract type (corrupt status → Internal).
    fn into_contract(self) -> Result<ReviewItem> {
        Ok(ReviewItem {
            review_id: self.review_id,
            domain: self.domain,
            action: self.action,
            status: ReviewStatus::from_db(&self.status)?,
            source_log_ids_json: self.source_log_ids_json,
            subject_json: self.subject_json,
            reason_json: self.reason_json,
            created_at: self.created_at,
            reviewed_at: self.reviewed_at,
            reviewed_by: self.reviewed_by,
            compile_task_id: self.compile_task_id,
        })
    }
}

impl super::sqlite::SqliteKernel {
    /// 幂等插入一条反馈事件（D5；A4）。
    /// Idempotently inserts one feedback event (D5; A4).
    ///
    /// 锁纪律：取 conn Mutex 一次，`immediate_transaction`（BEGIN IMMEDIATE）内
    /// 完成 log 校验 + 插入/回放；锁 poison → Internal，不持锁跨 await。事务体
    /// 单源于 [`insert_feedback_idempotent_on_conn`]（批5 抽取，与批量路径共享）。
    /// Lock discipline: the conn Mutex is taken once; log validation and
    /// insert/replay both happen inside `immediate_transaction` (BEGIN
    /// IMMEDIATE); poison → Internal; the lock is never held across await. The
    /// transaction body is single-sourced in
    /// [`insert_feedback_idempotent_on_conn`] (extracted in batch 5, shared with
    /// the batch path).
    pub fn insert_feedback_idempotent(
        &self,
        input: &FeedbackEventInput,
        now: i64,
    ) -> Result<FeedbackIngested> {
        // 纯校验先于事务（失败不取写锁）。
        // Pure validation runs before the transaction (failures take no write
        // lock).
        validate_feedback_input(input)?;
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| insert_feedback_idempotent_on_conn(tx, input, now))
    }

    /// 幂等批量插入（批5；spec §7.1「一个请求内任一事件失败，整批事务回滚，
    /// 禁止部分成功」）。
    /// Idempotent batch insert (batch 5; spec §7.1 "if any event in a request
    /// fails, the whole batch transaction rolls back — partial success is
    /// forbidden").
    ///
    /// 语义：全部输入先做纯校验（任一非法 → Validation，不取写锁、零行落库），
    /// 再在**单个** BEGIN IMMEDIATE 事务内逐条走与单事件完全相同的事务体
    /// （log 存在性/domain 一致性校验 + `ON CONFLICT DO NOTHING` 幂等插入/回放，
    /// 零 SQL 复制）；任一事件失败 → 整体回滚，`feedback_events` 零行变化；
    /// 全部重复 → 逐条返回 `replayed=true`（D5，A4）。
    /// Semantics: all inputs pass pure validation first (any illegal one →
    /// Validation, no write lock, zero rows persisted), then every event runs
    /// the exact same transaction body as the single-event path inside **one**
    /// BEGIN IMMEDIATE transaction (log existence / domain-coherence checks plus
    /// the `ON CONFLICT DO NOTHING` idempotent insert/replay, zero SQL
    /// duplication); any failing event → full rollback with zero row changes in
    /// `feedback_events`; an all-duplicate batch → per-event `replayed=true`
    /// (D5, A4).
    ///
    /// 锁纪律：取 conn Mutex 一次，不持锁跨 await；锁 poison → Internal。
    /// Lock discipline: the conn Mutex is taken once, never held across await;
    /// poison → Internal.
    pub fn insert_feedback_batch_idempotent(
        &self,
        inputs: &[FeedbackEventInput],
        now: i64,
    ) -> Result<Vec<FeedbackIngested>> {
        for input in inputs {
            validate_feedback_input(input)?;
        }
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            inputs
                .iter()
                .map(|input| insert_feedback_idempotent_on_conn(tx, input, now))
                .collect::<Result<Vec<_>>>()
        })
    }

    /// accepted 页存在性（批5；上层拍板：query_logs 未存结果快照，spec §5
    /// 「page_id 属于本次查询结果集合」不可实现，降级为「page_id 必须是该
    /// domain 下 accepted 页真实存在」的精确匹配，否则 HTTP 层 422）。
    /// Accepted-page existence (batch 5; upstream decision: query_logs keeps no
    /// result snapshot, so spec §5's "page_id belongs to this query's result
    /// set" is unimplementable and is downgraded to the exact-match rule "the
    /// page_id must really exist as an accepted page of this domain", else the
    /// HTTP layer answers 422).
    ///
    /// 锁纪律：单条 SELECT，取 conn Mutex 一次。
    /// Lock discipline: one SELECT, conn Mutex taken once.
    pub fn page_exists(&self, domain: &str, page_id: &str) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        let row: IdRow = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM pages
             WHERE domain = ? AND page_id = ? AND status = 'accepted'",
        )
        .bind::<diesel::sql_types::Text, _>(domain)
        .bind::<diesel::sql_types::Text, _>(page_id)
        .get_result(&mut *conn)?;
        Ok(row.n > 0)
    }

    /// 写一条 413 拒绝审计行（批5；spec §5/§7 D9：超预算请求不写
    /// feedback_events，写 `feedback_rejections` 计数并可观测，A6）。
    /// Writes one 413 rejection-audit row (batch 5; spec §5/§7 D9: over-budget
    /// requests never write feedback_events, they count into
    /// `feedback_rejections` and stay observable, A6).
    ///
    /// `domain` 可空（body 尚未解析即超预算时未知，DDL 同样允许 NULL）。
    /// `domain` is nullable (unknown when the body was never parsed because the
    /// budget was already exceeded; the DDL allows NULL too).
    ///
    /// 锁纪律：单条 INSERT，取 conn Mutex 一次（autocommit 短写）。
    /// Lock discipline: one INSERT, conn Mutex taken once (short autocommit
    /// write).
    pub fn record_feedback_rejection(
        &self,
        domain: Option<&str>,
        reason: FeedbackRejectionReason,
        payload_bytes: i64,
        now: i64,
    ) -> Result<()> {
        if payload_bytes < 0 {
            return Err(Error::Validation(format!(
                "payload_bytes must be >= 0 (got {payload_bytes})"
            )));
        }
        let mut conn = self.lock_conn()?;
        diesel::sql_query(
            "INSERT INTO feedback_rejections (domain, reason, payload_bytes, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(domain)
        .bind::<diesel::sql_types::Text, _>(reason.as_str())
        .bind::<diesel::sql_types::BigInt, _>(payload_bytes)
        .bind::<diesel::sql_types::BigInt, _>(now)
        .execute(&mut *conn)?;
        Ok(())
    }

    /// 全库 pending 审核项计数（批5；spec §7.2 `wiktor_feedback_review_pending`
    /// gauge 的数据源，只读、不带 domain 维度——指标禁止用户可控 label）。
    /// Counts pending review items across domains (batch 5; the data source for
    /// the spec §7.2 `wiktor_feedback_review_pending` gauge, read-only and
    /// domain-less — metrics forbid user-controlled labels).
    ///
    /// 锁纪律：单条 SELECT，取 conn Mutex 一次。
    /// Lock discipline: one SELECT, conn Mutex taken once.
    pub fn count_pending_reviews(&self) -> Result<i64> {
        let mut conn = self.lock_conn()?;
        let row: IdRow =
            diesel::sql_query("SELECT COUNT(*) AS n FROM review_queue WHERE status = 'pending'")
                .get_result(&mut *conn)?;
        Ok(row.n)
    }
}

/// 单事件幂等插入的事务体（批5 自 [`insert_feedback_idempotent`] 抽取；单事件
/// 与批量两条路径共享同一 SQL 主体，零复制；调用方必须先做纯校验）。
/// The per-event idempotent-insert transaction body (batch 5, extracted from
/// [`insert_feedback_idempotent`]; the single-event and batch paths share the
/// exact same SQL body with zero duplication; callers must run pure validation
/// first).
fn insert_feedback_idempotent_on_conn(
    conn: &mut SqliteConnection,
    input: &FeedbackEventInput,
    now: i64,
) -> Result<FeedbackIngested> {
    let metadata_json = serde_json::to_string(&input.metadata)?;
    // —— log_id 必须存在且 domain 一致（spec §5；批1 口径：不存在或
    //    domain 不一致 → Validation，FK RESTRICT 为兜底）。
    // —— The log_id must exist with a matching domain (spec §5; the batch-1
    //    contract: missing or mismatched domain → Validation, with FK RESTRICT
    //    as the backstop).
    let log_domain: Option<LogDomainRow> =
        diesel::sql_query("SELECT domain AS value FROM query_logs WHERE log_id = ?")
            .bind::<diesel::sql_types::BigInt, _>(input.log_id)
            .get_result(conn)
            .optional()?;
    let Some(log_domain) = log_domain else {
        return Err(Error::Validation(format!(
            "feedback log_id {} not found in query_logs",
            input.log_id
        )));
    };
    if log_domain.value != input.domain {
        return Err(Error::Validation(format!(
            "feedback event domain {:?} does not match query log domain {:?}",
            input.domain, log_domain.value
        )));
    }

    let kind = input.kind.as_str();
    let inserted = diesel::sql_query(
        "INSERT INTO feedback_events
            (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (domain, idempotency_key) DO NOTHING",
    )
    .bind::<diesel::sql_types::Text, _>(&input.idempotency_key)
    .bind::<diesel::sql_types::Text, _>(&input.domain)
    .bind::<diesel::sql_types::BigInt, _>(input.log_id)
    .bind::<diesel::sql_types::Text, _>(kind)
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(input.page_id.as_deref())
    .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(input.rating.map(i64::from))
    .bind::<diesel::sql_types::Text, _>(&metadata_json)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(conn)?;

    if inserted == 1 {
        // 新行：event_id 取本连接 last_insert_rowid（事务内同连接有效）。
        // New row: event_id comes from this connection's last_insert_rowid
        // (valid on the same connection in-transaction).
        let row: IdRow = diesel::sql_query("SELECT last_insert_rowid() AS n").get_result(conn)?;
        Ok(FeedbackIngested {
            event_id: row.n,
            replayed: false,
            received_at: now,
        })
    } else {
        // —— D5：重复 (domain, idempotency_key) → 回放原 event_id/
        //    received_at；DO NOTHING 已保证载荷不被覆盖。
        // —— D5: duplicate (domain, idempotency_key) → replay the original
        //    event_id/received_at; DO NOTHING already keeps the payload
        //    untouched.
        let row: ReplayRow = diesel::sql_query(
            "SELECT event_id, received_at FROM feedback_events
             WHERE domain = ? AND idempotency_key = ?",
        )
        .bind::<diesel::sql_types::Text, _>(&input.domain)
        .bind::<diesel::sql_types::Text, _>(&input.idempotency_key)
        .get_result(conn)?;
        Ok(FeedbackIngested {
            event_id: row.event_id,
            replayed: true,
            received_at: row.received_at,
        })
    }
}

// —— 以下 impl 块承载批1–批4 既有方法（load_feedback_window / list_reviews /
//    insert_review_suggestions / approve_review / ignore_review）；批5 新方法
//    已置于上方独立 impl 块，同一类型多个 impl 块合法。——
// —— The impl block below carries the existing batch-1–4 methods
//    (load_feedback_window / list_reviews / insert_review_suggestions /
//    approve_review / ignore_review); the batch-5 methods live in the separate
//    impl block above — multiple impl blocks for one type are legal. ——
impl super::sqlite::SqliteKernel {
    /// 读取分析窗口（D11 输入；A2 旧列可读、legacy 行三状态默认 false）。
    /// Reads the analysis window (the D11 input; A2 keeps old columns readable
    /// and legacy rows default to false on the three state columns).
    ///
    /// 锁纪律：取 conn Mutex 一次，读事务 BEGIN DEFERRED（spec §9：读窗口用
    /// 只读事务，不加写锁）；两组快照同事务一致读取，按 id 升序（确定性）。
    /// Lock discipline: the conn Mutex is taken once inside a BEGIN DEFERRED
    /// read transaction (spec §9: window reads are read-only, no write lock);
    /// both snapshots are read consistently in one transaction, ordered by id
    /// ascending (deterministic).
    pub fn load_feedback_window(
        &self,
        domain: &str,
        from: i64,
        to: i64,
    ) -> Result<(Vec<QueryLogSnapshot>, Vec<FeedbackEvent>)> {
        if from > to {
            return Err(Error::Validation(format!(
                "feedback window requires from <= to (got {from} > {to})"
            )));
        }
        let mut conn = self.lock_conn()?;
        conn.transaction(|tx| {
            let logs: Vec<QueryLogRow> = diesel::sql_query(
                "SELECT log_id, query_text, query_json, rewritten_json, rewrite_failure,
                        hit_count, latency_ms, timestamp, domain,
                        candidate_empty_initial, relaxation_attempted, relaxation_succeeded
                 FROM query_logs
                 WHERE domain = ? AND timestamp >= ? AND timestamp <= ?
                 ORDER BY log_id",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .bind::<diesel::sql_types::BigInt, _>(from)
            .bind::<diesel::sql_types::BigInt, _>(to)
            .load(tx)?;

            let events: Vec<FeedbackEventRow> = diesel::sql_query(
                "SELECT event_id, idempotency_key, domain, log_id, kind, page_id,
                        rating, metadata_json, received_at
                 FROM feedback_events
                 WHERE domain = ? AND received_at >= ? AND received_at <= ?
                 ORDER BY event_id",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .bind::<diesel::sql_types::BigInt, _>(from)
            .bind::<diesel::sql_types::BigInt, _>(to)
            .load(tx)?;

            let logs = logs
                .into_iter()
                .map(|r| QueryLogSnapshot {
                    log_id: r.log_id,
                    query_text: r.query_text,
                    query_json: r.query_json,
                    rewritten_json: r.rewritten_json,
                    rewrite_failure: r.rewrite_failure != 0,
                    hit_count: r.hit_count,
                    latency_ms: r.latency_ms,
                    timestamp: r.timestamp,
                    domain: r.domain,
                    candidate_empty_initial: r.candidate_empty_initial != 0,
                    relaxation_attempted: r.relaxation_attempted != 0,
                    relaxation_succeeded: r.relaxation_succeeded != 0,
                })
                .collect();
            let events = events
                .into_iter()
                .map(FeedbackEventRow::into_contract)
                .collect::<Result<Vec<_>>>()?;
            Ok((logs, events))
        })
    }

    /// 分页读审核队列（批1 只读；approve/ignore 转换属批4）。
    /// Pages through the review queue (read-only in batch 1; approve/ignore
    /// transitions belong to batch 4).
    ///
    /// 锁纪律：单条 SELECT，取 conn Mutex 一次（无显式事务，对齐 qug_store 的
    /// 单读方法）。
    /// Lock discipline: one SELECT with the conn Mutex taken once (no explicit
    /// transaction; matches qug_store's single-read methods).
    pub fn list_reviews(
        &self,
        domain: &str,
        status: Option<ReviewStatus>,
        limit: u32,
    ) -> Result<Vec<ReviewItem>> {
        if limit == 0 {
            return Err(Error::Validation("review list limit must be >= 1".into()));
        }
        if limit > MAX_REVIEW_LIMIT {
            return Err(Error::Validation(format!(
                "review list limit must be <= {MAX_REVIEW_LIMIT} (got {limit})"
            )));
        }
        let mut conn = self.lock_conn()?;
        let rows: Vec<ReviewRow> = match status {
            Some(s) => diesel::sql_query(
                "SELECT review_id, domain, action, status, source_log_ids_json,
                        subject_json, reason_json, created_at, reviewed_at,
                        reviewed_by, compile_task_id
                 FROM review_queue
                 WHERE domain = ? AND status = ?
                 ORDER BY created_at, review_id
                 LIMIT ?",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .bind::<diesel::sql_types::Text, _>(s.as_str())
            .bind::<diesel::sql_types::BigInt, _>(i64::from(limit))
            .load(&mut *conn)?,
            None => diesel::sql_query(
                "SELECT review_id, domain, action, status, source_log_ids_json,
                        subject_json, reason_json, created_at, reviewed_at,
                        reviewed_by, compile_task_id
                 FROM review_queue
                 WHERE domain = ?
                 ORDER BY created_at, review_id
                 LIMIT ?",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .bind::<diesel::sql_types::BigInt, _>(i64::from(limit))
            .load(&mut *conn)?,
        };
        rows.into_iter().map(ReviewRow::into_contract).collect()
    }

    /// 单事务批量插入审核建议（批3；批6 CLI `feedback analyze` 调用；A12 幂等）。
    /// Inserts review suggestions in one transaction (batch 3; called by the
    /// batch-6 CLI `feedback analyze`; A12 idempotency).
    ///
    /// 语义：UNIQUE(domain, action, subject_json) 冲突跳过不报错（重复分析幂
    /// 等，spec §6），只返回**实际新插入**行的 review_id（输入序；被跳过的输入
    /// 不产生 id，因此返回长度 ≤ 输入长度）。批内重复同样只插入第一份。
    /// 建议 semantics: UNIQUE(domain, action, subject_json) conflicts are
    /// skipped without error (repeated analysis is idempotent, spec §6), and
    /// only the review_ids of **actually newly inserted** rows are returned
    /// (input order; skipped inputs yield no id, so the returned length is ≤
    /// the input length). Within-batch duplicates likewise insert only once.
    ///
    /// 锁纪律：全部输入先做纯校验（任一非法 → Validation，不取写锁、不落任何
    /// 行），再取 conn Mutex 一次，`immediate_transaction`（BEGIN IMMEDIATE）
    /// 内逐条 `INSERT ... ON CONFLICT DO NOTHING`；锁 poison → Internal，不持锁
    /// 跨 await。
    /// Lock discipline: all inputs pass pure validation first (any illegal one →
    /// Validation, no write lock, no rows at all), then the conn Mutex is taken
    /// once and each row is inserted inside `immediate_transaction` (BEGIN
    /// IMMEDIATE) with `ON CONFLICT DO NOTHING`; poison → Internal; the lock is
    /// never held across await.
    pub fn insert_review_suggestions(
        &self,
        domain: &str,
        suggestions: &[ReviewSuggestionInput],
    ) -> Result<Vec<i64>> {
        // 纯校验先于事务（失败不取写锁、零行落库）。
        // Pure validation runs before the transaction (failures take no write
        // lock and persist zero rows).
        for suggestion in suggestions {
            validate_review_suggestion(domain, suggestion)?;
        }
        if suggestions.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let mut inserted_ids = Vec::with_capacity(suggestions.len());
            for suggestion in suggestions {
                let inserted = diesel::sql_query(
                    "INSERT INTO review_queue
                        (domain, action, source_log_ids_json, subject_json, reason_json, created_at)
                     VALUES (?, ?, ?, ?, ?, ?)
                     ON CONFLICT (domain, action, subject_json) DO NOTHING",
                )
                .bind::<diesel::sql_types::Text, _>(domain)
                .bind::<diesel::sql_types::Text, _>(&suggestion.action)
                .bind::<diesel::sql_types::Text, _>(&suggestion.source_log_ids_json)
                .bind::<diesel::sql_types::Text, _>(&suggestion.subject_json)
                .bind::<diesel::sql_types::Text, _>(&suggestion.reason_json)
                .bind::<diesel::sql_types::BigInt, _>(suggestion.created_at)
                .execute(tx)?;
                if inserted == 1 {
                    // 新行：review_id 取本连接 last_insert_rowid（事务内同连接有效）。
                    // New row: review_id comes from this connection's
                    // last_insert_rowid (valid on the same connection
                    // in-transaction).
                    let row: IdRow =
                        diesel::sql_query("SELECT last_insert_rowid() AS n").get_result(tx)?;
                    inserted_ids.push(row.n);
                }
                // inserted == 0：UNIQUE 冲突被 DO NOTHING 跳过，不报错也不产 id。
                // inserted == 0: the UNIQUE conflict was skipped by DO NOTHING —
                // no error, no id.
            }
            Ok(inserted_ids)
        })
    }

    /// 审核批准（批4，D12 / A15 / A16）。
    /// Approves a review item (batch 4, D12 / A15 / A16).
    ///
    /// 事务边界：单 `immediate_transaction`（BEGIN IMMEDIATE）内完成——读 review
    /// 行 → 校验 status=pending（非 pending → Validation「已审核」）→
    /// supplemental_compile 时校验 subject 五必需字段并在**同一事务**内复用
    /// `admit_compile_on_conn`（既有 admission 主体，零 SQL 复制；diesel 2.3 禁止
    /// 嵌套 BEGIN，故复用主体而非调用自带事务的 `admit_compile` 薄壳）→ 回填
    /// compile_task_id/status='approved'/审计字段。任一步失败（含 admission
    /// skipped/rejected）整体回滚：review 保持 pending、无孤儿 compile task。
    /// query_template 仅写 approved + 审计字段（A16：不写 compile_tasks、不改
    /// QUG/配置）；action=ignore 的建议不可 approve（fail-closed，请用
    /// `ignore_review`）。取 conn Mutex 一次，不持锁跨 await。
    /// Transaction boundary: one `immediate_transaction` (BEGIN IMMEDIATE)
    /// covering — read the review row → validate status=pending (non-pending →
    /// Validation "already reviewed") → for supplemental_compile, validate the
    /// five required subject fields and reuse `admit_compile_on_conn` (the
    /// existing admission body, zero SQL duplication; diesel 2.3 forbids nested
    /// BEGINs, so the body is reused instead of calling the transaction-owning
    /// `admit_compile` shell) inside the **same** transaction → backfill
    /// compile_task_id / status='approved' / audit fields. Any failure (including
    /// a skipped/rejected admission) rolls everything back: the review stays
    /// pending and no orphan compile task exists. query_template only writes
    /// approved plus audit fields (A16: no compile_tasks writes, no QUG/config
    /// changes); suggestions with action=ignore are not approvable (fail-closed;
    /// use `ignore_review`). The conn Mutex is taken once, never held across
    /// await.
    pub fn approve_review(
        &self,
        review_id: i64,
        reviewer: &str,
        now: i64,
    ) -> Result<ReviewOutcome> {
        // reviewer 纯校验先于事务（失败不取写锁）。
        // The reviewer pure validation runs before the transaction (failure takes
        // no write lock).
        validate_reviewer(reviewer)?;
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let row: Option<ReviewRow> = diesel::sql_query(
                "SELECT review_id, domain, action, status, source_log_ids_json,
                        subject_json, reason_json, created_at, reviewed_at,
                        reviewed_by, compile_task_id
                 FROM review_queue WHERE review_id = ?",
            )
            .bind::<diesel::sql_types::BigInt, _>(review_id)
            .get_result(tx)
            .optional()?;
            let Some(row) = row else {
                return Err(Error::Validation(format!("review {review_id} not found")));
            };
            // —— approve 只允许 pending（spec §8）；重复审核在此拦截 ——
            // —— approve only allows pending (spec §8); repeated reviews are
            //    intercepted here ——
            if row.status != "pending" {
                return Err(Error::Validation(format!(
                    "review {review_id} has already been reviewed (status {:?}); \
                     approve only allows pending",
                    row.status
                )));
            }
            match row.action.as_str() {
                "supplemental_compile" => {
                    // —— subject 五必需字段（spec §8：缺 → Validation，不猜造）——
                    // —— The five required subject fields (spec §8: missing →
                    //    Validation, never fabricated) ——
                    let subject = parse_supplemental_subject(&row.subject_json)?;
                    // 租户一致性（D7 精神）：subject 实体必须属于本 review 的 domain。
                    // Tenant coherence (D7's spirit): the subject entity must
                    // belong to the review's own domain.
                    if subject.raw.id.domain != row.domain {
                        return Err(Error::Validation(format!(
                            "review subject entity {:?} does not belong to review domain {:?}",
                            subject.raw.id.to_key(),
                            row.domain
                        )));
                    }
                    // 还原冻结依赖 → 重投影（policy.validate/knowledge 解析/facts/
                    // snapshot_hash 全部重跑）→ 复用 admission 主体（同一事务）。
                    // Restore the frozen dependencies → re-project (policy
                    // validation, knowledge resolution, facts and snapshot_hash
                    // all re-run) → reuse the admission body (same transaction).
                    let schema = schema_from_value(&subject.deps.schema)?;
                    let prepared = prepare_source(&subject.raw, &schema, &subject.deps.policy)?;
                    let admission = admit_compile_on_conn(
                        tx,
                        &prepared,
                        &subject.deps.context,
                        &subject.deps.policy,
                        &schema,
                        false,
                    )?;
                    // fail-closed：只有真正排队（新任务或合并既有 pending/running）
                    // 才算批准；skipped/rejected/deferred → 报错回滚、review 保持
                    // pending（A15：成功 = 写 compile_tasks 并回填 task_id）。
                    // Fail-closed: approval counts only when a task is truly queued
                    // (a new one, or a merged existing pending/running one);
                    // skipped/rejected/deferred → error + rollback, the review
                    // stays pending (A15: success = compile_tasks written and
                    // task_id backfilled).
                    let task_id = match admission {
                        Admission::Queued(task_id) => task_id,
                        other => {
                            return Err(Error::Validation(format!(
                                "supplemental compile admission did not queue a task \
                                 ({other:?}); review stays pending"
                            )));
                        }
                    };
                    let affected = diesel::sql_query(
                        "UPDATE review_queue
                         SET status = 'approved', reviewed_at = ?, reviewed_by = ?,
                             compile_task_id = ?
                         WHERE review_id = ? AND status = 'pending'",
                    )
                    .bind::<diesel::sql_types::BigInt, _>(now)
                    .bind::<diesel::sql_types::Text, _>(reviewer)
                    .bind::<diesel::sql_types::BigInt, _>(task_id)
                    .bind::<diesel::sql_types::BigInt, _>(review_id)
                    .execute(tx)?;
                    if affected != 1 {
                        // 单连接 + BEGIN IMMEDIATE 下不可达；防御性 CAS（对齐仓库风格）。
                        // Unreachable under a single conn + BEGIN IMMEDIATE;
                        // defensive CAS (aligned with the repo style).
                        return Err(Error::Internal(
                            "review row changed state during approval".into(),
                        ));
                    }
                    Ok(ReviewOutcome {
                        review_id,
                        status: ReviewStatus::Approved,
                        compile_task_id: Some(task_id),
                    })
                }
                "query_template" => {
                    // —— 仅审计批准：不写 compile_tasks、不自动改 QUG/配置（A16）——
                    // —— Audit-only approval: no compile_tasks writes, no automatic
                    //    QUG/config changes (A16) ——
                    let affected = diesel::sql_query(
                        "UPDATE review_queue
                         SET status = 'approved', reviewed_at = ?, reviewed_by = ?
                         WHERE review_id = ? AND status = 'pending'",
                    )
                    .bind::<diesel::sql_types::BigInt, _>(now)
                    .bind::<diesel::sql_types::Text, _>(reviewer)
                    .bind::<diesel::sql_types::BigInt, _>(review_id)
                    .execute(tx)?;
                    if affected != 1 {
                        return Err(Error::Internal(
                            "review row changed state during approval".into(),
                        ));
                    }
                    Ok(ReviewOutcome {
                        review_id,
                        status: ReviewStatus::Approved,
                        compile_task_id: None,
                    })
                }
                other => Err(Error::Validation(format!(
                    "review action {other:?} is not approvable; use ignore_review to \
                     dismiss audit-only suggestions"
                ))),
            }
        })
    }

    /// 忽略审核项（批4；A16）。
    /// Ignores a review item (batch 4; A16).
    ///
    /// 纯审计转换：pending → status='ignored' + reviewed_at/reviewed_by；不校验
    /// subject、不触碰 compile_tasks（畸形 subject 的建议同样可被忽略）。非
    /// pending → Validation「已审核」；不存在的 review_id → Validation。单
    /// BEGIN IMMEDIATE，取 conn Mutex 一次。
    /// A pure audit transition: pending → status='ignored' plus
    /// reviewed_at/reviewed_by; the subject is not validated and compile_tasks is
    /// never touched (suggestions with malformed subjects are ignorable too).
    /// Non-pending → Validation "already reviewed"; a missing review_id →
    /// Validation. One BEGIN IMMEDIATE, conn Mutex taken once.
    pub fn ignore_review(&self, review_id: i64, reviewer: &str, now: i64) -> Result<()> {
        validate_reviewer(reviewer)?;
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let row: Option<ReviewRow> = diesel::sql_query(
                "SELECT review_id, domain, action, status, source_log_ids_json,
                        subject_json, reason_json, created_at, reviewed_at,
                        reviewed_by, compile_task_id
                 FROM review_queue WHERE review_id = ?",
            )
            .bind::<diesel::sql_types::BigInt, _>(review_id)
            .get_result(tx)
            .optional()?;
            let Some(row) = row else {
                return Err(Error::Validation(format!("review {review_id} not found")));
            };
            if row.status != "pending" {
                return Err(Error::Validation(format!(
                    "review {review_id} has already been reviewed (status {:?}); \
                     ignore only allows pending",
                    row.status
                )));
            }
            let affected = diesel::sql_query(
                "UPDATE review_queue
                 SET status = 'ignored', reviewed_at = ?, reviewed_by = ?
                 WHERE review_id = ? AND status = 'pending'",
            )
            .bind::<diesel::sql_types::BigInt, _>(now)
            .bind::<diesel::sql_types::Text, _>(reviewer)
            .bind::<diesel::sql_types::BigInt, _>(review_id)
            .execute(tx)?;
            if affected != 1 {
                return Err(Error::Internal(
                    "review row changed state during ignore".into(),
                ));
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::SqliteKernel;
    use diesel::sql_types;

    const DOMAIN: &str = "milk-tea";

    fn kernel() -> SqliteKernel {
        SqliteKernel::open_in_memory().unwrap()
    }

    /// 插入一条带 domain/三状态列的 query_logs 行，返回 log_id。
    /// Inserts a query_logs row carrying domain/the three state columns and
    /// returns the log_id.
    fn insert_log(kernel: &SqliteKernel, domain: &str, ts: i64, hits: i64) -> i64 {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure,
                    hit_count, latency_ms, timestamp, domain,
                    candidate_empty_initial, relaxation_attempted, relaxation_succeeded)
             VALUES ('波霸奶茶', '{}', NULL, 0, ?, 12, ?, ?, 0, 0, 0)",
        )
        .bind::<sql_types::BigInt, _>(hits)
        .bind::<sql_types::BigInt, _>(ts)
        .bind::<sql_types::Text, _>(domain)
        .execute(&mut *conn)
        .unwrap();
        diesel::sql_query("SELECT last_insert_rowid() AS n")
            .get_result::<IdRow>(&mut *conn)
            .unwrap()
            .n
    }

    /// 构造合法输入（click，带 page_id）。
    /// Builds a valid input (click with a page_id).
    fn click_input(log_id: i64) -> FeedbackEventInput {
        FeedbackEventInput {
            idempotency_key: "checkout-7-1".into(),
            domain: DOMAIN.into(),
            log_id,
            kind: FeedbackKind::Click,
            page_id: Some("milk-tea:drink:boba".into()),
            rating: None,
            metadata: serde_json::json!({}),
        }
    }

    /// 直接读库取事件行（校验载荷未被覆盖）。
    /// Reads the event row straight from the DB (payload-overwrite checks).
    fn raw_event(
        kernel: &SqliteKernel,
        event_id: i64,
    ) -> (String, Option<String>, Option<i64>, String) {
        let mut conn = kernel.lock_conn().unwrap();
        let row: FeedbackEventRow = diesel::sql_query(
            "SELECT event_id, idempotency_key, domain, log_id, kind, page_id,
                    rating, metadata_json, received_at
             FROM feedback_events WHERE event_id = ?",
        )
        .bind::<sql_types::BigInt, _>(event_id)
        .get_result(&mut *conn)
        .unwrap();
        (row.kind, row.page_id, row.rating, row.metadata_json)
    }

    // A4：幂等重放——同 (domain,key) 二次插入返回 replayed=true，event_id/
    // received_at 不变，不同载荷不覆盖原行。
    // A4: idempotent replay — re-inserting the same (domain,key) returns
    // replayed=true with unchanged event_id/received_at; a different payload
    // never overwrites the original row.
    #[test]
    fn insert_then_replay_returns_original_without_overwrite() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        let first = kernel
            .insert_feedback_idempotent(&click_input(log_id), 1000)
            .unwrap();
        assert!(!first.replayed);
        assert_eq!(first.received_at, 1000);

        // 同 key、不同 kind/page/rating/metadata：必须回放原值，不覆盖载荷。
        // Same key with different kind/page/rating/metadata: must replay the
        // original values without overwriting the payload.
        let mut replay = click_input(log_id);
        replay.kind = FeedbackKind::Rate;
        replay.rating = Some(5);
        replay.page_id = None;
        replay.metadata = serde_json::json!({"hacked": true});
        let second = kernel.insert_feedback_idempotent(&replay, 2000).unwrap();
        assert!(second.replayed);
        assert_eq!(second.event_id, first.event_id);
        assert_eq!(second.received_at, 1000, "replay must keep received_at");

        let (kind, page_id, rating, metadata) = raw_event(&kernel, first.event_id);
        assert_eq!(kind, "click");
        assert_eq!(page_id.as_deref(), Some("milk-tea:drink:boba"));
        assert_eq!(rating, None);
        assert_eq!(metadata, "{}");
    }

    // log_id 不存在 / domain 不一致 → Validation（批1 口径；FK RESTRICT 兜底）。
    // Missing log_id / mismatched domain → Validation (batch-1 contract; FK
    // RESTRICT stays the backstop).
    #[test]
    fn insert_rejects_missing_log_and_domain_mismatch() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        let mut missing = click_input(999_999);
        missing.idempotency_key = "k-missing".into();
        let err = kernel.insert_feedback_idempotent(&missing, 1).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");

        let mut mismatch = click_input(log_id);
        mismatch.domain = "ecommerce".into();
        mismatch.idempotency_key = "k-mismatch".into();
        let err = kernel.insert_feedback_idempotent(&mismatch, 1).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        // 两库行数均为 0（错误不落任何行）。
        // Zero rows anywhere (failures never persist rows).
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["feedback_events"], 0);
    }

    // kind/rating/page/key/metadata 非法组合 → Validation（A3 应用层部分）。
    // Illegal kind/rating/page/key/metadata combinations → Validation (the A3
    // application-level half).
    #[test]
    fn insert_validates_kind_rating_page_key_and_metadata() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        // click 缺 page_id / 带 rating。
        // click without page_id / click with a rating.
        let mut no_page = click_input(log_id);
        no_page.idempotency_key = "k1".into();
        no_page.page_id = None;
        assert!(kernel
            .insert_feedback_idempotent(&no_page, 1)
            .unwrap_err()
            .to_string()
            .contains("page_id"));

        let mut with_rating = click_input(log_id);
        with_rating.idempotency_key = "k2".into();
        with_rating.rating = Some(4);
        assert!(kernel
            .insert_feedback_idempotent(&with_rating, 1)
            .unwrap_err()
            .to_string()
            .contains("rating"));

        // rate 缺 rating / rating 越界。
        // rate without rating / out-of-range rating.
        let mut rate = click_input(log_id);
        rate.idempotency_key = "k3".into();
        rate.kind = FeedbackKind::Rate;
        rate.page_id = None;
        rate.rating = None;
        assert!(kernel
            .insert_feedback_idempotent(&rate, 1)
            .unwrap_err()
            .to_string()
            .contains("rating"));
        rate.rating = Some(6);
        assert!(kernel.insert_feedback_idempotent(&rate, 1).is_err());

        // key 边界：空 / 超长 / 控制符。
        // Key bounds: empty / too long / control characters.
        let mut bad_key = click_input(log_id);
        bad_key.idempotency_key = String::new();
        assert!(kernel.insert_feedback_idempotent(&bad_key, 1).is_err());
        bad_key.idempotency_key = "x".repeat(129);
        assert!(kernel.insert_feedback_idempotent(&bad_key, 1).is_err());
        bad_key.idempotency_key = "bad\nkey".into();
        assert!(kernel.insert_feedback_idempotent(&bad_key, 1).is_err());

        // metadata 非对象 / 超 4 KiB。
        // metadata not an object / over 4 KiB.
        let mut bad_meta = click_input(log_id);
        bad_meta.idempotency_key = "k4".into();
        bad_meta.metadata = serde_json::json!("scalar");
        assert!(kernel
            .insert_feedback_idempotent(&bad_meta, 1)
            .unwrap_err()
            .to_string()
            .contains("object"));
        bad_meta.metadata = serde_json::json!({ "blob": "x".repeat(5000) });
        assert!(kernel
            .insert_feedback_idempotent(&bad_meta, 1)
            .unwrap_err()
            .to_string()
            .contains("bytes"));

        // 空事件表：全部非法输入都未落行。
        // Empty event table: none of the illegal inputs persisted a row.
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["feedback_events"], 0);
    }

    // A3：DDL 兜底——绕过应用层校验的原始 SQL 仍被 CHECK/FK/UNIQUE 拒绝。
    // A3: the DDL backstop — raw SQL bypassing app-level validation is still
    // rejected by CHECK/FK/UNIQUE.
    #[test]
    fn ddl_constraints_reject_invalid_rows() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        // insert 为 `execute_batch` 的短别名；断言统一「必须报错」。
        // `insert` is a short alias for `execute_batch`; the assertions all mean
        // "must fail".
        let insert = |sql: &str| kernel.execute_batch(sql);

        // 未知 kind（CHECK）。
        // Unknown kind (CHECK).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r1', '{DOMAIN}', {log_id}, 'hit', 'p', NULL, '{{}}', 1)"
        ))
        .is_err());
        // rate 无 rating（组合 CHECK）。
        // rate without rating (combination CHECK).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r2', '{DOMAIN}', {log_id}, 'rate', NULL, NULL, '{{}}', 1)"
        ))
        .is_err());
        // click 带 rating（组合 CHECK）。
        // click with a rating (combination CHECK).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r3', '{DOMAIN}', {log_id}, 'click', 'p', 3, '{{}}', 1)"
        ))
        .is_err());
        // click 缺 page_id（组合 CHECK）。
        // click without page_id (combination CHECK).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r4', '{DOMAIN}', {log_id}, 'click', NULL, NULL, '{{}}', 1)"
        ))
        .is_err());
        // rating 越界（BETWEEN CHECK）。
        // Out-of-range rating (BETWEEN CHECK).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r5', '{DOMAIN}', {log_id}, 'rate', NULL, 6, '{{}}', 1)"
        ))
        .is_err());
        // FK：log_id 不存在（foreign_keys=ON + RESTRICT）。
        // FK: missing log_id (foreign_keys=ON + RESTRICT).
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r6', '{DOMAIN}', 424242, 'click', 'p', NULL, '{{}}', 1)"
        ))
        .is_err());
        // UNIQUE(domain, idempotency_key) 重复。
        // UNIQUE(domain, idempotency_key) duplicate.
        insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r7', '{DOMAIN}', {log_id}, 'click', 'p', NULL, '{{}}', 1)"
        ))
        .unwrap();
        assert!(insert(&format!(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id, rating, metadata_json, received_at)
             VALUES ('r7', '{DOMAIN}', {log_id}, 'click', 'p', NULL, '{{}}', 1)"
        ))
        .is_err());

        // query_logs 新列 CHECK：candidate_empty_initial 只允许 0/1。
        // The new query_logs column CHECK: candidate_empty_initial only 0/1.
        assert!(insert(
            "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure,
                    hit_count, latency_ms, timestamp, candidate_empty_initial)
             VALUES ('x', '{}', NULL, 0, 0, 0, 1, 2)"
        )
        .is_err());

        // review_queue：非法 action / 非法 status / FK / UNIQUE 三元组。
        // review_queue: bad action / bad status / FK / UNIQUE triple.
        assert!(insert(
            "INSERT INTO review_queue (domain, action, source_log_ids_json, subject_json, reason_json, created_at)
             VALUES ('milk-tea', 'auto_compile', '[]', '{}', '{}', 1)"
        )
        .is_err());
        insert(
            "INSERT INTO review_queue (domain, action, source_log_ids_json, subject_json, reason_json, created_at)
             VALUES ('milk-tea', 'ignore', '[]', '{}', '{}', 1)",
        )
        .unwrap();
        assert!(
            insert("UPDATE review_queue SET status = 'closed' WHERE domain = 'milk-tea'").is_err()
        );
        assert!(insert(
            "INSERT INTO review_queue (domain, action, source_log_ids_json, subject_json, reason_json, created_at, compile_task_id)
             VALUES ('milk-tea', 'ignore', '[]', '{}', '{}', 2, 999)"
        )
        .is_err());
        assert!(insert(
            "INSERT INTO review_queue (domain, action, source_log_ids_json, subject_json, reason_json, created_at)
             VALUES ('milk-tea', 'ignore', '[]', '{}', '{}', 3)"
        )
        .is_err());

        // feedback_rejections：非法 reason（CHECK）。
        // feedback_rejections: bad reason (CHECK).
        assert!(insert(
            "INSERT INTO feedback_rejections (domain, reason, payload_bytes, created_at)
             VALUES (NULL, 'unknown_reason', 10, 1)"
        )
        .is_err());
    }

    // load_feedback_window：domain 与时间窗口过滤、两组快照、legacy 行不串域。
    // load_feedback_window: domain/time filtering, both snapshot groups, and no
    // cross-domain leakage from legacy rows.
    #[test]
    fn load_window_filters_domain_and_range() {
        let kernel = kernel();
        let in_log = insert_log(&kernel, DOMAIN, 100, 0);
        let _later_log = insert_log(&kernel, DOMAIN, 300, 2);
        let _other_log = insert_log(&kernel, "ecommerce", 150, 5);
        let _legacy_log = insert_log(&kernel, "__legacy__", 120, 1);

        kernel
            .insert_feedback_idempotent(&click_input(in_log), 100)
            .unwrap();
        let mut rate = click_input(in_log);
        rate.idempotency_key = "rate-1".into();
        rate.kind = FeedbackKind::Rate;
        rate.page_id = None;
        rate.rating = Some(5);
        kernel.insert_feedback_idempotent(&rate, 260).unwrap();

        // 窗口 [100, 200]：1 条日志 + 1 条事件；[300,300]：日志命中、事件为空。
        // Window [100, 200]: one log + one event; [300, 300]: log hit, no
        // events.
        let (logs, events) = kernel.load_feedback_window(DOMAIN, 100, 200).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].log_id, in_log);
        assert_eq!(logs[0].domain, DOMAIN);
        assert!(!logs[0].rewrite_failure);
        assert!(!logs[0].candidate_empty_initial);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, FeedbackKind::Click);
        assert_eq!(events[0].received_at, 100);

        let (logs, events) = kernel.load_feedback_window(DOMAIN, 250, 300).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].timestamp, 300);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rating, Some(5));

        // 其他 domain / legacy domain 均不可见。
        // Other domains / the legacy domain stay invisible.
        let (logs, events) = kernel.load_feedback_window("ecommerce", 0, 10_000).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(events.len(), 0);
        let (logs, _events) = kernel
            .load_feedback_window("__legacy__", 0, 10_000)
            .unwrap();
        assert_eq!(logs.len(), 1);

        // from > to → Validation。
        // from > to → Validation.
        assert!(kernel.load_feedback_window(DOMAIN, 10, 5).is_err());
    }

    // list_reviews：status 过滤 + limit 边界 + created_at 稳定排序。
    // list_reviews: status filtering, limit bounds, and stable created_at
    // ordering.
    #[test]
    fn list_reviews_filters_status_and_limit() {
        let kernel = kernel();
        kernel
            .execute_batch(
                "INSERT INTO review_queue (domain, action, status, source_log_ids_json, subject_json, reason_json, created_at)
                 VALUES ('milk-tea', 'ignore', 'pending', '[1]', '{\"q\":\"a\"}', '{}', 20),
                        ('milk-tea', 'ignore', 'pending', '[2]', '{\"q\":\"b\"}', '{}', 10),
                        ('milk-tea', 'ignore', 'approved', '[3]', '{\"q\":\"c\"}', '{}', 30),
                        ('ecommerce', 'ignore', 'pending', '[4]', '{\"q\":\"d\"}', '{}', 40)",
            )
            .unwrap();

        let all = kernel.list_reviews(DOMAIN, None, 1000).unwrap();
        assert_eq!(all.len(), 3);
        // created_at 升序（10 → 20 → 30），不受插入顺序影响。
        // created_at ascending (10 → 20 → 30), independent of insert order.
        assert_eq!(
            all.iter().map(|r| r.created_at).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
        assert!(all
            .iter()
            .all(|r| r.status == ReviewStatus::Pending || r.status == ReviewStatus::Approved));

        let pending = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Pending), 1000)
            .unwrap();
        assert_eq!(pending.len(), 2);

        let limited = kernel.list_reviews(DOMAIN, None, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].created_at, 10);

        // limit 0 / 超上限 → Validation。
        // limit 0 / over the cap → Validation.
        assert!(kernel.list_reviews(DOMAIN, None, 0).is_err());
        assert!(kernel.list_reviews(DOMAIN, None, 1001).is_err());
    }

    // serde：FeedbackKind/ReviewStatus 的 snake_case 往返（HTTP 批次契约面）。
    // serde: snake_case round-trip for FeedbackKind/ReviewStatus (the HTTP-batch
    // contract surface).
    #[test]
    fn kind_and_status_serde_round_trip() {
        assert_eq!(
            serde_json::to_value(FeedbackKind::Adopt).unwrap(),
            serde_json::json!("adopt")
        );
        assert_eq!(
            serde_json::from_value::<FeedbackKind>(serde_json::json!("adopt")).unwrap(),
            FeedbackKind::Adopt
        );
        assert_eq!(
            serde_json::to_value(ReviewStatus::Pending).unwrap(),
            serde_json::json!("pending")
        );
        assert_eq!(
            serde_json::from_value::<ReviewStatus>(serde_json::json!("pending")).unwrap(),
            ReviewStatus::Pending
        );
    }

    /// 构造一条合法建议输入（subject 以 query 为键）。
    /// Builds one legal suggestion input (query-keyed subject).
    fn suggestion(subject: &str) -> ReviewSuggestionInput {
        ReviewSuggestionInput {
            action: "supplemental_compile".into(),
            source_log_ids_json: "[1,3]".into(),
            subject_json: format!(r#"{{"normalized_query":"{subject}"}}"#),
            reason_json: r#"{"signal":"zero_recall"}"#.into(),
            created_at: 1000,
        }
    }

    // 批3 / A12：批量插入返回实际新插入 review_id；重复 (domain,action,subject)
    // 跳过不报错；重复分析幂等（第二次插不进任何行）。
    // Batch 3 / A12: bulk insert returns the actually newly inserted review_ids;
    // duplicate (domain,action,subject) is skipped without error; repeated
    // analysis is idempotent (the second run inserts nothing).
    #[test]
    fn insert_review_suggestions_is_idempotent_and_returns_new_ids() {
        let kernel = kernel();

        let first = kernel
            .insert_review_suggestions(DOMAIN, &[suggestion("boba"), suggestion("milk tea")])
            .unwrap();
        assert_eq!(first.len(), 2, "both new rows yield ids");
        assert!(first[0] != first[1]);
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 2);

        // 同输入重复分析：全部 UNIQUE 冲突 → 返回空，行数不变。
        // Repeated analysis over the same input: all UNIQUE conflicts → empty
        // result, row count unchanged.
        let replay = kernel
            .insert_review_suggestions(DOMAIN, &[suggestion("boba"), suggestion("milk tea")])
            .unwrap();
        assert!(replay.is_empty());
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 2);

        // 新旧混合：只有新 subject 产生 id。
        // Mixed old/new: only the new subject yields an id.
        let mixed = kernel
            .insert_review_suggestions(DOMAIN, &[suggestion("boba"), suggestion("fruit")])
            .unwrap();
        assert_eq!(mixed.len(), 1);
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 3);

        // 同批重复：批内第二份被跳过，只插一次。
        // Within-batch duplicate: the second copy is skipped, one insert only.
        let intra = kernel
            .insert_review_suggestions(DOMAIN, &[suggestion("latté"), suggestion("latté")])
            .unwrap();
        assert_eq!(intra.len(), 1);
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 4);

        // 其他 domain 的同 subject 不受 UNIQUE 约束（租户隔离）。
        // The same subject under another domain is untouched by the UNIQUE
        // (tenant isolation).
        let other = kernel
            .insert_review_suggestions("ecommerce", &[suggestion("boba")])
            .unwrap();
        assert_eq!(other.len(), 1);

        // 空输入：不做任何写入。
        // Empty input: nothing is written.
        assert!(kernel
            .insert_review_suggestions(DOMAIN, &[])
            .unwrap()
            .is_empty());
    }

    // 批3：非法 action / 非 JSON 字段 / 空 domain → Validation，且不落任何行。
    // Batch 3: illegal action / non-JSON fields / empty domain → Validation,
    // with zero rows persisted.
    #[test]
    fn insert_review_suggestions_validates_inputs() {
        let kernel = kernel();

        let mut bad_action = suggestion("boba");
        bad_action.action = "auto_compile".into();
        let err = kernel
            .insert_review_suggestions(DOMAIN, &[bad_action])
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");

        let mut bad_subject = suggestion("boba");
        bad_subject.subject_json = "not-json".into();
        assert!(matches!(
            kernel
                .insert_review_suggestions(DOMAIN, &[bad_subject])
                .unwrap_err(),
            Error::Validation(_)
        ));

        let mut bad_logs = suggestion("boba");
        bad_logs.source_log_ids_json = "[1,".into();
        assert!(matches!(
            kernel
                .insert_review_suggestions(DOMAIN, &[bad_logs])
                .unwrap_err(),
            Error::Validation(_)
        ));

        let mut bad_reason = suggestion("boba");
        bad_reason.reason_json = String::new();
        assert!(matches!(
            kernel
                .insert_review_suggestions(DOMAIN, &[bad_reason])
                .unwrap_err(),
            Error::Validation(_)
        ));

        assert!(matches!(
            kernel
                .insert_review_suggestions("", &[suggestion("boba")])
                .unwrap_err(),
            Error::Validation(_)
        ));

        // 校验失败发生在事务之前：零行落库。
        // Validation happens before the transaction: zero rows persisted.
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 0);
    }

    // ===== 批4 / A15–A16：审核转换 =====
    // ===== Batch 4 / A15–A16: review transitions =====

    use crate::compile::config::CompilePolicy;
    use crate::compile::hash::schema_to_value;
    use crate::traits::EntitySchema;
    use crate::types::{CompileContext, EntityId, FieldDefinition, FieldType};
    use std::collections::BTreeMap;

    /// 与 compile_store 测试同款的四字段源 schema（raw 夹具必须全覆盖声明字段，
    /// raw_to_facts 才能通过）。
    /// The four-field source schema matching compile_store's tests (the raw
    /// fixture must cover every declared field for raw_to_facts to pass).
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

    /// 合法 supplemental subject：subject 声明与 source_json/dependencies_json
    /// 完全一致（approve 成功路径的输入形状）。
    /// A legal supplemental subject: the subject declaration is fully consistent
    /// with source_json/dependencies_json (the approve-success input shape).
    fn supplemental_subject(revision: u64, price: f64) -> String {
        let raw = raw(revision, price);
        let deps = StoredDependencies {
            context: ctx(),
            policy: policy(),
            schema: schema_to_value(&schema()),
        };
        serde_json::json!({
            "entity_id": raw.id.to_key(),
            "source_revision": raw.source_revision,
            "domain_pack_version": ctx().domain_pack_version,
            "source_json": serde_json::to_string(&raw).unwrap(),
            "dependencies_json": serde_json::to_string(&deps).unwrap(),
        })
        .to_string()
    }

    /// 插入一条审核建议并取回 review_id。
    /// Inserts one review suggestion and returns its review_id.
    fn insert_review(kernel: &SqliteKernel, domain: &str, action: &str, subject: &str) -> i64 {
        kernel
            .insert_review_suggestions(
                domain,
                &[ReviewSuggestionInput {
                    action: action.into(),
                    source_log_ids_json: "[1]".into(),
                    subject_json: subject.into(),
                    reason_json: r#"{"signal":"zero_recall"}"#.into(),
                    created_at: 1000,
                }],
            )
            .unwrap()[0]
    }

    fn insert_supplemental_review(kernel: &SqliteKernel, subject: &str) -> i64 {
        insert_review(kernel, DOMAIN, "supplemental_compile", subject)
    }

    #[derive(QueryableByName)]
    struct TaskProbe {
        #[diesel(sql_type = sql_types::Text)]
        entity_id: String,
        #[diesel(sql_type = sql_types::BigInt)]
        source_revision: i64,
        #[diesel(sql_type = sql_types::Text)]
        status: String,
    }

    // A15 成功路径：pending supplemental approve 在一个事务内完成 admission——
    // compile_tasks 出现新任务（pending、实体/revision 一致），review 回填
    // task_id/status='approved'/审计字段；重复 approve 被状态校验拦截。
    // A15 success path: approving a pending supplemental review runs admission in
    // one transaction — a new compile task appears (pending, entity/revision
    // consistent) and the review is backfilled with
    // task_id/status='approved'/audit fields; a repeated approve is intercepted
    // by the status gate.
    #[test]
    fn approve_supplemental_compiles_and_backfills() {
        let kernel = kernel();
        let review_id = insert_supplemental_review(&kernel, &supplemental_subject(1, 19.0));

        let outcome = kernel.approve_review(review_id, "alice", 2000).unwrap();
        assert_eq!(outcome.review_id, review_id);
        assert_eq!(outcome.status, ReviewStatus::Approved);
        let task_id = outcome.compile_task_id.expect("approval must queue a task");

        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 1);
        let mut conn = kernel.lock_conn().unwrap();
        let task: TaskProbe = diesel::sql_query(
            "SELECT entity_id, source_revision, status FROM compile_tasks WHERE task_id = ?",
        )
        .bind::<sql_types::BigInt, _>(task_id)
        .get_result(&mut *conn)
        .unwrap();
        assert_eq!(task.entity_id, "milk-tea:drink:boba");
        assert_eq!(task.source_revision, 1);
        assert_eq!(task.status, "pending");
        drop(conn);

        let reviews = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Approved), 1000)
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].compile_task_id, Some(task_id));
        assert_eq!(reviews[0].reviewed_by.as_deref(), Some("alice"));
        assert_eq!(reviews[0].reviewed_at, Some(2000));
        assert!(kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Pending), 1000)
            .unwrap()
            .is_empty());

        // 重复 approve（并发场景在单连接 Mutex 串行化后同样落在这条状态校验上）。
        // Repeated approve (under concurrency, serialized by the single-conn
        // Mutex, it lands on the same status gate).
        let err = kernel.approve_review(review_id, "alice", 3000).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 1);
    }

    // spec §8：subject 缺字段/类型不符/自相矛盾/跨租户 → Validation，不猜造；
    // review 仍 pending、compile_tasks 无孤儿。
    // spec §8: missing/ill-typed/self-contradictory/cross-tenant subjects →
    // Validation, never fabricated; the review stays pending and compile_tasks
    // has no orphans.
    #[test]
    fn approve_supplemental_rejects_incomplete_or_foreign_subject() {
        let kernel = kernel();

        // 缺 source_json。
        // Missing source_json.
        let missing = serde_json::json!({
            "entity_id": "milk-tea:drink:boba",
            "source_revision": 1,
            "domain_pack_version": "0.1.0",
            "dependencies_json": "{}",
        })
        .to_string();
        let review_id = insert_supplemental_review(&kernel, &missing);
        let err = kernel.approve_review(review_id, "alice", 2000).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("source_json"), "got {err:?}");

        // source_json 不是 RawEntity。
        // source_json does not parse as a RawEntity.
        let mut bad_source =
            serde_json::from_str::<serde_json::Value>(&supplemental_subject(1, 19.0)).unwrap();
        bad_source["source_json"] = serde_json::json!("not-an-entity");
        let bad_source_id = insert_supplemental_review(&kernel, &bad_source.to_string());
        assert!(kernel.approve_review(bad_source_id, "alice", 2000).is_err());

        // subject 声明与 source_json 矛盾（entity_id 不一致）。
        // The subject declaration contradicts source_json (entity_id mismatch).
        let mut mismatch =
            serde_json::from_str::<serde_json::Value>(&supplemental_subject(1, 19.0)).unwrap();
        mismatch["entity_id"] = serde_json::json!("milk-tea:drink:oolong");
        let mismatch_id = insert_supplemental_review(&kernel, &mismatch.to_string());
        assert!(kernel.approve_review(mismatch_id, "alice", 2000).is_err());

        // 租户一致性：milk-tea 实体的 subject 挂在 ecommerce 的 review 下。
        // Tenant coherence: a milk-tea subject under an ecommerce review.
        let foreign = insert_review(
            &kernel,
            "ecommerce",
            "supplemental_compile",
            &supplemental_subject(1, 19.0),
        );
        let err = kernel.approve_review(foreign, "alice", 2000).unwrap_err();
        assert!(err.to_string().contains("does not belong"), "got {err:?}");

        // 四次失败全部零副作用：review 仍 pending、compile_tasks 无孤儿。
        // All four failures left zero side effects: reviews still pending, no
        // orphan compile tasks.
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 4);
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 0);
    }

    // admission 失败（同 revision 不同 snapshot 的冲突源）→ 整体回滚：review 仍
    // pending、compile_tasks 保持直接 admission 产生的那一条（无孤儿）。
    // Admission failure (a conflicting source with the same revision but a
    // different snapshot) → full rollback: the review stays pending and
    // compile_tasks keeps only the row from the direct admission (no orphans).
    #[test]
    fn approve_supplemental_rolls_back_when_admission_rejects() {
        let kernel = kernel();
        // 直接 admission 建立 revision 1 的 head + 一条合法任务。
        // A direct admission establishes the revision-1 head plus one legal task.
        let prepared = prepare_source(&raw(1, 19.0), &schema(), &policy()).unwrap();
        assert!(matches!(
            kernel
                .admit_compile(&prepared, &ctx(), &policy(), &schema(), false)
                .unwrap(),
            Admission::Queued(_)
        ));

        // 冲突源：同 revision（1）、不同内容（price 20.0 → 不同 snapshot_hash）。
        // Conflicting source: same revision (1), different content (price 20.0 →
        // a different snapshot_hash).
        let review_id = insert_supplemental_review(&kernel, &supplemental_subject(1, 20.0));
        let err = kernel.approve_review(review_id, "alice", 2000).unwrap_err();
        assert!(
            err.to_string().contains("source_revision_conflict"),
            "got {err:?}"
        );

        assert_eq!(
            kernel
                .list_reviews(DOMAIN, Some(ReviewStatus::Pending), 1000)
                .unwrap()
                .len(),
            1,
            "the review must stay pending after rollback"
        );
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 1);
    }

    // A16：query_template 批准只写审计（approved + reviewed_at/by），不写
    // compile_tasks、不改 QUG/配置；重复审核拒绝。
    // A16: approving a query_template only writes the audit (approved +
    // reviewed_at/by), never compile_tasks nor QUG/config changes; repeated
    // reviews are rejected.
    #[test]
    fn approve_query_template_is_audit_only() {
        let kernel = kernel();
        let review_id = insert_review(
            &kernel,
            DOMAIN,
            "query_template",
            r#"{"normalized_query":"波霸奶茶"}"#,
        );

        let outcome = kernel.approve_review(review_id, "ops", 2000).unwrap();
        assert_eq!(outcome.status, ReviewStatus::Approved);
        assert_eq!(
            outcome.compile_task_id, None,
            "query_template must not create compile tasks"
        );
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 0);

        let reviews = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Approved), 1000)
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].reviewed_by.as_deref(), Some("ops"));
        assert_eq!(reviews[0].reviewed_at, Some(2000));
        assert_eq!(reviews[0].compile_task_id, None);

        // 已审核后：approve/ignore 均拒绝（重复审核）。
        // Already reviewed: both approve and ignore are rejected (repeat).
        let err = kernel.approve_review(review_id, "ops", 3000).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(kernel.ignore_review(review_id, "ops", 3000).is_err());
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 1);
    }

    // A16：ignore 只写审计状态（ignored + reviewed_at/by），不校验 subject、不碰
    // compile_tasks；重复 ignore / 已忽略后 approve 拒绝。
    // A16: ignore only writes the audit status (ignored + reviewed_at/by), never
    // validates the subject nor touches compile_tasks; repeated ignore / approve
    // after ignore are rejected.
    #[test]
    fn ignore_review_is_audit_only_and_rejects_repeats() {
        let kernel = kernel();
        // 故意使用畸形 subject 的 supplemental 建议：ignore 不做 subject 校验。
        // A deliberately malformed supplemental subject: ignore performs no
        // subject validation.
        let review_id = insert_supplemental_review(&kernel, r#"{"signal":"only"}"#);

        kernel.ignore_review(review_id, "bob", 3000).unwrap();
        let reviews = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Ignored), 1000)
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].status, ReviewStatus::Ignored);
        assert_eq!(reviews[0].reviewed_by.as_deref(), Some("bob"));
        assert_eq!(reviews[0].reviewed_at, Some(3000));
        assert_eq!(reviews[0].compile_task_id, None);
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 0);

        assert!(kernel.ignore_review(review_id, "bob", 3001).is_err());
        let err = kernel.approve_review(review_id, "alice", 3001).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 1);
    }

    // 输入与目标校验：不存在的 review_id、空 reviewer、action=ignore 的建议不可
    // approve（fail-closed）；全部失败零副作用。
    // Input/target validation: missing review_ids, empty reviewers, and
    // action=ignore suggestions are not approvable (fail-closed); every failure
    // is side-effect free.
    #[test]
    fn review_transitions_validate_inputs_and_targets() {
        let kernel = kernel();

        assert!(kernel.approve_review(424_242, "ops", 1).is_err());
        assert!(kernel.ignore_review(424_242, "ops", 1).is_err());

        let review_id = insert_supplemental_review(&kernel, &supplemental_subject(1, 19.0));
        assert!(kernel.approve_review(review_id, "", 1).is_err());
        assert!(kernel.ignore_review(review_id, "", 1).is_err());

        let ignored = insert_review(&kernel, DOMAIN, "ignore", r#"{"note":"out of scope"}"#);
        let err = kernel.approve_review(ignored, "ops", 2).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("not approvable"), "got {err:?}");
        // action=ignore 的建议走 ignore_review 是合法路径。
        // ignore_review is the legal path for action=ignore suggestions.
        kernel.ignore_review(ignored, "ops", 3).unwrap();

        // supplemental 那条仍 pending；任务表为零。
        // The supplemental one stays pending; the task table stays empty.
        assert_eq!(
            kernel
                .list_reviews(DOMAIN, Some(ReviewStatus::Pending), 1000)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 0);
    }

    // ===== 批5：批量原子插入 / page_exists / 拒绝审计 / pending 计数 =====
    // ===== Batch 5: batch-atomic insert / page_exists / rejection audit /
    // pending count =====

    /// seed 一个 Wiki 页（sqlite.rs 测试同款 frontmatter 路径）。
    /// Seeds a wiki page (same frontmatter path as the sqlite.rs tests).
    fn seed_page(kernel: &SqliteKernel, page_id: &str, status: crate::types::PublishStatus) {
        let page = crate::seed::parse_page(&format!(
            "---\npage_id: {page_id}\nentity_id: {page_id}\ntitle: 波霸奶茶\nentity_type: drink\n---\n\n波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。"
        ))
        .unwrap();
        kernel.seed_pages(&page, DOMAIN, status).unwrap();
    }

    // 批5：批量全新插入 + 混入重复键逐条回放（D5/A4 批量面）。
    // Batch 5: all-new batch insert + per-event replay when duplicates are mixed
    // in (D5/A4 batch face).
    #[test]
    fn batch_insert_new_events_and_replay_duplicates() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        let e1 = click_input(log_id);
        let mut e2 = click_input(log_id);
        e2.idempotency_key = "checkout-7-2".into();
        let first = kernel
            .insert_feedback_batch_idempotent(&[e1.clone(), e2], 1000)
            .unwrap();
        assert_eq!(first.len(), 2);
        assert!(!first[0].replayed && !first[1].replayed);
        assert_eq!(first[0].received_at, 1000);

        // 二次批量：k1 重复（回放原 id/时间），k3 新插入。
        // Second batch: k1 duplicates (replays the original id/time), k3 is new.
        let mut e3 = click_input(log_id);
        e3.idempotency_key = "checkout-7-3".into();
        let second = kernel
            .insert_feedback_batch_idempotent(&[e1, e3], 2000)
            .unwrap();
        assert!(second[0].replayed);
        assert_eq!(second[0].event_id, first[0].event_id);
        assert_eq!(second[0].received_at, 1000, "replay keeps received_at");
        assert!(!second[1].replayed);
        assert_eq!(kernel.row_counts().unwrap()["feedback_events"], 3);
    }

    // 批5：批内任一事件 log 不存在 → 整批回滚、零行落库（禁止部分成功）。
    // Batch 5: any event with a missing log rolls the whole batch back with zero
    // rows persisted (partial success forbidden).
    #[test]
    fn batch_with_missing_log_rolls_back_everything() {
        let kernel = kernel();
        let log_id = insert_log(&kernel, DOMAIN, 100, 3);

        let good = click_input(log_id);
        let mut bad = click_input(log_id);
        bad.idempotency_key = "bad-log".into();
        bad.log_id = 999_999;
        let err = kernel
            .insert_feedback_batch_idempotent(&[good.clone(), bad], 1000)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert_eq!(kernel.row_counts().unwrap()["feedback_events"], 0);

        // 回滚后同批好事件可正常插入（事务无残留状态）。
        // After the rollback the good event inserts normally (no leftover tx
        // state).
        kernel
            .insert_feedback_batch_idempotent(&[good], 1100)
            .unwrap();
        assert_eq!(kernel.row_counts().unwrap()["feedback_events"], 1);
    }

    // 批5：page_exists 只认 accepted（candidate/quarantined/缺失均 false）。
    // Batch 5: page_exists accepts only accepted pages (candidate/quarantined/
    // missing are all false).
    #[test]
    fn page_exists_requires_accepted_status() {
        let kernel = kernel();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            crate::types::PublishStatus::Accepted,
        );
        seed_page(
            &kernel,
            "milk-tea:drink:candidate",
            crate::types::PublishStatus::Candidate,
        );
        seed_page(
            &kernel,
            "milk-tea:drink:quarantined",
            crate::types::PublishStatus::Quarantined,
        );

        assert!(kernel.page_exists(DOMAIN, "milk-tea:drink:boba").unwrap());
        assert!(!kernel
            .page_exists(DOMAIN, "milk-tea:drink:candidate")
            .unwrap());
        assert!(!kernel
            .page_exists(DOMAIN, "milk-tea:drink:quarantined")
            .unwrap());
        assert!(!kernel.page_exists(DOMAIN, "milk-tea:drink:ghost").unwrap());
        // 其他 domain 的同 page_id 不算存在（精确匹配口径）。
        // The same page_id under another domain does not count (exact match).
        assert!(!kernel.page_exists("other", "milk-tea:drink:boba").unwrap());
    }

    // 批5：拒绝审计行落库 + pending 计数只认 pending。
    // Batch 5: rejection-audit rows persist + the pending count only counts
    // pending.
    #[test]
    fn records_rejections_and_counts_pending_reviews() {
        let kernel = kernel();
        use FeedbackRejectionReason as R;
        kernel
            .record_feedback_rejection(Some(DOMAIN), R::PayloadTooLarge, 65_537, 100)
            .unwrap();
        kernel
            .record_feedback_rejection(None, R::EventCountTooLarge, 4096, 101)
            .unwrap();
        kernel
            .record_feedback_rejection(None, R::FieldTooLarge, 4097, 102)
            .unwrap();
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["feedback_rejections"], 3);
        assert_eq!(counts["feedback_events"], 0);
        // 负 payload_bytes → Validation（审计不做无意义计数）。
        // Negative payload_bytes → Validation (no meaningless audit counts).
        assert!(kernel
            .record_feedback_rejection(None, R::PayloadTooLarge, -1, 103)
            .is_err());

        assert_eq!(kernel.count_pending_reviews().unwrap(), 0);
        kernel
            .insert_review_suggestions(
                DOMAIN,
                &[ReviewSuggestionInput {
                    action: "ignore".into(),
                    source_log_ids_json: "[1]".into(),
                    subject_json: r#"{"q":"a"}"#.into(),
                    reason_json: "{}".into(),
                    created_at: 1,
                }],
            )
            .unwrap();
        assert_eq!(kernel.count_pending_reviews().unwrap(), 1);
    }
}
