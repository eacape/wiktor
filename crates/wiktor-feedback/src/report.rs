//! Step 6 批3：反馈报告模型与三件套输出（spec `step6-feedback-loop.md` §6、
//! §8、§10 A13）。
//! Step 6 batch 3: the feedback report model and the three-file output
//! (spec `step6-feedback-loop.md` §6, §8, §10 A13).
//!
//! 契约（§6/§8）：
//! - 报告 JSON 必须包含窗口（domain/from/to）、阈值（min_events=5、
//!   adoption_threshold=0.20）、输入行数、三类样本（zero_recall/
//!   rewrite_failures/low_quality）、建议（suggested_reviews）与 `report_hash`；
//! - `report_hash` = `BLAKE3(REPORT_HASH_PREFIX ++ canonical_json(报告 JSON 去
//!   hash 字段))`：前缀风格对齐 `compile::hash.rs` 的域定哈希（`wiktor.compile.
//!   hash.v1\0` → 本处定为 `wiktor.feedback.report.v1\0`），canonical 字节编码
//!   复用 `wiktor_core::compile::hash::canonical_json`（键序稳定 + 长度域防碰
//!   撞，spec §6 "canonical JSON 的 BLAKE3"）；编码/前缀变更必须换版本前缀；
//! - 双语三件套：`step6-feedback-report.md`（中文）+ `step6-feedback-report.
//!   en.md`（英文逐节对应）+ `step6-feedback-report.json`；数字、ID、hash 完全
//!   一致（双语共用同一模型的同一格式化函数），"啵啵""珍珠"等领域字面量是数据
//!   的一部分，不翻译；
//! - 报告模型不含时间戳等非确定字段——同一输入两次分析产出逐字节一致的三件套
//!   （A12）；报告项数量硬上限（1000 项/类，A13：超限报错不截断，复用 spec §9
//!   上限精神）。
//!
//! Contract (§6/§8):
//! - the report JSON must carry the window (domain/from/to), thresholds
//!   (min_events=5, adoption_threshold=0.20), input row counts, the three
//!   sample classes (zero_recall/rewrite_failures/low_quality), the
//!   suggestions (suggested_reviews) and `report_hash`;
//! - `report_hash` = `BLAKE3(REPORT_HASH_PREFIX ++ canonical_json(report JSON
//!   minus the hash field))`: the prefix style aligns with `compile::hash.rs`
//!   domain hashing (`wiktor.compile.hash.v1\0` → fixed here as
//!   `wiktor.feedback.report.v1\0`), and the canonical byte encoding reuses
//!   `wiktor_core::compile::hash::canonical_json` (stable key order +
//!   length-framing against collisions, spec §6 "BLAKE3 of the canonical
//!   JSON"); any encoding/prefix change must bump the version prefix;
//! - the bilingual trio: `step6-feedback-report.md` (Chinese) +
//!   `step6-feedback-report.en.md` (English, section-by-section mirror) +
//!   `step6-feedback-report.json`; numbers, ids and the hash are identical
//!   (both languages share one model and the same formatters); domain literals
//!   such as "啵啵"/"珍珠" are part of the data and stay untranslated;
//! - the report model carries no timestamps or other nondeterministic fields —
//!   two analyses over one input yield byte-identical files (A12); report-item
//!   counts hit a hard cap (1000 per class, A13: over-cap errors instead of
//!   truncation, reusing the spec §9 cap spirit).

use serde::Serialize;
use std::path::Path;
use wiktor_core::compile::hash::canonical_json;
use wiktor_core::kernel::feedback_store::ReviewSuggestionInput;
use wiktor_core::types::error::{Error, Result};

/// 报告 JSON 的 schema 版本。
/// The report JSON's schema version.
pub const REPORT_SCHEMA_VERSION: &str = "step6-feedback-report.v1";

/// 中文报告文件名（§8）。
/// Chinese report file name (§8).
pub const REPORT_FILE_ZH: &str = "step6-feedback-report.md";

/// 英文报告文件名（§8）。
/// English report file name (§8).
pub const REPORT_FILE_EN: &str = "step6-feedback-report.en.md";

/// JSON 结果文件名（§8）。
/// JSON result file name (§8).
pub const REPORT_FILE_JSON: &str = "step6-feedback-report.json";

/// 报告项数量硬上限（1000 项/类；A13：超限报错不截断；与 review list 上限同
/// 量级，spec §9 上限精神）。
/// Hard cap on report items (1000 per class; A13: over-cap errors instead of
/// truncation; same magnitude as the review-list cap, spec §9 cap spirit).
pub const REPORT_MAX_ITEMS_PER_CLASS: usize = 1000;

/// report_hash 的版本前缀（域定哈希惯例对齐 compile/hash.rs 的
/// `wiktor.compile.hash.v1\0`；canonical 编码或字段集变更必须换版本）。
/// The report_hash version prefix (domain-hash convention aligned with
/// compile/hash.rs's `wiktor.compile.hash.v1\0`; any canonical-encoding or
/// field-set change must bump the version).
pub const REPORT_HASH_PREFIX: &[u8] = b"wiktor.feedback.report.v1\0";

/// 阈值切片（MVP 固定并写进报告，D11：阈值以后按 domain 版本化）。
/// The threshold slice (fixed for the MVP and written into the report; D11:
/// thresholds become domain-versioned later).
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackThresholds {
    pub min_events: u32,
    pub adoption_threshold: f64,
}

