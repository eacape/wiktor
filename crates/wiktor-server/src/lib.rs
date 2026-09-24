//! # Wiktor Server
//!
//! Step 6 批5 最小 HTTP 面（spec `step6-feedback-loop.md` §1/§3 D1–D9、§7、
//! §10 A5–A8/A17、§11 批5）：`POST /feedback`、`GET /health`、`GET /metrics`。
//! 批1 占位壳由本实现替换；批6 CLI（如引入 `wiktor serve`）只调用
//! [`build_router`]。
//! The Step 6 batch-5 minimal HTTP surface (spec `step6-feedback-loop.md`
//! §1/§3 D1–D9, §7, §10 A5–A8/A17, §11 batch 5): `POST /feedback`,
//! `GET /health`, `GET /metrics`. The batch-1 placeholder shell is replaced by
//! this implementation; a future batch-6 CLI `wiktor serve` would only call
//! [`build_router`].
//!
//! ## 依赖边界（A1）
//! ## Dependency boundaries (A1)
//!
//! - 依赖 `wiktor-core`（存储）与 `wiktor-feedback`（`FeedbackStore` trait 面，
//!   单向允许）；`wiktor-core`/`wiktor-feedback` 均不得反向依赖本 crate；
//! - Depends on `wiktor-core` (storage) and `wiktor-feedback` (the
//!   `FeedbackStore` trait face, a one-way allowance); neither may depend back
//!   on this crate.
//!
//! ## `POST /feedback` 流水线（§7.1 顺序，禁止乱序）
//! ## The `POST /feedback` pipeline (§7.1 order, never reordered)
//!
//! 1. body 限额 64 KiB（[`body_limit_middleware`]，超限 413 + `payload_too_large`
//!    审计行 + 计数）；
//! 2. Bearer 认证（[`auth::auth_middleware`]，401）；
//! 3. 租户校验（key 允许 domain 与 body.domain 精确匹配，403）；
//! 4. 固定窗口限流（每 (domain, key_label) 60s/120 次，429 + Retry-After）；
//! 5. schema 校验（413 事件数/字段预算 + 审计行；422 事件级非法）；
//! 6. store 事务（整批原子：任一事件失败整批回滚；全部重复 → 200 全 replayed）。
//! 1. body limit 64 KiB ([`body_limit_middleware`]; over → 413 + a
//!    `payload_too_large` audit row + counter);
//! 2. Bearer authentication ([`auth::auth_middleware`], 401);
//! 3. tenant check (the key's allowed domain exactly matches body.domain, 403);
//! 4. fixed-window rate limit (120 per (domain, key_label) 60s, 429 +
//!    Retry-After);
//! 5. schema validation (413 event-count/field budget + audit rows; 422
//!    event-level illegality);
//! 6. the store transaction (batch-atomic: any failing event rolls the whole
//!    batch back; an all-duplicate batch → 200, all replayed).
//!
//! 上层拍板偏差（记入 spec §12）：不要求 `Idempotency-Key` header（body 内每
//! 事件的 `idempotency_key` 是唯一幂等源）；page_id 校验降级为「该 domain 下
//! accepted 页真实存在」（query_logs 未存结果快照）；log 不存在/domain 不匹配/
//! page 不存在统一 422（spec §7 错误码表的 403 分支按本批指令让位）。
//! Upstream deviations (recorded into spec §12): no `Idempotency-Key` header is
//! required (each event's body `idempotency_key` is the single idempotency
//! source); the page_id check is downgraded to "the page really exists as an
//! accepted page of this domain" (query_logs keeps no result snapshot); missing
//! log / domain mismatch / missing page all answer 422 (the spec §7 table's 403
//! branch yields to this batch's instructions).
//!
//! 锁纪律：kernel 是同步 API，handler 在多线程 runtime 上直接调用短事务
//! （spec §2：数据库锁不得跨 await——本 crate 无任何 DB 锁跨越 await 点）。
//! Lock discipline: the kernel is a synchronous API and handlers call short
//! transactions directly on the multi-threaded runtime (spec §2: DB locks are
//! never held across await — no DB lock in this crate crosses an await point).

pub mod auth;
pub mod error;
pub mod metrics;
pub mod rate_limit;
pub mod state;
#[cfg(test)]
mod tests;

