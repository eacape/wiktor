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
        let mut conn = self.conn.lock().unwrap();
        let started = std::time::Instant::now();
        let top_k = top_k.max(1);

        // 长查询（≥3 字符）走 FTS5 trigram MATCH；短查询（<3 字符，如"珍珠"）走 LIKE 兜底。
        // Long queries (≥3 chars) use FTS5 trigram MATCH; short queries (<3 chars,
        // e.g. "珍珠") fall back to LIKE.
        let is_long = text.chars().count() >= 3;

        let mut sql = String::new();

        if is_long {
            // FTS5 短语查询：双引号包裹，内部双引号加倍转义
            // FTS5 phrase query: wrapped in double quotes, inner quotes escaped by doubling
            let match_expr = sq(&format!("\"{}\"", text.replace('"', "\"\"")));
            sql.push_str(&format!(
                "SELECT p.page_id, p.entity_id, p.title, -bm25(pages_fts) AS score
                 FROM pages_fts f
                 JOIN pages p ON p.page_id = f.page_id
                 WHERE f.pages_fts MATCH {match_expr} AND p.status = 'accepted'"
            ));
        } else {
            let like = sq(&format!("%{text}%"));
            sql.push_str(&format!(
                "SELECT p.page_id, p.entity_id, p.title, 1.0 AS score
                 FROM pages p
                 WHERE (p.title LIKE {like} OR p.content LIKE {like}) AND p.status = 'accepted'"
            ));
        }

        if let Some(d) = domain {
            sql.push_str(&format!(" AND p.domain = {}", sq(d)));
        }

        // 事实平面过滤下推：SKU 满足条件 → category 值集合 → 页 IN
        // Fact-plane filter pushdown: SKUs matching conditions → category value set → page IN
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

        // top_k 来自本进程整数（CLI 解析 usize），内联安全
        // top_k originates from an in-process integer (CLI parses usize), safe to inline
        sql.push_str(if is_long {
            " ORDER BY score DESC"
        } else {
            " ORDER BY p.page_id"
        });
        sql.push_str(&format!(" LIMIT {top_k}"));

        let query_json = serde_json::to_string(&Query {
            text: text.to_string(),
            filters: filters.clone(),
            top_k,
            domain: domain.map(str::to_string),
        })?;

        // 执行；失败也写日志（hit_count=0）再返回错误
        // Execute; on failure, still write the log (hit_count=0) before returning the error
        let run = diesel::sql_query(&sql)
            .load::<SearchRow>(&mut *conn)
            .map_err(Error::Database);

        let latency_ms = started.elapsed().as_millis() as i64;
        let mut log = |hit_count: i64| {
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
        };

        match run {
            Ok(rows) => {
                let hits: Vec<SearchHit> = rows
                    .into_iter()
                    .map(|r| {
                        Ok(SearchHit {
                            page_id: r.page_id,
                            entity_id: EntityId::from_key(&r.entity_id)?,
                            score: r.score,
                            title: r.title,
                        })
                    })
                    .collect::<Result<_>>()?;
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

fn unix_now() -> i64 {
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
        conn.transaction(|tx| -> Result<()> {
            for (field_name, value) in &facts.fields {
                let (field_type, numeric, text, boolean, timestamp) = facts::fact_columns(value);
                // CAS 生效判断：仅当本 revision 真正覆盖/新插入 facts 行（影响行数==1）时，
                // 才允许重写派生行 fact_refs——否则旧 revision 会绕过 CAS 污染 reflist。
                // 注：CAS 是核心语义（excluded.source_revision > facts.source_revision），
                // 走 raw SQL 逃生（diesel on_conflict do_update 的 WHERE 表达力不足）。
                // 本 SQL 是 CAS 唯一真相（原 schema::facts::SQL_UPSERT_FACT 参考常量已删）。
                // CAS-effect check: only when this revision truly overwrites/inserts a
                // facts row (affected rows == 1) may we rewrite the derived fact_refs rows —
                // otherwise an older revision would bypass CAS and pollute the reflist.
                // Note: CAS is the core semantics (excluded.source_revision > facts.source_revision)
                // and uses the raw-SQL escape hatch (diesel's on_conflict do_update WHERE
                // is not expressive enough). This SQL is the single source of truth for
                // CAS (the former schema::facts::SQL_UPSERT_FACT reference constant was removed).
                let applied = diesel::sql_query(
                    "INSERT INTO facts (entity_id, field_name, field_type, value_numeric, value_text, value_boolean, value_timestamp, source_revision, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                     ON CONFLICT(entity_id, field_name) DO UPDATE SET
                         field_type      = excluded.field_type,
                         value_numeric   = excluded.value_numeric,
                         value_text      = excluded.value_text,
                         value_boolean   = excluded.value_boolean,
                         value_timestamp = excluded.value_timestamp,
                         source_revision = excluded.source_revision,
                         updated_at      = excluded.updated_at
                     WHERE excluded.source_revision > facts.source_revision",
                )
                .bind::<diesel::sql_types::Text, _>(id.to_key())
                .bind::<diesel::sql_types::Text, _>(field_name)
                .bind::<diesel::sql_types::Text, _>(&field_type)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Double>, _>(numeric)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(text)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(boolean)
                .bind::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>, _>(timestamp)
                .bind::<diesel::sql_types::BigInt, _>(source_revision as i64)
                .bind::<diesel::sql_types::BigInt, _>(now)
                .execute(tx)?
                    == 1;

                // reflist 拆行到 fact_refs（同一事务，先删后插；仅 CAS 生效时执行）
                // Split reflist into fact_refs rows (same transaction, delete-then-insert;
                // only runs when CAS applied)
                if applied {
                    if let FactValue::RefList(refs) = value {
                        diesel::delete(
                            fact_refs_t::table
                                .filter(fact_refs_t::entity_id.eq(id.to_key()))
                                .filter(fact_refs_t::field_name.eq(field_name)),
                        )
                        .execute(tx)?;
                        for r in refs {
                            diesel::insert_into(fact_refs_t::table)
                                .values((
                                    fact_refs_t::entity_id.eq(id.to_key()),
                                    fact_refs_t::field_name.eq(field_name),
                                    fact_refs_t::ref_value.eq(r),
                                ))
                                .execute(tx)?;
                        }
                    }
                }
            }
            Ok(())
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

    fn sample_page(entity_key: &str, title: &str, body: &str) -> WikiPage {
        crate::seed::parse_page(
            &format!(
                "---\npage_id: {entity_key}\nentity_id: {entity_key}\ntitle: {title}\nentity_type: drink\n---\n\n{body}"
            ),
        )
        .unwrap()
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
