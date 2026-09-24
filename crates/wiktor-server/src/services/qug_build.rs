//! QugBuild 服务（spec step7 §3 D1/D6）：同步执行一次 bounded build/publish，
//! 复用 kernel `build_and_publish_qug_from_bytes`（core 单事务保护，force 原样
//! 传递）。
//! The QugBuild service (spec step7 §3 D1/D6): one synchronous bounded
//! build/publish reusing the kernel's `build_and_publish_qug_from_bytes` (core
//! single-transaction protection, `force` passed through).

use std::sync::Arc;

use wiktor_core::kernel::qug_store::build_and_publish_qug_from_bytes;
use wiktor_core::query_engine::qug::qug_build::QugBuildOutcome;
use wiktor_core::traits::DomainConfig;
use wiktor_core::SqliteKernel;

use crate::grpc::v1::qug_build_server::QugBuild;
use crate::grpc::v1::{QugBuildRequest, QugBuildResponse};

/// QUG build gRPC handler：持 kernel，同步执行 build/publish（spawn_blocking
/// 包同步调用，锁不跨 await）。
/// The QUG build gRPC handler: holds the kernel and runs the build/publish
/// synchronously (the synchronous call is wrapped in spawn_blocking so locks
/// never cross an await point).
pub struct QugBuildService {
    kernel: Arc<SqliteKernel>,
}

impl QugBuildService {
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

#[tonic::async_trait]
impl QugBuild for QugBuildService {
    async fn build(
        &self,
        request: tonic::Request<QugBuildRequest>,
    ) -> std::result::Result<tonic::Response<QugBuildResponse>, tonic::Status> {
        let req = request.into_inner();
        let config: DomainConfig = serde_json::from_str(&req.domain_config_json).map_err(|e| {
            tonic::Status::invalid_argument(format!("invalid domain_config_json: {e}"))
        })?;
        let kernel = self.kernel.clone();
        let domain = req.domain.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            build_and_publish_qug_from_bytes(
                &kernel,
                &req.domain,
                &req.domain_version,
                req.qug_config_json,
                &req.intents_yaml,
                &config,
                req.force,
            )
        })
        .await
        .map_err(|e| tonic::Status::internal(format!("qug build task join failed: {e}")))?
        .map_err(|e| crate::error::grpc_status(&e, "qug build failed"))?;
        let (published, stats) = match outcome {
            QugBuildOutcome::Published(s) => (true, s),
            QugBuildOutcome::Reused(s) => (false, s),
        };
        Ok(tonic::Response::new(QugBuildResponse {
            domain,
            source_hash: stats.source_hash,
            edge_count: stats.edge_count as u32,
            published,
            reused: !published,
        }))
    }
}
