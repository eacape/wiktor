use crate::types::error::Result;
use rusqlite::Connection;

pub const CURRENT_SCHEMA_VERSION: i64 = 1;

/// 幂等迁移：打开连接后立即调用。已应用的版本跳过，fresh 库走全量。
///
/// 每个迁移在独立事务中应用：失败回滚并返回 `Error::Migration`，不会留下半迁移状态。
pub fn migrate(conn: &mut Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;

    for (version, sql) in migrations() {
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, unix_now()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// 迁移清单（有序）。`migrate` 内按 version 升序应用。
fn migrations() -> Vec<(i64, &'static str)> {
    vec![(1, MIGRATION_0001)]
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// 迁移 0001：两平面 + FTS5 + 任务队列 + 查询日志。
// 注意：SQLite 每语句自动提交，CREATE TRIGGER 不能与其它语句同批；
// 这里全部放在单个 execute_batch（事务内）执行是安全的，因为事务内没有显式 BEGIN/COMMIT 冲突。
const MIGRATION_0001: &str = r#"
-- ============ 知识平面 ============
CREATE TABLE pages (
    page_id             TEXT PRIMARY KEY,
    entity_id           TEXT NOT NULL,
    domain              TEXT NOT NULL,
    entity_type         TEXT NOT NULL,
    title               TEXT NOT NULL,
    content             TEXT NOT NULL,
    content_hash        TEXT NOT NULL,
    generation          INTEGER NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('candidate','accepted','quarantined')),
    domain_pack_version TEXT NOT NULL,
    compiled_at         INTEGER NOT NULL,
    model_version       TEXT NOT NULL,
    embedding_model     TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX idx_pages_entity_id   ON pages(entity_id);
CREATE INDEX idx_pages_domain      ON pages(domain);
CREATE INDEX idx_pages_status      ON pages(status);
CREATE INDEX idx_pages_generation  ON pages(generation);
CREATE INDEX idx_pages_content_hash ON pages(content_hash);

CREATE TABLE page_quality (
    page_id           TEXT PRIMARY KEY REFERENCES pages(page_id) ON DELETE CASCADE,
    coverage          REAL NOT NULL,
    citation          REAL NOT NULL,
    schema_compliance REAL NOT NULL,
    density           REAL NOT NULL,
    consistency       REAL CHECK (consistency IS NULL OR (consistency >= 0.0 AND consistency <= 1.0)),
    overall           REAL NOT NULL
);

CREATE TABLE page_sections (
    section_id   TEXT PRIMARY KEY,
    page_id      TEXT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
    heading      TEXT NOT NULL,
    content      TEXT NOT NULL,
    section_index INTEGER NOT NULL
);
CREATE INDEX idx_sections_page_id ON page_sections(page_id);

CREATE TABLE generations (
    generation          INTEGER PRIMARY KEY AUTOINCREMENT,
    domain_pack         TEXT NOT NULL,
    domain_pack_version TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'building' CHECK (status IN ('building','published')),
    created_at          INTEGER NOT NULL
);

-- ============ 事实平面 ============
CREATE TABLE facts (
    entity_id       TEXT NOT NULL,
    field_name      TEXT NOT NULL,
    field_type      TEXT NOT NULL CHECK (field_type IN ('numeric','text','boolean','reflist','timestamp')),
    value_numeric   REAL,
    value_text      TEXT,
    value_boolean   INTEGER,
    value_timestamp INTEGER,
    source_revision INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (entity_id, field_name)
);
CREATE INDEX idx_facts_field_numeric   ON facts(field_name, value_numeric)   WHERE field_type = 'numeric';
CREATE INDEX idx_facts_field_text      ON facts(field_name, value_text)      WHERE field_type = 'text';
CREATE INDEX idx_facts_field_timestamp ON facts(field_name, value_timestamp) WHERE field_type = 'timestamp';

CREATE TABLE fact_refs (
    entity_id  TEXT NOT NULL,
    field_name TEXT NOT NULL,
    ref_value  TEXT NOT NULL,
    PRIMARY KEY (entity_id, field_name, ref_value)
);
CREATE INDEX idx_fact_refs_field ON fact_refs(field_name, ref_value);

-- ============ FTS5 倒排（知识平面） ============
CREATE VIRTUAL TABLE pages_fts USING fts5(
    page_id UNINDEXED,
    entity_id UNINDEXED,
    title,
    content,
    tokenize = 'unicode61'
);

CREATE TRIGGER pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_update AFTER UPDATE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_delete AFTER DELETE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
END;

-- ============ 编译任务队列 ============
CREATE TABLE compile_tasks (
    task_id             INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_id           TEXT NOT NULL,
    source_revision     INTEGER NOT NULL,
    domain_pack_version TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','succeeded','failed','dead')),
    retry_count         INTEGER NOT NULL DEFAULT 0,
    max_retries         INTEGER NOT NULL DEFAULT 3,
    lease_expires_at    INTEGER,
    error_message       TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (entity_id, source_revision, domain_pack_version)
);
CREATE INDEX idx_tasks_status            ON compile_tasks(status);
CREATE INDEX idx_tasks_lease_expires_at  ON compile_tasks(lease_expires_at) WHERE status = 'running';

-- ============ 查询日志 ============
CREATE TABLE query_logs (
    log_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    query_text       TEXT NOT NULL,
    query_json       TEXT NOT NULL,
    rewritten_json   TEXT,
    rewrite_failure  INTEGER NOT NULL DEFAULT 0,
    hit_count        INTEGER NOT NULL,
    latency_ms       INTEGER NOT NULL,
    timestamp        INTEGER NOT NULL
);
CREATE INDEX idx_query_logs_timestamp         ON query_logs(timestamp);
CREATE INDEX idx_query_logs_rewrite_failure   ON query_logs(rewrite_failure) WHERE rewrite_failure = 1;
CREATE INDEX idx_query_logs_hit_count         ON query_logs(hit_count) WHERE hit_count = 0;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let v1: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v1, CURRENT_SCHEMA_VERSION);

        // 重跑无副作用
        migrate(&mut conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn pragmas_enabled() {
        // WAL 需要文件库；内存库 journal_mode 恒为 "memory"
        let dir = std::env::temp_dir().join(format!("wiktor_pragma_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn).unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
