//! Step 6 批3：三信号分析器（spec `step6-feedback-loop.md` §3 D11、§6、
//! §10 A10–A12、§11 批3）。
//! Step 6 batch 3: the three-signal analyzer (spec `step6-feedback-loop.md`
//! §3 D11, §6, §10 A10–A12, §11 batch 3).
//!
//! 职责边界（上层口径）：**纯函数 + 确定性**——接收 [`FeedbackWindow`] 返回
//! [`FeedbackReport`]，绝不写库（review_queue 插入由调用方组合，批6 CLI analyze
//! 通过 [`crate::store::FeedbackStore::insert_review_suggestions`] 落库，唯一键
//! 保证重复分析幂等），不调用 compile/admission。
//! Responsibility boundary (upstream decision): **pure function + determinism**
//! — takes a [`FeedbackWindow`] and returns a [`FeedbackReport`], never writes
//! (review_queue insertion is composed by the caller; the batch-6 CLI analyze
//! persists via [`crate::store::FeedbackStore::insert_review_suggestions`],
//! whose unique key makes repeated analysis idempotent), and never calls
//! compile/admission.
//!
//! 三信号判据（D11，逐字实现）：
//! - 零召回：`hit_count=0 AND NOT(candidate_empty_initial=1 AND
//!   relaxation_succeeded=0)`——滤空且放宽仍失败 ≠ 盲区，明确排除（A10）；
//!   legacy 行三状态默认 false，按普通零召回处理（STEP6-002：旧行不能安全判
//!   定滤空语义）；
//! - 改写失败：`rewrite_failure=1`（独立信号；与零召回可同时命中，两类各自
//!   记录同一条 log）；
//! - 低质量：按 page_id 聚合 click/adopt/rate（`adopt` 计 1、`rate>=4` 计 1、
//!   `click` 只进分母），事件数 ≥ 5 且采纳率**严格** < 0.20（等于不触发，A11；
//!   采纳率用 20/100 的整数交叉相乘比较，杜绝浮点边界误差）。
//!
//! Three-signal criteria (D11, verbatim):
//! - zero recall: `hit_count=0 AND NOT(candidate_empty_initial=1 AND
//!   relaxation_succeeded=0)` — filter-empty with a failed relaxation is NOT a
//!   blind spot and is explicitly excluded (A10); legacy rows default the three
//!   state flags to false and count as plain zero recall (STEP6-002: old rows
//!   cannot be safely judged under the filter-empty semantics);
//! - rewrite failure: `rewrite_failure=1` (an independent signal; it may
//!   coincide with zero recall, and both classes then record the same log);
//! - low quality: per-page aggregation of click/adopt/rate (`adopt` scores 1,
//!   `rate>=4` scores 1, `click` counts toward the denominator only), with at
//!   least 5 events and an adoption rate **strictly** < 0.20 (equality does not
//!   trigger, A11; the rate is compared via exact integer cross-multiplication
//!   over 20/100, eliminating float-boundary error).
//!
//! subject_json 确定性（spec §6）：零召回/改写失败按 (domain, normalized
//! query_text) 去重，保留 log_ids（升序）、出现次数、平均 latency；低质量按
//! page_id 记录 feedback_count/adopted_count/adoption_rate。normalized 口径
//! 复用 `wiktor_core::query_engine::qug::normalize`（Step 3 §3.1 契约）；该函
//! 数对空短语/超 128 scalar 拒绝——违反契约的日志行走 fail-closed（整体
//! Validation），绝不静默跳过或猜造兜底键。
//! subject_json determinism (spec §6): zero recall / rewrite failures dedup by
//! (domain, normalized query_text), keeping log_ids (ascending), occurrence
//! count and average latency; low quality records feedback_count /
//! adopted_count / adoption_rate per page_id. The normalized form reuses
//! `wiktor_core::query_engine::qug::normalize` (the Step 3 §3.1 contract); that
//! function rejects empty phrases / phrases over 128 scalars — a log row
//! violating the contract fails closed (whole-analysis Validation), never a
//! silent skip nor an invented fallback key.

