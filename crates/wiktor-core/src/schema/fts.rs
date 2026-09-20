//! FTS5 全文检索辅助（BM25 倒排，查询入口）。
//! DDL / 触发器见 `migrations::MIGRATION_0001`。

use rusqlite::{Connection, Result};

/// 对知识平面做 BM25 检索，返回 (page_id, entity_id, bm25_score)。
/// `query` 使用 FTS5 标准查询语法（MVP 不做中文分词定制）。
/// 注：Step 2 查询闭环将使用本函数，当前阶段允许 dead_code。
#[allow(dead_code)]
pub fn bm25_search(
    conn: &Connection,
    query: &str,
    top_k: usize,
) -> Result<Vec<(String, String, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT page_id, entity_id, bm25(pages_fts) AS score
         FROM pages_fts
         WHERE pages_fts MATCH ?1
         ORDER BY score
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![query, top_k as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
