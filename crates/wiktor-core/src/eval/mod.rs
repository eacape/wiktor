//! Step 5 golden 集加载器（spec：docs/design/step5-qug-build.md §3 D5、§6、§7 A9、§8 批 4）。
//! Step 5 golden-set loader (spec: docs/design/step5-qug-build.md §3 D5, §6, §7 A9,
//! §8 batch 4).
//!
//! 批5 起本模块还承载 A/B/C 三档评测（`metrics` 指标与判定、`runner` 三档
//! 运行器、`report` 双语 Markdown + JSON 报告；spec §3 D6、§6、§7 A10–A12）。
//! Since batch 5 this module also hosts the A/B/C evaluation (`metrics` for
//! aggregates and the decision, `runner` for the three-tier execution, and
//! `report` for the bilingual Markdown + JSON reports; spec §3 D6, §6,
//! §7 A10–A12).
//!
//! 职责：解析 `golden-queries.jsonl`（每行一个 JSON 记录）、结构与引用校验、
//! kind 配额校验、计算 `dataset_hash`（BLAKE3，固定前缀 + 原始文件字节，风格
//! 对齐 `compile/hash.rs`）。loader 不连库、不依赖 diesel：实体存在性校验接收
//! 调用方传入的合法 entity_id 集合；`filters` 的语义映射（→ `FilterCondition`）
//! 留给评测批次（批 5）。
//! Responsibilities: parse `golden-queries.jsonl` (one JSON record per line),
//! structural and reference validation, per-kind quota validation, and
//! `dataset_hash` computation (BLAKE3 over a fixed prefix + the raw file bytes,
//! styled after `compile/hash.rs`). The loader never touches the database and
//! does not depend on diesel: entity-existence validation receives the
//! caller-provided set of legal entity_ids; mapping `filters` onto
//! `FilterCondition` is left to the evaluation batch (batch 5).
//!
//! 兼容性（D5/A8/A9）：现有 34 条 legacy 记录逐字节保留且必须继续通过校验。
//! 它们没有 `id`/`kind`，期望字段叫 `expected_hits`；loader 以"无 kind = legacy"
//! 识别：id 合成为 `legacy-<行号三位零填充>`，kind 记为 [`GoldenKind::Legacy`]，
//! 不参与 kind 配额统计。新格式记录必须显式携带 `id`/`kind`/`expected_entity_ids`。
//! Compatibility (D5/A8/A9): the existing 34 legacy records are preserved
//! byte-for-byte and must keep validating. They carry no `id`/`kind` and name
//! their expectation field `expected_hits`; the loader detects them as
//! "no kind = legacy": the id is synthesized as `legacy-<line, zero-padded>`,
//! the kind is [`GoldenKind::Legacy`], and they do not count toward per-kind
//! quotas. New-format records must explicitly carry
//! `id`/`kind`/`expected_entity_ids`.
//!
//! 关于 D5 的"同一规范化 query 最多出现两次"：现有文件里"奶茶"以 4 种不同
//! `filters` 上下文出现（无过滤 / size / ingredients / sugar+size），按纯文本
//! 去重会误拒必须保留的合法文件；故去重键取
//! `(normalized_query, filter_signature)` —— 同一查询在完全相同的过滤上下文下
//! 最多出现两次。这是对任务字面表述的显式偏差，已记录于实现报告（比对
//! spec §9 的偏差登记约定）。
//! On D5's "the same normalized query at most twice": the existing file has
//! "奶茶" in 4 distinct `filters` contexts (none / size / ingredients /
//! sugar+size); a text-only key would reject the legacy file that must be kept.
//! The dedup key is therefore `(normalized_query, filter_signature)` — the same
//! query may appear at most twice under an identical filter context. This is an
//! explicit deviation from the literal task wording, recorded in the
//! implementation report (mirroring the spec §9 deviation convention).

use crate::compile::hash::canonical_json;
use crate::query_engine::qug::normalize;
use crate::types::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod metrics;
pub mod report;
pub mod runner;

pub use metrics::{
    decide, evaluate_variant, Decision, KindMetrics, QugDecision, SampleRun, VariantMetrics,
    EVAL_TOP_K_DEFAULT, QUG_ENABLE_GAIN_PP_THRESHOLD,
};
pub use report::{
    write_report_files, DecisionReport, EvaluationReport, FailedSample, TierReport,
    EVAL_REPORT_FILE_EN, EVAL_REPORT_FILE_JSON, EVAL_REPORT_FILE_ZH, EVAL_REPORT_SCHEMA_VERSION,
};
pub use runner::{run_evaluation, EvalConfig, EvalOutcome};

/// dataset_hash 的固定版本前缀；编码变更必须换前缀并做黄金哈希回归。
/// Fixed version prefix of dataset_hash; encoding changes must bump the prefix
/// and run golden-hash regression.
pub const GOLDEN_DATASET_HASH_PREFIX: &[u8] = b"wiktor.eval.golden.v1\0";

