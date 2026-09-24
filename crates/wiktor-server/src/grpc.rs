//! Step 7 gRPC 服务面（spec step7 §3 D1/D12）：proto 生成代码引入 + 六 service
//! 注册装配。认证经 tonic interceptor（B2 接入 [`crate::auth`]）；metrics 计数
//! gRPC 请求（D12）。
//! Step 7 gRPC service surface (spec step7 §3 D1/D12): the proto-generated code
//! plus six-service registration assembly. Authentication comes via a tonic
//! interceptor (wired in B2 through [`crate::auth`]); metrics count gRPC
//! requests (D12).

/// proto 生成代码（`wiktor.v1` 包 → `wiktor::v1` 模块树）。
/// The proto-generated code (package `wiktor.v1` → the `wiktor::v1` module tree).
pub mod v1 {
    tonic::include_proto!("wiktor.v1");
}

/// 把六个 service 注册到 tonic Router，每个 service 绑定其方法权限名的
/// interceptor（Step7 §3.1 方法权限映射 + D3 同源语义）。
/// Registers the six services on a tonic Router, each bound to its method
/// permission's interceptor (Step7 §3.1 method-permission mapping + D3 shared
/// semantics).
pub fn router(
    keys: crate::state::ApiKeys,
    search: crate::services::search::SearchService,
    compile: crate::services::compile::CompileService,
    qug: crate::services::qug_build::QugBuildService,
    review: crate::services::review::ReviewService,
    compatibility: crate::services::compatibility::CompatibilityService,
    status: crate::services::status::StatusService,
) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .add_service(v1::search_server::SearchServer::with_interceptor(
            search,
            crate::auth::grpc_interceptor(keys.clone(), "search"),
        ))
        .add_service(v1::compile_server::CompileServer::with_interceptor(
            compile,
            crate::auth::grpc_interceptor(keys.clone(), "compile"),
        ))
        .add_service(v1::qug_build_server::QugBuildServer::with_interceptor(
            qug,
            crate::auth::grpc_interceptor(keys.clone(), "qug_build"),
        ))
        .add_service(v1::review_server::ReviewServer::with_interceptor(
            review,
            crate::auth::grpc_interceptor(keys.clone(), "review"),
        ))
        .add_service(
            v1::compatibility_server::CompatibilityServer::with_interceptor(
                compatibility,
                crate::auth::grpc_interceptor(keys.clone(), "compatibility"),
            ),
        )
        .add_service(v1::status_server::StatusServer::with_interceptor(
            status,
            crate::auth::grpc_interceptor(keys, "status"),
        ))
}

#[cfg(test)]
mod tests {
    use super::v1;

    // A1：proto 生成代码可用，六 service server 类型存在（字段号稳定由 proto
    // 文件本身保证，任何字段号复用都会在 tonic-prost-build 报错）。
    // A1: the proto-generated code is usable and all six service server types
    // exist (field-number stability is guaranteed by the proto file itself —
    // any field-number reuse fails at tonic-prost-build time).
    #[test]
    fn generated_six_services_compile() {
        let _ = v1::SearchRequest {
            domain: "milk-tea".into(),
            text: "珍珠奶茶".into(),
            filters_json: String::new(),
            top_k: 5,
        };
        let _ =
            v1::search_server::SearchServer::new(crate::services::search::SearchService::default());
        let _ = v1::compile_server::CompileServer::new(
            crate::services::compile::CompileService::default(),
        );
        let _ = v1::qug_build_server::QugBuildServer::new(
            crate::services::qug_build::QugBuildService::default(),
        );
        let _ =
            v1::review_server::ReviewServer::new(crate::services::review::ReviewService::default());
        let _ = v1::compatibility_server::CompatibilityServer::new(
            crate::services::compatibility::CompatibilityService::default(),
        );
        let _ =
            v1::status_server::StatusServer::new(crate::services::status::StatusService::default());
    }

    // A2：空壳 service 的未知实现返回 UNIMPLEMENTED（B1 占位语义）。
    // A2: the shell service returns UNIMPLEMENTED (the B1 placeholder
    // semantics).
    #[tokio::test]
    async fn shell_search_returns_unimplemented() {
        use crate::grpc::v1::search_server::Search;
        let svc = crate::services::search::SearchService::default();
        let err = svc
            .search(tonic::Request::new(v1::SearchRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }
}
