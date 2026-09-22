//! Step 5 批2：QUG 持久化存储层（spec `step5-qug-build.md` §3 D2/D3/D4、
//! §4.2/§4.3、§5 DDL、§7 A5/A6、§8 批2）。
//! Step 5 batch 2: the QUG persistence storage layer (spec
//! `step5-qug-build.md` §3 D2/D3/D4, §4.2/§4.3, §5 DDL, §7 A5/A6, §8 batch 2).
//!
//! 职责边界（spec §4.1：kernel/qug_store.rs 承担"hash、事务发布、加载"；编排
//! `build_and_publish_qug` 也落在本模块，query_engine 因此不依赖 diesel）：
//! - `active_source_hash` / `active_build_identity`：只读 active published 代次；
//! - `publish_build`：单事务 BEGIN IMMEDIATE——先重读 domain 全部 accepted 页
//!   清单与输入 snapshot 比对（不一致 rollback 返回 `source_changed`）→ 同
//!   domain 旧 building 标 failed → 创建 building 行 → 写 qug_page_snapshots +
//!   qug_intent_edges → qug_edges 按 domain 页删除重写镜像（带 build_id）→
//!   读回校验五类计数与 edge hash → 旧 published 标 superseded、新 building 标
//!   published；任一步失败全 rollback，旧图保持可读（A6）；
//! - `load_active_edges`：只读 active published build；页面边取自
//!   qug_page_snapshots、意图边取自 qug_intent_edges，按 edge_hash 稳定排序后
//!   解码；persisted JSON 损坏 = 内部错误（spec §4.3）；
//! - `assemble_qug_snapshot`：从 DB 读指定 domain 全部 accepted 页（legacy seed
//!   与 Step4 accepted compiled 并集，相同 page_id 即 accepted head，D4）组装
//!   `QugSourceSnapshot`（frontmatter_json 直读，不回读 md 文件，STEP5-001）；
//! - `load_active_qug`（批3）：运行时加载——同一只读事务（BEGIN DEFERRED，只加
//!   读锁）内读 active published 代次、重读当前 accepted 页清单并读两类存储边，
//!   复核边计数后用「冻结 domain config（qug 段 canonical JSON）+ 冻结 intents
//!   bytes + 当前页清单 + 存储边载荷」重算 source_hash（复用批1
//!   `compute_source_hash`），一致才解码构图（`QugGraph::from_edges`）并包
//!   `Arc` 返回；无 active → `Ok(None)`（disabled 语义），hash 不一致 → 带
//!   [`QUG_STALE_PREFIX`] 的错误（stale 语义，绝不返回旧图顶替），边 JSON 损坏/
//!   计数不符/图校验失败 → Internal（spec §4.3/§4.4）。
//!
//! Responsibility boundary (spec §4.1: kernel/qug_store.rs owns "hash,
//! transactional publish and load"; the `build_and_publish_qug` orchestration
//! also lives here so query_engine never depends on diesel):
//! - `active_source_hash` / `active_build_identity`: read-only view of the
//!   active published generation;
//! - `publish_build`: one BEGIN IMMEDIATE transaction — first re-read the
//!   domain's full accepted page list and compare it with the input snapshot
//!   (mismatch → rollback with a `source_changed` error) → mark same-domain old
//!   building rows failed → insert the building row → write qug_page_snapshots +
//!   qug_intent_edges → delete-and-rewrite the qug_edges mirror for the domain's
//!   pages (with build_id) → read back and validate the five type counts and
//!   edge hashes → mark the old published row superseded and the new building
//!   row published; any failure rolls everything back and the old graph stays
//!   readable (A6);
//! - `load_active_edges`: reads only the active published build; page edges come
//!   from qug_page_snapshots, intent edges from qug_intent_edges, stably sorted
//!   by edge_hash before decoding; corrupt persisted JSON is an internal error
//!   (spec §4.3);
//! - `assemble_qug_snapshot`: reads every accepted page of the domain from the
//!   DB (legacy seed ∪ Step4 accepted compiled pages; one row per page_id is
//!   the accepted head, D4) into a `QugSourceSnapshot` (frontmatter_json is read
//!   straight from the DB, never re-read from md files, STEP5-001);
//! - `load_active_qug` (batch 3): runtime loading — inside one read-only
//!   transaction (BEGIN DEFERRED, read lock only) it fetches the active
//!   published generation, re-reads the current accepted page list and both
//!   stored edge tables, re-checks the edge counts, then recomputes the
//!   source_hash over "the frozen domain config (canonical qug JSON) + frozen
//!   intents bytes + the current page list + the stored edge payload" (reusing
//!   batch 1's `compute_source_hash`); only on a match does it decode the edges,
//!   build the graph (`QugGraph::from_edges`) and return it wrapped in an `Arc`.
//!   No active build → `Ok(None)` (disabled); hash mismatch → an error carrying
//!   [`QUG_STALE_PREFIX`] (stale — the stale graph is never served); corrupt
//!   edge JSON / count mismatch / graph validation failure → Internal (spec
//!   §4.3/§4.4).

use crate::query_engine::qug::qug_build::{
    compute_source_hash, derive_qug_edges, edge_canonical_json, edge_hash, edge_type_name,
    parse_intents, PersistedIntentEdge, PersistedPageEdge, QugBuildOutcome, QugBuildStats,
    QugPageInput, QugSourceSnapshot, QUG_BUILDER_VERSION,
};
use crate::query_engine::qug::QugGraph;
use crate::traits::{DomainConfig, IntentConfig};
use crate::types::error::{Error, Result};
use crate::types::QugEdge;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use std::collections::BTreeMap;
use std::sync::Arc;

/// QUG 存储契约（spec §4.2 类型契约；实现：[`crate::kernel::SqliteKernel`]）。
/// The QUG storage contract (spec §4.2 type contract; implemented by
/// [`crate::kernel::SqliteKernel`]).
///
/// 全部方法同步、单连接 Mutex 串行化；发布方法内部使用 BEGIN IMMEDIATE 单事务
/// （锁纪律对齐 compile_store）。错误面：非法输入 Validation、损坏持久化载荷
/// Internal、数据库故障 Database；任何错误都不返回部分图。
/// All methods are synchronous and serialized on the single-connection Mutex;
/// the publish method runs inside one BEGIN IMMEDIATE transaction (lock
/// discipline aligned with compile_store). Error surface: Validation for illegal
/// input, Internal for corrupt persisted payloads, Database for DB faults; no
/// error ever yields a partial graph.
pub trait QugStore: Send + Sync {
    /// active published build 的 source_hash；无 active 时 None。
    /// The active published build's source_hash; None when there is no active
    /// build.
    fn active_source_hash(&self, domain: &str, version: &str) -> Result<Option<String>>;

    /// active published build 的 (build_id, source_hash)（`active_source_hash`
    /// 的超集；编排复用路径需要 build_id 填充 `QugBuildStats`）。
    /// The active published build's (build_id, source_hash) (a superset of
    /// `active_source_hash`; the reuse path of the orchestrator needs build_id
    /// to fill `QugBuildStats`).
    fn active_build_identity(&self, domain: &str, version: &str) -> Result<Option<(i64, String)>>;

    /// 事务发布一个构建代次（D2）：发布前重读 domain accepted 页清单与 snapshot
    /// 比对；不一致 rollback 返回 `source_changed`（错误消息含该稳定前缀）。
    /// Publishes one build generation transactionally (D2): the domain's accepted
    /// page list is re-read and compared with the snapshot before any write; a
    /// mismatch rolls back and returns `source_changed` (stable message prefix).
    fn publish_build(
        &self,
        snapshot: &QugSourceSnapshot,
        page_edges: &[PersistedPageEdge],
        intent_edges: &[PersistedIntentEdge],
    ) -> Result<QugBuildStats>;

    /// 加载 active published build 的全部边；无 active（或仅孤立 building）返回
    /// 空集——孤立 building 不可被 load 读取（D2）。
    /// Loads all edges of the active published build; with no active build (or
    /// only an orphaned building row) returns an empty set — orphaned building
    /// rows are never loadable (D2).
    fn load_active_edges(&self, domain: &str, version: &str) -> Result<Vec<QugEdge>>;
}

// ===== `diesel::sql_query` 行映射（QueryableByName）=====
// ===== `diesel::sql_query` row mappings (QueryableByName) =====

#[derive(QueryableByName)]
struct ActiveBuildRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    build_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    source_hash: String,
}

#[derive(QueryableByName)]
struct BuildIdRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    build_id: i64,
}

/// 快照组装 / source 复核共用的 accepted 页身份行（D2 五元组 + frontmatter）。
/// Accepted-page identity row shared by snapshot assembly and the source
/// re-check (the D2 5-tuple plus frontmatter).
#[derive(QueryableByName)]
struct SnapshotPageRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    artifact_version: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    frontmatter_json: String,
}

#[derive(QueryableByName)]
struct PersistedEdgeRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    edge_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    edge_json: String,
}

/// active loader 读取的 published 代次身份与边计数（§4.3"边计数"复核用）。
/// The published-generation identity and edge count read by the active loader
/// (for the §4.3 edge-count re-check).
#[derive(QueryableByName)]
struct ActiveBuildLoadRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    build_id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    source_hash: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    edge_count: i64,
}

/// 页面边快照全列行（解码 + 重建 `PersistedPageEdge` 身份；hash 不吃 page 身份，
/// 但完整读出以便与批1 类型零改造复用）。
/// Full-column page-snapshot row (decode + `PersistedPageEdge` identity
/// reconstruction; the hash ignores page identity, but reading the full row lets
/// batch 1's types be reused unchanged).
#[derive(QueryableByName)]
struct PageSnapshotFullRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    edge_hash: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    edge_json: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    content_hash: String,
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// source_changed 错误的稳定消息前缀（spec D2：返回 source_changed；CLI 批次
/// 按该前缀映射退出码，禁止解析其余正文）。
/// Stable message prefix of the source_changed error (spec D2: return
/// source_changed; the CLI batch maps exit codes on this prefix and never parses
/// the rest of the message).
pub const SOURCE_CHANGED_PREFIX: &str = "qug publish: source_changed:";