/// 零召回/改写失败的去重样本（(domain, normalized query_text) 键）。
/// A deduplicated zero-recall / rewrite-failure sample (keyed by (domain,
/// normalized query_text)).
#[derive(Debug, Clone, Serialize)]
pub struct BlindSpotQuery {
    pub normalized_query: String,
    /// 来源 query log id，升序（确定性输出）。
    /// Source query-log ids, ascending (deterministic output).
    pub log_ids: Vec<i64>,
    /// 该归一化查询在窗口内的出现次数（= log_ids.len()，spec §6 "次数"）。
    /// Occurrences of the normalized query in the window (= log_ids.len(),
    /// spec §6 "occurrence count").
    pub occurrences: usize,
    /// 平均 latency_ms（f64 除法，确定性序列化）。
    /// Average latency_ms (f64 division, deterministic serialization).
    pub avg_latency_ms: f64,
}

/// 低质量页面样本（page_id 键；spec §6：feedback_count/adopted_count/
/// adoption_rate）。
/// A low-quality page sample (keyed by page_id; spec §6: feedback_count /
/// adopted_count / adoption_rate).
#[derive(Debug, Clone, Serialize)]
pub struct LowQualityPage {
    pub page_id: String,
    /// 该页面聚合到的 click/adopt/rate 事件数（≥5 才可能入选）。
    /// The click/adopt/rate events aggregated for this page (≥5 to qualify).
    pub feedback_count: usize,
    /// 采纳数（adopt 计 1、rate≥4 计 1、click 不计）。
    /// Adoption count (adopt scores 1, rate>=4 scores 1, click scores none).
    pub adopted_count: usize,
    /// 采纳率 = adopted_count / feedback_count（严格 <0.20 才入选）。
    /// Adoption rate = adopted_count / feedback_count (strictly <0.20 to
    /// qualify).
    pub adoption_rate: f64,
    /// 触发该页面判据的事件来源 log（升序去重，trace 用）。
    /// Source logs of the events that triggered this page (ascending,
    /// deduplicated, for trace).
    pub log_ids: Vec<i64>,
}

/// 建议审核项（分析器产物；批6 CLI 经 insert_review_suggestions 落库）。
/// A suggested review item (analyzer output; the batch-6 CLI persists it via
/// insert_review_suggestions).
#[derive(Debug, Clone, Serialize)]
pub struct ReviewSuggestion {
    pub domain: String,
    /// DDL 存储串：`supplemental_compile` / `query_template`（动作枚举本体按
    /// kernel 约定留给批4）。
    /// DDL storage string: `supplemental_compile` / `query_template` (the
    /// action enum itself is deferred to batch 4 per the kernel convention).
    pub action: String,
    /// 来源 query log id（升序去重；落库时序列化为 source_log_ids_json）。
    /// Source query-log ids (ascending, deduplicated; serialized into
    /// source_log_ids_json on insert).
    pub source_log_ids: Vec<i64>,
    /// 确定性 subject JSON 串（UNIQUE(domain,action,subject_json) 的幂等键成分）。
    /// The deterministic subject JSON string (part of the UNIQUE
    /// (domain,action,subject_json) idempotency key).
    pub subject_json: String,
    /// 判定理由 JSON 串（信号名与阈值上下文，审计用）。
    /// The reason JSON string (signal name plus threshold context; audit).
    pub reason_json: String,
}

impl ReviewSuggestion {
    /// 映射为 kernel 插入输入（created_at 由调用方提供——报告模型保持无时间戳
    /// 的确定性）。
    /// Maps into the kernel insert input (created_at is caller-supplied — the
    /// report model stays deterministic with no wall-clock inside).
    pub fn to_review_suggestion_input(&self, created_at: i64) -> Result<ReviewSuggestionInput> {
        Ok(ReviewSuggestionInput {
            action: self.action.clone(),
            source_log_ids_json: serde_json::to_string(&self.source_log_ids)?,
            subject_json: self.subject_json.clone(),
            reason_json: self.reason_json.clone(),
            created_at,
        })
    }
}

/// 计数切片：窗口边界外的输入行数与各类报告项数量（spec §6 counts + 任务口径
/// 的"输入行数"）。
/// The counts slice: input row counts plus per-class report-item counts
/// (spec §6 counts plus the task's "input row counts").
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackCounts {
    /// 输入 query_logs 行数（窗口内）。
    /// Input query_logs rows (in-window).
    pub input_log_rows: usize,
    /// 输入 feedback_events 行数（窗口内）。
    /// Input feedback_events rows (in-window).
    pub input_event_rows: usize,
    pub zero_recall_queries: usize,
    pub rewrite_failure_queries: usize,
    pub low_quality_pages: usize,
    pub suggested_reviews: usize,
}

/// 反馈分析报告（spec §6 FeedbackReport + 任务口径的 thresholds/report_hash）。
/// The feedback analysis report (spec §6 FeedbackReport plus the task's
/// thresholds/report_hash).
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackReport {
    pub schema_version: String,
    pub domain: String,
    pub from: i64,
    pub to: i64,
    pub thresholds: FeedbackThresholds,
    pub counts: FeedbackCounts,
    pub zero_recall: Vec<BlindSpotQuery>,
    pub rewrite_failures: Vec<BlindSpotQuery>,
    pub low_quality: Vec<LowQualityPage>,
    pub suggested_reviews: Vec<ReviewSuggestion>,
    /// BLAKE3(REPORT_HASH_PREFIX ++ canonical_json(本报告去 report_hash 字段))。
    /// BLAKE3(REPORT_HASH_PREFIX ++ canonical_json(this report minus the
    /// report_hash field)).
    pub report_hash: String,
}

