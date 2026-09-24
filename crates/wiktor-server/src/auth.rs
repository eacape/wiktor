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
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::metrics::RejectionReason;
use crate::state::{ApiKeys, ServerState};

/// 已认证 key 的身份快照（从 `KeyIdentity` 克隆出的非敏感字段；含 secret 的
/// 映射不进入请求扩展）。Step7 D3：含方法权限集供 handler 方法级授权。
/// The authenticated key's identity snapshot (non-sensitive fields cloned from
/// `KeyIdentity`; the secret-bearing map never enters request extensions).
/// Step7 D3: carries the method-permission set for handler-level authorization.
#[derive(Debug, Clone)]
pub struct AuthedKey {
    /// key 绑定的允许 domain（D7：租户校验的精确匹配对象）。
    /// The key's bound allowed domain (D7: the exact-match subject of the
    /// tenant check).
    pub domain: String,
    /// BLAKE3 截断标签（D8；限流键 + 指标安全标识）。
    /// The BLAKE3 truncated label (D8; limiter key + metrics-safe identifier).
    pub label: String,
    /// 授权的方法权限名（Step7 §3.1；空集 = 无任何方法权限）。
    /// The authorized method permissions (Step7 §3.1; an empty set authorizes
    /// nothing).
    pub methods: BTreeSet<String>,
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
        methods: identity.methods.clone(),
    });
    next.run(Request::from_parts(parts, body)).await
}

/// 方法级授权判定（Step7 D3：HTTP handler 与 gRPC interceptor 共用同一
/// 语义）。未认证身份 → 无任何方法权限（fail-closed）。
/// Method-authorization check (Step7 D3: the same semantics shared by the HTTP
/// handlers and the gRPC interceptor). An unauthenticated identity authorizes
/// nothing (fail-closed).
pub fn authorize_method(methods: &BTreeSet<String>, method: &str) -> bool {
    methods.contains(method)
}

/// 构造 gRPC interceptor（Step7 D3：与 HTTP middleware 同源同语义）。
/// 每个 service 注册时绑定一个方法权限名（如 `search`），interceptor 做
/// Bearer 认证 + 方法授权；失败返回稳定 gRPC 码（16 未认证 / 7 无权限）。
/// Builds a gRPC interceptor (Step7 D3: the same source and semantics as the
/// HTTP middleware). Each service registers with one method permission (e.g.
/// `search`); the interceptor performs bearer authentication + method
/// authorization, failing with stable gRPC codes (16 unauthenticated / 7
/// permission denied).
pub fn grpc_interceptor(
    keys: ApiKeys,
    method: &'static str,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    // keys 经 Arc 共享，闭包可 Clone（tonic Router 要求 service Clone）。
    // keys are shared via Arc so the closure is Clone (a tonic Router requires
    // Clone services).
    let keys = Arc::new(keys);
    move |mut req: tonic::Request<()>| {
        let secret = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|s| !s.is_empty());
        let Some(secret) = secret else {
            return Err(tonic::Status::unauthenticated(
                "missing or invalid bearer key",
            ));
        };
        let Some(identity) = keys.lookup(secret) else {
            return Err(tonic::Status::unauthenticated(
                "missing or invalid bearer key",
            ));
        };
        if !identity.authorize(method) {
            return Err(tonic::Status::permission_denied(
                "method not allowed for this key",
            ));
        }
        // 注入认证上下文供 handler 取用（不含 secret）。
        // Injects the auth context for handlers (never the secret).
        req.extensions_mut().insert(AuthedKey {
            domain: identity.domain.clone(),
            label: identity.label.clone(),
            methods: identity.methods.clone(),
        });
        Ok(req)
    }
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

    // Step7 A6：gRPC interceptor 的认证 + 方法授权（缺/错 key → 16；有 key
    // 无权限 → 7；通过后注入 AuthedKey）。
    // Step7 A6: the gRPC interceptor's auth + method authorization (missing/
    // wrong key → 16; valid key without the permission → 7; success injects
    // AuthedKey).
    #[test]
    fn grpc_interceptor_authenticates_and_authorizes() {
        let keys = ApiKeys::parse(r#"{"milk-tea":{"secret":"s1","methods":["search"]}}"#).unwrap();
        let mut interceptor = grpc_interceptor(keys, "search");

        // 缺 key → UNAUTHENTICATED (16)。
        // Missing key → UNAUTHENTICATED (16).
        let no_key = tonic::Request::new(());
        let err = interceptor(no_key).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // 错 key → UNAUTHENTICATED (16)。
        // Wrong key → UNAUTHENTICATED (16).
        let mut wrong = tonic::Request::new(());
        wrong
            .metadata_mut()
            .insert("authorization", "Bearer wrong".parse().unwrap());
        let err = interceptor(wrong).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // 有效 key + 方法 → 通过并注入 AuthedKey。
        // Valid key + the method → passes and injects AuthedKey.
        let mut ok = tonic::Request::new(());
        ok.metadata_mut()
            .insert("authorization", "Bearer s1".parse().unwrap());
        let req = interceptor(ok).unwrap();
        let authed = req.extensions().get::<AuthedKey>().unwrap();
        assert_eq!(authed.domain, "milk-tea");
        assert_eq!(authed.methods.len(), 1);
        assert!(authed.methods.contains("search"));
    }

    // Step7 A6：有 key 但方法不在授权集 → PERMISSION_DENIED (7)。
    // Step7 A6: a valid key whose method is outside the authorized set →
    // PERMISSION_DENIED (7).
    #[test]
    fn grpc_interceptor_rejects_unauthorized_method() {
        let keys = ApiKeys::parse(r#"{"milk-tea":{"secret":"s1","methods":["search"]}}"#).unwrap();
        // 用不同方法权限名构造拦截器（模拟注册到 review service 却无 review
        // 权限）。
        // Builds the interceptor with a different method (simulating a review
        // service registration without the review permission).
        let mut interceptor = grpc_interceptor(keys, "review");
        let mut req = tonic::Request::new(());
        req.metadata_mut()
            .insert("authorization", "Bearer s1".parse().unwrap());
        let err = interceptor(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }
}
