//! 查询日志辅助（反馈层输入）。
//! DDL 见 `migrations/0001_create_core/up.sql`。

/// 写入一条查询日志的 SQL。
/// 注：反馈层在后续步骤使用，当前阶段允许 dead_code。
#[allow(dead_code)]
pub const SQL_INSERT_LOG: &str = r#"
INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure, hit_count, latency_ms, timestamp)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#;

/// 按游标返回最新 N 条查询日志（测试/诊断用）。
#[allow(dead_code)]
pub const SQL_RECENT_LOGS: &str = r#"
SELECT query_text, hit_count, latency_ms, timestamp
FROM query_logs
ORDER BY log_id DESC
LIMIT ?1
"#;
