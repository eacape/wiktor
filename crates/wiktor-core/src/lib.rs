//! # Wiktor Core
//!
//! 知识编译与检索中间件（检索数据库）的核心引擎 crate。
//! Core engine crate of the knowledge-compilation & retrieval middleware (retrieval database).
//!
//! 两平面数据模型：知识平面（Markdown Wiki）承载 LLM 编译产物，事实平面
//! （结构化元数据）由 ETL 直写；SQLite 一体化内核（WAL/FTS5）持久化两平面、
//! 编译任务队列、查询日志与 FTS5 倒排；向量检索通过 [`VectorStore`] trait
//! 隔离，默认 qdrant 外部服务（feature-gated），另备进程内 Mock 评测基线。
//! Two-plane data model: the knowledge plane (Markdown Wiki) holds LLM compilation
//! artifacts, while the fact plane (structured metadata) is written directly by ETL;
//! a unified SQLite kernel (WAL/FTS5) persists both planes, the compile task queue,
//! query logs and the FTS5 inverted index; vector retrieval is isolated behind the
//! [`VectorStore`] trait, defaulting to the external qdrant service (feature-gated),
//! with an in-process Mock baseline for evaluation.
//!
//! ## 模块划分
//! ## Module layout
//!
//! - [`types`]：领域类型（无外部依赖）
//! - [`traits`]：核心抽象 trait（只依赖 `types`）
//! - [`schema`]：SQLite 两平面 + 队列 + 日志 + 倒排 DDL（diesel 迁移 + raw SQL 逃生）
//! - [`kernel`]：SQLite 内核与向量后端实现（依赖 `traits` + `schema`）
//! - [`data`]：数据源适配器（JSONL 起步）
//! - [`seed`]：种子 Wiki 页面解析（手工编译产物的 Markdown 契约）
//! - [`compile`]：Step 4 编译管线契约（配置/哈希/输出契约/评分）
//! - [`eval`]：Step 5 golden 集加载与 A/B/C 评测（批 4 起：loader/校验/hash）
//! - [`types`]: domain types (no external dependencies)
//! - [`traits`]: core abstract traits (depend only on `types`)
//! - [`schema`]: SQLite DDL for the two planes + queue + logs + inverted index
//!   (diesel migrations + raw-SQL escape hatch)
//! - [`kernel`]: SQLite kernel and vector backend implementations (depend on `traits` + `schema`)
//! - [`data`]: data source adapters (starting with JSONL)
//! - [`seed`]: seed-wiki page parsing (Markdown contract for hand-compiled artifacts)
//! - [`compile`]: Step 4 compile-pipeline contracts (config/hash/output contract/scoring)
//! - [`eval`]: Step 5 golden-set loading and A/B/C evaluation (batch 4 onward:
//!   loader/validation/hash)

pub mod compile;
pub mod data;
mod db_schema;
#[cfg(feature = "embedding-http")]
pub mod embedding;
pub mod eval;
pub mod kernel;
pub mod query_engine;
pub mod schema;
pub mod seed;
pub mod traits;
pub mod types;
#[cfg(feature = "vector-qdrant")]
pub use kernel::QdrantVectorStore;
pub use kernel::{MockVectorStore, SqliteKernel};
pub use query_engine::{QueryEmbedder, QueryEngine};
pub use traits::*;
