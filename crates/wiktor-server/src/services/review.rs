//! Review 服务（spec step7 §3 D1/D7；B5 实装）。List/Approve/Ignore 全部复用
//! kernel 的 review_queue 事务；reviewer 只取认证上下文的 BLAKE3 label，客户端
//! 不能传 reviewer（D8）。
//! The Review service (spec step7 §3 D1/D7; B5 implementation). List/Approve/
//! Ignore all reuse the kernel's review_queue transactions; the reviewer is only
//! the BLAKE3 label from the auth context, never a client-supplied reviewer (D8).

use std::sync::Arc;

use tonic::{Response, Status};
use wiktor_core::compile::config::{Clock, SystemClock};
use wiktor_core::kernel::{ReviewItem, ReviewStatus, SqliteKernel};

use crate::auth::AuthedKey;
use crate::grpc::v1::review_server::Review;
use crate::grpc::v1::{
    ReviewDecisionRequest, ReviewDecisionResponse, ReviewListRequest, ReviewListResponse,
};

/// Review gRPC handler（B5：kernel；reviewer 取认证 label）。
/// The Review gRPC handler (B5: kernel; the reviewer comes from the auth label).
#[derive(Clone)]
pub struct ReviewService {
    kernel: Arc<SqliteKernel>,
}

impl ReviewService {
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

/// B1 空壳 → B5 实装的过渡默认（测试注册用）。
/// The B1-shell → B5-implementation transition default (used for test
/// registration).
impl Default for ReviewService {
    fn default() -> Self {
        Self {
            kernel: Arc::new(SqliteKernel::open_in_memory().expect("in-memory kernel")),
        }
    }
}

#[tonic::async_trait]
impl Review for ReviewService {
    async fn list(
        &self,
        request: tonic::Request<ReviewListRequest>,
    ) -> std::result::Result<Response<ReviewListResponse>, Status> {
        let req = request.into_inner();
        if req.domain.is_empty() {
            return Err(Status::invalid_argument("domain is required"));
        }
        let status = if req.status.is_empty() {
            None
        } else {
            Some(parse_review_status(&req.status)?)
        };
        let limit = if req.limit == 0 { 50 } else { req.limit };
        let kernel = self.kernel.clone();
        let domain = req.domain;
        let items =
            tokio::task::spawn_blocking(move || kernel.list_reviews(&domain, status, limit))
                .await
                .map_err(|e| Status::internal(format!("review list task join failed: {e}")))?
                .map_err(|e| crate::error::grpc_status(&e, "review list failed"))?;
        Ok(Response::new(ReviewListResponse {
            items: items.into_iter().map(to_proto).collect(),
        }))
    }

    async fn approve(
        &self,
        request: tonic::Request<ReviewDecisionRequest>,
    ) -> std::result::Result<Response<ReviewDecisionResponse>, Status> {
        self.decide(request, true).await
    }

    async fn ignore(
        &self,
        request: tonic::Request<ReviewDecisionRequest>,
    ) -> std::result::Result<Response<ReviewDecisionResponse>, Status> {
        self.decide(request, false).await
    }
}

impl ReviewService {
    async fn decide(
        &self,
        request: tonic::Request<ReviewDecisionRequest>,
        approve: bool,
    ) -> std::result::Result<Response<ReviewDecisionResponse>, Status> {
        let reviewer = request
            .extensions()
            .get::<AuthedKey>()
            .map(|k| k.label.clone())
            .ok_or_else(|| Status::unauthenticated("missing auth context"))?;
        let req = request.into_inner();
        let kernel = self.kernel.clone();
        let review_id = req.review_id;
        let domain = req.domain;
        let lookup_domain = domain.clone();
        let now = Clock::unix_seconds(&SystemClock);
        let item = tokio::task::spawn_blocking(move || {
            if approve {
                kernel.approve_review(review_id, &reviewer, now)?;
            } else {
                kernel.ignore_review(review_id, &reviewer, now)?;
            }
            load_one(&kernel, &lookup_domain, review_id)
        })
        .await
        .map_err(|e| Status::internal(format!("review decision task join failed: {e}")))?
        .map_err(|e| crate::error::grpc_status(&e, "review decision failed"))?;
        Ok(Response::new(ReviewDecisionResponse { item: Some(item) }))
    }
}

fn load_one(
    kernel: &SqliteKernel,
    domain: &str,
    review_id: i64,
) -> Result<crate::grpc::v1::ReviewItem, wiktor_core::types::error::Error> {
    let items = kernel.list_reviews(domain, None, 1000)?;
    items
        .into_iter()
        .find(|item| item.review_id == review_id)
        .map(to_proto)
        .ok_or_else(|| {
            wiktor_core::types::error::Error::Validation(format!(
                "review {review_id} not found in domain {domain:?}"
            ))
        })
}

fn parse_review_status(raw: &str) -> Result<ReviewStatus, Status> {
    match raw {
        "pending" => Ok(ReviewStatus::Pending),
        "approved" => Ok(ReviewStatus::Approved),
        "ignored" => Ok(ReviewStatus::Ignored),
        other => Err(Status::invalid_argument(format!(
            "unknown review status {other:?}"
        ))),
    }
}

fn to_proto(item: ReviewItem) -> crate::grpc::v1::ReviewItem {
    crate::grpc::v1::ReviewItem {
        review_id: item.review_id,
        domain: item.domain,
        action: item.action,
        status: item.status.as_str().to_string(),
        subject_json: item.subject_json,
        reason_json: item.reason_json,
        created_at: item.created_at,
        reviewed_at: item.reviewed_at.unwrap_or(0),
        reviewed_by: item.reviewed_by.unwrap_or_default(),
        compile_task_id: item.compile_task_id.unwrap_or(0),
    }
}
