//! HTTP `GET /search` 读口（spec step7 §3 D3/D5，§6.1）：只读 FTS 检索，
//! 与 gRPC `Search` 对齐字段形状（hits/rewrite_failure/diagnostics/latency/
//! log_id 的 HTTP JSON 形态）。走 kernel 同步 `search`（spawn_blocking 包裹，
//! 锁不跨 await）；认证复用 Step6 middleware，方法授权用 `search` 权限。
//! The HTTP `GET /search` read surface (spec step7 §3 D3/D5, §6.1): a read-only
//! FTS search whose fields align with the gRPC `Search` (the HTTP JSON shape of
//! hits/rewrite_failure/diagnostics/latency/log_id). It uses the kernel's
//! synchronous `search` (wrapped in spawn_blocking so locks never cross an
//! await point); auth reuses the Step6 middleware with the `search` method
//! permission.

use std::sync::Arc;

use axum::extract::{Query as AxumQuery, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use wiktor_core::types::{Filters, SearchHit};

/// GET /search 查询参数。
/// The GET /search query parameters.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    /// 检索文本（必填）。The search text (required).
    pub q: String,
    /// domain（必填，与 key 授权域匹配）。The domain (required; matches the
    /// key's authorized domain).
    pub domain: String,
    /// 返回条数（默认 5，1..=100）。Max hits (default 5, 1..=100).
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// 过滤条件 JSON（可选）。Filter conditions JSON (optional).
    #[serde(default)]
    pub filters: Option<String>,
}

fn default_top_k() -> usize {
    5
}

/// GET /search 响应（与 gRPC `SearchResponse` 字段一一对应）。
/// The GET /search response (field-for-field corresponding to the gRPC
/// `SearchResponse`).
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub rewritten: Option<serde_json::Value>,
    pub rewrite_failure: bool,
    pub diagnostics_json: serde_json::Value,
    pub latency_ms: u64,
    pub log_id: Option<i64>,
}

/// GET /search handler：只读 FTS，认证经 middleware，`search` 方法权限。
/// The GET /search handler: read-only FTS, authenticated by the middleware,
/// authorized for the `search` method.
pub async fn search(
    State(state): State<Arc<crate::state::ServerState>>,
    axum::extract::Extension(authed): axum::extract::Extension<crate::auth::AuthedKey>,
    AxumQuery(params): AxumQuery<SearchParams>,
) -> Response {
    // 方法授权：key 需要 `search` 权限（403）。
    // Method authorization: the key needs the `search` permission (403).
    if !crate::auth::authorize_method(&authed.methods, "search") {
        return crate::error_json(
            axum::http::StatusCode::FORBIDDEN,
            crate::error::code::PERMISSION_DENIED,
            "method not allowed for this key",
        )
        .into_response();
    }
    // 租户校验：请求 domain 必须等于 key 的授权域（403）。
    // Tenant check: the request domain must equal the key's authorized domain
    // (403).
    if params.domain != authed.domain {
        return crate::error_json(
            axum::http::StatusCode::FORBIDDEN,
            crate::error::code::PERMISSION_DENIED,
            "domain not allowed for this key",
        )
        .into_response();
    }
    if !(1..=100).contains(&params.top_k) {
        return crate::error_json(
            axum::http::StatusCode::BAD_REQUEST,
            crate::error::code::INVALID_ARGUMENT,
            "top_k out of range 1..=100",
        )
        .into_response();
    }
    let filters: Filters = match &params.filters {
        Some(raw) if !raw.trim().is_empty() => match serde_json::from_str(raw) {
            Ok(f) => f,
            Err(e) => {
                return crate::error_json(
                    axum::http::StatusCode::BAD_REQUEST,
                    crate::error::code::INVALID_ARGUMENT,
                    format!("invalid filters JSON: {e}"),
                )
                .into_response();
            }
        },
        _ => Filters::empty(),
    };
    let kernel = state.kernel.clone();
    let text = params.q.clone();
    let domain = params.domain.clone();
    let started = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        kernel.search(&text, &filters, params.top_k, Some(&domain))
    })
    .await;
    let hits = match outcome {
        Ok(Ok(hits)) => hits,
        Ok(Err(e)) => {
            let (status, code) = crate::error::http_error(&e);
            return crate::error_json(status, code, "search failed").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "search task join failed");
            return crate::error_json(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                crate::error::code::INTERNAL,
                "search task failed",
            )
            .into_response();
        }
    };
    let resp = SearchResponse {
        hits,
        rewritten: None,
        rewrite_failure: false,
        diagnostics_json: serde_json::json!({}),
        latency_ms: started.elapsed().as_millis() as u64,
        log_id: None,
    };
    (axum::http::StatusCode::OK, Json(resp)).into_response()
}
