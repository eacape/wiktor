//! LLM 输出契约：envelope 解码、证据载荷、canonical Markdown 渲染与来源验证
//! （Step 4 spec §5，决策 D3）。
//! LLM output contract: envelope decoding, evidence payloads, canonical Markdown
//! rendering and source validation (Step 4 spec §5, decision D3).
//!
//! 防伪边界：validator 只机械核对引用存在性与证据绑定，不访问网络或当前 facts，
//! 不推断任何源未声明的知识；回放必须基于原始知识快照。
//! Anti-forgery boundary: the validator mechanically checks reference existence and
//! evidence binding, never touches the network or current facts, and never infers
//! knowledge the source did not declare; replay must be based on the original
//! knowledge snapshot.

use crate::types::RawEntity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

/// 模型响应体积上限：先于解析检查（§4/§5.2.1）。
/// Model-response size cap: checked before parsing (§4/§5.2.1).
pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;

/// 每条断言 text 的 Unicode scalar 上限（§5.2.3）。
/// Unicode-scalar cap per assertion text (§5.2.3).
const MAX_ASSERTION_SCALARS: usize = 1024;

/// 标题/别名/标签单项 scalar 上限（§5.2.1）。
/// Per-item scalar cap for title/aliases/tags (§5.2.1).
const MAX_TITLE_SCALARS: usize = 128;

/// ref id 字面量前缀/后缀（固定语法，不写泛化 Markdown parser）。
/// Literal marker prefix/suffix (fixed syntax; no general Markdown parser).
const MARKER_OPEN: &str = "[[ref:";
const MARKER_CLOSE: &str = "]]";

// —— 稳定诊断 code（§5.2；test-engineer 反例复核依赖这些字面量）——
// —— Stable diagnostic codes (§5.2; test-engineer negatives rely on these literals) ——

/// 断言引用了未在本节 `refs` 中声明的 ref id。
/// An assertion cites a ref id that is not declared in this section's `refs`.
pub const MISSING_REF: &str = "MISSING_REF";

/// `refs` 中出现无法解析的未知 ref id。
/// A ref id in `refs` cannot be resolved.
pub const UNKNOWN_REF: &str = "UNKNOWN_REF";

/// 声明了 ref 但没有任何断言使用它。
/// A ref was declared but no assertion uses it.
pub const UNUSED_REF: &str = "UNUSED_REF";

/// 引用的实体 id 与当前源实体不一致。
/// The referenced entity id does not match the current source entity.
pub const SOURCE_ID_MISMATCH: &str = "SOURCE_ID_MISMATCH";

/// 引用的 source revision 与快照 revision 不一致。
/// The referenced source revision does not match the snapshot revision.
pub const REVISION_MISMATCH: &str = "REVISION_MISMATCH";

/// JSON Pointer 在知识快照中不存在。
/// The JSON Pointer does not exist in the knowledge snapshot.
pub const POINTER_MISSING: &str = "POINTER_MISSING";

/// 引用记录的原值与快照中该指针处的值不一致。
/// The recorded source value does not match the snapshot value at the pointer.
pub const VALUE_MISMATCH: &str = "VALUE_MISMATCH";

/// 引文与源字段文本不一致。
/// The quote does not match the source field text.
pub const QUOTE_MISMATCH: &str = "QUOTE_MISMATCH";

/// 断言无法被其引用的来源支撑。
/// The assertion is not supported by its cited sources.
pub const ASSERTION_UNSUPPORTED: &str = "ASSERTION_UNSUPPORTED";

/// 渲染出的 canonical Markdown 与模型给出的 markdown 不一致。
/// The rendered canonical Markdown does not match the model's markdown.
pub const MARKDOWN_MISMATCH: &str = "MARKDOWN_MISMATCH";

/// token 用量（由 LlmClient 报告，模型输出本身不可信时为 None）。
/// Token usage (reported by LlmClient; None when the model output is untrusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

/// envelope 的 wiki 载荷（§5.1；字段全必填）。
/// The envelope's wiki payload (§5.1; all fields required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputWiki {
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub markdown: String,
}

/// 单条断言：抽取式文本 + 引用的 ref id 列表。
/// A single assertion: extractive text plus the referenced ref ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    pub text: String,
    pub ref_ids: Vec<String>,
}

/// 单条来源引用：定位到实体、revision、JSON Pointer、原值与引文。
/// A single source reference: entity, revision, JSON Pointer, original value and quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub id: String,
    pub entity_id: String,
    pub source_revision: u64,
    pub pointer: String,
    pub value: serde_json::Value,
    pub quote: String,
}

/// 章节：标题 + 断言 + 本节 refs。
/// A section: heading + assertions + this section's refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSection {
    pub heading: String,
    pub assertions: Vec<Assertion>,
    pub refs: Vec<SourceRef>,
}

/// 完整证据载荷（`CompiledPage.evidence`；wire envelope 的 `status` 分支在
/// `decode_response` 内部裁决，此处只保存 ok 分支的展开）。
/// Full evidence payload (`CompiledPage.evidence`; the wire envelope's `status`
/// branch is adjudicated inside `decode_response`, only the ok branch is stored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompileEvidence {
    pub schema_version: String,
    pub wiki: OutputWiki,
    pub sections: Vec<EvidenceSection>,
    /// usage 由适配器填充，不属于模型 JSON（模型 envelope 无此字段）。
    /// usage is filled by the adapter and is not part of the model JSON (the model
    /// envelope has no such field).
    pub usage: Option<TokenUsage>,
}

/// 错误 envelope 的 code 枚举（§5.1，仅三个合法值）。
/// The error-envelope code enum (§5.1; exactly three legal values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvelopeErrorCode {
    #[serde(rename = "MISSING_SOURCE_REFS")]
    MissingSourceRefs,
    #[serde(rename = "INSUFFICIENT_SOURCE")]
    InsufficientSource,
    #[serde(rename = "UNSUPPORTED_SOURCE")]
    UnsupportedSource,
}

impl EnvelopeErrorCode {
    /// 稳定 wire 字面量（与 serde rename 一致，供诊断与日志使用）。
    /// Stable wire literal (matches the serde rename; for diagnostics and logs).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MissingSourceRefs => "MISSING_SOURCE_REFS",
            Self::InsufficientSource => "INSUFFICIENT_SOURCE",
            Self::UnsupportedSource => "UNSUPPORTED_SOURCE",
        }
    }
}

impl fmt::Display for EnvelopeErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 编译失败分类（§4）：可重试传输错误 / 永久错误 / 低质量输出候选。
/// Compile-failure classes (§4): retryable transport / permanent / low-quality
/// output candidate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileFailure {
    #[error("retryable compile failure: {code}")]
    Retryable {
        code: String,
        retry_after_seconds: Option<u32>,
    },
    #[error("permanent compile failure: {code}")]
    Permanent { code: String },
    #[error("invalid model output ({code}): {response_prefix}")]
    InvalidOutput {
        code: String,
        response_prefix: String,
    },
}

impl CompileFailure {
    /// 构造 InvalidOutput（低质量候选，消耗重编译次数而非传输重试）。
    /// Builds InvalidOutput (a low-quality candidate consuming recompiles, not
    /// transport retries).
    pub fn invalid(code: impl Into<String>, response: &str) -> Self {
        Self::InvalidOutput {
            code: code.into(),
            response_prefix: response_prefix(response),
        }
    }
}

