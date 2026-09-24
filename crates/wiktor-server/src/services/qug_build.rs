//! QugBuild 服务（spec step7 §3 D1/D6；B3 填充 kernel 调用的同步 build）。
//! The QugBuild service (spec step7 §3 D1/D6; B3 fills the synchronous kernel
//! build call).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::qug_build_server::QugBuild;
use crate::grpc::v1::{QugBuildRequest, QugBuildResponse};

/// QUG build gRPC handler（B3 注入 kernel）。
/// The QUG build gRPC handler (B3 injects the kernel).
#[derive(Debug, Default)]
pub struct QugBuildService {
    // B3: kernel: Arc<SqliteKernel>
}

#[tonic::async_trait]
impl QugBuild for QugBuildService {
    async fn build(
        &self,
        _request: tonic::Request<QugBuildRequest>,
    ) -> std::result::Result<tonic::Response<QugBuildResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("QugBuild.Build not yet wired"))
    }
}
