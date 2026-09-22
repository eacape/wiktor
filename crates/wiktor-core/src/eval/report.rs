//! Step 5 批5：评测报告模型与三件套输出（spec `step5-qug-build.md` §6、
//! §7 A13 的报告部分）。
//! Step 5 batch 5: the evaluation report model and the three-file output
//! (spec `step5-qug-build.md` §6, the report part of §7 A13).
//!
//! 契约（§6）：
//! - 输出三件套：`step5-qug-evaluation.md`（中文）、
//!   `step5-qug-evaluation.en.md`（英文）、`step5-qug-evaluation.json`；
//! - 双语报告逐节对应，数字、query id、decision、dataset_hash 完全一致；
//!   "啵啵""珍珠"等领域字面量保留中文；
//! - JSON 至少含 schema_version、domain/version、dataset_hash、source_hash、
//!   A/B/C 的 @1/@5/@10、negative_precision、按 kind 分层、fallback_count、
//!   失败样本、qug_decision、reason、vector_backend、运行命令。
//!
//! Contract (§6):
//! - the trio: `step5-qug-evaluation.md` (Chinese), `step5-qug-evaluation.en.md`
//!   (English), `step5-qug-evaluation.json`;
//! - the bilingual reports correspond section by section with identical
//!   numbers, query ids, decision and dataset_hash; domain literals such as
//!   "啵啵"/"珍珠" stay Chinese;
//! - the JSON carries at least schema_version, domain/version, dataset_hash,
//!   source_hash, A/B/C @1/@5/@10, negative_precision, per-kind stratification,
//!   fallback_count, failed samples, qug_decision, reason, vector_backend and
//!   the run command.
//!
//! 失败语义：单条 query 错误在 runner 中已汇总为运行失败（§6），成功报告的
//! `failed_samples` 恒为空数组；字段保留以满足契约并为批6 CLI 的错误呈现
//! 留位。报告不含时间戳等非确定字段——同一输入两次运行产出逐字节一致的三
//! 件套（与 A10 对齐）。
//! Failure semantics: per-query errors already summarize into a run failure in
//! the runner (§6), so a success report's `failed_samples` is always an empty
//! array; the field stays to satisfy the contract and to give the batch-6 CLI
//! an error-presentation slot. Reports carry no timestamps or other
//! nondeterministic fields — two runs over one input yield byte-identical
//! files (aligned with A10).

use crate::eval::metrics::KindMetrics;
use crate::eval::runner::{EvalConfig, EvalOutcome};
use crate::eval::{GoldenSet, QugDecision};
use crate::traits::DomainConfig;
use crate::types::error::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// 报告 JSON 的 schema 版本。
/// The report JSON's schema version.
pub const EVAL_REPORT_SCHEMA_VERSION: u32 = 1;

/// 中文报告文件名（§6）。
/// Chinese report file name (§6).
pub const EVAL_REPORT_FILE_ZH: &str = "step5-qug-evaluation.md";

/// 英文报告文件名（§6）。
/// English report file name (§6).
pub const EVAL_REPORT_FILE_EN: &str = "step5-qug-evaluation.en.md";

/// JSON 结果文件名（§6）。
/// JSON result file name (§6).
pub const EVAL_REPORT_FILE_JSON: &str = "step5-qug-evaluation.json";

/// 失败样本（成功报告恒为空；批6 CLI 错误呈现位）。
/// A failed sample (always empty in success reports; the batch-6 CLI error
/// slot).
#[derive(Debug, Clone, Serialize)]
pub struct FailedSample {
    pub query_id: String,
    pub kind: String,
    pub error: String,
}

/// 单档报告切片（总体 + 分层指标 + 计数）。
/// One tier's report slice (overall + per-kind metrics + counts).
#[derive(Debug, Clone, Serialize)]
pub struct TierReport {
    /// 档位标签（"A" / "B" / "C"）。
    /// Tier label ("A" / "B" / "C").
    pub label: String,
    /// 是否纯 FTS 模式（A 档 true）。
    /// Whether the pure-FTS mode was on (true for tier A).
    pub fts_only: bool,
    /// 是否注入 QUG 图（C 档 true）。
    /// Whether a QUG graph was injected (true for tier C).
    pub qug_enabled: bool,
    pub positive_samples: usize,
    pub exclusion_samples: usize,
    pub fallback_count: usize,
    pub recall_at_1: Option<f64>,
    pub recall_at_5: Option<f64>,
    pub recall_at_10: Option<f64>,
    pub negative_precision: Option<f64>,
    /// 按 kind 分层（键 = GoldenKind::as_str）。
    /// Per-kind stratification (keyed by GoldenKind::as_str).
    pub by_kind: BTreeMap<String, KindMetrics>,
}