impl FeedbackReport {
    /// 计算并回填 report_hash（先构造时占位空串，序列化后移除该字段再哈希，
    /// 占位值不进入哈希输入）。
    /// Computes and fills in report_hash (constructed with an empty-string
    /// placeholder; the field is removed from the serialized value before
    /// hashing, so the placeholder never enters the hash input).
    pub fn finalize_hash(mut self) -> Result<Self> {
        let mut value = serde_json::to_value(&self)?;
        if let serde_json::Value::Object(ref mut map) = value {
            map.remove("report_hash");
        }
        let bytes = canonical_json(&value)?;
        let mut framed = Vec::with_capacity(REPORT_HASH_PREFIX.len() + bytes.len());
        framed.extend_from_slice(REPORT_HASH_PREFIX);
        framed.extend_from_slice(&bytes);
        self.report_hash = blake3::hash(&framed).to_hex().to_string();
        Ok(self)
    }
}

/// 数字格式化（双语共用，保证"数字完全一致"；与 Step5 eval 报告同款 {:.6}）。
/// The number formatter (shared by both languages, guaranteeing "identical
/// numbers"; the same {:.6} style as the Step5 eval report).
fn num(v: f64) -> String {
    format!("{v:.6}")
}

/// log_ids 的 JSON 文本（双语与 JSON 三处共用同一序列化 → ID 完全一致）。
/// The JSON text of log_ids (one serialization shared by both languages and
/// the JSON → identical ids).
fn log_ids_text(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_default()
}

/// 概览行（标签由调用方按语言提供；值全部来自同一模型 → 双语一致）。
/// The overview rows (labels are provided per language; every value comes from
/// the same model → bilingual consistency).
fn overview_rows(rep: &FeedbackReport) -> Vec<(String, String)> {
    vec![
        ("schema_version".into(), rep.schema_version.clone()),
        ("domain".into(), rep.domain.clone()),
        ("window".into(), format!("{} .. {}", rep.from, rep.to)),
        ("min_events".into(), rep.thresholds.min_events.to_string()),
        (
            "adoption_threshold".into(),
            num(rep.thresholds.adoption_threshold),
        ),
        (
            "input_rows".into(),
            format!(
                "query_logs={}, feedback_events={}",
                rep.counts.input_log_rows, rep.counts.input_event_rows
            ),
        ),
        (
            "counts".into(),
            format!(
                "zero_recall={}, rewrite_failures={}, low_quality={}, suggested_reviews={}",
                rep.counts.zero_recall_queries,
                rep.counts.rewrite_failure_queries,
                rep.counts.low_quality_pages,
                rep.counts.suggested_reviews,
            ),
        ),
        ("report_hash".into(), format!("`{}`", rep.report_hash)),
    ]
}

/// 查询类样本表（零召回/改写失败共用；双语共用数值与格式）。
/// The query-class sample table (shared by zero recall / rewrite failures;
/// both languages share the values and formatting).
fn query_table(items: &[BlindSpotQuery]) -> String {
    if items.is_empty() {
        return String::from("empty\n");
    }
    let mut out = String::new();
    out.push_str("normalized_query | occurrences | log_ids | avg_latency_ms\n");
    out.push_str("--- | --- | --- | ---\n");
    for item in items {
        out.push_str(&format!(
            "{} | {} | {} | {}\n",
            item.normalized_query,
            item.occurrences,
            log_ids_text(&item.log_ids),
            num(item.avg_latency_ms),
        ));
    }
    out
}

/// 低质量页面表（双语共用数值与格式）。
/// The low-quality page table (both languages share the values and formatting).
fn page_table(items: &[LowQualityPage]) -> String {
    if items.is_empty() {
        return String::from("empty\n");
    }
    let mut out = String::new();
    out.push_str("page_id | feedback_count | adopted_count | adoption_rate | log_ids\n");
    out.push_str("--- | --- | --- | --- | ---\n");
    for page in items {
        out.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            page.page_id,
            page.feedback_count,
            page.adopted_count,
            num(page.adoption_rate),
            log_ids_text(&page.log_ids),
        ));
    }
    out
}

/// 建议表（subject_json 原文入表，双语同串）。
/// The suggestion table (subject_json verbatim; the same string in both
/// languages).
fn suggestion_table(items: &[ReviewSuggestion]) -> String {
    if items.is_empty() {
        return String::from("empty\n");
    }
    let mut out = String::new();
    out.push_str("action | source_log_ids | subject_json\n");
    out.push_str("--- | --- | ---\n");
    for s in items {
        out.push_str(&format!(
            "{} | {} | {}\n",
            s.action,
            log_ids_text(&s.source_log_ids),
            s.subject_json,
        ));
    }
    out
}

