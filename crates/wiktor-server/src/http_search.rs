//! HTTP `GET /search` 读口（spec step7 §3 D3/D5，§6.1；Step13 D2 升级）：与
//! gRPC `Search` 共享同一 QueryEngine（混合检索：QUG/过滤下推/FTS+向量/RRF），
//! 检索照常落 `query_logs`（反馈闭环的数据前提），响应字段形状不变——
//! diagnostics_json/log_id 从占位变为真实值。认证复用 Step6 middleware，方法
//! 授权用 `search` 权限。
//! The HTTP `GET /search` read surface (spec step7 §3 D3/D5, §6.1; upgraded in
//! Step13 D2): it shares the same QueryEngine as the gRPC `Search` (hybrid
//! retrieval: QUG / filter pushdown / FTS+vector / RRF) and persists
//! `query_logs` as usual — the data prerequisite for the feedback loop. The
//! response field shape is unchanged, with diagnostics_json/log_id going from
//! placeholders to real values. Auth reuses the Step6 middleware with the
//! `search` method permission.

use std::sync::Arc;

use axum::extract::{Query as AxumQuery, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use wiktor_core::types::{Filters, Query, SearchHit};

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

/// GET /search handler：经共享 QueryEngine 的混合检索（Step13 D2），认证经
/// middleware，`search` 方法权限。
/// The GET /search handler: hybrid retrieval through the shared QueryEngine
/// (Step13 D2), authenticated by the middleware, authorized for the `search`
/// method.
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
    let query = Query {
        text: params.q,
        filters,
        top_k: params.top_k,
        domain: Some(params.domain),
    };
    // Step14 P4：按请求领域从多域引擎表分发；未装配该域 → NOT_FOUND（区别于
    // 上方认证 403：key 授权通过但该域未 serve）。
    // Step14 P4: dispatch by the request domain from the multi-domain engine
    // map; an unwired domain → NOT_FOUND (distinct from the auth 403 above:
    // key-authorized but the domain is not served).
    let Some(engine) = state.engine_for(query.domain.as_deref().unwrap_or("")) else {
        return crate::error_json(
            axum::http::StatusCode::NOT_FOUND,
            crate::error::code::NOT_FOUND,
            "domain not served by this instance",
        )
        .into_response();
    };
    let result = match engine.search(&query).await {
        Ok(result) => result,
        Err(e) => {
            let (status, code) = crate::error::http_error(&e);
            return crate::error_json(status, code, "search failed").into_response();
        }
    };
    let resp = SearchResponse {
        hits: result.hits,
        rewritten: result
            .rewritten
            .as_ref()
            .map(|r| serde_json::to_value(r).unwrap_or_default()),
        rewrite_failure: result.rewrite_failure,
        diagnostics_json: serde_json::to_value(&result.diagnostics).unwrap_or_default(),
        latency_ms: result.latency_ms,
        log_id: result.log_id,
    };
    (axum::http::StatusCode::OK, Json(resp)).into_response()
}
