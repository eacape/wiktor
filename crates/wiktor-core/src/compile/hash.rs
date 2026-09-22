//! 全依赖哈希（Step 4 spec §7，决策 D6）。
//! All-dependency hashing (Step 4 spec §7, decision D6).
//!
//! 设计要点：
//! - `canonical_json`：递归规范化字节编码。对象键按 UTF-8 字节序递归排序，数组
//!   严格保序，字符串不 trim、不做 Unicode 归一化；每个值/字符串/键都以
//!   `u64 little-endian` 长度域前导（长度域消除拼接碰撞），数值沿用
//!   `serde_json::Number` 的稳定文本（`1` 与 `1.0` 可不同），拒绝非有限值。
//! - `content_hash`：固定前缀 `wiktor.compile.hash.v1\0`，域定长帧（域名本身也
//!   带 u64 LE 长度域），域序固定；`source_revision` 是 CAS 身份，不入语义哈希。
//! - `snapshot_hash`：完整 RawEntity 的 BLAKE3，用于“同 revision 不同内容”冲突检测。
//!
//! Design highlights:
//! - `canonical_json`: recursive canonical byte encoding. Object keys are sorted
//!   recursively by UTF-8 byte order, arrays keep strict order, strings are neither
//!   trimmed nor Unicode-normalized; every value/string/key is prefixed with a
//!   `u64 little-endian` length field (length framing eliminates concatenation
//!   collisions); numbers use serde_json::Number's stable text (`1` and `1.0`
//!   may differ) and non-finite values are rejected.
//! - `content_hash`: fixed prefix `wiktor.compile.hash.v1\0`, length-framed
//!   domains (domain names themselves carry a u64 LE length prefix) in fixed
//!   order; `source_revision` is CAS identity and never enters the semantic hash.
//! - `snapshot_hash`: BLAKE3 over the full RawEntity, detecting same-revision
//!   content conflicts.

use crate::compile::config::{CompilePolicy, COMPILE_TEMPERATURE};
use crate::traits::EntitySchema;
use crate::types::error::{Error, Result};
use crate::types::{CompileContext, RawEntity};
use serde_json::Value;
use std::collections::BTreeMap;

/// 哈希格式版本前缀；serde_json 升级或编码变更必须换版本并做黄金哈希回归。
/// Hash-format version prefix; serde_json upgrades or encoding changes must bump
/// the version and run golden-hash regression.
pub const HASH_PREFIX: &[u8] = b"wiktor.compile.hash.v1\0";

// canonical_json 的类型标签（自描述，防跨类型拼接碰撞）。
// Type tags of canonical_json (self-describing, preventing cross-type splice collisions).
const TAG_NULL: u8 = 0x00;
const TAG_FALSE: u8 = 0x01;
const TAG_TRUE: u8 = 0x02;
const TAG_NUMBER: u8 = 0x03;
const TAG_STRING: u8 = 0x04;
const TAG_ARRAY: u8 = 0x05;
const TAG_OBJECT: u8 = 0x06;

/// 把 JSON Value 编码为规范化字节序列（键序稳定、数组保序、长度域防碰撞）。
/// Encodes a JSON Value into canonical bytes (stable key order, array order
/// preserved, length-prefixed against collisions).
pub fn canonical_json(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_u64(out: &mut Vec<u8>, n: usize) {
    // 长度域统一 u64 little-endian（§7）。
    // Length fields are uniformly u64 little-endian (§7).
    out.extend_from_slice(&(n as u64).to_le_bytes());
}

fn write_value(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(true) => out.push(TAG_TRUE),
        Value::Bool(false) => out.push(TAG_FALSE),
        Value::Number(n) => {
            // 拒绝非有限数值；不 clamp、不静默转 null。
            // Reject non-finite numbers; no clamping, no silent null.
            if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(Error::Validation(
                        "canonical_json rejects non-finite numbers".into(),
                    ));
                }
            }
            out.push(TAG_NUMBER);
            let text = n.to_string();
            write_u64(out, text.len());
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            write_u64(out, s.len());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            out.push(TAG_ARRAY);
            write_u64(out, items.len());
            for item in items {
                write_value(out, item)?;
            }
        }
        Value::Object(map) => {
            out.push(TAG_OBJECT);
            write_u64(out, map.len());
            // serde_json::Map 默认即 BTreeMap（字节序），显式排序避免依赖实现细节。
            // serde_json::Map is a BTreeMap (byte order) by default; sort explicitly
            // to avoid relying on that implementation detail.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                write_u64(out, key.len());
                out.extend_from_slice(key.as_bytes());
                write_value(out, &map[key])?;
            }
        }
    }
    Ok(())
}

