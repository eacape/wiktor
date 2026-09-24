//! Status 服务（spec step7 §3 D1；B3 填充 schema 版本 + 行数摘要）。
//! The Status service (spec step7 §3 D1; B3 fills schema version + row counts).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::status_server::Status;
use crate::grpc::v1::{StatusRequest, StatusResponse};

/// Status gRPC handler（B3 注入 kernel；对齐 CLI status）。
/// The Status gRPC handler (B3 injects the kernel; mirrors the CLI status).
#[derive(Debug, Default)]
pub struct StatusService {
    // B3: kernel: Arc<SqliteKernel>
}

#[tonic::async_trait]
impl Status for StatusService {
    async fn get(
        &self,
        _request: tonic::Request<StatusRequest>,
    ) -> std::result::Result<tonic::Response<StatusResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Status.Get not yet wired"))
    }
}