/// golden 记录的五类语义 kind（D5 配额 25/20/20/20/15）+ legacy 占位。
/// The five semantic kinds of a golden record (D5 quotas 25/20/20/20/15) plus
/// the legacy placeholder.
///
/// `Legacy` 只能由 loader 对"无 kind"的旧记录合成，永远不能从文件文本解析；
/// 文本里写 "legacy" 属于未知 kind，直接拒绝。
/// `Legacy` is only synthesized by the loader for records without a `kind`;
/// it can never be parsed from file text — a literal "legacy" in the file is an
/// unknown kind and is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldenKind {
    /// 同义/别名/黑话改写（页 frontmatter alias 语义）。
    /// Synonym / alias / slang rewriting (page-frontmatter alias semantics).
    Synonym,
    /// 意图短语（intents.yaml expansion / attribute 边驱动）。
    /// Intent phrases (driven by intents.yaml expansion / attribute edges).
    Intent,
    /// 否定查询，用 `must_exclude_entity_ids` 表达排除语义。
    /// Negated queries; exclusion semantics expressed via
    /// `must_exclude_entity_ids`.
    Negation,
    /// 属性过滤（fact 平面 filters 下推）。
    /// Attribute filtering (fact-plane filter pushdown).
    AttributeFilter,
    /// 负例：排除语义（must_exclude 非空）或零命中语义（expected 为空）。
    /// Negative: exclusion semantics (non-empty must_exclude) or zero-hit
    /// semantics (empty expected).
    Negative,
    /// Step2 旧格式（无 id/kind，expected_hits）；不参与配额。
    /// Step2 legacy format (no id/kind, expected_hits); not counted in quotas.
    Legacy,
}

impl GoldenKind {
    /// 稳定的字符串形式（报告与统计键使用）。
    /// Stable string form (used by reports and statistics keys).
    pub fn as_str(&self) -> &'static str {
        match self {
            GoldenKind::Synonym => "synonym",
            GoldenKind::Intent => "intent",
            GoldenKind::Negation => "negation",
            GoldenKind::AttributeFilter => "attribute_filter",
            GoldenKind::Negative => "negative",
            GoldenKind::Legacy => "legacy",
        }
    }

    /// 从文件文本解析；`Legacy` 不可达（见类型文档）。
    /// Parses from file text; `Legacy` is unreachable (see type docs).
    fn parse(s: &str) -> Option<GoldenKind> {
        match s {
            "synonym" => Some(GoldenKind::Synonym),
            "intent" => Some(GoldenKind::Intent),
            "negation" => Some(GoldenKind::Negation),
            "attribute_filter" => Some(GoldenKind::AttributeFilter),
            "negative" => Some(GoldenKind::Negative),
            _ => None,
        }
    }

    /// D5 最低配额；legacy 不设配额。
    /// D5 minimum quota; legacy has no quota.
    pub fn min_quota(&self) -> usize {
        match self {
            GoldenKind::Synonym => 25,
            GoldenKind::Intent => 20,
            GoldenKind::Negation => 20,
            GoldenKind::AttributeFilter => 20,
            GoldenKind::Negative => 15,
            GoldenKind::Legacy => 0,
        }
    }
}

/// golden 记录的扁平过滤表示（与 Step2 集成测试的 `GoldenFilters` 同构，键名
/// 沿用现有文件；`price_min/price_max`→price、`sugar_min/sugar_max`→sugar_level、
/// `size`→size、`ingredients/exclude_ingredients`→ingredient_ids，恰好覆盖
/// domain.yaml `query.filters` 白名单的四个事实字段）。
/// Flat filter representation of a golden record (isomorphic to the Step2
/// integration test's `GoldenFilters`; key names follow the existing file:
/// `price_min/price_max`→price, `sugar_min/sugar_max`→sugar_level, `size`→size,
/// `ingredients/exclude_ingredients`→ingredient_ids — exactly covering the four
/// fact fields of the domain.yaml `query.filters` whitelist).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GoldenFilters {
    pub price_min: Option<f64>,
    pub price_max: Option<f64>,
    pub sugar_min: Option<f64>,
    pub sugar_max: Option<f64>,
    pub size: Option<String>,
    pub ingredients: Vec<String>,
    pub exclude_ingredients: Vec<String>,
}

impl GoldenFilters {
    /// 是否完全为空（无任何过滤条件）。
    /// Whether it is entirely empty (no filter condition at all).
    pub fn is_empty(&self) -> bool {
        self.price_min.is_none()
            && self.price_max.is_none()
            && self.sugar_min.is_none()
            && self.sugar_max.is_none()
            && self.size.is_none()
            && self.ingredients.is_empty()
            && self.exclude_ingredients.is_empty()
    }
}

/// 校验后的 golden 查询记录。
/// A validated golden query record.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoldenQuery {
    /// 记录 id；legacy 记录合成为 `legacy-<行号>`。
    /// Record id; legacy records synthesize `legacy-<line>`.
    pub id: String,
    /// 文件中的 1-based 行号（错误上下文与报告用）。
    /// 1-based line number in the file (for error context and reports).
    pub line: usize,
    /// 原始查询文本（不 trim 不改写）。
    /// Raw query text (not trimmed, not rewritten).
    pub query: String,
    /// Step3 §3.1 归一化文本（去重键的一部分）。
    /// Step3 §3.1 normalized text (part of the dedup key).
    pub normalized_query: String,
    pub kind: GoldenKind,
    /// 期望命中的实体（可为空：negative 零命中语义）。
    /// Expected entities (may be empty: negative zero-hit semantics).
    pub expected_entity_ids: Vec<String>,
    /// 结果不得包含的实体（negation/negative 排除语义）。
    /// Entities that must not appear in results (negation/negative exclusion).
    pub must_exclude_entity_ids: Vec<String>,
    pub filters: GoldenFilters,
    pub notes: Option<String>,
}