/// wire envelope 内部表示：status 决定 ok/error 互斥分支（§5.1）。
/// Internal wire-envelope representation: `status` selects the mutually exclusive
/// ok/error branches (§5.1).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeRepr {
    schema_version: String,
    status: String,
    #[serde(default)]
    wiki: Option<OutputWiki>,
    #[serde(default)]
    sections: Option<Vec<EvidenceSection>>,
    #[serde(default)]
    error: Option<EnvelopeError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeError {
    code: EnvelopeErrorCode,
    // wire 契约必填字段：仅用于校验 envelope 形状；错误分支被映射为
    // InvalidOutput（code+prefix），missing_pointers 不越过解码边界。
    // Required wire-contract field: validated for envelope shape only; the error
    // branch maps to InvalidOutput (code+prefix), so missing_pointers never
    // crosses the decode boundary.
    #[allow(dead_code)]
    missing_pointers: Vec<String>,
}

/// 严格 JSON 值：访问器检测重复 object key（serde 默认会静默覆盖）。
/// Strict JSON value: visitor detects duplicate object keys (serde would silently
/// overwrite them by default).
#[derive(Debug)]
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictVisitor;
        impl<'de> serde::de::Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value without duplicate object keys")
            }
            fn visit_bool<E>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_f64<E>(self, v: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let n = serde_json::Number::from_f64(v)
                    .ok_or_else(|| serde::de::Error::custom("non-finite number"))?;
                Ok(StrictValue(Value::Number(n)))
            }
            fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.to_string())))
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(item) = seq.next_element::<StrictValue>()? {
                    out.push(item.0);
                }
                Ok(StrictValue(Value::Array(out)))
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut seen: HashSet<String> = HashSet::new();
                let mut out = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    // 重复 key 一律拒绝，封住 serde 覆盖语义。
                    // Reject duplicate keys outright, sealing serde's overwrite
                    // semantics.
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate object key {key:?}"
                        )));
                    }
                    let value = map.next_value::<StrictValue>()?;
                    out.insert(key, value.0);
                }
                Ok(StrictValue(Value::Object(out)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

/// 严格解码模型响应（§5.1/§5.2.1）：拒绝 fence、前后散文、重复 JSON key、
/// 未知字段与不支持版本；大小先于解析检查。错误 envelope 不是伪造 accepted 页，
/// 映射为 `InvalidOutput` 低质量候选。
/// Strictly decodes the model response (§5.1/§5.2.1): rejects markdown fences,
/// surrounding prose, duplicate JSON keys, unknown fields and unsupported
/// versions; size is checked before parsing. An error envelope never fabricates an
/// accepted page and maps to an `InvalidOutput` low-quality candidate.
pub fn decode_response(json: &str) -> std::result::Result<CompileEvidence, CompileFailure> {
    if json.len() > MAX_RESPONSE_BYTES {
        return Err(CompileFailure::invalid("RESPONSE_TOO_LARGE", json));
    }
    let strict: StrictValue =
        serde_json::from_str(json).map_err(|_| CompileFailure::invalid("MALFORMED_JSON", json))?;
    let env: EnvelopeRepr = serde_json::from_value(strict.0)
        .map_err(|_| CompileFailure::invalid("UNKNOWN_FIELD", json))?;
    if env.schema_version != crate::compile::config::ENVELOPE_SCHEMA_VERSION {
        return Err(CompileFailure::invalid("UNSUPPORTED_VERSION", json));
    }
    match env.status.as_str() {
        "ok" => {
            if env.error.is_some() {
                return Err(CompileFailure::invalid("INVALID_ENVELOPE", json));
            }
            let (wiki, sections) = match (env.wiki, env.sections) {
                (Some(wiki), Some(sections)) => (wiki, sections),
                _ => return Err(CompileFailure::invalid("INVALID_ENVELOPE", json)),
            };
            check_output_limits(&wiki, &sections)
                .map_err(|code| CompileFailure::invalid(code, json))?;
            Ok(CompileEvidence {
                schema_version: env.schema_version,
                wiki,
                sections,
                usage: None,
            })
        }
        "error" => {
            if env.wiki.is_some() || env.sections.is_some() {
                return Err(CompileFailure::invalid("INVALID_ENVELOPE", json));
            }
            let err = env
                .error
                .ok_or_else(|| CompileFailure::invalid("INVALID_ENVELOPE", json))?;
            // 错误 envelope 归入质量候选失败；code 保留领域错误枚举便于观测。
            // An error envelope counts as a quality-candidate failure; the code
            // keeps the domain error enum for observability.
            Err(CompileFailure::InvalidOutput {
                code: format!("ERROR_ENVELOPE_{}", err.code.as_str()),
                response_prefix: response_prefix(json),
            })
        }
        _ => Err(CompileFailure::invalid("INVALID_ENVELOPE", json)),
    }
}

/// wire 结构上限（§5.2.1 第一条）。返回值是 decode 阶段稳定 code。
/// Wire-shape limits (§5.2.1 item 1). The return value is a decode-phase code.
fn check_output_limits(
    wiki: &OutputWiki,
    sections: &[EvidenceSection],
) -> std::result::Result<(), &'static str> {
    if !plain_text_ok(&wiki.title, MAX_TITLE_SCALARS) {
        return Err("SCHEMA_TITLE");
    }
    if wiki.aliases.len() > 32
        || wiki
            .aliases
            .iter()
            .any(|a| !plain_text_ok(a, MAX_TITLE_SCALARS))
    {
        return Err("SCHEMA_ALIASES");
    }
    if wiki.tags.len() > 32
        || wiki
            .tags
            .iter()
            .any(|t| !plain_text_ok(t, MAX_TITLE_SCALARS))
    {
        return Err("SCHEMA_TAGS");
    }
    if sections.is_empty() || sections.len() > 32 {
        return Err("SCHEMA_SECTIONS");
    }
    let mut headings: HashSet<&str> = HashSet::new();
    let mut total_assertions = 0usize;
    let mut total_refs = 0usize;
    for section in sections {
        if !plain_text_ok(&section.heading, MAX_TITLE_SCALARS)
            || !headings.insert(section.heading.as_str())
        {
            return Err("SCHEMA_HEADING");
        }
        if section.assertions.is_empty() || section.assertions.len() > 64 {
            return Err("SCHEMA_ASSERTIONS");
        }
        total_assertions += section.assertions.len();
        total_refs += section.refs.len();
        if total_assertions > 256 {
            return Err("SCHEMA_ASSERTIONS");
        }
        if total_refs > 512 {
            return Err("SCHEMA_REFS");
        }
        for assertion in &section.assertions {
            if !assertion_text_shape_ok(&assertion.text) {
                return Err("SCHEMA_TEXT");
            }
        }
    }
    Ok(())
}

/// 标题类纯文本：非空、≤max scalar、无控制字符/换行。
/// Title-class plain text: non-empty, ≤max scalars, no control chars/newlines.
fn plain_text_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && s.chars().all(|c| !c.is_control())
}

