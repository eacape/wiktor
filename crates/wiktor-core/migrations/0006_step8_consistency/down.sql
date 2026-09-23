-- 迁移 0006 回退：仅在无 Step 8 审计数据时允许（Step 8 spec §5.3 down 守卫、
-- 决策 D12：既有 Step 6/4 审核事实不得被静默删除，Step 8 新动作数据非空即拒绝）。
-- Migration 0006 downgrade: allowed only with no Step 8 audit data (the Step 8
-- spec §5.3 down guard, decision D12: existing Step 6/4 review facts are never
-- silently dropped; any non-empty Step 8 action data refuses the downgrade).
--
-- 三重守卫（任一非零 → 插入违反 CHECK → 整个迁移以约束冲突中止）：
-- 1. review_queue 中 action 属于三个 Step 8 新增动作的行数为 0；
-- 2. compile_tasks 中 consistency_status / compatibility_status 非 'unchecked'
--    的行数为 0；
-- 3. compile_attempts 中 consistency_json / compatibility_json 非 '{}' 的行数
--    为 0。
-- 守卫通过后：删除 Step 8 索引与列，重建 review_queue 恢复 0005 的三动作
-- CHECK 与既有唯一约束（既有行/审核生命周期原样带回，不丢 0005 审计）。
-- Triple guard (any non-zero count → a CHECK-violating insert aborts the whole
-- migration with a constraint error):
-- 1. zero rows in review_queue whose action is one of the three new Step 8
--    actions;
-- 2. zero rows in compile_tasks with consistency_status / compatibility_status
--    other than 'unchecked';
-- 3. zero rows in compile_attempts with consistency_json / compatibility_json
--    other than '{}'.
-- Once the guard passes: the Step 8 indexes and columns are dropped and
-- review_queue is rebuilt restoring the 0005 three-action CHECK and the
-- existing UNIQUE constraint (existing rows and the review lifecycle carry over
-- verbatim; 0005 audit is never dropped).
--
-- RAISE(ABORT) 只能用于触发器体内，这里用带 CHECK 的临时表守卫等价实现（对齐
-- 0003/0004/0005 down 的做法）。IF NOT EXISTS + DELETE 保证同连接重复执行回退
-- 时守卫可重建。
-- RAISE(ABORT) is only legal inside trigger bodies, so a CHECK-guarded temp
-- table is the equivalent (same approach as the 0003/0004/0005 down).
-- IF NOT EXISTS + DELETE keep the guard reusable across repeated downgrades on
-- one connection.
CREATE TEMP TABLE IF NOT EXISTS downgrade_guard (guard INTEGER NOT NULL CHECK (guard = 0));
DELETE FROM downgrade_guard;
INSERT INTO downgrade_guard (guard)
  SELECT COUNT(*) FROM review_queue
  WHERE action IN ('compile_dead_letter','consistency_conflict','compatibility_conflict');
INSERT INTO downgrade_guard (guard)
  SELECT COUNT(*) FROM compile_tasks
  WHERE consistency_status <> 'unchecked' OR compatibility_status <> 'unchecked';
INSERT INTO downgrade_guard (guard)
  SELECT COUNT(*) FROM compile_attempts
  WHERE consistency_json <> '{}' OR compatibility_json <> '{}';

-- 删除 Step 8 新增索引（review_queue 侧索引随表重建处理；compile_tasks 侧
-- 索引在删列前先行删除）。
-- Drop the Step 8 indexes (the review_queue-side ones go with the table rebuild;
-- the compile_tasks-side ones are dropped before their columns).
DROP INDEX IF EXISTS idx_compile_tasks_dead_review;
DROP INDEX IF EXISTS idx_compile_tasks_preflight;

-- 重建 review_queue：恢复 0005 的三动作 CHECK（既有行经 INSERT SELECT 原样
-- 带回；guard 已保证无 Step 8 新动作行）。
-- Rebuild review_queue: restore the 0005 three-action CHECK (existing rows are
-- carried back verbatim via INSERT SELECT; the guard guarantees no Step 8
-- action rows remain).
CREATE TABLE review_queue_step6_restore (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN ('supplemental_compile','query_template','ignore')),
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending','approved','ignored','failed')),
  source_log_ids_json TEXT NOT NULL,
  subject_json TEXT NOT NULL,
  reason_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  reviewed_at INTEGER,
  reviewed_by TEXT,
  compile_task_id INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT,
  UNIQUE(domain, action, subject_json)
);
INSERT INTO review_queue_step6_restore
  SELECT review_id,domain,action,status,source_log_ids_json,subject_json,
         reason_json,created_at,reviewed_at,reviewed_by,compile_task_id
  FROM review_queue;
DROP TABLE review_queue;
ALTER TABLE review_queue_step6_restore RENAME TO review_queue;
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);

-- 回退 compile_tasks / compile_attempts 新增列。
-- Revert the compile_tasks / compile_attempts columns.
ALTER TABLE compile_tasks DROP COLUMN consistency_status;
ALTER TABLE compile_tasks DROP COLUMN compatibility_status;
ALTER TABLE compile_attempts DROP COLUMN consistency_json;
ALTER TABLE compile_attempts DROP COLUMN compatibility_json;

-- 守卫表用后即弃（连接可重复执行回退）。
-- Drop the guard table afterwards (so downgrades can run repeatedly).
DROP TABLE IF EXISTS temp.downgrade_guard;
