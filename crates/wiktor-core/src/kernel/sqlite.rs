use crate::schema::{self, facts};
use crate::traits::EntityStore;
use crate::types::error::{Error, Result};
use crate::types::{EntityId, FactValue, Facts, FilterCondition, Filters};
use async_trait::async_trait;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

/// SQLite 内核：两平面 + FTS5 + 任务队列 + 查询日志，单连接（MVP 单写者）。
pub struct SqliteKernel {
    conn: Mutex<Connection>,
}

impl SqliteKernel {
    /// 打开（不存在则创建）并应用迁移。
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 当前 schema 版本（MAX(schema_migrations.version)）。
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let v: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    /// 各核心表的行数。
    pub fn row_counts(&self) -> Result<BTreeMap<String, i64>> {
        let conn = self.conn.lock().unwrap();
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
            let sql = format!("SELECT COUNT(*) FROM {t}");
            let n: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
            out.insert(t.to_string(), n);
        }
        Ok(out)
    }

    /// 直接执行（供 CLI/工具使用）。
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql)?;
        Ok(())
    }
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
        let conn = self.conn.lock().unwrap();
        let now = unix_now();
        let tx = conn.unchecked_transaction()?;

        for (field_name, value) in &facts.fields {
            let (field_type, numeric, text, boolean, timestamp) = facts::fact_columns(value);
            // CAS 生效判断：仅当本 revision 真正覆盖/新插入 facts 行（changes()==1）时，
            // 才允许重写派生行 fact_refs——否则旧 revision 会绕过 CAS 污染 reflist。
            let applied = tx.execute(
                facts::SQL_UPSERT_FACT,
                params![
                    id.to_key(),
                    field_name,
                    field_type,
                    numeric,
                    text,
                    boolean,
                    timestamp,
                    source_revision as i64,
                    now
                ],
            )? == 1;

            // reflist 拆行到 fact_refs（同一事务，先删后插；仅 CAS 生效时执行）
            if applied {
                if let FactValue::RefList(refs) = value {
                    tx.execute(
                        "DELETE FROM fact_refs WHERE entity_id = ?1 AND field_name = ?2",
                        params![id.to_key(), field_name],
                    )?;
                    for r in refs {
                        tx.execute(
                            "INSERT OR IGNORE INTO fact_refs (entity_id, field_name, ref_value) VALUES (?1, ?2, ?3)",
                            params![id.to_key(), field_name, r],
                        )?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>> {
        let conn = self.conn.lock().unwrap();
        if filters.is_empty() {
            // 空条件：返回全量（上限保护）
            let mut stmt = conn.prepare("SELECT DISTINCT entity_id FROM facts LIMIT 10000")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(EntityId::from_key(&r?)?);
            }
            return Ok(out);
        }
        let (sql, params) = facts::translate_filters(&conn, filters)?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(EntityId::from_key(&r?)?);
        }
        Ok(out)
    }

    async fn delete_facts(&self, id: &EntityId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM fact_refs WHERE entity_id = ?1",
            params![id.to_key()],
        )?;
        tx.execute(
            "DELETE FROM facts WHERE entity_id = ?1",
            params![id.to_key()],
        )?;
        tx.commit()?;
        Ok(())
    }

    async fn get_facts(&self, id: &EntityId) -> Result<Option<Facts>> {
        let conn = self.conn.lock().unwrap();
        let key = id.to_key();
        let mut stmt = conn.prepare(
            "SELECT field_name, field_type, value_numeric, value_text, value_boolean, value_timestamp, source_revision
             FROM facts WHERE entity_id = ?1",
        )?;
        let rows = stmt.query_map(params![key], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;

        let mut fields = BTreeMap::new();
        let mut revision = 0u64;
        let mut found = false;
        for r in rows {
            let (name, ftype, numeric, text, boolean, timestamp, rev) = r?;
            found = true;
            revision = rev as u64;
            let value = match ftype.as_str() {
                "numeric" => FactValue::Numeric(numeric.unwrap_or(0.0)),
                "text" => FactValue::Text(text.unwrap_or_default()),
                "boolean" => FactValue::Boolean(boolean.unwrap_or(0) != 0),
                "timestamp" => FactValue::Timestamp(timestamp.unwrap_or(0)),
                "reflist" => {
                    // reflist 值从 fact_refs 读
                    let mut rstmt = conn.prepare(
                        "SELECT ref_value FROM fact_refs WHERE entity_id = ?1 AND field_name = ?2 ORDER BY ref_value",
                    )?;
                    let rrows = rstmt.query_map(params![key, name], |r| r.get::<_, String>(0))?;
                    let mut refs = Vec::new();
                    for rr in rrows {
                        refs.push(rr?);
                    }
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

// 保留 import：FilterCondition 供后续过滤扩展与 trait 方法签名使用。
#[allow(unused_imports)]
use FilterCondition as _FilterConditionAlias;

#[cfg(test)]
mod tests {
    use super::*;

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
        kernel.upsert_facts(&id, &f2, 2).await.unwrap();
        // 旧 rev=1 不得覆盖
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
}