use crate::report::{
    BlindSpotQuery, FeedbackCounts, FeedbackReport, FeedbackThresholds, LowQualityPage,
    ReviewSuggestion, REPORT_MAX_ITEMS_PER_CLASS, REPORT_SCHEMA_VERSION,
};
use wiktor_core::kernel::feedback_store::{FeedbackEvent, FeedbackKind, QueryLogSnapshot};
use wiktor_core::query_engine::qug::normalize;
use wiktor_core::types::error::{Error, Result};

/// 低质量判据最小事件数（D11：至少 5 个 click/adopt/rate 事件）。
/// Minimum event count for the low-quality rule (D11: at least 5 click/adopt/
/// rate events).
pub const MIN_EVENTS: usize = 5;

/// 低质量判据采纳率阈值（D11：严格 < 0.20 触发，等于不触发）。
/// Adoption-rate threshold of the low-quality rule (D11: strictly < 0.20
/// triggers; equality does not).
pub const ADOPTION_THRESHOLD: f64 = 0.20;

/// 采纳率阈值的有理数形式 20/100（整数交叉相乘用，见 [`adoption_below_threshold`]）。
/// The threshold as the exact rational 20/100 (for integer cross-multiplication;
/// see [`adoption_below_threshold`]).
const ADOPTION_THRESHOLD_NUM: u128 = 20;
const ADOPTION_THRESHOLD_DEN: u128 = 100;

/// 建议动作常量（D12 的三个合法 action 之二；动作枚举本体按 kernel 约定留给
/// 批4 引入，本批以 DDL 存储串表达）。
/// Suggested-action constants (two of D12's three legal actions; the action
/// enum itself is deferred to batch 4 per the kernel convention, so this batch
/// speaks in DDL storage strings).
pub const ACTION_SUPPLEMENTAL_COMPILE: &str = "supplemental_compile";
pub const ACTION_QUERY_TEMPLATE: &str = "query_template";

/// 判定信号名（reason_json 的 `signal` 字段值）。
/// Signal names (the `signal` field inside reason_json).
const SIGNAL_ZERO_RECALL: &str = "zero_recall";
const SIGNAL_REWRITE_FAILURE: &str = "rewrite_failure";
const SIGNAL_LOW_QUALITY: &str = "low_quality";

/// 采纳率是否低于阈值：`adopted/count < 20/100` 用整数交叉相乘精确判定
/// （`100*adopted < 20*count`），边界 adopted/count == 1/5 恒不触发（A11）。
/// Whether the adoption rate is below the threshold: `adopted/count < 20/100`
/// decided by exact integer cross-multiplication (`100*adopted < 20*count`);
/// the boundary adopted/count == 1/5 never triggers (A11).
fn adoption_below_threshold(adopted: usize, count: usize) -> bool {
    (adopted as u128) * ADOPTION_THRESHOLD_DEN < (count as u128) * ADOPTION_THRESHOLD_NUM
}

/// 分析窗口（spec §6：domain + 窗口边界 + load_window 的两组快照）。
/// The analysis window (spec §6: domain + window bounds + the two load_window
/// snapshot groups).
#[derive(Debug, Clone)]
pub struct FeedbackWindow {
    pub domain: String,
    pub from: i64,
    pub to: i64,
    pub logs: Vec<QueryLogSnapshot>,
    pub events: Vec<FeedbackEvent>,
}

/// 分析器契约（spec §6 原文签名；实现为纯函数的薄壳）。
/// The analyzer contract (verbatim spec §6 signature; the impl is a thin shell
/// over the pure function).
#[async_trait::async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    async fn analyze(&self, input: FeedbackWindow) -> Result<FeedbackReport>;
}

