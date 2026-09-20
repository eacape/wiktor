//! # Wiktor Core
//!
//! 知识编译与检索中间件（检索数据库）的核心引擎 crate。
//!
//! 两平面数据模型：知识平面（Markdown Wiki）承载 LLM 编译产物，事实平面
//! （结构化元数据）由 ETL 直写；SQLite 一体化内核（WAL/FTS5）持久化两平面、
//! 编译任务队列、查询日志与 FTS5 倒排；向量检索通过 [`VectorStore`] trait
//! 隔离，默认 qdrant 外部服务（feature-gated），另备进程内 Mock 评测基线。
//!
//! ## 模块划分
//!
//! - [`types`]：领域类型（无外部依赖）
//! - [`traits`]：核心抽象 trait（只依赖 `types`）
//! - [`schema`]：SQLite 两平面 + 队列 + 日志 + 倒排 DDL（diesel 迁移 + raw SQL 逃生）
//! - [`kernel`]：SQLite 内核与向量后端实现（依赖 `traits` + `schema`）
//! - [`data`]：数据源适配器（JSONL 起步）
//! - [`seed`]：种子 Wiki 页面解析（手工编译产物的 Markdown 契约）

pub mod data;
mod db_schema;
pub mod kernel;
pub mod schema;
pub mod seed;
pub mod traits;
pub mod types;
#[cfg(feature = "vector-qdrant")]
pub use kernel::QdrantVectorStore;
pub use kernel::{MockVectorStore, SqliteKernel};
pub use traits::*;
