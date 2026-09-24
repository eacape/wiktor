//! Compatibility 服务（spec step7 §3 D1/D8）：只读 preflight，不创建任务、不写
//! review；返回逐项 report（A21）。从 `DomainConfig` 构造 `DomainIdentity` 与
//! `CompatibilitySpec`，调用 core 只读 `check_domain_compatibility`。
//! The Compatibility service (spec step7 §3 D1/D8): a read-only preflight that
//! never creates tasks or writes reviews; it returns an itemized report (A21).
//! It builds the `DomainIdentity` and `CompatibilitySpec` from a `DomainConfig`
//! and calls the core read-only `check_domain_compatibility`.

use std::sync::Arc;

use wiktor_core::compile::compatibility::{
    check_domain_compatibility, StandardCompatibilityChecker,
};
use wiktor_core::traits::DomainConfig;
use wiktor_core::SqliteKernel;

use crate::grpc::v1::compatibility_server::Compatibility;
use crate::grpc::v1::{
    CompatibilityCheckRequest, CompatibilityCheckResponse, CompatibilityFinding,
    CompatibilityReport,
};

/// Compatibility gRPC handler：持 kernel，只读执行 preflight。
/// The Compatibility gRPC handler: holds the kernel and runs the read-only
/// preflight.
pub struct CompatibilityService {
    kernel: Arc<SqliteKernel>,
}

impl CompatibilityService {
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

#[tonic::async_trait]
impl Compatibility for CompatibilityService {
    async fn check(
        &self,
        request: tonic::Request<CompatibilityCheckRequest>,
    ) -> std::result::Result<tonic::Response<CompatibilityCheckResponse>, tonic::Status> {
        let req = request.into_inner();
        let config: DomainConfig = serde_json::from_str(&req.domain_config_json).map_err(|e| {
            tonic::Status::invalid_argument(format!("invalid domain_config_json: {e}"))
        })?;
        let identity = config
            .identity()
            .map_err(|e| crate::error::grpc_status(&e, "domain identity invalid"))?;
        let spec = config.compile_policy.compatibility.clone();
        let kernel = self.kernel.clone();
        let checker = StandardCompatibilityChecker;
        let identity_for_task = identity.clone();
        let report = tokio::task::spawn_blocking(move || {
            check_domain_compatibility(&kernel, &checker, &identity_for_task, spec.as_ref())
        })
        .await
        .map_err(|e| tonic::Status::internal(format!("compat task join failed: {e}")))?
        .map_err(|e| crate::error::grpc_status(&e, "compatibility check failed"))?;
        let findings = report
            .violations
            .iter()
            .map(|v| CompatibilityFinding {
                code: v.code.clone(),
                component: v.field.clone(),
                expected: v.expected.clone(),
                actual: v.observed.clone(),
                message: format!("{}: {}", v.subject, v.field),
            })
            .collect();
        let current_versions_json = serde_json::to_string(&identity).map_err(|e| {
            crate::error::grpc_status(
                &wiktor_core::types::error::Error::Serialization(e),
                "identity serialization failed",
            )
        })?;
        Ok(tonic::Response::new(CompatibilityCheckResponse {
            compatible: report.compatible,
            report: Some(CompatibilityReport {
                findings,
                current_versions_json,
            }),
        }))
    }
}