/// 断言 text 形状：非空、单行 ≤1024 scalar、无 Markdown 控制结构、HTML、`[[`/`]]`。
/// Assertion-text shape: non-empty, single line ≤1024 scalars, no Markdown control
/// structures, HTML or `[[`/`]]`.
fn assertion_text_shape_ok(text: &str) -> bool {
    let scalars = text.chars().count();
    scalars > 0
        && scalars <= MAX_ASSERTION_SCALARS
        && text.chars().all(|c| !c.is_control())
        && !text.contains(MARKER_OPEN)
        && !text.contains(MARKER_CLOSE)
        && !text.contains('<')
}

/// 响应前缀（诊断用，截 64 scalar）。
/// Response prefix for diagnostics (first 64 scalars).
fn response_prefix(json: &str) -> String {
    json.chars().take(64).collect()
}

/// ref id 是否匹配 `r[1-9][0-9]{0,5}`。
/// Whether a ref id matches `r[1-9][0-9]{0,5}`.
pub fn is_valid_ref_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() >= 2
        && bytes.len() <= 7
        && bytes[0] == b'r'
        && bytes[1].is_ascii_digit()
        && bytes[1] != b'0'
        && bytes[2..].iter().all(u8::is_ascii_digit)
}

/// 原样扫描 `[[ref:rN]]` markers，返回 (按出现顺序的合法 id 序列, 合法 marker 数)。
/// Scans `[[ref:rN]]` markers verbatim, returning (valid ids in order, valid
/// marker count).
fn scan_markers(markdown: &str) -> (Vec<String>, u32) {
    let mut ids = Vec::new();
    let mut count = 0u32;
    for (idx, _) in markdown.char_indices() {
        if !markdown[idx..].starts_with(MARKER_OPEN) {
            continue;
        }
        let after = idx + MARKER_OPEN.len();
        let Some(end) = markdown[after..].find(MARKER_CLOSE) else {
            continue;
        };
        let id = &markdown[after..after + end];
        if is_valid_ref_id(id) {
            ids.push(id.to_string());
            count += 1;
        }
    }
    (ids, count)
}

/// RFC6901 指针切分（`~1`→`/`，`~0`→`~`，顺序不可颠倒）。
/// RFC6901 pointer tokenization (`~1`→`/`, `~0`→`~`; order must not be swapped).
fn pointer_tokens(pointer: &str) -> Option<Vec<String>> {
    let rest = pointer.strip_prefix('/')?;
    Some(
        rest.split('/')
            .map(|tok| tok.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

/// RFC6901 数组下标：无前导零（"0" 除外）。
/// RFC6901 array index: no leading zeros (except "0" itself).
fn array_index(token: &str) -> Option<usize> {
    if token == "0" {
        return Some(0);
    }
    if token.is_empty() || token.starts_with('0') || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse::<usize>().ok()
}

/// 从知识快照根 `{id,fields,source_revision}` 解析指针，只允许
/// `/fields/<allowed>` 下的 string leaf；reflist 必须指到索引元素（§5.2.2）。
/// Resolves a pointer from the knowledge-snapshot root `{id,fields,source_revision}`,
/// allowing only string leaves under `/fields/<allowed>`; reflists must point at an
/// indexed element (§5.2.2).
fn resolve_string_leaf<'a>(source: &'a RawEntity, pointer: &str) -> Option<&'a serde_json::Value> {
    let tokens = pointer_tokens(pointer)?;
    if tokens.len() < 2 || tokens[0] != "fields" {
        return None;
    }
    let value = source.fields.get(&tokens[1])?;
    match tokens.len() {
        // 纯 string 字段 leaf。
        // Plain string-field leaf.
        2 => value.as_str().map(|_| value),
        // reflist：必须指到单个元素，不能整数组引用。
        // reflist: must target a single element, never the whole array.
        3 => {
            let index = array_index(&tokens[2])?;
            let element = value.as_array()?.get(index)?;
            element.as_str().map(|_| element)
        }
        _ => None,
    }
}

/// canonical Markdown 渲染（§5.2.4）：每节 `## {heading}\n\n`，每断言
/// `- {text}{markers}\n`（markers 按 ref_ids 顺序连接），节间一个空行。
/// Canonical Markdown rendering (§5.2.4): each section `## {heading}\n\n`, each
/// assertion `- {text}{markers}\n` (markers joined in ref_ids order), one blank
/// line between sections.
pub fn render_canonical_markdown(evidence: &CompileEvidence) -> String {
    let mut out = String::new();
    for (si, section) in evidence.sections.iter().enumerate() {
        if si > 0 {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(&section.heading);
        out.push_str("\n\n");
        for assertion in &section.assertions {
            out.push_str("- ");
            out.push_str(&assertion.text);
            for id in &assertion.ref_ids {
                out.push_str(MARKER_OPEN);
                out.push_str(id);
                out.push_str(MARKER_CLOSE);
            }
            out.push('\n');
        }
    }
    out
}

/// 单条诊断（JSON path + 稳定 code）。
/// A single diagnostic (JSON path + stable code).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityIssue {
    pub code: String,
    pub path: String,
}

/// validator 的机械报告（§5 签名；U/C/A/S/R/V/I/T 语义见 §6）。
/// The validator's mechanical report (§5 signature; U/C/A/S/R/V/I/T semantics in §6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefReport {
    /// A：断言总数。
    /// A: total assertions.
    pub assertions: u32,
    /// S：引用全部有效且满足抽取式约束的断言数。
    /// S: assertions whose refs are all valid and satisfy the extractive constraint.
    pub supported_assertions: u32,
    /// R：正文 marker 引用次数 + 未使用 ref 定义数量。
    /// R: body marker occurrences + unused ref definitions.
    pub ref_occurrences: u32,
    /// V：有效 marker 引用次数。
    /// V: valid marker occurrences.
    pub valid_ref_occurrences: u32,
    /// C：被有效且已使用且断言支持成功的引用命中的单元（U 的子集）。
    /// C: units hit by valid, used, assertion-supported refs (a subset of U).
    pub covered_units: BTreeSet<String>,
    /// I：density 信息字符数（源字符位置并集，忽略空白）。
    /// I: density information chars (union of source char positions, whitespace
    /// ignored).
    pub information_chars: u64,
    /// T：全部断言 text 的非空白 Unicode scalar 数。
    /// T: non-whitespace Unicode scalars across all assertion texts.
    pub total_chars: u64,
    pub issues: Vec<QualityIssue>,
}

/// 来源引用验证器（§5 签名）。
/// The source-ref validator (§5 signature).
pub trait SourceRefValidator: Send + Sync {
    fn validate(
        &self,
        source: &RawEntity,
        evidence: &CompileEvidence,
        require_refs: bool,
    ) -> RefReport;
}

/// 默认验证器：spec §5.2 算法 1-6 的机械实现，零外部状态。
/// The default validator: mechanical implementation of spec §5.2 items 1-6 with
/// zero external state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultSourceRefValidator;

impl DefaultSourceRefValidator {
    /// 构造默认来源验证器（无配置、无外部状态）。
    /// Constructs the default source-ref validator (no config, no external state).
    pub fn new() -> Self {
        Self
    }
}

