-- 迁移 0004 回退：仅在无 Step5 QUG 构建数据时允许（spec §5 down 契约：
-- 先检查不存在 Step5 数据再删除新结构，避免破坏性丢失）。
-- Migration 0004 downgrade: allowed only with no Step5 QUG build data (spec §5
-- down contract: verify no Step5 data exists before dropping the new structures,
-- so nothing is destructively lost).
--
-- RAISE(ABORT) 只能用于触发器体内，这里用带 CHECK 的临时表守卫等价实现（对齐
-- 0003 down 的做法）：qug_builds 非空 → 插入违反 CHECK → 整个迁移以约束冲突
-- 中止（不静默丢构建代次）。IF NOT EXISTS + DELETE 保证同连接重复执行时守卫可重建。
-- RAISE(ABORT) is only legal inside trigger bodies, so a CHECK-guarded temp table
-- is the equivalent (same approach as the 0003 down): a non-empty qug_builds
-- violates the CHECK and aborts the whole migration with a constraint error
-- (build generations are never silently dropped). IF NOT EXISTS + DELETE keep
-- the guard reusable across repeated downgrades on one connection.
CREATE TEMP TABLE IF NOT EXISTS downgrade_guard (guard INTEGER NOT NULL CHECK (guard = 0));
DELETE FROM downgrade_guard;
INSERT INTO downgrade_guard (guard) SELECT COUNT(*) FROM qug_builds;

-- 删除 0004 新增结构（先子后父，FK 顺序安全）。
-- Drop the 0004 structures (children before parents for FK safety).
DROP INDEX IF EXISTS idx_qug_intent_edges_build;
DROP TABLE IF EXISTS qug_intent_edges;
DROP TABLE IF EXISTS qug_page_snapshots;

-- 回退 qug_edges 的 build_id 列（先删依赖它的索引；守卫通过 ⇒ 无 build_id
-- 非空行——镜像行全部挂在 qug_builds 行上，随之已不存在）。
-- Revert the qug_edges build_id column (its index goes first; a passed guard ⇒
-- no non-NULL build_id rows can exist — mirror rows hang off qug_builds rows
-- and are gone with them).
DROP INDEX IF EXISTS idx_qug_edges_build;
ALTER TABLE qug_edges DROP COLUMN build_id;

-- 最后删除代次父表（守卫保证为空；FK 由子表先行删除保障）。
-- Finally drop the generation parent (empty per the guard; children were
-- dropped first for FK safety).
DROP TABLE IF EXISTS qug_builds;

-- 守卫表用后即弃（连接可重复执行回退）。
-- Drop the guard table afterwards (so downgrades can run repeatedly).
DROP TABLE IF EXISTS temp.downgrade_guard;