/// stale 语义的稳定消息前缀（spec §4.4：active build 的 source_hash 与当前来源
/// 不一致 → 返回携带该前缀的错误；查询接线按前缀写 `stale` 诊断并显式 fallback，
/// 读侧绝不返回旧图顶替；调用方只匹配前缀，禁止解析其余正文）。
/// Stable message prefix of the stale semantics (spec §4.4: when the active
/// build's source_hash disagrees with the current source, an error carrying this
/// prefix is returned; the query wiring records the `stale` diagnosis on the
/// prefix and falls back explicitly — the read side never serves the stale
/// graph; callers match the prefix only and never parse the rest of the
/// message).
pub const QUG_STALE_PREFIX: &str = "qug load: stale:";

/// publish 事务主体（D2 全序：source 复核 → building 行 → 两类边 → 镜像 →
/// 校验 → published；任一步失败整事务回滚）。
/// The publish transaction body (D2 total order: source re-check → building row
/// → both edge kinds → mirror → validation → published; any failure rolls the
/// whole transaction back).
fn publish_in_transaction(
    tx: &mut SqliteConnection,
    snapshot: &QugSourceSnapshot,
    page_edges: &[PersistedPageEdge],
    intent_edges: &[PersistedIntentEdge],
) -> Result<QugBuildStats> {
    let now = super::sqlite::unix_now();

    // —— 步骤 1：发布前重读整个 domain 的 accepted 页面清单，与输入 snapshot
    //    比对（spec §4.3：变化则 rollback 返回 source_changed，不得旧快照覆盖
    //    新来源；读在写之前，FK 失败在 happy path 不可达）。
    // —— Item 1: before any write, re-read the whole domain's accepted page list
    //    and compare it with the input snapshot (spec §4.3: a mismatch rolls back
    //    with source_changed — a stale snapshot must never overwrite a newer
    //    source; the read precedes all writes so FK failures are unreachable on
    //    the happy path).
    let db_pages: Vec<SnapshotPageRow> = diesel::sql_query(
        "SELECT page_id, generation, content_hash, artifact_version, frontmatter_json
         FROM pages
         WHERE domain = ? AND status = 'accepted'
         ORDER BY page_id",
    )
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain)
    .load(tx)?;
    let mut expected: Vec<&QugPageInput> = snapshot.pages.iter().collect();
    expected.sort_by(|a, b| a.page_id.cmp(&b.page_id));
    let source_unchanged = db_pages.len() == expected.len()
        && db_pages.iter().zip(expected.iter()).all(|(db, exp)| {
            db.page_id == exp.page_id
                && db.generation == exp.generation
                && db.content_hash == exp.content_hash
                && db.artifact_version == exp.artifact_version
                && db.frontmatter_json == exp.frontmatter_json
        });
    if !source_unchanged {
        return Err(Error::Validation(format!(
            "{SOURCE_CHANGED_PREFIX} accepted page set of domain {} no longer matches the input \
             snapshot (expected {} pages, db has {})",
            snapshot.domain,
            expected.len(),
            db_pages.len()
        )));
    }

    // —— 步骤 2：同 domain 旧 building 标 failed（孤立 building 不可被 load
    //    读取，D2；失败标记与本次发布同事务，崩溃也不留半状态）。
    // —— Item 2: mark same-domain old building rows failed (orphaned building
    //    rows are never loadable, D2; the marking shares this transaction so a
    //    crash leaves no half state either).
    diesel::sql_query(
        "UPDATE qug_builds SET status = 'failed'
         WHERE domain_name = ? AND domain_version = ? AND status = 'building'",
    )
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain)
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain_version)
    .execute(tx)?;

    // —— 步骤 3：创建 building 行（hash 在事务内对 (snapshot, edges) 重算，与
    //    派生侧同一实现；page_count/edge_count/counts_json 先记预期值，步骤 6
    //    读回校验）。
    // —— Item 3: create the building row (the hash is recomputed inside the
    //    transaction over (snapshot, edges) with the same implementation as the
    //    derivation side; page/edge counts and counts_json record the
    //    expectations first and are read back for validation in item 6).
    let source_hash = compute_source_hash(snapshot, page_edges, intent_edges)?;
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    for edge in page_edges
        .iter()
        .map(|p| &p.edge)
        .chain(intent_edges.iter().map(|p| &p.edge))
    {
        *by_type.entry(edge_type_name(edge).to_string()).or_insert(0) += 1;
    }
    let counts_json = serde_json::to_string(&by_type)?;
    let edge_count = page_edges.len() + intent_edges.len();
    diesel::sql_query(
        "INSERT INTO qug_builds
            (domain_name, domain_version, builder_version, source_hash, status,
             page_count, edge_count, counts_json, created_at)
         VALUES (?, ?, ?, ?, 'building', ?, ?, ?, ?)",
    )
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain)
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain_version)
    .bind::<diesel::sql_types::Text, _>(QUG_BUILDER_VERSION)
    .bind::<diesel::sql_types::Text, _>(&source_hash)
    .bind::<diesel::sql_types::BigInt, _>(snapshot.pages.len() as i64)
    .bind::<diesel::sql_types::BigInt, _>(edge_count as i64)
    .bind::<diesel::sql_types::Text, _>(&counts_json)
    .bind::<diesel::sql_types::BigInt, _>(now)
    .execute(tx)?;
    let build_id = diesel::sql_query("SELECT last_insert_rowid() AS build_id")
        .get_result::<BuildIdRow>(tx)?
        .build_id;

    // —— 步骤 4：写 qug_page_snapshots（页面边，保留 pages FK 语义）+
    //    qug_intent_edges（配置边，无 page_id，A5）。
    // —— Item 4: write qug_page_snapshots (page edges, pages FK semantics kept)
    //    plus qug_intent_edges (config edges, no page_id, A5).
    for p in page_edges {
        let edge_json = edge_canonical_json(&p.edge)?;
        diesel::sql_query(
            "INSERT INTO qug_page_snapshots
                (build_id, page_id, edge_hash, edge_json, generation, content_hash)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(build_id)
        .bind::<diesel::sql_types::Text, _>(&p.page_id)
        .bind::<diesel::sql_types::Text, _>(&p.edge_hash)
        .bind::<diesel::sql_types::Text, _>(&edge_json)
        .bind::<diesel::sql_types::BigInt, _>(p.generation)
        .bind::<diesel::sql_types::Text, _>(&p.content_hash)
        .execute(tx)?;
    }
    for p in intent_edges {
        let edge_json = edge_canonical_json(&p.edge)?;
        diesel::sql_query(
            "INSERT INTO qug_intent_edges (build_id, edge_hash, edge_json) VALUES (?, ?, ?)",
        )
        .bind::<diesel::sql_types::BigInt, _>(build_id)
        .bind::<diesel::sql_types::Text, _>(&p.edge_hash)
        .bind::<diesel::sql_types::Text, _>(&edge_json)
        .execute(tx)?;
    }

    // —— 步骤 5：qug_edges 按 domain 页删除重写镜像（带 build_id；Step4 旧载荷
    //    build_id=NULL "不代表有效完整图"，随域页一起被镜像替换，spec §4.3）。
    // —— Item 5: delete-and-rewrite the qug_edges mirror for the domain's pages
    //    (with build_id; Step4's legacy payload rows with build_id=NULL "never
    //    represent a valid full graph" and are replaced by the mirror along with
    //    the domain's pages, spec §4.3).
    diesel::sql_query(
        "DELETE FROM qug_edges
         WHERE page_id IN (SELECT page_id FROM pages WHERE domain = ?)",
    )
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain)
    .execute(tx)?;
    for p in page_edges {
        let edge_json = edge_canonical_json(&p.edge)?;
        diesel::sql_query(
            "INSERT INTO qug_edges
                (page_id, edge_hash, edge_json, generation, content_hash, build_id)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind::<diesel::sql_types::Text, _>(&p.page_id)
        .bind::<diesel::sql_types::Text, _>(&p.edge_hash)
        .bind::<diesel::sql_types::Text, _>(&edge_json)
        .bind::<diesel::sql_types::BigInt, _>(p.generation)
        .bind::<diesel::sql_types::Text, _>(&p.content_hash)
        .bind::<diesel::sql_types::BigInt, _>(build_id)
        .execute(tx)?;
    }

    // —— 步骤 6：校验五类计数与 hash（读回两类边 + 镜像，重算 edge_hash 与类型
    //    计数；不一致 = 写入损坏 → Internal → 整事务回滚，A6）。
    // —— Item 6: validate the five type counts and hashes (read back both edge
    //    tables plus the mirror, recompute edge_hash and the type tally; any
    //    mismatch means a corrupted write → Internal → the whole transaction
    //    rolls back, A6).
    let stored_pages =
        diesel::sql_query("SELECT COUNT(*) AS n FROM qug_page_snapshots WHERE build_id = ?")
            .bind::<diesel::sql_types::BigInt, _>(build_id)
            .get_result::<CountRow>(tx)?
            .n;
    let stored_intents =
        diesel::sql_query("SELECT COUNT(*) AS n FROM qug_intent_edges WHERE build_id = ?")
            .bind::<diesel::sql_types::BigInt, _>(build_id)
            .get_result::<CountRow>(tx)?
            .n;
    if stored_pages as usize != page_edges.len() || stored_intents as usize != intent_edges.len() {
        return Err(Error::Internal(format!(
            "qug publish: stored edge counts ({stored_pages} page, {stored_intents} intent) \
             do not match the derived expectations ({}, {})",
            page_edges.len(),
            intent_edges.len()
        )));
    }
    let mut read_back: Vec<PersistedEdgeRow> = diesel::sql_query(
        "SELECT edge_hash, edge_json FROM qug_page_snapshots WHERE build_id = ? ORDER BY edge_hash",
    )
    .bind::<diesel::sql_types::BigInt, _>(build_id)
    .load(tx)?;
    read_back.extend(
        diesel::sql_query(
            "SELECT edge_hash, edge_json FROM qug_intent_edges WHERE build_id = ? ORDER BY edge_hash",
        )
        .bind::<diesel::sql_types::BigInt, _>(build_id)
        .load::<PersistedEdgeRow>(tx)?,
    );
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    for row in &read_back {
        // persisted 载荷在写入后立即损坏属于不可能状态 → Internal（spec §4.3
        // 的损坏语义同样适用于发布自检）。
        // A payload corrupted right after the write is an impossible state →
        // Internal (the §4.3 corruption semantics apply to the publish self-check
        // as well).
        let edge: QugEdge = serde_json::from_str(&row.edge_json).map_err(|e| {
            Error::Internal(format!("qug publish: corrupt persisted edge payload: {e}"))
        })?;
        let recomputed = edge_hash(&edge)?;
        if recomputed != row.edge_hash {
            return Err(Error::Internal(format!(
                "qug publish: persisted edge hash mismatch (stored {}, recomputed {recomputed})",
                row.edge_hash
            )));
        }
        *tally.entry(edge_type_name(&edge).to_string()).or_insert(0) += 1;
    }
    if tally != by_type {
        return Err(Error::Internal(format!(
            "qug publish: per-type counts {tally:?} do not match the derived {by_type:?}"
        )));
    }
    let mirror = diesel::sql_query("SELECT COUNT(*) AS n FROM qug_edges WHERE build_id = ?")
        .bind::<diesel::sql_types::BigInt, _>(build_id)
        .get_result::<CountRow>(tx)?
        .n;
    if mirror as usize != page_edges.len() {
        return Err(Error::Internal(format!(
            "qug publish: qug_edges mirror holds {mirror} rows, expected {}",
            page_edges.len()
        )));
    }

    // —— 步骤 7：旧 published 标 superseded → 新 building 标 published（partial
    //    unique index 保证全程至多一个 published；提交后读者只见事务前或事务后
    //    状态，D2）。
    // —— Item 7: mark the old published row superseded → promote the new building
    //    row to published (the partial unique index keeps at most one published
    //    row throughout; after commit readers see either the pre- or the
    //    post-transaction state, D2).
    diesel::sql_query(
        "UPDATE qug_builds SET status = 'superseded'
         WHERE domain_name = ? AND domain_version = ? AND status = 'published'",
    )
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain)
    .bind::<diesel::sql_types::Text, _>(&snapshot.domain_version)
    .execute(tx)?;
    let affected = diesel::sql_query(
        "UPDATE qug_builds SET status = 'published', published_at = ? WHERE build_id = ?",
    )
    .bind::<diesel::sql_types::BigInt, _>(now)
    .bind::<diesel::sql_types::BigInt, _>(build_id)
    .execute(tx)?;
    if affected != 1 {
        return Err(Error::Internal(
            "qug publish: building row vanished before promotion".into(),
        ));
    }

    Ok(QugBuildStats {
        build_id,
        reused: false,
        source_hash,
        accepted_page_count: snapshot.pages.len(),
        edge_count,
        by_type,
    })
}