/// 判定切片（D6/A11/A12）。
/// The decision slice (D6/A11/A12).
#[derive(Debug, Clone, Serialize)]
pub struct DecisionReport {
    pub qug_decision: QugDecision,
    pub gain_pp: Option<f64>,
    pub reason: String,
    pub recall_regression: bool,
    pub negative_precision_regression: bool,
    pub active_qug_samples: usize,
    pub has_active_graph: bool,
}

/// 评测报告完整模型（JSON 直接序列化本结构）。
/// The full evaluation-report model (the JSON serializes this struct).
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub domain: String,
    pub domain_version: String,
    /// golden 文件 hash（BLAKE3，loader 契约）。
    /// The golden file hash (BLAKE3, the loader contract).
    pub dataset_hash: String,
    /// active published QUG 构建的 source_hash；无 active 时 None。
    /// The active published QUG build's source_hash; None without one.
    pub source_hash: Option<String>,
    pub golden_total: usize,
    pub kind_counts: BTreeMap<String, usize>,
    pub eval_top_k: usize,
    pub rrf_k: u32,
    pub vector_backend: String,
    pub command: String,
    /// 三档切片（键 "A"/"B"/"C"）。
    /// The three tier slices (keyed "A"/"B"/"C").
    pub tiers: BTreeMap<String, TierReport>,
    /// C 档 fallback 计数（D6 fallback 契约的顶层字段）。
    /// Tier-C fallback count (the top-level field of the D6 fallback contract).
    pub fallback_count: usize,
    pub failed_samples: Vec<FailedSample>,
    pub decision: DecisionReport,
}

impl EvaluationReport {
    /// 从评测产物构建报告模型。
    /// Builds the report model from the evaluation outcome.
    pub fn build(
        outcome: &EvalOutcome,
        golden: &GoldenSet,
        domain: &DomainConfig,
        config: &EvalConfig,
    ) -> Self {
        let tier =
            |label: &str, fts_only: bool, qug: bool, m: &crate::eval::VariantMetrics| TierReport {
                label: label.into(),
                fts_only,
                qug_enabled: qug,
                positive_samples: m.positive_samples,
                exclusion_samples: m.exclusion_samples,
                fallback_count: m.fallback_count,
                recall_at_1: m.recall_at_1,
                recall_at_5: m.recall_at_5,
                recall_at_10: m.recall_at_10,
                negative_precision: m.negative_precision,
                by_kind: m.by_kind.clone(),
            };
        let mut tiers = BTreeMap::new();
        tiers.insert("A".into(), tier("A", true, false, &outcome.tier_a));
        tiers.insert("B".into(), tier("B", false, false, &outcome.tier_b));
        tiers.insert("C".into(), tier("C", false, true, &outcome.tier_c));
        Self {
            schema_version: EVAL_REPORT_SCHEMA_VERSION,
            domain: domain.name.clone(),
            domain_version: domain.version.clone(),
            dataset_hash: golden.dataset_hash().to_string(),
            source_hash: outcome.source_hash.clone(),
            golden_total: outcome.golden_total,
            kind_counts: outcome.kind_counts.clone(),
            eval_top_k: outcome.eval_top_k,
            rrf_k: config.rrf_k,
            vector_backend: config.vector_backend.clone(),
            command: config.command.clone(),
            tiers,
            fallback_count: outcome.tier_c.fallback_count,
            failed_samples: Vec::new(),
            decision: DecisionReport {
                qug_decision: outcome.decision.qug_decision,
                gain_pp: outcome.decision.gain_pp,
                reason: outcome.decision.reason.clone(),
                recall_regression: outcome.decision.recall_regression,
                negative_precision_regression: outcome.decision.negative_precision_regression,
                active_qug_samples: outcome.decision.active_qug_samples,
                has_active_graph: outcome.decision.has_active_graph,
            },
        }
    }
}

