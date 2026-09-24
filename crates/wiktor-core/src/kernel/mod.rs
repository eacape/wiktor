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
// pub：Step 6 批1 反馈闭环存储层（insert_feedback_idempotent / load_feedback_window /
// list_reviews 与契约类型单源定义；偏差 STEP6-003：不引入第二连接栈，SQLite 实现
// 按仓库模式落 kernel，`wiktor-feedback` 只持有 FeedbackStore trait 与 re-export）。
// pub: the Step 6 batch-1 feedback-loop storage layer (insert_feedback_idempotent /
// load_feedback_window / list_reviews plus the single-source contract types;
// deviation STEP6-003: no second connection stack — the SQLite implementation
// lands in the kernel per the repo pattern, and `wiktor-feedback` only carries
// the FeedbackStore trait plus re-exports).
pub mod feedback_store;
mod sqlite;

// Step8 批 B4：recover_compile_leases 的结构化返回（§6.3 RecoveryStats）在
// kernel 层再导出——compile_store 是 pub(crate) 模块，公开方法
// SqliteKernel::recover_compile_leases 的返回类型必须可命名。
// Step8 batch B4: the structured return of recover_compile_leases (§6.3
// RecoveryStats) is re-exported at kernel level — compile_store is a pub(crate)
// module, so the return type of the public method
// SqliteKernel::recover_compile_leases must be nameable.
pub use compile_store::RecoveryStats;
// 真实嵌入实验：accepted 页向量构建读取面的返回类型（供 CLI `vector build`
// 消费；compile_store 是 pub(crate) 模块，公开方法的返回类型必须可命名）。
// Real-embedding experiment: the return type of the accepted-page vector-build
// read surface (consumed by the CLI `vector build`; compile_store is a
// pub(crate) module, so the public method's return type must be nameable).
pub use compile_store::AcceptedPageVector;
// Step7 B4（spec step7 §3 D5/A15）：Compile.Status 的快照类型公开给 server。
// Step7 B4 (spec step7 §3 D5/A15): the Compile.Status snapshot type is exposed
// to the server.
pub use compile_store::CompileTaskStatus;
pub use mock_vector::MockVectorStore;
pub use qug_store::{build_and_publish_qug, load_active_qug, QugStore, QUG_STALE_PREFIX};
pub use sqlite::SqliteKernel;
// Step 6 批1 契约类型在 kernel 层再导出（wiktor-feedback 全量 re-export 为 §6 面；
// 批3 追加 ReviewSuggestionInput，批4 追加 ReviewOutcome，批5 追加
// FeedbackRejectionReason）。
// Step 6 batch-1 contract types re-exported at kernel level (wiktor-feedback
// re-exports them fully as the §6 surface; batch 3 adds ReviewSuggestionInput,
// batch 4 adds ReviewOutcome, batch 5 adds FeedbackRejectionReason).
pub use feedback_store::{
    FeedbackEvent, FeedbackEventInput, FeedbackIngested, FeedbackKind, FeedbackRejectionReason,
    QueryLogSnapshot, ReviewItem, ReviewOutcome, ReviewStatus, ReviewSuggestionInput,
};
// Step 6 批2：查询日志写入口（QueryEngine 用）——载荷类型与 domain 缺省常量。
// Step 6 batch 2: the query-log write entry (used by the QueryEngine) — the
// payload type and the default-domain constant.
pub use sqlite::{QueryLogInsert, DEFAULT_QUERY_LOG_DOMAIN};

#[cfg(feature = "vector-qdrant")]
pub use qdrant_vector::QdrantVectorStore;
