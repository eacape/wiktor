//! Status 服务（spec step7 §3 D1）：schema 版本 + 行数摘要（对齐 CLI status）。
//! The Status service (spec step7 §3 D1): the schema version + row-count
//! summary (mirrors the CLI status).

use std::sync::Arc;

use wiktor_core::SqliteKernel;

use crate::grpc::v1::status_server::Status;
use crate::grpc::v1::{StatusRequest, StatusResponse};

/// Status gRPC handler：持 kernel，返回 schema 版本 + 行数摘要。
/// The Status gRPC handler: holds the kernel and returns the schema version
/// plus a row-count summary.
pub struct StatusService {
    kernel: Arc<SqliteKernel>,
}

impl StatusService {
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

#[tonic::async_trait]
impl Status for StatusService {
    async fn get(
        &self,
        _request: tonic::Request<StatusRequest>,
    ) -> std::result::Result<tonic::Response<StatusResponse>, tonic::Status> {
        // 同步 kernel 调用经 spawn_blocking：锁不跨 await（§6.4/A23）。
        // The synchronous kernel call goes through spawn_blocking: locks never
        // cross an await point (§6.4/A23).
        let kernel = self.kernel.clone();
        let (schema, rows) = tokio::task::spawn_blocking(move || {
            let version = kernel.schema_version()?;
            let counts: std::collections::HashMap<String, u64> = kernel
                .row_counts()?
                .into_iter()
                .map(|(k, v)| (k, v as u64))
                .collect();
            Ok::<_, wiktor_core::types::error::Error>((version, counts))
        })
        .await
        .map_err(|e| tonic::Status::internal(format!("status task join failed: {e}")))?
        .map_err(|e| crate::error::grpc_status(&e, "status failed"))?;
        Ok(tonic::Response::new(StatusResponse {
            schema_version: schema.to_string(),
            healthy: true,
            row_counts: rows,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }
}