impl super::sqlite::SqliteKernel {
    /// active published 代次读取（单 SELECT，无事务）。
    /// Reads the active published generation (one SELECT, no transaction).
    fn active_build_row(&self, domain: &str, version: &str) -> Result<Option<ActiveBuildRow>> {
        let mut conn = self.lock_conn()?;
        let row = diesel::sql_query(
            "SELECT build_id, source_hash FROM qug_builds
             WHERE domain_name = ? AND domain_version = ? AND status = 'published'",
        )
        .bind::<diesel::sql_types::Text, _>(domain)
        .bind::<diesel::sql_types::Text, _>(version)
        .get_result(&mut *conn)
        .optional()?;
        Ok(row)
    }

    /// 组装 QUG 构建快照（D4）：domain 全部 accepted 页 = legacy seed（generation=1、
    /// artifact_version='seed-v1'）与 Step4 accepted compiled 页并集；pages 表
    /// page_id 唯一，一行即 accepted head；按 page_id 序输出（D2 哈希输入序）。
    /// Assembles the QUG build snapshot (D4): the domain's accepted pages = the
    /// union of legacy seed (generation=1, artifact_version='seed-v1') and Step4
    /// accepted compiled pages; pages.page_id is unique so one row is the
    /// accepted head; output ordered by page_id (the D2 hash input order).
    pub fn assemble_qug_snapshot(
        &self,
        domain: &str,
        domain_version: &str,
        qug_config_json: String,
        intents_bytes: Vec<u8>,
    ) -> Result<QugSourceSnapshot> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let rows: Vec<SnapshotPageRow> = diesel::sql_query(
                "SELECT page_id, generation, content_hash, artifact_version, frontmatter_json
                 FROM pages
                 WHERE domain = ? AND status = 'accepted'
                 ORDER BY page_id",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .load(tx)?;
            Ok(QugSourceSnapshot {
                domain: domain.to_string(),
                domain_version: domain_version.to_string(),
                qug_config_json,
                intents_bytes,
                pages: rows
                    .into_iter()
                    .map(|r| QugPageInput {
                        page_id: r.page_id,
                        generation: r.generation,
                        content_hash: r.content_hash,
                        artifact_version: r.artifact_version,
                        frontmatter_json: r.frontmatter_json,
                    })
                    .collect(),
            })
        })
    }
}

impl QugStore for super::sqlite::SqliteKernel {
    fn active_source_hash(&self, domain: &str, version: &str) -> Result<Option<String>> {
        Ok(self
            .active_build_row(domain, version)?
            .map(|r| r.source_hash))
    }

    fn active_build_identity(&self, domain: &str, version: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .active_build_row(domain, version)?
            .map(|r| (r.build_id, r.source_hash)))
    }

    /// 发布事务（D2/A6）：单次 BEGIN IMMEDIATE；详见 [`publish_in_transaction`]。
    /// The publish transaction (D2/A6): one BEGIN IMMEDIATE; see
    /// [`publish_in_transaction`].
    fn publish_build(
        &self,
        snapshot: &QugSourceSnapshot,
        page_edges: &[PersistedPageEdge],
        intent_edges: &[PersistedIntentEdge],
    ) -> Result<QugBuildStats> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            publish_in_transaction(tx, snapshot, page_edges, intent_edges)
        })
    }

    /// 加载 active 图（spec §4.3）：只读 published；页面边 + 意图边按 edge_hash
    /// 稳定排序后 JSON 解码；persisted JSON 损坏 = Internal（exit 4 语义）。
    /// Loads the active graph (spec §4.3): published rows only; page + intent
    /// edges are stably sorted by edge_hash then JSON-decoded; corrupt persisted
    /// JSON is Internal (the exit-4 semantics).
    fn load_active_edges(&self, domain: &str, version: &str) -> Result<Vec<QugEdge>> {
        let mut conn = self.lock_conn()?;
        conn.immediate_transaction(|tx| {
            let Some(active) = diesel::sql_query(
                "SELECT build_id FROM qug_builds
                 WHERE domain_name = ? AND domain_version = ? AND status = 'published'",
            )
            .bind::<diesel::sql_types::Text, _>(domain)
            .bind::<diesel::sql_types::Text, _>(version)
            .get_result::<BuildIdRow>(tx)
            .optional()?
            else {
                // 无 active published：返回空集（普通查询显式 fallback 并记录
                // 原因；eval 不得把缺图当有效 C，spec §4.3）。
                // No active published build: return an empty set (ordinary
                // queries fall back explicitly with a recorded reason; eval must
                // never treat a missing graph as a valid C, spec §4.3).
                return Ok(Vec::new());
            };
            let mut items: Vec<(String, String)> = diesel::sql_query(
                "SELECT edge_hash, edge_json FROM qug_page_snapshots
                 WHERE build_id = ? ORDER BY edge_hash",
            )
            .bind::<diesel::sql_types::BigInt, _>(active.build_id)
            .load::<PersistedEdgeRow>(tx)?
            .into_iter()
            .map(|r| (r.edge_hash, r.edge_json))
            .collect();
            items.extend(
                diesel::sql_query(
                    "SELECT edge_hash, edge_json FROM qug_intent_edges
                     WHERE build_id = ? ORDER BY edge_hash",
                )
                .bind::<diesel::sql_types::BigInt, _>(active.build_id)
                .load::<PersistedEdgeRow>(tx)?
                .into_iter()
                .map(|r| (r.edge_hash, r.edge_json)),
            );
            // 两表各自已按 edge_hash 排序，这里全局稳定归并（同 hash 时页面边
            // 在前，确定性总序）。
            // Both tables arrive sorted by edge_hash; merge stably into one
            // global order (on equal hashes page edges come first — a
            // deterministic total order).
            items.sort_by(|a, b| a.0.cmp(&b.0));
            items
                .iter()
                .map(|(_, json)| {
                    serde_json::from_str(json).map_err(|e| {
                        Error::Internal(format!(
                            "qug store: corrupt persisted edge payload for active build {}: {e}",
                            active.build_id
                        ))
                    })
                })
                .collect()
        })
    }
}

/// 编排入口（spec §4.2）：派生边与 hash → 非 force 且 active hash 命中 → 复用
/// `Reused`；否则 `publish_build` 发布 `Published`。派生（图提取）在锁外执行，
/// 发布在单事务内完成（spec §4.3）。
/// The orchestration entry (spec §4.2): derive edges and hash → without force,
/// an active-hash hit reuses and returns `Reused`; otherwise `publish_build`
/// publishes and returns `Published`. Derivation (graph extraction) runs outside
/// the lock; publishing happens in one transaction (spec §4.3).
///
/// 非 force 复用命中时 `QugBuildStats.build_id` 取 active published 代次；
/// 复用检查与发布之间的并发窗口由 publish 事务内的 source 复核兜底（变化 →
/// `source_changed`），不会旧快照覆盖新来源。
/// On a non-force reuse hit, `QugBuildStats.build_id` carries the active
/// published generation; the concurrency window between the reuse check and the
/// publish is fenced by the in-transaction source re-check (a change →
/// `source_changed`), so a stale snapshot never overwrites a newer source.
pub fn build_and_publish_qug(
    store: &dyn QugStore,
    snapshot: &QugSourceSnapshot,
    config: &DomainConfig,
    intents: &IntentConfig,
    force: bool,
) -> Result<QugBuildOutcome> {
    let derived = derive_qug_edges(snapshot, config, intents)?;
    if !force {
        if let Some(active) =
            store.active_source_hash(&snapshot.domain, &snapshot.domain_version)?
        {
            if active == derived.source_hash {
                // hash 命中复用（A1）；build_id 取 active 代次，identity 消失
                // （并发 supersede）则退回发布路径。
                // Hash hit → reuse (A1); build_id carries the active generation;
                // if the identity vanished (concurrent supersede), fall back to
                // publishing.
                if let Some((build_id, _)) =
                    store.active_build_identity(&snapshot.domain, &snapshot.domain_version)?
                {
                    return Ok(QugBuildOutcome::Reused(QugBuildStats {
                        build_id,
                        reused: true,
                        accepted_page_count: derived.accepted_page_count,
                        edge_count: derived.edge_count(),
                        by_type: derived.by_type,
                        source_hash: derived.source_hash,
                    }));
                }
            }
        }
    }
    let stats = store.publish_build(snapshot, &derived.page_edges, &derived.intent_edges)?;
    Ok(QugBuildOutcome::Published(stats))
}

