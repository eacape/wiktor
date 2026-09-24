//! 批5 HTTP 验收测试（spec `step6-feedback-loop.md` §10 A5–A8/A17 + D5 幂等
//! 回放）：tower oneshot 驱动，不联网、不落盘（in-memory kernel + MockClock）。
//! Batch-5 HTTP acceptance tests (spec §10 A5–A8/A17 + the D5 idempotent
//! replay): driven by tower oneshot — no network, no disk (in-memory kernel +
//! MockClock).

use std::sync::Arc;

use crate::state::{ApiKeys, KernelHealthCheck, MockClock, ServerState};
use crate::{build_router, SCHEMA_VERSION};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiktor_core::kernel::QueryLogInsert;
use wiktor_core::types::PublishStatus;
use wiktor_core::SqliteKernel;

const DOMAIN: &str = "milk-tea";
const SECRET: &str = "s3cret-milk-tea";
const PAGE_ID: &str = "milk-tea:drink:boba";

/// 测试环境：in-memory kernel（accepted 页 + 两个 domain 的 query log）+
/// 可注入时钟 + 已建路由。
/// The test environment: an in-memory kernel (one accepted page plus query
/// logs in two domains) + an injectable clock + a built router.
struct Env {
    kernel: Arc<SqliteKernel>,
    router: Router,
    clock: Arc<MockClock>,
    log_id: i64,
    other_log_id: i64,
}

fn env() -> Env {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let page = wiktor_core::seed::parse_page(&format!(
        "---\npage_id: {PAGE_ID}\nentity_id: {PAGE_ID}\ntitle: 波霸奶茶\nentity_type: drink\n---\n\n波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。"
    ))
    .unwrap();
    kernel
        .seed_pages(&page, DOMAIN, PublishStatus::Accepted)
        .unwrap();
    let log_id = insert_log(&kernel, DOMAIN);
    let other_log_id = insert_log(&kernel, "other");
    let clock = MockClock::new(3600);
    let state = Arc::new(ServerState::with_parts(
        kernel.clone(),
        keys(),
        clock.clone(),
        Arc::new(KernelHealthCheck {
            kernel: kernel.clone(),
        }),
    ));
    let router = build_router(state);
    Env {
        kernel,
        router,
        clock,
        log_id,
        other_log_id,
    }
}

fn keys() -> ApiKeys {
    // Step7 D2：HTTP feedback 测试沿用旧格式 key（仅 feedback 权限），
    // 语义不变。
    // Step7 D2: the HTTP feedback tests reuse the legacy-format key (feedback
    // only), semantics unchanged.
    ApiKeys::parse_legacy(r#"{"milk-tea":"s3cret-milk-tea"}"#).unwrap()
}

fn insert_log(kernel: &SqliteKernel, domain: &str) -> i64 {
    kernel
        .insert_query_log(&QueryLogInsert {
            query_text: "波霸奶茶",
            query_json: "{}",
            rewritten_json: None,
            rewrite_failure: false,
            hit_count: 3,
            latency_ms: 12,
            domain,
            candidate_empty_initial: false,
            relaxation_attempted: false,
            relaxation_succeeded: false,
        })
        .unwrap()
}

/// POST /feedback 请求（带 Content-Length，触发 middleware 的预检快路径）。
/// A POST /feedback request (with Content-Length, exercising the middleware's
/// fast-path pre-check).
fn post_request(body: Vec<u8>, bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/feedback")
        .header("content-type", "application/json")
        .header("content-length", body.len());
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// 发送并收集（状态码 + 响应头 + 原始字节）。
/// Sends and collects (status + headers + raw bytes).
async fn send(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = router.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

async fn send_json(router: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let (status, _headers, bytes) = send(router, req).await;
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn error_code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or("")
}

/// 单事件 rate 请求体（幂等重放/限流测试的最小合法载荷）。
/// A single-event rate body (the minimal legal payload for replay/rate-limit
/// tests).
fn rate_body(log_id: i64, key: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [{"idempotency_key": key, "log_id": log_id, "kind": "rate", "rating": 5, "metadata": {}}]
    }))
    .unwrap()
}