// Step 7 落地依赖 #7（spec step7 §3 D1/D12）：tonic gRPC 服务面。proto 生成
// 代码经 tonic::include_proto! 引入（见 grpc.rs），六 service 实现放
// services/ 子模块。core 不依赖本模块（§1 非目标）。
// Step 7 dependency #7 (spec step7 §3 D1/D12): the tonic gRPC service surface.
// The proto-generated code is pulled in via tonic::include_proto! (see
// grpc.rs); the six service implementations live under services/. core never
// depends on this module (§1 non-goal).
pub mod grpc;
pub mod http_search;
pub mod services;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use wiktor_core::types::error::Error;
use wiktor_feedback::{FeedbackEventInput, FeedbackKind, FeedbackRejectionReason, FeedbackStore};

use crate::auth::AuthedKey;
use crate::metrics::RejectionReason;
use crate::state::ServerState;

// ===== 输入预算（spec §7.1 请求限制；kernel 侧同规则先行校验，此处复述以给出
// 带索引的清晰错误面）=====
// ===== Input budgets (spec §7.1 request limits; the kernel validates the same
// rules first — restated here for a clearer per-index error surface) =====

/// 请求 body UTF-8 字节上限 64 KiB（D9；恰好 65536 可接受）。
/// The request-body UTF-8 cap of 64 KiB (D9; exactly 65536 is acceptable).
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// events 数组 1..=100（D9；空数组与 >100 均为 413）。
/// The events array 1..=100 (D9; both empty and >100 answer 413).
pub const MAX_EVENTS: usize = 100;
/// metadata 序列化上限 4 KiB（spec §5，与 core kernel 同值）。
/// The metadata serialization cap of 4 KiB (spec §5, same value as the core
/// kernel).
const MAX_METADATA_BYTES: usize = 4 * 1024;
/// idempotency_key 1..=128 Unicode scalar 且无控制符（spec §7；非 ASCII 会被
/// kernel 的 ASCII 可见字符规则拒绝，同样归 422）。
/// idempotency_key of 1..=128 Unicode scalars without control characters (spec
/// §7; non-ASCII is rejected by the kernel's ASCII-visible rule, also as 422).
const MAX_KEY_CHARS: usize = 128;
/// click/adopt page_id 非空且 ≤512（spec §7）。
/// click/adopt page_id non-empty and ≤512 (spec §7).
const MAX_PAGE_ID_CHARS: usize = 512;

/// /health 与 /metrics 的 API 契约版本串（A17 固定值）。
/// The API contract version string of /health and /metrics (the fixed A17
/// value).
pub const SCHEMA_VERSION: &str = "step6-v1";

// ===== 请求/响应 DTO（spec §7.1 形状；deny_unknown_fields 保证顶层与事件级
// 形状错误分别落 400/422，不做静默忽略）=====
// ===== Request/response DTOs (spec §7.1 shapes; deny_unknown_fields routes
// top-level vs event-level shape errors to 400/422 respectively — never
// silently ignored) =====

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackRequest {
    domain: String,
    events: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventDto {
    idempotency_key: String,
    log_id: i64,
    kind: FeedbackKind,
    /// click/adopt 必填；rate 可选（kernel 同规则）。
    /// Required for click/adopt; optional for rate (same kernel rule).
    page_id: Option<String>,
    /// 仅 rate 使用且必填 1..=5（kernel 同规则）。
    /// Only for rate, required 1..=5 there (same kernel rule).
    rating: Option<u8>,
    /// 缺省 `{}`（与 0005 DDL `DEFAULT '{}'` 对齐）；显式 null → 422。
    /// Defaults to `{}` (aligned with the 0005 DDL `DEFAULT '{}'`); an explicit
    /// null → 422.
    #[serde(default = "default_metadata")]
    metadata: serde_json::Value,
}

fn default_metadata() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Serialize)]
struct IngestResponse {
    domain: String,
    events: Vec<EventAck>,
    accepted: usize,
}

#[derive(Debug, Serialize)]
struct EventAck {
    idempotency_key: String,
    event_id: i64,
    replayed: bool,
    received_at: i64,
}

/// 统一错误 JSON（spec §7.1：message 不含 secret、SQL、原始 metadata；500 的
/// diesel 细节只进 tracing，不进响应）。
/// The unified error JSON (spec §7.1: messages carry no secret, SQL, or raw
/// metadata; diesel details of a 500 go to tracing only, never the response).
pub fn error_json(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"code": code, "message": message.into()}})),
    )
        .into_response()
}

