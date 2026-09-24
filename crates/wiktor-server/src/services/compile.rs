//! Compile 服务（spec step7 §3 D1/D4/D5；B4 填充 worker 与状态查询）。
//! The Compile service (spec step7 §3 D1/D4/D5; B4 fills the worker and the
//! status query).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::compile_server::Compile;
use crate::grpc::v1::{
    CompileAdmitRequest, CompileAdmitResponse, CompileStatusRequest, CompileStatusResponse,
};

/// Compile gRPC handler（B4 注入 kernel + worker handle）。
/// The Compile gRPC handler (B4 injects the kernel + worker handle).
#[derive(Debug, Default)]
pub struct CompileService {
    // B4: kernel: Arc<SqliteKernel>, worker: CompileWorkerHandle
}

#[tonic::async_trait]
impl Compile for CompileService {
    async fn admit(
        &self,
        _request: tonic::Request<CompileAdmitRequest>,
    ) -> std::result::Result<tonic::Response<CompileAdmitResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Compile.Admit not yet wired"))
    }

    async fn status(
        &self,
        _request: tonic::Request<CompileStatusRequest>,
    ) -> std::result::Result<tonic::Response<CompileStatusResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Compile.Status not yet wired"))
    }
}