/// 指标数字的统一格式化（双语报告共用，保证"数字完全一致"）。
/// The shared number formatter for metrics (used by both reports, guaranteeing
/// "identical numbers").
fn num(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.6}"),
        None => "-".into(),
    }
}

/// 概览节正文（标签由调用方按语言提供；值全部来自同一模型 → 双语一致）。
/// The overview section body (labels are provided per language; every value
/// comes from the same model → bilingual consistency).
fn overview_rows(rep: &EvaluationReport) -> Vec<(String, String)> {
    let kinds: String = rep
        .kind_counts
        .iter()
        .map(|(k, n)| format!("{k}={n}"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        ("domain".into(), rep.domain.clone()),
        ("domain_version".into(), rep.domain_version.clone()),
        ("dataset_hash".into(), format!("`{}`", rep.dataset_hash)),
        (
            "source_hash".into(),
            rep.source_hash
                .as_deref()
                .map(|h| format!("`{h}`"))
                .unwrap_or_else(|| "none".into()),
        ),
        ("golden_total".into(), rep.golden_total.to_string()),
        ("kind_counts".into(), kinds),
        ("eval_top_k".into(), rep.eval_top_k.to_string()),
        ("rrf_k".into(), rep.rrf_k.to_string()),
        ("vector_backend".into(), rep.vector_backend.clone()),
        ("command".into(), format!("`{}`", rep.command)),
    ]
}

/// 总体指标表（三档 × 固定列；双语共用同一数值格式，仅首列标题按语言）。
/// The overall metrics table (three tiers × fixed columns; both languages share
/// the same numeric formatting, only the first column title is localized).
fn overall_table(rep: &EvaluationReport, tier_title: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{tier_title} | recall@1 | recall@5 | recall@10 | negative_precision | fallback_count | effective_qug_samples\n"
    ));
    out.push_str("--- | --- | --- | --- | --- | --- | ---\n");
    for key in ["A", "B", "C"] {
        let t = &rep.tiers[key];
        let effective = if key == "C" {
            rep.decision.active_qug_samples.to_string()
        } else {
            "-".into()
        };
        out.push_str(&format!(
            "{tier_title} {key} | {} | {} | {} | {} | {} | {effective}\n",
            num(t.recall_at_1),
            num(t.recall_at_5),
            num(t.recall_at_10),
            num(t.negative_precision),
            t.fallback_count,
        ));
    }
    out.push('\n');
    out
}

/// 分 kind 分层表（单档；双语共用数值）。
/// The per-kind stratified table (one tier; shared values across languages).
fn kind_table(rep: &EvaluationReport, key: &str) -> String {
    let t = &rep.tiers[key];
    if t.by_kind.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("kind | count | recall@1 | recall@5 | recall@10 | negative_precision\n");
    out.push_str("--- | --- | --- | --- | --- | ---\n");
    for (kind, m) in &t.by_kind {
        out.push_str(&format!(
            "{kind} | {} | {} | {} | {} | {}\n",
            m.count,
            num(m.recall_at_1),
            num(m.recall_at_5),
            num(m.recall_at_10),
            num(m.negative_precision),
        ));
    }
    out.push('\n');
    out
}

/// 回退标注块（D6/A12：C 低于 B 时突出展示；双语各有文字、同一事实）。
/// The regression callout (D6/A12: highlighted whenever C is below B; per-
/// language wording over the same facts).
fn regression_callouts(rep: &EvaluationReport) -> Vec<String> {
    let mut out = Vec::new();
    let decision = serde_json::to_string(&rep.decision.qug_decision).unwrap_or_default();
    if rep.decision.recall_regression {
        out.push(format!(
            "> **⚠ 回退标注（recall）**：C 的 recall@10（{}）低于 B（{}），QUG 判定为 {decision}（disabled 为合格交付）；fallback 记录见 fallback_count={}。\n",
            num(rep.tiers["C"].recall_at_10),
            num(rep.tiers["B"].recall_at_10),
            rep.fallback_count
        ));
    }
    if rep.decision.negative_precision_regression {
        out.push(format!(
            "> **⚠ 回退标注（negative_precision）**：C 的 negative_precision（{}）低于 B（{}）。\n",
            num(rep.tiers["C"].negative_precision),
            num(rep.tiers["B"].negative_precision),
        ));
    }
    out
}