/// content_hash 的全部输入（§7）；`source` 指投影后的知识快照。
/// All inputs of content_hash (§7); `source` refers to the projected knowledge snapshot.
pub struct HashDependencies<'a> {
    pub source: &'a RawEntity,
    pub context: &'a CompileContext,
    pub policy: &'a CompilePolicy,
    pub source_schema: &'a EntitySchema,
}

/// 全依赖内容哈希：小写 64 位 BLAKE3 hex（§7）。
/// All-dependency content hash: lowercase 64-hex BLAKE3 (§7).
///
/// 域序固定：`source`、`domain_pack_version`、`prompt_template`、
/// `compiler_version`、`model_version`、`embedding_model`、`artifact_version`、
/// `scorer_version`、`quality_policy`、`knowledge_schema`。
/// Fixed domain order: `source`, `domain_pack_version`, `prompt_template`,
/// `compiler_version`, `model_version`, `embedding_model`, `artifact_version`,
/// `scorer_version`, `quality_policy`, `knowledge_schema`.
pub fn content_hash(input: HashDependencies<'_>) -> Result<String> {
    // source 域 = canonical {entity_id, fields}；source_revision 不入语义哈希。
    // source domain = canonical {entity_id, fields}; source_revision is excluded
    // from the semantic hash.
    let mut source_fields: BTreeMap<String, Value> = BTreeMap::new();
    for (name, value) in &input.source.fields {
        source_fields.insert(name.clone(), value.clone());
    }
    let source_value = serde_json::json!({
        "entity_id": input.source.id.to_key(),
        "fields": source_fields,
    });

    // quality_policy：阈值/引用要求/覆盖度/密度/字段列表/标题/生成参数；
    // 预算、lease、时钟、usage、重试次数不入哈希（§7）。
    // quality_policy: thresholds/ref requirement/coverage/density/field lists/
    // headings/generation params; budgets, lease, clock, usage and retry counts
    // are excluded from the hash (§7).
    let quality_policy = serde_json::json!({
        "quality_threshold": input.context.quality_threshold,
        "require_source_refs": input.context.require_source_refs,
        "min_coverage": input.policy.min_coverage,
        "min_density": input.policy.min_density,
        "knowledge_fields": input.policy.knowledge_fields,
        "sensitive_fields": input.policy.sensitive_fields,
        "required_headings": input.policy.required_headings,
        "temperature": COMPILE_TEMPERATURE,
        "max_output_tokens": input.policy.max_output_tokens,
    });
    let knowledge_schema = schema_to_value(input.source_schema);

    // 文本域直接写 UTF-8 bytes（域名带长度域，无拼接歧义）；路径字符串不入哈希。
    // Text domains are written as raw UTF-8 bytes (length-framed domain names, no
    // splice ambiguity); path strings are excluded from the hash.
    let domains: [(&str, Vec<u8>); 10] = [
        ("source", canonical_json(&source_value)?),
        (
            "domain_pack_version",
            input.context.domain_pack_version.as_bytes().to_vec(),
        ),
        (
            "prompt_template",
            input.context.prompt_template.as_bytes().to_vec(),
        ),
        (
            "compiler_version",
            input.policy.compiler_version.as_bytes().to_vec(),
        ),
        (
            "model_version",
            input.context.model_version.as_bytes().to_vec(),
        ),
        (
            "embedding_model",
            input.context.embedding_model.as_bytes().to_vec(),
        ),
        (
            "artifact_version",
            input.policy.artifact_version.as_bytes().to_vec(),
        ),
        (
            "scorer_version",
            input.policy.scorer_version.as_bytes().to_vec(),
        ),
        ("quality_policy", canonical_json(&quality_policy)?),
        ("knowledge_schema", canonical_json(&knowledge_schema)?),
    ];

    let mut framed = Vec::from(HASH_PREFIX);
    for (name, bytes) in domains {
        write_u64(&mut framed, name.len());
        framed.extend_from_slice(name.as_bytes());
        write_u64(&mut framed, bytes.len());
        framed.extend_from_slice(&bytes);
    }
    Ok(blake3::hash(&framed).to_hex().to_string())
}

