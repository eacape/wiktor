//! Step 5 批5：评测指标与 QUG 启用判定（spec `step5-qug-build.md` §3 D6、§6、
//! §7 A10–A12）。
//! Step 5 batch 5: evaluation metrics and the QUG enablement decision (spec
//! `step5-qug-build.md` §3 D6, §6, §7 A10–A12).
//!
//! 口径（D6，逐条落实）：
//! - 正例 recall@k = `|top_k ∩ expected| / |expected|`，对全部非空期望样本取
//!   macro mean；k 固定为 1/5/10；
//! - negative 单独报告 `negative_precision`：对所有携带非空 `must_exclude` 的
//!   样本，top_k 结果中不得出现任何被排除实体，指标为合规样本占比；
//! - 期望为空的 negative/零命中样本既不计入正例 recall，也不计入
//!   negative_precision（无排除语义即无可检项）；
//! - 按 kind 分层对上述两个口径各算一遍（某层无正例/无排除样本时为 null）；
//! - 主判定 `gain_pp = (C_recall@10 − B_recall@10) × 100`，用未取整的 f64 判定；
//!   C 的 active QUG 有效改写样本不足 1 条（或无 active 图）视为无增益；
//! - `gain_pp ≥ 5.0` → `enabled`，否则 `disabled`；disabled 是合格交付；
//! - C 的 recall@10 或 negative_precision 低于 B 时打回退标注（报告突出展示），
//!   但不改变判定本身。
//!
//! Semantics (D6, item by item):
//! - positive recall@k = `|top_k ∩ expected| / |expected|`, macro-averaged over
//!   all samples with non-empty expectations; k is fixed at 1/5/10;
//! - negatives report `negative_precision` separately: over every sample with a
//!   non-empty `must_exclude`, no excluded entity may appear in the top_k
//!   results; the metric is the compliant fraction;
//! - negatives with empty expectations (zero-hit semantics) count toward neither
//!   positive recall nor negative_precision (nothing to check without
//!   exclusion semantics);
//! - per-kind stratification recomputes both aggregates per kind (null when a
//!   stratum has no positive / no exclusion samples);
//! - the primary decision is `gain_pp = (C_recall@10 − B_recall@10) × 100`,
//!   judged on unrounded f64; fewer than 1 effective C rewrite sample under an
//!   active QUG (or no active graph) counts as no gain;
//! - `gain_pp ≥ 5.0` → `enabled`, otherwise `disabled`; disabled is a valid
//!   delivery;
//! - C recall@10 or negative_precision below B raises a regression flag
//!   (highlighted in the report) without changing the decision itself.

use crate::eval::GoldenKind;
use serde::Serialize;
use std::collections::BTreeMap;

/// 评测固定 top-k（D7：默认 10，范围 10..=100）。
/// Fixed evaluation top-k (D7: default 10, range 10..=100).
pub const EVAL_TOP_K_DEFAULT: usize = 10;

/// QUG 启用阈值（D6：gain_pp ≥ 5.0 → enabled）。
/// QUG enablement threshold (D6: gain_pp ≥ 5.0 → enabled).
pub const QUG_ENABLE_GAIN_PP_THRESHOLD: f64 = 5.0;

/// 单条 golden 样本在三档之一的运行结果（指标计算的输入）。
/// One golden sample's run under one of the three tiers (metrics input).
#[derive(Debug, Clone)]
pub struct SampleRun {
    /// golden 记录 id（报告与失败清单用）。
    /// Golden record id (reports and failure lists).
    pub query_id: String,
    pub kind: GoldenKind,
    /// 期望实体（entity key 形式；空 = 零命中语义，不计入正例 recall）。
    /// Expected entities (entity keys; empty = zero-hit semantics, excluded
    /// from positive recall).
    pub expected: Vec<String>,
    /// 结果不得包含的实体（非空才计入 negative_precision）。
    /// Entities that must not appear (non-empty counts toward
    /// negative_precision).
    pub must_exclude: Vec<String>,
    /// 该样本的命中实体（按最终排序，长度 ≤ eval top_k）。
    /// Hit entities in final order (length ≤ eval top_k).
    pub hit_entity_ids: Vec<String>,
    /// 引擎报告的 rewrite_failure（C 档 Fallback；A/B 恒 false）。
    /// Engine-reported rewrite_failure (tier-C Fallback; always false for A/B).
    pub rewrite_failure: bool,
    /// 引擎报告的 rewrite_status == Applied（C 档"有效样本"计数）。
    /// Engine-reported rewrite_status == Applied (tier-C effective-sample count).
    pub rewrite_applied: bool,
}

