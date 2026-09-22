-- 迁移 0004：QUG 持久化（Step 5 spec §5 DDL 草案，决策 D2/D3）。
-- Migration 0004: QUG persistence (Step 5 spec §5 DDL draft, decisions D2/D3).
--
-- 要点：
-- - qug_builds 是代次父表：status 状态机 building/published/superseded/failed；
--   partial unique index 保证每个 (domain_name, domain_version) 只有一个
--   published（active build 唯一可读，D2）。
-- - qug_edges 增加 build_id（Step4 载荷允许 NULL，Step5 新写入必须非空，D3）。
-- - qug_page_snapshots 是 active loader 的唯一页面边来源（保留 pages FK 语义）；
--   意图边无 page_id，独立 qug_intent_edges（D3：哨兵 page 不进 pages 生命周期）。
-- Highlights:
-- - qug_builds is the generation parent: status machine
--   building/published/superseded/failed; the partial unique index keeps exactly
--   one published row per (domain_name, domain_version) (the readable active
--   build, D2).
-- - qug_edges gains build_id (Step4 payloads keep NULL; Step5 writes are always
--   non-NULL, D3).
-- - qug_page_snapshots is the single page-edge source for the active loader
--   (pages FK semantics preserved); intent edges carry no page_id and live in
--   their own qug_intent_edges table (D3: no sentinel pages inside `pages`).

-- ============ qug_builds：QUG 构建代次（active published 唯一） ============
CREATE TABLE qug_builds (
  build_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain_name TEXT NOT NULL,
  domain_version TEXT NOT NULL,
  builder_version TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('building','published','superseded','failed')),
  page_count INTEGER NOT NULL DEFAULT 0,
  edge_count INTEGER NOT NULL DEFAULT 0,
  counts_json TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  published_at INTEGER
);
-- 每 domain/version 至多一个 published（读者只见事务前或事务后状态，D2）。
-- At most one published row per domain/version (readers see either the
-- pre-transaction or the post-transaction state, D2).
CREATE UNIQUE INDEX uq_qug_active
  ON qug_builds(domain_name, domain_version) WHERE status='published';

-- ============ qug_edges：增加代次归属（Step4 载荷 NULL 合法） ============
ALTER TABLE qug_edges ADD COLUMN build_id INTEGER
  REFERENCES qug_builds(build_id) ON DELETE CASCADE;
CREATE INDEX idx_qug_edges_build ON qug_edges(build_id, page_id);

-- ============ qug_page_snapshots：页面边完整代次副本 ============
-- 避免原 (page_id, edge_hash) 主键阻止跨代保留；删除 page 级联删页面边（A5）。
-- Avoids the original (page_id, edge_hash) primary key blocking cross-generation
-- retention; deleting a page cascades its page edges (A5).
CREATE TABLE qug_page_snapshots (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  page_id TEXT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  generation INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  PRIMARY KEY(build_id,page_id,edge_hash)
);

-- ============ qug_intent_edges：配置边独立持久化（无 page_id，A5） ============
CREATE TABLE qug_intent_edges (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  PRIMARY KEY(build_id, edge_hash)
);
CREATE INDEX idx_qug_intent_edges_build ON qug_intent_edges(build_id);