/// 构造恰好 `target` 字节的合法多事件请求体：metadata pad 均摊（每个 metadata
/// 序列化 ≤ 4096 字节），全部为指向 accepted 页的 click 事件。
/// Builds a legal multi-event body of exactly `target` bytes: the metadata pad
/// is spread across events (each metadata stays ≤ 4096 bytes serialized), all
/// click events pointing at the accepted page.
fn body_of_exact_size(target: usize, n_events: usize, log_id: i64) -> Vec<u8> {
    const PAD_CAP: usize = 4000; // ≤ 4096 - len({"pad":""}) / ≤ 4096 - len({"pad":""})
    let make = |pads: &[usize]| -> Vec<u8> {
        let events: Vec<Value> = (0..n_events)
            .map(|i| {
                json!({
                    "idempotency_key": format!("pad-{i}"),
                    "log_id": log_id,
                    "kind": "click",
                    "page_id": PAGE_ID,
                    "metadata": {"pad": "x".repeat(pads[i])}
                })
            })
            .collect();
        serde_json::to_vec(&json!({"domain": DOMAIN, "events": events})).unwrap()
    };
    let zero_pads = vec![0usize; n_events];
    let base_len = make(&zero_pads).len();
    assert!(target >= base_len, "target {target} below base {base_len}");
    let mut needed = target - base_len;
    let mut pads = vec![0usize; n_events];
    for pad in pads.iter_mut() {
        let take = needed.min(PAD_CAP);
        *pad = take;
        needed -= take;
        if needed == 0 {
            break;
        }
    }
    assert_eq!(
        needed, 0,
        "target {target} unreachable with {n_events} events"
    );
    let body = make(&pads);
    assert_eq!(body.len(), target, "exact-size construction failed");
    body
}

// ===== A5：认证与租户 =====
// ===== A5: authentication and tenancy =====

// 缺 key → 401 UNAUTHENTICATED；不写任何事件。
// Missing key → 401 UNAUTHENTICATED; no event written.
#[tokio::test]
async fn a5_missing_key_returns_401() {
    let env = env();
    let (status, body) = send_json(
        &env.router,
        post_request(rate_body(env.log_id, "k-1"), None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "UNAUTHENTICATED");
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 0);
}

// 错 key → 401；响应不含正确 secret 原文（D8/A5）。
// Wrong key → 401; the response never carries the real secret (D8/A5).
#[tokio::test]
async fn a5_wrong_key_returns_401_without_leaking_secret() {
    let env = env();
    let (status, _headers, bytes) = send(
        &env.router,
        post_request(rate_body(env.log_id, "k-1"), Some("totally-wrong")),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("UNAUTHENTICATED"));
    assert!(
        !text.contains(SECRET),
        "response must not echo the valid secret"
    );
}

// domain 越权 → 403 TENANT_FORBIDDEN；消息不泄漏 key 的允许 domain（D7）。
// Domain overreach → 403 TENANT_FORBIDDEN; the message never leaks the key's
// allowed domain (D7).
#[tokio::test]
async fn a5_domain_not_allowed_returns_403() {
    let env = env();
    let body = serde_json::to_vec(&json!({
        "domain": "other",
        "events": [{"idempotency_key": "k-other", "log_id": env.other_log_id, "kind": "rate", "rating": 5, "metadata": {}}]
    }))
    .unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&payload), "TENANT_FORBIDDEN");
    let message = payload["error"]["message"].as_str().unwrap_or("");
    assert!(
        !message.contains(DOMAIN),
        "403 must not leak the allowed domain"
    );
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 0);
}

// ===== A6：载荷与事件数预算 =====
// ===== A6: payload and event-count budgets =====

// 恰好 64 KiB 可接受（A6 边界）。
// Exactly 64 KiB is accepted (the A6 boundary).
#[tokio::test]
async fn a6_body_exactly_64kib_is_accepted() {
    let env = env();
    let body = body_of_exact_size(64 * 1024, 16, env.log_id);
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK, "payload: {payload}");
    assert_eq!(payload["accepted"], 16);
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 16);
}

// 超 64 KiB → 413 + payload_too_large 审计行 + 不写事件（A6）。
// Over 64 KiB → 413 + a payload_too_large audit row + no events (A6).
#[tokio::test]
async fn a6_body_over_64kib_rejects_with_audit_row() {
    let env = env();
    let body = body_of_exact_size(64 * 1024 + 1, 16, env.log_id);
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(&payload), "PAYLOAD_TOO_LARGE");
    let counts = env.kernel.row_counts().unwrap();
    assert_eq!(
        counts["feedback_rejections"], 1,
        "audit row must be written"
    );
    assert_eq!(counts["feedback_events"], 0, "no event may be written");
    // 指标可见（A6「计数可观测」）。
    // Observable via metrics (A6 "counts stay observable").
    let (_status, _headers, metrics) = send(&env.router, get_request("/metrics")).await;
    let text = String::from_utf8(metrics).unwrap();
    assert!(text.contains("wiktor_feedback_rejected_total{reason=\"payload_too_large\"} 1"));
}