/// 中文 Markdown 报告（与英文逐节对应）。
/// The Chinese Markdown report (mirrors the English section by section).
pub fn render_markdown_zh(rep: &FeedbackReport) -> String {
    let mut s = String::new();
    s.push_str("# Step 6 反馈分析报告\n\n");
    s.push_str("## 1. 概览\n\n");
    for (k, v) in overview_rows(rep) {
        s.push_str(&format!("- **{k}**：{v}\n"));
    }
    s.push_str("\n## 2. 零召回查询\n\n");
    s.push_str("判据：hit_count=0 且非「滤空且放宽仍失败」（D11）。\n\n");
    s.push_str(&query_table(&rep.zero_recall));
    s.push('\n');
    s.push_str("## 3. 改写失败查询\n\n");
    s.push_str("判据：rewrite_failure=1（D11，独立信号）。\n\n");
    s.push_str(&query_table(&rep.rewrite_failures));
    s.push('\n');
    s.push_str("## 4. 低质量页面\n\n");
    s.push_str(&format!(
        "判据：事件数 ≥ {} 且采纳率严格 < {}（adopt 计 1、rate≥4 计 1、click 只进分母；D11）。\n\n",
        rep.thresholds.min_events,
        num(rep.thresholds.adoption_threshold),
    ));
    s.push_str(&page_table(&rep.low_quality));
    s.push('\n');
    s.push_str("## 5. 建议审核项\n\n");
    s.push_str("建议只进入人工审核队列，approve 前不触碰编译任务状态（D12）。\n\n");
    s.push_str(&suggestion_table(&rep.suggested_reviews));
    s
}

/// 英文 Markdown 报告（与中文逐节对应；数字/ID/hash 完全一致）。
/// The English Markdown report (mirrors the Chinese section by section;
/// numbers/ids/hash are identical).
pub fn render_markdown_en(rep: &FeedbackReport) -> String {
    let mut s = String::new();
    s.push_str("# Step 6 Feedback Analysis Report\n\n");
    s.push_str("## 1. Overview\n\n");
    for (k, v) in overview_rows(rep) {
        s.push_str(&format!("- **{k}**: {v}\n"));
    }
    s.push_str("\n## 2. Zero-recall queries\n\n");
    s.push_str(
        "Criterion: hit_count=0 and NOT \"filter-empty with a failed relaxation\" (D11).\n\n",
    );
    s.push_str(&query_table(&rep.zero_recall));
    s.push('\n');
    s.push_str("## 3. Rewrite-failure queries\n\n");
    s.push_str("Criterion: rewrite_failure=1 (D11, an independent signal).\n\n");
    s.push_str(&query_table(&rep.rewrite_failures));
    s.push('\n');
    s.push_str("## 4. Low-quality pages\n\n");
    s.push_str(&format!(
        "Criterion: at least {} events and an adoption rate strictly < {} (adopt scores 1, rate>=4 scores 1, click feeds the denominator only; D11).\n\n",
        rep.thresholds.min_events,
        num(rep.thresholds.adoption_threshold),
    ));
    s.push_str(&page_table(&rep.low_quality));
    s.push('\n');
    s.push_str("## 5. Suggested review items\n\n");
    s.push_str(
        "Suggestions only enter the human review queue; nothing touches the compile-task state before approval (D12).\n\n",
    );
    s.push_str(&suggestion_table(&rep.suggested_reviews));
    s
}

/// JSON 结果（契约字段全集；数字为原始 f64，不做字符串化）。
/// The JSON result (the full contract field set; numbers stay raw f64, never
/// stringified).
pub fn render_json(rep: &FeedbackReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(rep)?)
}

