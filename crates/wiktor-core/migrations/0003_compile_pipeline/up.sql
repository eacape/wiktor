-- 迁移 0003：编译管线（Step 4 spec §8.1 列级 DDL 合同）。
-- Migration 0003: compile pipeline (Step 4 spec §8.1 column-level DDL contract).
--
-- 要点：
-- - 不改 0001/0002；只 ALTER 新增列、建新表、重建 FTS 触发器为「仅 accepted 入索引」。
-- - pages 增加 source_revision（0=legacy）/artifact_version/frontmatter_json。
-- - compile_tasks 增加全依赖哈希、epoch、任务/attempt 快照、预算与租约列；
--   保留三元 UNIQUE(entity_id, source_revision, domain_pack_version) 与 status CHECK。
-- - legacy 未终结任务没有快照无法恢复：标记 dead/result=failed 并清租约（§8.1）。
-- Highlights:
-- - 0001/0002 untouched; only ALTER ADD COLUMN, new tables, and FTS triggers
--   rebuilt to "accepted-only indexing".
-- - pages gains source_revision (0=legacy) / artifact_version / frontmatter_json.
-- - compile_tasks gains all-dependency hash, epoch, task/attempt snapshots,
--   budget and lease columns; the triple UNIQUE and status CHECK are preserved.
-- - Legacy non-terminal tasks have no snapshot and cannot resume: marked
--   dead/result=failed with lease cleared (§8.1).

-- ============ pages：frontmatter / 来源版本 / 产物版本 ============
ALTER TABLE pages ADD COLUMN source_revision INTEGER NOT NULL DEFAULT 0; -- 0=legacy（seed 页）
ALTER TABLE pages ADD COLUMN artifact_version TEXT NOT NULL DEFAULT 'seed-v1';
ALTER TABLE pages ADD COLUMN frontmatter_json TEXT NOT NULL DEFAULT '{}';

-- ============ compile_tasks：哈希/epoch/快照/预算/租约 ============
ALTER TABLE compile_tasks ADD COLUMN desired_hash      TEXT NOT NULL DEFAULT '';
ALTER TABLE compile_tasks ADD COLUMN epoch             INTEGER NOT NULL DEFAULT 1;
ALTER TABLE compile_tasks ADD COLUMN source_json       TEXT NOT NULL DEFAULT '{}';
ALTER TABLE compile_tasks ADD COLUMN dependencies_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE compile_tasks ADD COLUMN snapshot_hash     TEXT NOT NULL DEFAULT '';
ALTER TABLE compile_tasks ADD COLUMN recompile_count   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE compile_tasks ADD COLUMN attempt_count     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE compile_tasks ADD COLUMN lease_token       TEXT;
ALTER TABLE compile_tasks ADD COLUMN next_attempt_at   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE compile_tasks ADD COLUMN result TEXT
    CHECK (result IS NULL OR result IN ('accepted','quarantined','failed','skipped','superseded'));
ALTER TABLE compile_tasks ADD COLUMN reserved_tokens   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE compile_tasks ADD COLUMN task_token_budget INTEGER NOT NULL DEFAULT 65536;

-- pending 调度索引（§8.1）；running 租约索引沿用 0001 的部分索引。
-- Pending-schedule index (§8.1); the running-lease partial index from 0001 stays.
CREATE INDEX idx_tasks_pending_schedule ON compile_tasks(status, next_attempt_at, task_id);

-- legacy 未终结任务没有快照，无法恢复 → dead/failed + legacy 标记 + 清租约；
-- 合法新 admission 可补齐快照并 epoch+1（§8.1）。
-- Legacy non-terminal tasks lack snapshots and cannot resume → dead/failed with
-- a legacy marker and cleared lease; a legal new admission may backfill the
-- snapshot with epoch+1 (§8.1).
UPDATE compile_tasks
SET status = 'dead',
    result = 'failed',
    error_message = 'legacy_task_missing_snapshot',
    lease_expires_at = NULL,
    updated_at = unixepoch()
WHERE status IN ('pending', 'running');