/// 标准分析器：确定性规则实现（D11）。无状态之外仅携带低质量判据的
/// `min_events`（批6 CLI `--min-events` 的注入口子）；默认值 = 固定常量
/// [`MIN_EVENTS`]，采纳率阈值恒为 [`ADOPTION_THRESHOLD`]（D11：MVP 固定，
/// 本结构不开放改写）。
/// The standard analyzer: the deterministic D11 rule set. Beyond statelessness
/// it carries only the low-quality rule's `min_events` (the injection point for
/// the batch-6 CLI `--min-events`); the default equals the fixed constant
/// [`MIN_EVENTS`], and the adoption-rate threshold stays fixed at
/// [`ADOPTION_THRESHOLD`] (D11: MVP-fixed, not open for rewriting here).
#[derive(Debug, Clone, Copy)]
pub struct StandardFeedbackAnalyzer {
    min_events: usize,
}

impl Default for StandardFeedbackAnalyzer {
    fn default() -> Self {
        Self {
            min_events: MIN_EVENTS,
        }
    }
}

impl StandardFeedbackAnalyzer {
    /// 以自定义最小事件数构造（CLI `--min-events`；调用方负责范围校验，此处
    /// 仅兜底 `>= 1`，非法值在 [`analyze_window_with`] 内 fail-closed）。
    /// Builds with a custom minimum-event count (the CLI `--min-events`; the
    /// caller owns range validation, here only `>= 1` is backstopped and an
    /// illegal value fails closed inside [`analyze_window_with`]).
    pub fn with_min_events(min_events: usize) -> Self {
        Self { min_events }
    }
}

#[async_trait::async_trait]
impl FeedbackAnalyzer for StandardFeedbackAnalyzer {
    async fn analyze(&self, input: FeedbackWindow) -> Result<FeedbackReport> {
        analyze_window_with(input, self.min_events)
    }
}

/// (domain, normalized query_text) 去重聚合的中间形状：log_ids 保序收集后统一
/// 升序，latency 求和后取平均（确定性 f64 除法）。
/// Intermediate shape of the (domain, normalized query_text) dedup aggregation:
/// log_ids are collected in arrival order then sorted ascending; latency is
/// summed then averaged (deterministic f64 division).
#[derive(Debug, Default)]
struct QueryAgg {
    log_ids: Vec<i64>,
    latency_sum: i64,
}

impl QueryAgg {
    fn push(&mut self, log_id: i64, latency_ms: i64) {
        self.log_ids.push(log_id);
        self.latency_sum += latency_ms;
    }

    fn occurrences(&self) -> usize {
        self.log_ids.len()
    }

    fn avg_latency_ms(&self) -> f64 {
        if self.log_ids.is_empty() {
            return 0.0;
        }
        self.latency_sum as f64 / self.log_ids.len() as f64
    }
}

/// 低质量页面聚合的中间形状（log_ids 用 BTreeSet 保证升序去重）。
/// Intermediate shape of the low-quality page aggregation (a BTreeSet keeps
/// log_ids ascending and deduplicated).
#[derive(Debug, Default)]
struct PageAgg {
    feedback_count: usize,
    adopted_count: usize,
    log_ids: std::collections::BTreeSet<i64>,
}

/// 三信号聚合（spec §6 / D11）。报告数组按归一化键/page_id 字节序排序（确定
/// 性输出序），报告项超上限报错不截断（A13）。`min_events` 取 D11 固定默认
/// [`MIN_EVENTS`]；自定义值请走 [`analyze_window_with`]。
/// The three-signal aggregation (spec §6 / D11). Report arrays are sorted by
/// the normalized key / page_id byte order (deterministic output order); report
/// items over the cap error out instead of truncating (A13). `min_events` takes
/// the D11 fixed default [`MIN_EVENTS`]; for a custom value use
/// [`analyze_window_with`].
pub fn analyze_window(window: FeedbackWindow) -> Result<FeedbackReport> {
    analyze_window_with(window, MIN_EVENTS)
}