/// 中文 Markdown 报告（与英文逐节对应）。
/// The Chinese Markdown report (mirrors the English section by section).
pub fn render_markdown_zh(rep: &EvaluationReport) -> String {
    let mut s = String::new();
    s.push_str("# Step 5 QUG 三档评测报告\n\n");
    s.push_str("## 1. 概览\n\n");
    for (k, v) in overview_rows(rep) {
        s.push_str(&format!("- **{k}**：{v}\n"));
    }
    s.push_str("\n## 2. 三档定义\n\n");
    s.push_str("- **A**：纯 FTS5 BM25（无 QUG、无向量路）。\n");
    s.push_str("- **B**：FTS + 向量 + RRF（Step3 混合检索，无 QUG）。\n");
    s.push_str("- **C**：启用 QUG（rewrite + 过滤下推）后走同一混合检索；QUG 无匹配显式 fallback 并计入 fallback_count。\n");
    s.push_str(
        "- 三档共用同一数据库快照、accepted 代次、facts、向量输入与排序 tie-break（D6）。\n\n",
    );
    s.push_str("## 3. 总体指标\n\n");
    s.push_str(&overall_table(rep, "档位"));
    s.push_str("## 4. 按 kind 分层\n\n");
    for (idx, key) in ["A", "B", "C"].iter().enumerate() {
        s.push_str(&format!("### 4.{} 档位 {key}\n\n", idx + 1));
        s.push_str(&kind_table(rep, key));
    }
    s.push_str("## 5. QUG 判定（C 相对 B 的 recall@10）\n\n");
    s.push_str(&format!(
        "- **gain_pp**：{}\n- **qug_decision**：`{}`\n- **reason**：{}\n- **active QUG 有效样本**：{}（active 构建存在：{}）\n\n",
        rep.decision
            .gain_pp
            .map(|g| format!("{g:.6}"))
            .unwrap_or_else(|| "-".into()),
        serde_json::to_string(&rep.decision.qug_decision).unwrap_or_default(),
        rep.decision.reason,
        rep.decision.active_qug_samples,
        rep.decision.has_active_graph,
    ));
    let callouts = regression_callouts(rep);
    if !callouts.is_empty() {
        s.push_str("### 5.1 回退标注（C 低于 B）\n\n");
        for c in callouts {
            s.push_str(&c);
            s.push('\n');
        }
    }
    s.push_str("## 6. 失败样本\n\n");
    if rep.failed_samples.is_empty() {
        s.push_str("无（评测运行成功；单条 query 错误会在运行失败路径中列出）。\n");
    } else {
        s.push_str("query_id | kind | error\n--- | --- | ---\n");
        for f in &rep.failed_samples {
            s.push_str(&format!("{} | {} | {}\n", f.query_id, f.kind, f.error));
        }
    }
    s
}