/// serde 错误的安全日志形态（批7 敏感字段审计；只影响 tracing，不影响响应——
/// 响应本就是服务端固定文案）：syntax/eof 类消息只含位置信息（无客户端数据）
/// 可原样保留；data/io 类消息可能内嵌客户端值片段（如 `invalid type: string
/// "…"`、`unknown field "<长字段名>"`），只落分类名，不落原文——原始
/// metadata/事件值不进日志。
/// The log-safe shape of a serde error (batch-7 sensitive-field audit; tracing
/// only — HTTP responses are already fixed server-side text): syntax/eof
/// messages carry position info only (no client data) and are kept verbatim;
/// data/io messages can embed client-controlled value fragments (e.g.
/// `invalid type: string "…"`, `unknown field "<long field name>"`), so only
/// the category name is logged — raw metadata / event values never reach the
/// logs.
fn safe_serde_detail(e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    match e.classify() {
        Category::Syntax | Category::Eof => e.to_string(),
        Category::Data | Category::Io => {
            format!(
                "{:?} serde error (value-bearing details withheld)",
                e.classify()
            )
        }
    }
}

/// 写一条 413 拒绝审计行（尽力而为：审计写失败不掩盖客户端的 413，只记
/// store_errors 计数 + tracing，A6「不写 event、计数可观测」）。
/// Writes one 413 rejection-audit row (best effort: an audit-write failure
/// never masks the client's 413; it only records the store_errors counter plus
/// tracing — A6 "no event written, counts observable").
fn record_rejection(
    state: &ServerState,
    domain: Option<&str>,
    reason: FeedbackRejectionReason,
    payload_bytes: i64,
) {
    if let Err(e) = state.kernel.record_feedback_rejection(
        domain,
        reason,
        payload_bytes,
        state.clock.now_secs(),
    ) {
        tracing::error!(
            reason = reason.as_str(),
            error = %e,
            "failed to record the feedback_rejections audit row"
        );
        state.metrics.record_store_error();
    }
}

/// 422 INVALID_FEEDBACK（带事件索引；why 为服务端固定文案，不含事件值）。
/// 422 INVALID_FEEDBACK (with the event index; `why` is fixed server-side
/// wording without event values).
fn invalid_feedback(state: &ServerState, index: usize, why: &str) -> Response {
    state
        .metrics
        .record_rejected(RejectionReason::InvalidFeedback);
    error_json(
        StatusCode::UNPROCESSABLE_ENTITY,
        "INVALID_FEEDBACK",
        format!("events[{index}]: {why}"),
    )
}

