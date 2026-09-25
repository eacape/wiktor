//! 服务端指标（spec `step6-feedback-loop.md` §7.2、§10 A17）：原子计数器 +
//! 最小 Prometheus text 渲染。固定六个指标名、固定 label 集合，**禁止用户
//! 输入作 label**（不出现 domain、query 文本、key 原文/标签）。
//! Server metrics (spec `step6-feedback-loop.md` §7.2, §10 A17): atomic
//! counters plus a minimal Prometheus text rendering. Six fixed metric names
//! and a fixed label set; **user input is never used as a label** (no domain,
//! query text, or raw/derived key material appears).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// 拒绝原因（rejected_total 的固定 label 值集合；全部为服务端常量字符串）。
/// Rejection reasons (the fixed label-value set of rejected_total; all are
/// server-side constant strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionReason {
    Unauthenticated,
    TenantForbidden,
    InvalidJson,
    PayloadTooLarge,
    EventCountTooLarge,
    FieldTooLarge,
    InvalidFeedback,
}

impl RejectionReason {
    /// Prometheus label 值（与 spec §7.1 错误码语义一一对应）。
    /// The Prometheus label value (one-to-one with the spec §7.1 error-code
    /// semantics).
    pub fn as_label(self) -> &'static str {
        match self {
            RejectionReason::Unauthenticated => "unauthenticated",
            RejectionReason::TenantForbidden => "tenant_forbidden",
            RejectionReason::InvalidJson => "invalid_json",
            RejectionReason::PayloadTooLarge => "payload_too_large",
            RejectionReason::EventCountTooLarge => "event_count_too_large",
            RejectionReason::FieldTooLarge => "field_too_large",
            RejectionReason::InvalidFeedback => "invalid_feedback",
        }
    }
}

/// 固定原因集合的拒绝计数器（label 集合封闭：新原因必须加字段 + 渲染分支）。
/// Rejection counters over the fixed reason set (the label set is closed: a new
/// reason requires a new field plus a render branch).
#[derive(Debug, Default)]
pub struct RejectionCounters {
    unauthenticated: AtomicU64,
    tenant_forbidden: AtomicU64,
    invalid_json: AtomicU64,
    payload_too_large: AtomicU64,
    event_count_too_large: AtomicU64,
    field_too_large: AtomicU64,
    invalid_feedback: AtomicU64,
}

impl RejectionCounters {
    fn record(&self, reason: RejectionReason) {
        let cell = match reason {
            RejectionReason::Unauthenticated => &self.unauthenticated,
            RejectionReason::TenantForbidden => &self.tenant_forbidden,
            RejectionReason::InvalidJson => &self.invalid_json,
            RejectionReason::PayloadTooLarge => &self.payload_too_large,
            RejectionReason::EventCountTooLarge => &self.event_count_too_large,
            RejectionReason::FieldTooLarge => &self.field_too_large,
            RejectionReason::InvalidFeedback => &self.invalid_feedback,
        };
        cell.fetch_add(1, Ordering::Relaxed);
    }

    fn pairs(&self) -> [(&'static str, u64); 7] {
        [
            (
                RejectionReason::Unauthenticated.as_label(),
                self.unauthenticated.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::TenantForbidden.as_label(),
                self.tenant_forbidden.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::InvalidJson.as_label(),
                self.invalid_json.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::PayloadTooLarge.as_label(),
                self.payload_too_large.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::EventCountTooLarge.as_label(),
                self.event_count_too_large.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::FieldTooLarge.as_label(),
                self.field_too_large.load(Ordering::Relaxed),
            ),
            (
                RejectionReason::InvalidFeedback.as_label(),
                self.invalid_feedback.load(Ordering::Relaxed),
            ),
        ]
    }
}

/// 进程内指标（§7.2 六个指标名；review_pending 由 /metrics 端点每次抓取时
/// 从只读查询取值传入，本结构不缓存 DB 状态）。STEP11 B4 增补：查询延迟直方图
/// 与编译任务状态计数（保持手写 Prometheus 文本；无用户输入作标签，贴 A17）。
/// The in-process metrics (the six §7.2 metric names; review_pending is fetched
/// per scrape by the /metrics endpoint from a read-only query and passed in —
/// this struct does not cache DB state). STEP11 B4 adds a query-latency
/// histogram and compile-task-state counts (hand-rolled Prometheus text; no user
/// input as a label, per A17).
#[derive(Debug, Default)]
pub struct Metrics {
    ingested: AtomicU64,
    replayed: AtomicU64,
    rate_limited: AtomicU64,
    store_errors: AtomicU64,
    rejected: RejectionCounters,
    // STEP11 B4：查询计数与延迟直方图（固定桶边界，无动态标签）。
    query_total: AtomicU64,
    query_latency_buckets: [AtomicU64; QUERY_BUCKET_COUNT],
    // STEP11 B4：编译任务状态计数（固定状态集）。
    compile: CompileStatusCounters,
}

/// 查询延迟直方图桶数量（`+Inf` 越界也落到最后一桶）。
/// The number of query-latency histogram buckets (`+Inf` overflows land in the
/// last bucket).
const QUERY_BUCKET_COUNT: usize = 7;

/// 查询延迟直方图固定桶边界（ms）：`[1,5,10,25,50,100]`，最后一桶 `+Inf`。
/// Fixed query-latency histogram bucket bounds (ms): `[1,5,10,25,50,100]`, the
/// last bucket being `+Inf`.
const QUERY_LATENCY_BOUNDS_MS: [u64; 6] = [1, 5, 10, 25, 50, 100];

/// 编译任务状态计数（固定状态集，与 kernel 状态机一致）。
/// Compile-task-state counts (a fixed state set matching the kernel state
/// machine).
#[derive(Debug, Default)]
struct CompileStatusCounters {
    pending: AtomicU64,
    running: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
    dead: AtomicU64,
}

impl Metrics {
    /// 新事件成功写入（replayed=false 的每事件 +1）。
    /// One newly written event (per event with replayed=false).
    pub fn add_ingested(&self, n: u64) {
        self.ingested.fetch_add(n, Ordering::Relaxed);
    }

