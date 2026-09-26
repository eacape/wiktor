//! # Wiktor Feedback
//!
//! Step 6 反馈闭环 crate（spec `step6-feedback-loop.md` §1/§3 D1、§6）：反馈分析、
//! 三类盲区报告与人工审核队列访问。批1 只交付最小骨架——`FeedbackStore` trait、
//! 契约类型 re-export 与 SQLite 委托实现；分析器（批3）、报告（批3）、approve/
//! ignore（批4）随后续批次加入，均通过持有 `wiktor_core::SqliteKernel` 句柄
//! 委托调用，本 crate 自身不持有连接。
//! The Step 6 feedback-loop crate (spec `step6-feedback-loop.md` §1/§3 D1, §6):
//! feedback analysis, the three blind-spot reports and human-review queue
//! access. Batch 1 ships only the minimal skeleton — the `FeedbackStore` trait,
//! re-exports of the contract types and the SQLite delegation impl; the analyzer
//! (batch 3), reports (batch 3) and approve/ignore (batch 4) land in later
//! batches, all delegating through a `wiktor_core::SqliteKernel` handle — this
//! crate never owns a connection itself.
//!
//! ## 依赖边界（A1）
//! ## Dependency boundaries (A1)
//!
//! - 只依赖 `wiktor-core`（+ serde/serde_json/async-trait/tracing）；禁止依赖
//!   `wiktor-server`，也禁止 `wiktor-core` 反向依赖本 crate；
//! - 偏差 STEP6-003：不引入第二连接栈。SQLite 实现按仓库既有模式单源落在
//!   `wiktor-core/src/kernel/feedback_store.rs`（`impl SqliteKernel` 方法）；
//!   本 crate 只持有 trait 与类型面。
//! - Depends on `wiktor-core` only (+ serde/serde_json/async-trait/tracing);
//!   depending on `wiktor-server` is forbidden, and so is a reverse
//!   `wiktor-core` → feedback edge;
//! - Deviation STEP6-003: no second connection stack. The SQLite implementation
//!   lives single-source in `wiktor-core/src/kernel/feedback_store.rs`
//!   (`impl SqliteKernel` methods) following the repo's existing pattern; this
//!   crate carries only the trait and the type surface.
//!
//! ## 模块划分
//! ## Module layout
//!
//! - [`store`]：`FeedbackStore` trait（insert_idempotent / load_window /
//!   list_reviews / insert_review_suggestions / approve_review / ignore_review）+
//!   `SqliteKernel` 委托实现；
//! - [`analyzer`]：三信号分析器（D11 判据 + subject_json 确定性去重；纯函数，
//!   不写库、不调 compile）；
//! - [`report`]：`FeedbackReport` 模型、`report_hash`（BLAKE3 + canonical
//!   JSON）与双语三件套落盘（A13）；
//! - 契约类型（`FeedbackKind`/`FeedbackEventInput`/`FeedbackIngested`/
//!   `FeedbackEvent`/`QueryLogSnapshot`/`ReviewItem`/`ReviewStatus`/
//!   `ReviewSuggestionInput`/`ReviewOutcome`）单源定义在 core（kernel/
//!   feedback_store.rs，偏差 STEP6-003 的必然结果），此处全量 re-export 为
//!   spec §6 的公开契约面；
//! - [`store`]: the `FeedbackStore` trait (insert_idempotent / load_window /
//!   list_reviews / insert_review_suggestions / approve_review / ignore_review)
//!   plus the `SqliteKernel` delegation impl;
//! - [`analyzer`]: the three-signal analyzer (D11 criteria + deterministic
//!   subject_json dedup; a pure function that neither writes nor calls
//!   compile);
//! - [`report`]: the `FeedbackReport` model, the `report_hash` (BLAKE3 over
//!   canonical JSON) and the bilingual trio on-disk output (A13);
//! - The contract types (`FeedbackKind`/`FeedbackEventInput`/
//!   `FeedbackIngested`/`FeedbackEvent`/`QueryLogSnapshot`/`ReviewItem`/
//!   `ReviewStatus`/`ReviewSuggestionInput`/`ReviewOutcome`) are defined
//!   single-source in core (kernel/feedback_store.rs — the inevitable result of
//!   deviation STEP6-003) and fully re-exported here as the spec §6 public
//!   contract surface.

pub mod analyzer;
pub mod report;
pub mod store;

pub use analyzer::{
    FeedbackAnalyzer, FeedbackKeyMatcher, FeedbackWindow, StandardFeedbackAnalyzer,
    StandardKeyMatcher,
};
pub use report::{
    BlindSpotQuery, FeedbackCounts, FeedbackReport, FeedbackThresholds, LowQualityPage,
    ReviewSuggestion,
};
pub use store::FeedbackStore;
// spec §6 契约类型的 re-export（单源在 core，见模块文档 STEP6-003 说明；批5
// 追加 FeedbackRejectionReason 供 server 的 413 审计与指标映射）。
// Re-exports of the spec §6 contract types (single source in core; see the
// STEP6-003 note in the module docs; batch 5 adds FeedbackRejectionReason for
// the server's 413 audit and metrics mapping).
pub use wiktor_core::kernel::feedback_store::{
    FeedbackEvent, FeedbackEventInput, FeedbackIngested, FeedbackKind, FeedbackRejectionReason,
    QueryLogSnapshot, ReviewItem, ReviewOutcome, ReviewStatus, ReviewSuggestionInput,
};
