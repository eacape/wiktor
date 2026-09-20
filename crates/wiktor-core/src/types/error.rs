use thiserror::Error;

/// Wiktor 统一错误类型。
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid entity id: {0}")]
    InvalidEntityId(String),
    #[error("entity not found: {0}")]
    EntityNotFound(String),
    #[error("duplicate entity: {0}")]
    DuplicateEntity(String),

    #[error("database error: {0}")]
    Database(#[from] diesel::result::Error),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("yaml serialization error: {0}")]
    SerializationYaml(#[from] serde_yaml_ng::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("vector store error: {0}")]
    VectorStore(String),
    #[error("qdrant connection error: {0}")]
    QdrantConnection(String),

    #[error("compilation error: {0}")]
    Compilation(String),
    #[error("quality below threshold: {actual} < {threshold}")]
    QualityBelowThreshold { actual: f32, threshold: f32 },
    #[error("content hash mismatch: expected {expected}, got {actual}")]
    ContentHashMismatch { expected: String, actual: String },

    #[error("query error: {0}")]
    Query(String),
    #[error("qug rewrite error: {0}")]
    QugRewrite(String),
    #[error("filter error: {0}")]
    Filter(String),

    #[error("invalid config: {0}")]
    InvalidConfig(String),
    #[error("domain pack not found: {0}")]
    DomainPackNotFound(String),

    #[error("validation error: {0}")]
    Validation(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
