-- 迁移 0003 回退：仅在无 Step4 attempts 审计记录时允许（§8.1 down 契约）。
-- Migration 0003 downgrade: only allowed with no Step4 attempt audit records
-- (§8.1 down contract).
--
-- RAISE(ABORT) 只能用于触发器体内，这里用带 CHECK 的临时表守卫等价实现：
-- compile_attempts 非空 → 插入违反 CHECK → 整个迁移以约束冲突中止（不静默丢审计）。
-- IF NOT EXISTS + DELETE 保证同连接重复执行回退时守卫表可重建。
-- RAISE(ABORT) is only legal inside trigger bodies, so a CHECK-guarded temp table
-- is the equivalent: a non-empty compile_attempts violates the CHECK and aborts
-- the whole migration with a constraint error (audit records are never silently
-- dropped). IF NOT EXISTS + DELETE keep the guard reusable across repeated
-- downgrades on one connection.
CREATE TEMP TABLE IF NOT EXISTS downgrade_guard (guard INTEGER NOT NULL CHECK (guard = 0));
DELETE FROM downgrade_guard;
INSERT INTO downgrade_guard (guard) SELECT COUNT(*) FROM compile_attempts;

-- 删除 0003 新增表（先子后父，FK 顺序安全）。
-- Drop the 0003 tables (children before parents for FK safety).
DROP TABLE IF EXISTS compile_attempts;
DROP TABLE IF EXISTS compile_source_heads;
DROP TABLE IF EXISTS compile_runs;
DROP TABLE IF EXISTS compile_daily_budget;
DROP TABLE IF EXISTS qug_edges;

-- 回退 compile_tasks 新增列（先删依赖这些列的索引）。
-- Revert the compile_tasks columns (index depending on them goes first).
DROP INDEX IF EXISTS idx_tasks_pending_schedule;
ALTER TABLE compile_tasks DROP COLUMN desired_hash;
ALTER TABLE compile_tasks DROP COLUMN epoch;
ALTER TABLE compile_tasks DROP COLUMN source_json;
ALTER TABLE compile_tasks DROP COLUMN dependencies_json;
ALTER TABLE compile_tasks DROP COLUMN snapshot_hash;
ALTER TABLE compile_tasks DROP COLUMN recompile_count;
ALTER TABLE compile_tasks DROP COLUMN attempt_count;
ALTER TABLE compile_tasks DROP COLUMN lease_token;
ALTER TABLE compile_tasks DROP COLUMN next_attempt_at;
ALTER TABLE compile_tasks DROP COLUMN result;
ALTER TABLE compile_tasks DROP COLUMN reserved_tokens;
ALTER TABLE compile_tasks DROP COLUMN task_token_budget;

-- 回退 pages 新增列。
-- Revert the pages columns.
ALTER TABLE pages DROP COLUMN source_revision;
ALTER TABLE pages DROP COLUMN artifact_version;
ALTER TABLE pages DROP COLUMN frontmatter_json;

-- 删除 0003 的 legacy 哨兵。
-- Drop the 0003 legacy sentinel.
DELETE FROM generations WHERE domain_pack = '__legacy__';

-- 恢复 0002 的 FTS 触发器（全部状态入索引；SQL 原样抄回）。
-- Restore the 0002 FTS triggers (all statuses indexed; SQL copied verbatim).
DROP TRIGGER IF EXISTS pages_fts_insert;
DROP TRIGGER IF EXISTS pages_fts_update;
DROP TRIGGER IF EXISTS pages_fts_delete;

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

-- 恢复 0002 的全量回填语义（0003 清空过 FTS 且只回填了 accepted）。
-- Restore 0002's full backfill semantics (0003 cleared FTS and backfilled
-- accepted pages only).
DELETE FROM pages_fts;
INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT page_id, entity_id, title, content FROM pages;

-- 守卫表用后即弃（连接可重复执行回退）。
-- Drop the guard table afterwards (so downgrades can run repeatedly).
DROP TABLE IF EXISTS temp.downgrade_guard;
