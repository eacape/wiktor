//! `FeedbackStore` trait（spec §6 类型契约；批1 三方法 + 批3 建议插入 + 批4
//! approve/ignore + 批5 批量插入）与 `SqliteKernel` 委托实现（偏差 STEP6-003：
//! 实现单源在 core kernel，此处只做 trait 适配）。
//! The `FeedbackStore` trait (spec §6 type contract; batch-1 trio + batch-3
//! suggestion insert + batch-4 approve/ignore + batch-5 batch insert) plus the
//! `SqliteKernel` delegation impl (deviation STEP6-003: the implementation is
//! single-source in the core kernel; this module only adapts the trait).
//!
//! 错误面：使用 core 的统一 [`wiktor_core::types::error::Error`]（spec §6 允许
//! "现有错误类型"）——非法输入 `Validation`、数据库故障 `Database`、损坏持久化
//! 载荷 `Internal`；任何错误都不转成空结果。
//! Error surface: the core unified [`wiktor_core::types::error::Error`] (spec §6
//! allows "the existing error types") — illegal input is `Validation`, DB faults
//! are `Database`, corrupt persisted payloads are `Internal`; no error is ever
//! converted into an empty result.

use wiktor_core::kernel::feedback_store::{
    FeedbackEvent, FeedbackEventInput, FeedbackIngested, QueryLogSnapshot, ReviewItem,
    ReviewOutcome, ReviewStatus, ReviewSuggestionInput,
};
use wiktor_core::types::error::Result;
use wiktor_core::SqliteKernel;

/// 反馈存储契约（spec §6；SQLite 单连接同步语义，spec §2：数据库锁不得跨 await，
/// 故全为同步方法；`#[async_trait]` 对同步方法是 no-op，按 spec 原文保留以对齐
/// 契约面）。
/// The feedback-storage contract (spec §6; SQLite single-connection synchronous
/// semantics — spec §2 forbids holding a DB lock across await, hence all-sync
/// methods; `#[async_trait]` is a no-op for sync methods and is kept verbatim
/// from the spec to align the contract surface).
#[async_trait::async_trait]
pub trait FeedbackStore: Send + Sync {
    /// 幂等插入一条反馈事件（D5）：重复 (domain, idempotency_key) 返回原
    /// event_id/received_at 且 `replayed=true`，不更新载荷。
    /// Idempotently inserts one feedback event (D5): a duplicate
    /// (domain, idempotency_key) returns the original event_id/received_at with
    /// `replayed=true` and never overwrites the payload.
    fn insert_idempotent(&self, input: &FeedbackEventInput, now: i64) -> Result<FeedbackIngested>;

    /// 幂等批量插入（批5，spec §7.1）：单个 BEGIN IMMEDIATE 事务内逐条走与
    /// [`FeedbackStore::insert_idempotent`] 相同的事务体；任一事件失败整批回滚、
    /// 零行落库（禁止部分成功），重复键逐条回放原 event_id/received_at。
    /// Idempotent batch insert (batch 5, spec §7.1): every event runs the same
    /// transaction body as [`FeedbackStore::insert_idempotent`] inside one BEGIN
    /// IMMEDIATE transaction; any failing event rolls the whole batch back with
    /// zero rows persisted (partial success forbidden), duplicates replay their
    /// original event_id/received_at per event.
    fn insert_batch_idempotent(
        &self,
        inputs: &[FeedbackEventInput],
        now: i64,
    ) -> Result<Vec<FeedbackIngested>>;

    /// 读取 [from, to] 窗口内该 domain 的查询日志与反馈事件两组快照（D11 分析
    /// 窗口输入；同事务一致读取）。
    /// Reads both snapshots — the domain's query logs and feedback events —
    /// inside [from, to] (the D11 analysis-window input; read consistently in
    /// one transaction).
    fn load_window(
        &self,
        domain: &str,
        from: i64,
        to: i64,
    ) -> Result<(Vec<QueryLogSnapshot>, Vec<FeedbackEvent>)>;

    /// 按 domain（可选 status）分页读审核队列（limit 1..=1000）。
    /// Pages through the review queue by domain (optionally by status), with a
    /// limit of 1..=1000.
    fn list_reviews(
        &self,
        domain: &str,
        status: Option<ReviewStatus>,
        limit: u32,
    ) -> Result<Vec<ReviewItem>>;