/// 单个 kind 分层的指标（无正例 → recall 为 None；无排除样本 →
/// negative_precision 为 None）。
/// Metrics for one kind stratum (no positives → None recall; no exclusion
/// samples → None negative_precision).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KindMetrics {
    /// 该 kind 的样本数（正例 + 排除 + 零命中全都计入）。
    /// Sample count of this kind (positives + exclusions + zero-hit all count).
    pub count: usize,
    pub recall_at_1: Option<f64>,
    pub recall_at_5: Option<f64>,
    pub recall_at_10: Option<f64>,
    pub negative_precision: Option<f64>,
}

/// 单档（A/B/C）指标汇总。
/// Aggregate metrics for one tier (A/B/C).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VariantMetrics {
    pub recall_at_1: Option<f64>,
    pub recall_at_5: Option<f64>,
    pub recall_at_10: Option<f64>,
    pub negative_precision: Option<f64>,
    /// 按 kind 分层（键为 [`GoldenKind::as_str`]）。
    /// Per-kind stratification (keyed by [`GoldenKind::as_str`]).
    pub by_kind: BTreeMap<String, KindMetrics>,
    /// 参与正例 recall 的样本数。
    /// Samples participating in positive recall.
    pub positive_samples: usize,
    /// 参与 negative_precision 的样本数。
    /// Samples participating in negative_precision.
    pub exclusion_samples: usize,
    /// rewrite_failure 样本数（D6 fallback：C 档 QUG 无匹配显式 fallback）。
    /// rewrite_failure sample count (D6 fallback: tier-C explicit fallback on
    /// no QUG match).
    pub fallback_count: usize,
}

/// 正例 recall@k 的 macro mean：每样本 `|top_k ∩ expected| / |expected|`。
/// Macro mean of positive recall@k: per sample
/// `|top_k ∩ expected| / |expected|`.
fn macro_recall(samples: &[&SampleRun], k: usize) -> Option<f64> {
    let positives: Vec<&SampleRun> = samples
        .iter()
        .copied()
        .filter(|s| !s.expected.is_empty())
        .collect();
    if positives.is_empty() {
        return None;
    }
    let sum: f64 = positives
        .iter()
        .map(|s| {
            let top: std::collections::BTreeSet<&str> = s
                .hit_entity_ids
                .iter()
                .take(k)
                .map(String::as_str)
                .collect();
            let hits = s
                .expected
                .iter()
                .filter(|e| top.contains(e.as_str()))
                .count();
            hits as f64 / s.expected.len() as f64
        })
        .sum();
    Some(sum / positives.len() as f64)
}

/// negative_precision：携带 must_exclude 的样本中，top_k 不含任何排除实体的占比。
/// negative_precision: fraction of must_exclude samples whose top_k contains no
/// excluded entity.
fn negative_precision(samples: &[&SampleRun], k: usize) -> Option<f64> {
    let exclusions: Vec<&SampleRun> = samples
        .iter()
        .copied()
        .filter(|s| !s.must_exclude.is_empty())
        .collect();
    if exclusions.is_empty() {
        return None;
    }
    let compliant = exclusions
        .iter()
        .filter(|s| {
            !s.hit_entity_ids
                .iter()
                .take(k)
                .any(|hit| s.must_exclude.contains(hit))
        })
        .count();
    Some(compliant as f64 / exclusions.len() as f64)
}