/// EntitySchema → 稳定 JSON（name/field_type/filterable 保序声明序）。
/// EntitySchema → stable JSON (name/field_type/filterable in declaration order).
///
/// crate 内复用：kernel 把 knowledge_schema 存进 compile_tasks.dependencies_json
/// （Step 4 §7 步骤 3）。
/// Reused crate-internally: the kernel stores knowledge_schema inside
/// compile_tasks.dependencies_json (Step 4 §7 item 3).
pub(crate) fn schema_to_value(schema: &EntitySchema) -> Value {
    let fields: Vec<Value> = schema
        .fields
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "field_type": f.field_type,
                "filterable": f.filterable,
            })
        })
        .collect();
    serde_json::json!({
        "entity_type": schema.entity_type,
        "fields": fields,
    })
}

/// 完整源快照哈希：`BLAKE3(canonical(full RawEntity))`，供同 revision 不同内容
/// 冲突检测；price/stock 只改变 snapshot_hash，不改变 content_hash（§7）。
/// Full source-snapshot hash: `BLAKE3(canonical(full RawEntity))` for
/// same-revision conflict detection; price/stock changes only move
/// snapshot_hash, never content_hash (§7).
pub fn snapshot_hash(raw: &RawEntity) -> Result<String> {
    let value = serde_json::to_value(raw)?;
    let bytes = canonical_json(&value)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EntityId, FieldDefinition, FieldType};
    use std::collections::BTreeMap;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    // A4：嵌套对象键置换 → 同一规范化字节与哈希。
    // A4: nested key permutation → identical canonical bytes and hash.
    #[test]
    fn key_permutation_is_irrelevant() {
        let a = parse(r#"{"b":1,"a":{"d":"x","c":[1,2]}}"#);
        let b = parse(r#"{"a":{"c":[1,2],"d":"x"},"b":1}"#);
        assert_eq!(canonical_json(&a).unwrap(), canonical_json(&b).unwrap());
    }

    // A4：数组顺序参与哈希；字符串空白参与哈希；JSON 排版空白无关。
    // A4: array order and string whitespace affect the hash; JSON formatting
    // whitespace does not.
    #[test]
    fn order_and_whitespace_sensitivity() {
        let a = canonical_json(&parse(r#"[1,2]"#)).unwrap();
        let b = canonical_json(&parse(r#"[2,1]"#)).unwrap();
        assert_ne!(a, b);
        // 字符串内容空白差异必须改变哈希（不 trim 不归一化）。
        // Whitespace inside strings must change the hash (no trim/normalization).
        assert_ne!(
            canonical_json(&parse(r#""a b""#)).unwrap(),
            canonical_json(&parse(r#""ab""#)).unwrap()
        );
        assert_ne!(
            canonical_json(&parse(r#""a b""#)).unwrap(),
            canonical_json(&parse(r#""a  b""#)).unwrap()
        );
        // JSON 排版空白不影响规范化字节。
        // JSON formatting whitespace does not affect canonical bytes.
        assert_eq!(
            canonical_json(&parse(r#"{ "a" : 1 }"#)).unwrap(),
            canonical_json(&parse(r#"{"a":1}"#)).unwrap()
        );
    }

    // A4：长度域消除拼接碰撞 —— ["ab","c"] 与 ["a","bc"] 载荷拼接相同但编码不同。
    // A4: length framing removes splice collisions — ["ab","c"] and ["a","bc"]
    // concatenate to the same payload yet encode differently.
    #[test]
    fn length_framing_prevents_splice_collision() {
        let a = canonical_json(&parse(r#"["ab","c"]"#)).unwrap();
        let b = canonical_json(&parse(r#"["a","bc"]"#)).unwrap();
        assert_ne!(a, b);
        // 域帧同理：prompt/model 值组合不同 → 哈希不同。
        // Same for domain framing: different prompt/model value combos → different hash.
        let h1 = frame_hash(&[("p", "ab"), ("m", "c")]);
        let h2 = frame_hash(&[("p", "a"), ("m", "bc")]);
        assert_ne!(h1, h2);
    }

    fn frame_hash(pairs: &[(&str, &str)]) -> String {
        let mut framed = Vec::from(HASH_PREFIX);
        for (name, value) in pairs {
            write_u64(&mut framed, name.len());
            framed.extend_from_slice(name.as_bytes());
            write_u64(&mut framed, value.len());
            framed.extend_from_slice(value.as_bytes());
        }
        blake3::hash(&framed).to_hex().to_string()
    }

    // A4：数值 1 与 1.0 可不同；非有限值拒绝。
    // A4: numbers 1 and 1.0 may differ; non-finite values are rejected.
    #[test]
    fn number_representation_is_stable() {
        assert_ne!(
            canonical_json(&parse(r#"[1]"#)).unwrap(),
            canonical_json(&parse(r#"[1.0]"#)).unwrap()
        );
        let mut map = serde_json::Map::new();
        // 直接构造含非有限数的 Number 不可行（serde_json 拒绝构造），此处验证
        // 有限浮点的文本稳定性即可。
        // Constructing a non-finite Number is impossible (serde_json refuses), so
        // verifying finite-float text stability suffices here.
        map.insert(
            "f".to_string(),
            Value::Number(serde_json::Number::from_f64(0.1).unwrap()),
        );
        let value = Value::Object(map);
        assert_eq!(
            canonical_json(&value).unwrap(),
            canonical_json(&value).unwrap()
        );
    }

    fn fixture() -> (RawEntity, CompileContext, CompilePolicy, EntitySchema) {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        let source = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let context = CompileContext {
            domain_pack_version: "0.1.0".into(),
            prompt_template: "SYSTEM source-ref-v1\nTEMPLATE".into(),
            model_version: "mock-v1".into(),
            embedding_model: "none".into(),
            quality_threshold: 0.75,
            require_source_refs: true,
        };
        let policy = CompilePolicy::default();
        let schema = EntitySchema {
            entity_type: "drink".into(),
            fields: vec![
                FieldDefinition {
                    name: "name".into(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "description".into(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
            ],
        };
        (source, context, policy, schema)
    }

    // A4：content_hash 黄金值（编码/域序任何变动都会破坏此断言）。
    // A4: golden content_hash (any encoding/domain-order change breaks this).
    #[test]
    fn content_hash_golden() {
        let (source, context, policy, schema) = fixture();
        let hex = content_hash(HashDependencies {
            source: &source,
            context: &context,
            policy: &policy,
            source_schema: &schema,
        })
        .unwrap();
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // 黄金 hex（首次生成后固定；与 A3 全依赖失效语义联动）。
        // Golden hex (pinned after first generation; tied to A3 dependency
        // invalidation semantics).
        assert_eq!(
            hex,
            "22454908077c71dcf1e9fd073395ccbf392a769dd4a84fa7b9a8d6283e8a102e"
        );
    }

    // A3 语义：revision 不入 content_hash，但入 snapshot_hash。
    // A3 semantics: revision is excluded from content_hash but included in
    // snapshot_hash.
    #[test]
    fn revision_split_between_hashes() {
        let (source, context, policy, schema) = fixture();
        let h1 = content_hash(HashDependencies {
            source: &source,
            context: &context,
            policy: &policy,
            source_schema: &schema,
        })
        .unwrap();
        let mut rev2 = source.clone();
        rev2.source_revision = 2;
        let h2 = content_hash(HashDependencies {
            source: &rev2,
            context: &context,
            policy: &policy,
            source_schema: &schema,
        })
        .unwrap();
        assert_eq!(h1, h2, "revision must not enter content_hash");
        assert_ne!(
            snapshot_hash(&source).unwrap(),
            snapshot_hash(&rev2).unwrap(),
            "revision must enter snapshot_hash"
        );
        // 同 revision 改内容 → snapshot_hash 变（同 revision 冲突检测）。
        // Same revision with changed content → different snapshot_hash (conflict
        // detection for same revision).
        let mut edited = source.clone();
        edited
            .fields
            .insert("price".to_string(), serde_json::json!(21.0));
        assert_ne!(
            snapshot_hash(&source).unwrap(),
            snapshot_hash(&edited).unwrap()
        );
    }
}
