//! 核心抽象 trait 模块。
//! Core abstract-trait module.
//!
//! 定义解耦引擎与实现的接口，仅依赖 `types`：数据源适配
//! Defines interfaces decoupling the engine from implementations; depends only on `types`: data-source adapters
//! （`DataSource`）、实体存储（`EntityStore`）、编译器（`Compiler`）、
//! (`DataSource`), entity storage (`EntityStore`), compiler (`Compiler`),
//! QUG（`QueryUnderstandingGraph` / `QugBuilder`）、重排（`Reranker`）、
//! QUG (`QueryUnderstandingGraph` / `QugBuilder`), reranking (`Reranker`),
//! 领域包（`DomainPack`）、反馈分析（`FeedbackAnalyzer`）与可插拔向量后端
//! domain packs (`DomainPack`), feedback analysis (`FeedbackAnalyzer`) and pluggable vector backends
//! （`VectorStore`）。
//! (`VectorStore`).

mod compiler;
mod data_source;
mod domain_pack;
mod entity_store;
mod feedback;
mod qug;
mod reranker;
mod vector_store;

pub use compiler::Compiler;
pub use data_source::{DataSource, EntitySchema};
pub use domain_pack::{DomainConfig, DomainPack, EntityConfig};
pub use entity_store::EntityStore;
pub use feedback::{FeedbackAnalyzer, FeedbackReport};
pub use qug::{QueryUnderstandingGraph, QugBuilder};
pub use reranker::Reranker;
pub use vector_store::{
    ChunkType, DistanceMetric, VectorHit, VectorMetadata, VectorPoint, VectorStore,
};

pub use crate::types::*;