-- ============ compile_source_heads：来源 head（CAS 身份） ============
CREATE TABLE compile_source_heads (
    entity_id       TEXT PRIMARY KEY,
    source_revision INTEGER NOT NULL,
    snapshot_hash   TEXT NOT NULL,
    desired_hash    TEXT NOT NULL,
    task_id         INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT,
    epoch           INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- ============ compile_attempts：逐 attempt 审计（含隔离候选） ============
CREATE TABLE compile_attempts (
    task_id        INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT,
    epoch          INTEGER,
    attempt_no     INTEGER,
    lease_token    TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('reserved','completed','abandoned')),
    publish_status TEXT CHECK (publish_status IS NULL OR publish_status IN ('candidate','accepted','quarantined')),
    run_id         TEXT REFERENCES compile_runs(run_id) ON DELETE RESTRICT,
    utc_day        INTEGER,
    artifact_json  TEXT,
    quality_json   TEXT,
    issues_json    TEXT NOT NULL,
    reserved_tokens INTEGER NOT NULL,
    reported_tokens INTEGER,
    error_code      TEXT,
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    PRIMARY KEY (task_id, epoch, attempt_no)
);
CREATE INDEX idx_attempts_lease_token ON compile_attempts(lease_token);

-- ============ compile_runs：一次 CLI run 的批预算（不随 fetch 清零） ============
CREATE TABLE compile_runs (
    run_id          TEXT PRIMARY KEY,
    token_limit     INTEGER NOT NULL,
    reserved_tokens INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL
);

-- ============ compile_daily_budget：可选日预算（全部 domain/run 共享） ============
CREATE TABLE compile_daily_budget (
    utc_day         INTEGER PRIMARY KEY, -- floor(unix_seconds / 86400)
    token_limit     INTEGER NOT NULL,
    reserved_tokens INTEGER NOT NULL DEFAULT 0
);

-- ============ qug_edges：接受页的持久化 QUG 边载荷 ============
CREATE TABLE qug_edges (
    page_id      TEXT REFERENCES pages(page_id) ON DELETE CASCADE,
    edge_hash    TEXT,
    edge_json    TEXT NOT NULL,
    generation   INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY (page_id, edge_hash)
);

-- ============ FTS 触发器：仅 accepted 入索引（§8.1） ============
-- 0001/0002 的触发器对所有状态建索引；隔离版本不得进入查询索引，故重建。
-- The 0001/0002 triggers index every status; quarantined versions must stay out
-- of the query index, so rebuild them.
DROP TRIGGER IF EXISTS pages_fts_insert;
DROP TRIGGER IF EXISTS pages_fts_update;
DROP TRIGGER IF EXISTS pages_fts_delete;

CREATE TRIGGER pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT NEW.page_id, NEW.entity_id, NEW.title, NEW.content
    WHERE NEW.status = 'accepted';
END;
CREATE TRIGGER pages_fts_update AFTER UPDATE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT NEW.page_id, NEW.entity_id, NEW.title, NEW.content
    WHERE NEW.status = 'accepted';
END;
CREATE TRIGGER pages_fts_delete AFTER DELETE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
END;

-- 回填前清空 FTS，再仅从 accepted 页回填（trigram 分词保持 0002 不变）。
-- Clear FTS before backfilling from accepted pages only (trigram tokenizer from
-- 0002 unchanged).
DELETE FROM pages_fts;
INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT page_id, entity_id, title, content FROM pages WHERE status = 'accepted';

-- ============ legacy generation sentinel ============
-- legacy 页固定 generation=1 而 generations 表为空时插入保留哨兵
-- （generation=1 / __legacy__ / seed / published），避免新发布与存量 1 碰撞；
-- 后续每接受一页分配全局递增 generation，不重用 1（§8.1）。
-- When legacy pages pin generation=1 while generations is empty, insert the
-- reserved sentinel (generation=1 / __legacy__ / seed / published) so new
-- publishes never collide with the existing 1; every later acceptance allocates
-- a globally increasing generation and never reuses 1 (§8.1).
INSERT INTO generations (generation, domain_pack, domain_pack_version, status, created_at)
SELECT 1, '__legacy__', 'seed', 'published', 0
WHERE NOT EXISTS (SELECT 1 FROM generations)
  AND EXISTS (SELECT 1 FROM pages WHERE generation = 1);
