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
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// 默认分页批量大小。
/// Default pagination batch size.
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// 单行字节上限（Step 4 §3.1：行 ≤256 KiB，超限受控报错；禁止 read_to_string
/// 整文件无界读取）。
/// Per-line byte cap (Step 4 §3.1: a line is ≤256 KiB; oversize fails in a
/// controlled way; `read_to_string` whole-file reads are forbidden).
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// 单次 fetch 总输入字节上限（Step 4 §3.1：≤8 MiB）。
/// Per-fetch total input byte cap (Step 4 §3.1: ≤8 MiB).
pub const MAX_FETCH_INPUT_BYTES: usize = 8 * 1024 * 1024;

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

        // 有界读取（§3.1）：禁止 read_to_string 整文件无界读取；逐行扫描，行
        // ≤256 KiB，单次 fetch 累计输入 ≤8 MiB，超限受控报错。
        // Bounded read (§3.1): whole-file `read_to_string` is forbidden; scan
        // line by line with a 256 KiB per-line cap and an 8 MiB per-fetch total
        // input cap; oversize fails in a controlled way.
        let file = std::fs::File::open(&self.path)?;
        let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
        let mut out = Vec::new();
        let mut line_no = 0usize;
        let mut total_bytes = 0usize;
        // 只统计非空行作为行号；跳过 offset 个有效行
        // Count only non-empty lines and skip `offset` valid records
        while let Some(line) = next_bounded_line(&mut reader, MAX_LINE_BYTES)? {
            let trimmed = trim_ascii(&line);
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
            // 单次 fetch 总输入上限（§3.1）：超出即协议错误，不静默截断。
            // Per-fetch total input cap (§3.1): exceeding it is a protocol error,
            // never a silent truncation.
            total_bytes = total_bytes.saturating_add(trimmed.len());
            if total_bytes > MAX_FETCH_INPUT_BYTES {
                return Err(Error::Validation(format!(
                    "{}:{line_no} fetch input exceeds {} bytes cap",
                    self.path.display(),
                    MAX_FETCH_INPUT_BYTES
                )));
            }
            // fail-fast 坏行
            // Fail fast on malformed lines
            let text = std::str::from_utf8(trimmed).map_err(|e| {
                Error::Validation(format!(
                    "{}:{line_no} invalid utf-8 line: {e}",
                    self.path.display()
                ))
            })?;
            let record: serde_json::Value = serde_json::from_str(text).map_err(|e| {
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

            // Step 4 §3.1：缺 revision 兼容默认 1；提供了非法/负数/非整数
            // revision 必须报错，禁止静默退为 1（A23）。
            // Step 4 §3.1: a missing revision stays compatible at 1; an illegal,
            // negative or non-integer revision must error out — never silently
            // fall back to 1 (A23).
            let source_revision = match record.get("source_revision") {
                None | Some(serde_json::Value::Null) => 1u64,
                Some(v) => {
                    let n = v.as_i64().ok_or_else(|| {
                        Error::Validation(format!(
                            "{}:{line_no} source_revision must be an integer in 1..=i64::MAX, got {v}",
                            self.path.display()
                        ))
                    })?;
                    if n < 1 {
                        return Err(Error::Validation(format!(
                            "{}:{line_no} source_revision must be >= 1, got {n}",
                            self.path.display()
                        )));
                    }
                    n as u64
                }
            };

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

/// 有界逐行读取（§3.1）：返回下一行（不含换行符）；流结束返回 None。缓冲最多
/// 保留 `max+1` 字节，行超限立即报错——内存占用有界，不因超长行无界增长。
/// Bounded line reading (§3.1): returns the next line (without the newline) or
/// `None` at end of stream. The buffer keeps at most `max+1` bytes and oversize
/// lines error out immediately, so memory stays bounded even on a pathological
/// multi-gigabyte line.
fn next_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // EOF：有残留（无换行结尾）则作为最后一行返回。
            // EOF: return any remainder (newline-terminated absent) as the last line.
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(buf))
            };
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                if buf.len() + pos > max {
                    return Err(Error::Validation(format!(
                        "jsonl line exceeds {max} bytes cap"
                    )));
                }
                buf.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                return Ok(Some(buf));
            }
            None => {
                // 只保留 max+1 字节即可判定超限；剩余 chunk 全部消费但不入缓冲。
                // Keep at most max+1 bytes to detect the cap; the rest of the
                // chunk is consumed but never buffered.
                let take = available.len().min(max.saturating_sub(buf.len()) + 1);
                buf.extend_from_slice(&available[..take]);
                let consumed = available.len();
                reader.consume(consumed);
                if buf.len() > max {
                    return Err(Error::Validation(format!(
                        "jsonl line exceeds {max} bytes cap"
                    )));
                }
            }
        }
    }
}

