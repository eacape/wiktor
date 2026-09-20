//! Command-line filter parsing.
//! 命令行过滤条件解析。
//!
//! Translates a comma-separated `--filter` string into [`Filters`].
//! 把逗号分隔的 `--filter` 字符串翻译成 [`Filters`]。
//!
//! Supported operators / 支持的运算符：
//! ```text
//! price<=20       NumericRange { max: 20 }
//! price>=15       NumericRange { min: 15 }
//! size=中杯        TextEquals
//! ing in=a|b      RefContains (list separated by '|')
//! ing not_in=a    RefExcludes
//! on_sale=true    error: boolean filtering is not supported yet
//! ```
//! Note: `,` separates conditions; `|` separates items inside `in=`/`not_in=`.
//! 注意：`，` 分隔条件；`|` 分隔 `in=`/`not_in=` 列表项。

use anyhow::{anyhow, Result};
use wiktor_core::types::{FilterCondition, Filters};

/// Parse a `--filter` string into [`Filters`].
/// 把 `--filter` 字符串解析为 [`Filters`]。
///
/// Empty / whitespace-only input yields empty filters (no conditions).
/// 空字符串/纯空白输入返回空过滤条件。
pub fn parse_filter(spec: &str) -> Result<Filters> {
    let mut conditions = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        conditions.push(parse_one(part)?);
    }
    Ok(Filters { conditions })
}

fn parse_one(part: &str) -> Result<FilterCondition> {
    // Numeric range: field<=max or field>=min (checked first, longest match first)
    // 数值区间：field<=max 或 field>=min（先匹配较长运算符）
    if let Some((field, val)) = part.split_once("<=") {
        let max: f64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow!("invalid numeric value for '<=': {val:?}"))?;
        return Ok(FilterCondition::NumericRange {
            field: field.trim().to_string(),
            min: None,
            max: Some(max),
        });
    }
    if let Some((field, val)) = part.split_once(">=") {
        let min: f64 = val
            .trim()
            .parse()
            .map_err(|_| anyhow!("invalid numeric value for '>=': {val:?}"))?;
        return Ok(FilterCondition::NumericRange {
            field: field.trim().to_string(),
            min: Some(min),
            max: None,
        });
    }
    // Ref list: field in=a|b / field not_in=a (items separated by '|')
    // 引用列表：field in=a|b / field not_in=a（列表项用 '|' 分隔）
    if let Some((field, val)) = part.split_once("not_in=") {
        let refs = val
            .trim()
            .split('|')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        return Ok(FilterCondition::RefExcludes {
            field: field.trim().to_string(),
            refs,
        });
    }
    if let Some((field, val)) = part.split_once("in=") {
        let refs = val
            .trim()
            .split('|')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        return Ok(FilterCondition::RefContains {
            field: field.trim().to_string(),
            refs,
        });
    }
    // Exact text equals: field=value
    // 文本相等：field=value
    if let Some((field, val)) = part.split_once('=') {
        let value = val.trim();
        if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
            return Err(anyhow!(
                "boolean filtering is not supported yet; got `{part}` \
                 (boolean 过滤暂不支持)"
            ));
        }
        return Ok(FilterCondition::TextEquals {
            field: field.trim().to_string(),
            value: value.to_string(),
        });
    }
    Err(anyhow!("cannot parse filter condition: {part:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_ranges() {
        let f = parse_filter("price<=20,price>=15").unwrap();
        assert_eq!(f.conditions.len(), 2);
        assert!(matches!(
            &f.conditions[0],
            FilterCondition::NumericRange {
                max: Some(20.0),
                min: None,
                ..
            }
        ));
        assert!(matches!(
            &f.conditions[1],
            FilterCondition::NumericRange {
                min: Some(15.0),
                max: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_text_equals() {
        let f = parse_filter("size=中杯").unwrap();
        assert!(matches!(
            &f.conditions[0],
            FilterCondition::TextEquals { field, value } if field == "size" && value == "中杯"
        ));
    }

    #[test]
    fn parses_ref_in_and_not_in() {
        let f = parse_filter("ingredient_ids in=pearl|taro").unwrap();
        assert!(matches!(
            &f.conditions[0],
            FilterCondition::RefContains { field, refs } if field == "ingredient_ids"
                && refs == &vec!["pearl".to_string(), "taro".to_string()]
        ));

        let f = parse_filter("ingredient_ids not_in=pearl").unwrap();
        assert!(matches!(
            &f.conditions[0],
            FilterCondition::RefExcludes { field, refs } if field == "ingredient_ids"
                && refs == &vec!["pearl".to_string()]
        ));
    }

    #[test]
    fn rejects_boolean_filter() {
        assert!(parse_filter("on_sale=true").is_err());
    }

    #[test]
    fn empty_input_is_empty_filters() {
        let f = parse_filter("").unwrap();
        assert!(f.conditions.is_empty());
    }
}
