//! 内核实现模块。
//! Kernel implementation module.
//!
//! 组装 `traits` + `schema` 的实际执行内核：`sqlite`（SQLite 单连接
//! Mutex 串行化 + WAL/foreign_keys pragma）、`compile_store`（Step 4 编译管线
//! 专用事务接口，admit/claim/heartbeat/recover/publish/failure）、
//! `qdrant_vector`（默认向量后端，feature-gated）与 `mock_vector`（进程内暴力
//! 扫描，评测基线）。
//! Assembles the concrete execution kernel from `traits` + `schema`: `sqlite`
//! (single-connection SQLite serialized via Mutex + WAL/foreign_keys pragmas),
//! `compile_store` (Step 4 compile-pipeline transactional API:
//! admit/claim/heartbeat/recover/publish/failure), `qdrant_vector` (the default
//! vector backend, feature-gated) and `mock_vector` (in-process brute-force scan,
//! evaluation baseline).

// pub(crate)：executor（compile 模块）复用预算估算公式 estimate_budget_units，
// 避免双实现漂移（Step 4 §8.4）。
// pub(crate): the executor (compile module) reuses estimate_budget_units so the
// budget formula cannot drift between two implementations (Step 4 §8.4).
pub(crate) mod compile_store;
mod mock_vector;
mod qdrant_vector;
mod sqlite;

pub use mock_vector::MockVectorStore;
pub use sqlite::SqliteKernel;

#[cfg(feature = "vector-qdrant")]
pub use qdrant_vector::QdrantVectorStore;
