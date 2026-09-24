//! Wiktor 可插拔向量后端：Qdrant 外部服务。
//! Wiktor pluggable vector backend: the Qdrant external service.
//!
//! 这是 MASTER-PLAN §5.6 / 决策 #8 的插件点 3（向量后端）的一个实现，拆出为
//! 独立 crate 验证"插件只依赖 core、不依赖 server/feedback"（STEP10 D4/D5，
//! B3）。`MockVectorStore`（评测基线）留在 wiktor-core。
//! This is one implementation of MASTER-PLAN §5.6 / decision #8 plugin point 3
//! (vector backend), split into its own crate to prove "plugins depend only on
//! core, not on server/feedback" (STEP10 D4/D5, B3). `MockVectorStore` (the eval
//! baseline) stays in wiktor-core.

mod qdrant_vector;

pub use qdrant_vector::QdrantVectorStore;

/// 本 crate 依赖的向量契约面（重导出，供装配方对齐类型）。
/// The vector contract surface this crate implements (re-exported so the
/// assembler's types line up).
pub use wiktor_core::traits::{
    ChunkType, DistanceMetric, VectorHit, VectorMetadata, VectorPoint, VectorStore,
};
