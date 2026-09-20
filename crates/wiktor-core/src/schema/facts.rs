//! 事实平面查询辅助（source_revision CAS + 过滤下推翻译）。
//! DDL 见 `migrations::MIGRATION_0001`。

use crate::types::{FactValue, FilterCondition, Filters};
use rusqlite::types::Value;
use rusqlite::Connection;

/// 单语句 CAS：`excluded.source_revision > facts.source_revision` 时才覆盖。
pub const SQL_UPSERT_FACT: &str = r#"
INSERT INTO facts (entity_id, field_name, field_type, value_numeric, value_text, value_boolean, value_timestamp, source_revision, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
ON CONFLICT(entity_id, field_name) DO UPDATE SET
    field_type      = excluded.field_type,
    value_numeric   = excluded.value_numeric,
    value_text      = excluded.value_text,
    value_boolean   = excluded.value_boolean,
    value_timestamp = excluded.value_timestamp,
    source_revision = excluded.source_revision,
    updated_at      = excluded.updated_at
WHERE excluded.source_revision > facts.source_revision
"#;

/// 把 FactValue 展开为 (field_type, numeric, text, boolean, timestamp) 参数。
pub fn fact_columns(
    value: &FactValue,
) -> (
    String,
    Option<f64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
) {
    match value {
        FactValue::Numeric(v) => ("numeric".into(), Some(*v), None, None, None),
        FactValue::Text(v) => ("text".into(), None, Some(v.clone()), None, None),
        FactValue::Boolean(v) => ("boolean".into(), None, None, Some(*v as i64), None),
        FactValue::RefList(_) => ("reflist".into(), None, None, None, None),
        FactValue::Timestamp(v) => ("timestamp".into(), None, None, None, Some(*v)),
    }
}

/// 把 Filters 翻译成 `(WHERE 片段, 参数)`；空条件返回 None（调用方按全量处理）。
pub fn translate_filters(
    conn: &Connection,
    filters: &Filters,
) -> rusqlite::Result<(String, Vec<Value>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    for cond in &filters.conditions {
        match cond {
            FilterCondition::NumericRange { field, min, max } => {
                // SQL 片段形如 `(field_name = ? AND value_numeric >= ? ...)`，
                // 参数必须按出现顺序入栈：field_name 在前，min/max 在后。
                if min.is_none() && max.is_none() {
                    continue;
                }
                clauses.push(format!(
                    "(field_name = ? AND {})",
                    [
                        min.map(|_| "value_numeric >= ?"),
                        max.map(|_| "value_numeric <= ?")
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" AND ")
                ));
                params.push(Value::Text(field.clone()));
                if let Some(m) = min {
                    params.push(Value::Real(*m));
                }
                if let Some(m) = max {
                    params.push(Value::Real(*m));
                }
            }
            FilterCondition::TextEquals { field, value } => {
                clauses.push("(field_name = ? AND value_text = ?)".to_string());
                params.push(Value::Text(field.clone()));
                params.push(Value::Text(value.clone()));
            }
            FilterCondition::RefContains { field, refs } => {
                let marks = refs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                clauses.push(format!(
                    "EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN ({marks}))"
                ));
                params.push(Value::Text(field.clone()));
                for r in refs {
                    params.push(Value::Text(r.clone()));
                }
            }
            FilterCondition::RefExcludes { field, refs } => {
                let marks = refs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                clauses.push(format!(
                    "NOT EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN ({marks}))"
                ));
                params.push(Value::Text(field.clone()));
                for r in refs {
                    params.push(Value::Text(r.clone()));
                }
            }
        }
    }

    if clauses.is_empty() {
        return Ok((String::new(), params));
    }

    let sql = format!(
        "SELECT DISTINCT entity_id FROM facts WHERE {} LIMIT 10000",
        clauses.join(" AND ")
    );
    let _ = conn; // 保留 conn 参数：SQL 复用点（如后续走 prepare）在此扩展
    Ok((sql, params))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::migrations::migrate;

    #[test]
    fn translates_numeric_range() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let filters = Filters {
            conditions: vec![FilterCondition::NumericRange {
                field: "sugar_level".into(),
                min: Some(0.0),
                max: Some(30.0),
            }],
        };
        let (sql, params) = translate_filters(&conn, &filters).unwrap();
        assert!(sql.contains("value_numeric >= ?"));
        assert!(sql.contains("value_numeric <= ?"));
        assert_eq!(params.len(), 3); // min, max, field_name
    }

    #[test]
    fn translates_ref_contains() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let filters = Filters {
            conditions: vec![FilterCondition::RefContains {
                field: "ingredient_ids".into(),
                refs: vec!["pearl".into()],
            }],
        };
        let (sql, params) = translate_filters(&conn, &filters).unwrap();
        assert!(sql.contains("fact_refs"));
        assert_eq!(params.len(), 2); // field_name, ref_value
    }
}