/// [`analyze_window`] 的参数化形态（批6 CLI `--min-events`）：低质量判据与报告
/// 阈值使用传入的 `min_events`；`min_events = 0` 会让「最小样本」失去意义 →
/// Validation（fail-closed，绝不静默放宽 D11）。
/// The parameterized form of [`analyze_window`] (batch-6 CLI `--min-events`):
/// the low-quality criterion and the report threshold use the given
/// `min_events`; `min_events = 0` voids the "minimum sample" rule → Validation
/// (fail-closed, never silently relaxing D11).
pub fn analyze_window_with(window: FeedbackWindow, min_events: usize) -> Result<FeedbackReport> {
    if min_events == 0 {
        return Err(Error::Validation(
            "feedback analysis min_events must be >= 1".into(),
        ));
    }
    if window.from > window.to {
        return Err(Error::Validation(format!(
            "feedback analysis window requires from <= to (got {} > {})",
            window.from, window.to
        )));
    }

    // —— 信号一/二：零召回与改写失败（日志级判据，按归一化查询文本去重）。
    //    两类独立判定：rewrite_failure=1 且 hit_count=0 的日志两类各记一次
    //    （D11 三信号互相独立，"rewrite_failure=1 独立出现"，A10）。
    // —— Signals 1/2: zero recall and rewrite failures (log-level criteria,
    //    deduped by normalized query text). The two classes are judged
    //    independently: a log with rewrite_failure=1 and hit_count=0 is recorded
    //    in both (D11's three signals are mutually independent — "rewrite_
    //    failure=1 appears independently", A10).
    let mut zero_by_query: std::collections::BTreeMap<String, QueryAgg> =
        std::collections::BTreeMap::new();
    let mut rewrite_by_query: std::collections::BTreeMap<String, QueryAgg> =
        std::collections::BTreeMap::new();
    for log in &window.logs {
        if log.hit_count == 0 {
            // D11 判据原文：滤空（candidate_empty_initial=1）且放宽未成功
            // （relaxation_succeeded=0）≠ 盲区，排除；其余零命中全部入选。
            // The verbatim D11 predicate: filter-empty (candidate_empty_initial=1)
            // with an unsuccessful relaxation (relaxation_succeeded=0) is NOT a
            // blind spot and is excluded; every other zero-hit log qualifies.
            let relaxed_and_still_empty = log.candidate_empty_initial && !log.relaxation_succeeded;
            if !relaxed_and_still_empty {
                let key = normalized_log_query(log)?;
                zero_by_query
                    .entry(key)
                    .or_default()
                    .push(log.log_id, log.latency_ms);
            }
        }
        if log.rewrite_failure {
            let key = normalized_log_query(log)?;
            rewrite_by_query
                .entry(key)
                .or_default()
                .push(log.log_id, log.latency_ms);
        }
    }

    // —— 信号三：低质量页面（事件级判据）。rate 事件可无 page_id（DDL 允许），
    //    无法归属页面 → 不参与页面聚合（仍计入输入行数；不静默丢弃判据之外
    //    的事实，只是它本来就不属于任何 page 桶）。
    // —— Signal 3: low-quality pages (event-level criterion). A rate event may
    //    lack page_id (the DDL allows it) and cannot be attributed to a page →
    //    it stays out of the page aggregation (still counted in the input row
    //    counts; nothing is silently dropped — it simply belongs to no page
    //    bucket in the first place).
    let mut pages: std::collections::BTreeMap<String, PageAgg> = std::collections::BTreeMap::new();
    for event in &window.events {
        let Some(page_id) = event.page_id.as_deref() else {
            continue;
        };
        let agg = pages.entry(page_id.to_string()).or_default();
        agg.feedback_count += 1;
        agg.log_ids.insert(event.log_id);
        match event.kind {
            // adopt 计 1；rate≥4 计 1；click 只进分母（D11）。
            // adopt scores 1; rate>=4 scores 1; click feeds the denominator
            // only (D11).
            FeedbackKind::Adopt => agg.adopted_count += 1,
            FeedbackKind::Rate => {
                if event.rating.is_some_and(|r| r >= 4) {
                    agg.adopted_count += 1;
                }
            }
            FeedbackKind::Click => {}
        }
    }

    // 报告数组：BTreeMap 迭代即按键字节序（确定性）；超上限报错不截断（A13，
    // 复用 spec §9 上限精神；1000 与 review list 上限同量级）。
    // Report arrays: iterating a BTreeMap is key-byte-ordered (deterministic);
    // over the cap → error, never truncation (A13, reusing the spec §9 cap
    // spirit; 1000 is the same magnitude as the review-list cap).
    let zero_recall =
        finish_query_class(&zero_by_query, REPORT_MAX_ITEMS_PER_CLASS, "zero_recall")?;
    let rewrite_failures = finish_query_class(
        &rewrite_by_query,
        REPORT_MAX_ITEMS_PER_CLASS,
        "rewrite_failures",
    )?;
    let low_quality: Vec<LowQualityPage> = pages
        .iter()
        .filter(|(_, agg)| {
            agg.feedback_count >= min_events
                && adoption_below_threshold(agg.adopted_count, agg.feedback_count)
        })
        .map(|(page_id, agg)| LowQualityPage {
            page_id: page_id.clone(),
            feedback_count: agg.feedback_count,
            adopted_count: agg.adopted_count,
            adoption_rate: agg.adopted_count as f64 / agg.feedback_count as f64,
            // 触发该页面判据的事件来源 log（升序去重，trace 用）。
            // Source logs of the events that triggered this page (ascending,
            // deduplicated, for trace).
            log_ids: agg.log_ids.iter().copied().collect(),
        })
        .collect();
    if low_quality.len() > REPORT_MAX_ITEMS_PER_CLASS {
        return Err(Error::Validation(format!(
            "low_quality report items exceed the hard cap {REPORT_MAX_ITEMS_PER_CLASS} \
             (got {}); refusing to truncate",
            low_quality.len()
        )));
    }

    // —— 建议（D12 合法 action；approve 时的 subject 字段闸门是人工审核的
    //    检查点，分析器不猜造 entity/source 任务载荷）：
    //    零召回 → supplemental_compile（缺知识补编译）；
    //    改写失败 → query_template（查询模板）；
    //    低质量 → supplemental_compile（按 page_id 的页面质量证据）。
    // —— Suggestions (legal D12 actions; the approve-time subject-field gate is
    //    the human checkpoint — the analyzer never invents entity/source task
    //    payloads): zero recall → supplemental_compile (missing knowledge);
    //    rewrite failure → query_template; low quality → supplemental_compile
    //    (per-page quality evidence keyed by page_id).
    let mut suggested_reviews = Vec::new();
    for item in &zero_recall {
        suggested_reviews.push(query_suggestion(
            &window.domain,
            ACTION_SUPPLEMENTAL_COMPILE,
            SIGNAL_ZERO_RECALL,
            item,
        ));
    }
    for item in &rewrite_failures {
        suggested_reviews.push(query_suggestion(
            &window.domain,
            ACTION_QUERY_TEMPLATE,
            SIGNAL_REWRITE_FAILURE,
            item,
        ));
    }
    for page in &low_quality {
        suggested_reviews.push(page_suggestion(&window.domain, page));
    }

    let report = FeedbackReport {
        schema_version: REPORT_SCHEMA_VERSION.to_string(),
        domain: window.domain,
        from: window.from,
        to: window.to,
        thresholds: FeedbackThresholds {
            min_events: min_events as u32,
            adoption_threshold: ADOPTION_THRESHOLD,
        },
        counts: FeedbackCounts {
            input_log_rows: window.logs.len(),
            input_event_rows: window.events.len(),
            zero_recall_queries: zero_recall.len(),
            rewrite_failure_queries: rewrite_failures.len(),
            low_quality_pages: low_quality.len(),
            suggested_reviews: suggested_reviews.len(),
        },
        zero_recall,
        rewrite_failures,
        low_quality,
        suggested_reviews,
        report_hash: String::new(),
    };
    report.finalize_hash()
}