// 100 事件可接受；101 事件 → 413 event_count_too_large + 审计行。
// 100 events are accepted; 101 → 413 event_count_too_large + an audit row.
#[tokio::test]
async fn a6_event_count_100_ok_and_101_rejected() {
    let env = env();
    let events: Vec<Value> = (0..100)
        .map(|i| {
            json!({"idempotency_key": format!("bulk-{i}"), "log_id": env.log_id,
                   "kind": "rate", "rating": 4, "metadata": {}})
        })
        .collect();
    let body = serde_json::to_vec(&json!({"domain": DOMAIN, "events": events})).unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK, "payload: {payload}");
    assert_eq!(payload["accepted"], 100);

    let events: Vec<Value> = (0..101)
        .map(|i| {
            json!({"idempotency_key": format!("over-{i}"), "log_id": env.log_id,
                   "kind": "rate", "rating": 4, "metadata": {}})
        })
        .collect();
    let body = serde_json::to_vec(&json!({"domain": DOMAIN, "events": events})).unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(&payload), "PAYLOAD_TOO_LARGE");
    let counts = env.kernel.row_counts().unwrap();
    assert_eq!(counts["feedback_rejections"], 1);
    // 101 事件批零行落库，先前 100 事件批不受影响。
    // The 101-event batch persists zero rows; the earlier 100-event batch is
    // untouched.
    assert_eq!(counts["feedback_events"], 100);
}

// metadata 超 4 KiB → 413 field_too_large + 审计行。
// Metadata over 4 KiB → 413 field_too_large + an audit row.
#[tokio::test]
async fn a6_oversized_metadata_is_field_too_large() {
    let env = env();
    let body = serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [{"idempotency_key": "big-meta", "log_id": env.log_id, "kind": "rate",
                    "rating": 5, "metadata": {"pad": "x".repeat(4096)}}]
    }))
    .unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(&payload), "PAYLOAD_TOO_LARGE");
    let counts = env.kernel.row_counts().unwrap();
    assert_eq!(counts["feedback_rejections"], 1);
    assert_eq!(counts["feedback_events"], 0);
}

// ===== A7：整批原子与 422 面 =====
// ===== A7: batch atomicity and the 422 surface =====

// 好事件 + 坏事件混合 → 422 且整批回滚、零行落库（禁止部分成功）。
// Good + bad events mixed → 422 with the whole batch rolled back, zero rows
// (partial success forbidden).
#[tokio::test]
async fn a7_mixed_good_and_bad_rolls_back_everything() {
    let env = env();
    let body = serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [
            {"idempotency_key": "good-1", "log_id": env.log_id, "kind": "click",
             "page_id": PAGE_ID, "metadata": {}},
            {"idempotency_key": "bad-1", "log_id": 999_999, "kind": "rate", "rating": 5, "metadata": {}}
        ]
    }))
    .unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&payload), "INVALID_FEEDBACK");
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 0);
    // 好事件未被半提交：随后可整体重发成功。
    // The good event was not half-committed: resending the batch succeeds.
    let body = serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [
            {"idempotency_key": "good-1", "log_id": env.log_id, "kind": "click",
             "page_id": PAGE_ID, "metadata": {}}
        ]
    }))
    .unwrap();
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK, "payload: {payload}");
    assert_eq!(payload["accepted"], 1);
}

// log 不存在 → 422。
// A missing log → 422.
#[tokio::test]
async fn a7_missing_log_is_422() {
    let env = env();
    let body = rate_body(999_999, "k-missing-log");
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&payload), "INVALID_FEEDBACK");
}

// log/domain 不一致 → 422（本批拍板口径，见 lib.rs 模块文档偏差段）。
// A log/domain mismatch → 422 (this batch's decided reading; see the deviation
// note in the lib.rs module docs).
#[tokio::test]
async fn a7_log_domain_mismatch_is_422() {
    let env = env();
    let body = rate_body(env.other_log_id, "k-mismatch");
    let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&payload), "INVALID_FEEDBACK");
}

