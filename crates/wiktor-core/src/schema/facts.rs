//! 事实平面查询辅助（source_revision CAS 语义 + 过滤下推翻译）。
//! Fact-plane query helpers (source_revision CAS semantics + filter pushdown translation).
//! DDL 见 `migrations/0001_create_core/up.sql`。
//! DDL lives in `migrations/0001_create_core/up.sql`.

use crate::types::error::Result;
use crate::types::{FactValue, FilterCondition, Filters};

/// 把 FactValue 展开为 (field_type, numeric, text, boolean, timestamp) 参数。
/// Expands a FactValue into (field_type, numeric, text, boolean, timestamp) columns.
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

/// 把 Filters 翻译成纯 WHERE 片段 `(条件 AND 条件...)` + 文本参数。
/// Translates Filters into a bare WHERE fragment `(cond AND cond...)` plus text params.
///
/// 返回 `None` 表示无过滤（调用方按全量处理）；`Some((fragment, params))`
/// 中参数按 fragment 内 `?` 出现的顺序排列，**全部字符串化**——SQLite 的
/// 类型亲和会把 `'20'` 这类文本在 `value_numeric`（REAL 列）比较时自动转
/// 数值，因此 diesel `bind::<Text>` 即可覆盖数值/文本/reflist 三类条件。
/// Returns `None` when there is no filtering (caller treats it as full scan);
/// in `Some((fragment, params))` the params follow the order of `?` marks in the
/// fragment and are **all stringified** — SQLite's type affinity coerces text
/// like `'20'` to a number when compared against `value_numeric` (a REAL column),
/// so diesel `bind::<Text>` covers numeric/text/reflist conditions alike.
///
/// 该片段可直接嵌入其它查询的 `WHERE` 子句或子查询（事实平面过滤下推复用）。
/// The fragment can be embedded directly into another query's `WHERE` clause or
/// subquery (reused by fact-plane filter pushdown).
pub fn filter_where(filters: &Filters) -> Result<Option<(String, Vec<String>)>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for cond in &filters.conditions {
        match cond {
            FilterCondition::NumericRange { field, min, max } => {
                // SQL 片段形如 `(field_name = ? AND value_numeric >= ? ...)`，
                // 参数必须按出现顺序入栈：field_name 在前，min/max 在后。
                // SQL fragment looks like `(field_name = ? AND value_numeric >= ? ...)`;
                // params must be pushed in occurrence order: field_name first,
                // min/max after.
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
                params.push(field.clone());
                if let Some(m) = min {
                    params.push(m.to_string());
                }
                if let Some(m) = max {
                    params.push(m.to_string());
                }
            }
            FilterCondition::TextEquals { field, value } => {
                clauses.push("(field_name = ? AND value_text = ?)".to_string());
                params.push(field.clone());
                params.push(value.clone());
            }
            FilterCondition::RefContains { field, refs } => {
                let marks = refs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                clauses.push(format!(
                    "EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN ({marks}))"
                ));
                params.push(field.clone());
                params.extend(refs.iter().cloned());
            }
            FilterCondition::RefExcludes { field, refs } => {
                let marks = refs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                clauses.push(format!(
                    "NOT EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN ({marks}))"
                ));
                params.push(field.clone());
                params.extend(refs.iter().cloned());
            }
        }
    }

    if clauses.is_empty() {
        return Ok(None);
    }

    Ok(Some((format!("({})", clauses.join(" AND ")), params)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_where_numeric_range() {
        let filters = Filters {
            conditions: vec![FilterCondition::NumericRange {
                field: "price".into(),
                min: Some(10.0),
                max: Some(20.0),
            }],
        };
        let (fragment, params) = filter_where(&filters).unwrap().unwrap();
        assert!(fragment.contains("value_numeric >= ?"));
        assert!(fragment.contains("value_numeric <= ?"));
        // 参数顺序：field_name, min, max
        // Param order: field_name, min, max
        assert_eq!(params, vec!["price", "10", "20"]);
    }

    #[test]
    fn filter_where_ref_contains_and_excludes() {
        let filters = Filters {
            conditions: vec![
                FilterCondition::RefContains {
                    field: "ingredient_ids".into(),
                    refs: vec!["pearl".into(), "taro".into()],
                },
                FilterCondition::RefExcludes {
                    field: "ingredient_ids".into(),
                    refs: vec!["jelly".into()],
                },
            ],
        };
        let (fragment, params) = filter_where(&filters).unwrap().unwrap();
        assert!(fragment.contains("fact_refs"));
        assert!(fragment.contains("NOT EXISTS"));
        // contains: field + 2 refs；excludes: field + 1 ref
        // contains: field + 2 refs; excludes: field + 1 ref
        assert_eq!(params.len(), 5);
    }

    #[test]
    fn filter_where_empty_returns_none() {
        assert!(filter_where(&Filters::empty()).unwrap().is_none());
    }

    #[test]
    fn filter_where_ignores_open_range() {
        // min/max 都缺省的 NumericRange 视为无约束，跳过
        // A NumericRange with both min and max absent is unconstrained; skip it
        let filters = Filters {
            conditions: vec![FilterCondition::NumericRange {
                field: "price".into(),
                min: None,
                max: None,
            }],
        };
        assert!(filter_where(&filters).unwrap().is_none());
    }
}