/// 汇总单档指标（D6 固定 @1/@5/@10 + negative_precision + 分 kind + fallback）。
/// Aggregates one tier's metrics (D6 fixed @1/@5/@10 + negative_precision +
/// per-kind + fallback).
pub fn evaluate_variant(samples: &[SampleRun], top_k: usize) -> VariantMetrics {
    let refs: Vec<&SampleRun> = samples.iter().collect();
    let mut by_kind: BTreeMap<String, KindMetrics> = BTreeMap::new();
    for kind in [
        GoldenKind::Synonym,
        GoldenKind::Intent,
        GoldenKind::Negation,
        GoldenKind::AttributeFilter,
        GoldenKind::Negative,
        GoldenKind::Legacy,
    ] {
        let stratum: Vec<&SampleRun> = refs.iter().copied().filter(|s| s.kind == kind).collect();
        if stratum.is_empty() {
            continue;
        }
        by_kind.insert(
            kind.as_str().to_string(),
            KindMetrics {
                count: stratum.len(),
                recall_at_1: macro_recall(&stratum, 1),
                recall_at_5: macro_recall(&stratum, 5),
                recall_at_10: macro_recall(&stratum, top_k.min(10)),
                negative_precision: negative_precision(&stratum, top_k),
            },
        );
    }
    VariantMetrics {
        recall_at_1: macro_recall(&refs, 1),
        recall_at_5: macro_recall(&refs, 5),
        recall_at_10: macro_recall(&refs, top_k.min(10)),
        negative_precision: negative_precision(&refs, top_k),
        by_kind,
        positive_samples: refs.iter().filter(|s| !s.expected.is_empty()).count(),
        exclusion_samples: refs.iter().filter(|s| !s.must_exclude.is_empty()).count(),
        fallback_count: refs.iter().filter(|s| s.rewrite_failure).count(),
    }
}

/// QUG 启用判定结论（D6：只有 enabled / disabled 两态；disabled 是合格交付）。
/// The QUG enablement verdict (D6: exactly two states; disabled is a valid
/// delivery).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QugDecision {
    Enabled,
    Disabled,
}

/// 判定明细：决策 + 未取整 gain + 双语 reason + 回退标注。
/// Decision details: the verdict + unrounded gain + bilingual reason +
/// regression flags.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub qug_decision: QugDecision,
    /// `gain_pp = (C_recall@10 − B_recall@10) × 100`（绝对百分点）；无增益路径为 None。
    /// `gain_pp = (C_recall@10 − B_recall@10) × 100` (absolute percentage points); None on the no-gain paths.
    pub gain_pp: Option<f64>,
    /// `relative_gain = (C−B)/B`（相对提升，B>0 才有定义）；与 `gain_pp` 双指标并录，
    /// 供口径统一对比（MASTER-PLAN §5.2 与代码实现 P2 统一：启用判定用绝对 pp）。
    /// `relative_gain = (C−B)/B` (relative improvement, defined only when B>0);
    /// recorded alongside `gain_pp` for cross-metric comparison (P2 unifies the
    /// §5.2 wording with the code: enablement is judged on absolute pp).
    pub relative_gain: Option<f64>,
    /// 判定原因（中英并列；JSON 与双语报告共用同一字符串）。
    /// Decision reason (Chinese + English; shared by the JSON and both
    /// markdown reports).
    pub reason: String,
    /// C 的 recall@10 低于 B（报告突出标注）。
    /// C recall@10 below B (highlighted in the report).
    pub recall_regression: bool,
    /// C 的 negative_precision 低于 B（报告突出标注）。
    /// C negative_precision below B (highlighted in the report).
    pub negative_precision_regression: bool,
    /// C 档 rewrite_status == Applied 的样本数（"有效样本"）。
    /// Tier-C sample count with rewrite_status == Applied (effective samples).
    pub active_qug_samples: usize,
    /// 是否存在 active published QUG 构建（load_active_qug 返回 Some）。
    /// Whether an active published QUG build exists (load_active_qug → Some).
    pub has_active_graph: bool,
}

