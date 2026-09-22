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
// pub：Step 5 批2 的 QUG 存储层（QugStore trait + SqliteKernel 实现 +
// build_and_publish_qug 编排），供 CLI 后续批次直接使用；query_engine 不依赖
// diesel，故编排落在本模块（spec step5 §4.1）。批3 追加运行时加载入口
// load_active_qug 与 stale 稳定前缀。
// pub: the Step 5 batch-2 QUG storage layer (QugStore trait + SqliteKernel
// implementation + the build_and_publish_qug orchestration) for later CLI
// batches; query_engine stays diesel-free, so the orchestration lives here
// (spec step5 §4.1). Batch 3 adds the runtime load entry load_active_qug and
// the stale stable prefix.
pub mod qug_store;
mod sqlite;

pub use mock_vector::MockVectorStore;
pub use qug_store::{build_and_publish_qug, load_active_qug, QugStore, QUG_STALE_PREFIX};
pub use sqlite::SqliteKernel;

#[cfg(feature = "vector-qdrant")]
pub use qdrant_vector::QdrantVectorStore;