/// 通过全部校验的 ref（供 marker/citation/coverage/density 复用）。
/// A ref that passed all checks (reused by marker/citation/coverage/density).
struct ValidRef {
    pointer: String,
    quote: String,
}

impl SourceRefValidator for DefaultSourceRefValidator {
    fn validate(
        &self,
        source: &RawEntity,
        evidence: &CompileEvidence,
        require_refs: bool,
    ) -> RefReport {
        let mut issues: Vec<QualityIssue> = Vec::new();
        let entity_key = source.id.to_key();
        let mut report = RefReport::default();

        // —— 算法 2：ref 定义校验（id 形状/全页唯一、entity/revision 精确匹配、
        //    指针 string leaf、value 精确相等、quote 连续精确子串）——
        // —— Item 2: ref-definition validation (id shape/page-wide uniqueness,
        //    exact entity/revision match, pointer string leaf, exact value,
        //    contiguous exact quote) ——
        let mut valid_refs: HashMap<String, ValidRef> = HashMap::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        for (si, section) in evidence.sections.iter().enumerate() {
            for (ri, r) in section.refs.iter().enumerate() {
                let path = format!("/sections/{si}/refs/{ri}");
                // 非法形状或重复 id 视为不可解析引用（稳定 code 集内最贴近的
                // UNKNOWN_REF；见偏差注释：稳定 code 不含 DUP_REF）。
                // Malformed or duplicate ids are unresolvable references (mapped to
                // UNKNOWN_REF, the closest stable code; see deviation note: the
                // stable set has no DUP_REF).
                if !is_valid_ref_id(&r.id) || !seen_ids.insert(r.id.clone()) {
                    issues.push(QualityIssue {
                        code: UNKNOWN_REF.to_string(),
                        path,
                    });
                    continue;
                }
                if r.entity_id != entity_key {
                    issues.push(QualityIssue {
                        code: SOURCE_ID_MISMATCH.to_string(),
                        path: path.clone(),
                    });
                }
                if r.source_revision != source.source_revision {
                    issues.push(QualityIssue {
                        code: REVISION_MISMATCH.to_string(),
                        path: path.clone(),
                    });
                }
                let mut valid = true;
                match resolve_string_leaf(source, &r.pointer) {
                    None => {
                        // 指针缺失/越界/非 string leaf/整数组引用统一此 code。
                        // Missing/out-of-bounds pointers, non-string leaves and
                        // whole-array references all use this code.
                        issues.push(QualityIssue {
                            code: POINTER_MISSING.to_string(),
                            path: path.clone(),
                        });
                        valid = false;
                    }
                    Some(leaf) => {
                        if r.value != *leaf {
                            issues.push(QualityIssue {
                                code: VALUE_MISMATCH.to_string(),
                                path: path.clone(),
                            });
                            valid = false;
                        }
                        let leaf_str = leaf.as_str().unwrap_or_default();
                        // quote 非空且为原值连续精确子串，不做繁简/大小写/空白归一化。
                        // Quote must be non-empty and a contiguous exact substring;
                        // no Han/case/whitespace normalization.
                        if r.quote.is_empty() || !leaf_str.contains(&r.quote) {
                            issues.push(QualityIssue {
                                code: QUOTE_MISMATCH.to_string(),
                                path: path.clone(),
                            });
                            valid = false;
                        }
                    }
                }
                if valid {
                    valid_refs.insert(
                        r.id.clone(),
                        ValidRef {
                            pointer: r.pointer.clone(),
                            quote: r.quote.clone(),
                        },
                    );
                }
            }
        }

        // —— 算法 3/4：断言抽取式约束 + canonical Markdown 比对 + 计数 ——
        // —— Items 3/4: assertion extractive constraint + canonical Markdown
        //    comparison + counters ——
        let mut total_chars: u64 = 0;
        let mut expected_markers: Vec<String> = Vec::new();
        // density：pointer → (leaf 原文, 已覆盖 scalar 位置集合)。
        // density: pointer → (leaf text, covered scalar positions).
        let mut covered_spans: HashMap<String, (String, BTreeSet<usize>)> = HashMap::new();

        for (si, section) in evidence.sections.iter().enumerate() {
            let ref_index: HashMap<&str, &SourceRef> =
                section.refs.iter().map(|r| (r.id.as_str(), r)).collect();
            let mut used_in_section: HashSet<&str> = HashSet::new();
            for (ai, assertion) in section.assertions.iter().enumerate() {
                report.assertions = report.assertions.saturating_add(1);
                let path = format!("/sections/{si}/assertions/{ai}");
                // T 计全部断言 text 的非空白 scalar（含失败断言）。
                // T counts non-whitespace scalars of every assertion text
                // (including failed ones).
                total_chars += assertion
                    .text
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count() as u64;
                for id in &assertion.ref_ids {
                    expected_markers.push(id.clone());
                }
                // 形状违规 → 断言不可支持（稳定 code 集内映射为
                // ASSERTION_UNSUPPORTED；decode 阶段另有 SCHEMA_TEXT）。
                // Shape violations make the assertion unsupported (mapped to
                // ASSERTION_UNSUPPORTED within the stable set; decode has
                // SCHEMA_TEXT separately).
                if !assertion_text_shape_ok(&assertion.text) {
                    issues.push(QualityIssue {
                        code: ASSERTION_UNSUPPORTED.to_string(),
                        path,
                    });
                    continue;
                }
                // 无引用断言：require_refs 时 MISSING_REF；无论开关均不计支持。
                // No-ref assertion: MISSING_REF under require_refs; never counted
                // as supported either way (citation<1 quarantines such pages).
                if assertion.ref_ids.is_empty() {
                    if require_refs {
                        issues.push(QualityIssue {
                            code: MISSING_REF.to_string(),
                            path,
                        });
                    }
                    continue;
                }
                let mut all_known = true;
                let mut all_valid = true;
                for (ki, id) in assertion.ref_ids.iter().enumerate() {
                    // 必须引用本节 refs；跨节 id 即 UNKNOWN_REF。
                    // Must reference this section's refs; a cross-section id is
                    // UNKNOWN_REF.
                    match ref_index.get(id.as_str()) {
                        Some(r) => {
                            used_in_section.insert(r.id.as_str());
                            if !valid_refs.contains_key(id) {
                                all_valid = false;
                            }
                        }
                        None => {
                            all_known = false;
                            all_valid = false;
                            issues.push(QualityIssue {
                                code: UNKNOWN_REF.to_string(),
                                path: format!("{path}/ref_ids/{ki}"),
                            });
                        }
                    }
                }
                // 抽取式约束：text == 按 ref_ids 顺序以单个空格连接的 quote。
                // Extractive constraint: text equals the quotes joined by single
                // spaces in ref_ids order.
                if all_known {
                    let joined = assertion
                        .ref_ids
                        .iter()
                        .map(|id| ref_index[id.as_str()].quote.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if assertion.text != joined {
                        issues.push(QualityIssue {
                            code: ASSERTION_UNSUPPORTED.to_string(),
                            path: path.clone(),
                        });
                        all_valid = false;
                    }
                }
                if !(all_known && all_valid) {
                    continue;
                }
                // 支持成功的断言：进 C（coverage）与 density。
                // A fully supported assertion: enters C (coverage) and density.
                report.supported_assertions = report.supported_assertions.saturating_add(1);
                let mut seen_pairs: HashSet<(&str, &str)> = HashSet::new();
                for id in &assertion.ref_ids {
                    let valid_ref = &valid_refs[id];
                    report.covered_units.insert(valid_ref.pointer.clone());
                    // 同一断言内重复 (pointer,quote) 只算一次（§6 density）。
                    // Duplicate (pointer,quote) inside one assertion counts once
                    // (§6 density).
                    if !seen_pairs.insert((valid_ref.pointer.as_str(), valid_ref.quote.as_str())) {
                        continue;
                    }
                    if let Some(leaf_value) = resolve_string_leaf(source, &valid_ref.pointer) {
                        if let Some(leaf) = leaf_value.as_str() {
                            // 同一源串多次出现取最左匹配；字符位置按 Unicode scalar 计。
                            // Leftmost occurrence on repeated matches; positions in
                            // Unicode scalars.
                            if let Some(byte_pos) = leaf.find(&valid_ref.quote) {
                                let start = leaf[..byte_pos].chars().count();
                                let len = valid_ref.quote.chars().count();
                                let entry = covered_spans
                                    .entry(valid_ref.pointer.clone())
                                    .or_insert_with(|| (leaf.to_string(), BTreeSet::new()));
                                entry.1.extend(start..start + len);
                            }
                        }
                    }
                }
            }
            // —— 未使用 ref 定义：本节 refs 未被任何断言引用 → UNUSED_REF ——
            // —— Unused ref definitions: section refs never referenced by any
            //    assertion → UNUSED_REF ——
            for (ri, r) in section.refs.iter().enumerate() {
                if !used_in_section.contains(r.id.as_str()) {
                    issues.push(QualityIssue {
                        code: UNUSED_REF.to_string(),
                        path: format!("/sections/{si}/refs/{ri}"),
                    });
                    report.ref_occurrences = report.ref_occurrences.saturating_add(1);
                }
            }
        }

        // —— 算法 4：markers 原样扫描 + canonical 渲染字节比对 + pulldown 结构确认 ——
        // —— Item 4: verbatim marker scan + canonical renderer byte comparison +
        //    pulldown structure confirmation ——
        let (actual_markers, valid_marker_count) = scan_markers(&evidence.wiki.markdown);
        report.valid_ref_occurrences = valid_marker_count;
        report.ref_occurrences = report.ref_occurrences.saturating_add(valid_marker_count);
        let canonical = render_canonical_markdown(evidence);
        let markdown_ok = actual_markers == expected_markers
            && evidence.wiki.markdown == canonical
            && markdown_structure_ok(&evidence.wiki.markdown);
        if !markdown_ok {
            // 额外散文、代码块、HTML、伪 refs、不受审计的段落全部落在这一 code。
            // Extra prose, code blocks, HTML, pseudo refs and unaudited paragraphs
            // all land on this code.
            issues.push(QualityIssue {
                code: MARKDOWN_MISMATCH.to_string(),
                path: "/wiki/markdown".to_string(),
            });
        }

        // —— 算法 3（尾部）：标题/aliases/tags 必须精确出现在至少一条有效 quote ——
        // —— Item 3 (tail): title/aliases/tags must appear exactly inside at least
        //    one valid quote ——
        let valid_quotes: Vec<&str> = valid_refs.values().map(|v| v.quote.as_str()).collect();
        if !appears_in_any_quote(&evidence.wiki.title, &valid_quotes) {
            issues.push(QualityIssue {
                code: QUOTE_MISMATCH.to_string(),
                path: "/wiki/title".to_string(),
            });
        }
        for (i, alias) in evidence.wiki.aliases.iter().enumerate() {
            if !appears_in_any_quote(alias, &valid_quotes) {
                issues.push(QualityIssue {
                    code: QUOTE_MISMATCH.to_string(),
                    path: format!("/wiki/aliases/{i}"),
                });
            }
        }
        for (i, tag) in evidence.wiki.tags.iter().enumerate() {
            if !appears_in_any_quote(tag, &valid_quotes) {
                issues.push(QualityIssue {
                    code: QUOTE_MISMATCH.to_string(),
                    path: format!("/wiki/tags/{i}"),
                });
            }
        }

        // —— density I：源字符位置并集，忽略空白，上限 T（在 scorer 中再 clamp）——
        // —— density I: union of source char positions, whitespace ignored, capped
        //    at T (clamped again in the scorer) ——
        let information_chars: u64 = covered_spans
            .values()
            .map(|(leaf, positions)| {
                let chars: Vec<char> = leaf.chars().collect();
                positions
                    .iter()
                    .filter(|&&p| chars.get(p).is_some_and(|c| !c.is_whitespace()))
                    .count() as u64
            })
            .sum();

        report.total_chars = total_chars;
        report.information_chars = information_chars.min(total_chars);
        report.issues = issues;
        report
    }
}

fn appears_in_any_quote(needle: &str, quotes: &[&str]) -> bool {
    quotes.iter().any(|q| q.contains(needle))
}

/// 内置 source-ref-v1 system prompt（§3.1「prompt 可省略，使用内置模板」的缺省
/// 模板面；Prompt bytes 参与 content_hash，改动即失效）。领域字面量保持中文示例
/// 原样。非 feature-gated：CLI dry-run/编译在无 `llm-openai` feature 时冻结的
/// Prompt bytes 与 `LlmCompiler` 实际发送的 system 消息保持同一份。
/// The built-in source-ref-v1 system prompt (the default template when §3.1's
/// `prompt` is omitted; prompt bytes join content_hash, so edits invalidate
/// artifacts). Domain literals keep their Chinese examples verbatim. Not
/// feature-gated: the prompt bytes frozen by CLI compile/dry-run stay identical
/// to the system message `LlmCompiler` actually sends, even without the
/// `llm-openai` feature.
pub fn system_prompt() -> String {
    // 模板含 "## {heading}" 字样，故用 r###"..."### 避免 " 提前终止。
    // The template contains "## {heading}", so r###"..."### is required to avoid
    // early termination on ".
    r###"你是 Wiktor 知识编译器。只依据用户消息中的 JSON 知识快照编译一页 Wiki，输出一个 JSON 对象，禁止输出 markdown 代码块、解释或任何额外文本。

输出契约（source-ref-v1）：
- 顶层字段：schema_version（固定 "source-ref-v1"）、status（"ok" 或 "error"）。
- status="ok" 时：wiki（title、aliases、tags、markdown）与 sections（heading、assertions、refs）必填，且互斥地不得出现 error 字段。
- status="error" 时：error（code ∈ MISSING_SOURCE_REFS|INSUFFICIENT_SOURCE|UNSUPPORTED_SOURCE，missing_pointers 为字符串数组），不得出现 wiki/sections。

抽取式规则（必须全部满足）：
1. 每条断言 assertion 的 text 必须逐字等于其 ref_ids 顺序对应的 quote 以单个空格连接的结果；禁止改写、推断或补充快照之外的知识。
2. 每个引用 ref 的 id 形如 r1、r2（递增，全页唯一），entity_id 与 source_revision 必须等于知识快照的值——绝不能省略前缀或截断，例如快照中的 entity_id 是 "milk-tea:product:sku_0001"，refs 里的 entity_id 必须原样写 "milk-tea:product:sku_0001"，禁止写成 "sku_0001"；pointer 是 RFC6901 指针，只能指向 /fields/<字段> 下的字符串（数组字段必须带下标，如 /fields/ingredients/0）；value 必须等于指针解出的原值；quote 必须是该原值的连续精确子串。
3. wiki.markdown 必须严格按以下格式渲染：每节为 "## {heading}\n\n"，每条断言一行 "- {text}"，随后按 ref_ids 顺序**紧贴**其末尾追加 "[[ref:rN]]"（text 与 marker 之间不得插入任何空格或字符），行尾换行，节间一个空行；不得有其它散文、代码块或 HTML。示例：text="珍珠"、ref_ids=["r1"] 时该行必须为 `- 珍珠[[ref:r1]]`（"珍珠" 与 "[[ref:r1]]" 之间无空格）。
4. 必须为 title 字段创建一条 ref（pointer="/fields/title"、value=title 原值、quote=title 整值的连续子串），并在至少一条断言中引用该 ref，使 title 出现在合法 quote 之内；aliases、tags 亦必须逐字出现在某条合法 quote 之内；title 为纯文本，无换行。示例：若快照 fields.title="语义化版本规范（howto）"，则须有 ref{id:"r1",pointer:"/fields/title",quote:"语义化版本规范（howto）"}。
5. heading 必须使用给定领域标题（如「概述」）。
6. 若快照信息不足（例如字段全为空），输出 status="error" 且 code="MISSING_SOURCE_REFS"，列出缺失指针。
"###
    .to_string()
}

/// pulldown-cmark 结构确认：无 HTML/代码块，标题仅 H2（canonical 字节比对之外的
/// 第二道闸；正文可见文本由字节比对保证）。
/// pulldown-cmark structure confirmation: no HTML/code blocks, headings limited to
/// H2 (a second gate beyond the canonical byte comparison; visible text is already
/// guaranteed by byte equality).
fn markdown_structure_ok(markdown: &str) -> bool {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
    let parser = Parser::new_ext(markdown, Options::empty());
    for event in parser {
        match event {
            Event::Html(_) | Event::InlineHtml(_) => return false,
            Event::Start(Tag::CodeBlock(_)) => return false,
            Event::Start(Tag::Heading { level, .. }) if level != HeadingLevel::H2 => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EntityId;
    use std::collections::BTreeMap;

    /// §5.1 fixture：源与证据两断言两合法 refs。
    /// The §5.1 fixture: source plus evidence with two assertions and two valid refs.
    fn fixture_source() -> RawEntity {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠"));
        RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        }
    }

    fn fixture_evidence() -> CompileEvidence {
        CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                title: "啵啵".to_string(),
                aliases: vec![],
                tags: vec![],
                markdown: "## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n".to_string(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".to_string(),
                assertions: vec![
                    Assertion {
                        text: "啵啵".to_string(),
                        ref_ids: vec!["r1".to_string()],
                    },
                    Assertion {
                        text: "珍珠".to_string(),
                        ref_ids: vec!["r2".to_string()],
                    },
                ],
                refs: vec![
                    SourceRef {
                        id: "r1".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/name".to_string(),
                        value: serde_json::json!("啵啵"),
                        quote: "啵啵".to_string(),
                    },
                    SourceRef {
                        id: "r2".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/description".to_string(),
                        value: serde_json::json!("珍珠"),
                        quote: "珍珠".to_string(),
                    },
                ],
            }],
            usage: None,
        }
    }