/// 单文件原子写：先写同目录临时文件再 rename（spec §8：报告文件写临时文件后
/// rename；同目录 rename 在 POSIX 上原子）。临时名带进程 id，避免并发运行互
/// 踩；失败路径尽力清理临时文件。
/// One-file atomic write: write a same-directory temp file first, then rename
/// (spec §8: report files are written to a temp file and renamed; a same-dir
/// rename is atomic on POSIX). The temp name carries the process id so
/// concurrent runs never collide; the failure path best-effort removes the
/// temp file.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Validation(format!("invalid report path: {}", path.display())))?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    let write_result = std::fs::write(&tmp_path, contents);
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    std::fs::rename(&tmp_path, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

/// 写出三件套到 `dir`，返回三个文件路径（§8 契约文件名；每个文件临时写后
/// rename，失败向上传 Err，绝不半写）。
/// Writes the trio into `dir` and returns the three file paths (§8 contract
/// file names; each file goes through temp-write then rename — failures
/// propagate as Err, never a half-written file).
pub fn write_report_files(dir: &Path, rep: &FeedbackReport) -> Result<[String; 3]> {
    std::fs::create_dir_all(dir)?;
    let zh = dir.join(REPORT_FILE_ZH);
    let en = dir.join(REPORT_FILE_EN);
    let json = dir.join(REPORT_FILE_JSON);
    write_atomic(&zh, &render_markdown_zh(rep))?;
    write_atomic(&en, &render_markdown_en(rep))?;
    write_atomic(&json, &render_json(rep)?)?;
    Ok([
        zh.to_string_lossy().into_owned(),
        en.to_string_lossy().into_owned(),
        json.to_string_lossy().into_owned(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{analyze_window, FeedbackWindow};
    use wiktor_core::kernel::feedback_store::{FeedbackEvent, FeedbackKind, QueryLogSnapshot};

    const DOMAIN: &str = "milk-tea";

    /// 构造 query_logs 快照。
    /// Builds a query_logs snapshot.
    // 测试夹具字段全量显式（D11 三状态列 + latency），8 参数是判据覆盖的最小
    // 形状，允许超 7 参 lint。
    // The test fixture spells out every field (the D11 three state columns +
    // latency); 8 args are the minimal shape for criterion coverage — allow the
    // too-many-arguments lint.
    #[allow(clippy::too_many_arguments)]
    fn log(
        log_id: i64,
        query_text: &str,
        hits: i64,
        rewrite_failure: bool,
        empty_initial: bool,
        relax_attempted: bool,
        relax_succeeded: bool,
        latency: i64,
    ) -> QueryLogSnapshot {
        QueryLogSnapshot {
            log_id,
            query_text: query_text.into(),
            query_json: "{}".into(),
            rewritten_json: None,
            rewrite_failure,
            hit_count: hits,
            latency_ms: latency,
            timestamp: 100,
            domain: DOMAIN.into(),
            candidate_empty_initial: empty_initial,
            relaxation_attempted: relax_attempted,
            relaxation_succeeded: relax_succeeded,
        }
    }

    /// 构造 feedback_events 事件。
    /// Builds a feedback_events event.
    fn event(
        event_id: i64,
        log_id: i64,
        kind: FeedbackKind,
        page_id: Option<&str>,
        rating: Option<u8>,
    ) -> FeedbackEvent {
        FeedbackEvent {
            event_id,
            idempotency_key: format!("k-{event_id}"),
            domain: DOMAIN.into(),
            log_id,
            kind,
            page_id: page_id.map(Into::into),
            rating,
            metadata_json: "{}".into(),
            received_at: 100,
        }
    }

    fn window(logs: Vec<QueryLogSnapshot>, events: Vec<FeedbackEvent>) -> FeedbackWindow {
        FeedbackWindow {
            domain: DOMAIN.into(),
            from: 0,
            to: 1000,
            logs,
            events,
        }
    }

    // A10：三信号判据——滤空且放宽仍失败不进 zero_recall；放宽重开后仍零命中
    // 与普通零命中进入；rewrite_failure 独立出现（与零召回可同时命中）。
    // A10: the three-signal criteria — filter-empty with a failed relaxation
    // stays out of zero_recall; a reopened-but-zero-hit log and a plain
    // zero-hit log both enter; rewrite_failure appears independently (it may
    // coincide with zero recall).
    #[test]
    fn zero_recall_and_rewrite_failure_follow_d11_predicates() {
        let logs = vec![
            // 普通零命中：进入。
            // Plain zero hit: enters.
            log(1, "波霸奶茶", 0, false, false, false, false, 10),
            // 滤空且放宽仍失败：排除（≠ 盲区）。
            // Filter-empty with a failed relaxation: excluded (not a blind spot).
            log(2, "珍珠奶茶", 0, false, true, true, false, 10),
            // 放宽重开候选仍零命中：进入（真盲区）。
            // Reopened candidates but still zero hits: enters (a true blind spot).
            log(3, "芋泥波波", 0, false, true, true, true, 30),
            // 有命中：不进入。
            // Has hits: stays out.
            log(4, "四季春", 3, false, false, false, false, 10),
            // 改写失败且零命中（无滤空）：两类各记一次（信号独立）。
            // Rewrite failure with zero hits (no filter emptiness): recorded in
            // both classes (independent signals).
            log(5, "红茶玛奇朵", 0, true, false, false, false, 20),
            // 改写失败但有命中：只进改写失败类。
            // Rewrite failure with hits: rewrite-failure class only.
            log(6, "乌龙拿铁", 2, true, false, false, false, 20),
        ];
        let rep = analyze_window(window(logs, Vec::new())).unwrap();

        assert_eq!(rep.counts.input_log_rows, 6);
        assert_eq!(rep.counts.zero_recall_queries, 3);
        // BTreeMap 键序：波霸奶茶 < 红茶玛奇朵 < 芋泥波波（Unicode 字节序）。
        // BTreeMap key order: 波霸奶茶 < 红茶玛奇朵 < 芋泥波波 (Unicode byte
        // order).
        let zero_keys: Vec<&str> = rep
            .zero_recall
            .iter()
            .map(|i| i.normalized_query.as_str())
            .collect();
        assert_eq!(zero_keys, vec!["波霸奶茶", "红茶玛奇朵", "芋泥波波"]);
        let boba = rep
            .zero_recall
            .iter()
            .find(|i| i.normalized_query == "波霸奶茶")
            .unwrap();
        assert_eq!(boba.log_ids, vec![1]);
        assert!(
            rep.zero_recall.iter().all(|i| !i.log_ids.contains(&2)),
            "filter-empty with failed relaxation must be excluded"
        );

        assert_eq!(rep.counts.rewrite_failure_queries, 2);
        let rewrite_keys: Vec<&str> = rep
            .rewrite_failures
            .iter()
            .map(|i| i.normalized_query.as_str())
            .collect();
        assert_eq!(rewrite_keys, vec!["乌龙拿铁", "红茶玛奇朵"]);
        // 独立信号：log 5（红茶玛奇朵）同时出现在两类。
        // Independent signal: log 5 (红茶玛奇朵) appears in both classes.
        let matcha = rep
            .rewrite_failures
            .iter()
            .find(|i| i.normalized_query == "红茶玛奇朵")
            .unwrap();
        assert_eq!(matcha.log_ids, vec![5]);
        assert_eq!(rep.counts.suggested_reviews, 5);
    }

    /// 为一个页面追加一组事件（log_id 恒为 1；event_id 按累计序分配）。
    /// Appends one page's events (log_id fixed at 1; event_ids assigned
    /// sequentially).
    fn push_page(
        events: &mut Vec<FeedbackEvent>,
        page: &str,
        specs: &[(FeedbackKind, Option<u8>)],
    ) {
        for (kind, rating) in specs {
            let event_id = events.len() as i64 + 1;
            events.push(FeedbackEvent {
                event_id,
                idempotency_key: format!("k-{event_id}"),
                domain: DOMAIN.into(),
                log_id: 1,
                kind: *kind,
                page_id: Some(page.to_string()),
                rating: *rating,
                metadata_json: "{}".into(),
                received_at: 100,
            });
        }
    }

    // A11：低质量边界——rate=4 计采纳、rate≤3 不计；事件数 4 不触发；采纳率
    // = 0.20 不触发；< 0.20 触发（整数交叉相乘精确判定）。
    // A11: low-quality boundaries — rate=4 scores an adoption, rate<=3 does
    // not; 4 events never trigger; adoption rate == 0.20 does not trigger;
    // < 0.20 triggers (exact integer cross-multiplication).
    #[test]
    fn low_quality_boundaries_match_d11() {
        let mut events = Vec::new();

        // page boundary：1 adopt + 4 click = 5 事件，采纳率 0.20 → 不触发。
        // page boundary: 1 adopt + 4 clicks = 5 events, rate 0.20 → no trigger.
        push_page(
            &mut events,
            "drink:boundary",
            &[
                (FeedbackKind::Adopt, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
            ],
        );
        // page rate4：1 rate(4) + 5 click = 6 事件，采纳率 1/6 → 触发。
        // page rate4: 1 rate(4) + 5 clicks = 6 events, 1/6 → triggers.
        push_page(
            &mut events,
            "drink:rate4",
            &[
                (FeedbackKind::Rate, Some(4)),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
            ],
        );
        // page rate3：1 rate(3) + 5 click = 6 事件，采纳率 0 → 触发（rate≤3
        // 不计采纳）。
        // page rate3: 1 rate(3) + 5 clicks = 6 events, rate 0 → triggers
        // (rate<=3 scores no adoption).
        push_page(
            &mut events,
            "drink:rate3",
            &[
                (FeedbackKind::Rate, Some(3)),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
            ],
        );
        // page few：1 adopt + 3 click = 4 事件 → 不触发（最小样本）。
        // page few: 1 adopt + 3 clicks = 4 events → no trigger (min sample).
        push_page(
            &mut events,
            "drink:few",
            &[
                (FeedbackKind::Adopt, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
            ],
        );
        // page healthy：1 adopt + 1 rate(5) + 3 click = 5 事件，采纳率 0.4 →
        // 不触发。
        // page healthy: 1 adopt + 1 rate(5) + 3 clicks = 5 events, rate 0.4 →
        // no trigger.
        push_page(
            &mut events,
            "drink:healthy",
            &[
                (FeedbackKind::Adopt, None),
                (FeedbackKind::Rate, Some(5)),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
                (FeedbackKind::Click, None),
            ],
        );
        // 无 page_id 的 rate（DDL 允许）不归属任何页面，只计输入行数。
        // A page-less rate (DDL-legal) is attributed to no page and only counts
        // toward the input rows.
        events.push(event(99, 1, FeedbackKind::Rate, None, Some(5)));

        let rep = analyze_window(window(Vec::new(), events)).unwrap();
        assert_eq!(rep.counts.input_event_rows, 27);
        assert_eq!(rep.counts.low_quality_pages, 2);
        let pages: Vec<&str> = rep.low_quality.iter().map(|p| p.page_id.as_str()).collect();
        assert_eq!(pages, vec!["drink:rate3", "drink:rate4"]);
        let rate4 = rep
            .low_quality
            .iter()
            .find(|p| p.page_id == "drink:rate4")
            .unwrap();
        assert_eq!(rate4.feedback_count, 6);
        assert_eq!(rate4.adopted_count, 1);
        assert!((rate4.adoption_rate - 1.0 / 6.0).abs() < 1e-12);
        assert_eq!(rate4.log_ids, vec![1]);
        // 触发页建议（supplemental_compile）带事件来源 log_ids。
        // The triggered page's suggestion (supplemental_compile) carries the
        // events' source log_ids.
        let suggestion = rep
            .suggested_reviews
            .iter()
            .find(|s| s.subject_json.contains("drink:rate4"))
            .unwrap();
        assert_eq!(suggestion.action, "supplemental_compile");
        assert_eq!(suggestion.source_log_ids, vec![1]);
    }

    // A10/A12：subject_json 确定性——(domain, normalized query_text) 去重，
    // log_ids 升序、次数、平均 latency；归一化复用 query_engine 的 normalize
    // （trim/折叠空白/Unicode 小写）。
    // A10/A12: subject_json determinism — dedup by (domain, normalized
    // query_text) with ascending log_ids, occurrences and average latency;
    // normalization reuses the query_engine normalize (trim/collapse
    // whitespace/Unicode lowercase).
    #[test]
    fn subject_dedup_merges_normalized_queries() {
        let logs = vec![
            log(7, "  BoBa  奶茶 ", 0, false, false, false, false, 10),
            log(3, "boba 奶茶", 0, false, false, false, false, 20),
            log(9, "四季春", 0, false, false, false, false, 5),
        ];
        let rep = analyze_window(window(logs, Vec::new())).unwrap();

        assert_eq!(rep.counts.zero_recall_queries, 2);
        let merged = rep
            .zero_recall
            .iter()
            .find(|i| i.normalized_query == "boba 奶茶")
            .unwrap();
        assert_eq!(merged.log_ids, vec![3, 7], "ascending log_ids");
        assert_eq!(merged.occurrences, 2);
        assert!((merged.avg_latency_ms - 15.0).abs() < 1e-12);

        // subject_json：确定性字段集（domain/normalized_query/log_ids/
        // occurrences/avg_latency_ms），两次序列化逐字节一致。
        // subject_json: the deterministic field set (domain/normalized_query/
        // log_ids/occurrences/avg_latency_ms); two serializations are
        // byte-identical.
        let suggestion = rep
            .suggested_reviews
            .iter()
            .find(|s| s.action == "supplemental_compile")
            .unwrap();
        let subject: serde_json::Value = serde_json::from_str(&suggestion.subject_json).unwrap();
        assert_eq!(subject["domain"], DOMAIN);
        assert_eq!(subject["normalized_query"], "boba 奶茶");
        assert_eq!(subject["log_ids"], serde_json::json!([3, 7]));
        assert_eq!(subject["occurrences"], 2);
        assert_eq!(subject["avg_latency_ms"], 15.0);
        assert_eq!(
            suggestion.subject_json,
            serde_json::json!({
                "domain": DOMAIN,
                "normalized_query": "boba 奶茶",
                "log_ids": [3, 7],
                "occurrences": 2,
                "avg_latency_ms": 15.0,
            })
            .to_string()
        );
        assert_eq!(suggestion.reason_json, r#"{"signal":"zero_recall"}"#);
    }

    // A12：同输入两次分析 → JSON 逐字节一致 → report_hash 一致（64 位小写
    // hex）。
    // A12: two analyses over the same input → byte-identical JSON → identical
    // report_hash (64 lowercase hex).
    #[test]
    fn repeated_analysis_is_byte_identical() {
        let make = || {
            window(
                vec![
                    log(1, "波霸奶茶", 0, false, false, false, false, 10),
                    log(2, "珍珠奶茶", 0, true, false, false, false, 20),
                ],
                vec![
                    event(1, 1, FeedbackKind::Adopt, Some("drink:boba"), None),
                    event(2, 1, FeedbackKind::Click, Some("drink:boba"), None),
                    event(3, 1, FeedbackKind::Click, Some("drink:boba"), None),
                    event(4, 1, FeedbackKind::Click, Some("drink:boba"), None),
                    event(5, 1, FeedbackKind::Click, Some("drink:boba"), None),
                ],
            )
        };
        let a = analyze_window(make()).unwrap();
        let b = analyze_window(make()).unwrap();

        let json_a = render_json(&a).unwrap();
        let json_b = render_json(&b).unwrap();
        assert_eq!(json_a, json_b, "byte-identical JSON for equal inputs");
        assert_eq!(a.report_hash, b.report_hash);
        assert_eq!(a.report_hash.len(), 64);
        assert!(a
            .report_hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    // A13：报告项超硬上限（1000/类）报错不截断——零召回与低质量两类各验一次。
    // A13: report items over the hard cap (1000/class) error instead of
    // truncating — verified for the zero-recall and low-quality classes.
    #[test]
    fn over_cap_report_items_error_instead_of_truncating() {
        // 1001 个互不相同的零命中查询。
        // 1001 distinct zero-hit queries.
        let logs = (0..1001)
            .map(|i| {
                log(
                    i + 1,
                    &format!("query-{i:05}"),
                    0,
                    false,
                    false,
                    false,
                    false,
                    1,
                )
            })
            .collect();
        let err = analyze_window(window(logs, Vec::new())).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("1000"));

        // 1001 个事件数 ≥5 且采纳率 0 的页面。
        // 1001 pages with ≥5 events and a zero adoption rate.
        let mut events = Vec::new();
        for page in 0..1001 {
            for k in 0..5 {
                events.push(event(
                    page * 10 + k,
                    1,
                    FeedbackKind::Click,
                    Some(&format!("page-{page:05}")),
                    None,
                ));
            }
        }
        let err = analyze_window(window(Vec::new(), events)).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("1000"));
    }

    // 归一化契约违反 fail-closed：空白查询文本 → 整体 Validation，绝不静默
    // 跳过或转空报告。
    // A normalization-contract violation fails closed: whitespace-only query
    // text → a whole-analysis Validation, never a silent skip or an empty
    // report.
    #[test]
    fn normalization_failure_fails_closed() {
        let logs = vec![log(1, "   ", 0, false, false, false, false, 10)];
        let err = analyze_window(window(logs, Vec::new())).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("normalization"));
    }

    // 窗口边界与空窗口：from > to → Validation；空输入 → 零样本报告且 hash
    // 仍确定。
    // Window bounds and the empty window: from > to → Validation; empty input
    // → a zero-sample report with a still-deterministic hash.
    #[test]
    fn window_bounds_and_empty_window() {
        let mut w = window(Vec::new(), Vec::new());
        w.from = 10;
        w.to = 5;
        assert!(matches!(
            analyze_window(w).unwrap_err(),
            Error::Validation(_)
        ));

        let rep = analyze_window(window(Vec::new(), Vec::new())).unwrap();
        assert_eq!(rep.counts.input_log_rows, 0);
        assert_eq!(rep.counts.input_event_rows, 0);
        assert!(rep.zero_recall.is_empty() && rep.low_quality.is_empty());
        assert_eq!(rep.report_hash.len(), 64);
    }

    // A13：双语三件套——中文/英文 Markdown 与 JSON 的数字、ID、hash 完全一致；
    // JSON 含窗口、阈值、输入行数、三类样本；报告文件写临时文件后 rename，目录
    // 无临时残留。
    // A13: the bilingual trio — numbers, ids and the hash are identical across
    // the Chinese/English Markdown and the JSON; the JSON carries the window,
    // thresholds, input rows and the three sample classes; report files go
    // through temp-write then rename with no temp leftovers in the directory.
    #[test]
    fn bilingual_trio_agrees_and_write_is_atomic() {
        let rep = analyze_window(window(
            vec![
                log(7, "  BoBa  奶茶 ", 0, false, false, false, false, 10),
                log(3, "boba 奶茶", 0, false, false, false, false, 20),
                log(9, "四季春", 2, false, false, false, false, 5),
            ],
            vec![
                event(1, 9, FeedbackKind::Adopt, Some("drink:oolong"), None),
                event(2, 9, FeedbackKind::Click, Some("drink:oolong"), None),
                event(3, 9, FeedbackKind::Click, Some("drink:oolong"), None),
                event(4, 9, FeedbackKind::Click, Some("drink:oolong"), None),
                event(5, 9, FeedbackKind::Click, Some("drink:oolong"), None),
                event(6, 9, FeedbackKind::Click, Some("drink:oolong"), None),
            ],
        ))
        .unwrap();

        let zh = render_markdown_zh(&rep);
        let en = render_markdown_en(&rep);
        let json = render_json(&rep).unwrap();

        // hash 与 ID 三处一致（Markdown 表用紧凑 JSON 串；pretty JSON 的
        // log_ids 经解析比对）。
        // The hash and ids agree across all three artifacts (Markdown tables
        // use the compact JSON text; pretty-JSON log_ids are compared after
        // parsing).
        assert!(zh.contains(&rep.report_hash) && en.contains(&rep.report_hash));
        assert!(json.contains(&rep.report_hash));
        assert!(zh.contains("[3,7]") && en.contains("[3,7]"));
        // 数字一致：平均 latency 15.000000、采纳率 0.166667（1/6）两语都在。
        // Numbers agree: average latency 15.000000 and adoption 0.166667 (1/6)
        // appear in both languages.
        assert!(zh.contains("15.000000") && en.contains("15.000000"));
        assert!(zh.contains("0.166667") && en.contains("0.166667"));
        // 领域字面量不翻译（查询文本是数据；fixture 的中文片段进入两语报告；
        // 有命中的「四季春」不属于任何盲区类，故不出现在报告中）。
        // Domain literals stay untranslated (query text is data; the fixture's
        // Chinese fragment enters both language reports; the hitting query
        // 四季春 belongs to no blind-spot class and is absent from the report).
        assert!(zh.contains("奶茶") && en.contains("奶茶"));
        // 章节逐节对应。
        // Sections mirror each other.
        for section in ["1.", "2.", "3.", "4.", "5."] {
            assert!(zh.contains(&format!("## {section}")) && en.contains(&format!("## {section}")));
        }

        // JSON 契约字段：窗口、阈值、输入行数、三类样本、hash。
        // JSON contract fields: window, thresholds, input rows, three sample
        // classes, hash.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(v["domain"], DOMAIN);
        assert_eq!(v["from"], 0);
        assert_eq!(v["to"], 1000);
        assert_eq!(v["thresholds"]["min_events"], 5);
        assert_eq!(v["thresholds"]["adoption_threshold"], 0.2);
        assert_eq!(v["counts"]["input_log_rows"], 3);
        assert_eq!(v["counts"]["input_event_rows"], 6);
        assert_eq!(v["counts"]["zero_recall_queries"], 1);
        assert_eq!(v["counts"]["low_quality_pages"], 1);
        assert_eq!(v["zero_recall"].as_array().unwrap().len(), 1);
        assert_eq!(v["zero_recall"][0]["log_ids"], serde_json::json!([3, 7]));
        assert_eq!(v["low_quality"].as_array().unwrap().len(), 1);
        assert_eq!(v["low_quality"][0]["page_id"], "drink:oolong");
        assert_eq!(v["suggested_reviews"].as_array().unwrap().len(), 2);
        assert_eq!(v["report_hash"].as_str().unwrap().len(), 64);
        assert_eq!(v["report_hash"], rep.report_hash);

        // 落盘三件套 + 无临时残留（rename 原子性）。
        // The on-disk trio with no temp leftovers (rename atomicity).
        let dir = tempfile::tempdir().unwrap();
        let paths = write_report_files(dir.path(), &rep).unwrap();
        assert!(paths[0].ends_with(REPORT_FILE_ZH));
        assert!(paths[1].ends_with(REPORT_FILE_EN));
        assert!(paths[2].ends_with(REPORT_FILE_JSON));
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), zh);
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), en);
        assert_eq!(std::fs::read_to_string(&paths[2]).unwrap(), json);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");

        // 二次写盘逐字节一致（报告无时间戳等非确定字段，A12）。
        // A second write is byte-identical (no timestamps or other
        // nondeterministic fields, A12).
        write_report_files(dir.path(), &rep).unwrap();
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), zh);
    }

    // ReviewSuggestion → kernel 插入输入：source_log_ids 序列化、created_at
    // 调用方注入（报告模型不含时间戳）。
    // ReviewSuggestion → kernel insert input: source_log_ids serialization and
    // caller-injected created_at (the report model carries no timestamps).
    #[test]
    fn suggestion_maps_to_kernel_input() {
        let rep = analyze_window(window(
            vec![log(1, "波霸奶茶", 0, false, false, false, false, 10)],
            Vec::new(),
        ))
        .unwrap();
        let s = &rep.suggested_reviews[0];
        let input = s.to_review_suggestion_input(1234).unwrap();
        assert_eq!(input.action, "supplemental_compile");
        assert_eq!(input.source_log_ids_json, "[1]");
        assert_eq!(input.subject_json, s.subject_json);
        assert_eq!(input.reason_json, s.reason_json);
        assert_eq!(input.created_at, 1234);
    }
}
