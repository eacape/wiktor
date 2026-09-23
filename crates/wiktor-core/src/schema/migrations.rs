use crate::types::error::{Error, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

/// 嵌入的 SQLite 迁移（0001 两平面 + FTS5 基础，0002 FTS trigram，0003 编译管线，
/// 0004 QUG 持久化，0005 反馈闭环，0006 Step8 一致性/死信审核/兼容检查）。
/// Embedded SQLite migrations (0001: two planes + FTS5 basics; 0002: FTS trigram;
/// 0003: compile pipeline; 0004: QUG persistence; 0005: feedback loop; 0006:
/// Step8 consistency/dead-letter review/compatibility checks).
///
/// 迁移文件位于 `crates/wiktor-core/migrations/<version>_<name>/up.sql`，
/// 由 diesel 在单个事务内应用；版本跟踪表 `__diesel_schema_migrations`。
/// Migration files live in `crates/wiktor-core/migrations/<version>_<name>/up.sql`
/// and are applied by diesel within a single transaction; the version tracking
/// table is `__diesel_schema_migrations`.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// spec step6 §9：server 与 CLI 可同时打开同一 SQLite——连接级 busy timeout
/// 5 秒：写竞争时等待而非立即 SQLITE_BUSY，超时后错误显式上抛（store 层无
/// 重试逻辑，不会靠重试绕过幂等）。该 PRAGMA 只作用于当前连接、不落库，
/// 因此对 open_existing 的 dry-run「不写库」语义安全；落库类 pragma
/// （journal_mode）仍只在 [`migrate`] 中设置。
/// spec step6 §9: server and CLI may open the same SQLite concurrently — a
/// connection-local busy timeout of 5 seconds: write contention waits instead
/// of failing immediately with SQLITE_BUSY, and past the timeout the error
/// surfaces explicitly (stores carry no retry logic, so idempotency can never
/// be bypassed by retrying). The PRAGMA only affects the current connection
/// and is never persisted, so it is safe for open_existing's dry-run "never
/// writes" semantics; DB-persisting pragmas (journal_mode) remain exclusive to
/// [`migrate`].
pub(crate) const BUSY_TIMEOUT_PRAGMA_SQL: &str = "PRAGMA busy_timeout=5000;";

/// 打开连接后立即调用：设置 pragma + 应用未跑过的迁移，幂等。
/// Call right after opening a connection: sets pragmas + applies pending
/// migrations; idempotent.
pub fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    conn.batch_execute(&format!(
        // WAL + 外键强制（内存库 journal_mode 恒为 "memory"，pragma 静默忽略）+
        // busy timeout 5s（spec step6 §9，见 BUSY_TIMEOUT_PRAGMA_SQL 注释）。
        // WAL + enforced foreign keys (in-memory DBs keep journal_mode "memory";
        // the pragma is silently ignored) + the 5s busy timeout (spec step6 §9,
        // see the BUSY_TIMEOUT_PRAGMA_SQL doc comment).
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; {BUSY_TIMEOUT_PRAGMA_SQL}"
    ))?;
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
        // A22/A2：0006 后 schema 版本为 6（Step8 A1）。
        // A22/A2: schema version is 6 after 0006 (Step8 A1).
        assert_eq!(schema_version(&mut c).unwrap(), 6);
        // 重跑无副作用
        // Re-running is side-effect free
        migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);
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
        assert_eq!(schema_version(&mut c).unwrap(), 6);

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

    // A22：down 迁移链 —— Step8/反馈/qug/compile_attempts 数据非空时拒绝破坏性
    // 降级；空新表才允许回退，且回退后 schema 回到 0002 语义、再次 migrate 可达 6。
    // A22: the down-migration chain — non-empty feedback_events/qug_builds/
    // compile_attempts refuse destructive downgrades; empty new tables allow
    // rollback, after which the schema is back to 0002 semantics and
    // re-migrating reaches 5 again.
    #[test]
    fn migration_0003_down_guarded_by_attempts() {
        use diesel_migrations::MigrationHarness;

        // 空 Step6/Step8/legacy 审计数据：可回退（guard 表插入 0 不违反 CHECK）。
        // 先回退 0006（Step8 数据空）→ 5，再回退 0005（反馈表空）→ 4，再回退
        // 0004（qug_builds 空）→ 3，最后回退 0003（attempts 空）→ 2。
        // Empty Step6/Step8/legacy audit data: revertible (the guard inserts 0,
        // CHECK holds). First revert 0006 (empty Step8 data) → 5, then 0005
        // (empty feedback tables) → 4, then 0004 (empty qug_builds) → 3,
        // finally 0003 (empty attempts) → 2.
        let mut c = conn();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 5);
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 4);
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 3);
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
        // 再升级回 0006：candidate 被清出 FTS。
        // Upgrade to 0006 again: the candidate is pushed out of FTS.
        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);
        assert_eq!(count(&mut c, "pages_fts"), 0);

        // 非空 attempts：guard CHECK 失败 → revert 报错，不静默丢审计（§8.1）。
        // revert_last_migration 先回退 0006（Step8 数据空 → 成功）→ 5，再回退
        // 0005（反馈表空 → 成功）→ 4，再回退 0004（qug_builds 空 → 成功）→ 3，
        // 第四次 revert 触发 0003 守卫失败，版本停在 3。
        // Non-empty attempts: the guard CHECK fails → revert errors, audit never
        // silently dropped (§8.1). revert_last_migration first reverts 0006
        // (empty Step8 data → OK) → 5, then 0005 (empty feedback tables → OK)
        // → 4, then 0004 (empty qug_builds → OK) → 3; the fourth revert trips
        // the 0003 guard and the version stays 3.
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
        c.revert_last_migration(MIGRATIONS).unwrap();
        c.revert_last_migration(MIGRATIONS).unwrap();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 3);
    }

    // A5/A6：0004 down —— qug_builds 非空时拒绝回退（不丢 QUG 构建代次）；清空
    // 后回退成功，且 0004 新结构（表 + build_id 列）一并移除。
    // A5/A6: the 0004 down — a non-empty qug_builds refuses the downgrade (QUG
    // build generations are never dropped); once cleared, the downgrade succeeds
    // and the 0004 structures (tables + build_id column) go away with it.
    #[test]
    fn migration_0004_down_guarded_by_builds() {
        use diesel_migrations::MigrationHarness;

        let mut c = conn();
        // 先回退 0006（Step8 数据空 → 成功）到 5，再回退 0005（反馈表空 → 成功）
        // 到 4，使 revert_last_migration 指向 0004。
        // First revert 0006 (empty Step8 data → OK) down to 5, then revert 0005
        // (empty feedback tables → OK) down to 4 so revert_last_migration
        // targets 0004.
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 5);
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 4);
        // building 行不占 active published 唯一索引，可独立插入。
        // A building row does not occupy the active-published partial unique
        // index and can be inserted standalone.
        diesel::sql_query(
            "INSERT INTO qug_builds
                (domain_name, domain_version, builder_version, source_hash, status,
                 page_count, edge_count, counts_json, created_at)
             VALUES ('milk-tea', '0.1.0', 'qug-build-v1', 'deadbeef', 'building', 0, 0, '{}', 1)",
        )
        .execute(&mut c)
        .unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 4);

        // 清空后回退：qug_builds/qug_page_snapshots/qug_intent_edges 消失，
        // qug_edges 回到 0003 形状（无 build_id 列）。
        // Once cleared: qug_builds/qug_page_snapshots/qug_intent_edges vanish and
        // qug_edges is back to the 0003 shape (no build_id column).
        diesel::sql_query("DELETE FROM qug_builds")
            .execute(&mut c)
            .unwrap();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 3);
        let tables: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'table'
             AND name IN ('qug_builds', 'qug_page_snapshots', 'qug_intent_edges')",
        )
        .load(&mut c)
        .unwrap();
        assert!(
            tables.is_empty(),
            "0004 tables must be dropped, {} remain",
            tables.len()
        );
        let cols: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM pragma_table_info('qug_edges') WHERE name = 'build_id'",
        )
        .load(&mut c)
        .unwrap();
        assert!(cols.is_empty(), "qug_edges.build_id must be dropped");
    }

    /// 搭一个 0001..0004 的库（含版本行），供 0005 真实升级路径测试。
    /// Builds a 0001..0004 database (with version rows) for the real 0005
    /// upgrade-path test.
    fn conn_at_0004(dir_name: &str) -> SqliteConnection {
        use diesel::connection::SimpleConnection;
        let dir =
            std::env::temp_dir().join(format!("wiktor_mig_v4_{}_{}", std::process::id(), dir_name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v4.db");
        let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        c.batch_execute(include_str!("../../migrations/0001_create_core/up.sql"))
            .unwrap();
        c.batch_execute(include_str!("../../migrations/0002_fts_trigram/up.sql"))
            .unwrap();
        c.batch_execute(include_str!(
            "../../migrations/0003_compile_pipeline/up.sql"
        ))
        .unwrap();
        c.batch_execute(include_str!("../../migrations/0004_qug_persistence/up.sql"))
            .unwrap();
        c.batch_execute(
            "CREATE TABLE __diesel_schema_migrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                version TEXT NOT NULL,
                run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        // 版本行从全新 migrate 的库里取（按版本序前 4 行即 0001..0004）。
        // Version rows come from a freshly migrated DB (the first 4 in version
        // order are 0001..0004).
        let mut fresh = conn();
        let versions: Vec<SqlRow> = diesel::sql_query(
            "SELECT version AS value FROM __diesel_schema_migrations ORDER BY version",
        )
        .load(&mut fresh)
        .unwrap();
        for v in &versions[..4] {
            diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES (?)")
                .bind::<diesel::sql_types::Text, _>(&v.value)
                .execute(&mut c)
                .unwrap();
        }
        c
    }

    // A2：0005 在已有 0004 数据的库上成功迁移——旧 query log 可读，新列默认值
    // 确定（domain='__legacy__'、三状态列 0，STEP6-002）。
    // A2: 0005 migrates successfully over a database with existing 0004 data —
    // old query logs stay readable and the new column defaults are deterministic
    // (domain='__legacy__', the three state columns 0, STEP6-002).
    #[test]
    fn migration_0005_upgrade_keeps_legacy_logs() {
        let mut c = conn_at_0004("s6upgrade");
        // 0004 形状（7 列）的 query_logs 行。
        // A query_logs row in the 0004 shape (7 columns).
        diesel::sql_query(
            "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure,
                    hit_count, latency_ms, timestamp)
             VALUES ('波霸奶茶', '{}', NULL, 0, 3, 12, 1000)",
        )
        .execute(&mut c)
        .unwrap();

        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);

        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT domain || '/' || candidate_empty_initial || '/' ||
                    relaxation_attempted || '/' || relaxation_succeeded ||
                    '/' || CAST(hit_count AS TEXT) AS value
             FROM query_logs",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(
            rows[0].value, "__legacy__/0/0/0/3",
            "legacy log keeps its data and gains deterministic 0005 defaults"
        );
        // 新索引就位。
        // The new index is in place.
        let idx: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'index'
             AND name = 'idx_query_logs_domain_time'",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(idx.len(), 1);
    }

    // A2/A3：0005 down —— feedback_events/review_queue 非空时拒绝回退（不丢反馈
    // /审核审计）；清空后回退成功，0005 新表与 query_logs 新列一并移除，再迁移
    // 可达 6。
    // A2/A3: the 0005 down — non-empty feedback_events/review_queue refuse the
    // downgrade (feedback/review audit is never dropped); once cleared, the
    // downgrade succeeds, the 0005 tables and the query_logs columns go away,
    // and re-migrating reaches 6 again.
    #[test]
    fn migration_0005_down_guarded_by_feedback() {
        use diesel_migrations::MigrationHarness;

        let mut c = conn();
        // 先回退 0006（Step8 数据空 → 成功）到 5，使 revert_last_migration 指向 0005。
        // First revert 0006 (empty Step8 data → OK) down to 5 so
        // revert_last_migration targets 0005.
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 5);
        // 塞一条合法反馈事件（先有 query_logs 行供 FK）。
        // Seed one valid feedback event (a query_logs row first, for the FK).
        diesel::sql_query(
            "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure,
                    hit_count, latency_ms, timestamp, domain)
             VALUES ('波霸奶茶', '{}', NULL, 0, 3, 12, 1000, 'milk-tea')",
        )
        .execute(&mut c)
        .unwrap();
        let log_id: i64 = diesel::sql_query("SELECT log_id AS n FROM query_logs")
            .get_result::<CountRow>(&mut c)
            .unwrap()
            .n;
        diesel::sql_query(
            "INSERT INTO feedback_events (idempotency_key, domain, log_id, kind, page_id,
                    rating, metadata_json, received_at)
             VALUES ('k1', 'milk-tea', ?, 'click', 'milk-tea:drink:boba', NULL, '{}', 5)",
        )
        .bind::<diesel::sql_types::BigInt, _>(log_id)
        .execute(&mut c)
        .unwrap();

        // feedback_events 非空 → 0005 守卫拒绝。
        // Non-empty feedback_events → the 0005 guard refuses.
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 5);

        // 清空后回退：三张新表消失、query_logs 回到 0004 形状。
        // Once cleared: the three new tables vanish and query_logs is back to
        // the 0004 shape.
        diesel::sql_query("DELETE FROM feedback_events")
            .execute(&mut c)
            .unwrap();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 4);
        let tables: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'table'
             AND name IN ('feedback_events', 'review_queue', 'feedback_rejections')",
        )
        .load(&mut c)
        .unwrap();
        assert!(
            tables.is_empty(),
            "0005 tables must be dropped, {} remain",
            tables.len()
        );
        let cols: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM pragma_table_info('query_logs')
             WHERE name IN ('domain', 'candidate_empty_initial', 'relaxation_attempted',
                            'relaxation_succeeded')",
        )
        .load(&mut c)
        .unwrap();
        assert!(cols.is_empty(), "0005 query_logs columns must be dropped");

        // 再迁移回 6（up/down 幂等往返）。
        // Re-migrate to 6 again (the up/down round trip is idempotent).
        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);
    }

    /// 搭一个 0001..0005 的库（含版本行），供 0006 真实升级路径测试。
    /// Builds a 0001..0005 database (with version rows) for the real 0006
    /// upgrade-path test.
    fn conn_at_0005(dir_name: &str) -> SqliteConnection {
        use diesel::connection::SimpleConnection;
        let dir =
            std::env::temp_dir().join(format!("wiktor_mig_v5_{}_{}", std::process::id(), dir_name));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v5.db");
        let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        c.batch_execute(include_str!("../../migrations/0001_create_core/up.sql"))
            .unwrap();
        c.batch_execute(include_str!("../../migrations/0002_fts_trigram/up.sql"))
            .unwrap();
        c.batch_execute(include_str!(
            "../../migrations/0003_compile_pipeline/up.sql"
        ))
        .unwrap();
        c.batch_execute(include_str!("../../migrations/0004_qug_persistence/up.sql"))
            .unwrap();
        c.batch_execute(include_str!("../../migrations/0005_feedback_loop/up.sql"))
            .unwrap();
        c.batch_execute(
            "CREATE TABLE __diesel_schema_migrations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                version TEXT NOT NULL,
                run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .unwrap();
        // 版本行从全新 migrate 的库里取（按版本序前 5 行即 0001..0005）。
        // Version rows come from a freshly migrated DB (the first 5 in version
        // order are 0001..0005).
        let mut fresh = conn();
        let versions: Vec<SqlRow> = diesel::sql_query(
            "SELECT version AS value FROM __diesel_schema_migrations ORDER BY version",
        )
        .load(&mut fresh)
        .unwrap();
        for v in &versions[..5] {
            diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES (?)")
                .bind::<diesel::sql_types::Text, _>(&v.value)
                .execute(&mut c)
                .unwrap();
        }
        c
    }

    // A1（Step8）：0001–0005 数据库升级到 6 —— 既有三种 review action 行
    // （含 compile_task_id FK）原样保留，0006 新列取确定默认
    // （'unchecked' / '{}'），四个索引就位，放宽后的 CHECK 接受新动作。
    // A1 (Step8): a 0001–0005 database upgrades to 6 — existing rows with the
    // three legacy review actions (including the compile_task_id FK) are kept
    // verbatim, the 0006 columns take deterministic defaults ('unchecked' /
    // '{}'), the four indexes exist, and the widened CHECK accepts the new
    // actions.
    #[test]
    fn migration_0006_upgrade_keeps_legacy_reviews() {
        let mut c = conn_at_0005("s8upgrade");
        diesel::sql_query(
            "INSERT INTO compile_tasks (entity_id, source_revision, domain_pack_version,
                status, retry_count, max_retries, created_at, updated_at,
                desired_hash, epoch, source_json, dependencies_json, snapshot_hash,
                next_attempt_at, task_token_budget)
             VALUES ('milk-tea:drink:boba', 1, '0.1.0', 'dead', 0, 3, 1, 1,
                'h', 1, '{}', '{}', 's', 0, 65536)",
        )
        .execute(&mut c)
        .unwrap();
        let task_id: i64 = diesel::sql_query("SELECT task_id AS n FROM compile_tasks")
            .get_result::<CountRow>(&mut c)
            .unwrap()
            .n;
        // 0005 形状的 review_queue 行：legacy 三动作之一 + compile_task_id 回填。
        // A 0005-shape review_queue row: a legacy action plus a filled
        // compile_task_id.
        diesel::sql_query(
            "INSERT INTO review_queue (domain, action, status, source_log_ids_json,
                    subject_json, reason_json, created_at, compile_task_id)
             VALUES ('milk-tea', 'query_template', 'pending', '[]', '{\"id\":1}',
                     '{\"note\":\"legacy\"}', 10, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .execute(&mut c)
        .unwrap();

        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);

        // 既有审核行原样保留（review_id/action/subject/FK 全部不变）。
        // The existing review row is preserved verbatim (review_id/action/
        // subject/FK unchanged).
        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT action || '/' || status || '/' || subject_json || '/' ||
                    CAST(compile_task_id AS TEXT) AS value FROM review_queue",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(
            rows[0].value,
            format!("query_template/pending/{{\"id\":1}}/{task_id}"),
            "legacy review rows survive the rebuild"
        );

        // 0006 新列取确定默认。
        // The 0006 columns take deterministic defaults.
        let rows: Vec<SqlRow> = diesel::sql_query(
            "SELECT consistency_status || '/' || compatibility_status AS value
             FROM compile_tasks",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(rows[0].value, "unchecked/unchecked");

        // 四个索引就位（含部分索引 idx_review_task 与重建的 idx_review_status）。
        // The four indexes exist (including the partial idx_review_task and the
        // rebuilt idx_review_status).
        let idx: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'index'
             AND name IN ('idx_compile_tasks_dead_review','idx_compile_tasks_preflight',
                          'idx_review_task','idx_review_status')
             ORDER BY name",
        )
        .load(&mut c)
        .unwrap();
        assert_eq!(idx.len(), 4, "0006 indexes must exist");

        // 放宽后的 CHECK 接受三个新动作。
        // The widened CHECK accepts the three new actions.
        diesel::sql_query(
            "INSERT INTO review_queue (domain, action, status, source_log_ids_json,
                    subject_json, reason_json, created_at, compile_task_id)
             VALUES ('milk-tea', 'compile_dead_letter', 'pending', '[]',
                     '{\"task_id\":1}', '{}', 20, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .execute(&mut c)
        .unwrap();

        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("wiktor_mig_v5_{}_s8upgrade", std::process::id())),
        );
    }

    // A2（Step8/D12）：0006 down 守卫 —— 任一 Step 8 审计非空（新动作审核行、
    // 非默认一致性/兼容状态、非空诊断 JSON）即拒绝回退且行不变；全部清空后
    // 恢复 0005 三动作 CHECK 与索引/列，再迁移可达 6。
    // A2 (Step8/D12): the 0006 down guard — any non-empty Step 8 audit (a new
    // action review row, a non-default consistency/compatibility status, a
    // non-empty diagnostic JSON) refuses the downgrade with rows untouched;
    // once everything is cleared the 0005 three-action CHECK and the indexes/
    // columns are restored, and re-migrating reaches 6.
    #[test]
    fn migration_0006_down_guarded_by_step8_data() {
        use diesel_migrations::MigrationHarness;

        let mut c = conn();

        // 守卫 1：Step 8 新动作审核行非空 → 拒绝回退，版本停在 6。
        // Guard 1: a non-empty Step 8 action review row refuses the downgrade;
        // the version stays 6.
        diesel::sql_query(
            "INSERT INTO compile_tasks (entity_id, source_revision, domain_pack_version,
                status, retry_count, max_retries, created_at, updated_at,
                desired_hash, epoch, source_json, dependencies_json, snapshot_hash,
                next_attempt_at, task_token_budget)
             VALUES ('milk-tea:drink:boba', 1, '0.1.0', 'dead', 0, 3, 1, 1,
                'h', 1, '{}', '{}', 's', 0, 65536)",
        )
        .execute(&mut c)
        .unwrap();
        let task_id: i64 = diesel::sql_query("SELECT task_id AS n FROM compile_tasks")
            .get_result::<CountRow>(&mut c)
            .unwrap()
            .n;
        diesel::sql_query(
            "INSERT INTO review_queue (domain, action, status, source_log_ids_json,
                    subject_json, reason_json, created_at, compile_task_id)
             VALUES ('milk-tea', 'compile_dead_letter', 'pending', '[]',
                     '{\"task_id\":1}', '{}', 20, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .execute(&mut c)
        .unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 6);

        // 守卫 2：consistency_status 非默认 → 拒绝回退。
        // Guard 2: a non-default consistency_status refuses the downgrade.
        diesel::sql_query("DELETE FROM review_queue")
            .execute(&mut c)
            .unwrap();
        diesel::sql_query("UPDATE compile_tasks SET consistency_status = 'conflict'")
            .execute(&mut c)
            .unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 6);

        // 守卫 3：compile_attempts 诊断 JSON 非空 → 拒绝回退。
        // Guard 3: a non-empty compile_attempts diagnostic JSON refuses the
        // downgrade.
        diesel::sql_query("UPDATE compile_tasks SET consistency_status = 'unchecked'")
            .execute(&mut c)
            .unwrap();
        diesel::sql_query(
            "INSERT INTO compile_runs (run_id, token_limit, reserved_tokens, created_at)
             VALUES ('run-1', 1000, 0, 1)",
        )
        .execute(&mut c)
        .unwrap();
        diesel::sql_query(
            "INSERT INTO compile_attempts (task_id, epoch, attempt_no, lease_token, status,
                run_id, utc_day, issues_json, reserved_tokens, created_at,
                consistency_json)
             VALUES (?, 1, 1, 'tok', 'completed', 'run-1', 0, '[]', 10, 1,
                     '{\"code\":\"VALUE_DIVERGENCE\"}')",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .execute(&mut c)
        .unwrap();
        assert!(c.revert_last_migration(MIGRATIONS).is_err());
        assert_eq!(schema_version(&mut c).unwrap(), 6);

        // 全部清空 → 回退成功：0006 新列消失，review_queue 恢复三动作 CHECK
        // （新动作被拒、旧动作可用），0006 索引消失，再迁移可达 6。
        // Once everything is cleared → the downgrade succeeds: the 0006 columns
        // vanish, review_queue is back to the three-action CHECK (new actions
        // rejected, legacy actions fine), the 0006 indexes are gone, and
        // re-migrating reaches 6.
        diesel::sql_query("DELETE FROM compile_attempts")
            .execute(&mut c)
            .unwrap();
        diesel::sql_query("DELETE FROM compile_tasks")
            .execute(&mut c)
            .unwrap();
        c.revert_last_migration(MIGRATIONS).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 5);
        let cols: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM pragma_table_info('compile_tasks')
             WHERE name IN ('consistency_status','compatibility_status')
             UNION ALL
             SELECT name FROM pragma_table_info('compile_attempts')
             WHERE name IN ('consistency_json','compatibility_json')",
        )
        .load(&mut c)
        .unwrap();
        assert!(cols.is_empty(), "0006 columns must be dropped");
        let idx: Vec<SqlRow> = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'index'
             AND name IN ('idx_compile_tasks_dead_review','idx_compile_tasks_preflight',
                          'idx_review_task')",
        )
        .load(&mut c)
        .unwrap();
        assert!(idx.is_empty(), "0006 indexes must be dropped");
        // 0005 三动作 CHECK 恢复：compile_dead_letter 被拒、旧动作可用。
        // The 0005 three-action CHECK is restored: compile_dead_letter is
        // rejected while legacy actions insert fine.
        assert!(diesel::sql_query(
            "INSERT INTO review_queue (domain, action, status, source_log_ids_json,
                    subject_json, reason_json, created_at)
             VALUES ('milk-tea', 'compile_dead_letter', 'pending', '[]', '{}', '{}', 1)"
        )
        .execute(&mut c)
        .is_err());
        diesel::sql_query(
            "INSERT INTO review_queue (domain, action, status, source_log_ids_json,
                    subject_json, reason_json, created_at)
             VALUES ('milk-tea', 'query_template', 'pending', '[]', '{}', '{}', 1)",
        )
        .execute(&mut c)
        .unwrap();

        crate::schema::migrate(&mut c).unwrap();
        assert_eq!(schema_version(&mut c).unwrap(), 6);
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