// page 不存在 / 非 accepted → 422（click/adopt 校验；rate 的 page 不校验）。
// A missing / non-accepted page → 422 (click/adopt checked; rate pages are
// not).
#[tokio::test]
async fn a7_page_must_be_accepted_for_click_and_adopt() {
    let env = env();
    // candidate 页不是 accepted：click 拒绝。
    // A candidate page is not accepted: the click is rejected.
    let draft = wiktor_core::seed::parse_page(
        "---\npage_id: milk-tea:drink:draft\nentity_id: milk-tea:drink:draft\ntitle: 草稿\nentity_type: drink\n---\n\n草稿页。",
    )
    .unwrap();
    env.kernel
        .seed_pages(&draft, DOMAIN, PublishStatus::Candidate)
        .unwrap();
    for page_id in ["milk-tea:drink:ghost", "milk-tea:drink:draft"] {
        let body = serde_json::to_vec(&json!({
            "domain": DOMAIN,
            "events": [{"idempotency_key": format!("click-{page_id}"), "log_id": env.log_id,
                        "kind": "click", "page_id": page_id, "metadata": {}}]
        }))
        .unwrap();
        let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "page {page_id}");
        assert_eq!(error_code(&payload), "INVALID_FEEDBACK");
    }
    // rate 带 page 不做存在性校验（拍板口径）。
    // A rate with a page skips the existence check (the decided reading).
    let body = serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [{"idempotency_key": "rate-with-page", "log_id": env.log_id, "kind": "rate",
                    "rating": 3, "page_id": "milk-tea:drink:ghost", "metadata": {}}]
    }))
    .unwrap();
    let (status, _payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK);
}

// ===== schema 校验细目（422/413 映射）=====
// ===== Schema-validation details (422/413 mapping) =====

#[tokio::test]
async fn schema_violations_map_to_422() {
    let env = env();
    let cases: Vec<Value> = vec![
        // rating 越界 / 缺失 / click 带 rating / click 缺 page。
        // Rating out of range / missing / click with rating / click without
        // page.
        json!({"idempotency_key": "r-6", "log_id": env.log_id, "kind": "rate", "rating": 6, "metadata": {}}),
        json!({"idempotency_key": "r-none", "log_id": env.log_id, "kind": "rate", "metadata": {}}),
        json!({"idempotency_key": "c-rated", "log_id": env.log_id, "kind": "click", "rating": 5, "page_id": PAGE_ID, "metadata": {}}),
        json!({"idempotency_key": "c-pageless", "log_id": env.log_id, "kind": "click", "metadata": {}}),
        // 未知 kind / 未知字段 / log_id 非正 / 空 key / metadata 非对象 / null metadata。
        // Unknown kind / unknown field / non-positive log_id / empty key /
        // non-object metadata / null metadata.
        json!({"idempotency_key": "k-hit", "log_id": env.log_id, "kind": "hit", "metadata": {}}),
        json!({"idempotency_key": "k-extra", "log_id": env.log_id, "kind": "rate", "rating": 5, "metadata": {}, "extra": 1}),
        json!({"idempotency_key": "k-zero", "log_id": 0, "kind": "rate", "rating": 5, "metadata": {}}),
        json!({"idempotency_key": "", "log_id": env.log_id, "kind": "rate", "rating": 5, "metadata": {}}),
        json!({"idempotency_key": "k-meta-arr", "log_id": env.log_id, "kind": "rate", "rating": 5, "metadata": []}),
        json!({"idempotency_key": "k-meta-null", "log_id": env.log_id, "kind": "rate", "rating": 5, "metadata": null}),
    ];
    for (i, event) in cases.iter().enumerate() {
        let body = serde_json::to_vec(&json!({"domain": DOMAIN, "events": [event]})).unwrap();
        let (status, payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "case {i}: {event}"
        );
        assert_eq!(error_code(&payload), "INVALID_FEEDBACK", "case {i}");
    }
    // 全程零事件落库 + rejected{invalid_feedback} 计数 = cases 数。
    // Zero events persisted throughout + rejected{invalid_feedback} counts the
    // cases.
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 0);
    let (_status, _headers, metrics) = send(&env.router, get_request("/metrics")).await;
    let text = String::from_utf8(metrics).unwrap();
    let expected = format!(
        "wiktor_feedback_rejected_total{{reason=\"invalid_feedback\"}} {}",
        cases.len()
    );
    assert!(text.contains(&expected), "metrics missing {expected}");
}

