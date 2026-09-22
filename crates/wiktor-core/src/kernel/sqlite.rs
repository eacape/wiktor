use crate::db_schema::{
    fact_refs as fact_refs_t, facts as facts_t, page_quality as page_quality_t,
    page_sections as page_sections_t, pages as pages_t,
};
use crate::schema::{self, facts};
use crate::traits::EntityStore;
use crate::types::error::{Error, Result};
#[cfg(test)]
use crate::types::FilterCondition;
use crate::types::{
    EntityId, FactValue, Facts, Filters, PublishStatus, Query, SearchHit, WikiPage,
};
use async_trait::async_trait;
use diesel::connection::Connection;
use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

/// SQLite 内核：两平面 + FTS5 + 任务队列 + 查询日志，单连接（MVP 单写者）。
/// SQLite kernel: two-plane storage + FTS5 + task queue + query log, single
/// connection (MVP single-writer model).
///
/// 存储层使用 diesel（SQLite bundled）：pages/facts 等 CRUD 走 ORM 的类型安全
/// DSL；FTS5 trigram MATCH、bm25、过滤下推这类核心检索 SQL 走 `diesel::sql_query`
/// raw SQL 逃生（见 `kernel/sqlite.rs::search`）。
/// The storage layer uses diesel (SQLite bundled): type-safe CRUD via the ORM DSL
/// for pages/facts; core retrieval SQL (FTS5 trigram MATCH, bm25, filter pushdown)
/// goes through the `diesel::sql_query` raw-SQL escape hatch (see `kernel/sqlite.rs::search`).
pub struct SqliteKernel {
    conn: Mutex<SqliteConnection>,
}

/// search 返回行（`QueryableByName` 供 `diesel::sql_query` 映射）。
/// Row returned by `search`, mapped via `QueryableByName` for `diesel::sql_query`.
#[derive(QueryableByName)]
struct SearchRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    page_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    entity_id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    title: String,
    #[diesel(sql_type = diesel::sql_types::Float)]
    score: f32,
}

/// 本二进制支持的 schema 版本（0003_compile_pipeline，见 A22）。`open_existing`
/// 用它拒绝旧/新 schema 而不迁移。
/// The schema version this binary supports (0003_compile_pipeline, see A22).
/// `open_existing` uses it to reject older/newer schemas without migrating.
const SUPPORTED_SCHEMA_VERSION: i64 = 3;

/// 单列文本行（filter 全量 / 过滤下推用）。
/// Single text-column row (used for full filter scans and filter pushdown).
#[derive(QueryableByName)]
struct TextRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

/// COUNT(*) 行（row_counts / schema 版本用）。
/// COUNT(*) row (used by `row_counts` / schema versioning).
#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