/// 日志查询文本归一化（复用 query_engine 的 normalize 契约）；违反契约的行
/// fail-closed：整体 Validation，带 log_id 可审计，绝不静默跳过。
/// Normalizes a log's query text (reusing the query_engine normalize contract);
/// a row violating the contract fails closed: a whole-analysis Validation with
/// the log_id for auditability, never a silent skip.
fn normalized_log_query(log: &QueryLogSnapshot) -> Result<String> {
    normalize(&log.query_text).map_err(|e| {
        Error::Validation(format!(
            "query log {} query_text violates the normalization contract: {e}",
            log.log_id
        ))
    })
}

/// 查询类（零召回/改写失败）去重聚合 → 报告项（log_ids 升序 + 次数 + 平均
/// latency），并执行报告项硬上限检查。
/// Query-class (zero recall / rewrite failure) dedup aggregation → report items
/// (ascending log_ids + occurrences + average latency), enforcing the report
/// hard cap.
fn finish_query_class(
    by_query: &std::collections::BTreeMap<String, QueryAgg>,
    cap: usize,
    class: &str,
) -> Result<Vec<BlindSpotQuery>> {
    if by_query.len() > cap {
        return Err(Error::Validation(format!(
            "{class} report items exceed the hard cap {cap} (got {}); refusing to truncate",
            by_query.len()
        )));
    }
    Ok(by_query
        .iter()
        .map(|(normalized_query, agg)| {
            let mut log_ids = agg.log_ids.clone();
            log_ids.sort_unstable();
            BlindSpotQuery {
                normalized_query: normalized_query.clone(),
                occurrences: agg.occurrences(),
                avg_latency_ms: agg.avg_latency_ms(),
                log_ids,
            }
        })
        .collect())
}