/// 加载完成的 golden 集：记录、按 kind 统计与 dataset_hash（spec §6 报告契约）。
/// A loaded golden set: records, per-kind statistics and dataset_hash (spec §6
/// report contract).
#[derive(Debug, Clone, PartialEq)]
pub struct GoldenSet {
    queries: Vec<GoldenQuery>,
    kind_counts: BTreeMap<String, usize>,
    dataset_hash: String,
}

impl GoldenSet {
    /// 全部记录（含 legacy，保持文件顺序）。
    /// All records (legacy included, in file order).
    pub fn queries(&self) -> &[GoldenQuery] {
        &self.queries
    }

    /// 按 kind 的记录数统计（含 `legacy` 键；零计数不出现）。
    /// Per-kind record counts (the `legacy` key included; zero counts absent).
    pub fn kind_counts(&self) -> &BTreeMap<String, usize> {
        &self.kind_counts
    }

    /// 某一 kind 的记录数。
    /// Record count of one kind.
    pub fn count_of(&self, kind: GoldenKind) -> usize {
        self.kind_counts.get(kind.as_str()).copied().unwrap_or(0)
    }

    /// dataset_hash：`BLAKE3(GOLDEN_DATASET_HASH_PREFIX || 原始文件字节)`。
    /// dataset_hash: `BLAKE3(GOLDEN_DATASET_HASH_PREFIX || raw file bytes)`.
    pub fn dataset_hash(&self) -> &str {
        &self.dataset_hash
    }

    /// 记录总数。
    /// Total record count.
    pub fn len(&self) -> usize {
        self.queries.len()
    }

    /// 是否为空集。
    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.queries.is_empty()
    }

    /// 从既有记录程序化合成 golden 集（评测批5 fixture 与编程消费者用）。
    /// Synthesizes a golden set from pre-built records (batch-5 evaluation
    /// fixtures and programmatic consumers).
    ///
    /// 与 [`load_golden_set`] 的差异：**不做任何校验**（不查配额、不查实体
    /// 存在性、不查去重）——调用方保证记录合法。dataset_hash 对记录的
    /// canonical JSON 序列化计算（文件加载集对原始文件字节计算），同输入
    /// 稳定、不同输入互异，满足报告可复现审计口径。
    /// Unlike [`load_golden_set`], **no validation is performed** (no quotas,
    /// no entity existence, no dedup) — the caller guarantees the records.
    /// dataset_hash is computed over the records' canonical JSON serialization
    /// (file-loaded sets hash the raw file bytes); stable for identical input
    /// and distinct across different inputs, satisfying the report's
    /// reproducible-audit contract.
    pub fn from_queries(queries: Vec<GoldenQuery>) -> GoldenSet {
        let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
        for q in &queries {
            *kind_counts.entry(q.kind.as_str().to_string()).or_insert(0) += 1;
        }
        // GoldenQuery/GoldenFilters 的 serde 序列化仅对 NaN/Infinity f64 失败，
        // 此处退化为空字节域（hash 仍稳定）；文件路径不走本函数。
        // serde fails only on NaN/Infinity f64; degrade to an empty byte domain
        // (the hash stays stable). The file path never goes through here.
        let bytes = serde_json::to_vec(&queries).unwrap_or_default();
        GoldenSet {
            queries,
            kind_counts,
            dataset_hash: golden_dataset_hash(&bytes),
        }
    }
}

/// 文件字节 → dataset_hash（小写 64 位 BLAKE3 hex）。
/// File bytes → dataset_hash (lowercase 64-hex BLAKE3).
///
/// 风格对齐 `compile/hash.rs`：固定版本前缀 + 单一原始字节域。任何字节变化
/// （包括行序与空白）都会改变 hash，保证报告里的 dataset_hash 可复现审计。
/// Styled after `compile/hash.rs`: a fixed version prefix + a single raw-byte
/// domain. Any byte change (including line order and whitespace) changes the
/// hash, keeping dataset_hash in reports reproducibly auditable.
pub fn golden_dataset_hash(bytes: &[u8]) -> String {
    let mut framed = Vec::with_capacity(GOLDEN_DATASET_HASH_PREFIX.len() + bytes.len());
    framed.extend_from_slice(GOLDEN_DATASET_HASH_PREFIX);
    framed.extend_from_slice(bytes);
    blake3::hash(&framed).to_hex().to_string()
}

/// legacy 记录的合成 id（行号三位零填充）。
/// Synthesized id for legacy records (zero-padded line number).
fn legacy_id(line: usize) -> String {
    format!("legacy-{line:03}")
}

/// 线上（文件）记录结构。字段集 = 现有 34 条的字段集 ∪ 新格式字段集；
/// `deny_unknown_fields` 因此对两代格式都安全（已核对现有记录仅含
/// query/expected_hits/filters）。
/// The wire (file) record. Field set = legacy 34 records' fields ∪ new-format
/// fields; `deny_unknown_fields` is therefore safe for both generations (the
/// existing records only contain query/expected_hits/filters, verified).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawGoldenRecord {
    id: Option<String>,
    query: String,
    kind: Option<String>,
    expected_entity_ids: Option<Vec<String>>,
    expected_hits: Option<Vec<String>>,
    must_exclude_entity_ids: Option<Vec<String>>,
    filters: Option<GoldenFilters>,
    notes: Option<String>,
}

