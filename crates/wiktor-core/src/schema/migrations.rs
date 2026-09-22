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
        // A22：0003 后 schema 版本为 3。
        // A22: schema version is 3 after 0003.
        assert_eq!(schema_version(&mut c).unwrap(), 3);
        // 重跑无副作用
        // Re-running is side-effect free
        migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 3);
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

    // A22：FTS 仅 accepted 入索引 —— candidate 不进、accepted 进、quarantined 出。
    // A22: FTS indexes accepted only — candidate kept out, accepted in,
    // quarantined out.
    #[test]
    fn fts_gate_indexes_accepted_only() {
        let mut c = conn();
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at)
             VALUES ('milk-tea:drink:boba', 'milk-tea:drink:boba', 'milk-tea', 'drink',
                '波霸奶茶', '波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。', 'h',
                2, 'candidate', '0.1.0', 1, 'compile', 'none', 1, 1)",
        )
        .execute(&mut c)
        .unwrap();
        assert_eq!(
            count(&mut c, "pages_fts"),
            0,
            "candidate must stay out of FTS"
        );

        // candidate → accepted：UPDATE 触发器删旧 + 条件插新。
        // candidate → accepted: the UPDATE trigger deletes the old row and
        // conditionally inserts the new one.
        diesel::sql_query(
            "UPDATE pages SET status = 'accepted' WHERE page_id = 'milk-tea:drink:boba'",
        )
        .execute(&mut c)
        .unwrap();
        assert_eq!(
            count(&mut c, "pages_fts"),
            1,
            "accepted page must be indexed"
        );

        // accepted → quarantined：隔离版本必须离开查询索引（§8.1）。
        // accepted → quarantined: a quarantined version must leave the query index.
        diesel::sql_query(
            "UPDATE pages SET status = 'quarantined' WHERE page_id = 'milk-tea:drink:boba'",
        )
        .execute(&mut c)
        .unwrap();
        assert_eq!(
            count(&mut c, "pages_fts"),
            0,
            "quarantined page must leave FTS"
        );
    }

    /// 搭一个 0001+0002 的 legacy 库（含 __diesel_schema_migrations 版本行），
    /// 用于真实升级路径测试。
    /// Builds a legacy 0001+0002 database (including __diesel_schema_migrations
    /// version rows) for real upgrade-path tests.
    fn legacy_conn(dir_name: &str) -> SqliteConnection {
        use diesel::connection::SimpleConnection;
        let dir = std::env::temp_dir().join(format!(
            "wiktor_mig_legacy_{}_{}",
            std::process::id(),
            dir_name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        c.batch_execute(include_str!("../../migrations/0001_create_core/up.sql"))
            .unwrap();
        c.batch_execute(include_str!("../../migrations/0002_fts_trigram/up.sql"))
            .unwrap();
        c.batch_execute(
            "CREATE TABLE __diesel_schema_migrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                version TEXT NOT NULL,
                run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        // 版本行从全新 migrate 的库里取（目录名即 diesel 记录的版本串）。
        // Version rows are taken from a freshly migrated DB (the directory name is
        // diesel's recorded version string).
        let mut fresh = conn();
        let versions: Vec<SqlRow> = diesel::sql_query(
            "SELECT version AS value FROM __diesel_schema_migrations ORDER BY version",
        )
        .load(&mut fresh)
        .unwrap();
        for v in &versions[..2] {
            diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES (?)")
                .bind::<diesel::sql_types::Text, _>(&v.value)
                .execute(&mut c)
                .unwrap();
        }
        c
    }

    // A22：0001+0002 老库升级到 0003 —— legacy accepted 页回填 FTS、candidate
    // 不入、旧未终结任务标记 dead/failed、legacy generation 哨兵就位。
    // A22: 0001+0002 → 0003 upgrade — legacy accepted pages backfilled into FTS,
    // candidates kept out, legacy non-terminal tasks marked dead/failed, and the
    // legacy generation sentinel in place.
    #[test]
    fn migration_0003_upgrade_from_legacy() {
        let mut c = legacy_conn("upgrade");
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at)
             VALUES ('milk-tea:drink:boba', 'milk-tea:drink:boba', 'milk-tea', 'drink',
                '波霸奶茶', '波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。', 'h',
                1, 'accepted', '0.1.0', 1, 'seed', 'none', 1, 1)",
        )
        .execute(&mut c)
        .unwrap();
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at)
             VALUES ('milk-tea:drink:demo', 'milk-tea:drink:demo', 'milk-tea', 'drink',
                '桂花乌龙', '桂花乌龙茶汤清澈。', 'h2',
                1, 'quarantined', '0.1.0', 1, 'seed', 'none', 1, 1)",
        )
        .execute(&mut c)
        .unwrap();
        // 旧语义触发器把 quarantined 页也放进 FTS（0001/0002 行为），升级后必须清除。
        // Old-semantics triggers indexed the quarantined page too (0001/0002
        // behavior); the upgrade must clear it.
        diesel::sql_query(
            "INSERT INTO compile_tasks (entity_id, source_revision, domain_pack_version,
                status, retry_count, max_retries, lease_expires_at, created_at, updated_at)
             VALUES ('milk-tea:drink:boba', 1, '0.1.0', 'running', 0, 3, 1, 1, 1)",
        )
        .execute(&mut c)
        .unwrap();

        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 3);

        // FTS 仅剩 accepted legacy 页（迁移清空 + accepted 回填）。
        // FTS keeps only the accepted legacy page (cleared then accepted backfill).
        assert_eq!(count(&mut c, "pages_fts"), 1);
        let fts_pages: Vec<SqlRow> = diesel::sql_query("SELECT page_id AS value FROM pages_fts")
            .load(&mut c)
            .unwrap();
        assert_eq!(fts_pages[0].value, "milk-tea:drink:boba");

        // legacy 未终结任务 → dead/result=failed + legacy 标记 + 清租约（§8.1）。
        // Legacy non-terminal task → dead/result=failed + legacy marker + cleared
        // lease (§8.1).
        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT status || '/' || COALESCE(result, '') || '/' || COALESCE(error_message, '') AS value
             FROM compile_tasks",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(rows[0].value, "dead/failed/legacy_task_missing_snapshot");

        // legacy generation 哨兵：generation=1 / __legacy__ / seed / published。
        // Legacy generation sentinel: generation=1 / __legacy__ / seed / published.
        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT generation || '/' || domain_pack || '/' || domain_pack_version || '/'
                || status AS value FROM generations",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(rows[0].value, "1/__legacy__/seed/published");

        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("wiktor_mig_legacy_{}_upgrade", std::process::id())),
        );
    }

    // A22：down 迁移 —— compile_attempts 非空时拒绝破坏性降级；空新表才允许回退，
    // 且回退后 schema 回到 0002 语义、再次 migrate 可达 3。
    // A22: down migration — non-empty compile_attempts refuses destructive
    // downgrade; empty new tables allow rollback, after which the schema is back to
    // 0002 semantics and re-migrating reaches 3 again.
    #[test]
    fn migration_0003_down_guarded_by_attempts() {
        use diesel_migrations::MigrationHarness;

        // 空 attempts：可回退（guard 表插入 0 不违反 CHECK）。
        // Empty attempts: revertible (guard inserts 0, CHECK holds).
        let mut c = conn();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 2);
        // 回退后旧语义触发器恢复：candidate 也入 FTS。
        // Old-semantics triggers restored: candidates are indexed again.
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at)
             VALUES ('milk-tea:drink:a', 'milk-tea:drink:a', 'milk-tea', 'drink',
                '波霸奶茶', '波霸奶茶', 'h', 1, 'candidate', '0.1.0', 1, 'seed', 'none', 1, 1)",
        )
        .execute(&mut c)
        .unwrap();
        assert_eq!(count(&mut c, "pages_fts"), 1);
        // 再升级回 0003：candidate 被清出 FTS。
        // Upgrade to 0003 again: the candidate is pushed out of FTS.
        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 3);
        assert_eq!(count(&mut c, "pages_fts"), 0);

        // 非空 attempts：guard CHECK 失败 → revert 报错，不静默丢审计（§8.1）。
        // Non-empty attempts: guard CHECK fails → revert errors, audit never
        // silently dropped (§8.1).
        diesel::sql_query("DELETE FROM pages")
            .execute(&mut c)
            .unwrap();
        diesel::sql_query(
            "INSERT INTO compile_tasks (entity_id, source_revision, domain_pack_version,
                status, retry_count, max_retries, created_at, updated_at,
                desired_hash, epoch, source_json, dependencies_json, snapshot_hash,
                next_attempt_at, task_token_budget)
             VALUES ('milk-tea:drink:boba', 1, '0.1.0', 'pending', 0, 3, 1, 1,
                'h', 1, '{}', '{}', 's', 0, 65536)",
        )
        .execute(&mut c)
        .unwrap();
        let task_id: i64 = diesel::sql_query("SELECT task_id AS n FROM compile_tasks")
            .get_result::<CountRow>(&mut c)
            .unwrap()
            .n;
        diesel::sql_query(
            "INSERT INTO compile_runs (run_id, token_limit, reserved_tokens, created_at)
             VALUES ('run-1', 1000, 0, 1)",
        )
        .execute(&mut c)
        .unwrap();
        diesel::sql_query(
            "INSERT INTO compile_attempts (task_id, epoch, attempt_no, lease_token, status,
                run_id, utc_day, issues_json, reserved_tokens, created_at)
             VALUES (?, 1, 1, 'tok', 'completed', 'run-1', 0, '[]', 10, 1)",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .execute(&mut c)
        .unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 3);
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