/// 查询类建议的确定性 subject/reason（serde_json 对象键为字节序，构造固定 →
/// 序列化逐字节确定）。
/// Deterministic subject/reason for query-class suggestions (serde_json object
/// keys are byte-ordered and construction is fixed → byte-stable output).
fn query_suggestion(
    domain: &str,
    action: &str,
    signal: &str,
    item: &BlindSpotQuery,
) -> ReviewSuggestion {
    ReviewSuggestion {
        domain: domain.to_string(),
        action: action.to_string(),
        source_log_ids: item.log_ids.clone(),
        subject_json: serde_json::json!({
            "domain": domain,
            "normalized_query": item.normalized_query,
            "log_ids": item.log_ids,
            "occurrences": item.occurrences,
            "avg_latency_ms": item.avg_latency_ms,
        })
        .to_string(),
        reason_json: serde_json::json!({ "signal": signal }).to_string(),
    }
}

/// 低质量页面建议的确定性 subject/reason（spec §6：按 page_id 记录
/// feedback_count/adopted_count/adoption_rate；source_log_ids 带事件的来源
/// query log，供审核 trace）。
/// Deterministic subject/reason for a low-quality page suggestion (spec §6:
/// feedback_count/adopted_count/adoption_rate recorded per page_id;
/// source_log_ids carries the events' source query logs for review trace).
fn page_suggestion(domain: &str, page: &LowQualityPage) -> ReviewSuggestion {
    ReviewSuggestion {
        domain: domain.to_string(),
        action: ACTION_SUPPLEMENTAL_COMPILE.to_string(),
        source_log_ids: page.log_ids.clone(),
        subject_json: serde_json::json!({
            "page_id": page.page_id,
            "feedback_count": page.feedback_count,
            "adopted_count": page.adopted_count,
            "adoption_rate": page.adoption_rate,
        })
        .to_string(),
        reason_json: serde_json::json!({
            "signal": SIGNAL_LOW_QUALITY,
            "feedback_count": page.feedback_count,
            "adopted_count": page.adopted_count,
            "adoption_rate": page.adoption_rate,
        })
        .to_string(),
    }
}
