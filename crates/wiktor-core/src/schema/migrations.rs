use crate::types::error::{Error, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

/// 嵌入的 SQLite 迁移（0001 两平面 + FTS5 基础，0002 FTS trigram）。
/// Embedded SQLite migrations (0001: two planes + FTS5 basics; 0002: FTS trigram).
///
/// 迁移文件位于 `crates/wiktor-core/migrations/<version>_<name>/up.sql`，
/// 由 diesel 在单个事务内应用；版本跟踪表 `__diesel_schema_migrations`。
/// Migration files live in `crates/wiktor-core/migrations/<version>_<name>/up.sql`
/// and are applied by diesel within a single transaction; the version tracking
/// table is `__diesel_schema_migrations`.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// 打开连接后立即调用：设置 pragma + 应用未跑过的迁移，幂等。
/// Call right after opening a connection: sets pragmas + applies pending
/// migrations; idempotent.
pub fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    // WAL + 外键强制（内存库 journal_mode 恒为 "memory"，pragma 静默忽略）
    // WAL + enforced foreign keys (in-memory DBs keep journal_mode "memory";
    // the pragma is silently ignored)
    conn.batch_execute("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| Error::Migration(e.to_string()))?;
    Ok(())
}

/// COUNT 结果行（`diesel::sql_query` 需 QueryableByName）。
/// COUNT result row (required by `diesel::sql_query` via QueryableByName).
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// 已应用的迁移数（`__diesel_schema_migrations` 行数），用作 schema 版本。
/// Number of applied migrations (rows in `__diesel_schema_migrations`), used as the
/// schema version.
pub fn schema_version(conn: &mut SqliteConnection) -> Result<i64> {
    let r: CountRow = diesel::sql_query("SELECT COUNT(*) AS n FROM __diesel_schema_migrations")
        .get_result(conn)?;
    Ok(r.n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> SqliteConnection {
        // 测试用独立文件库（WAL 需要文件库；内存库 journal_mode 恒 "memory"）。
        // 目录按 process id + 自增序号区分，避免并发测试共享同一文件库。
        // Tests use separate file-backed DBs (WAL needs a file DB; in-memory DBs
        // keep journal_mode "memory"). Directories are keyed by process id + an
        // auto-incrementing sequence so concurrent tests never share one file DB.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("wiktor_mig_test_{}_{}", std::process::id(), seq));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut c = conn();
        assert_eq!(schema_version(&mut c).unwrap(), 2);
        // 重跑无副作用
        // Re-running is side-effect free
        migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 2);
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("wiktor_mig_test_{}", std::process::id())),
        );
    }

    #[test]
    fn fts_uses_trigram_tokenizer() {
        let mut c = conn();
        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT sql AS value FROM sqlite_master WHERE type = 'table' AND name = 'pages_fts'",
        )
        .load(&mut c)
        .unwrap();
        let sql = &rows[0].value;
        assert!(sql.contains("trigram"), "pages_fts must use trigram: {sql}");
    }

    #[test]
    fn fts_stays_in_sync_with_pages() {
        let mut c = conn();
        // 手工插页，触发器应同步进 FTS
        // Manually inserting a page; the trigger should sync it into FTS
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at)
             VALUES ('milk-tea:drink:a', 'milk-tea:drink:a', 'milk-tea', 'drink',
                '波霸奶茶', '波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。', 'h',
                1, 'accepted', '0.1.0', 1, 'seed-manual', 'none', 1, 1)",
        )
        .execute(&mut c)
        .unwrap();

        let n_pages: i64 = count(&mut c, "pages");
        let n_fts: i64 = count(&mut c, "pages_fts");
        assert_eq!(n_pages, 1);
        assert_eq!(n_fts, 1);

        // 删除页应级联删除 FTS 行（DELETE 触发器）
        // Deleting a page should cascade-delete its FTS row (DELETE trigger)
        diesel::sql_query("DELETE FROM pages WHERE page_id = 'milk-tea:drink:a'")
            .execute(&mut c)
            .unwrap();
        let n_fts: i64 = count(&mut c, "pages_fts");
        assert_eq!(n_fts, 0);
    }
}

/// 单列文本行（测试查 sqlite_master 用）。
/// Single text-column row (used by tests to query sqlite_master).
#[cfg(test)]
#[derive(diesel::QueryableByName)]
struct SqlRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

/// 查询表行数（测试辅助）。
/// Counts rows in a table (test helper).
#[cfg(test)]
fn count(conn: &mut SqliteConnection, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) AS n FROM {table}");
    let r: CountRow = diesel::sql_query(sql).get_result(conn).unwrap();
    r.n
}
