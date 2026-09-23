-- 迁移 0006：Step 8 一致性、死信审核与兼容检查（Step 8 spec §5.2 DDL 草案
-- 逐列照抄；决策 D10/D12）。
-- Migration 0006: Step 8 consistency, dead-letter review, compatibility checks
-- (Step 8 spec §5.2 DDL draft copied column-by-column; decisions D10/D12).
--
-- 要点：
-- - compile_tasks 增 consistency_status / compatibility_status（均以
--   'unchecked' 为默认，保持既有行语义不变并支撑 A1 兼容升级）。
-- - compile_attempts 增 consistency_json / compatibility_json：仅保存确定性
--   诊断摘要（code、比较键、旧/新值 BLAKE3 摘要、版本和数量），不保存源明文；
--   完整候选 artifact 仍在 attempt 自身。
-- - review_queue 的 action CHECK 必须通过表重建放宽（SQLite 不支持直接
--   ALTER CHECK，STEP8-004/009）：新表逐列复制、INSERT SELECT 保留既有行
--   （review_id/FK/UNIQUE(domain,action,subject_json) 原样带入），三个新增
--   动作 compile_dead_letter/consistency_conflict/compatibility_conflict 仅
--   扩展 CHECK，不改 Step 6 审核生命周期。
-- - 索引：死信审核扫描（status,result,updated_at）、兼容 preflight 扫描
--   （domain_pack_version,compatibility_status）与任务级死信去重部分索引
--   （D7：同一 task 只允许一条 compile_dead_letter）。
-- Highlights:
-- - compile_tasks gains consistency_status / compatibility_status (both
--   defaulting to 'unchecked', keeping existing-row semantics intact and
--   supporting the A1 compatible upgrade).
-- - compile_attempts gains consistency_json / compatibility_json: only
--   deterministic diagnostic summaries are stored (code, comparison keys, old/
--   new value BLAKE3 digests, versions and counts), never raw source text; the
--   full candidate artifact stays in the attempt itself.
-- - The review_queue action CHECK must be widened via a table rebuild (SQLite
--   cannot ALTER a CHECK in place, STEP8-004/009): the new table copies every
--   column, INSERT SELECT preserves existing rows (review_id/FK and the
--   UNIQUE(domain,action,subject_json) carry over), and the three new actions
--   compile_dead_letter/consistency_conflict/compatibility_conflict only extend
--   the CHECK without touching the Step 6 review lifecycle.
-- - Indexes: dead-letter review scan (status,result,updated_at), compatibility
--   preflight scan (domain_pack_version,compatibility_status) and the task-keyed
--   partial index for dead-letter dedup (D7: one compile_dead_letter per task).

-- ============ compile_tasks：一致性 / 兼容状态（D10/D12） ============
ALTER TABLE compile_tasks ADD COLUMN consistency_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (consistency_status IN ('unchecked','not_comparable','consistent','conflict'));
ALTER TABLE compile_tasks ADD COLUMN compatibility_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (compatibility_status IN ('unchecked','compatible','incompatible'));

-- ============ compile_attempts：确定性诊断摘要（不存源明文） ============
-- Only deterministic diagnostic summaries; raw source text never lands here.
ALTER TABLE compile_attempts ADD COLUMN consistency_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE compile_attempts ADD COLUMN compatibility_json TEXT NOT NULL DEFAULT '{}';

CREATE INDEX idx_compile_tasks_dead_review
  ON compile_tasks(status, result, updated_at);
CREATE INDEX idx_compile_tasks_preflight
  ON compile_tasks(domain_pack_version, compatibility_status);

-- ============ review_queue：重建放宽 action CHECK（X1，STEP8-004/009） ============
-- 0005 review_queue 的 action CHECK 必须通过表重建放宽；SQLite 不支持直接 ALTER CHECK。
-- The 0005 review_queue action CHECK must be widened via a table rebuild; SQLite
-- cannot ALTER a CHECK in place.
CREATE TABLE review_queue_step8_new (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN (
    'supplemental_compile','query_template','ignore',
    'compile_dead_letter','consistency_conflict','compatibility_conflict'
  )),
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
INSERT INTO review_queue_step8_new
  SELECT review_id,domain,action,status,source_log_ids_json,subject_json,
         reason_json,created_at,reviewed_at,reviewed_by,compile_task_id
  FROM review_queue;
DROP TABLE review_queue;
ALTER TABLE review_queue_step8_new RENAME TO review_queue;
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);
CREATE INDEX idx_review_task ON review_queue(domain, action, compile_task_id)
  WHERE compile_task_id IS NOT NULL;
