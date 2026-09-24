//! 统一错误映射（Step7 spec §3 D9/§6.4）：core `Error` 与业务错误 → 稳定的
//! gRPC `Status` 码 + HTTP 状态码。HTTP 沿用 Step6 的 `error_json` 形状；
//! gRPC 用稳定 code，客户端按 code 处理，不解析英文错误字符串（§6.4）。
//! Unified error mapping (Step7 spec §3 D9/§6.4): the core `Error` and business
//! errors → a stable gRPC `Status` code + an HTTP status code. HTTP keeps the
//! Step6 `error_json` shape; gRPC uses stable codes, so clients branch on codes
//! rather than parsing English error strings (§6.4).
//!
//! 安全纪律（§6.4）：错误 message 为固定安全文案；数据库、SQL、secret、原始
//! metadata、原始 source 不进响应或日志（D8）。
//! Security discipline (§6.4): error messages are fixed safe copy; database
//! details, SQL, secrets, raw metadata and raw source never enter responses or
//! logs (D8).

use wiktor_core::types::error::Error;

/// 稳定错误码（§6.4 表）。
/// Stable error codes (the §6.4 table).
pub mod code {
    pub const UNAUTHENTICATED: &str = "UNAUTHENTICATED";
    pub const PERMISSION_DENIED: &str = "PERMISSION_DENIED";
    pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const FAILED_PRECONDITION: &str = "FAILED_PRECONDITION";
    pub const RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
    pub const INTERNAL: &str = "INTERNAL";
    pub const UNAVAILABLE: &str = "UNAVAILABLE";
}

/// core `Error` → gRPC `Status` 映射（§6.4 表；message 固定安全文案，不携带
/// 底层错误详情）。
/// Maps a core `Error` to a gRPC `Status` (the §6.4 table; the message is fixed
/// safe copy and never carries underlying error details).
pub fn grpc_status(err: &Error, fallback_desc: &str) -> tonic::Status {
    match err {
        Error::Validation(_)
        | Error::InvalidConfig(_)
        | Error::Serialization(_)
        | Error::SerializationYaml(_) => tonic::Status::invalid_argument(fallback_desc.to_string()),
        Error::DomainPackNotFound(_) | Error::EntityNotFound(_) => {
            tonic::Status::not_found(fallback_desc.to_string())
        }
        Error::DuplicateEntity(_)
        | Error::ContentHashMismatch { .. }
        | Error::QugRewrite(_)
        | Error::Filter(_) => tonic::Status::failed_precondition(fallback_desc.to_string()),
        Error::CompileFailure(_) | Error::Compilation(_) | Error::QualityBelowThreshold { .. } => {
            tonic::Status::failed_precondition(fallback_desc.to_string())
        }
        Error::VectorStore(_) | Error::QdrantConnection(_) => {
            tonic::Status::unavailable(fallback_desc.to_string())
        }
        Error::Database(_)
        | Error::Migration(_)
        | Error::Io(_)
        | Error::InvalidEntityId(_)
        | Error::Query(_)
        | Error::Internal(_) => tonic::Status::internal(fallback_desc.to_string()),
    }
}

/// core `Error` → HTTP 状态码 + 稳定 code（§6.4 表；HTTP handler 组合
/// `error_json` 响应时使用）。
/// Maps a core `Error` to an HTTP status + stable code (the §6.4 table; used by
/// HTTP handlers when composing an `error_json` response).
pub fn http_error(err: &Error) -> (axum::http::StatusCode, &'static str) {
    match err {
        Error::Validation(_)
        | Error::InvalidConfig(_)
        | Error::Serialization(_)
        | Error::SerializationYaml(_) => {
            (axum::http::StatusCode::BAD_REQUEST, code::INVALID_ARGUMENT)
        }
        Error::DomainPackNotFound(_) | Error::EntityNotFound(_) => {
            (axum::http::StatusCode::NOT_FOUND, code::NOT_FOUND)
        }
        Error::DuplicateEntity(_)
        | Error::ContentHashMismatch { .. }
        | Error::QugRewrite(_)
        | Error::Filter(_)
        | Error::CompileFailure(_)
        | Error::Compilation(_)
        | Error::QualityBelowThreshold { .. } => (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            code::FAILED_PRECONDITION,
        ),
        Error::VectorStore(_) | Error::QdrantConnection(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            code::UNAVAILABLE,
        ),
        Error::Database(_)
        | Error::Migration(_)
        | Error::Io(_)
        | Error::InvalidEntityId(_)
        | Error::Query(_)
        | Error::Internal(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            code::INTERNAL,
        ),
    }
}