/// 去除首尾 ASCII 空白（行级 trim；不做 Unicode 归一化，§7 字符串不归一化）。
/// Trims leading/trailing ASCII whitespace (line-level trim; no Unicode
/// normalization, per §7 strings are never normalized).
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FieldDefinition, FieldType};

    fn write_lines(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        (dir, path)
    }

    fn source(path: &Path) -> JsonlDataSource {
        JsonlDataSource {
            path: path.to_path_buf(),
            schema: EntitySchema {
                entity_type: "drink".to_string(),
                fields: vec![FieldDefinition {
                    name: "name".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                }],
            },
        }
    }

    // A23：负数/非整数/零/越界 revision 一律报错，禁止静默退为 1。
    // A23: negative/non-integer/zero/out-of-range revisions must error out — never
    // silently fall back to 1.
    #[tokio::test]
    async fn invalid_revisions_rejected() {
        let cases = [
            r#"{"entity_id":"d:drink:a","name":"x","source_revision":-3}"#,
            r#"{"entity_id":"d:drink:a","name":"x","source_revision":0}"#,
            r#"{"entity_id":"d:drink:a","name":"x","source_revision":1.5}"#,
            r#"{"entity_id":"d:drink:a","name":"x","source_revision":"1"}"#,
            r#"{"entity_id":"d:drink:a","name":"x","source_revision":18446744073709551615}"#,
        ];
        for line in cases {
            let (_dir, path) = write_lines(&[line]);
            let ds = source(&path);
            let err = ds
                .fetch(Some(Cursor {
                    offset: 0,
                    batch_size: 10,
                }))
                .await
                .expect_err("invalid revision must fail");
            assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        }
    }

    // A23：缺 revision 兼容默认 1；合法 revision 原样保留。
    // A23: a missing revision stays compatible at 1; legal revisions are kept.
    #[tokio::test]
    async fn legal_revisions_pass_through() {
        let lines = [
            r#"{"entity_id":"d:drink:a","name":"x"}"#,
            r#"{"entity_id":"d:drink:b","name":"y","source_revision":42}"#,
            r#"{"entity_id":"d:drink:c","name":"z","source_revision":9223372036854775807}"#,
        ];
        let (_dir, path) = write_lines(&lines);
        let ds = source(&path);
        let batch = ds
            .fetch(Some(Cursor {
                offset: 0,
                batch_size: 10,
            }))
            .await
            .unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].source_revision, 1);
        assert_eq!(batch[1].source_revision, 42);
        assert_eq!(batch[2].source_revision, i64::MAX as u64);
    }

    // A23（§3.1）：行超 256 KiB → 受控报错，不做无界读取。
    // A23 (§3.1): a line over 256 KiB fails in a controlled way; no unbounded read.
    #[tokio::test]
    async fn oversize_line_rejected() {
        let long = "y".repeat(MAX_LINE_BYTES + 1);
        let line = format!(r#"{{"entity_id":"d:drink:a","name":"{long}"}}"#);
        let (_dir, p2) = write_lines(&[line.as_str()]);
        let ds = source(&p2);
        let err = ds
            .fetch(Some(Cursor {
                offset: 0,
                batch_size: 10,
            }))
            .await
            .expect_err("oversize line must fail");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("cap"));
    }

    // A23（§3.1）：单次 fetch 总输入超 8 MiB → 受控报错。
    // A23 (§3.1): per-fetch total input over 8 MiB fails in a controlled way.
    #[tokio::test]
    async fn fetch_total_input_cap_enforced() {
        // 每行约 640 KiB × 20 行 ≈ 12.5 MiB > 8 MiB；单行本身在 256 KiB 内？不——
        // 640 KiB 行会先触发行上限。改用多行小记录无法凑到 8 MiB 而测试太慢，
        // 因此直接对累计路径做单元验证：以 pad 字段把每行撑到 ~250 KiB（行内），
        // 40 行 ≈ 10 MiB 超总量上限且行内合法。
        // Each padded line is ~250 KiB (within the line cap); 40 lines ≈ 10 MiB
        // exceed the per-fetch total cap while every single line stays legal.
        let pad = "p".repeat(250 * 1024);
        let lines: Vec<String> = (0..40)
            .map(|i| format!(r#"{{"entity_id":"d:drink:e{i}","name":"{pad}"}}"#))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, path) = write_lines(&refs);
        let ds = source(&path);
        let err = ds
            .fetch(Some(Cursor {
                offset: 0,
                batch_size: 128,
            }))
            .await
            .expect_err("per-fetch total over 8MiB must fail");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("fetch input"));
    }

    // 有界分页：batch 截断 + offset 续读行为保持（供 cursor 循环消费）。
    // Bounded pagination: batch truncation plus offset continuation (consumed by
    // cursor loops) are preserved.
    #[tokio::test]
    async fn pagination_batch_and_offset_preserved() {
        let lines = [
            r#"{"entity_id":"d:drink:a","name":"x"}"#,
            r#"{"entity_id":"d:drink:b","name":"y"}"#,
            r#"{"entity_id":"d:drink:c","name":"z"}"#,
        ];
        let (_dir, path) = write_lines(&lines);
        let ds = source(&path);
        let first = ds
            .fetch(Some(Cursor {
                offset: 0,
                batch_size: 2,
            }))
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        let second = ds
            .fetch(Some(Cursor {
                offset: 2,
                batch_size: 2,
            }))
            .await
            .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id.to_key(), "d:drink:c");
    }

    // 行超限时缓冲有界：next_bounded_line 直接验证（max 内合法、超限报错）。
    // The oversize-line buffer stays bounded: verified directly against
    // next_bounded_line (legal within max, error beyond).
    #[test]
    fn next_bounded_line_caps_memory() {
        let small = b"a\nbb\nccc\n".as_slice();
        let mut r = std::io::BufReader::new(small);
        assert_eq!(next_bounded_line(&mut r, 8).unwrap(), Some(b"a".to_vec()));
        assert_eq!(next_bounded_line(&mut r, 8).unwrap(), Some(b"bb".to_vec()));
        // 超限（max=2 < "ccc"）→ 报错且不消费该行（fail-fast，调用方中止）。
        // Oversize (max=2 < "ccc") → error without consuming the line (fail-fast;
        // the caller aborts).
        assert!(next_bounded_line(&mut r, 2).is_err());
        // 行未被消费：max 足够时可重读。
        // The line was not consumed: re-readable with a sufficient max.
        assert_eq!(next_bounded_line(&mut r, 8).unwrap(), Some(b"ccc".to_vec()));
        // EOF 且无残留 → None
        // EOF with no remainder → None
        assert_eq!(next_bounded_line(&mut r, 8).unwrap(), None);
        // 无换行结尾的残留行
        // A trailing line without a newline
        let mut r = std::io::BufReader::new(b"tail".as_slice());
        assert_eq!(
            next_bounded_line(&mut r, 8).unwrap(),
            Some(b"tail".to_vec())
        );
        assert_eq!(next_bounded_line(&mut r, 8).unwrap(), None);
    }
}
