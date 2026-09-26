//! 四规则评分与发布门槛（Step 4 spec §6，决策 D4）。
//! Four-rule scoring and publish gates (Step 4 spec §6, decision D4).
//!
//! 语义（§6）：
//! - U：知识快照中非空 string leaf 指针集合（string 字段一项，reflist 每个非空
//!   元素一项）；C 为被有效且已使用且断言支持成功的引用命中的 U 子集。
//! - coverage=|C|/|U|（U 空取 0）；citation=min(S/A, V/R)（A=0 或 R=0 取 0）；
//!   schema_compliance 由 executor 以 schema_valid 传入（1/0）；density=I/T（T=0
//!   取 0，I 上限 T）。
//! - overall=四维平均；consistency=None（SQL NULL，不参与 overall）。
//! - 硬门槛：overall≥阈值 且 coverage/density≥min 且 schema=1 且 citation=1
//!   且无 ref issue。比值以 f64 中间计算后转 f32，无 epsilon；NaN/Inf 由
//!   [`validate_score_finite`] 拒绝，禁止 clamp 隐藏插件 bug。
//!
//! Step8 §6.2 扩展（偏差 STEP8-013）：`RuleScorer` 新增
//! [`RuleScorer::score_with_consistency`] 扩展方法（默认实现 = 旧四维路径，
//! 既有 executor 四维调用零改动）；`RuleBasedScorer` 覆写为五维组合——
//! consistency=None 时 overall 仍为四维平均，`Some(s)` 时五维等权，且
//! `s < min_consistency` 产生稳定 issue `CONSISTENCY_BELOW_THRESHOLD` 不得
//! accepted。
//!
//! Semantics (§6):
//! - U: non-empty string-leaf pointers in the knowledge snapshot (one per string
//!   field, one per non-empty reflist element); C is the subset of U hit by valid,
//!   used, assertion-supported refs.
//! - coverage=|C|/|U| (0 when U is empty); citation=min(S/A, V/R) (0 when A=0 or
//!   R=0); schema_compliance comes from the executor's schema_valid (1/0);
//!   density=I/T (0 when T=0, I capped at T).
//! - overall = average of the four dimensions; consistency=None (SQL NULL, not in
//!   overall).
//! - Hard gates: overall≥threshold AND coverage/density≥min AND schema=1 AND
//!   citation=1 AND no ref issue. Ratios are computed in f64 then converted to
//!   f32 with no epsilon; NaN/Inf is rejected by [`validate_score_finite`] —
//!   never clamped to hide plugin bugs.
//!
//! Step8 §6.2 extension (deviation STEP8-013): `RuleScorer` gains the
//! [`RuleScorer::score_with_consistency`] extension method (default
//! implementation = the legacy four-dim path, so the existing executor call site
//! is untouched); `RuleBasedScorer` overrides it with the five-dim combination —
//! consistency=None keeps the four-dim overall, `Some(s)` yields the five-dim
//! equal-weight average, and `s < min_consistency` raises the stable issue
//! `CONSISTENCY_BELOW_THRESHOLD` so the page can never be accepted.

use crate::compile::config::CompilePolicy;
use crate::compile::consistency::{ConsistencyReport, CONSISTENCY_BELOW_THRESHOLD};
use crate::compile::contract::{QualityIssue, RefReport};
use crate::types::error::{Error, Result};
use crate::types::{CompileContext, CompiledPage, QualityScore, RawEntity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 评分报告（§6 签名）。
/// Scoring report (§6 signature).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreReport {
    pub quality: QualityScore,
    pub issues: Vec<QualityIssue>,
    pub accepted: bool,
}