/// 英文 Markdown 报告（与中文逐节对应；数字/decision/dataset_hash 完全一致）。
/// The English Markdown report (mirrors the Chinese section by section;
/// numbers/decision/dataset_hash are identical).
pub fn render_markdown_en(rep: &EvaluationReport) -> String {
    let mut s = String::new();
    s.push_str("# Step 5 QUG Three-Tier Evaluation Report\n\n");
    s.push_str("## 1. Overview\n\n");
    for (k, v) in overview_rows(rep) {
        s.push_str(&format!("- **{k}**: {v}\n"));
    }
    s.push_str("\n## 2. Tier definitions\n\n");
    s.push_str("- **A**: pure FTS5 BM25 (no QUG, no vector path).\n");
    s.push_str("- **B**: FTS + vector + RRF (the Step3 hybrid retrieval, no QUG).\n");
    s.push_str("- **C**: QUG enabled (rewrite + filter pushdown) over the same hybrid retrieval; QUG misses fall back explicitly and count into fallback_count.\n");
    s.push_str("- All tiers share one database snapshot, accepted generation, facts, vector input and sort tie-break (D6).\n\n");
    s.push_str("## 3. Overall metrics\n\n");
    s.push_str(&overall_table(rep, "Tier"));
    s.push_str("## 4. Per-kind stratification\n\n");
    for (idx, key) in ["A", "B", "C"].iter().enumerate() {
        s.push_str(&format!("### 4.{} Tier {key}\n\n", idx + 1));
        s.push_str(&kind_table(rep, key));
    }
    s.push_str("## 5. QUG decision (C vs B on recall@10)\n\n");
    s.push_str(&format!(
        "- **gain_pp**: {}\n- **qug_decision**: `{}`\n- **reason**: {}\n- **effective QUG samples**: {} (active build present: {})\n\n",
        rep.decision
            .gain_pp
            .map(|g| format!("{g:.6}"))
            .unwrap_or_else(|| "-".into()),
        serde_json::to_string(&rep.decision.qug_decision).unwrap_or_default(),
        rep.decision.reason,
        rep.decision.active_qug_samples,
        rep.decision.has_active_graph,
    ));
    // 回退标注文字双语分写，但数值与 fallback 计数来自同一模型。
    // The regression wording differs per language, but the numbers and the
    // fallback count come from the same model.
    let mut callouts = Vec::new();
    if rep.decision.recall_regression {
        callouts.push(format!(
            "> **⚠ Regression flag (recall)**: tier C's recall@10 ({}) is below tier B's ({}); the QUG verdict is `{}` (a disabled verdict is a valid delivery); see fallback_count={} for the fallback record.\n",
            num(rep.tiers["C"].recall_at_10),
            num(rep.tiers["B"].recall_at_10),
            serde_json::to_string(&rep.decision.qug_decision).unwrap_or_default(),
            rep.fallback_count
        ));
    }
    if rep.decision.negative_precision_regression {
        callouts.push(format!(
            "> **⚠ Regression flag (negative_precision)**: tier C's negative_precision ({}) is below tier B's ({}).\n",
            num(rep.tiers["C"].negative_precision),
            num(rep.tiers["B"].negative_precision),
        ));
    }
    if !callouts.is_empty() {
        s.push_str("### 5.1 Regression flags (C below B)\n\n");
        for c in callouts {
            s.push_str(&c);
            s.push('\n');
        }
    }
    s.push_str("## 6. Failed samples\n\n");
    if rep.failed_samples.is_empty() {
        s.push_str("None (the evaluation run succeeded; per-query errors are listed on the run-failure path).\n");
    } else {
        s.push_str("query_id | kind | error\n--- | --- | ---\n");
        for f in &rep.failed_samples {
            s.push_str(&format!("{} | {} | {}\n", f.query_id, f.kind, f.error));
        }
    }
    s
}

/// JSON 结果（契约字段全集；数字为原始 f64，不做字符串化）。
/// The JSON result (the full contract field set; numbers stay raw f64, never
/// stringified).
pub fn render_json(rep: &EvaluationReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(rep)?)
}