    /// 幂等回放事件（replayed=true 的每事件 +1，D5）。
    /// One idempotently replayed event (per event with replayed=true, D5).
    pub fn add_replayed(&self, n: u64) {
        self.replayed.fetch_add(n, Ordering::Relaxed);
    }

    /// 固定窗口限流命中（429，每请求 +1）。
    /// One fixed-window limiter hit (429, per request).
    pub fn record_rate_limited(&self) {
        self.rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    /// 存储事务失败（500 STORE_ERROR / 审计行写失败，每请求 +1）。
    /// One store-transaction failure (500 STORE_ERROR / a failed audit-row
    /// write, per request).
    pub fn record_store_error(&self) {
        self.store_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// 请求拒绝（按固定 reason；413 的三类同时写
    /// `feedback_rejections` 审计行，见 handler）。
    /// One rejected request (by a fixed reason; the three 413 kinds also write
    /// a `feedback_rejections` audit row, see the handler).
    pub fn record_rejected(&self, reason: RejectionReason) {
        self.rejected.record(reason);
    }

    /// STEP11 B4：记录一次查询（+1）并累进其延迟到固定桶。
    /// STEP11 B4: records one query (+1) and buckets its latency.
    pub fn record_query_latency_ms(&self, ms: u64) {
        self.query_total.fetch_add(1, Ordering::Relaxed);
        let mut i = 0;
        while i < QUERY_LATENCY_BOUNDS_MS.len() && ms > QUERY_LATENCY_BOUNDS_MS[i] {
            i += 1;
        }
        let bucket = i.min(QUERY_BUCKET_COUNT - 1);
        self.query_latency_buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// STEP11 B4：记录一个编译任务到达某状态（未知状态归入 pending，防动态
    /// label）。
    /// STEP11 B4: records a compile task reaching a state (unknown states fall
    /// into pending, preventing dynamic labels).
    pub fn record_compile_status(&self, status: &str) {
        let c = &self.compile;
        match status {
            "running" => c.running.fetch_add(1, Ordering::Relaxed),
            "succeeded" => c.succeeded.fetch_add(1, Ordering::Relaxed),
            "failed" => c.failed.fetch_add(1, Ordering::Relaxed),
            "dead" => c.dead.fetch_add(1, Ordering::Relaxed),
            _ => c.pending.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// 渲染 Prometheus text（§7.2 固定六名；`review_pending` 由调用方传入的
    /// 只读计数提供）。只读、无锁争用点（原子 Relaxed 读）。
    /// Renders the Prometheus text (the fixed six §7.2 names; `review_pending`
    /// comes from the caller's read-only count). Read-only, no lock contention
    /// points (relaxed atomic reads).
    pub fn render(&self, review_pending: i64) -> String {
        let mut out = String::with_capacity(1024);
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_ingested_total Total feedback events newly ingested (non-replayed)."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_ingested_total counter");
        let _ = writeln!(
            out,
            "wiktor_feedback_ingested_total {}",
            self.ingested.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_replayed_total Total feedback events replayed from idempotency."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_replayed_total counter");
        let _ = writeln!(
            out,
            "wiktor_feedback_replayed_total {}",
            self.replayed.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_rejected_total Total requests rejected, by fixed reason."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_rejected_total counter");
        for (label, value) in self.rejected.pairs() {
            let _ = writeln!(
                out,
                "wiktor_feedback_rejected_total{{reason=\"{label}\"}} {value}"
            );
        }
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_rate_limited_total Total requests rejected by the fixed-window limiter."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_rate_limited_total counter");
        let _ = writeln!(
            out,
            "wiktor_feedback_rate_limited_total {}",
            self.rate_limited.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_store_errors_total Total feedback store transaction failures."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_store_errors_total counter");
        let _ = writeln!(
            out,
            "wiktor_feedback_store_errors_total {}",
            self.store_errors.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP wiktor_feedback_review_pending Review-queue items currently pending."
        );
        let _ = writeln!(out, "# TYPE wiktor_feedback_review_pending gauge");
        let _ = writeln!(out, "wiktor_feedback_review_pending {review_pending}");

        // STEP11 B4：查询延迟直方图（固定桶，无用户输入标签）。
        let _ = writeln!(
            out,
            "# HELP wiktor_query_latency_ms_bucket Query latency in ms (fixed buckets)."
        );
        let _ = writeln!(out, "# TYPE wiktor_query_latency_ms_bucket histogram");
        let _ = writeln!(
            out,
            "wiktor_query_latency_ms_bucket_count {}",
            self.query_total.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "wiktor_query_latency_ms_bucket_sum {}",
            self.query_latency_ms_sum()
        );
        let mut cum = 0u64;
        for (i, bound) in QUERY_LATENCY_BOUNDS_MS.iter().enumerate() {
            cum += self.query_latency_buckets[i].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "wiktor_query_latency_ms_bucket{{le=\"{bound}\"}} {cum}"
            );
        }
        cum += self.query_latency_buckets[QUERY_BUCKET_COUNT - 1].load(Ordering::Relaxed);
        let _ = writeln!(out, "wiktor_query_latency_ms_bucket{{le=\"+Inf\"}} {cum}");

        // STEP11 B4：编译任务状态计数（固定状态集）。
        let _ = writeln!(
            out,
            "# HELP wiktor_compile_status_total Compile tasks by terminal/active state."
        );
        let _ = writeln!(out, "# TYPE wiktor_compile_status_total counter");
        for (label, cell) in [
            ("pending", self.compile.pending.load(Ordering::Relaxed)),
            ("running", self.compile.running.load(Ordering::Relaxed)),
            ("succeeded", self.compile.succeeded.load(Ordering::Relaxed)),
            ("failed", self.compile.failed.load(Ordering::Relaxed)),
            ("dead", self.compile.dead.load(Ordering::Relaxed)),
        ] {
            let _ = writeln!(
                out,
                "wiktor_compile_status_total{{state=\"{label}\"}} {cell}"
            );
        }
        out
    }

    /// 累计查询延迟（粗略，按桶中位数近似；供 histogram `_sum` 占比参考）。
    /// Cumulative query latency (approximate by bucket midpoint; a histogram
    /// `_sum` for reference).
    fn query_latency_ms_sum(&self) -> u64 {
        let bounds = [0.5, 3.0, 7.5, 17.5, 37.5, 75.0, 150.0];
        self.query_latency_buckets
            .iter()
            .enumerate()
            .map(|(i, b)| (bounds[i.min(6)] as u64) * b.load(Ordering::Relaxed))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // §7.2：六个指标名固定出现；rejected 按固定 reason 展开为多行。
    // §7.2: the six metric names appear verbatim; rejected expands to one line
    // per fixed reason.
    #[test]
    fn renders_fixed_metric_names_and_reason_labels() {
        let metrics = Metrics::default();
        metrics.add_ingested(3);
        metrics.add_replayed(2);
        metrics.record_rejected(RejectionReason::Unauthenticated);
        metrics.record_rejected(RejectionReason::PayloadTooLarge);
        metrics.record_rate_limited();
        metrics.record_store_error();
        // STEP11 B4：查询延迟分桶 + 编译状态计数。
        metrics.record_query_latency_ms(3);
        metrics.record_query_latency_ms(30);
        metrics.record_compile_status("running");
        metrics.record_compile_status("succeeded");
        let text = metrics.render(7);
        for name in [
            "wiktor_feedback_ingested_total",
            "wiktor_feedback_replayed_total",
            "wiktor_feedback_rejected_total",
            "wiktor_feedback_rate_limited_total",
            "wiktor_feedback_store_errors_total",
            "wiktor_feedback_review_pending",
            "wiktor_query_latency_ms_bucket",
            "wiktor_compile_status_total",
        ] {
            assert!(text.contains(name), "missing {name} in:\n{text}");
        }
        assert!(text.contains("wiktor_feedback_ingested_total 3"));
        assert!(text.contains("wiktor_feedback_rejected_total{reason=\"payload_too_large\"} 1"));
        assert!(text.contains("wiktor_feedback_review_pending 7"));
        assert!(text.contains("wiktor_query_latency_ms_bucket{le=\"5\"} 1"));
        assert!(text.contains("wiktor_compile_status_total{state=\"running\"} 1"));
        // 用户可控值永不进入输出（A17）。
        // User-controlled values never enter the output (A17).
        assert!(!text.contains("milk-tea"));
    }
}