/// 四规则评分器（§6 签名；executor 在所有 Compiler 实现之后调用）。
/// The rule scorer (§6 signature; the executor calls it after every Compiler).
///
/// Step8 §6.2（偏差 STEP8-013）：扩展方法
/// [`RuleScorer::score_with_consistency`] 以「带默认实现 = 旧四维路径」的方式
/// 挂在本 trait 上，而非独立 `QualityScorerV2` trait——既有实现（只覆写
/// `score`）自动保持 Step4 行为，B3 接状态机时 executor 改点最小。
/// Step8 §6.2 (deviation STEP8-013): the extension method
/// [`RuleScorer::score_with_consistency`] hangs off this trait with a default
/// implementation equal to the legacy four-dim path instead of a separate
/// `QualityScorerV2` trait — existing implementations that only override `score`
/// keep Step4 behavior automatically, minimizing the executor delta when B3
/// wires the state machine.
pub trait RuleScorer: Send + Sync {
    fn score(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport;

    /// Step8 §6.2 扩展入口：在四维之上组合仲裁报告（`consistency.score` 为
    /// `None` 时与 [`RuleScorer::score`] 等价）。
    /// Step8 §6.2 extension entry: combines the arbitration report on top of the
    /// four dimensions (with `consistency.score` of `None` this equals
    /// [`RuleScorer::score`]).
    // 参数形状由 spec §6.2 签名逐字固定（self + 7 参），不做收敛重构。
    // The parameter shape is fixed verbatim by the §6.2 signature (self + 7
    // args); no collapsing refactor.
    #[allow(clippy::too_many_arguments)]
    fn score_with_consistency(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        consistency: &ConsistencyReport,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport {
        // 默认实现 = 旧四维路径：未适配一致性的实现行为逐字节不变。
        // Default = the legacy four-dim path: unadapted implementations stay
        // byte-identical.
        let _ = consistency;
        self.score(source, page, refs, schema_valid, ctx, policy)
    }
}

/// 内置规则评分器（scorer_version=rules-v1 的参考实现）。
/// The built-in rule scorer (the reference implementation of scorer_version=rules-v1).
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleBasedScorer;

impl RuleBasedScorer {
    /// 构造内置四规则评分器（无配置、无外部状态）。
    /// Constructs the built-in four-rule scorer (no config, no external state).
    pub fn new() -> Self {
        Self
    }
}

impl RuleScorer for RuleBasedScorer {
    /// 旧四维入口（Step4 兼容）：以「无可比较证据」的空报告走 V2 组合——
    /// 空报告 score=None，四维逻辑单源，行为逐字节不变。
    /// The legacy four-dim entry (Step4 compatible): routes through the V2
    /// combination with an empty no-comparison report — score=None, the four-dim
    /// logic stays single-source and byte-identical.
    fn score(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport {
        self.score_with_consistency(
            source,
            page,
            refs,
            schema_valid,
            &ConsistencyReport::default(),
            ctx,
            policy,
        )
    }

    /// Step8 §6.2：四维按 Step4 公式；consistency=None → 四维 overall；
    /// Some(s) → 五维等权（由 [`QualityScore::overall`] 单源实现）；且
    /// `s < min_consistency` 追加稳定 issue `CONSISTENCY_BELOW_THRESHOLD`，
    /// issues 非空即拒绝发布（D4）。
    /// Step8 §6.2: the four dimensions follow the Step4 formulas;
    /// consistency=None → four-dim overall; Some(s) → five-dim equal weight
    /// (single-sourced in [`QualityScore::overall`]); and `s < min_consistency`
    /// appends the stable issue `CONSISTENCY_BELOW_THRESHOLD` — a non-empty issue
    /// list rejects publishing (D4).
    #[allow(clippy::too_many_arguments)]
    fn score_with_consistency(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        consistency: &ConsistencyReport,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport {
        let mut issues = refs.issues.clone();
        // D4：有可比证据且低于阈值 → 稳定 issue（无论四维是否可算都记录）。
        // D4: comparable evidence below the floor → the stable issue (recorded
        // whether or not the four dimensions are computable).
        if let Some(s) = consistency.score {
            if s < policy.consistency.min_consistency {
                issues.push(QualityIssue {
                    code: CONSISTENCY_BELOW_THRESHOLD.to_string(),
                    path: "/consistency".to_string(),
                });
            }
        }
        // 无法解码（page=None）或结构失败：四维全零，仍产出可观测报告；一致性
        // 得分原样保留（结构失败页本身无可比较证据，正常管线不会出现 Some）。
        // Undecodable (page=None) or structural failure: all four dimensions
        // zero, still producing an observable report; the consistency score is
        // carried through verbatim (a structurally broken page has no comparable
        // evidence, so Some never occurs in the real pipeline).
        if page.is_none() || !schema_valid {
            return ScoreReport {
                quality: QualityScore {
                    coverage: 0.0,
                    citation: 0.0,
                    schema_compliance: 0.0,
                    density: 0.0,
                    consistency: consistency.score,
                },
                issues,
                accepted: false,
            };
        }

        // —— coverage：|C|/|U|，U 空取 0；重复引用不涨分（C 是集合）——
        // —— coverage: |C|/|U|, 0 when U is empty; repeated references add nothing
        //    (C is a set) ——
        let units = coverable_units(source);
        let covered: BTreeSet<&String> = refs
            .covered_units
            .iter()
            .filter(|p| units.contains(*p))
            .collect();
        let coverage = if units.is_empty() {
            0.0
        } else {
            (covered.len() as f64) / (units.len() as f64)
        };

        // —— citation：min(S/A, V/R)，A=0 或 R=0 取 0 ——
        // —— citation: min(S/A, V/R), 0 when A=0 or R=0 ——
        let citation = if refs.assertions == 0 || refs.ref_occurrences == 0 {
            0.0
        } else {
            let s_over_a = (refs.supported_assertions as f64) / (refs.assertions as f64);
            let v_over_r = (refs.valid_ref_occurrences as f64) / (refs.ref_occurrences as f64);
            s_over_a.min(v_over_r)
        };

        // —— density：I/T，T=0 取 0；I 上限 T ——
        // —— density: I/T, 0 when T=0; I capped at T ——
        let density = if refs.total_chars == 0 {
            0.0
        } else {
            let information = refs.information_chars.min(refs.total_chars) as f64;
            information / (refs.total_chars as f64)
        };

        // schema 合规即 1（schema_valid 已在入口判定）。
        // Schema compliance is 1 (schema_valid was adjudicated at the entry).
        let schema_compliance = 1.0f64;

        // f64 中间计算后转 f32；比较使用现有 f32 API，无 epsilon（§6）。
        // consistency 原样进入 QualityScore（None 时 overall 走四维平均）。
        // f64 intermediates converted to f32; comparisons use the existing f32 API
        // with no epsilon (§6). consistency flows into QualityScore verbatim
        // (None keeps the four-dim overall).
        let quality = QualityScore {
            coverage: coverage as f32,
            citation: citation as f32,
            schema_compliance: schema_compliance as f32,
            density: density as f32,
            consistency: consistency.score,
        };
        let accepted = passes_gates(&quality, &issues, ctx, policy);
        ScoreReport {
            quality,
            issues,
            accepted,
        }
    }
}

/// 发布硬门槛（§6/D4）：overall≥阈值 且 coverage/density≥min 且 schema=1 且
/// citation=1 且无 ref issue。所有比较为现有 f32 API，无 epsilon。
/// The publish hard gates (§6/D4): overall≥threshold AND coverage/density≥min AND
/// schema=1 AND citation=1 AND no ref issue. All comparisons use the existing f32
/// API with no epsilon.
fn passes_gates(
    quality: &QualityScore,
    issues: &[QualityIssue],
    ctx: &CompileContext,
    policy: &CompilePolicy,
) -> bool {
    issues.is_empty()
        && quality.overall() >= ctx.quality_threshold
        && quality.coverage >= policy.min_coverage
        && quality.density >= policy.min_density
        && quality.schema_compliance >= 1.0
        && quality.citation >= 1.0
}

/// U：知识快照中非空 string leaf 指针集合。
/// U: non-empty string-leaf pointers in the knowledge snapshot.
fn coverable_units(source: &RawEntity) -> BTreeSet<String> {
    let mut units = BTreeSet::new();
    for (name, value) in &source.fields {
        match value {
            serde_json::Value::String(s) if !s.is_empty() => {
                units.insert(format!("/fields/{name}"));
            }
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let serde_json::Value::String(s) = item {
                        if !s.is_empty() {
                            units.insert(format!("/fields/{name}/{i}"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    units
}

/// executor 发布前的有限性检查：任何 NaN/Inf/越界分数都是内部错误（§6，
/// 禁止 clamp 隐藏插件 bug）。Step8 §6.2：consistency 为 `Some(s)` 时同样
/// 必须 finite 且在 [0,1]，越界按内部/数据错误拒绝（fail-closed）。
/// Finiteness gate before publishing: any NaN/Inf/out-of-range score is an
/// internal error (§6; never clamp to hide plugin bugs). Step8 §6.2: a
/// `Some(s)` consistency must likewise be finite within [0,1]; violations are
/// rejected as internal/data errors (fail-closed).
pub fn validate_score_finite(report: &ScoreReport) -> Result<()> {
    let q = report.quality;
    for (name, v) in [
        ("coverage", q.coverage),
        ("citation", q.citation),
        ("schema_compliance", q.schema_compliance),
        ("density", q.density),
    ] {
        if !(v.is_finite() && (0.0..=1.0).contains(&v)) {
            return Err(Error::Internal(format!(
                "scorer produced non-finite or out-of-range {name}: {v}"
            )));
        }
    }
    if let Some(s) = q.consistency {
        if !(s.is_finite() && (0.0..=1.0).contains(&s)) {
            return Err(Error::Internal(format!(
                "scorer produced non-finite or out-of-range consistency: {s}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::config::CompilePolicy;
    use crate::compile::contract::{
        Assertion, CompileEvidence, DefaultSourceRefValidator, EvidenceSection, OutputWiki,
        SourceRef, SourceRefValidator,
    };
    use crate::types::{EntityId, PageMetadata, Section, WikiPage};
    use std::collections::BTreeMap;

    fn source() -> RawEntity {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠"));
        RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        }
    }

    fn evidence(markdown: &str) -> CompileEvidence {
        CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                title: "啵啵".to_string(),
                aliases: vec![],
                tags: vec![],
                markdown: markdown.to_string(),
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

    fn context(threshold: f32) -> CompileContext {
        CompileContext {
            domain_pack_version: "0.1.0".into(),
            prompt_template: "template".into(),
            model_version: "mock-v1".into(),
            embedding_model: "none".into(),
            quality_threshold: threshold,
            require_source_refs: true,
            schema_version: None,
            prompt_version: None,
        }
    }

    fn page(evidence: &CompileEvidence) -> CompiledPage {
        CompiledPage {
            wiki: WikiPage {
                page_id: "milk-tea:drink:boba".into(),
                entity_id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
                title: evidence.wiki.title.clone(),
                content: evidence.wiki.markdown.clone(),
                sections: vec![Section {
                    heading: "概述".into(),
                    content: evidence.wiki.markdown.clone(),
                }],
                metadata: PageMetadata {
                    domain_pack_version: "0.1.0".into(),
                    compiled_at: 0,
                    model_version: "mock-v1".into(),
                    embedding_model: "none".into(),
                },
                aliases: vec![],
                tags: vec![],
            },
            quality: QualityScore {
                coverage: 0.0,
                citation: 0.0,
                schema_compliance: 0.0,
                density: 0.0,
                consistency: None,
            },
            qug_edges: Vec::new(),
            content_hash: String::new(),
            evidence: Some(evidence.clone()),
        }
    }

    // A5：正引用 → citation=coverage=density=1，accepted。
    // A5: positive refs → citation=coverage=density=1, accepted.
    #[test]
    fn positive_refs_reach_full_scores() {
        let src = source();
        let ev = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let refs = DefaultSourceRefValidator.validate(&src, &ev, true);
        assert!(refs.issues.is_empty(), "issues: {:?}", refs.issues);
        let report = RuleBasedScorer.score(
            &src,
            Some(&page(&ev)),
            &refs,
            true,
            &context(0.75),
            &CompilePolicy::default(),
        );
        assert_eq!(report.quality.coverage, 1.0);
        assert_eq!(report.quality.citation, 1.0);
        assert_eq!(report.quality.schema_compliance, 1.0);
        assert_eq!(report.quality.density, 1.0);
        assert_eq!(report.quality.consistency, None);
        assert!(report.accepted);
        assert!(validate_score_finite(&report).is_ok());
    }

    // A9：门槛正反例 —— (0.6,1,1,0.4) 恰好过 0.75；coverage=.59（overall>0.75）
    // 被硬门槛拒绝；consistency 恒为 None。
    // A9: gate positives/negatives — (0.6,1,1,0.4) passes 0.75 exactly;
    // coverage=.59 (overall>0.75) is rejected by the hard gate; consistency stays
    // None.
    #[test]
    fn gate_boundary_cases() {
        let ctx = context(0.75);
        let policy = CompilePolicy::default();

        // 正例：spec §6 的 (0.6,1,1,0.4) → overall=0.75，恰好通过全部硬门槛。
        // Positive: the §6 example (0.6,1,1,0.4) → overall=0.75, passing all hard
        // gates exactly.
        let boundary = QualityScore {
            coverage: 0.6,
            citation: 1.0,
            schema_compliance: 1.0,
            density: 0.4,
            consistency: None,
        };
        assert_eq!(boundary.overall(), 0.75);
        assert!(passes_gates(&boundary, &[], &ctx, &policy));

        // 反例：coverage=.59 → overall≈0.8975>0.75 仍拒绝（硬门槛独立生效）。
        // Negative: coverage=.59 → overall≈0.8975>0.75, still rejected (each hard
        // gate applies independently).
        let low_coverage = QualityScore {
            coverage: 0.59,
            citation: 1.0,
            schema_compliance: 1.0,
            density: 1.0,
            consistency: None,
        };
        assert!(low_coverage.overall() > 0.75);
        assert!(!passes_gates(&low_coverage, &[], &ctx, &policy));

        // 无 ref issue 是门槛之一。/ Absence of ref issues is itself a gate.
        let with_issue = vec![QualityIssue {
            code: "UNUSED_REF".into(),
            path: "/sections/0/refs/0".into(),
        }];
        assert!(!passes_gates(&boundary, &with_issue, &ctx, &policy));

        // consistency 恒为 None（SQL NULL，不参与 overall）。
        // consistency stays None (SQL NULL, not part of overall).
        assert_eq!(boundary.consistency, None);
        assert_eq!(boundary.overall(), (0.6f32 + 1.0 + 1.0 + 0.4) / 4.0);
    }

    // A9：空输入/输出零分；page=None 或 schema 失败 → 四维全零 + consistency=None。
    // A9: empty input/output scores zero; page=None or schema failure → all four
    // dimensions zero with consistency=None.
    #[test]
    fn empty_and_broken_inputs_score_zero() {
        let scorer = RuleBasedScorer;
        let ctx = context(0.75);
        let policy = CompilePolicy::default();

        // page=None。/ page=None.
        let report = scorer.score(&source(), None, &RefReport::default(), true, &ctx, &policy);
        assert_eq!(report.quality.coverage, 0.0);
        assert_eq!(report.quality.citation, 0.0);
        assert_eq!(report.quality.schema_compliance, 0.0);
        assert_eq!(report.quality.density, 0.0);
        assert_eq!(report.quality.consistency, None);
        assert!(!report.accepted);

        // schema_valid=false。/ schema_valid=false.
        let ev = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let report = scorer.score(
            &source(),
            Some(&page(&ev)),
            &RefReport::default(),
            false,
            &ctx,
            &policy,
        );
        assert_eq!(report.quality.coverage, 0.0);
        assert!(!report.accepted);

        // 空知识源：U 空 → coverage 0；断言计数为 0 → citation 0。
        // Empty knowledge source: U empty → coverage 0; zero assertions → citation 0.
        let mut empty_fields = BTreeMap::new();
        empty_fields.insert("name".to_string(), serde_json::json!(""));
        let empty_src = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields: empty_fields,
            source_revision: 1,
        };
        let ev2 = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let refs = DefaultSourceRefValidator.validate(&empty_src, &ev2, true);
        let report = scorer.score(&empty_src, Some(&page(&ev2)), &refs, true, &ctx, &policy);
        assert_eq!(report.quality.coverage, 0.0);
        assert!(!report.accepted);
        // 所有分数有限且 [0,1]。
        // All scores finite within [0,1].
        assert!(validate_score_finite(&report).is_ok());
    }

    // A10：同 quote 重复十次 → density=0.1；重复 refs 不增 coverage。
    // A10: the same quote repeated ten times → density=0.1; repeated refs add no
    // coverage.
    #[test]
    fn density_penalizes_repetition() {
        let src = source();
        let text = ["啵啵"; 10].join(" ");
        let ev = CompileEvidence {
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
                    quote: "啵啵".to_string(),
                }],
            }],
            usage: None,
        };
        let mut ev = ev;
        ev.wiki.markdown = crate::compile::contract::render_canonical_markdown(&ev);
        let refs = DefaultSourceRefValidator.validate(&src, &ev, true);
        assert!(refs.issues.is_empty(), "issues: {:?}", refs.issues);
        let report = RuleBasedScorer.score(
            &src,
            Some(&page(&ev)),
            &refs,
            true,
            &context(0.75),
            &CompilePolicy::default(),
        );
        // T=20，I=2 → density=0.1。
        // T=20, I=2 → density=0.1.
        assert!((report.quality.density - 0.1).abs() < f32::EPSILON);
        // coverage：U 含 name/description，C 只有 name → 0.5（重复引用不涨分）。
        // coverage: U has name/description, C only name → 0.5 (repeats add nothing).
        assert!((report.quality.coverage - 0.5).abs() < 1e-6);
        assert!(!report.accepted, "density below min_density must reject");
    }

    // validate_score_finite 拒绝 NaN/Inf（插件 bug 不被 clamp 掩盖）。
    // validate_score_finite rejects NaN/Inf (plugin bugs are not hidden by clamping).
    #[test]
    fn non_finite_scores_are_internal_errors() {
        let mut report = ScoreReport {
            quality: QualityScore {
                coverage: f32::NAN,
                citation: 0.0,
                schema_compliance: 0.0,
                density: 0.0,
                consistency: None,
            },
            issues: vec![],
            accepted: false,
        };
        assert!(matches!(
            validate_score_finite(&report),
            Err(Error::Internal(_))
        ));
        report.quality.coverage = 1.5;
        assert!(matches!(
            validate_score_finite(&report),
            Err(Error::Internal(_))
        ));
        // Step8 §6.2：consistency Some(s) 越界同样拒绝（fail-closed）。
        // Step8 §6.2: an out-of-range consistency Some(s) is rejected too
        // (fail-closed).
        report.quality.coverage = 0.5;
        report.quality.consistency = Some(f32::NAN);
        assert!(matches!(
            validate_score_finite(&report),
            Err(Error::Internal(_))
        ));
        report.quality.consistency = Some(1.5);
        assert!(matches!(
            validate_score_finite(&report),
            Err(Error::Internal(_))
        ));
        report.quality.consistency = Some(0.0);
        assert!(validate_score_finite(&report).is_ok());
    }

    // —— Step8 §6.2 评分组合（A6/A3）/ Step8 §6.2 score combination (A6/A3) ——

    use crate::compile::config::ConsistencyPolicy;
    use crate::compile::consistency::{ConsistencyArbiter, ConsistencyReport};
    use std::sync::Arc;

    struct PresetArbiter {
        report: ConsistencyReport,
    }

    #[async_trait::async_trait]
    impl ConsistencyArbiter for PresetArbiter {
        async fn arbitrate(
            &self,
            _candidate: &CompiledPage,
            _related: &[CompiledPage],
            _policy: &crate::compile::config::ConsistencyPolicy,
        ) -> crate::types::error::Result<ConsistencyReport> {
            Ok(self.report.clone())
        }
    }

    fn report_with(score: Option<f32>) -> ConsistencyReport {
        ConsistencyReport {
            score,
            compared_claims: u32::from(score.is_some()),
            findings: vec![],
            candidate_count: 0,
        }
    }

    // A6（第一性）：overall() None → 四维平均逐字节不变；Some(s) → 五维等权。
    // A6 (first principles): overall() with None is the byte-identical four-dim
    // average; with Some(s) the five-dim equal-weight average.
    #[test]
    fn overall_branches_on_consistency() {
        let dims = (0.6f32, 1.0f32, 1.0f32, 0.4f32);
        let none = QualityScore {
            coverage: dims.0,
            citation: dims.1,
            schema_compliance: dims.2,
            density: dims.3,
            consistency: None,
        };
        assert_eq!(none.overall(), (dims.0 + dims.1 + dims.2 + dims.3) / 4.0);
        let some = QualityScore {
            consistency: Some(1.0),
            ..none
        };
        assert_eq!(
            some.overall(),
            (dims.0 + dims.1 + dims.2 + dims.3 + 1.0) / 5.0
        );
    }

    // A6/A3：fake arbiter（trait 对象）产出 None/1/0 报告流入评分组合器：
    // None → overall=旧四维平均且 accepted；Some(1) → 五维平均且 accepted；
    // Some(0) → 低于 min_consistency=1.0 产生 CONSISTENCY_BELOW_THRESHOLD 且
    // 不得 accepted；0/1 精确表达。
    // A6/A3: a fake arbiter (trait object) feeds None/1/0 reports into the
    // combinator: None → overall equals the legacy four-dim average and accepted;
    // Some(1) → the five-dim average and accepted; Some(0) → below
    // min_consistency=1.0 raises CONSISTENCY_BELOW_THRESHOLD and can never be
    // accepted; 0/1 are expressed exactly.
    #[tokio::test]
    async fn fake_arbiter_reports_flow_through_scorer() {
        let src = source();
        let ev = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let refs = DefaultSourceRefValidator.validate(&src, &ev, true);
        assert!(refs.issues.is_empty());
        let p = page(&ev);
        let ctx = context(0.75);
        let policy = CompilePolicy::default();
        let scorer = RuleBasedScorer;
        // 四维全 1（正引用满分路径）。
        // All four dimensions are 1 (the positive-refs full-score path).
        assert_eq!(
            scorer
                .score(&src, Some(&p), &refs, true, &ctx, &policy)
                .quality
                .coverage,
            1.0
        );

        // None：与旧四维入口 score() 逐字节一致。
        // None: byte-identical to the legacy score() entry.
        let arb_none = Arc::new(PresetArbiter {
            report: report_with(None),
        }) as Arc<dyn ConsistencyArbiter>;
        let r_none = arb_none
            .arbitrate(&p, &[], &policy.consistency)
            .await
            .unwrap();
        let via_v2 =
            scorer.score_with_consistency(&src, Some(&p), &refs, true, &r_none, &ctx, &policy);
        let legacy = scorer.score(&src, Some(&p), &refs, true, &ctx, &policy);
        assert_eq!(via_v2, legacy);
        assert_eq!(via_v2.quality.consistency, None);
        assert_eq!(via_v2.quality.overall(), 1.0);
        assert!(via_v2.accepted);

        // Some(1)：五维平均 = 1，accepted，0/1 精确表达。
        // Some(1): the five-dim average is 1, accepted, expressed exactly.
        let arb_one = Arc::new(PresetArbiter {
            report: report_with(Some(1.0)),
        }) as Arc<dyn ConsistencyArbiter>;
        let r_one = arb_one
            .arbitrate(&p, &[], &policy.consistency)
            .await
            .unwrap();
        let via_one =
            scorer.score_with_consistency(&src, Some(&p), &refs, true, &r_one, &ctx, &policy);
        assert_eq!(via_one.quality.consistency, Some(1.0));
        assert_eq!(via_one.quality.overall(), 1.0);
        assert!(via_one.issues.is_empty());
        assert!(via_one.accepted);
        assert!(validate_score_finite(&via_one).is_ok());

        // Some(0)：五维平均 0.8>0.75 但低于 min_consistency → 稳定 issue、不得
        // accepted。
        // Some(0): the five-dim average 0.8>0.75 but below min_consistency → the
        // stable issue and never accepted.
        let arb_zero = Arc::new(PresetArbiter {
            report: report_with(Some(0.0)),
        }) as Arc<dyn ConsistencyArbiter>;
        let r_zero = arb_zero
            .arbitrate(&p, &[], &policy.consistency)
            .await
            .unwrap();
        let via_zero =
            scorer.score_with_consistency(&src, Some(&p), &refs, true, &r_zero, &ctx, &policy);
        assert_eq!(via_zero.quality.consistency, Some(0.0));
        assert_eq!(via_zero.quality.overall(), 0.8);
        assert!(via_zero
            .issues
            .iter()
            .any(|i| i.code == "CONSISTENCY_BELOW_THRESHOLD"));
        assert!(!via_zero.accepted);
        assert!(validate_score_finite(&via_zero).is_ok());
    }

    // A6：Some(s) 高于自定义阈值（min_consistency=0.5）→ 无 issue、accepted；
    // 五维 overall 参与阈值门槛。
    // A6: Some(s) above a custom floor (min_consistency=0.5) → no issue, accepted;
    // the five-dim overall participates in the threshold gate.
    #[test]
    fn consistency_above_custom_floor_is_accepted() {
        let src = source();
        let ev = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let refs = DefaultSourceRefValidator.validate(&src, &ev, true);
        let p = page(&ev);
        let ctx = context(0.75);
        let policy = CompilePolicy {
            consistency: ConsistencyPolicy {
                min_consistency: 0.5,
                ..ConsistencyPolicy::default()
            },
            ..CompilePolicy::default()
        };
        let report = RuleBasedScorer.score_with_consistency(
            &src,
            Some(&p),
            &refs,
            true,
            &report_with(Some(0.6)),
            &ctx,
            &policy,
        );
        assert_eq!(report.quality.consistency, Some(0.6));
        // 五维：(1+1+1+1+0.6)/5 = 0.92。
        // Five dims: (1+1+1+1+0.6)/5 = 0.92.
        assert!((report.quality.overall() - 0.92).abs() < 1e-6);
        assert!(report.issues.is_empty());
        assert!(report.accepted);
    }

    // A3（scorer 侧）：只覆写 score() 的既有实现经默认 score_with_consistency
    // 保持旧四维路径（consistency 参数不影响结果）。
    // A3 (scorer side): an existing implementation that only overrides score()
    // keeps the legacy four-dim path through the default score_with_consistency
    // (the consistency parameter never affects the result).
    #[test]
    fn legacy_scorer_default_method_ignores_consistency() {
        struct LegacyOnly;
        impl RuleScorer for LegacyOnly {
            fn score(
                &self,
                source: &RawEntity,
                page: Option<&CompiledPage>,
                refs: &RefReport,
                schema_valid: bool,
                ctx: &CompileContext,
                policy: &CompilePolicy,
            ) -> ScoreReport {
                RuleBasedScorer.score(source, page, refs, schema_valid, ctx, policy)
            }
        }
        let src = source();
        let ev = evidence("## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n");
        let refs = DefaultSourceRefValidator.validate(&src, &ev, true);
        let ctx = context(0.75);
        let policy = CompilePolicy::default();
        let legacy = LegacyOnly.score(&src, Some(&page(&ev)), &refs, true, &ctx, &policy);
        let via_default = LegacyOnly.score_with_consistency(
            &src,
            Some(&page(&ev)),
            &refs,
            true,
            &report_with(Some(0.0)),
            &ctx,
            &policy,
        );
        assert_eq!(legacy, via_default);
        assert_eq!(via_default.quality.consistency, None);
    }
}
