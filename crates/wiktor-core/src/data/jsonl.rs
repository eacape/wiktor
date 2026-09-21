//! JSONL 文件数据源。
//! JSONL file data source.
//!
//! 每条记录是 JSON 行，必须含 `entity_id`（完整实体 key）；可选含
//! `source_revision`（缺省 1）；其余字段按 `EntitySchema.fields`
//! 的 [`FieldType`](crate::types::FieldType) 转成事实平面载荷。
//! Each record is one JSON line and must contain `entity_id` (a complete entity key);
//! `source_revision` is optional (defaults to 1); all other fields are converted into
//! fact-plane payloads according to [`FieldType`](crate::types::FieldType) in
//! `EntitySchema.fields`.

use crate::traits::EntityConfig;
use crate::traits::{DataSource, EntitySchema};
use crate::types::error::{Error, Result};
use crate::types::{Cursor, EntityId, FactValue, Facts, FieldDefinition, FieldType, RawEntity};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 默认分页批量大小。
/// Default pagination batch size.
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// JSONL 文件数据源（实现 [`DataSource`]）。
/// JSONL file data source (implements [`DataSource`]).
pub struct JsonlDataSource {
    path: PathBuf,
    schema: EntitySchema,
}

impl JsonlDataSource {
    /// 从 `jsonl://` URI 构造；相对路径基于 `base_dir` 解析。
    /// Constructs from a `jsonl://` URI; resolves relative paths against `base_dir`.
    pub fn from_config(cfg: &EntityConfig, base_dir: &Path) -> Result<Self> {
        let uri = cfg.source.strip_prefix("jsonl://").ok_or_else(|| {
            Error::InvalidConfig(format!(
                "source {:?} is not a jsonl:// URI (entity {})",
                cfg.source, cfg.name
            ))
        })?;
        let path = if Path::new(uri).is_absolute() {
            PathBuf::from(uri)
        } else {
            base_dir.join(uri)
        };
        Ok(Self {
            path,
            schema: EntitySchema {
                entity_type: cfg.name.clone(),
                fields: cfg.fields.clone(),
            },
        })
    }

    /// 文件路径（测试/诊断用）。
    /// File path (for tests and diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 按 Schema 把一条原始实体转成事实平面载荷（`Facts`）。
    /// Converts one raw entity into a fact-plane payload (`Facts`) according to the schema.
    ///
    /// 缺必填字段或类型不匹配 → `Error::Validation`（fail-fast，带 entity_id）。
    /// Missing required fields or type mismatches return `Error::Validation`
    /// (fail-fast, including the entity_id).
    pub fn raw_to_facts(&self, raw: &RawEntity) -> Result<Facts> {
        raw_to_facts(raw, &self.schema)
    }
}

/// 按 Schema 把 RawEntity 的 fields 按字段类型转成 `Facts` 载荷。
/// Converts RawEntity fields into a `Facts` payload by their schema-declared types.
pub fn raw_to_facts(raw: &RawEntity, schema: &EntitySchema) -> Result<Facts> {
    let mut fields: BTreeMap<String, FactValue> = BTreeMap::new();
    for fd in &schema.fields {
        let Some(value) = raw.fields.get(&fd.name) else {
            return Err(Error::Validation(format!(
                "entity {} missing required field {:?}",
                raw.id.to_key(),
                fd.name
            )));
        };
        let fv = json_value_to_fact(value, fd, &raw.id)?;
        fields.insert(fd.name.clone(), fv);
    }
    Ok(Facts {
        entity_id: raw.id.clone(),
        fields,
        source_revision: raw.source_revision,
    })
}

fn json_value_to_fact(
    value: &serde_json::Value,
    fd: &FieldDefinition,
    id: &EntityId,
) -> Result<FactValue> {
    let key = id.to_key();
    let fv = match fd.field_type {
        FieldType::Numeric => value.as_f64().map(FactValue::Numeric).ok_or_else(|| {
            Error::Validation(format!("entity {key} field {:?} must be number", fd.name))
        })?,
        FieldType::Text => value
            .as_str()
            .map(|s| FactValue::Text(s.to_string()))
            .ok_or_else(|| {
                Error::Validation(format!("entity {key} field {:?} must be string", fd.name))
            })?,
        FieldType::Boolean => value.as_bool().map(FactValue::Boolean).ok_or_else(|| {
            Error::Validation(format!("entity {key} field {:?} must be boolean", fd.name))
        })?,
        FieldType::RefList => {
            let arr = value.as_array().ok_or_else(|| {
                Error::Validation(format!("entity {key} field {:?} must be array", fd.name))
            })?;
            let refs = arr
                .iter()
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        Error::Validation(format!(
                            "entity {key} field {:?} ref must be string",
                            fd.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            FactValue::RefList(refs)
        }
        FieldType::Timestamp => value.as_i64().map(FactValue::Timestamp).ok_or_else(|| {
            Error::Validation(format!("entity {key} field {:?} must be integer", fd.name))
        })?,
    };
    Ok(fv)
}

#[async_trait]
impl DataSource for JsonlDataSource {
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>> {
        let (offset, batch) = match cursor {
            Some(c) => (c.offset, c.batch_size.max(1)),
            None => (0usize, DEFAULT_BATCH_SIZE),
        };
        if batch == 0 || offset >= usize::MAX / batch {
            return Ok(Vec::new());
        }

        let text = std::fs::read_to_string(&self.path)?;
        let mut out = Vec::new();
        let mut line_no = 0usize;
        // 只统计非空行作为行号；跳过 offset 个有效行
        // Count only non-empty lines and skip `offset` valid records
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            line_no += 1;
            if line_no <= offset {
                continue;
            }
            if out.len() >= batch {
                break;
            }
            // fail-fast 坏行
            // Fail fast on malformed lines
            let record: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
                Error::Validation(format!(
                    "{}:{} invalid json line: {e}",
                    self.path.display(),
                    line_no
                ))
            })?;

            let id_str = record
                .get("entity_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    Error::Validation(format!(
                        "{}:{line_no} missing/invalid entity_id",
                        self.path.display()
                    ))
                })?;
            let id = EntityId::from_key(id_str)?;

            let source_revision = record
                .get("source_revision")
                .and_then(|v| v.as_u64())
                .unwrap_or(1);

            let mut fields = BTreeMap::new();
            if let serde_json::Value::Object(map) = &record {
                for (k, v) in map {
                    if k == "entity_id" || k == "source_revision" {
                        continue;
                    }
                    fields.insert(k.clone(), v.clone());
                }
            }

            out.push(RawEntity {
                id,
                fields,
                source_revision,
            });
        }
        Ok(out)
    }

    fn schema(&self) -> EntitySchema {
        self.schema.clone()
    }
}
