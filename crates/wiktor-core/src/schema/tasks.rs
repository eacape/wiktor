//! 编译任务队列辅助（状态机查询）。
//! Compile-task queue helpers (state-machine queries).
//! DDL 见 `migrations/0001_create_core/up.sql` 与 `migrations/0003_compile_pipeline/up.sql`。
//! DDL lives in `migrations/0001_create_core/up.sql` and
//! `migrations/0003_compile_pipeline/up.sql`.

/// 参数化领取 SQL（Step 4 spec §8.3：替换旧 `SQL_CLAIM_NEXT` 常量、保留名称）。
/// Parameterized claim SQL (Step 4 spec §8.3: replaces the legacy `SQL_CLAIM_NEXT`
/// constant, keeping the name).
///
/// 与 0001 时代旧常量的差异：
/// - 只查**当前 run 已 admission** 的 `task_ids`（旧版扫全表）；
/// - 只取 `pending` 且 `next_attempt_at <= now` 且 `retry_count < max_retries`
///   （旧版会偷取过期 running 并加 retry_count；过期 running 必须先经
///   `SqliteKernel::recover_compile_leases` 回收）；
/// - 按 `(next_attempt_at, task_id)` 取 1，领取**不增加** retry_count；
/// - 返回列供租约/预算/attempt 记账使用。
///
/// Differences from the 0001-era constant:
/// - Only tasks **admitted into the current run** (`task_ids`) are considered (the
///   old version scanned the whole table);
/// - Only `pending`, due (`next_attempt_at <= now`) tasks with
///   `retry_count < max_retries` qualify (the old version stole expired running
///   tasks and incremented retry_count; expired running tasks must first go
///   through `SqliteKernel::recover_compile_leases`);
/// - One task ordered by `(next_attempt_at, task_id)`; claiming **never**
///   increments retry_count;
/// - The returned columns feed lease/budget/attempt bookkeeping.
///
/// 参数（按序 bind，绝不内联数据）：`?1` = task_ids 的 JSON 数组文本，经
/// SQLite 内建 `json_each` 展开为候选集合（diesel 的 sql_query bind 链无法
/// 动态追加，故用表值函数承载变长参数）；`?2` = now。
/// Parameters (bound in order, never inlined): `?1` = the task_ids JSON array
/// text, expanded by SQLite's built-in `json_each` table-valued function (diesel's
/// sql_query bind chain cannot be appended dynamically, so a variadic candidate
/// set rides on a TVF); `?2` = now.
#[allow(dead_code)]
pub const SQL_CLAIM_NEXT: &str = r#"
SELECT task_id, epoch, desired_hash, source_json, dependencies_json,
       task_token_budget, reserved_tokens, attempt_count
FROM compile_tasks
WHERE task_id IN (SELECT CAST(value AS INTEGER) FROM json_each(?))
  AND status = 'pending'
  AND next_attempt_at <= ?
  AND retry_count < max_retries
ORDER BY next_attempt_at, task_id
LIMIT 1
"#;
