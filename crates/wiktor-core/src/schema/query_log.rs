//! 查询日志辅助（反馈层输入）。
//! DDL 见 `migrations::MIGRATION_0001`。

/// 写入一条查询日志的 SQL。
/// 注：反馈层在后续步骤使用，当前阶段允许 dead_code。
#[allow(dead_code)]
pub const SQL_INSERT_LOG: &str = r#"
INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure, hit_count, latency_ms, timestamp)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#;
