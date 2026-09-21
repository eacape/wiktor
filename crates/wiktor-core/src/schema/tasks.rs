//! 编译任务队列辅助（状态机查询）。
//! Compile-task queue helpers (state-machine queries).
//! DDL 见 `migrations/0001_create_core/up.sql`。
//! DDL lives in `migrations/0001_create_core/up.sql`.

/// 领取一个 pending 任务（幂等去重由 UNIQUE(entity_id, source_revision, domain_pack_version) 保证）。
/// Claims one pending task (idempotent deduplication is guaranteed by UNIQUE(entity_id, source_revision, domain_pack_version)).
/// 注：编译管线在后续步骤使用，当前阶段允许 dead_code。
/// Note: used by the compilation pipeline in a later step; dead_code is allowed at this stage.
#[allow(dead_code)]
pub const SQL_CLAIM_NEXT: &str = r#"
UPDATE compile_tasks
SET status = 'running', lease_expires_at = unixepoch() + 300, retry_count = retry_count + 1
WHERE task_id = (
    SELECT task_id FROM compile_tasks
    WHERE status = 'pending' OR (status = 'running' AND lease_expires_at < unixepoch())
    ORDER BY task_id LIMIT 1
)
RETURNING task_id, entity_id, source_revision, domain_pack_version
"#;
