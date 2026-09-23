//! Bearer 认证（spec `step6-feedback-loop.md` §3 D6/D7、§7.1、§10 A5）：
//! `Authorization: Bearer <secret>` 精确匹配；缺失/错误 → 401 UNAUTHENTICATED。
//! 租户校验（key 的允许 domain 与 body.domain 精确匹配 → 否则 403）需要已解析
//! 的请求体，落在 handler 的流水线步骤 3（见 `crate::lib::post_feedback`）。
//! Bearer authentication (spec `step6-feedback-loop.md` §3 D6/D7, §7.1, §10 A5):
//! exact `Authorization: Bearer <secret>` matching; missing/wrong → 401
//! UNAUTHENTICATED. The tenant check (the key's allowed domain exactly matches
//! body.domain, else 403) needs the parsed body, so it lives at pipeline step 3
//! of the handler (see `crate::lib::post_feedback`).
//!
//! key 原文永不进入日志/指标/错误消息（D8）：401 的响应消息为固定文本，
//! 失败路径只递增 `rejected{reason="unauthenticated"}` 计数。
//! Raw keys never enter logs/metrics/error messages (D8): the 401 response
//! message is fixed text and the failure path only increments the
//! `rejected{reason="unauthenticated"}` counter.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::metrics::RejectionReason;
use crate::state::ServerState;

/// 已认证 key 的身份快照（从 `KeyIdentity` 克隆出的非敏感字段；含 secret 的
/// 映射不进入请求扩展）。
/// The authenticated key's identity snapshot (non-sensitive fields cloned from
/// `KeyIdentity`; the secret-bearing map never enters request extensions).
#[derive(Debug, Clone)]
pub struct AuthedKey {
    /// key 绑定的允许 domain（D7：租户校验的精确匹配对象）。
    /// The key's bound allowed domain (D7: the exact-match subject of the
    /// tenant check).
    pub domain: String,
    /// BLAKE3 截断标签（D8；限流键 + 指标安全标识）。
    /// The BLAKE3 truncated label (D8; limiter key + metrics-safe identifier).
    pub label: String,
}

/// 提取 `Bearer <secret>`（大小写按 RFC 精确 `Bearer ` 前缀；缺失/错误方案/
/// 空 secret → None，统一 401，不泄漏细节）。
/// Extracts `Bearer <secret>` (a literal `Bearer ` prefix per RFC; missing /
/// wrong scheme / empty secret → None, uniformly 401, no detail leaked).
fn bearer_secret(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let secret = value.strip_prefix("Bearer ")?;
    if secret.is_empty() {
        None
    } else {
        Some(secret)
    }
}

/// 认证 middleware（流水线步骤 2；步骤 1 body 限额在 `crate::lib` 的外层
/// middleware）。通过后把 [`AuthedKey`] 注入请求扩展供下游使用。
/// The auth middleware (pipeline step 2; step 1, the body limit, is the outer
/// middleware in `crate::lib`). On success it injects [`AuthedKey`] into the
/// request extensions for downstream consumers.
pub async fn auth_middleware(
    State(state): State<Arc<ServerState>>,
    req: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();
    let identity = bearer_secret(&parts.headers).and_then(|secret| state.keys.lookup(secret));
    let Some(identity) = identity else {
        state
            .metrics
            .record_rejected(RejectionReason::Unauthenticated);
        return crate::error_json(
            axum::http::StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "missing or invalid bearer key",
        )
        .into_response();
    };
    parts.extensions.insert(AuthedKey {
        domain: identity.domain.clone(),
        label: identity.label.clone(),
    });
    next.run(Request::from_parts(parts, body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // A5：Bearer 提取的精确语义（缺失/非 Bearer/空 secret → None）。
    // A5: exact bearer-extraction semantics (missing / non-Bearer / empty
    // secret → None).
    #[test]
    fn extracts_bearer_exactly() {
        let header = |v: &str| HeaderValue::from_str(v).unwrap();
        let mut headers = HeaderMap::new();
        assert!(bearer_secret(&headers).is_none());
        headers.insert(AUTHORIZATION, header("Basic abc"));
        assert!(bearer_secret(&headers).is_none());
        headers.insert(AUTHORIZATION, header("Bearer "));
        assert!(bearer_secret(&headers).is_none());
        headers.insert(AUTHORIZATION, header("bearer tok"));
        assert!(
            bearer_secret(&headers).is_none(),
            "scheme is case-sensitive"
        );
        headers.insert(AUTHORIZATION, header("Bearer tok-1"));
        assert_eq!(bearer_secret(&headers), Some("tok-1"));
    }
}