/// 编排便捷封装：从 [`SqliteKernel`] 组装快照（intents 原文 bytes 由调用方读取）
/// 后走 [`build_and_publish_qug`]；`intents_yaml` 为空时等价于未配置
/// intents.yaml。快照组装需要 kernel 的读取接口，故绑定具体实现而非
/// `&dyn QugStore`（trait 本体仍保持 spec §4.2 三方法 + 身份辅助）。
/// Convenience wrapper: assembles the snapshot from the [`SqliteKernel`] (the
/// caller reads the raw intents bytes) and delegates to
/// [`build_and_publish_qug`]; an empty `intents_yaml` is equivalent to a domain
/// without intents.yaml. Snapshot assembly needs the kernel's read APIs, so the
/// wrapper binds the concrete implementation instead of `&dyn QugStore` (the
/// trait itself keeps the spec §4.2 three methods plus the identity helper).
pub fn build_and_publish_qug_from_bytes(
    store: &super::sqlite::SqliteKernel,
    domain: &str,
    domain_version: &str,
    qug_config_json: String,
    intents_yaml: &[u8],
    config: &DomainConfig,
    force: bool,
) -> Result<QugBuildOutcome> {
    // 组装器只读 DB；parse_intents 在派生前完成结构校验（超限/非法 YAML →
    // Validation，spec §4.2）。
    // The assembler only reads the DB; parse_intents performs structural
    // validation before derivation (cap/illegal-YAML → Validation, spec §4.2).
    let intents = parse_intents(intents_yaml)?;
    let snapshot = store.assemble_qug_snapshot(
        domain,
        domain_version,
        qug_config_json,
        intents_yaml.to_vec(),
    )?;
    build_and_publish_qug(store, &snapshot, config, &intents, force)
}

/// 批3 读取事务主体（spec §4.3/§4.4）：在**同一只读事务**内完成
/// 「active 行 → 当前 accepted 页清单 → 两类存储边 → 边计数复核 → source_hash
/// 复核 → 构图」全序；hash 一致才构图，任何一步失败都不返回图。
/// The batch-3 read-transaction body (spec §4.3/§4.4): the full order "active
/// row → current accepted page list → both stored edge tables → edge-count
/// re-check → source_hash re-check → graph construction" completes inside **one
/// read-only transaction**; the graph is built only on a hash match, and no step
/// ever returns a graph on failure.
fn load_active_qug_in_transaction(
    tx: &mut SqliteConnection,
    domain: &DomainConfig,
    intents_bytes: &[u8],
) -> Result<Option<Arc<QugGraph>>> {
    // —— 步骤 1：active published 代次；缺失 → Ok(None)（disabled 语义，§4.4；
    //    孤立 building 不可读，D2）。
    // —— Item 1: the active published generation; missing → Ok(None) (the
    //    disabled semantics, §4.4; orphaned building rows are unreadable, D2).
    let Some(active) = diesel::sql_query(
        "SELECT build_id, source_hash, edge_count FROM qug_builds
         WHERE domain_name = ? AND domain_version = ? AND status = 'published'",
    )
    .bind::<diesel::sql_types::Text, _>(&domain.name)
    .bind::<diesel::sql_types::Text, _>(&domain.version)
    .get_result::<ActiveBuildLoadRow>(tx)
    .optional()?
    else {
        return Ok(None);
    };

    // —— 步骤 2：重读当前 accepted 页清单（与发布侧快照组装同口径：accepted-only
    //    并按 page_id 序，D4）。页清单参与 source_hash——发布后任何页变化（增删/
    //    换代/降级/内容更新）都会让重算结果偏离记录值 → stale。
    // —— Item 2: re-read the current accepted page list (same shape as the
    //    publish-side snapshot assembler: accepted-only, ordered by page_id, D4).
    //    The page list enters source_hash — any post-publish page change
    //    (add/remove/re-generation/demotion/content update) shifts the recomputed
    //    value away from the recorded one → stale.
    let pages: Vec<SnapshotPageRow> = diesel::sql_query(
        "SELECT page_id, generation, content_hash, artifact_version, frontmatter_json
         FROM pages
         WHERE domain = ? AND status = 'accepted'
         ORDER BY page_id",
    )
    .bind::<diesel::sql_types::Text, _>(&domain.name)
    .load(tx)?;

    // —— 步骤 3：读两类存储边并解码（页面边取自 qug_page_snapshots——active
    //    loader 的唯一页面边来源，§4.3；配置边取自 qug_intent_edges）。各自按
    //    edge_hash 升序返回；persisted JSON 损坏 = Internal（exit 4 语义，§4.3）。
    // —— Item 3: read and decode both stored edge kinds (page edges come from
    //    qug_page_snapshots — the active loader's only page-edge source, §4.3;
    //    config edges from qug_intent_edges). Each query returns rows ascending
    //    by edge_hash; corrupt persisted JSON = Internal (the exit-4 semantics,
    //    §4.3).
    let page_rows: Vec<PageSnapshotFullRow> = diesel::sql_query(
        "SELECT page_id, edge_hash, edge_json, generation, content_hash
         FROM qug_page_snapshots
         WHERE build_id = ? ORDER BY edge_hash",
    )
    .bind::<diesel::sql_types::BigInt, _>(active.build_id)
    .load(tx)?;
    let mut page_edges: Vec<PersistedPageEdge> = Vec::with_capacity(page_rows.len());
    for row in page_rows {
        let edge: QugEdge = serde_json::from_str(&row.edge_json).map_err(|e| {
            Error::Internal(format!(
                "qug load: corrupt persisted page-edge payload for active build {}: {e}",
                active.build_id
            ))
        })?;
        page_edges.push(PersistedPageEdge {
            page_id: row.page_id,
            edge_hash: row.edge_hash,
            edge,
            generation: row.generation,
            content_hash: row.content_hash,
        });
    }
    let intent_rows: Vec<PersistedEdgeRow> = diesel::sql_query(
        "SELECT edge_hash, edge_json FROM qug_intent_edges
         WHERE build_id = ? ORDER BY edge_hash",
    )
    .bind::<diesel::sql_types::BigInt, _>(active.build_id)
    .load(tx)?;
    let mut intent_edges: Vec<PersistedIntentEdge> = Vec::with_capacity(intent_rows.len());
    for row in intent_rows {
        let edge: QugEdge = serde_json::from_str(&row.edge_json).map_err(|e| {
            Error::Internal(format!(
                "qug load: corrupt persisted intent-edge payload for active build {}: {e}",
                active.build_id
            ))
        })?;
        intent_edges.push(PersistedIntentEdge {
            edge,
            edge_hash: row.edge_hash,
        });
    }

    // —— 步骤 4：边计数复核（§4.3"边计数"；与 qug_builds 记录值不符 = 元数据
    //    损坏 → Internal，不构图）。
    // —— Item 4: edge-count re-check (§4.3; a mismatch against qug_builds'
    //    recorded value = metadata corruption → Internal, no graph).
    if (page_edges.len() + intent_edges.len()) as i64 != active.edge_count {
        return Err(Error::Internal(format!(
            "qug load: stored edge count ({}) does not match the recorded edge_count {} of \
             active build {}",
            page_edges.len() + intent_edges.len(),
            active.edge_count,
            active.build_id
        )));
    }

    // —— 步骤 5：source_hash 复核（§4.3"比较 source_hash 后才构图"）。重算输入
    //    与 D2 完全同源：冻结的 qug 段 canonical JSON、冻结的 intents 原文 bytes、
    //    当前 accepted 页清单、存储边载荷；复用批1 `compute_source_hash`，不解析
    //    YAML、不派生边。不一致 → 稳定前缀 stale 错误（读侧不得用旧图顶替）。
    // —— Item 5: source_hash re-check (§4.3 "compare source_hash before building
    //    the graph"). The recomputation inputs are exactly D2's: the frozen
    //    canonical qug-config JSON, the frozen raw intents bytes, the current
    //    accepted page list and the stored edge payload; batch 1's
    //    `compute_source_hash` is reused — no YAML parsing, no edge derivation.
    //    A mismatch → a stable-prefixed stale error (the read side must never
    //    substitute the stale graph).
    let snapshot = QugSourceSnapshot {
        domain: domain.name.clone(),
        domain_version: domain.version.clone(),
        qug_config_json: serde_json::to_string(&domain.qug)?,
        intents_bytes: intents_bytes.to_vec(),
        pages: pages
            .into_iter()
            .map(|r| QugPageInput {
                page_id: r.page_id,
                generation: r.generation,
                content_hash: r.content_hash,
                artifact_version: r.artifact_version,
                frontmatter_json: r.frontmatter_json,
            })
            .collect(),
    };
    let recomputed = compute_source_hash(&snapshot, &page_edges, &intent_edges)?;
    if recomputed != active.source_hash {
        return Err(Error::Validation(format!(
            "{QUG_STALE_PREFIX} active build {} recorded source_hash {} does not match the \
             current source (recomputed {recomputed}); refusing to serve the stale graph",
            active.build_id, active.source_hash
        )));
    }

    // —— 步骤 6：按 edge_hash 稳定排序解码边（两表各自已升序，全局稳定归并——
    //    同 hash 页面边在前，与 load_active_edges 的确定性总序一致）→
    //    `QugGraph::from_edges` 校验构图 → Arc（§4.4）。图校验失败 = 持久化图
    //    损坏 → Internal。
    // —— Item 6: stably order the decoded edges by edge_hash (both tables arrive
    //    ascending; a stable global merge keeps page edges ahead on equal hashes,
    //    matching load_active_edges' deterministic total order) → validate and
    //    build via `QugGraph::from_edges` → wrap in an Arc (§4.4). A graph
    //    validation failure = a corrupt persisted graph → Internal.
    let mut merged: Vec<(String, QugEdge)> = page_edges
        .iter()
        .map(|p| (p.edge_hash.clone(), p.edge.clone()))
        .collect();
    merged.extend(
        intent_edges
            .iter()
            .map(|p| (p.edge_hash.clone(), p.edge.clone())),
    );
    merged.sort_by(|a, b| a.0.cmp(&b.0));
    let graph = QugGraph::from_edges(
        merged.into_iter().map(|(_, edge)| edge),
        domain.qug.max_depth,
    )
    .map_err(|e| {
        Error::Internal(format!(
            "qug load: graph validation failed for active build {}: {e}",
            active.build_id
        ))
    })?;
    Ok(Some(Arc::new(graph)))
}