/// 主判定（D6/A11/A12）。
/// The primary decision (D6/A11/A12).
///
/// - 无 active 图或 C 有效改写样本不足 1 条 → disabled（无增益路径，gain None）；
/// - 否则 `gain_pp = (C@10 − B@10) × 100`（未取整 f64 判定）：≥ 5.0 → enabled，
///   否则 disabled；
/// - 回退标注独立计算，不反转判定。
/// - No active graph or <1 effective C rewrite sample → disabled (the no-gain
///   path, gain None);
/// - otherwise `gain_pp = (C@10 − B@10) × 100` (judged on unrounded f64):
///   ≥ 5.0 → enabled, else disabled;
/// - regression flags are computed independently and never flip the verdict.
pub fn decide(
    b: &VariantMetrics,
    c: &VariantMetrics,
    has_active_graph: bool,
    active_qug_samples: usize,
) -> Decision {
    let recall_regression =
        matches!((b.recall_at_10, c.recall_at_10), (Some(bv), Some(cv)) if cv < bv);
    let negative_precision_regression = matches!(
        (b.negative_precision, c.negative_precision),
        (Some(bv), Some(cv)) if cv < bv
    );

    if !has_active_graph || active_qug_samples < 1 {
        return Decision {
            qug_decision: QugDecision::Disabled,
            gain_pp: None,
            relative_gain: None,
            reason: "无 active QUG 构建或 C 档有效改写样本不足 1 条，视为无增益，判定关闭 \
                     / no active QUG build or fewer than 1 effective C rewrite sample; \
                     treated as no gain, disabled"
                .into(),
            recall_regression,
            negative_precision_regression,
            active_qug_samples,
            has_active_graph,
        };
    }
    let Some(bv) = b.recall_at_10 else {
        return Decision {
            qug_decision: QugDecision::Disabled,
            gain_pp: None,
            relative_gain: None,
            reason: "B 档正例 recall@10 无定义（无正例样本），视为无增益，判定关闭 \
                     / tier-B positive recall@10 undefined (no positive samples); \
                     treated as no gain, disabled"
                .into(),
            recall_regression,
            negative_precision_regression,
            active_qug_samples,
            has_active_graph,
        };
    };
    let Some(cv) = c.recall_at_10 else {
        return Decision {
            qug_decision: QugDecision::Disabled,
            gain_pp: None,
            relative_gain: None,
            reason: "C 档正例 recall@10 无定义（无正例样本），视为无增益，判定关闭 \
                     / tier-C positive recall@10 undefined (no positive samples); \
                     treated as no gain, disabled"
                .into(),
            recall_regression,
            negative_precision_regression,
            active_qug_samples,
            has_active_graph,
        };
    };
    let gain_pp = (cv - bv) * 100.0;
    // P2：相对增益双指标并录（B>0 才有定义；供口径统一对比，不参与启用判定）。
    // P2: relative gain recorded alongside absolute pp (defined only when B>0;
    // for cross-metric comparison; never used for the enablement verdict).
    let relative_gain = if bv > 0.0 { Some((cv - bv) / bv) } else { None };
    let (qug_decision, reason) = if gain_pp >= QUG_ENABLE_GAIN_PP_THRESHOLD {
        (
            QugDecision::Enabled,
            format!(
                "C−B recall@10 增益 {gain_pp:.6}pp 达到 5.0pp 阈值，判定启用 \
                 / C−B recall@10 gain {gain_pp:.6}pp meets the 5.0pp threshold; enabled"
            ),
        )
    } else {
        (
            QugDecision::Disabled,
            format!(
                "C−B recall@10 增益 {gain_pp:.6}pp 低于 5.0pp 阈值，判定关闭（disabled 为合格交付） \
                 / C−B recall@10 gain {gain_pp:.6}pp is below the 5.0pp threshold; disabled \
                 (a disabled verdict is a valid delivery)"
            ),
        )
    };
    Decision {
        qug_decision,
        gain_pp: Some(gain_pp),
        relative_gain,
        reason,
        recall_regression,
        negative_precision_regression,
        active_qug_samples,
        has_active_graph,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快捷样本构造。
    /// Sample constructor shortcut.
    fn sample(
        id: &str,
        kind: GoldenKind,
        expected: &[&str],
        must_exclude: &[&str],
        hits: &[&str],
        fallback: bool,
    ) -> SampleRun {
        SampleRun {
            query_id: id.into(),
            kind,
            expected: expected.iter().map(|s| s.to_string()).collect(),
            must_exclude: must_exclude.iter().map(|s| s.to_string()).collect(),
            hit_entity_ids: hits.iter().map(|s| s.to_string()).collect(),
            rewrite_failure: fallback,
            rewrite_applied: !fallback && !expected.is_empty(),
        }
    }

    // 正例 recall：|top_k ∩ expected| / |expected| 的 macro mean；@1/@5/@10 前缀。
    // Positive recall: macro mean of |top_k ∩ expected| / |expected|; @1/@5/@10
    // are prefixes.
    #[test]
    fn recall_macro_mean_and_prefixes() {
        let samples = vec![
            sample(
                "p1",
                GoldenKind::Synonym,
                &["a", "b"],
                &[],
                &["a", "x", "b"],
                false,
            ),
            sample("p2", GoldenKind::Intent, &["c"], &[], &["c"], false),
        ];
        let m = evaluate_variant(&samples, 10);
        // p1 @1 = 1/2, @5 = 2/2; p2 = 1 → macro @1 = 0.75, @5/@10 = 1.0。
        // p1 @1 = 1/2, @5 = 2/2; p2 = 1 → macro @1 = 0.75, @5/@10 = 1.0.
        assert_eq!(m.recall_at_1, Some((0.5 + 1.0) / 2.0));
        assert_eq!(m.recall_at_5, Some(1.0));
        assert_eq!(m.recall_at_10, Some(1.0));
        assert_eq!(m.positive_samples, 2);
        assert_eq!(m.exclusion_samples, 0);
        assert_eq!(m.negative_precision, None, "no exclusion samples");
    }

    // 空期望的 negative 不计入正例 recall；negative_precision 只看 must_exclude。
    // Empty-expected negatives are excluded from positive recall;
    // negative_precision looks only at must_exclude.
    #[test]
    fn zero_hit_negatives_excluded_and_negative_precision() {
        let samples = vec![
            sample("p1", GoldenKind::Synonym, &["a"], &[], &["a"], false),
            // 零命中语义：无期望也无排除 → 两边都不计。
            // Zero-hit semantics: no expectations and no exclusions → counts
            // toward neither.
            sample("z1", GoldenKind::Negative, &[], &[], &["b"], false),
            // 排除样本：命中含被排除实体 → 不合规。
            // Exclusion sample: a hit contains the excluded entity → violates.
            sample("n1", GoldenKind::Negation, &[], &["b"], &["b"], false),
            sample("n2", GoldenKind::Negative, &[], &["c"], &["a", "b"], false),
        ];
        let m = evaluate_variant(&samples, 10);
        assert_eq!(m.recall_at_10, Some(1.0), "only p1 counts");
        assert_eq!(
            m.negative_precision,
            Some(0.5),
            "1 of 2 exclusion samples compliant"
        );
        assert_eq!(m.exclusion_samples, 2);
        let negative = &m.by_kind["negative"];
        assert_eq!(
            negative.count, 2,
            "zero-hit + exclusion negatives both counted"
        );
        assert_eq!(negative.recall_at_10, None, "no positive negative samples");
        // 该层内只有 n2 携带 must_exclude（且合规）→ np = 1.0；总体 np 由
        // n1(违规) + n2(合规) 取均值 0.5。
        // Within this stratum only n2 carries must_exclude (and complies) →
        // np = 1.0; the overall np averages n1 (violation) + n2 (compliant).
        assert_eq!(negative.negative_precision, Some(1.0));
    }

    // fallback 计数与分 kind 键名。
    // Fallback counting and per-kind key names.
    #[test]
    fn fallback_count_and_kind_keys() {
        let samples = vec![
            sample("p1", GoldenKind::Synonym, &["a"], &[], &["a"], false),
            sample("f1", GoldenKind::Intent, &["b"], &[], &[], true),
        ];
        let m = evaluate_variant(&samples, 10);
        assert_eq!(m.fallback_count, 1);
        assert_eq!(m.by_kind["synonym"].count, 1);
        assert_eq!(m.by_kind["intent"].count, 1);
        assert!(m.by_kind["intent"].recall_at_10.unwrap() == 0.0);
    }

    // 判定路径：enabled（≥5pp）/ disabled（<5pp）/ 无 active 图 / 无有效样本 /
    // recall 未定义 / 回退标注不反转判定。
    // Decision paths: enabled (≥5pp) / disabled (<5pp) / no active graph / no
    // effective samples / undefined recall / regression flags never flip.
    #[test]
    fn decision_paths() {
        let b_high = evaluate_variant(
            &[sample("p", GoldenKind::Synonym, &["a"], &[], &["a"], false)],
            10,
        );
        let c_low = evaluate_variant(
            &[sample("p", GoldenKind::Synonym, &["a"], &[], &[], false)],
            10,
        );

        // 无 active 图 → disabled（gain None），即使数值上 C=B。
        // No active graph → disabled (gain None) even when C == B numerically.
        let d = decide(&b_high, &b_high, false, 5);
        assert_eq!(d.qug_decision, QugDecision::Disabled);
        assert_eq!(d.gain_pp, None);
        assert!(d.reason.contains("no active QUG build"));

        // 有图但有效样本 0 → disabled。
        // Graph present but 0 effective samples → disabled.
        let d = decide(&b_high, &b_high, true, 0);
        assert_eq!(d.qug_decision, QugDecision::Disabled);
        assert!(d.reason.contains("fewer than 1 effective"));

        // gain = -100pp → disabled + recall 回退标注；运行本身仍是成功语义。
        // gain = -100pp → disabled + recall regression flag; the run itself is
        // still a success.
        let d = decide(&b_high, &c_low, true, 1);
        assert_eq!(d.qug_decision, QugDecision::Disabled);
        assert_eq!(d.gain_pp, Some(-100.0));
        assert!(d.recall_regression);
        assert!(!d.negative_precision_regression);

        // gain = 4.999...（不取整判定）→ disabled；gain = 5.0 → enabled。
        // gain = 4.999... (no rounding) → disabled; gain = 5.0 → enabled.
        let b = VariantMetrics {
            recall_at_10: Some(0.5),
            ..b_high.clone()
        };
        let c_just_below = VariantMetrics {
            recall_at_10: Some(0.5 + 0.049_999),
            ..b_high.clone()
        };
        assert_eq!(
            decide(&b, &c_just_below, true, 1).qug_decision,
            QugDecision::Disabled
        );
        let c_at_threshold = VariantMetrics {
            recall_at_10: Some(0.55),
            ..b_high.clone()
        };
        let d = decide(&b, &c_at_threshold, true, 1);
        assert_eq!(d.qug_decision, QugDecision::Enabled);
        // f64 增益非精确 5.0（0.55−0.5 → 5.000000000000004），阈值判定本身
        // 不取整；断言用容差。
        // The f64 gain is not exactly 5.0 (0.55−0.5 → 5.000000000000004); the
        // threshold judgment stays unrounded; the assertion uses a tolerance.
        assert!((d.gain_pp.unwrap() - 5.0).abs() < 1e-9);

        // negative_precision 回退：C np 更低但 recall 达标 → enabled + np 标注。
        // negative_precision regression: lower C np with sufficient recall →
        // enabled + np flag.
        let b_np_high = VariantMetrics {
            negative_precision: Some(1.0),
            ..b.clone()
        };
        let c_np_low = VariantMetrics {
            negative_precision: Some(0.0),
            ..c_at_threshold.clone()
        };
        let d = decide(&b_np_high, &c_np_low, true, 1);
        assert_eq!(d.qug_decision, QugDecision::Enabled);
        assert!(d.negative_precision_regression);
        assert!(!d.recall_regression);
    }
}