// JSON 语法/顶层形状错误 → 400（含缺 events、未知顶层字段、events 非数组）。
// JSON syntax / top-level shape errors → 400 (missing events, unknown top-level
// fields, non-array events).
#[tokio::test]
async fn invalid_json_and_top_level_shape_map_to_400() {
    let env = env();
    let bodies: Vec<Vec<u8>> = vec![
        b"not json at all".to_vec(),
        serde_json::to_vec(&json!({"domain": DOMAIN})).unwrap(),
        serde_json::to_vec(&json!({"domain": DOMAIN, "events": [], "surprise": 1})).unwrap(),
        serde_json::to_vec(&json!({"domain": DOMAIN, "events": "nope"})).unwrap(),
        serde_json::to_vec(&json!({"events": []})).unwrap(),
    ];
    for (i, body) in bodies.iter().enumerate() {
        let (status, payload) =
            send_json(&env.router, post_request(body.clone(), Some(SECRET))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case {i}");
        assert_eq!(error_code(&payload), "INVALID_JSON", "case {i}");
    }
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 0);
}

// ===== D5：幂等重放 =====
// ===== D5: idempotent replay =====

#[tokio::test]
async fn idempotent_replay_returns_200_with_original_ids() {
    let env = env();
    let body = serde_json::to_vec(&json!({
        "domain": DOMAIN,
        "events": [
            {"idempotency_key": "checkout-7-1", "log_id": env.log_id, "kind": "click",
             "page_id": PAGE_ID, "metadata": {}},
            {"idempotency_key": "checkout-7-2", "log_id": env.log_id, "kind": "adopt",
             "page_id": PAGE_ID, "metadata": {}},
            {"idempotency_key": "checkout-7-3", "log_id": env.log_id, "kind": "rate",
             "rating": 5, "metadata": {}}
        ]
    }))
    .unwrap();
    let (status, first) = send_json(&env.router, post_request(body.clone(), Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK, "payload: {first}");
    assert_eq!(first["accepted"], 3);
    assert_eq!(first["domain"], DOMAIN);
    for event in first["events"].as_array().unwrap() {
        assert_eq!(event["replayed"], false);
    }
    let first_ids: Vec<i64> = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].as_i64().unwrap())
        .collect();

    // 重发同一批：200 全 replayed=true，event_id/received_at 不变。
    // Resend the same batch: 200 with all replayed=true and unchanged
    // event_id/received_at.
    let (status, second) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK, "payload: {second}");
    for (i, event) in second["events"].as_array().unwrap().iter().enumerate() {
        assert_eq!(event["replayed"], true, "event {i}");
        assert_eq!(event["event_id"], first_ids[i], "event {i}");
        assert_eq!(
            event["received_at"], first["events"][i]["received_at"],
            "event {i}"
        );
    }
    assert_eq!(env.kernel.row_counts().unwrap()["feedback_events"], 3);
    // 计数（A17）：1 批新 3 条 + 1 批回放 3 条。
    // Counters (A17): one batch of 3 new + one batch of 3 replayed.
    let (_status, _headers, metrics) = send(&env.router, get_request("/metrics")).await;
    let text = String::from_utf8(metrics).unwrap();
    assert!(text.contains("wiktor_feedback_ingested_total 3"));
    assert!(text.contains("wiktor_feedback_replayed_total 3"));
}

// ===== A8：固定窗口限流 =====
// ===== A8: the fixed-window limiter =====

#[tokio::test]
async fn a8_rate_limit_429_with_retry_after_and_window_recovery() {
    let env = env();
    let body = rate_body(env.log_id, "hot-key");
    // 窗口内前 120 个已认证请求全部放行（同 key 幂等重放也算已认证请求）。
    // The first 120 authenticated requests in the window pass (same-key
    // replays count as authenticated requests too).
    for i in 0..120 {
        let (status, _payload) =
            send_json(&env.router, post_request(body.clone(), Some(SECRET))).await;
        assert_eq!(status, StatusCode::OK, "request {i} must pass");
    }
    // 第 121 个 → 429 + Retry-After = 窗口剩余秒（now=3600, 窗口 [3600,3660)）。
    // The 121st → 429 + Retry-After = remaining window seconds (now=3600,
    // window [3600,3660)).
    let (status, headers, bytes) =
        send(&env.router, post_request(body.clone(), Some(SECRET))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("retry-after").unwrap(), "60");
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error_code(&payload), "RATE_LIMITED");

    // 窗口内时间推进 → Retry-After 收缩仍 429。
    // Advancing within the window → shrinking Retry-After, still 429.
    env.clock.set(3659);
    let (status, headers, _bytes) =
        send(&env.router, post_request(body.clone(), Some(SECRET))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("retry-after").unwrap(), "1");

    // 窗口翻转后恢复 200。
    // After the rollover the flow recovers with 200.
    env.clock.set(3661);
    let (status, _payload) = send_json(&env.router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK);

    // 限流计数可观测（A17）。
    // The rate-limited counter is observable (A17).
    let (_status, _headers, metrics) = send(&env.router, get_request("/metrics")).await;
    let text = String::from_utf8(metrics).unwrap();
    assert!(text.contains("wiktor_feedback_rate_limited_total 2"));
}

