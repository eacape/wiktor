-- 迁移 0005 回退：仅在无 Step6 反馈/审核数据时允许（spec §5 down 契约：先检查
-- 不存在 Step6 数据再删除新结构，避免破坏性丢失审计）。
-- Migration 0005 downgrade: allowed only with no Step6 feedback/review data
-- (spec §5 down contract: verify no Step6 data exists before dropping the new
-- structures, so audit records are never destructively lost).
--
-- RAISE(ABORT) 只能用于触发器体内，这里用带 CHECK 的临时表守卫等价实现（对齐
-- 0003/0004 down 的做法）：feedback_events 或 review_queue 任一非空 → 插入
-- 违反 CHECK → 整个迁移以约束冲突中止（不静默丢反馈/审核审计）。
-- IF NOT EXISTS + DELETE 保证同连接重复执行回退时守卫可重建。
-- RAISE(ABORT) is only legal inside trigger bodies, so a CHECK-guarded temp
-- table is the equivalent (same approach as the 0003/0004 down): a non-empty
-- feedback_events or review_queue violates the CHECK and aborts the whole
-- migration with a constraint error (feedback/review audit records are never
-- silently dropped). IF NOT EXISTS + DELETE keep the guard reusable across
-- repeated downgrades on one connection.
CREATE TEMP TABLE IF NOT EXISTS downgrade_guard (guard INTEGER NOT NULL CHECK (guard = 0));
DELETE FROM downgrade_guard;
INSERT INTO downgrade_guard (guard) SELECT COUNT(*) FROM feedback_events;
INSERT INTO downgrade_guard (guard) SELECT COUNT(*) FROM review_queue;

-- 删除 0005 新增表（三表之间无互相外键；review_queue→compile_tasks 与
-- feedback_events→query_logs 均指向保留父表，先删子表即 FK 安全）。
-- Drop the 0005 tables (no FKs among the three; review_queue→compile_tasks and
-- feedback_events→query_logs both reference kept parent tables, so dropping
-- children first is FK-safe).
DROP TABLE IF EXISTS feedback_rejections;
DROP TABLE IF EXISTS review_queue;
DROP TABLE IF EXISTS feedback_events;

-- 回退 query_logs 新增列（先删依赖 domain 的新索引再删列）。
-- Revert the query_logs columns (the index depending on `domain` goes first).
DROP INDEX IF EXISTS idx_query_logs_domain_time;
ALTER TABLE query_logs DROP COLUMN domain;
ALTER TABLE query_logs DROP COLUMN candidate_empty_initial;
ALTER TABLE query_logs DROP COLUMN relaxation_attempted;
ALTER TABLE query_logs DROP COLUMN relaxation_succeeded;

-- 守卫表用后即弃（连接可重复执行回退）。
-- Drop the guard table afterwards (so downgrades can run repeatedly).
DROP TABLE IF EXISTS temp.downgrade_guard;