    fn codes(report: &RefReport) -> Vec<&str> {
        report.issues.iter().map(|i| i.code.as_str()).collect()
    }

    // A5：正引用 —— 全部机械指标为 1，无 issue。
    // A5: positive references — every mechanical metric equals 1 with no issues.
    #[test]
    fn positive_refs_score_full() {
        let source = fixture_source();
        let evidence = fixture_evidence();
        let report = DefaultSourceRefValidator.validate(&source, &evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
        assert_eq!(report.assertions, 2);
        assert_eq!(report.supported_assertions, 2);
        assert_eq!(report.ref_occurrences, 2);
        assert_eq!(report.valid_ref_occurrences, 2);
        assert_eq!(
            report.covered_units,
            BTreeSet::from([
                "/fields/name".to_string(),
                "/fields/description".to_string()
            ])
        );
        assert_eq!(report.information_chars, 4);
        assert_eq!(report.total_chars, 4);
    }

    // A5：JSON Pointer 转义 ~0/~1 可解析。
    // A5: JSON Pointer escapes ~0/~1 resolve.
    #[test]
    fn pointer_escapes_resolve() {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("a/b".to_string(), serde_json::json!("斜线值"));
        fields.insert("a~b".to_string(), serde_json::json!("波浪值"));
        let source = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let evidence = CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                // 标题必须出现在有效 quote 中（§5.2.3），fixture 取第一个断言文本。
                // The title must appear in a valid quote (§5.2.3); the fixture uses
                // the first assertion text.
                title: "斜线值".to_string(),
                aliases: vec![],
                tags: vec![],
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".to_string(),
                assertions: vec![
                    Assertion {
                        text: "斜线值".to_string(),
                        ref_ids: vec!["r1".to_string()],
                    },
                    Assertion {
                        text: "波浪值".to_string(),
                        ref_ids: vec!["r2".to_string()],
                    },
                ],
                refs: vec![
                    SourceRef {
                        id: "r1".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/a~1b".to_string(),
                        value: serde_json::json!("斜线值"),
                        quote: "斜线值".to_string(),
                    },
                    SourceRef {
                        id: "r2".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/a~0b".to_string(),
                        value: serde_json::json!("波浪值"),
                        quote: "波浪值".to_string(),
                    },
                ],
            }],
            usage: None,
        };
        let mut evidence = evidence;
        evidence.wiki.markdown = render_canonical_markdown(&evidence);
        let report = DefaultSourceRefValidator.validate(&source, &evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    }

    // A6：反引用 —— 每个 mutation 产出对应稳定 issue code 且不可接受。
    // A6: negative references — each mutation yields its stable issue code and is
    // rejected.
    #[test]
    fn negative_refs_report_stable_codes() {
        let source = fixture_source();

        // 错 entity → SOURCE_ID_MISMATCH。/ Wrong entity → SOURCE_ID_MISMATCH.
        let mut e = fixture_evidence();
        e.sections[0].refs[0].entity_id = "milk-tea:drink:other".into();
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&SOURCE_ID_MISMATCH));

        // 错 revision → REVISION_MISMATCH。/ Wrong revision → REVISION_MISMATCH.
        let mut e = fixture_evidence();
        e.sections[0].refs[0].source_revision = 2;
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&REVISION_MISMATCH));

        // 错 pointer → POINTER_MISSING。/ Wrong pointer → POINTER_MISSING.
        let mut e = fixture_evidence();
        e.sections[0].refs[1].pointer = "/fields/unknown".into();
        e.sections[0].refs[1].value = serde_json::json!("珍珠");
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&POINTER_MISSING));

        // 错 value → VALUE_MISMATCH。/ Wrong value → VALUE_MISMATCH.
        let mut e = fixture_evidence();
        e.sections[0].refs[1].value = serde_json::json!("别的");
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&VALUE_MISMATCH));

        // 错 quote → QUOTE_MISMATCH。/ Wrong quote → QUOTE_MISMATCH.
        let mut e = fixture_evidence();
        e.sections[0].refs[1].quote = "奶茶".into();
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&QUOTE_MISMATCH));
        assert!(codes(&r).contains(&ASSERTION_UNSUPPORTED));

        // 整数组引用 → POINTER_MISSING。/ Whole-array reference → POINTER_MISSING.
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert(
            "ingredients".to_string(),
            serde_json::json!(["珍珠", "椰果"]),
        );
        let list_source = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let mut e = fixture_evidence();
        e.sections[0].refs[1].pointer = "/fields/ingredients".into();
        e.sections[0].refs[1].value = serde_json::json!(["珍珠", "椰果"]);
        e.sections[0].refs[1].quote = "珍珠".into();
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&list_source, &e, true);
        assert!(codes(&r).contains(&POINTER_MISSING));

        // reflist 索引元素引用合法。/ Indexed reflist element is legal.
        let mut e = fixture_evidence();
        e.sections[0].refs[1].pointer = "/fields/ingredients/0".into();
        e.sections[0].refs[1].value = serde_json::json!("珍珠");
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&list_source, &e, true);
        assert!(r.issues.is_empty(), "issues: {:?}", r.issues);
    }

    // A6：跨节 ID / 悬空 / 未使用 ref 各自对应 UNKNOWN_REF / UNUSED_REF。
    // A6: cross-section ids / dangling / unused refs map to UNKNOWN_REF and
    // UNUSED_REF.
    #[test]
    fn cross_section_dangling_and_unused() {
        let source = fixture_source();

        // 悬空：断言引用未定义 id。/ Dangling: assertion references an undefined id.
        let mut e = fixture_evidence();
        e.sections[0].assertions[1].ref_ids = vec!["r9".into()];
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&UNKNOWN_REF));

        // 跨节：第二节断言引用第一节的 ref。/ Cross-section: the second section's
        // assertion references the first section's ref.
        let mut e = fixture_evidence();
        e.sections.push(EvidenceSection {
            heading: "制作".to_string(),
            assertions: vec![Assertion {
                text: "啵啵".to_string(),
                ref_ids: vec!["r1".to_string()],
            }],
            refs: vec![],
        });
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&UNKNOWN_REF));

        // 未使用：第二节定义 r3 但无断言引用。/ Unused: the second section defines
        // r3 that no assertion references.
        let mut e = fixture_evidence();
        e.sections[0].refs.push(SourceRef {
            id: "r3".to_string(),
            entity_id: "milk-tea:drink:boba".to_string(),
            source_revision: 1,
            pointer: "/fields/name".to_string(),
            value: serde_json::json!("啵啵"),
            quote: "啵啵".to_string(),
        });
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&UNUSED_REF));
        assert_eq!(r.ref_occurrences, 2 + 1, "R = markers + unused definitions");
    }

    // A8（部分）：canonical 之外的散文/伪 marker → MARKDOWN_MISMATCH。
    // A8 (partial): prose/pseudo markers beyond canonical → MARKDOWN_MISMATCH.
    #[test]
    fn markdown_mismatch_detects_prose_and_pseudo_markers() {
        let source = fixture_source();

        let mut e = fixture_evidence();
        e.wiki.markdown = format!("导语\n\n{}", render_canonical_markdown(&e));
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&MARKDOWN_MISMATCH));

        let mut e = fixture_evidence();
        e.wiki.markdown = format!("{}[[ref:r0]]\n", render_canonical_markdown(&e));
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&MARKDOWN_MISMATCH));

        let mut e = fixture_evidence();
        // HTML → 结构确认失败。/ HTML fails the structure confirmation.
        e.wiki.markdown = render_canonical_markdown(&e);
        e.wiki.markdown.push_str("<script>alert(1)</script>\n");
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&MARKDOWN_MISMATCH));

        // 断言顺序置换（marker 顺序与映射不一致）。/ Shuffled assertion order
        // (marker order diverges from the mapping).
        let mut e = fixture_evidence();
        e.wiki.markdown = "## 概述\n\n- 珍珠[[ref:r2]]\n- 啵啵[[ref:r1]]\n".to_string();
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&MARKDOWN_MISMATCH));
    }

    // require_refs=false 放松无引用断言，但断言仍不计支持（citation<1）。
    // require_refs=false relaxes no-ref assertions, yet they are never supported
    // (citation<1).
    #[test]
    fn require_refs_false_relaxes_but_does_not_support() {
        let source = fixture_source();
        let mut e = fixture_evidence();
        e.sections[0].assertions.push(Assertion {
            text: "自由陈述".to_string(),
            ref_ids: vec![],
        });
        e.wiki.markdown = render_canonical_markdown(&e);
        let strict = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&strict).contains(&MISSING_REF));
        let loose = DefaultSourceRefValidator.validate(&source, &e, false);
        assert!(!codes(&loose).contains(&MISSING_REF));
        assert_eq!(loose.assertions, 3);
        assert_eq!(loose.supported_assertions, 2);
    }

    // 重复 ref id → UNKNOWN_REF（稳定 code 集无 DUP_REF 的映射决策）。
    // Duplicate ref id → UNKNOWN_REF (mapping decision: the stable set has no
    // DUP_REF).
    #[test]
    fn duplicate_ref_id_rejected() {
        let source = fixture_source();
        let mut e = fixture_evidence();
        e.sections[0].refs.push(SourceRef {
            id: "r1".to_string(),
            entity_id: "milk-tea:drink:boba".to_string(),
            source_revision: 1,
            pointer: "/fields/name".to_string(),
            value: serde_json::json!("啵啵"),
            quote: "啵啵".to_string(),
        });
        e.wiki.markdown = render_canonical_markdown(&e);
        let r = DefaultSourceRefValidator.validate(&source, &e, true);
        assert!(codes(&r).contains(&UNKNOWN_REF));
    }

    // A10（部分）：同 quote 重复十次 → I 不随重复增长。
    // A10 (partial): the same quote repeated ten times → I does not grow with
    // repeats.
    #[test]
    fn repeated_quote_counts_once_for_density() {
        let source = fixture_source();
        let quote = "啵啵";
        let text = ["啵啵"; 10].join(" ");
        let evidence = CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                title: "啵啵".to_string(),
                aliases: vec![],
                tags: vec![],
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".to_string(),
                assertions: vec![Assertion {
                    text,
                    ref_ids: vec!["r1".to_string(); 10],
                }],
                refs: vec![SourceRef {
                    id: "r1".to_string(),
                    entity_id: "milk-tea:drink:boba".to_string(),
                    source_revision: 1,
                    pointer: "/fields/name".to_string(),
                    value: serde_json::json!("啵啵"),
                    quote: quote.to_string(),
                }],
            }],
            usage: None,
        };
        let mut evidence = evidence;
        evidence.wiki.markdown = render_canonical_markdown(&evidence);
        let report = DefaultSourceRefValidator.validate(&source, &evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
        assert_eq!(report.total_chars, 20, "T = 10 repeats x 2 scalars");
        assert_eq!(report.information_chars, 2, "I counts the unique pair once");
    }

    // A10（部分）：重叠 quote 的源字符位置取并集。
    // A10 (partial): overlapping quotes union their source char positions.
    #[test]
    fn overlapping_quotes_union_positions() {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("body".to_string(), serde_json::json!("abcabc"));
        let source = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let evidence = CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                // 标题必须出现在有效 quote 中（§5.2.3）。
                // The title must appear in a valid quote (§5.2.3).
                title: "abc".to_string(),
                aliases: vec![],
                tags: vec![],
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".to_string(),
                assertions: vec![Assertion {
                    text: "abc abcabc".to_string(),
                    ref_ids: vec!["r1".to_string(), "r2".to_string()],
                }],
                refs: vec![
                    SourceRef {
                        id: "r1".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/body".to_string(),
                        value: serde_json::json!("abcabc"),
                        quote: "abc".to_string(),
                    },
                    SourceRef {
                        id: "r2".to_string(),
                        entity_id: "milk-tea:drink:boba".to_string(),
                        source_revision: 1,
                        pointer: "/fields/body".to_string(),
                        value: serde_json::json!("abcabc"),
                        quote: "abcabc".to_string(),
                    },
                ],
            }],
            usage: None,
        };
        let mut evidence = evidence;
        evidence.wiki.markdown = render_canonical_markdown(&evidence);
        let report = DefaultSourceRefValidator.validate(&source, &evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
        // r1 覆盖 [0,3)，r2 覆盖 [0,6)，并集 6 而非 9。
        // r1 covers [0,3), r2 covers [0,6); the union is 6, not 9.
        assert_eq!(report.information_chars, 6);
        assert_eq!(report.total_chars, 9);
    }

    fn envelope_ok() -> String {
        // 注意 markdown 值形如 "## ...，须用 r###"..."### 避免提前终止。
        // Note: the markdown value looks like "## ..., so r###"..."### is required
        // to avoid premature termination.
        r###"{
            "schema_version": "source-ref-v1",
            "status": "ok",
            "wiki": {
                "title": "啵啵",
                "aliases": [],
                "tags": [],
                "markdown": "## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n"
            },
            "sections": [{
                "heading": "概述",
                "assertions": [
                    {"text": "啵啵", "ref_ids": ["r1"]},
                    {"text": "珍珠", "ref_ids": ["r2"]}
                ],
                "refs": [
                    {"id": "r1", "entity_id": "milk-tea:drink:boba", "source_revision": 1,
                     "pointer": "/fields/name", "value": "啵啵", "quote": "啵啵"},
                    {"id": "r2", "entity_id": "milk-tea:drink:boba", "source_revision": 1,
                     "pointer": "/fields/description", "value": "珍珠", "quote": "珍珠"}
                ]
            }]
        }"###
            .to_string()
    }

    // 严格解码：合法 envelope 通过。
    // Strict decoding: a legal envelope passes.
    #[test]
    fn decode_accepts_legal_envelope() {
        let evidence = decode_response(&envelope_ok()).unwrap();
        assert_eq!(evidence.schema_version, "source-ref-v1");
        assert_eq!(evidence.sections.len(), 1);
        assert!(evidence.usage.is_none());
        // 渲染器与 §5.1 示例 markdown 字节一致。
        // The renderer reproduces the §5.1 example markdown byte-for-byte.
        assert_eq!(
            render_canonical_markdown(&evidence),
            "## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n"
        );
    }

    // 严格解码：fence / 前后散文 / 重复 key / 未知字段 / 版本 / 超大 全部拒绝。
    // Strict decoding: fence / surrounding prose / duplicate keys / unknown fields /
    // version / oversize are all rejected.
    #[test]
    fn decode_rejects_escaped_payloads() {
        let fenced = format!("```json\n{}\n```", envelope_ok());
        assert!(matches!(
            decode_response(&fenced),
            Err(CompileFailure::InvalidOutput { .. })
        ));
        let prose = format!("Here is the JSON: {}", envelope_ok());
        assert!(decode_response(&prose).is_err());
        let trailing = format!("{}\nHope that helps!", envelope_ok());
        assert!(decode_response(&trailing).is_err());

        let dup_key = r#"{"schema_version":"source-ref-v1","schema_version":"source-ref-v1","status":"ok","wiki":null}"#;
        assert!(decode_response(dup_key).is_err());
        let nested_dup = r#"{"schema_version":"source-ref-v1","status":"ok","wiki":{"title":"a","title":"a","aliases":[],"tags":[],"markdown":""},"sections":[]}"#;
        assert!(decode_response(nested_dup).is_err());

        let unknown_field = r#"{"schema_version":"source-ref-v1","status":"ok","wiki":{"title":"a","aliases":[],"tags":[],"markdown":"","extra":1},"sections":[]}"#;
        assert!(decode_response(unknown_field).is_err());

        let bad_version = envelope_ok().replace("source-ref-v1", "source-ref-v2");
        assert!(decode_response(&bad_version).is_err());

        let oversize = format!("\"{}\"", "a".repeat(MAX_RESPONSE_BYTES + 1));
        assert!(decode_response(&oversize).is_err());
    }

    // status ok/error 互斥：错误分支不带 wiki；ok 分支缺 sections 拒绝。
    // status ok/error mutual exclusion: the error branch carries no wiki; the ok
    // branch without sections is rejected.
    #[test]
    fn decode_enforces_status_exclusivity() {
        let err_envelope = r#"{"schema_version":"source-ref-v1","status":"error","error":{"code":"MISSING_SOURCE_REFS","missing_pointers":["/fields/description"]}}"#;
        match decode_response(err_envelope) {
            Err(CompileFailure::InvalidOutput { code, .. }) => {
                assert_eq!(code, "ERROR_ENVELOPE_MISSING_SOURCE_REFS");
            }
            other => panic!("expected invalid output, got {other:?}"),
        }
        let err_with_wiki = r#"{"schema_version":"source-ref-v1","status":"error","error":{"code":"MISSING_SOURCE_REFS","missing_pointers":[]},"wiki":null}"#;
        assert!(decode_response(err_with_wiki).is_err());
        let ok_without_sections = r#"{"schema_version":"source-ref-v1","status":"ok","wiki":{"title":"a","aliases":[],"tags":[],"markdown":""}}"#;
        assert!(decode_response(ok_without_sections).is_err());
        let bad_error_code = r#"{"schema_version":"source-ref-v1","status":"error","error":{"code":"SOMETHING_ELSE","missing_pointers":[]}}"#;
        assert!(decode_response(bad_error_code).is_err());
    }

    // ref id 形状：r[1-9][0-9]{0,5}。
    // ref id shape: r[1-9][0-9]{0,5}.
    #[test]
    fn ref_id_pattern() {
        for ok in ["r1", "r9", "r10", "r123456"] {
            assert!(is_valid_ref_id(ok), "{ok} should match");
        }
        for bad in ["", "r", "r0", "R1", "r01", "r1234567", "ref1", "r1a"] {
            assert!(!is_valid_ref_id(bad), "{bad} should not match");
        }
    }
}