impl SqliteKernel {
    /// 打开（不存在则创建）并应用迁移。
    /// Opens the database (creating it if missing) and applies migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let path_str = path.to_str().ok_or_else(|| {
            Error::InvalidConfig(format!("db path is not valid UTF-8: {}", path.display()))
        })?;
        let mut conn = establish(path_str)?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = establish(":memory:")?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 打开**已存在**的数据库但不做迁移（Step 4 §9 dry-run 专用 inspect 连接）：
    /// 不创建文件、不升级 schema；版本落后于 [`SUPPORTED_SCHEMA_VERSION`] 时报
    /// `migration_required`（消息前缀稳定，供 CLI 判别），绝不自行升级。
    /// 打开后只读路径（如 `load_accepted_pages`）可用；不执行任何写操作。
    /// Opens an **existing** database without migrating (the Step 4 §9 dry-run
    /// inspect connection): never creates the file, never upgrades the schema;
    /// when the version is behind [`SUPPORTED_SCHEMA_VERSION`] it fails with a
    /// stable `migration_required` message prefix (for CLI triage) and never
    /// upgrades on its own. Read-only paths (e.g. `load_accepted_pages`) work on
    /// the returned kernel; no writes are performed.
    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::Validation(format!(
                "database not found: {} (dry-run never creates one)",
                path.display()
            )));
        }
        let path_str = path.to_str().ok_or_else(|| {
            Error::InvalidConfig(format!("db path is not valid UTF-8: {}", path.display()))
        })?;
        let mut conn = establish(path_str)?;
        // 不调用 migrate：dry-run 语义禁止迁移/写 pragma。
        // `schema_version` reads the migrations ledger only; a missing table (a
        // non-Wiktor or pre-0001 file) surfaces as Error::Database.
        let version = schema::schema_version(&mut conn).map_err(|e| {
            Error::InvalidConfig(format!(
                "migration_required: cannot read schema version of {}: {e}",
                path.display()
            ))
        })?;
        if version != SUPPORTED_SCHEMA_VERSION {
            return Err(Error::InvalidConfig(format!(
                "migration_required: database {} is at schema version {version}, expected \
                 {SUPPORTED_SCHEMA_VERSION}; dry-run never migrates, open it with a migrating \
                 command first",
                path.display()
            )));
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 取连接锁一次（编译管线事务接口与既有方法共用；poison → Internal）。
    /// Locks the connection exactly once (shared by the compile-pipeline
    /// transactional API and the existing methods; poison → Internal).
    pub(super) fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, SqliteConnection>> {
        self.conn
            .lock()
            .map_err(|_| Error::Internal("kernel connection mutex poisoned".into()))
    }

    /// 当前 schema 版本（已应用迁移数）。
    /// Current schema version (number of applied migrations).
    pub fn schema_version(&self) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        schema::schema_version(&mut conn)
    }

    /// 各核心表的行数。
    /// Row counts for each core table.
    pub fn row_counts(&self) -> Result<BTreeMap<String, i64>> {
        let mut conn = self.conn.lock().unwrap();
        let tables = [
            "pages",
            "page_quality",
            "page_sections",
            "facts",
            "fact_refs",
            "compile_tasks",
            "query_logs",
            "generations",
        ];
        let mut out = BTreeMap::new();
        for t in tables {
            let sql = format!("SELECT COUNT(*) AS n FROM {t}");
            let r: CountRow = diesel::sql_query(sql).get_result(&mut *conn)?;
            out.insert(t.to_string(), r.n);
        }
        Ok(out)
    }

    /// 直接执行（供 CLI/工具使用）。
    /// Executes SQL directly (for CLI / tooling use).
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        diesel::connection::SimpleConnection::batch_execute(&mut *conn, sql)?;
        Ok(())
    }

    /// 写入/覆盖一页知识平面（seed 场景：手工编译产物，评分默认满分，幂等）。
    /// Writes/overwrites one knowledge-plane page (seed scenario: hand-compiled
    /// artifacts, default perfect quality score, idempotent).
    ///
    /// 重复导入同一 page_id：diesel `on_conflict(page_id).do_update()` 覆盖页面，
    /// 章节先删后插，不产生重复行；FTS 由触发器同步。
    /// Re-importing the same page_id: diesel `on_conflict(page_id).do_update()`
    /// overwrites the page, sections are delete-then-insert, so no duplicate rows;
    /// FTS is kept in sync by triggers.
    pub fn seed_pages(&self, page: &WikiPage, domain: &str, status: PublishStatus) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let now = unix_now();
        let content_hash = blake3::hash(format!("{}\0{}", page.title, page.content).as_bytes())
            .to_hex()
            .to_string();
        let status_str = match status {
            PublishStatus::Candidate => "candidate",
            PublishStatus::Accepted => "accepted",
            PublishStatus::Quarantined => "quarantined",
        };
        let entity_key = page.entity_id.to_key();
        let entity_type = page.entity_id.entity_type.clone();

        conn.transaction(|tx| -> Result<()> {
            // 页面 upsert（等价 INSERT OR REPLACE）
            // Page upsert (equivalent to INSERT OR REPLACE)
            diesel::insert_into(pages_t::table)
                .values((
                    pages_t::page_id.eq(&page.page_id),
                    pages_t::entity_id.eq(&entity_key),
                    pages_t::domain.eq(domain),
                    pages_t::entity_type.eq(&entity_type),
                    pages_t::title.eq(&page.title),
                    pages_t::content.eq(&page.content),
                    pages_t::content_hash.eq(&content_hash),
                    pages_t::generation.eq(1_i64),
                    pages_t::status.eq(status_str),
                    pages_t::domain_pack_version.eq(&page.metadata.domain_pack_version),
                    pages_t::compiled_at.eq(page.metadata.compiled_at),
                    pages_t::model_version.eq(&page.metadata.model_version),
                    pages_t::embedding_model.eq(&page.metadata.embedding_model),
                    pages_t::created_at.eq(now),
                    pages_t::updated_at.eq(now),
                ))
                .on_conflict(pages_t::page_id)
                .do_update()
                .set((
                    pages_t::entity_id.eq(&entity_key),
                    pages_t::domain.eq(domain),
                    pages_t::entity_type.eq(&entity_type),
                    pages_t::title.eq(&page.title),
                    pages_t::content.eq(&page.content),
                    pages_t::content_hash.eq(&content_hash),
                    pages_t::generation.eq(1_i64),
                    pages_t::status.eq(status_str),
                    pages_t::domain_pack_version.eq(&page.metadata.domain_pack_version),
                    pages_t::compiled_at.eq(page.metadata.compiled_at),
                    pages_t::model_version.eq(&page.metadata.model_version),
                    pages_t::embedding_model.eq(&page.metadata.embedding_model),
                    pages_t::updated_at.eq(now),
                ))
                .execute(tx)?;

            // 章节重写（先删后插，幂等）
            // Section rewrite (delete-then-insert, idempotent)
            diesel::delete(
                page_sections_t::table.filter(page_sections_t::page_id.eq(&page.page_id)),
            )
            .execute(tx)?;
            for (i, s) in page.sections.iter().enumerate() {
                diesel::insert_into(page_sections_t::table)
                    .values((
                        page_sections_t::section_id.eq(format!("{}#{}", page.page_id, i)),
                        page_sections_t::page_id.eq(&page.page_id),
                        page_sections_t::heading.eq(&s.heading),
                        page_sections_t::content.eq(&s.content),
                        page_sections_t::section_index.eq(i as i64),
                    ))
                    .execute(tx)?;
            }

            // 手工 seed 页面默认质量满分（四规则维度 1.0，一致性留空）
            // Hand-seeded pages default to a perfect quality score (1.0 on the four
            // rule dimensions; consistency left empty)
            diesel::insert_into(page_quality_t::table)
                .values((
                    page_quality_t::page_id.eq(&page.page_id),
                    page_quality_t::coverage.eq(1.0_f64),
                    page_quality_t::citation.eq(1.0_f64),
                    page_quality_t::schema_compliance.eq(1.0_f64),
                    page_quality_t::density.eq(1.0_f64),
                    page_quality_t::consistency.eq(Option::<f64>::None),
                    page_quality_t::overall.eq(1.0_f64),
                ))
                .on_conflict(page_quality_t::page_id)
                .do_update()
                .set((
                    page_quality_t::coverage.eq(1.0_f64),
                    page_quality_t::citation.eq(1.0_f64),
                    page_quality_t::schema_compliance.eq(1.0_f64),
                    page_quality_t::density.eq(1.0_f64),
                    page_quality_t::consistency.eq(Option::<f64>::None),
                    page_quality_t::overall.eq(1.0_f64),
                ))
                .execute(tx)?;
            Ok(())
        })?;
        Ok(())
    }

    /// 事实平面过滤 → 知识页候选集合（Step 3 QueryEngine 用）。
    /// Fact-plane filter → knowledge-page candidate set (used by the Step 3 QueryEngine).
    ///
    /// 语义：SKU 满足过滤条件 → 取其 `category` 值集合（= 知识页 entity_id）。
    /// 与 `search` 内联的过滤下推一致；独立暴露供引擎层先预筛、再把候选域同时
    /// 传给 FTS 与向量路径（避免 top-k 后过滤漏召回）。
    /// Semantics: SKUs matching the conditions → collect their `category` values
    /// (= knowledge-page entity_id). Same as the inline filter pushdown in `search`,
    /// exposed separately so the engine can prefilter first and pass the candidate
    /// scope to both the FTS and vector paths (avoiding post-top-k filtering misses).
    pub fn filter_page_candidates(&self, filters: &Filters) -> Result<Vec<EntityId>> {
        if filters.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock().unwrap();
        let Some((fragment, fparams)) = facts::filter_where(filters)? else {
            return Ok(Vec::new());
        };
        let fragment = inline_params(&fragment, &fparams);
        let sql = format!(
            "SELECT DISTINCT cat.value_text AS value
             FROM facts cat
             JOIN (SELECT DISTINCT entity_id FROM facts WHERE {fragment}) ft
                 ON ft.entity_id = cat.entity_id
             WHERE cat.field_name = 'category' AND cat.field_type = 'text'"
        );
        let rows: Vec<TextRow> = diesel::sql_query(&sql).load(&mut *conn)?;
        rows.into_iter()
            .map(|r| EntityId::from_key(&r.value))
            .collect()
    }

    /// 候选检索（Step 3）：对每个 term 独立执行 FTS5/LIKE，按页面取最高 BM25 分；
    /// 支持事实平面过滤下推与候选实体域白名单，**不写查询日志**（由 QueryEngine
    /// 统一写，避免一条查询产生两条日志）。
    /// Candidate retrieval (Step 3): runs FTS5/LIKE per term and keeps the best
    /// BM25 score per page; supports fact-plane filter pushdown and a candidate
    /// entity-scope whitelist, and does **not** write a query log (the QueryEngine
    /// writes one log, avoiding double logging for a single query).
    ///
    /// `candidate_ids`：
    ///   - `None` = 无候选域限制（仅受 filters 下推约束）；
    ///   - `Some(非空)` = 严格白名单，`pages.entity_id` 必须属于该集合；
    ///   - `Some(空)` = 空候选域，直接返回空结果。
    ///
    /// `candidate_ids`:
    ///   - `None` = unrestricted (only the filter pushdown applies);
    ///   - `Some(non-empty)` = strict whitelist; `pages.entity_id` must be in the set;
    ///   - `Some(empty)` = empty scope, returns no results.
    pub fn search_candidates(
        &self,
        terms: &[String],
        filters: &Filters,
        top_k: usize,
        domain: Option<&str>,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<SearchHit>> {
        let mut conn = self.conn.lock().unwrap();
        let top_k = top_k.max(1);

        // 空候选域：直接空结果（避免构造 UNION 空集）
        // Empty candidate scope: return no results (avoids building an empty UNION set)
        if let Some(ids) = candidate_ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
        }

        let terms: Vec<&str> = {
            let mut t: Vec<&str> = terms.iter().map(String::as_str).collect();
            if t.is_empty() {
                vec![""]
            } else {
                t.dedup();
                t
            }
        };

        // 每 term 一段 SELECT；子查询只带 LIMIT（各段去重上限），ORDER BY 在外层
        // UNION ALL 之后统一执行（SQLite 语法：UNION 内不能有 ORDER BY）。
        // One SELECT per term; subqueries only carry LIMIT (per-segment cap);
        // ORDER BY is applied once after the outer UNION ALL (SQLite forbids
        // ORDER BY inside a UNION).
        let mut selects: Vec<String> = Vec::with_capacity(terms.len());
        for term in terms {
            let is_long = term.chars().count() >= 3;
            let mut sql = String::new();
            if is_long {
                let match_expr = sq(&format!("\"{}\"", term.replace('"', "\"\"")));
                sql.push_str(&format!(
                    "SELECT p.page_id, p.entity_id, p.title, -bm25(pages_fts) AS score
                     FROM pages_fts f
                     JOIN pages p ON p.page_id = f.page_id
                     WHERE f.pages_fts MATCH {match_expr} AND p.status = 'accepted'"
                ));
            } else {
                let like = sq(&format!("%{term}%"));
                sql.push_str(&format!(
                    "SELECT p.page_id, p.entity_id, p.title, 1.0 AS score
                     FROM pages p
                     WHERE (p.title LIKE {like} OR p.content LIKE {like}) AND p.status = 'accepted'"
                ));
            }
            if let Some(d) = domain {
                sql.push_str(&format!(" AND p.domain = {}", sq(d)));
            }
            // 候选实体域白名单（引擎已预筛；这里不再重复查事实表）
            // Candidate entity-scope whitelist (already prefiltered by the engine;
            // no need to query the fact table again here)
            if let Some(ids) = candidate_ids {
                let keyed: Vec<String> = ids.iter().map(EntityId::to_key).collect();
                sql.push_str(" AND p.entity_id IN (");
                for (i, k) in keyed.iter().enumerate() {
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push_str(&sq(k));
                }
                sql.push(')');
            }
            // 事实平面过滤下推（category 锚点 join）
            // Fact-plane filter pushdown (category-anchor join)
            if let Some((fragment, fparams)) = facts::filter_where(filters)? {
                let fragment = inline_params(&fragment, &fparams);
                sql.push_str(&format!(
                    " AND p.entity_id IN (
                        SELECT DISTINCT cat.value_text
                        FROM facts cat
                        JOIN (SELECT DISTINCT entity_id FROM facts WHERE {fragment}) ft
                            ON ft.entity_id = cat.entity_id
                        WHERE cat.field_name = 'category' AND cat.field_type = 'text'
                    )"
                ));
            }
            // 无 ORDER BY 也无 LIMIT：UNION ALL 的子查询不能带 LIMIT（SQLite
            // 语法约束），排序与截断统一在外层完成。
            // No ORDER BY and no LIMIT here: UNION ALL subqueries cannot carry
            // LIMIT (SQLite constraint); sorting and truncation happen outside.
            selects.push(sql);
        }

        let sql = format!(
            "SELECT page_id, entity_id, title, score FROM ({}) ORDER BY score DESC LIMIT {top_k}",
            selects.join(" UNION ALL ")
        );
        let rows = diesel::sql_query(&sql).load::<SearchRow>(&mut *conn)?;

        // 按 page_id 取最高分（BM25 分数；LIKE 恒 1.0 不影响取 max 语义）
        // Keep the best score per page_id (BM25 scores; LIKE is always 1.0, so
        // multi-term max semantics are unaffected)
        let mut best: std::collections::HashMap<String, SearchHit> =
            std::collections::HashMap::new();
        for r in rows {
            let hit = SearchHit {
                page_id: r.page_id,
                entity_id: EntityId::from_key(&r.entity_id)?,
                score: r.score,
                title: r.title,
            };
            match best.get(&hit.page_id) {
                Some(prev) if prev.score >= hit.score => {}
                _ => {
                    best.insert(hit.page_id.clone(), hit);
                }
            }
        }
        let mut hits: Vec<SearchHit> = best.into_values().collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.page_id.cmp(&b.page_id))
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    /// 最小查询闭环：FTS5（长查询 MATCH / 短查询 LIKE）检索知识平面，
    /// 可选事实平面过滤下推（category 关联锚点），写查询日志。
    /// Minimal query loop: FTS5 (MATCH for long queries / LIKE for short ones)
    /// over the knowledge plane, optional fact-plane filter pushdown (category
    /// anchor join), and a query-log write.
    ///
    /// 过滤下推语义：FTS 命中页 → 事实平面过滤出满足条件的 SKU → 取其
    /// `category` 值集合（= 知识页 entity_id）→ `pages.entity_id IN (...)`。
    /// Filter-pushdown semantics: FTS hit pages → filter SKUs in the fact plane
    /// → collect their `category` values (= knowledge-page entity_id) →
    /// `pages.entity_id IN (...)`.
    pub fn search(
        &self,
        text: &str,
        filters: &Filters,
        top_k: usize,
        domain: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let started = std::time::Instant::now();
        let top_k = top_k.max(1);

        let query_json = serde_json::to_string(&Query {
            text: text.to_string(),
            filters: filters.clone(),
            top_k,
            domain: domain.map(str::to_string),
        })?;

        // 复用候选检索（内部锁 conn，不写日志）；随后单独锁 conn 写旧格式日志。
        // 注意不能在持有 conn 锁时再调 search_candidates——Mutex 非重入，会自锁死。
        // Reuse candidate retrieval (it locks the connection internally and writes no
        // log); then lock the connection separately to write the legacy-format log.
        // Never call search_candidates while holding the conn lock — Mutex is not
        // reentrant and would self-deadlock.
        let run = self.search_candidates(&[text.to_string()], filters, top_k, domain, None);

        let latency_ms = started.elapsed().as_millis() as i64;
        let log = |hit_count: i64| -> Result<()> {
            let mut conn = self.conn.lock().unwrap();
            diesel::sql_query(
                "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure, hit_count, latency_ms, timestamp)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
            )
            .bind::<diesel::sql_types::Text, _>(text)
            .bind::<diesel::sql_types::Text, _>(&query_json)
            .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(Option::<String>::None)
            .bind::<diesel::sql_types::Integer, _>(0_i32)
            .bind::<diesel::sql_types::BigInt, _>(hit_count)
            .bind::<diesel::sql_types::BigInt, _>(latency_ms)
            .bind::<diesel::sql_types::BigInt, _>(unix_now())
            .execute(&mut *conn)
            .map(|_| ())
            .map_err(Error::Database)
        };

        match run {
            Ok(hits) => {
                let _ = log(hits.len() as i64);
                Ok(hits)
            }
            Err(e) => {
                let _ = log(0);
                Err(e)
            }
        }
    }
}

