//! 内核实现模块。
//! Kernel implementation module.
//!
//! 组装 `traits` + `schema` 的实际执行内核：`sqlite`（SQLite 单连接
//! Mutex 串行化 + WAL/foreign_keys pragma）、`qdrant_vector`（默认向量后端，
//! feature-gated）与 `mock_vector`（进程内暴力扫描，评测基线）。
//! Assembles the concrete execution kernel from `traits` + `schema`: `sqlite`
//! (single-connection SQLite serialized via Mutex + WAL/foreign_keys pragmas),
//! `qdrant_vector` (the default vector backend, feature-gated) and `mock_vector`
//! (in-process brute-force scan, evaluation baseline).

mod mock_vector;
mod qdrant_vector;
mod sqlite;

pub use mock_vector::MockVectorStore;
pub use sqlite::SqliteKernel;

#[cfg(feature = "vector-qdrant")]
pub use qdrant_vector::QdrantVectorStore;