/// 运行时加载 active QUG 图（spec §4.4；批3 查询接线入口）。
/// Loads the active QUG graph at runtime (spec §4.4; the batch-3 query wiring
/// entry).
///
/// 语义（任务口径）：
/// - 无 active published build → `Ok(None)`（disabled：查询诊断写 `disabled`）；
/// - active build 存在，但其 source_hash 与「冻结的 domain config（qug 段
///   canonical JSON）+ 冻结的 intents 原文 bytes + 当前 accepted 页清单 + 存储
///   边载荷」重算结果不一致 → 携带稳定前缀 [`QUG_STALE_PREFIX`] 的
///   `Validation` 错误（stale：查询诊断写 `stale`；本二选一选择"稳定前缀错误"
///   而非 `Ok(None)`，因为 §4.4 要求 disabled 与 stale 两种诊断可区分，而前置
///   `SOURCE_CHANGED_PREFIX` 已确立该模式）；
/// - 边 JSON 损坏 / 边计数不符 / 图校验失败 → `Internal`（exit 4 语义；eval 等
///   强一致路径必须直接失败，普通查询可显式 fallback 并记录原因，§4.3）。
///
/// 读侧约束（§4.4）：全部读取发生在同一只读事务（BEGIN DEFERRED，只加读锁不加
/// 写锁）；本函数不解析 YAML、不派生边——`intents_bytes` 由调用方在启动/reload
/// 时冻结传入；构图成功返回 `Some(Arc<QugGraph>)`。
/// Semantics (the task's terms):
/// - no active published build → `Ok(None)` (disabled: query diagnostics report
///   `disabled`);
/// - an active build exists but its source_hash disagrees with the value
///   recomputed over "the frozen domain config (canonical qug JSON) + the frozen
///   raw intents bytes + the current accepted page list + the stored edge
///   payload" → a `Validation` error carrying the stable [`QUG_STALE_PREFIX`]
///   (stale: query diagnostics report `stale`; of the two allowed shapes this
///   implementation picks the stable-prefixed error over `Ok(None)` because
///   §4.4 requires disabled and stale diagnoses to be distinguishable, and the
///   pre-existing `SOURCE_CHANGED_PREFIX` already established the pattern);
/// - corrupt edge JSON / count mismatch / graph validation failure → `Internal`
///   (the exit-4 semantics; strongly-consistent paths such as eval must fail
///   outright, while ordinary queries may fall back explicitly with the reason
///   recorded, §4.3).
///
/// Read-side constraints (§4.4): every read happens in one read-only transaction
/// (BEGIN DEFERRED — a read lock, never a write lock); this function never parses
/// YAML and never derives edges — `intents_bytes` are frozen and passed in by the
/// caller at startup/reload; a successful build returns
/// `Some(Arc<QugGraph>)`.
pub fn load_active_qug(
    kernel: &super::sqlite::SqliteKernel,
    domain: &DomainConfig,
    intents_bytes: &[u8],
) -> Result<Option<Arc<QugGraph>>> {
    let mut conn = kernel.lock_conn()?;
    // 读事务（BEGIN DEFERRED）：只加读锁，不加写锁（spec §4.4）。
    // Read transaction (BEGIN DEFERRED): read lock only, never a write lock
    // (spec §4.4).
    conn.transaction(|tx| load_active_qug_in_transaction(tx, domain, intents_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::SqliteKernel;
    use crate::query_engine::qug::qug_build::page_frontmatter_json;
    use crate::seed::parse_page;
    use crate::types::PublishStatus;

    const INTENTS_YAML: &str = r#"version: "0.1.0"
intents:
  - id: low_sugar
    phrases: ["不甜的", "少糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
"#;

    fn fixture_config() -> DomainConfig {
        let yaml = r#"name: milk-tea
version: "0.1.0"
entities:
  - name: drink
    source: jsonl://fixture
    id_field: id
    type_field: type
    fields:
      - name: sugar_level
        field_type: numeric
        filterable: true
      - name: ingredient_ids
        field_type: reflist
        filterable: true
query:
  filters: [sugar_level, ingredient_ids]
qug:
  enabled: true
  max_depth: 2
  candidate_multiplier: 5
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    /// seed 一页带 aliases/tags 的 accepted 页（frontmatter 由 seed 路径按
    /// STEP5-001 规范落库）。
    /// Seeds one accepted page with aliases/tags (frontmatter persisted by the
    /// seed path per STEP5-001).
    fn seed_page(kernel: &SqliteKernel, page_id: &str, title: &str, aliases: &str, tags: &str) {
        let md = format!(
            "---\npage_id: {page_id}\nentity_id: {page_id}\ntitle: {title}\nentity_type: drink\naliases: {aliases}\ntags: {tags}\n---\n\n{title}是经典饮品。\n\n## 概述\n\n- 茶底\n"
        );
        let page = parse_page(&md).unwrap();
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();
    }

    fn qug_config_json(config: &DomainConfig) -> String {
        serde_json::to_string(&config.qug).unwrap()
    }

    /// load 结果的稳定指纹：边内容重算 edge_hash 升序列（QugEdge 未实现
    /// PartialEq，以内容哈希比较代替整边相等）。
    /// A stable fingerprint of a load result: edge hashes recomputed from the
    /// edge contents, ascending (QugEdge does not implement PartialEq, so
    /// content hashes stand in for whole-edge equality).
    fn edge_fingerprint(edges: &[QugEdge]) -> Vec<String> {
        let mut hashes: Vec<String> = edges.iter().map(|e| edge_hash(e).unwrap()).collect();
        hashes.sort();
        hashes
    }

    /// 从 DB 组装当前快照并派生意图配置（测试共用编排输入）。
    /// Assembles the current snapshot from the DB and parses intents (shared
    /// orchestration input for tests).
    fn snapshot_and_intents(
        kernel: &SqliteKernel,
        config: &DomainConfig,
    ) -> (QugSourceSnapshot, IntentConfig) {
        let snapshot = kernel
            .assemble_qug_snapshot(
                "milk-tea",
                "0.1.0",
                qug_config_json(config),
                INTENTS_YAML.as_bytes().to_vec(),
            )
            .unwrap();
        let intents = parse_intents(&snapshot.intents_bytes).unwrap();
        (snapshot, intents)
    }

    #[derive(QueryableByName)]
    struct TextRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    fn count(kernel: &SqliteKernel, sql: &str) -> i64 {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(sql)
            .get_result::<CountRow>(&mut *conn)
            .unwrap()
            .n
    }

    fn text_scalar(kernel: &SqliteKernel, sql: &str) -> String {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(sql)
            .get_result::<TextRow>(&mut *conn)
            .unwrap()
            .value
    }

    // ===== A5：发布 → load 全量边；页面删除级联删页面边、意图边保留 =====
    // ===== A5: publish → load all edges; page delete cascades page edges and
    // keeps intent edges =====

    #[test]
    fn a5_publish_load_and_page_delete_cascade() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸奶茶]",
            "[奶茶]",
        );
        seed_page(
            &kernel,
            "milk-tea:drink:oolong",
            "乌龙奶茶",
            "[乌龙]",
            "[奶茶, 茶底]",
        );
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);

        let outcome = build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap();
        let stats = match &outcome {
            QugBuildOutcome::Published(s) => s,
            other => panic!("expected published, got {other:?}"),
        };
        assert_eq!(stats.accepted_page_count, 2);
        // 页面边：boba 1 alias + 1 tag；oolong 1 alias + 2 tag → 5；
        // 配置边：attribute 2 短语 + negation 1 短语 → 3。
        // Page edges: boba 1 alias + 1 tag; oolong 1 alias + 2 tags → 5;
        // config edges: attribute 2 phrases + negation 1 phrase → 3.
        assert_eq!(stats.edge_count, 8);
        assert_eq!(stats.by_type.get("synonym"), Some(&2));
        assert_eq!(stats.by_type.get("hyponym"), Some(&3));
        assert_eq!(stats.by_type.get("attribute_propagation"), Some(&2));
        assert_eq!(stats.by_type.get("negation"), Some(&1));

        // load 返回全部 8 条边（页面边 5 + 意图边 3），类型齐全。
        // load returns all 8 edges (5 page + 3 intent), all four types present.
        let edges = kernel.load_active_edges("milk-tea", "0.1.0").unwrap();
        assert_eq!(edges.len(), 8);
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for e in &edges {
            *tally.entry(edge_type_name(e).to_string()).or_insert(0) += 1;
        }
        assert_eq!(tally.get("synonym"), Some(&2));
        assert_eq!(tally.get("hyponym"), Some(&3));
        assert_eq!(tally.get("attribute_propagation"), Some(&2));
        assert_eq!(tally.get("negation"), Some(&1));

        // A5：删除 page 级联删页面边（qug_page_snapshots + qug_edges 镜像），
        // 但不删意图边（qug_intent_edges 无 page_id）。
        // A5: deleting a page cascades its page edges (qug_page_snapshots +
        // qug_edges mirror) but never the intent edges (no page_id there).
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query("DELETE FROM pages WHERE page_id = 'milk-tea:drink:oolong'")
                .execute(&mut *conn)
                .unwrap();
        }
        assert_eq!(
            count(&kernel, "SELECT COUNT(*) AS n FROM qug_page_snapshots"),
            2,
            "only boba's page edges survive the cascade"
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_edges WHERE build_id IS NOT NULL"
            ),
            2,
            "mirror page edges cascade with the page"
        );
        assert_eq!(
            count(&kernel, "SELECT COUNT(*) AS n FROM qug_intent_edges"),
            3,
            "intent edges have no page_id and survive"
        );
        let edges = kernel.load_active_edges("milk-tea", "0.1.0").unwrap();
        assert_eq!(edges.len(), 5, "2 page edges + 3 intent edges");
    }

    // ===== A1（存储侧）：同输入二次 build hash 命中 reused，不产生新代次 =====
    // ===== A1 (storage side): a second build over identical input hits the hash
    // and is reused without a new generation =====

    #[test]
    fn a1_second_build_reuses_active_hash() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸奶茶]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);

        let first = build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap();
        let first_stats = match &first {
            QugBuildOutcome::Published(s) => s,
            other => panic!("expected published, got {other:?}"),
        };
        let second = build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap();
        let reused = match &second {
            QugBuildOutcome::Reused(s) => s,
            other => panic!("expected reused, got {other:?}"),
        };
        assert!(reused.reused);
        assert_eq!(reused.build_id, first_stats.build_id);
        assert_eq!(reused.source_hash, first_stats.source_hash);
        // 不产生新代次：published 恰 1 行，边表行数不变。
        // No new generation: exactly one published row; edge-table row counts
        // unchanged.
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'published'"
            ),
            1
        );
        assert_eq!(count(&kernel, "SELECT COUNT(*) AS n FROM qug_builds"), 1);
        let page_edges = count(&kernel, "SELECT COUNT(*) AS n FROM qug_page_snapshots");
        let intent_edges = count(&kernel, "SELECT COUNT(*) AS n FROM qug_intent_edges");
        assert_eq!(page_edges + intent_edges, first_stats.edge_count as i64);
    }

    // force：同 hash 也新建代次（旧 published → superseded，partial unique index
    // 保持至多一个 published）。
    // force: a new generation even on a hash hit (old published → superseded; the
    // partial unique index keeps at most one published row).
    #[test]
    fn force_publishes_new_generation_and_supersedes_old() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        let first =
            match build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap() {
                QugBuildOutcome::Published(s) => s,
                other => panic!("expected published, got {other:?}"),
            };

        let forced = build_and_publish_qug(&kernel, &snapshot, &config, &intents, true).unwrap();
        let stats = match &forced {
            QugBuildOutcome::Published(s) => s,
            other => panic!("expected published, got {other:?}"),
        };
        assert!(
            stats.build_id > first.build_id,
            "force allocates a new build_id"
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'superseded'"
            ),
            1
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'published'"
            ),
            1,
            "partial unique index keeps exactly one published row"
        );
        // active 读的是新代次。
        // The active read resolves to the new generation.
        let (_, active_hash) = kernel
            .active_build_identity("milk-tea", "0.1.0")
            .unwrap()
            .unwrap();
        assert_eq!(active_hash, stats.source_hash);
        assert_eq!(
            kernel.load_active_edges("milk-tea", "0.1.0").unwrap().len(),
            stats.edge_count
        );
    }

    // ===== A6：publish 中途失败（注入错误 edge_hash）→ 全 rollback，旧 active
    // 图仍可加载、无半套新边 =====
    // ===== A6: a mid-publish failure (injected wrong edge_hash) → full rollback;
    // the old active graph still loads and no half-set of new edges exists =====

    #[test]
    fn a6_publish_failure_rolls_back_and_keeps_old_graph() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        let first =
            match build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap() {
                QugBuildOutcome::Published(s) => s,
                other => panic!("expected published, got {other:?}"),
            };
        let baseline_edges = kernel.load_active_edges("milk-tea", "0.1.0").unwrap();
        assert_eq!(
            baseline_edges.len(),
            first.edge_count,
            "build 1 must be fully loadable before the failing publish"
        );
        let mirror_before = count(
            &kernel,
            "SELECT COUNT(*) AS n FROM qug_edges WHERE build_id IS NOT NULL",
        );
        assert_eq!(
            mirror_before, 2,
            "build 1 mirrors exactly boba's two page edges"
        );

        // 注入失败：payload 合法但 edge_hash 与内容不符 → 步骤 6 校验失败 →
        // Internal → 整事务回滚（此时镜像删除已发生，必须被还原）。
        // Injected failure: a legal payload whose edge_hash disagrees with its
        // content → item-6 validation fails → Internal → the whole transaction
        // rolls back (the mirror delete already happened and must be restored).
        let edge = QugEdge::IntentTemplate {
            phrase: "招牌".into(),
            expansion: crate::types::Query {
                text: "招牌奶茶".into(),
                filters: crate::types::Filters::empty(),
                top_k: 5,
                domain: Some("milk-tea".into()),
            },
        };
        let bad = PersistedIntentEdge {
            edge_hash: "0".repeat(64),
            edge,
        };
        let err = kernel
            .publish_build(&snapshot, &[], std::slice::from_ref(&bad))
            .unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");

        // 旧 active 图仍可加载；qug_edges 镜像被还原；无 building/新代次残留。
        // The old active graph still loads; the qug_edges mirror is restored; no
        // building row or extra generation remains.
        assert_eq!(
            edge_fingerprint(&kernel.load_active_edges("milk-tea", "0.1.0").unwrap()),
            edge_fingerprint(&baseline_edges)
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_edges WHERE build_id IS NOT NULL"
            ),
            mirror_before,
            "the mid-transaction mirror delete must roll back"
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'building'"
            ),
            0
        );
        assert_eq!(count(&kernel, "SELECT COUNT(*) AS n FROM qug_builds"), 1);
        assert_eq!(
            count(&kernel, "SELECT COUNT(*) AS n FROM qug_page_snapshots"),
            2,
            "no half-set of new page snapshots"
        );
        assert_eq!(
            count(&kernel, "SELECT COUNT(*) AS n FROM qug_intent_edges"),
            3,
            "no half-set of new intent edges (only the first build's 3 remain)"
        );
    }

    // ===== source_changed：来源变化后仍用旧快照发布 → 稳定前缀错误 + rollback；
    // 重新组装快照后发布成功 =====
    // ===== source_changed: publishing a stale snapshot after the source moved →
    // stable-prefix error + rollback; re-assembling then publishing succeeds =====

    #[test]
    fn source_changed_is_reported_and_rolls_back() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (stale_snapshot, intents) = snapshot_and_intents(&kernel, &config);
        let baseline = match build_and_publish_qug(
            &kernel,
            &stale_snapshot,
            &config,
            &intents,
            false,
        )
        .unwrap()
        {
            QugBuildOutcome::Published(s) => s,
            other => panic!("expected published, got {other:?}"),
        };

        // 来源前进：新增一页，但发布仍携带旧快照。force 跳过复用检查，强制走
        // 发布路径（旧快照与新 active 同 hash，非 force 会直接复用）。
        // The source moves on: a new page appears, but the publish carries the
        // stale snapshot. force skips the reuse check and reaches the publish
        // path (a stale snapshot shares the active hash, so non-force would just
        // reuse).
        seed_page(
            &kernel,
            "milk-tea:drink:lemon",
            "柠檬茶",
            "[柠檬]",
            "[果茶]",
        );
        let err =
            build_and_publish_qug(&kernel, &stale_snapshot, &config, &intents, true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(SOURCE_CHANGED_PREFIX),
            "expected source_changed prefix, got {msg}"
        );
        // rollback 后旧图原样可读、无 building 残留。
        // After the rollback the old graph loads unchanged and no building row
        // lingers.
        assert_eq!(
            kernel.load_active_edges("milk-tea", "0.1.0").unwrap().len(),
            baseline.edge_count
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'building'"
            ),
            0
        );

        // 重新组装快照 → 发布成功，新页边进入 active 图。
        // Re-assemble the snapshot → publish succeeds and the new page's edges
        // join the active graph.
        let (fresh, intents) = snapshot_and_intents(&kernel, &config);
        match build_and_publish_qug(&kernel, &fresh, &config, &intents, false).unwrap() {
            QugBuildOutcome::Published(s) => {
                assert_eq!(s.accepted_page_count, 2);
                assert_eq!(
                    kernel.load_active_edges("milk-tea", "0.1.0").unwrap().len(),
                    s.edge_count
                );
            }
            other => panic!("expected published, got {other:?}"),
        }
    }

    // ===== 孤立 building：不可被 load 读取；下次 publish 标 failed =====
    // ===== Orphaned building rows: never loadable; the next publish marks them
    // failed =====

    #[test]
    fn orphaned_building_is_unloadable_and_failed_on_next_publish() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        // 手工插入一个 building 代次（模拟崩溃残留）。
        // Manually insert a building generation (a crash leftover).
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query(
                "INSERT INTO qug_builds
                    (domain_name, domain_version, builder_version, source_hash, status,
                     page_count, edge_count, counts_json, created_at)
                 VALUES ('milk-tea', '0.1.0', 'qug-build-v1', 'deadbeef', 'building', 0, 0, '{}', 1)",
            )
            .execute(&mut *conn)
            .unwrap();
        }
        // 孤立 building 不可被 load 读取（D2）。
        // An orphaned building row is never loadable (D2).
        assert!(kernel
            .load_active_edges("milk-tea", "0.1.0")
            .unwrap()
            .is_empty());
        assert!(kernel
            .active_source_hash("milk-tea", "0.1.0")
            .unwrap()
            .is_none());

        // 下次 publish 把同 domain 旧 building 标 failed 并正常发布。
        // The next publish marks the same-domain old building row failed and
        // publishes normally.
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        assert!(matches!(
            build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap(),
            QugBuildOutcome::Published(_)
        ));
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'failed'"
            ),
            1
        );
        assert_eq!(
            count(
                &kernel,
                "SELECT COUNT(*) AS n FROM qug_builds WHERE status = 'published'"
            ),
            1
        );
        assert!(!kernel
            .load_active_edges("milk-tea", "0.1.0")
            .unwrap()
            .is_empty());
    }

    // 无 active published → load 返回空集（普通查询显式 fallback 的前提）。
    // No active published build → load returns an empty set (the precondition
    // for an explicit query-time fallback).
    #[test]
    fn load_without_active_build_is_empty() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        assert!(kernel
            .load_active_edges("milk-tea", "0.1.0")
            .unwrap()
            .is_empty());
        assert!(kernel
            .active_source_hash("milk-tea", "0.1.0")
            .unwrap()
            .is_none());
    }

    // 空 accepted 页面允许构建：零页面边、配置边照常发布（spec §4.2）。
    // Empty accepted page set may still build: zero page edges, config edges
    // publish as usual (spec §4.2).
    #[test]
    fn empty_page_set_still_publishes_config_edges() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        assert!(snapshot.pages.is_empty());
        match build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap() {
            QugBuildOutcome::Published(s) => {
                assert_eq!(s.accepted_page_count, 0);
                assert_eq!(s.edge_count, 3, "attribute 2 + negation 1");
            }
            other => panic!("expected published, got {other:?}"),
        }
    }

    // ===== STEP5-001（seed 侧）：seed 页 frontmatter_json 含 title/aliases/tags，
    // 且重复 seed 幂等（文本不变） =====
    // ===== STEP5-001 (seed side): the seed page's frontmatter_json carries
    // title/aliases/tags and repeated seeding is idempotent (text unchanged) =====

    #[test]
    fn seed_frontmatter_json_contains_title_aliases_tags() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸奶茶, boba]",
            "[奶茶, 经典]",
        );
        let fm = text_scalar(
            &kernel,
            "SELECT frontmatter_json AS value FROM pages WHERE page_id = 'milk-tea:drink:boba'",
        );
        let v: serde_json::Value = serde_json::from_str(&fm).unwrap();
        assert_eq!(v["title"], "珍珠奶茶");
        assert_eq!(v["aliases"][0], "波霸奶茶");
        assert_eq!(v["aliases"][1], "boba");
        assert_eq!(v["tags"][0], "奶茶");
        assert_eq!(v["tags"][1], "经典");

        // 快照组装只读 DB：frontmatter_json 直读进 QugPageInput。
        // Snapshot assembly reads the DB only: frontmatter_json flows straight
        // into QugPageInput.
        let config = fixture_config();
        let snapshot = kernel
            .assemble_qug_snapshot("milk-tea", "0.1.0", qug_config_json(&config), Vec::new())
            .unwrap();
        assert_eq!(snapshot.pages.len(), 1);
        assert_eq!(snapshot.pages[0].frontmatter_json, fm);
        assert_eq!(snapshot.pages[0].artifact_version, "seed-v1");
        assert_eq!(snapshot.pages[0].generation, 1);

        // 重复 seed 幂等：frontmatter 文本不变。
        // Repeated seeding is idempotent: the frontmatter text stays identical.
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸奶茶, boba]",
            "[奶茶, 经典]",
        );
        let again = text_scalar(
            &kernel,
            "SELECT frontmatter_json AS value FROM pages WHERE page_id = 'milk-tea:drink:boba'",
        );
        assert_eq!(fm, again);
    }

    // source 复核拒绝快照与 DB 不一致（幽灵页）：不产生任何行。
    // The source re-check rejects a snapshot inconsistent with the DB (a phantom
    // page): no rows are produced.
    #[test]
    fn phantom_page_in_snapshot_is_source_changed() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (mut snapshot, intents) = snapshot_and_intents(&kernel, &config);
        snapshot.pages.push(QugPageInput {
            page_id: "milk-tea:drink:ghost".into(),
            generation: 1,
            content_hash: "ghost".into(),
            artifact_version: "seed-v1".into(),
            frontmatter_json: "{}".into(),
        });
        let err = build_and_publish_qug(&kernel, &snapshot, &config, &intents, true).unwrap_err();
        assert!(err.to_string().contains(SOURCE_CHANGED_PREFIX), "got {err}");
        assert_eq!(count(&kernel, "SELECT COUNT(*) AS n FROM qug_builds"), 0);
        assert_eq!(count(&kernel, "SELECT COUNT(*) AS n FROM qug_edges"), 0);
    }

    // ===== 批3 fixture：直接插入一行 pages（镜像 Step4 管线写入的非 accepted /
    // compiled 形态；frontmatter_json 按 STEP5-001 规范承载 title/aliases/tags）=====
    // ===== Batch-3 fixture: insert one pages row directly (mirrors non-accepted /
    // compiled shapes written by the Step4 pipeline; frontmatter_json carries
    // title/aliases/tags per STEP5-001) =====

    fn insert_page_row(
        kernel: &SqliteKernel,
        page_id: &str,
        title: &str,
        status: &str,
        generation: i64,
        artifact_version: &str,
        frontmatter_json: &str,
    ) {
        let mut conn = kernel.lock_conn().unwrap();
        diesel::sql_query(
            "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                content_hash, generation, status, domain_pack_version, compiled_at,
                model_version, embedding_model, created_at, updated_at,
                source_revision, artifact_version, frontmatter_json)
             VALUES (?, ?, 'milk-tea', 'drink', ?, '内容。', ?, ?, ?, '0.1.0', 1,
                'seed', 'none', 1, 1, 0, ?, ?)",
        )
        .bind::<diesel::sql_types::Text, _>(page_id)
        .bind::<diesel::sql_types::Text, _>(page_id)
        .bind::<diesel::sql_types::Text, _>(title)
        .bind::<diesel::sql_types::Text, _>(format!("h-{page_id}"))
        .bind::<diesel::sql_types::BigInt, _>(generation)
        .bind::<diesel::sql_types::Text, _>(status)
        .bind::<diesel::sql_types::Text, _>(artifact_version)
        .bind::<diesel::sql_types::Text, _>(frontmatter_json)
        .execute(&mut *conn)
        .unwrap();
    }

    // ===== A4（批3）：仅 accepted 页参与构图与加载；candidate/quarantined/
    // 非 accepted 高代次页不产生边；发布后 accepted 集变化 → 读侧 stale，绝不
    // 返回旧图；重建后新代次只含 accepted 页边 =====
    // ===== A4 (batch 3): only accepted pages build and load; candidate/
    // quarantined/higher-generation non-accepted pages yield no edges; any
    // post-publish change of the accepted set → the read side reports stale and
    // never serves the old graph; after a rebuild the new generation only holds
    // accepted-page edges =====

    #[test]
    fn a4_accepted_only_build_and_load() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        // 非 accepted 三形态：candidate / quarantined / 高 generation 但未 accepted
        //（"孤立旧 generation" 的真实形态是状态而非代次号决定参与）。
        // Three non-accepted shapes: candidate / quarantined / a higher generation
        // that is not accepted (participation is decided by status, not by the
        // generation number).
        insert_page_row(
            &kernel,
            "milk-tea:drink:cand",
            "候选茶",
            "candidate",
            2,
            "wiki-v1",
            &page_frontmatter_json("候选茶", &["候选".to_string()], &["茶".to_string()]).unwrap(),
        );
        insert_page_row(
            &kernel,
            "milk-tea:drink:quar",
            "隔离茶",
            "quarantined",
            3,
            "wiki-v1",
            &page_frontmatter_json("隔离茶", &["隔离".to_string()], &["茶".to_string()]).unwrap(),
        );
        insert_page_row(
            &kernel,
            "milk-tea:drink:oldgen",
            "旧代茶",
            "quarantined",
            9,
            "wiki-v1",
            &page_frontmatter_json("旧代茶", &["旧代".to_string()], &["茶".to_string()]).unwrap(),
        );

        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        let stats =
            match build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap() {
                QugBuildOutcome::Published(s) => s,
                other => panic!("expected published, got {other:?}"),
            };
        // 构图侧 accepted-only：页数 1、页面边仅 boba 的 1 alias + 1 tag。
        // Build side is accepted-only: 1 page, page edges only boba's 1 alias + 1
        // tag.
        assert_eq!(stats.accepted_page_count, 1);
        assert_eq!(stats.by_type.get("synonym"), Some(&1));
        assert_eq!(stats.by_type.get("hyponym"), Some(&1));

        // 加载侧 accepted-only：图里只有 boba 的 2 条页面边 + 3 条配置边。
        // Load side is accepted-only: only boba's 2 page edges + 3 config edges.
        let graph = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .expect("active build must load");
        assert_eq!(graph.graph.edge_count(), 5);

        // 发布后 accepted 集变化（boba 降级为 candidate）→ 读侧 stale，不返回旧图。
        // The accepted set changes after publish (boba demoted to candidate) → the
        // read side reports stale and never serves the old graph.
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query(
                "UPDATE pages SET status = 'candidate' WHERE page_id = 'milk-tea:drink:boba'",
            )
            .execute(&mut *conn)
            .unwrap();
        }
        let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains(QUG_STALE_PREFIX),
            "expected stale prefix, got {err}"
        );

        // 重建后新代次：零页面边 + 3 条配置边照常可加载（spec §4.2）。
        // After a rebuild the new generation: zero page edges + the 3 config edges
        // still load (spec §4.2).
        let (fresh, intents) = snapshot_and_intents(&kernel, &config);
        assert_eq!(fresh.pages.len(), 0, "accepted-only: no pages remain");
        match build_and_publish_qug(&kernel, &fresh, &config, &intents, false).unwrap() {
            QugBuildOutcome::Published(s) => {
                assert_eq!(s.accepted_page_count, 0);
                assert_eq!(s.edge_count, 3);
            }
            other => panic!("expected published, got {other:?}"),
        }
        let graph = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .expect("the rebuilt generation must load");
        assert_eq!(graph.graph.edge_count(), 3);
    }

    // ===== A7（批3）：hash 一致加载成功；页变化 → stale 稳定前缀错误；边 JSON
    // 损坏 / 边计数被篡改 → Internal；无 active build → Ok(None)。四种情况均不
    // 返回旧图顶替。 =====
    // ===== A7 (batch 3): a consistent hash loads; a page change → the stale
    // stable-prefix error; corrupt edge JSON / a tampered edge count → Internal;
    // no active build → Ok(None). None of the four ever returns the stale graph
    // as a substitute. =====

    #[test]
    fn a7_load_active_qug_hash_corruption_and_missing_semantics() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap();

        // hash 一致 → Some，且与 load_active_edges 的边集一致（同一存储载荷）。
        // Consistent hash → Some, with the same edge set as load_active_edges (the
        // same stored payload).
        let graph = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .expect("consistent hash must load");
        assert_eq!(graph.graph.edge_count(), 5);
        let again = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .expect("repeat load must succeed");
        assert_eq!(again.graph.edge_count(), 5);

        // 页 content/frontmatter 变化 → 重算 hash 偏离记录值 → stale 稳定前缀。
        // A page content/frontmatter change → the recomputed hash drifts from the
        // recorded one → the stale stable prefix.
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query(
                "UPDATE pages SET content_hash = 'h-changed'
                 WHERE page_id = 'milk-tea:drink:boba'",
            )
            .execute(&mut *conn)
            .unwrap();
        }
        let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains(QUG_STALE_PREFIX),
            "expected stale prefix, got {err}"
        );

        // 还原 → 恢复加载。
        // Restored → loads again.
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query(
                "UPDATE pages SET content_hash = ? WHERE page_id = 'milk-tea:drink:boba'",
            )
            .bind::<diesel::sql_types::Text, _>(snapshot.pages[0].content_hash.as_str())
            .execute(&mut *conn)
            .unwrap();
        }
        assert!(load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .is_some());

        // 边 JSON 损坏 → Internal（exit 4 语义），绝不解码出旧图。载荷按
        // edge_hash 逐行捕获/还原（edge_hash 在代次内唯一），避免整表覆盖。
        // Corrupt edge JSON → Internal (the exit-4 semantics); the old graph is
        // never decoded out of it. Payloads are captured/restored row by row by
        // edge_hash (unique within a generation) instead of a whole-table
        // overwrite.
        type EdgePairs = Vec<(String, String)>;
        let (page_pairs, intent_pairs): (EdgePairs, EdgePairs) = {
            let mut conn = kernel.lock_conn().unwrap();
            #[derive(QueryableByName)]
            struct P {
                #[diesel(sql_type = diesel::sql_types::Text)]
                h: String,
                #[diesel(sql_type = diesel::sql_types::Text)]
                j: String,
            }
            let p =
                diesel::sql_query("SELECT edge_hash AS h, edge_json AS j FROM qug_page_snapshots")
                    .load::<P>(&mut *conn)
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.h, r.j))
                    .collect();
            let i =
                diesel::sql_query("SELECT edge_hash AS h, edge_json AS j FROM qug_intent_edges")
                    .load::<P>(&mut *conn)
                    .unwrap()
                    .into_iter()
                    .map(|r| (r.h, r.j))
                    .collect();
            (p, i)
        };
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query("UPDATE qug_page_snapshots SET edge_json = 'not json'")
                .execute(&mut *conn)
                .unwrap();
        }
        let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");
        // 还原页面边 → 损坏意图边 → 仍 Internal。
        // Restore the page edge → corrupt an intent edge → still Internal.
        {
            let mut conn = kernel.lock_conn().unwrap();
            for (h, j) in &page_pairs {
                diesel::sql_query(
                    "UPDATE qug_page_snapshots SET edge_json = ? WHERE edge_hash = ?",
                )
                .bind::<diesel::sql_types::Text, _>(j)
                .bind::<diesel::sql_types::Text, _>(h)
                .execute(&mut *conn)
                .unwrap();
            }
            diesel::sql_query("UPDATE qug_intent_edges SET edge_json = 'not json'")
                .execute(&mut *conn)
                .unwrap();
        }
        let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");
        {
            let mut conn = kernel.lock_conn().unwrap();
            for (h, j) in &intent_pairs {
                diesel::sql_query("UPDATE qug_intent_edges SET edge_json = ? WHERE edge_hash = ?")
                    .bind::<diesel::sql_types::Text, _>(j)
                    .bind::<diesel::sql_types::Text, _>(h)
                    .execute(&mut *conn)
                    .unwrap();
            }
        }

        // 边计数被篡改（qug_builds.edge_count 与存储行不符）→ Internal。
        // A tampered edge count (qug_builds.edge_count vs the stored rows) →
        // Internal.
        {
            let mut conn = kernel.lock_conn().unwrap();
            diesel::sql_query("UPDATE qug_builds SET edge_count = 99 WHERE status = 'published'")
                .execute(&mut *conn)
                .unwrap();
        }
        let err = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes()).unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "got {err:?}");
        {
            let mut conn = kernel.lock_conn().unwrap();
            // 还原为存储边行数的真实值，不硬编码。
            // Restore to the actual stored-edge row count instead of a hardcoded
            // number.
            let stored = diesel::sql_query(
                "SELECT (SELECT COUNT(*) FROM qug_page_snapshots)
                        + (SELECT COUNT(*) FROM qug_intent_edges) AS n",
            )
            .get_result::<CountRow>(&mut *conn)
            .unwrap()
            .n;
            diesel::sql_query("UPDATE qug_builds SET edge_count = ? WHERE status = 'published'")
                .bind::<diesel::sql_types::BigInt, _>(stored)
                .execute(&mut *conn)
                .unwrap();
        }
        assert!(load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .is_some());

        // 无 active build（有页、无发布）→ Ok(None)：disabled 语义，不是错误。
        // No active build (pages but no publish) → Ok(None): the disabled
        // semantics, not an error.
        let fresh = SqliteKernel::open_in_memory().unwrap();
        seed_page(
            &fresh,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        assert!(load_active_qug(&fresh, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .is_none());
    }

    // ===== A8（批3）：legacy seed（generation=1, seed-v1）与 Step4 accepted
    // compiled 页共存供边；空 CompiledPage.qug_edges 不阻断；golden 文件只读
    // 健全性（本批不改 golden）。 =====
    // ===== A8 (batch 3): legacy seed (generation=1, seed-v1) and Step4 accepted
    // compiled pages coexist as edge sources; empty CompiledPage.qug_edges does
    // not block; golden file read-only sanity (this batch never touches goldens).
    // =====

    #[test]
    fn a8_seed_and_compiled_pages_coexist_and_load() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        // legacy seed 页（seed 路径：generation=1, artifact_version='seed-v1'）。
        // Legacy seed page (seed path: generation=1, artifact_version='seed-v1').
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "[波霸]",
            "[奶茶]",
        );
        // Step4 accepted compiled 页形态（generation=2, artifact_version='wiki-v1'）。
        // The Step4 accepted compiled-page shape (generation=2,
        // artifact_version='wiki-v1').
        insert_page_row(
            &kernel,
            "milk-tea:drink:fruit",
            "水果茶",
            "accepted",
            2,
            "wiki-v1",
            &page_frontmatter_json("水果茶", &["fruit".to_string()], &["果茶".to_string()])
                .unwrap(),
        );

        let config = fixture_config();
        let (snapshot, intents) = snapshot_and_intents(&kernel, &config);
        assert_eq!(snapshot.pages.len(), 2, "seed ∪ compiled accepted union");
        assert!(snapshot
            .pages
            .iter()
            .any(|p| p.artifact_version == "seed-v1" && p.generation == 1));
        assert!(snapshot
            .pages
            .iter()
            .any(|p| p.artifact_version == "wiki-v1" && p.generation == 2));
        let stats =
            match build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap() {
                QugBuildOutcome::Published(s) => s,
                other => panic!("expected published, got {other:?}"),
            };
        // 两页各 1 alias + 1 tag → 4 条页面边 + 3 条配置边。
        // Each page contributes 1 alias + 1 tag → 4 page edges + 3 config edges.
        assert_eq!(stats.accepted_page_count, 2);
        assert_eq!(stats.edge_count, 7);

        let graph = load_active_qug(&kernel, &config, INTENTS_YAML.as_bytes())
            .unwrap()
            .expect("seed+compiled generation must load");
        assert_eq!(graph.graph.edge_count(), 7);

        // Step4 空 `CompiledPage.qug_edges` 不阻断：Step3 提取器对空载荷零贡献、
        // 零错误（Step5 构图只吃 frontmatter_json，不依赖 CompiledPage.qug_edges）。
        // Step4's empty `CompiledPage.qug_edges` does not block: the Step3
        // extractor contributes nothing and errors on nothing for an empty
        // payload (Step5 building consumes frontmatter_json only and never
        // depends on CompiledPage.qug_edges).
        let md = "---\npage_id: milk-tea:drink:plain\nentity_id: milk-tea:drink:plain\n\
                  entity_type: drink\ntitle: 白开水\n---\n\n## 概述\n\n- 水\n";
        let wiki = crate::seed::parse_page(md).unwrap();
        let compiled = crate::query_engine::qug::compiled_page(&wiki);
        assert!(compiled.qug_edges.is_empty());
        assert!(
            crate::query_engine::qug::extract_page_edges(std::slice::from_ref(&compiled))
                .is_empty()
        );
    }

    // A8 只读健全性：golden 文件存在、逐行可解析、每条含 query 字段；本批不改
    // golden（34 条原文与期望保持原样，批4 再扩容）。
    // A8 read-only sanity: the golden file exists, parses line by line and every
    // record carries a query field; this batch never modifies it (the 34 existing
    // records stay byte-identical; batch 4 extends the file).
    #[test]
    fn a8_golden_file_readonly_sanity() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("milk-tea")
            .join("golden-queries.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.trim().is_empty(), "golden file must not be empty");
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("golden line must parse as JSON: {e}"));
            assert!(
                v.get("query")
                    .and_then(|q| q.as_str())
                    .is_some_and(|s| !s.is_empty()),
                "golden record must carry a non-empty query: {v}"
            );
        }
    }
}