/// 写出三件套到 `dir`，返回三个文件路径（§6 契约文件名）。
/// Writes the trio into `dir` and returns the three file paths (§6 contract
/// file names).
pub fn write_report_files(dir: &Path, rep: &EvaluationReport) -> Result<[String; 3]> {
    std::fs::create_dir_all(dir)?;
    let zh = dir.join(EVAL_REPORT_FILE_ZH);
    let en = dir.join(EVAL_REPORT_FILE_EN);
    let json = dir.join(EVAL_REPORT_FILE_JSON);
    std::fs::write(&zh, render_markdown_zh(rep))?;
    std::fs::write(&en, render_markdown_en(rep))?;
    std::fs::write(&json, render_json(rep)?)?;
    Ok([
        zh.to_string_lossy().into_owned(),
        en.to_string_lossy().into_owned(),
        json.to_string_lossy().into_owned(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::runner::EvalOutcome;
    use crate::eval::{Decision, QugDecision, VariantMetrics};
    use std::collections::BTreeMap;

    /// 构造固定指标与产物（不连库；报告渲染是纯函数）。
    /// Builds fixed metrics and an outcome (no database; rendering is pure).
    fn variant(
        r1: Option<f64>,
        r5: Option<f64>,
        r10: Option<f64>,
        np: Option<f64>,
        fallback: usize,
    ) -> VariantMetrics {
        let mut by_kind = BTreeMap::new();
        by_kind.insert(
            "synonym".into(),
            KindMetrics {
                count: 2,
                recall_at_1: r1,
                recall_at_5: r5,
                recall_at_10: r10,
                negative_precision: None,
            },
        );
        VariantMetrics {
            recall_at_1: r1,
            recall_at_5: r5,
            recall_at_10: r10,
            negative_precision: np,
            by_kind,
            positive_samples: 2,
            exclusion_samples: 1,
            fallback_count: fallback,
        }
    }

    fn outcome(gain: Option<f64>, recall_regression: bool, np_regression: bool) -> EvalOutcome {
        let two_thirds = 2.0 / 3.0;
        let b = variant(
            Some(two_thirds),
            Some(two_thirds),
            Some(two_thirds),
            Some(two_thirds),
            0,
        );
        let c_r10 = match gain {
            Some(g) => Some(two_thirds + g / 100.0),
            None => Some(two_thirds),
        };
        let c_np = if np_regression {
            Some(two_thirds - 0.5)
        } else {
            Some(two_thirds)
        };
        let c = variant(c_r10, c_r10, c_r10, c_np, 1);
        EvalOutcome {
            eval_top_k: 10,
            tier_a: variant(
                Some(two_thirds),
                Some(two_thirds),
                Some(two_thirds),
                Some(two_thirds),
                0,
            ),
            tier_b: b.clone(),
            tier_c: c.clone(),
            decision: Decision {
                qug_decision: if gain.map(|g| g >= 5.0).unwrap_or(false) {
                    QugDecision::Enabled
                } else {
                    QugDecision::Disabled
                },
                gain_pp: gain,
                reason: "测试理由 / test reason".into(),
                recall_regression,
                negative_precision_regression: np_regression,
                active_qug_samples: 4,
                has_active_graph: true,
            },
            source_hash: Some("abc".into()),
            golden_total: 3,
            kind_counts: BTreeMap::from([("synonym".into(), 2), ("negative".into(), 1)]),
        }
    }

    fn golden() -> GoldenSet {
        GoldenSet::from_queries(Vec::new())
    }

    fn domain() -> DomainConfig {
        serde_yaml_ng::from_str("name: milk-tea\nversion: \"0.1.0\"\n").unwrap()
    }

    fn config() -> EvalConfig {
        EvalConfig {
            top_k: 10,
            rrf_k: 60,
            collection: "milk-tea".into(),
            vector_backend: "mock".into(),
            command: "wiktor eval --domain d.yaml --db d.db --golden g.jsonl --out-dir out".into(),
        }
    }

    /// A10/§6：双语报告数字、decision、dataset_hash 完全一致；JSON 契约字段
    /// 齐全；disabled 为默认判定。
    /// A10/§6: bilingual reports share identical numbers, decision and
    /// dataset_hash; the JSON carries every contract field; disabled is the
    /// default verdict.
    #[test]
    fn bilingual_reports_agree_and_json_carries_contract_fields() {
        let out = outcome(Some(0.0), false, false);
        let rep = EvaluationReport::build(&out, &golden(), &domain(), &config());
        let zh = render_markdown_zh(&rep);
        let en = render_markdown_en(&rep);
        let json = render_json(&rep).unwrap();

        // 数据集 hash / decision / reason 三处一致。
        // dataset_hash / decision / reason agree across all three artifacts.
        let hash = &rep.dataset_hash;
        assert!(hash.len() == 64);
        assert!(zh.contains(hash) && en.contains(hash) && json.contains(hash));
        assert!(zh.contains("\"disabled\"") && en.contains("\"disabled\""));
        assert!(zh.contains("测试理由 / test reason") && en.contains("测试理由 / test reason"));
        // 数字一致：2/3 与 0.000000 在两份 Markdown 均出现。
        // Numbers agree: 2/3 and 0.000000 appear in both Markdown reports.
        assert!(zh.contains("0.666667") && en.contains("0.666667"));
        assert!(zh.contains("0.000000") && en.contains("0.000000"));
        // 三档切片齐全。
        // All three tier slices exist.
        for key in ["A", "B", "C"] {
            assert!(zh.contains(&format!("档位 {key}")));
            assert!(en.contains(&format!("Tier {key}")));
        }
        // JSON 契约字段。
        // JSON contract fields.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["domain"], "milk-tea");
        assert_eq!(v["domain_version"], "0.1.0");
        assert_eq!(v["vector_backend"], "mock");
        assert_eq!(v["fallback_count"], 1);
        assert_eq!(v["decision"]["qug_decision"], "disabled");
        assert!(v["source_hash"].is_string());
        for key in ["A", "B", "C"] {
            let t = &v["tiers"][key];
            assert!(t["recall_at_1"].is_number());
            assert!(t["recall_at_5"].is_number());
            assert!(t["recall_at_10"].is_number());
            assert!(t["negative_precision"].is_number());
            assert!(t["by_kind"].is_object(), "per-kind stratification present");
        }
        assert!(v["failed_samples"].as_array().unwrap().is_empty());
        assert!(json.contains("wiktor eval --domain d.yaml"));
    }

    // enabled 路径：decision=enabled、gain 值入 JSON 与双语报告。
    // The enabled path: decision=enabled and the gain value lands in the JSON
    // and both reports.
    #[test]
    fn enabled_path_renders_gain_and_decision() {
        let out = outcome(Some(50.0), false, false);
        let rep = EvaluationReport::build(&out, &golden(), &domain(), &config());
        let zh = render_markdown_zh(&rep);
        let en = render_markdown_en(&rep);
        let json = render_json(&rep).unwrap();
        assert!(zh.contains("\"enabled\"") && en.contains("\"enabled\""));
        assert!(zh.contains("50.000000") && en.contains("50.000000"));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decision"]["qug_decision"], "enabled");
        assert_eq!(v["decision"]["gain_pp"], 50.0);
    }

    // 回退标注：C recall@10 低于 B → 中英文报告都含 5.1 回退标注节。
    // Regression flags: C recall@10 below B → both reports carry the 5.1
    // regression section.
    #[test]
    fn regression_flags_appear_in_both_languages() {
        let out = outcome(Some(-10.0), true, true);
        let rep = EvaluationReport::build(&out, &golden(), &domain(), &config());
        let zh = render_markdown_zh(&rep);
        let en = render_markdown_en(&rep);
        assert!(zh.contains("回退标注（recall）"));
        assert!(zh.contains("回退标注（negative_precision）"));
        assert!(en.contains("Regression flag (recall)"));
        assert!(en.contains("Regression flag (negative_precision)"));
        // 回退标注不改变判定本身（disabled 保持 disabled）。
        // Regression flags never flip the verdict (disabled stays disabled).
        assert!(zh.contains("\"disabled\""));
    }

    // 无回退时 5.1 节缺席。
    // Section 5.1 is absent without regressions.
    #[test]
    fn no_regression_section_without_regressions() {
        let out = outcome(Some(0.0), false, false);
        let rep = EvaluationReport::build(&out, &golden(), &domain(), &config());
        assert!(!render_markdown_zh(&rep).contains("5.1"));
        assert!(!render_markdown_en(&rep).contains("5.1"));
    }

    // 三件套落盘：文件名符合 §6 契约，内容与渲染函数一致。
    // The on-disk trio: §6 contract file names, contents match the renderers.
    #[test]
    fn write_report_files_writes_the_trio() {
        let dir = tempfile::tempdir().unwrap();
        let out = outcome(Some(0.0), false, false);
        let rep = EvaluationReport::build(&out, &golden(), &domain(), &config());
        let paths = write_report_files(dir.path(), &rep).unwrap();
        assert!(paths[0].ends_with(EVAL_REPORT_FILE_ZH));
        assert!(paths[1].ends_with(EVAL_REPORT_FILE_EN));
        assert!(paths[2].ends_with(EVAL_REPORT_FILE_JSON));
        let zh = std::fs::read_to_string(&paths[0]).unwrap();
        let en = std::fs::read_to_string(&paths[1]).unwrap();
        let json = std::fs::read_to_string(&paths[2]).unwrap();
        assert_eq!(zh, render_markdown_zh(&rep));
        assert_eq!(en, render_markdown_en(&rep));
        assert_eq!(json, render_json(&rep).unwrap());
        // 二次写入逐字节一致（报告无时间戳等非确定字段）。
        // A second write is byte-identical (no timestamps or other
        // nondeterministic fields).
        write_report_files(dir.path(), &rep).unwrap();
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), zh);
    }
}
