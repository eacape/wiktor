//! Review 服务（spec step7 §3 D1/D7；B5 填充 list/approve/ignore）。
//! The Review service (spec step7 §3 D1/D7; B5 fills list/approve/ignore).
//!
//! B1 空壳：trait 实现返回 UNIMPLEMENTED，保证 proto 生成代码可编译注册。
//! B1 shell: the trait impl returns UNIMPLEMENTED, keeping the generated code
//! compilable and registered.

use crate::grpc::v1::review_server::Review;
use crate::grpc::v1::{
    ReviewDecisionRequest, ReviewDecisionResponse, ReviewListRequest, ReviewListResponse,
};

/// Review gRPC handler（B5 注入 kernel；reviewer 取认证 label，拒绝客户端传）。
/// The Review gRPC handler (B5 injects the kernel; the reviewer comes from the
/// auth label, never from the client).
#[derive(Debug, Default)]
pub struct ReviewService {
    // B5: kernel: Arc<SqliteKernel>
}

#[tonic::async_trait]
impl Review for ReviewService {
    async fn list(
        &self,
        _request: tonic::Request<ReviewListRequest>,
    ) -> std::result::Result<tonic::Response<ReviewListResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Review.List not yet wired"))
    }

    async fn approve(
        &self,
        _request: tonic::Request<ReviewDecisionRequest>,
    ) -> std::result::Result<tonic::Response<ReviewDecisionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Review.Approve not yet wired"))
    }

    async fn ignore(
        &self,
        _request: tonic::Request<ReviewDecisionRequest>,
    ) -> std::result::Result<tonic::Response<ReviewDecisionResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("Review.Ignore not yet wired"))
    }
}