// A8：重启清空——新 state/limiter 对同一 (domain, key) 从零计数。
// A8: restart clears — a fresh state/limiter counts the same (domain, key)
// from zero.
#[tokio::test]
async fn a8_restart_clears_rate_limit() {
    let env = env();
    let body = rate_body(env.log_id, "hot-key");
    for _ in 0..120 {
        let (status, _payload) =
            send_json(&env.router, post_request(body.clone(), Some(SECRET))).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(
        send(&env.router, post_request(body.clone(), Some(SECRET)))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // 「重启」：同 kernel/时钟/key 配置、全新限流器。
    // The "restart": same kernel/clock/key config, a brand-new limiter.
    let restarted = Arc::new(ServerState::with_parts(
        env.kernel.clone(),
        keys(),
        env.clock.clone(),
        Arc::new(KernelHealthCheck {
            kernel: env.kernel.clone(),
        }),
    ));
    let router = build_router(restarted);
    let (status, _payload) = send_json(&router, post_request(body, Some(SECRET))).await;
    assert_eq!(status, StatusCode::OK);
}

// ===== A17：health 与 metrics =====
// ===== A17: health and metrics =====

#[tokio::test]
async fn a17_health_ok_and_503_when_unavailable() {
    let env = env();
    // 无认证可访问（spec §7.2）。
    // Accessible without authentication (spec §7.2).
    let (status, payload) = send_json(&env.router, get_request("/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["schema_version"], SCHEMA_VERSION);

    // 存储不可用 → 503 SERVER_NOT_READY（注入失败探针）。
    // Storage unavailable → 503 SERVER_NOT_READY (an injected failing probe).
    struct FailingHealth;
    impl crate::state::HealthCheck for FailingHealth {
        fn check(&self) -> Result<(), wiktor_core::types::error::Error> {
            Err(wiktor_core::types::error::Error::Internal(
                "probe failure".into(),
            ))
        }
    }
    let broken = Arc::new(ServerState::with_parts(
        env.kernel.clone(),
        keys(),
        env.clock.clone(),
        Arc::new(FailingHealth),
    ));
    let (status, payload) = send_json(&build_router(broken), get_request("/health")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_code(&payload), "SERVER_NOT_READY");
}

#[tokio::test]
async fn a17_metrics_have_fixed_names_and_no_user_labels() {
    let env = env();
    // 制造一点流量（含拒绝），再抓取。
    // Generate some traffic (including rejections), then scrape.
    let (_status, _payload) = send_json(
        &env.router,
        post_request(rate_body(env.log_id, "m-1"), Some(SECRET)),
    )
    .await;
    let (_status, _payload) =
        send_json(&env.router, post_request(b"broken{".to_vec(), Some(SECRET))).await;
    let (status, headers, bytes) = send(&env.router, get_request("/metrics")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = String::from_utf8(bytes).unwrap();
    for name in [
        "wiktor_feedback_ingested_total",
        "wiktor_feedback_replayed_total",
        "wiktor_feedback_rejected_total",
        "wiktor_feedback_rate_limited_total",
        "wiktor_feedback_store_errors_total",
        "wiktor_feedback_review_pending",
    ] {
        assert!(text.contains(name), "missing metric {name}");
    }
    // 用户可控值（domain/页/幂等键/secret）永不作为 label 或内容出现（A17）。
    // User-controlled values (domain/page/idempotency key/secret) never appear
    // as labels or content (A17).
    for forbidden in [DOMAIN, PAGE_ID, SECRET, "m-1"] {
        assert!(!text.contains(forbidden), "metrics leaked {forbidden:?}");
    }
}