fn establish(url: &str) -> Result<SqliteConnection> {
    SqliteConnection::establish(url).map_err(|e| {
        Error::Database(diesel::result::Error::QueryBuilderError(
            format!("connection error: {e}").into(),
        ))
    })
}

/// SQL 字符串字面量（单引号转义防注入）。用于 raw SQL 逃生路径的参数内联。
/// SQL string literal (single quotes escaped to prevent injection). Used to inline
/// parameters on the raw-SQL escape path.
fn sq(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// 把 `fragment` 中按序出现的 `?` 占位符替换为内联字符串字面量。
/// Replaces the `?` placeholders appearing in `fragment`, in order, with inline
/// string literals.
fn inline_params(fragment: &str, params: &[String]) -> String {
    let mut out = String::with_capacity(fragment.len() + params.len() * 8);
    let mut iter = params.iter();
    for (i, chunk) in fragment.split('?').enumerate() {
        if i > 0 {
            if let Some(p) = iter.next() {
                out.push_str(&sq(p));
            } else {
                out.push('?');
            }
        }
        out.push_str(chunk);
    }
    out
}

/// 当前 Unix 秒（kernel 模块内共用；编译管线 admit 无注入时钟时使用）。
/// Current Unix seconds (shared inside the kernel module; used by compile admit
/// which has no injected clock).
pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl EntityStore for SqliteKernel {
    async fn upsert_facts(&self, id: &EntityId, facts: &Facts, source_revision: u64) -> Result<()> {
        if id != &facts.entity_id {
            return Err(Error::Validation(format!(
                "upsert_facts id mismatch: {} vs {}",
                id.to_key(),
                facts.entity_id.to_key()
            )));
        }
        let mut conn = self.conn.lock().unwrap();
        let now = unix_now();
        // 事实 CAS 写入已抽取为 [`write_facts_cas`]（Step 4 admit 事务复用同一
        // 实现，保证 facts/fact_refs 语义单源）。
        // The fact CAS write is extracted into [`write_facts_cas`] (reused by the
        // Step 4 admit transaction so facts/fact_refs semantics have one source).
        conn.transaction(|tx| {
            super::compile_store::write_facts_cas(tx, facts, source_revision, now)
        })?;
        Ok(())
    }

    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>> {
        let mut conn = self.conn.lock().unwrap();
        if filters.is_empty() {
            // 空条件：返回全量（上限保护）
            // Empty condition: return everything (with an upper-bound guard)
            let rows: Vec<TextRow> =
                diesel::sql_query("SELECT DISTINCT entity_id AS value FROM facts LIMIT 10000")
                    .load(&mut *conn)?;
            return rows.iter().map(|r| EntityId::from_key(&r.value)).collect();
        }
        let (fragment, params) = facts::filter_where(filters)?
            .ok_or_else(|| Error::Internal("non-empty filters produced no where clause".into()))?;
        let fragment = inline_params(&fragment, &params);
        let sql =
            format!("SELECT DISTINCT entity_id AS value FROM facts WHERE {fragment} LIMIT 10000");
        let rows: Vec<TextRow> = diesel::sql_query(&sql).load(&mut *conn)?;
        rows.iter().map(|r| EntityId::from_key(&r.value)).collect()
    }

    async fn delete_facts(&self, id: &EntityId) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        conn.transaction(|tx| -> Result<()> {
            diesel::delete(fact_refs_t::table.filter(fact_refs_t::entity_id.eq(id.to_key())))
                .execute(tx)?;
            diesel::delete(facts_t::table.filter(facts_t::entity_id.eq(id.to_key())))
                .execute(tx)?;
            Ok(())
        })?;
        Ok(())
    }

    async fn get_facts(&self, id: &EntityId) -> Result<Option<Facts>> {
        let mut conn = self.conn.lock().unwrap();
        let key = id.to_key();
        let rows: Vec<(
            String,
            String,
            Option<f64>,
            Option<String>,
            Option<i64>,
            Option<i64>,
            i64,
        )> = facts_t::table
            .filter(facts_t::entity_id.eq(&key))
            .select((
                facts_t::field_name,
                facts_t::field_type,
                facts_t::value_numeric,
                facts_t::value_text,
                facts_t::value_boolean,
                facts_t::value_timestamp,
                facts_t::source_revision,
            ))
            .load::<(
                String,
                String,
                Option<f64>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                i64,
            )>(&mut *conn)?;

        let mut fields = BTreeMap::new();
        let mut revision = 0u64;
        let mut found = false;
        for (name, ftype, numeric, text, boolean, timestamp, rev) in rows {
            found = true;
            revision = rev as u64;
            let value = match ftype.as_str() {
                "numeric" => FactValue::Numeric(numeric.unwrap_or(0.0)),
                "text" => FactValue::Text(text.unwrap_or_default()),
                "boolean" => FactValue::Boolean(boolean.unwrap_or(0) != 0),
                "timestamp" => FactValue::Timestamp(timestamp.unwrap_or(0)),
                "reflist" => {
                    // reflist 值从 fact_refs 读
                    // reflist values are read back from fact_refs
                    let refs: Vec<String> = fact_refs_t::table
                        .filter(fact_refs_t::entity_id.eq(&key))
                        .filter(fact_refs_t::field_name.eq(&name))
                        .select(fact_refs_t::ref_value)
                        .order(fact_refs_t::ref_value.asc())
                        .load(&mut *conn)?;
                    FactValue::RefList(refs)
                }
                other => {
                    return Err(Error::Internal(format!(
                        "unknown field_type {other:?} in facts"
                    )))
                }
            };
            fields.insert(name, value);
        }
        if !found {
            return Ok(None);
        }
        Ok(Some(Facts {
            entity_id: id.clone(),
            fields,
            source_revision: revision,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 测试用版本行（读 `__diesel_schema_migrations.version`）。
    /// Test-only version row (reads `__diesel_schema_migrations.version`).
    #[derive(QueryableByName)]
    struct TestVersionRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    fn sample_page(entity_key: &str, title: &str, body: &str) -> WikiPage {
        crate::seed::parse_page(
            &format!(
                "---\npage_id: {entity_key}\nentity_id: {entity_key}\ntitle: {title}\nentity_type: drink\n---\n\n{body}"
            ),
        )
        .unwrap()
    }

    /// 搭一个 0001+0002 的 legacy 库文件（含版本行），供 open_existing 拒绝测试。
    /// Builds a legacy 0001+0002 database file (with version rows) for the
    /// open_existing rejection tests.
    fn legacy_db_file(dir: &Path) -> PathBuf {
        use diesel::connection::SimpleConnection;
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
        // 版本行从全新 migrate 的库里取（diesel 记录的版本串以它为准）。
        // Version rows come from a freshly migrated DB (diesel's recorded strings).
        let mut fresh = SqliteConnection::establish(":memory:").unwrap();
        schema::migrate(&mut fresh).unwrap();
        let versions: Vec<TestVersionRow> = diesel::sql_query(
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
        path
    }

    // A20：open_existing 对旧 schema 报 migration_required 且不升级（dry-run
    // 只读语义）；legacy 数据原样保留。
    // A20: open_existing reports migration_required on an old schema without
    // upgrading (dry-run read-only semantics); legacy data survives untouched.
    #[test]
    fn open_existing_refuses_legacy_schema_without_migrating() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_db_file(dir.path());
        // legacy 页数据
        // Legacy page data
        {
            let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
            diesel::sql_query(
                "INSERT INTO pages (page_id, entity_id, domain, entity_type, title, content,
                    content_hash, generation, status, domain_pack_version, compiled_at,
                    model_version, embedding_model, created_at, updated_at)
                 VALUES ('milk-tea:drink:legacy', 'milk-tea:drink:legacy', 'milk-tea', 'drink',
                    '乌龙奶茶', '乌龙奶茶是经典茶底。', 'h', 1, 'accepted', '0.1.0', 1, 'seed',
                    'none', 1, 1)",
            )
            .execute(&mut c)
            .unwrap();
        }

        let err = match SqliteKernel::open_existing(&path) {
            Err(e) => e,
            Ok(_) => panic!("open_existing must refuse a legacy schema"),
        };
        match &err {
            Error::InvalidConfig(msg) => {
                assert!(
                    msg.starts_with("migration_required:"),
                    "expected migration_required prefix, got {msg}"
                );
            }
            other => panic!("expected InvalidConfig(migration_required), got {other:?}"),
        }

        // 未升级：版本仍是 2，legacy 页保留。
        // Not upgraded: version stays 2 and the legacy page survives.
        let mut c = SqliteConnection::establish(path.to_str().unwrap()).unwrap();
        assert_eq!(schema::schema_version(&mut c).unwrap(), 2);
        let n: i64 = diesel::sql_query("SELECT COUNT(*) AS n FROM pages")
            .get_result::<CountRow>(&mut c)
            .unwrap()
            .n;
        assert_eq!(n, 1);
    }

    // A20：open_existing 对不存在的文件报错且不创建文件。
    // A20: open_existing errors on a missing file and never creates it.
    #[test]
    fn open_existing_missing_file_never_creates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-here.db");
        let err = match SqliteKernel::open_existing(&path) {
            Err(e) => e,
            Ok(_) => panic!("open_existing must refuse a missing file"),
        };
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(!path.exists(), "dry-run must not create the DB file");
    }

    // open_existing 对当前 schema 可用，只读路径（load_accepted_pages）正常。
    // open_existing works on a current-schema DB; read-only paths (e.g.
    // load_accepted_pages) function normally.
    #[test]
    fn open_existing_works_on_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.db");
        {
            let kernel = SqliteKernel::open(&path).unwrap();
            kernel
                .seed_pages(
                    &sample_page("milk-tea:drink:a", "乌龙奶茶", "乌龙奶茶是经典茶底。"),
                    "milk-tea",
                    PublishStatus::Accepted,
                )
                .unwrap();
        }
        let kernel = SqliteKernel::open_existing(&path).unwrap();
        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].wiki.page_id, "milk-tea:drink:a");
    }

    #[tokio::test]
    async fn upsert_facts_cas_old_revision_does_not_override() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let id = EntityId::new("ecommerce", "product", "1").unwrap();

        let mut f1 = Facts {
            entity_id: id.clone(),
            fields: BTreeMap::new(),
            source_revision: 1,
        };
        f1.fields.insert("price".into(), FactValue::Numeric(10.0));

        let mut f2 = Facts {
            entity_id: id.clone(),
            fields: BTreeMap::new(),
            source_revision: 2,
        };
        f2.fields.insert("price".into(), FactValue::Numeric(20.0));

        // 写入 rev=2
        // Write revision 2
        kernel.upsert_facts(&id, &f2, 2).await.unwrap();
        // 旧 rev=1 不得覆盖
        // The old revision 1 must not overwrite it
        kernel.upsert_facts(&id, &f1, 1).await.unwrap();

        let got = kernel.get_facts(&id).await.unwrap().unwrap();
        assert_eq!(got.fields.get("price"), Some(&FactValue::Numeric(20.0)));
        assert_eq!(got.source_revision, 2);
    }

    #[tokio::test]
    async fn filter_ref_contains_and_excludes() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let id_a = EntityId::new("ecommerce", "product", "a").unwrap();
        let id_b = EntityId::new("ecommerce", "product", "b").unwrap();

        let mut fa = Facts {
            entity_id: id_a.clone(),
            fields: BTreeMap::new(),
            source_revision: 1,
        };
        fa.fields.insert(
            "ingredient_ids".into(),
            FactValue::RefList(vec!["pearl".into(), "taro".into()]),
        );
        kernel.upsert_facts(&id_a, &fa, 1).await.unwrap();

        let mut fb = Facts {
            entity_id: id_b.clone(),
            fields: BTreeMap::new(),
            source_revision: 1,
        };
        fb.fields.insert(
            "ingredient_ids".into(),
            FactValue::RefList(vec!["taro".into()]),
        );
        kernel.upsert_facts(&id_b, &fb, 1).await.unwrap();

        let contains = kernel
            .filter(&Filters {
                conditions: vec![FilterCondition::RefContains {
                    field: "ingredient_ids".into(),
                    refs: vec!["pearl".into()],
                }],
            })
            .await
            .unwrap();
        assert_eq!(contains, vec![id_a.clone()]);

        let excludes = kernel
            .filter(&Filters {
                conditions: vec![FilterCondition::RefExcludes {
                    field: "ingredient_ids".into(),
                    refs: vec!["pearl".into()],
                }],
            })
            .await
            .unwrap();
        assert_eq!(excludes, vec![id_b.clone()]);
    }

    #[test]
    fn seed_pages_then_search_chinese() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let page = sample_page(
            "milk-tea:drink:boba",
            "波霸奶茶",
            "波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。\n\n## 成分\n\n- 红茶\n- 波霸珍珠\n- 鲜奶",
        );
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();

        // 长查询（≥3 字符）走 trigram MATCH
        // Long queries (≥3 characters) use trigram MATCH
        let hits = kernel
            .search("波霸奶茶", &Filters::empty(), 5, None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page_id, "milk-tea:drink:boba");
        assert!(hits[0].score > 0.0, "bm25 score must be positive");

        // 幂等：重复导入不产生重复页
        // Idempotent: repeated imports do not create duplicate pages
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["pages"], 1);
        // 页面 = 导语（概述）+ 成分，共 2 节；重复导入不翻倍
        // Page = intro (overview) + ingredients, two sections; repeated imports do not double them
        assert_eq!(counts["page_sections"], 2);
    }

    #[test]
    fn short_query_uses_like() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let page = sample_page(
            "milk-tea:drink:boba",
            "波霸奶茶",
            "珍珠软糯，茶味浓郁。\n\n## 成分\n\n- 波霸珍珠",
        );
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();

        // 短查询（<3 字符）走 LIKE，score 固定 1.0
        // Short queries (<3 characters) use LIKE, with a fixed score of 1.0
        let hits = kernel.search("珍珠", &Filters::empty(), 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].score, 1.0);
    }

    #[test]
    fn search_writes_query_logs() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        let page = sample_page(
            "milk-tea:drink:boba",
            "波霸奶茶",
            "波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。",
        );
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();
        let _ = kernel
            .search("波霸奶茶", &Filters::empty(), 5, None)
            .unwrap();
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["query_logs"], 1);
    }

    #[test]
    fn search_with_category_filter_pushdown() {
        let kernel = SqliteKernel::open_in_memory().unwrap();
        kernel
            .seed_pages(
                &sample_page(
                    "milk-tea:drink:boba",
                    "波霸奶茶",
                    "波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。",
                ),
                "milk-tea",
                PublishStatus::Accepted,
            )
            .unwrap();
        kernel
            .seed_pages(
                &sample_page(
                    "milk-tea:drink:lemon",
                    "柠檬茶",
                    "柠檬茶是清新酸甜的果茶，带柠檬香气。",
                ),
                "milk-tea",
                PublishStatus::Accepted,
            )
            .unwrap();

        // 两个 SKU：boba 页 category 低价 18 元；lemon 页 category 高价 25 元
        // Two SKUs: the boba page has category price 18; the lemon page category price is 25
        let sku = |id: &str, category: &str, price: f64| -> Facts {
            let mut f = Facts {
                entity_id: EntityId::new("milk-tea", "product", id).unwrap(),
                fields: BTreeMap::new(),
                source_revision: 1,
            };
            f.fields
                .insert("category".into(), FactValue::Text(category.into()));
            f.fields.insert("price".into(), FactValue::Numeric(price));
            f
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let a = sku("a", "milk-tea:drink:boba", 18.0);
            kernel.upsert_facts(&a.entity_id, &a, 1).await.unwrap();
            let b = sku("b", "milk-tea:drink:lemon", 25.0);
            kernel.upsert_facts(&b.entity_id, &b, 1).await.unwrap();
        });

        // "茶"（短查询 LIKE）→ 两页都命中；过滤 price<=20 → 只保留 boba 页
        // "茶" (short-query LIKE) hits both pages; price<=20 keeps only the boba page
        let hits = kernel
            .search(
                "茶",
                &Filters {
                    conditions: vec![FilterCondition::NumericRange {
                        field: "price".into(),
                        min: None,
                        max: Some(20.0),
                    }],
                },
                5,
                None,
            )
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.page_id.as_str()).collect();
        assert!(ids.contains(&"milk-tea:drink:boba"));
        assert!(!ids.contains(&"milk-tea:drink:lemon"));
    }
}