/// 过滤条件的稳定签名（去重键的第二分量）。
/// Stable filter signature (second component of the dedup key).
fn filter_signature(line: usize, filters: &GoldenFilters) -> Result<Vec<u8>> {
    let value = serde_json::to_value(filters).map_err(|e| {
        Error::Validation(format!("golden line {line}: filters not serializable: {e}"))
    })?;
    canonical_json(&value).map_err(|e| {
        Error::Validation(format!(
            "golden line {line}: filters canonicalization failed: {e}"
        ))
    })
}

/// 校验单个实体引用集合，返回未知引用错误（带字段名上下文）。
/// Validates one entity-reference collection; returns unknown-reference errors
/// with field-name context.
fn check_refs(
    line: usize,
    field: &str,
    refs: &[String],
    known_entities: &BTreeSet<String>,
) -> Result<()> {
    for id in refs {
        if !known_entities.contains(id) {
            return Err(Error::Validation(format!(
                "golden line {line}: {field} references unknown entity {id:?}"
            )));
        }
    }
    Ok(())
}

/// 加载并校验 golden 集。
/// Loads and validates a golden set.
///
/// 拒绝路径（A9）：非法 JSON、id/query 缺失或为空、未知 kind、新格式缺
/// id/expected_entity_ids、legacy 记录混用新字段或缺 expected_hits、
/// expected/must_exclude 引用未知实体、正例 kind（synonym/intent/
/// attribute_filter）期望为空、negation 缺 must_exclude、negative 既无排除
/// 也无零命中语义、attribute_filter 无 filters、重复 id、同
/// (规范化 query, 过滤签名) 超过两次、kind 配额不足。
/// Rejection paths (A9): invalid JSON, missing/empty id or query, unknown kind,
/// new-format missing id/expected_entity_ids, legacy records mixing new fields
/// or missing expected_hits, expected/must_exclude referencing unknown
/// entities, positive kinds (synonym/intent/attribute_filter) with empty
/// expectations, negation without must_exclude, negative with neither
/// exclusion nor zero-hit semantics, attribute_filter without filters,
/// duplicate ids, more than two occurrences of the same (normalized query,
/// filter signature), and insufficient kind quotas.
pub fn load_golden_set(bytes: &[u8], known_entities: &BTreeSet<String>) -> Result<GoldenSet> {
    let dataset_hash = golden_dataset_hash(bytes);
    let text = std::str::from_utf8(bytes)
        .map_err(|e| Error::Validation(format!("golden set: file is not UTF-8: {e}")))?;

    let mut queries: Vec<GoldenQuery> = Vec::new();
    let mut id_lines: BTreeMap<String, usize> = BTreeMap::new();
    // 去重键 (normalized_query, filter_signature) → 出现行号列表。
    // Dedup key (normalized_query, filter_signature) → list of occurrence lines.
    let mut query_occurrences: BTreeMap<(String, Vec<u8>), Vec<usize>> = BTreeMap::new();

    for (idx, line_text) in text.lines().enumerate() {
        let line = idx + 1;
        if line_text.trim().is_empty() {
            // 空行跳过（行号仍按物理行计，保证错误定位准确）。
            // Blank lines are skipped (physical line numbering keeps error
            // locations accurate).
            continue;
        }

        // 1) 非法 JSON → 带行号的 Validation。
        // 1) Invalid JSON → line-scoped Validation.
        let raw: RawGoldenRecord = serde_json::from_str(line_text).map_err(|e| {
            Error::Validation(format!("golden line {line}: invalid JSON record: {e}"))
        })?;

        // 2) query 缺失/为空。
        // 2) Missing/empty query.
        if raw.query.trim().is_empty() {
            return Err(Error::Validation(format!(
                "golden line {line}: query is missing or empty"
            )));
        }
        // 3) 规范化（Step3 §3.1；同时拒绝空串与超长）。
        // 3) Normalization (Step3 §3.1; rejects empty and overlong phrases).
        let normalized_query = normalize(&raw.query)
            .map_err(|e| Error::Validation(format!("golden line {line}: {e}")))?;

        // 4) id：新格式必填非空；legacy（无 kind）缺省时按行号合成。
        // 4) id: required non-empty for new format; synthesized per line for
        //    legacy (no kind).
        let id = match &raw.id {
            Some(id) => {
                if id.trim().is_empty() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: id is empty"
                    )));
                }
                id.clone()
            }
            None => {
                if raw.kind.is_some() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: id is required for records with kind (D5)"
                    )));
                }
                legacy_id(line)
            }
        };

        // 5) kind：未知值拒绝（列出合法值）；无 kind 走 legacy 路径。
        // 5) kind: unknown values are rejected (legal values listed); no kind
        //    takes the legacy path.
        let kind = match raw.kind.as_deref() {
            Some(s) => GoldenKind::parse(s).ok_or_else(|| {
                Error::Validation(format!(
                    "golden line {line}: unknown kind {s:?} (expected one of: synonym, \
                     intent, negation, attribute_filter, negative)"
                ))
            })?,
            None => {
                // legacy 记录不得混用新格式字段。
                // Legacy records must not mix in new-format fields.
                if raw.expected_entity_ids.is_some() || raw.must_exclude_entity_ids.is_some() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: record without kind must not carry \
                         expected_entity_ids/must_exclude_entity_ids (declare kind first)"
                    )));
                }
                let expected_hits = raw.expected_hits.as_ref().ok_or_else(|| {
                    Error::Validation(format!(
                        "golden line {line}: legacy record (no kind) is missing expected_hits"
                    ))
                })?;
                // legacy 期望同样必须引用真实实体。
                // Legacy expectations must reference real entities too.
                check_refs(line, "expected_hits", expected_hits, known_entities)?;
                let query = GoldenQuery {
                    id,
                    line,
                    query: raw.query,
                    normalized_query,
                    kind: GoldenKind::Legacy,
                    expected_entity_ids: expected_hits.clone(),
                    must_exclude_entity_ids: Vec::new(),
                    filters: raw.filters.unwrap_or_default(),
                    notes: raw.notes,
                };
                register_record(&mut queries, &mut id_lines, &mut query_occurrences, query)?;
                continue;
            }
        };

        // 6) 新格式：expected_entity_ids 必填；expected_hits 仅限 legacy。
        // 6) New format: expected_entity_ids required; expected_hits is
        //    legacy-only.
        if raw.expected_hits.is_some() {
            return Err(Error::Validation(format!(
                "golden line {line}: expected_hits is legacy-only; use expected_entity_ids"
            )));
        }
        let expected_entity_ids = raw.expected_entity_ids.ok_or_else(|| {
            Error::Validation(format!(
                "golden line {line}: expected_entity_ids is required for records with kind (D5)"
            ))
        })?;
        let must_exclude_entity_ids = raw.must_exclude_entity_ids.unwrap_or_default();
        let filters = raw.filters.unwrap_or_default();

        // 7) 引用存在性（调用方传入合法实体集合；loader 不连库）。
        // 7) Reference existence (caller-provided legal entity set; the loader
        //    never touches the database).
        check_refs(
            line,
            "expected_entity_ids",
            &expected_entity_ids,
            known_entities,
        )?;
        check_refs(
            line,
            "must_exclude_entity_ids",
            &must_exclude_entity_ids,
            known_entities,
        )?;

        // 8) kind 语义约束（D5：negative 可空期望但必须有排除或零命中语义）。
        // 8) Kind semantics (D5: negative may have empty expectations but must
        //    carry exclusion or zero-hit semantics).
        match kind {
            GoldenKind::Synonym | GoldenKind::Intent | GoldenKind::AttributeFilter => {
                if expected_entity_ids.is_empty() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: kind {kind:?} requires non-empty expected_entity_ids \
                         (use negative for zero-hit semantics)"
                    )));
                }
            }
            GoldenKind::Negation => {
                if must_exclude_entity_ids.is_empty() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: kind negation requires non-empty \
                         must_exclude_entity_ids to express the exclusion"
                    )));
                }
            }
            GoldenKind::Negative => {
                if must_exclude_entity_ids.is_empty() && !expected_entity_ids.is_empty() {
                    return Err(Error::Validation(format!(
                        "golden line {line}: kind negative requires non-empty \
                         must_exclude_entity_ids or empty expected_entity_ids (zero-hit semantics)"
                    )));
                }
            }
            GoldenKind::Legacy => unreachable!("legacy records never reach the new-format path"),
        }
        if kind == GoldenKind::AttributeFilter && filters.is_empty() {
            return Err(Error::Validation(format!(
                "golden line {line}: kind attribute_filter requires non-empty filters"
            )));
        }

        let query = GoldenQuery {
            id,
            line,
            query: raw.query,
            normalized_query,
            kind,
            expected_entity_ids,
            must_exclude_entity_ids,
            filters,
            notes: raw.notes,
        };
        register_record(&mut queries, &mut id_lines, &mut query_occurrences, query)?;
    }

    // 9) 全局：空集拒绝。
    // 9) Global: reject an empty set.
    if queries.is_empty() {
        return Err(Error::Validation(
            "golden set: no golden records found".into(),
        ));
    }

    // 10) 全局：kind 配额（仅显式 kind 的记录参与，legacy 不计入）。
    // 10) Global: kind quotas (only explicitly kinded records count; legacy
    //     records do not).
    let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
    for q in &queries {
        *kind_counts.entry(q.kind.as_str().to_string()).or_insert(0) += 1;
    }
    for kind in [
        GoldenKind::Synonym,
        GoldenKind::Intent,
        GoldenKind::Negation,
        GoldenKind::AttributeFilter,
        GoldenKind::Negative,
    ] {
        let count = kind_counts.get(kind.as_str()).copied().unwrap_or(0);
        let min = kind.min_quota();
        if count < min {
            return Err(Error::Validation(format!(
                "golden set: quota insufficient for kind {}: {count} < {min}",
                kind.as_str()
            )));
        }
    }

    Ok(GoldenSet {
        queries,
        kind_counts,
        dataset_hash,
    })
}