/// 流水线步骤 1：body 限额 64 KiB middleware（最外层，先于认证；超限不读后续
/// 内容、写 `payload_too_large` 审计行并回 413）。
/// Pipeline step 1: the 64 KiB body-limit middleware (outermost, before auth;
/// over the limit it reads nothing further, writes a `payload_too_large` audit
/// row and answers 413).
///
/// 读取策略：`axum::body::to_bytes(body, 64 KiB)` 一次性受限缓冲——恰好 64 KiB
/// 可接受，超一字节即失败（A6 边界）。任何 body 读取失败一律按超预算处理
/// （fail-closed）。审计行的 payload_bytes 记录预算值（真实溢出大小未知）。
/// Read strategy: one limited buffering pass via
/// `axum::body::to_bytes(body, 64 KiB)` — exactly 64 KiB is accepted, one byte
/// over fails (the A6 boundary). Any body-read failure is treated as over
/// budget (fail-closed). The audit row's payload_bytes records the budget (the
/// true overflow size is unknowable).
async fn body_limit_middleware(
    State(state): State<Arc<ServerState>>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    // Content-Length 预检：声明超限的直接拒绝，不再读取（快速路径）。
    // Content-Length pre-check: declared-oversized requests are rejected before
    // any read (fast path).
    if let Some(len) = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(len) = len.parse::<u64>() {
            if len > MAX_BODY_BYTES as u64 {
                state
                    .metrics
                    .record_rejected(RejectionReason::PayloadTooLarge);
                record_rejection(
                    &state,
                    None,
                    FeedbackRejectionReason::PayloadTooLarge,
                    MAX_BODY_BYTES as i64,
                );
                return error_json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "PAYLOAD_TOO_LARGE",
                    "request body exceeds the 64 KiB limit",
                );
            }
        }
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            state
                .metrics
                .record_rejected(RejectionReason::PayloadTooLarge);
            record_rejection(
                &state,
                None,
                FeedbackRejectionReason::PayloadTooLarge,
                MAX_BODY_BYTES as i64,
            );
            return error_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                "request body exceeds the 64 KiB limit",
            );
        }
    };
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// `POST /feedback`（spec §7.1；流水线步骤 2 已在认证 middleware 完成）。
/// `POST /feedback` (spec §7.1; pipeline step 2 already happened in the auth
/// middleware).
async fn post_feedback(
    State(state): State<Arc<ServerState>>,
    Extension(key): Extension<AuthedKey>,
    body: axum::body::Bytes,
) -> Response {
    // —— 步骤 3 前置：JSON 解析与顶层形状（400；syntax/shape 同码，不泄漏细节。
    //    租户校验需要 body.domain，故解析先于租户比较，但 400 优先于 403/429）——
    // —— Step 3 precondition: JSON parse + top-level shape (400; syntax/shape
    //    share the code, no details leaked. The tenant check needs body.domain,
    //    so parsing precedes it, and 400 takes precedence over 403/429) ——
    let request = match serde_json::from_slice::<FeedbackRequest>(&body) {
        Ok(request) => request,
        Err(e) => {
            // 审计批7：data 类 serde 消息可能内嵌客户端值片段，日志只落安全
            // 形态（见 safe_serde_detail）；响应保持固定文案。
            // Batch-7 audit: data-category serde messages can embed
            // client-controlled value fragments; logs carry only the safe shape
            // (see safe_serde_detail) while the response stays fixed text.
            tracing::debug!(error = %safe_serde_detail(&e), "feedback request failed JSON/shape parse");
            state.metrics.record_rejected(RejectionReason::InvalidJson);
            return error_json(
                StatusCode::BAD_REQUEST,
                "INVALID_JSON",
                "request body is not a valid feedback JSON object",
            );
        }
    };

    // —— 步骤 3：租户校验（D7 精确匹配；403 不泄漏 key 的 domain 集合）——
    // —— Step 3: tenant check (D7 exact match; 403 never leaks the key's
    //    domain set) ——
    if request.domain != key.domain {
        state
            .metrics
            .record_rejected(RejectionReason::TenantForbidden);
        return error_json(
            StatusCode::FORBIDDEN,
            "TENANT_FORBIDDEN",
            "api key is not allowed for this domain",
        );
    }

    // —— 步骤 4：固定窗口限流（D8；key 用 BLAKE3 标签，原文不参与）——
    // —— Step 4: the fixed-window limiter (D8; the key participates via its
    //    BLAKE3 label only) ——
    if let Err(remaining) = state.limiter.try_acquire(&request.domain, &key.label) {
        state.metrics.record_rate_limited();
        let mut response = error_json(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            format!("rate limit exceeded; retry after {remaining} seconds"),
        );
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(remaining));
        return response;
    }

    // —— 步骤 5a：事件数预算（413 + event_count_too_large 审计行 + 计数）——
    // —— Step 5a: the event-count budget (413 + an event_count_too_large audit
    //    row + counter) ——
    if request.events.is_empty() || request.events.len() > MAX_EVENTS {
        state
            .metrics
            .record_rejected(RejectionReason::EventCountTooLarge);
        record_rejection(
            &state,
            Some(&request.domain),
            FeedbackRejectionReason::EventCountTooLarge,
            body.len() as i64,
        );
        return error_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PAYLOAD_TOO_LARGE",
            format!("events must contain 1..={MAX_EVENTS} items"),
        );
    }

    // —— 步骤 5b：逐事件 schema 校验（422；任一失败 → 整批拒绝，不触达 store）——
    // —— Step 5b: per-event schema validation (422; any failure rejects the
    //    whole batch before the store is touched) ——
    let mut inputs: Vec<FeedbackEventInput> = Vec::with_capacity(request.events.len());
    for (index, raw) in request.events.iter().enumerate() {
        // 事件级反序列化失败（缺字段/类型/未知字段/未知 kind）→ 422；审计批7：
        // serde 消息可能内嵌客户端值片段（错误类型的字符串值/长字段名），日志
        // 只落安全形态（safe_serde_detail），客户端仍拿到固定 422 文案 + 索引。
        // Event-level deserialize failures (missing field / wrong type / unknown
        // field / unknown kind) → 422; batch-7 audit: serde messages can embed
        // client-controlled value fragments (wrong-typed string values / long
        // field names), so logs carry only the safe shape (safe_serde_detail)
        // while the client still gets the fixed 422 text plus the index.
        let dto: EventDto = match serde_json::from_value(raw.clone()) {
            Ok(dto) => dto,
            Err(e) => {
                tracing::debug!(index, error = %safe_serde_detail(&e), "feedback event failed schema parse");
                return invalid_feedback(&state, index, "failed schema validation");
            }
        };
        // idempotency_key：1..=128 scalar 且无控制符（kernel 的 ASCII 可见规则
        // 仍会在 store 阶段兜底拒绝非 ASCII）。
        // idempotency_key: 1..=128 scalars, no control characters (the kernel's
        // ASCII-visible rule still backstops non-ASCII at the store stage).
        let key_chars = dto.idempotency_key.chars().count();
        if key_chars == 0
            || key_chars > MAX_KEY_CHARS
            || dto.idempotency_key.chars().any(char::is_control)
        {
            return invalid_feedback(
                &state,
                index,
                "idempotency_key must be 1..=128 visible characters",
            );
        }
        // log_id > 0（kernel 同规则）。
        // log_id > 0 (same kernel rule).
        if dto.log_id <= 0 {
            return invalid_feedback(&state, index, "log_id must be > 0");
        }
        // kind/rating/page 组合（D3；kernel 同规则）。
        // kind/rating/page combination (D3; same kernel rule).
        match dto.kind {
            FeedbackKind::Rate => match dto.rating {
                Some(r) if (1..=5).contains(&r) => {}
                _ => {
                    return invalid_feedback(&state, index, "rate events require rating 1..=5");
                }
            },
            FeedbackKind::Click | FeedbackKind::Adopt => {
                if dto.rating.is_some() {
                    return invalid_feedback(
                        &state,
                        index,
                        "click/adopt events must not carry a rating",
                    );
                }
                let page_chars = dto.page_id.as_deref().unwrap_or_default().chars().count();
                if page_chars == 0 || page_chars > MAX_PAGE_ID_CHARS {
                    return invalid_feedback(
                        &state,
                        index,
                        "click/adopt events require page_id of 1..=512 characters",
                    );
                }
            }
        }
        // metadata：只允许对象且序列化 ≤4 KiB（超限 413 + field_too_large）。
        // metadata: an object only, ≤4 KiB serialized (over → 413 +
        // field_too_large).
        if !dto.metadata.is_object() {
            return invalid_feedback(&state, index, "metadata must be a JSON object");
        }
        let metadata_size = match serde_json::to_vec(&dto.metadata) {
            Ok(bytes) => bytes.len(),
            Err(e) => {
                // 审计批7：同 safe_serde_detail 口径（该路径实际不可达——Value
                // 序列化恒成功——但保持同一安全形态）。
                // Batch-7 audit: same safe_serde_detail policy (this path is
                // effectively unreachable — Value serialization always
                // succeeds — but the shape stays uniform).
                tracing::debug!(index, error = %safe_serde_detail(&e), "metadata serialization failed");
                return invalid_feedback(&state, index, "metadata must be a JSON object");
            }
        };
        if metadata_size > MAX_METADATA_BYTES {
            state
                .metrics
                .record_rejected(RejectionReason::FieldTooLarge);
            record_rejection(
                &state,
                Some(&request.domain),
                FeedbackRejectionReason::FieldTooLarge,
                body.len() as i64,
            );
            return error_json(
                StatusCode::PAYLOAD_TOO_LARGE,
                "PAYLOAD_TOO_LARGE",
                format!("metadata must not exceed {MAX_METADATA_BYTES} bytes"),
            );
        }
        inputs.push(FeedbackEventInput {
            idempotency_key: dto.idempotency_key,
            domain: request.domain.clone(),
            log_id: dto.log_id,
            kind: dto.kind,
            page_id: dto.page_id,
            rating: dto.rating,
            metadata: dto.metadata,
        });
    }

    // —— 步骤 5c：accepted 页存在性（上层拍板口径；仅 click/adopt 校验）——
    // —— Step 5c: accepted-page existence (the upstream decision; only
    //    click/adopt are checked) ——
    let mut referenced_pages: BTreeSet<&str> = BTreeSet::new();
    for input in &inputs {
        if matches!(input.kind, FeedbackKind::Click | FeedbackKind::Adopt) {
            if let Some(page) = input.page_id.as_deref() {
                referenced_pages.insert(page);
            }
        }
    }
    for page in referenced_pages {
        match state.kernel.page_exists(&request.domain, page) {
            Ok(true) => {}
            Ok(false) => {
                return invalid_feedback(
                    &state,
                    0,
                    "page_id is not an accepted page of this domain",
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "page_exists lookup failed");
                state.metrics.record_store_error();
                return error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "STORE_ERROR",
                    "feedback store transaction failed",
                );
            }
        }
    }

    // —— 步骤 6：store 事务（整批原子；Validation → 422，其余 → 500）——
    // —— Step 6: the store transaction (batch-atomic; Validation → 422, all
    //    else → 500) ——
    let now = state.clock.now_secs();
    match state.kernel.insert_batch_idempotent(&inputs, now) {
        Ok(ingested) => {
            // 计数（A17）：新事件 → ingested，重复回放 → replayed（D5）。
            // Counters (A17): new events → ingested, duplicate replays →
            // replayed (D5).
            let new_events = ingested.iter().filter(|r| !r.replayed).count();
            state.metrics.add_ingested(new_events as u64);
            state
                .metrics
                .add_replayed((ingested.len() - new_events) as u64);
            let accepted = ingested.len();
            let acks: Vec<EventAck> = ingested
                .into_iter()
                .zip(inputs)
                .map(|(row, input)| EventAck {
                    idempotency_key: input.idempotency_key,
                    event_id: row.event_id,
                    replayed: row.replayed,
                    received_at: row.received_at,
                })
                .collect();
            (
                StatusCode::OK,
                Json(IngestResponse {
                    domain: request.domain,
                    events: acks,
                    accepted,
                }),
            )
                .into_response()
        }
        Err(e @ Error::Validation(_)) => {
            // kernel Validation 消息只含 log_id/domain/计数等自有字段（无
            // secret/SQL/metadata），可安全透传；整批已被 kernel 回滚。
            // Kernel validation messages only carry our own fields (log_id,
            // domains, counts — no secret/SQL/metadata) and are safe to pass
            // through; the whole batch was already rolled back by the kernel.
            state
                .metrics
                .record_rejected(RejectionReason::InvalidFeedback);
            error_json(
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_FEEDBACK",
                e.to_string(),
            )
        }
        Err(e) => {
            // 数据库/序列化错误细节只进 tracing（消息可能含 SQL 片段）。
            // Database/serialization details go to tracing only (messages can
            // contain SQL fragments).
            tracing::error!(error = %e, "feedback store transaction failed");
            state.metrics.record_store_error();
            error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORE_ERROR",
                "feedback store transaction failed",
            )
        }
    }
}

