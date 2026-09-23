-- 迁移 0005：反馈闭环（Step 6 spec §5 DDL 草案逐字段照抄；决策 D3/D4/D5、D10）。
-- Migration 0005: feedback loop (Step 6 spec §5 DDL draft copied field-by-field;
-- decisions D3/D4/D5, D10).
--
-- 要点：
-- - query_logs 增加租户 domain（legacy 行默认 '__legacy__'，STEP6-002）与
--   滤空/放宽三状态列（D10：区分「滤空」与「知识盲区」，放宽至多一次）。
-- - feedback_events：click/adopt/rate 反馈事实（D3：hit 不入库，命中数已在
--   query log）；(domain, idempotency_key) UNIQUE 支撑 200 幂等回放（D5：
--   不更新载荷）；log_id FK RESTRICT 防孤儿事件；kind/rating/page 组合由
--   CHECK 约束背书。
-- - review_queue：控制面审核事实（D4：审核建议不是可运行任务，不扩展
--   compile_tasks 审核列、不污染 Step4 状态机）；UNIQUE(domain, action,
--   subject_json) 保证重复分析幂等。
-- - feedback_rejections：输入预算 #7 的可审计拒绝计数（D9：超预算请求不写
--   正常反馈表）。
-- Highlights:
-- - query_logs gains the tenant domain (legacy rows default to '__legacy__',
--   STEP6-002) and the three filter-empty/relaxation state columns (D10:
--   separate "filtered empty" from "knowledge blind spot"; relaxation runs at
--   most once).
-- - feedback_events: click/adopt/rate feedback facts (D3: no `hit` events —
--   hit counts already live in the query log); the (domain, idempotency_key)
--   UNIQUE supports 200 idempotent replay (D5: payloads never overwritten);
--   log_id FK RESTRICT prevents orphan events; kind/rating/page combinations
--   are backed by CHECK constraints.
-- - review_queue: control-plane review facts (D4: suggestions are not runnable
--   tasks; compile_tasks keeps no review columns and the Step4 state machine
--   stays untouched); UNIQUE(domain, action, subject_json) makes repeated
--   analysis runs idempotent.
-- - feedback_rejections: auditable rejection counters for input budget #7
--   (D9: over-budget requests never reach the normal feedback table).

-- ============ query_logs：租户 domain + 滤空/放宽状态 ============
ALTER TABLE query_logs ADD COLUMN domain TEXT NOT NULL DEFAULT '__legacy__';
ALTER TABLE query_logs ADD COLUMN candidate_empty_initial INTEGER NOT NULL DEFAULT 0
  CHECK (candidate_empty_initial IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_attempted INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_attempted IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_succeeded INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_succeeded IN (0,1));
CREATE INDEX idx_query_logs_domain_time ON query_logs(domain, timestamp);

-- ============ feedback_events：click/adopt/rate 反馈事实（D3/D5） ============
CREATE TABLE feedback_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  idempotency_key TEXT NOT NULL,
  domain TEXT NOT NULL,
  log_id INTEGER NOT NULL REFERENCES query_logs(log_id) ON DELETE RESTRICT,
  kind TEXT NOT NULL CHECK (kind IN ('click','adopt','rate')),
  page_id TEXT,
  rating INTEGER CHECK (rating IS NULL OR rating BETWEEN 1 AND 5),
  metadata_json TEXT NOT NULL DEFAULT '{}',
  received_at INTEGER NOT NULL,
  CHECK ((kind = 'rate' AND rating IS NOT NULL) OR
         (kind IN ('click','adopt') AND rating IS NULL)),
  CHECK ((kind IN ('click','adopt') AND page_id IS NOT NULL) OR
         (kind = 'rate')),
  UNIQUE(domain, idempotency_key)
);
CREATE INDEX idx_feedback_log ON feedback_events(domain, log_id);
CREATE INDEX idx_feedback_page ON feedback_events(domain, page_id) WHERE page_id IS NOT NULL;
CREATE INDEX idx_feedback_received ON feedback_events(domain, received_at);

-- ============ review_queue：控制面审核事实（D4/D12） ============
CREATE TABLE review_queue (
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
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);

-- ============ feedback_rejections：输入预算拒绝计数（D9/#7） ============
CREATE TABLE feedback_rejections (
  rejection_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT,
  reason TEXT NOT NULL CHECK (reason IN ('payload_too_large','event_count_too_large','field_too_large')),
  payload_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_feedback_rejections_time ON feedback_rejections(created_at);