    /// 单事务批量插入审核建议（批3；批6 CLI analyze 调用）：UNIQUE
    /// (domain,action,subject_json) 冲突跳过不报错，只返回实际新插入的
    /// review_id（A12 重复分析幂等）。
    /// Bulk-inserts review suggestions in one transaction (batch 3; called by
    /// the batch-6 CLI analyze): UNIQUE(domain,action,subject_json) conflicts
    /// are skipped without error and only the actually newly inserted
    /// review_ids are returned (A12 repeated-analysis idempotency).
    fn insert_review_suggestions(
        &self,
        domain: &str,
        suggestions: &[ReviewSuggestionInput],
    ) -> Result<Vec<i64>>;

    /// 审核批准（批4，D12 / A15 / A16）：单 BEGIN IMMEDIATE 内校验 status=pending；
    /// supplemental_compile 校验 subject 五必需字段并在同一事务内复用既有
    /// admission（成功回填 compile_task_id，admission 未排队或失败则整体回滚、
    /// review 保持 pending）；query_template 仅写 approved + 审计字段；重复审核
    /// 拒绝。
    /// Approves a review item (batch 4, D12 / A15 / A16): pending is validated
    /// inside one BEGIN IMMEDIATE; supplemental_compile validates the five
    /// required subject fields and reuses the existing admission inside the same
    /// transaction (success backfills compile_task_id; an admission that fails
    /// to queue rolls everything back with the review staying pending);
    /// query_template only writes approved plus audit fields; repeated reviews
    /// are rejected.
    fn approve_review(&self, review_id: i64, reviewer: &str, now: i64) -> Result<ReviewOutcome>;

    /// 忽略审核项（批4；A16）：pending → status='ignored' + 审计字段的纯审计
    /// 转换（不校验 subject、不触碰 compile_tasks）；非 pending 拒绝。
    /// Ignores a review item (batch 4; A16): a pure audit transition from pending
    /// to status='ignored' plus audit fields (no subject validation, no
    /// compile_tasks touches); non-pending rows are rejected.
    fn ignore_review(&self, review_id: i64, reviewer: &str, now: i64) -> Result<()>;
}

#[async_trait::async_trait]
impl FeedbackStore for SqliteKernel {
    fn insert_idempotent(&self, input: &FeedbackEventInput, now: i64) -> Result<FeedbackIngested> {
        self.insert_feedback_idempotent(input, now)
    }

    fn insert_batch_idempotent(
        &self,
        inputs: &[FeedbackEventInput],
        now: i64,
    ) -> Result<Vec<FeedbackIngested>> {
        self.insert_feedback_batch_idempotent(inputs, now)
    }

    fn load_window(
        &self,
        domain: &str,
        from: i64,
        to: i64,
    ) -> Result<(Vec<QueryLogSnapshot>, Vec<FeedbackEvent>)> {
        self.load_feedback_window(domain, from, to)
    }

    fn list_reviews(
        &self,
        domain: &str,
        status: Option<ReviewStatus>,
        limit: u32,
    ) -> Result<Vec<ReviewItem>> {
        self.list_reviews(domain, status, limit)
    }

    fn insert_review_suggestions(
        &self,
        domain: &str,
        suggestions: &[ReviewSuggestionInput],
    ) -> Result<Vec<i64>> {
        self.insert_review_suggestions(domain, suggestions)
    }

    fn approve_review(&self, review_id: i64, reviewer: &str, now: i64) -> Result<ReviewOutcome> {
        self.approve_review(review_id, reviewer, now)
    }