/// 单记录登记：重复 id 与 (规范化 query, 过滤签名) 频次检查。
/// Per-record registration: duplicate-id and (normalized query, filter
/// signature) frequency checks.
fn register_record(
    queries: &mut Vec<GoldenQuery>,
    id_lines: &mut BTreeMap<String, usize>,
    query_occurrences: &mut BTreeMap<(String, Vec<u8>), Vec<usize>>,
    query: GoldenQuery,
) -> Result<()> {
    let line = query.line;
    if let Some(first_line) = id_lines.insert(query.id.clone(), line) {
        return Err(Error::Validation(format!(
            "golden set: duplicate golden id {:?} at lines {first_line} and {line}",
            query.id
        )));
    }
    let sig = filter_signature(line, &query.filters)?;
    let key = (query.normalized_query.clone(), sig);
    let occurrences = query_occurrences.entry(key).or_default();
    occurrences.push(line);
    if occurrences.len() > 2 {
        return Err(Error::Validation(format!(
            "golden set: normalized query {:?} with identical filters appears {} times \
             (lines {:?}, limit 2)",
            query.normalized_query,
            occurrences.len(),
            occurrences
        )));
    }
    queries.push(query);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// milk-tea 领域知识平面的 20 个实体 id（8 drinks + 6 ingredients +
    /// 2 concepts + 2 brands + 2 practices，来自 seed-wiki frontmatter 与
    /// compile-entities.jsonl）。
    /// The 20 knowledge-plane entity ids of the milk-tea domain (8 drinks +
    /// 6 ingredients + 2 concepts + 2 brands + 2 practices, from seed-wiki
    /// frontmatter and compile-entities.jsonl).
    const KNOWN_ENTITIES: [&str; 20] = [
        "milk-tea:drink:boba-milk-tea",
        "milk-tea:drink:cheese-tea",
        "milk-tea:drink:coconut-sago",
        "milk-tea:drink:lemon-tea",
        "milk-tea:drink:mango-pomelo-sago",
        "milk-tea:drink:mango-smoothie",
        "milk-tea:drink:matcha-latte",
        "milk-tea:drink:tapioca-milk-tea",
        "milk-tea:ingredient:cheese-foam",
        "milk-tea:ingredient:coconut-jelly",
        "milk-tea:ingredient:pearl",
        "milk-tea:ingredient:red-bean",
        "milk-tea:ingredient:sago",
        "milk-tea:ingredient:taro-ball",
        "milk-tea:concept:fruit-tea",
        "milk-tea:concept:milk-tea",
        "milk-tea:brand:demo-a",
        "milk-tea:brand:demo-b",
        "milk-tea:practice:half-sugar",
        "milk-tea:practice:no-ice",
    ];

    fn known_set() -> BTreeSet<String> {
        KNOWN_ENTITIES.iter().map(|s| s.to_string()).collect()
    }

    /// examples/milk-tea 目录路径（与 tests/step2_golden.rs 同一定位方式）。
    /// Path to examples/milk-tea (same resolution as tests/step2_golden.rs).
    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("milk-tea")
    }

    fn load_str(text: &str) -> Result<GoldenSet> {
        load_golden_set(text.as_bytes(), &known_set())
    }

    fn assert_validation(err: Error, needle: &str) {
        match err {
            Error::Validation(msg) => assert!(
                msg.contains(needle),
                "validation message {msg:?} should contain {needle:?}"
            ),
            other => panic!("expected Error::Validation, got {other:?}"),
        }
    }

    fn valid_new_record() -> String {
        r#"{"id":"synonym-99","query":"木薯珍珠","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:pearl"]}"#.into()
    }

    /// 程序化生成恰好满足配额的最小合法集（id/query 互异，各 kind 语义约束
    /// 均满足：正例非空期望、negation/negative 带 must_exclude、
    /// attribute_filter 带非空 filters）。供正路径测试打底。
    /// Programmatically generates the smallest legal set that exactly meets the
    /// quotas (unique ids/queries; every kind's semantic constraints satisfied:
    /// non-empty expectations for positive kinds, must_exclude for
    /// negation/negative, non-empty filters for attribute_filter). Base for
    /// positive-path tests.
    fn full_quota_text() -> String {
        let plan: [(&str, usize); 5] = [
            ("synonym", 25),
            ("intent", 20),
            ("negation", 20),
            ("attribute_filter", 20),
            ("negative", 15),
        ];
        let mut lines: Vec<String> = Vec::new();
        for (kind, n) in plan {
            for i in 1..=n {
                lines.push(format!(
                    r#"{{"id":"{kind}-{i:02}","query":"{kind} probe {i:02}","kind":"{kind}","expected_entity_ids":["milk-tea:ingredient:taro-ball"],"must_exclude_entity_ids":["milk-tea:ingredient:pearl"],"filters":{{"price_max":10}}}}"#
                ));
            }
        }
        lines.join("\n")
    }

    // A9：现有 34 条 legacy + 100 条新记录全部通过校验，配额达标，总数 134。
    // A9: the existing 34 legacy records plus 100 new records all validate,
    // quotas are met, total is 134.
    #[test]
    fn real_golden_file_loads_with_quotas() {
        let path = examples_dir().join("golden-queries.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        let set = load_golden_set(&bytes, &known_set()).unwrap();

        eprintln!(
            "golden total={}, dataset_hash={}, kind_counts={:?}",
            set.len(),
            set.dataset_hash(),
            set.kind_counts()
        );
        // 逐字节保留约束：legacy 记录固定 34 条。
        // Byte-preservation constraint: legacy records stay at exactly 34.
        assert_eq!(
            set.count_of(GoldenKind::Legacy),
            34,
            "legacy records pinned"
        );
        assert_eq!(set.len(), 134, "34 legacy + 100 new records");
        assert!(set.len() >= 100, "total must be at least 100 (A9)");
        for kind in [
            GoldenKind::Synonym,
            GoldenKind::Intent,
            GoldenKind::Negation,
            GoldenKind::AttributeFilter,
            GoldenKind::Negative,
        ] {
            let count = set.count_of(kind);
            assert!(
                count >= kind.min_quota(),
                "kind {} count {count} below quota {}",
                kind.as_str(),
                kind.min_quota()
            );
        }
        // 每条记录行号与 id 唯一性 sanity。
        // Sanity: per-record line numbers and unique ids.
        let mut ids: Vec<&str> = set.queries().iter().map(|q| q.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), set.len(), "ids must be unique after loading");
    }

    // dataset_hash：同字节稳定、对前缀/字节敏感、手算一致。
    // dataset_hash: stable for identical bytes, sensitive to prefix/bytes,
    // and matches a manual recomputation.
    #[test]
    fn dataset_hash_is_stable_and_prefix_framed() {
        let a = golden_dataset_hash(b"hello\n");
        let b = golden_dataset_hash(b"hello\n");
        assert_eq!(a, b, "same bytes must yield the same hash");
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        // 手算复核：前缀 + 原始字节。
        // Manual recomputation: prefix + raw bytes.
        let mut framed = Vec::from(GOLDEN_DATASET_HASH_PREFIX);
        framed.extend_from_slice(b"hello\n");
        assert_eq!(a, blake3::hash(&framed).to_hex().to_string());

        // 内容或字节差异必须改变 hash（含尾随空白）。
        // Content or byte differences must change the hash (trailing
        // whitespace included).
        assert_ne!(a, golden_dataset_hash(b"hellp\n"));
        assert_ne!(a, golden_dataset_hash(b"hello"));
        assert_ne!(a, golden_dataset_hash(b"hello \n"));
        // 前缀参与哈希：无前缀的裸 BLAKE3 与 dataset_hash 不同。
        // The prefix participates: bare BLAKE3 differs from dataset_hash.
        assert_ne!(a, blake3::hash(b"hello\n").to_hex().to_string());
    }

    // 拒绝：重复 id。
    // Rejection: duplicate id.
    #[test]
    fn rejects_duplicate_id() {
        let text = format!(
            "{}\n{}",
            valid_new_record(),
            r#"{"id":"synonym-99","query":"芋头圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#
        );
        let err = load_str(&text).unwrap_err();
        assert_validation(err, "duplicate golden id \"synonym-99\" at lines 1 and 2");
    }

    // 拒绝：未知 kind（含字面 "legacy"）。
    // Rejection: unknown kind (including the literal "legacy").
    #[test]
    fn rejects_unknown_kind() {
        let text = r#"{"id":"x-1","query":"芋圆","kind":"topping","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "unknown kind \"topping\"");

        let text = r#"{"id":"x-2","query":"芋圆","kind":"legacy","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "unknown kind \"legacy\"");
    }

    // 拒绝：expected 引用未知实体。
    // Rejection: expected references an unknown entity.
    #[test]
    fn rejects_unknown_expected_entity() {
        let text = r#"{"id":"x-1","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:pearl","milk-tea:ingredient:boba-jelly"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(
            err,
            "line 1: expected_entity_ids references unknown entity \"milk-tea:ingredient:boba-jelly\"",
        );
    }

    // 拒绝：must_exclude 引用未知实体。
    // Rejection: must_exclude references an unknown entity.
    #[test]
    fn rejects_unknown_must_exclude_entity() {
        let text = r#"{"id":"x-1","query":"不要珍珠的柠檬茶","kind":"negation","expected_entity_ids":["milk-tea:drink:lemon-tea"],"must_exclude_entity_ids":["milk-tea:drink:ghost-tea"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(
            err,
            "line 1: must_exclude_entity_ids references unknown entity \"milk-tea:drink:ghost-tea\"",
        );
    }

    // 拒绝：非法 JSON（带行号）。
    // Rejection: invalid JSON (with line number).
    #[test]
    fn rejects_invalid_json() {
        let text = format!("{}\n{{\"id\":\"x-2\",\"query\":", valid_new_record());
        let err = load_str(&text).unwrap_err();
        assert_validation(err, "golden line 2: invalid JSON record");
    }

    // 拒绝：kind 配额不足。
    // Rejection: insufficient kind quota.
    #[test]
    fn rejects_insufficient_quota() {
        let text = valid_new_record();
        let err = load_str(&text).unwrap_err();
        assert_validation(err, "quota insufficient for kind");
    }

    // 拒绝：同一规范化 query + 相同过滤上下文出现 3 次；2 次允许。
    // Rejection: the same normalized query with identical filters 3 times;
    // twice is allowed.
    #[test]
    fn rejects_query_more_than_twice_same_filters() {
        // 同 id 三连会先触发重复 id，这里用三个不同 id 表达同一
        // (规范化 query, 过滤签名) 出现 3 次。
        // Three identical ids would trip duplicate-id first; use three distinct
        // ids for the same (normalized query, filter signature) 3 times.
        let one = r#"{"id":"a-1","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let two = r#"{"id":"a-1b","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let three = r#"{"id":"a-1c","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        // 同一查询、不同 filters → 不同去重键，不计数。
        // Same query, different filters → different dedup key, not counted.
        let other_filters = r#"{"id":"a-2","query":"芋圆","kind":"attribute_filter","expected_entity_ids":["milk-tea:ingredient:taro-ball"],"filters":{"price_max":20}}"#;
        let text = format!("{one}\n{other_filters}\n{two}\n{three}");
        let err = load_str(&text).unwrap_err();
        assert_validation(err, "appears 3 times");

        // 两次（不同 id、同 query、同 filters）合法：以满配额集为底避免
        // 提前触发配额错误。
        // Two occurrences (distinct ids, same query and filters) are legal:
        // base on the quota-meeting set to avoid the earlier quota error.
        let dup2 = r#"{"id":"a-1b","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let text = format!("{}\n{one}\n{dup2}", full_quota_text());
        assert!(
            load_str(&text).is_ok(),
            "twice with identical filters is legal"
        );
    }

    // 拒绝：id 为空 / 新格式缺 id / query 为空。
    // Rejection: empty id / missing id on new format / empty query.
    #[test]
    fn rejects_empty_or_missing_id_and_query() {
        let text = r#"{"id":"  ","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "line 1: id is empty");

        let text = r#"{"query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "line 1: id is required for records with kind");

        let text = r#"{"id":"x-1","query":"   ","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "line 1: query is missing or empty");
    }

    // 拒绝：negative 既无排除也无零命中语义。
    // Rejection: negative with neither exclusion nor zero-hit semantics.
    #[test]
    fn rejects_negative_without_exclusion_or_zero_hit() {
        let text = r#"{"id":"x-1","query":"芋圆","kind":"negative","expected_entity_ids":["milk-tea:ingredient:taro-ball"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(
            err,
            "kind negative requires non-empty must_exclude_entity_ids or empty expected_entity_ids",
        );
    }

    // 拒绝：negation 缺 must_exclude。
    // Rejection: negation without must_exclude.
    #[test]
    fn rejects_negation_without_must_exclude() {
        let text = r#"{"id":"x-1","query":"不要珍珠的柠檬茶","kind":"negation","expected_entity_ids":["milk-tea:drink:lemon-tea"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(
            err,
            "kind negation requires non-empty must_exclude_entity_ids",
        );
    }

    // 拒绝：attribute_filter 无 filters；正例 kind 期望为空。
    // Rejection: attribute_filter without filters; positive kinds with empty
    // expectations.
    #[test]
    fn rejects_attribute_filter_without_filters() {
        let text = r#"{"id":"x-1","query":"芋圆","kind":"attribute_filter","expected_entity_ids":["milk-tea:ingredient:taro-ball"],"filters":{}}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "kind attribute_filter requires non-empty filters");

        let text = r#"{"id":"x-2","query":"芋圆","kind":"synonym","expected_entity_ids":[]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "requires non-empty expected_entity_ids");
    }

    // legacy 兼容：expected_hits 记录通过、合成 id、不计配额；legacy 缺
    // expected_hits 或混用新字段被拒。
    // Legacy compatibility: expected_hits records pass with synthesized ids
    // and no quota contribution; legacy missing expected_hits or mixing new
    // fields is rejected.
    #[test]
    fn legacy_records_and_their_rejections() {
        // legacy 正常解析：合成 id、计入 legacy、不参与 kind 配额（底座为
        // 满配额集，否则先触发配额错误）。
        // Legacy records parse fine: synthesized id, counted as legacy, and
        // excluded from kind quotas (base is the quota-meeting set, otherwise
        // the quota error fires first).
        let text = format!(
            "{}\n{{\"query\":\"波霸奶茶\",\"expected_hits\":[\"milk-tea:drink:boba-milk-tea\"],\"filters\":{{}}}}",
            full_quota_text()
        );
        let set = load_str(&text).unwrap();
        assert_eq!(set.count_of(GoldenKind::Legacy), 1);
        let legacy = set
            .queries()
            .iter()
            .find(|q| q.kind == GoldenKind::Legacy)
            .unwrap();
        assert_eq!(legacy.id, "legacy-101");
        assert_eq!(legacy.line, 101);
        assert_eq!(
            legacy.expected_entity_ids,
            vec!["milk-tea:drink:boba-milk-tea".to_string()]
        );

        let text = r#"{"query":"波霸奶茶"}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "legacy record (no kind) is missing expected_hits");

        let text = r#"{"query":"波霸奶茶","expected_hits":["milk-tea:drink:boba-milk-tea"],"expected_entity_ids":["milk-tea:drink:boba-milk-tea"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "record without kind must not carry");

        // 空白行跳过且不影响行号定位。
        // Blank lines are skipped without breaking line-number reporting.
        let text = format!("\n{}\n\n{{\"id\":\"x-9\",\"query\":\"芋圆\",\"kind\":\"synonym\",\"expected_entity_ids\":[\"milk-tea:ghost\"]}}", valid_new_record());
        let err = load_str(&text).unwrap_err();
        assert_validation(err, "golden line 4:");
    }

    // deny_unknown_fields：未知字段拒绝（两代格式字段集之外的任何字段）。
    // deny_unknown_fields: unknown fields are rejected (anything outside the
    // union of both generations' field sets).
    #[test]
    fn rejects_unknown_fields() {
        let text = r#"{"id":"x-1","query":"芋圆","kind":"synonym","expected_entity_ids":["milk-tea:ingredient:taro-ball"],"expected_kinds":["taro"]}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "invalid JSON record");

        // legacy 记录的未知字段同样拒绝。
        // Unknown fields on legacy records are rejected too.
        let text =
            r#"{"query":"波霸奶茶","expected_hits":["milk-tea:drink:boba-milk-tea"],"weight":3}"#;
        let err = load_str(text).unwrap_err();
        assert_validation(err, "invalid JSON record");
    }
}
