//! Compatibility 服务（spec step7 §3 D1/D8；B3 填充只读 preflight）。
//! The Compatibility service (spec step7 §3 D1/D8; B3 fills the read-only
//! preflight).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::compatibility_server::Compatibility;
use crate::grpc::v1::{CompatibilityCheckRequest, CompatibilityCheckResponse};

/// Compatibility gRPC handler（B3 注入 kernel；只读不写库）。
/// The Compatibility gRPC handler (B3 injects the kernel; read-only, never
/// writes the DB).
#[derive(Debug, Default)]
pub struct CompatibilityService {
    // B3: kernel: Arc<SqliteKernel>
}

#[tonic::async_trait]
impl Compatibility for CompatibilityService {
    async fn check(
        &self,
        _request: tonic::Request<CompatibilityCheckRequest>,
    ) -> std::result::Result<tonic::Response<CompatibilityCheckResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "Compatibility.Check not yet wired",
        ))
    }
}