    fn ignore_review(&self, review_id: i64, reviewer: &str, now: i64) -> Result<()> {
        self.ignore_review(review_id, reviewer, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiktor_core::kernel::feedback_store::FeedbackKind;

    /// trait 委托冒烟：经 `FeedbackStore` 走一遍 insert/load/list（A1 的依赖图
    /// 与可达性同时得到验证）。
    /// Delegation smoke test through `FeedbackStore` for insert/load/list
    /// (verifies the A1 dependency graph and reachability at the same time).
    #[test]
    fn sqlite_kernel_delegates_feedback_store() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        kernel
            .execute_batch(
                "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure,
                        hit_count, latency_ms, timestamp, domain)
                 VALUES ('波霸奶茶', '{}', NULL, 0, 3, 12, 1000, 'milk-tea')",
            )
            .unwrap();

        let input = FeedbackEventInput {
            idempotency_key: "k-1".into(),
            domain: "milk-tea".into(),
            log_id: 1,
            kind: FeedbackKind::Click,
            page_id: Some("milk-tea:drink:boba".into()),
            rating: None,
            metadata: serde_json::json!({}),
        };

        let store: &dyn FeedbackStore = &kernel;
        let ingested = store.insert_idempotent(&input, 1000).unwrap();
        assert!(!ingested.replayed);

        // 批5：trait 面的批量插入委托（重复键回放 + 新键插入混合批）。
        // Batch 5: batch-insert delegation through the trait face (duplicate
        // replay + new insert in one mixed batch).
        let mut input2 = input.clone();
        input2.idempotency_key = "k-2".into();
        let batch = store
            .insert_batch_idempotent(&[input.clone(), input2], 1100)
            .unwrap();
        assert!(batch[0].replayed, "k-1 must replay with its original id");
        assert_eq!(batch[0].event_id, ingested.event_id);
        assert!(!batch[1].replayed);

        let (logs, events) = store.load_window("milk-tea", 0, 2000).unwrap();
        assert_eq!(logs.len(), 1);
        // 批5 起窗口含 2 条事件：k-1（原插入）+ k-2（批量委托插入）。
        // Since batch 5 the window holds 2 events: k-1 (original insert) plus
        // k-2 (batch-delegation insert).
        assert_eq!(events.len(), 2);

        let reviews = store.list_reviews("milk-tea", None, 100).unwrap();
        assert!(reviews.is_empty());

        // 批3：trait 面的 insert_review_suggestions 委托（UNIQUE 冲突跳过 +
        // 只返回新插入 id）。
        // Batch 3: insert_review_suggestions delegation through the trait face
        // (UNIQUE conflicts skipped + only new ids returned).
        let suggestion = ReviewSuggestionInput {
            action: "query_template".into(),
            source_log_ids_json: "[1]".into(),
            subject_json: r#"{"normalized_query":"波霸奶茶"}"#.into(),
            reason_json: r#"{"signal":"rewrite_failure"}"#.into(),
            created_at: 1000,
        };
        let inserted = store
            .insert_review_suggestions("milk-tea", &[suggestion])
            .unwrap();
        assert_eq!(inserted.len(), 1);
        let replay_input = ReviewSuggestionInput {
            action: "query_template".into(),
            source_log_ids_json: "[1]".into(),
            subject_json: r#"{"normalized_query":"波霸奶茶"}"#.into(),
            reason_json: r#"{"signal":"rewrite_failure"}"#.into(),
            created_at: 1001,
        };
        assert!(store
            .insert_review_suggestions("milk-tea", &[replay_input])
            .unwrap()
            .is_empty());

        // 批4：trait 面的 approve_review/ignore_review 委托（query_template 审计
        // 批准 + ignore 转换 + 重复审核拒绝；A16）。
        // Batch 4: approve_review/ignore_review delegation through the trait face
        // (audit-only query_template approval + the ignore transition + repeated
        // reviews rejected; A16).
        let outcome = store.approve_review(1, "ops", 2000).unwrap();
        assert_eq!(outcome.status, ReviewStatus::Approved);
        assert_eq!(outcome.compile_task_id, None);

        let suggestion2 = ReviewSuggestionInput {
            action: "supplemental_compile".into(),
            source_log_ids_json: "[1]".into(),
            subject_json: r#"{"signal":"zero_recall"}"#.into(),
            reason_json: r#"{"signal":"zero_recall"}"#.into(),
            created_at: 1000,
        };
        let review_id = store
            .insert_review_suggestions("milk-tea", &[suggestion2])
            .unwrap()[0];
        store.ignore_review(review_id, "bob", 3000).unwrap();
        assert!(store.ignore_review(review_id, "bob", 3001).is_err());
        let err = store.approve_review(review_id, "alice", 3001).unwrap_err();
        assert!(matches!(
            err,
            wiktor_core::types::error::Error::Validation(_)
        ));
    }
}