/// `GET /health`（A17：无认证；SQLite 可用 → 200，否则 503）。
/// `GET /health` (A17: unauthenticated; SQLite available → 200, else 503).
async fn health(State(state): State<Arc<ServerState>>) -> Response {
    match state.health.check() {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "ok", "schema_version": SCHEMA_VERSION})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVER_NOT_READY",
                "storage unavailable",
            )
        }
    }
}

/// `GET /metrics`（A17：Prometheus text 固定六指标名，无用户可控 label）。
/// `GET /metrics` (A17: Prometheus text with the fixed six metric names and no
/// user-controlled labels).
async fn metrics_handler(State(state): State<Arc<ServerState>>) -> Response {
    // review_pending 是唯一需要 DB 的指标：只读短查询，不阻塞写事务（§7.2）。
    // review_pending is the only DB-backed metric: one short read-only query
    // that never blocks write transactions (§7.2).
    let pending = match state.kernel.count_pending_reviews() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, "count_pending_reviews failed");
            return error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVER_NOT_READY",
                "storage unavailable",
            );
        }
    };
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.render(pending),
    )
        .into_response()
}

/// 构建路由（spec §7）：/feedback 套 body-limit → auth 两层 middleware
/// （layer 后加者为外层），/health 与 /metrics 无认证。
/// Builds the router (spec §7): /feedback carries the body-limit → auth
/// middleware pair (the later-added layer is the outer one); /health and
/// /metrics are unauthenticated.
pub fn build_router(state: Arc<ServerState>) -> Router {
    let feedback = Router::new()
        .route("/feedback", post(post_feedback))
        .layer(from_fn_with_state(state.clone(), auth::auth_middleware))
        .layer(from_fn_with_state(state.clone(), body_limit_middleware));
    // Step7：GET /search 读口（只读 FTS，认证 + search 方法授权）。
    // Step7: the GET /search read surface (read-only FTS, authenticated with
    // the search method permission).
    let search = Router::new()
        .route("/search", get(http_search::search))
        .layer(from_fn_with_state(state.clone(), auth::auth_middleware));
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_handler))
        .merge(feedback)
        .merge(search)
        .with_state(state)
}
