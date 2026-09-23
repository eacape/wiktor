//! `wiktor feedback analyze|list|review`（Step 6 spec `step6-feedback-loop.md`
//! §3 D11/D12、§8 CLI 契约与退出码、§10 A13/A14/A15/A16、§11 批6）。
//!
//! 流程（§8）：
//! - `analyze`：用法校验（from ≤ to、跨度 ≤ 31 天、min-events 1..=1000；越界 →
//!   退出码 2，先于一切 IO/DB）→ 打开 DB（迁移 → 3、数据库故障 → 1）→
//!   `FeedbackStore::load_window` → [`StandardFeedbackAnalyzer`]（min_events 取
//!   `--min-events`（默认 = 分析器 D11 固定常量 5），adoption_threshold 固定
//!   0.20）→ `insert_review_suggestions`（UNIQUE 冲突跳过，重复分析幂等，A12）
//!   → `write_report_files` 到 `--out-dir`（默认 ./feedback-reports；写盘失败 →
//!   1）→ 人类摘要 / `--json` 单份报告 JSON（stdout 只一份 JSON）；
//! - `list`：limit 1..=1000（越界 → 2）→ 表格或 `--json` 数组；
//! - `review approve/ignore`：透传 kernel 批4 转换 —— 非 pending / subject 缺
//!   字段 / 协议错误 → Validation → 3；存储错误 → 1。
//!
//! `wiktor feedback analyze|list|review` (Step 6 spec `step6-feedback-loop.md`
//! §3 D11/D12, §8 CLI contract and exit codes, §10 A13/A14/A15/A16, §11 batch 6).
//!
//! Flow (§8):
//! - `analyze`: usage validation (from <= to, span <= 31 days, min-events
//!   1..=1000; out of range → exit 2, before any IO/DB) → open the DB
//!   (migration → 3, database faults → 1) → `FeedbackStore::load_window` →
//!   [`StandardFeedbackAnalyzer`] (min_events from `--min-events`, default =
//!   the analyzer's fixed D11 constant 5; adoption_threshold stays fixed at
//!   0.20) → `insert_review_suggestions` (UNIQUE conflicts skipped, repeated
//!   analysis idempotent, A12) → `write_report_files` under `--out-dir`
//!   (default ./feedback-reports; write failure → 1) → human summary / `--json`
//!   single report JSON (exactly one JSON on stdout, logs to stderr);
//! - `list`: limit 1..=1000 (out of range → 2) → table or `--json` array;
//! - `review approve/ignore`: pass-through to the kernel's batch-4 transitions —
//!   non-pending / missing subject fields / protocol errors → Validation → 3;
//!   store errors → 1.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use wiktor_core::kernel::feedback_store::ReviewStatus;
use wiktor_core::kernel::SqliteKernel;
use wiktor_feedback::report::{render_json, write_report_files};
use wiktor_feedback::{
    FeedbackAnalyzer, FeedbackStore, FeedbackWindow, ReviewItem, StandardFeedbackAnalyzer,
};

use super::{report_error, EXIT_OK};

/// 窗口最大跨度：31 天（§8；越界 = CLI 用法错误 → 退出码 2）。
/// Maximum window span: 31 days (§8; out of range = a CLI usage error → exit 2).
const WINDOW_MAX_SPAN_SECS: i64 = 31 * 24 * 60 * 60;

/// 窗口默认长度：24 小时（§8：默认 `--from now-24h --to now`）。
/// Default window length: 24 hours (§8: defaults to `--from now-24h --to now`).
const WINDOW_DEFAULT_SECS: i64 = 24 * 60 * 60;

/// `--limit` 上限（§8：1..=1000；与 kernel `MAX_REVIEW_LIMIT` 同值）。
/// The `--limit` cap (§8: 1..=1000; same value as the kernel `MAX_REVIEW_LIMIT`).
const LIMIT_MAX: u32 = 1000;

/// `--min-events` 上限（与报告项硬上限 REPORT_MAX_ITEMS_PER_CLASS 同量级；
/// 下限 1：0 会让 D11「最小样本」判据失效）。
/// The `--min-events` cap (same magnitude as the report hard cap
/// REPORT_MAX_ITEMS_PER_CLASS; the floor is 1 — 0 would void D11's "minimum
/// sample" rule).
const MIN_EVENTS_MAX: u32 = 1000;

/// `--min-events` 默认值：单源引用分析器的 D11 固定常量（不复制字面量）。
/// The `--min-events` default: a single-source reference to the analyzer's
/// fixed D11 constant (never a copied literal).
const MIN_EVENTS_DEFAULT: u32 = wiktor_feedback::analyzer::MIN_EVENTS as u32;

/// `--out-dir` 默认目录（任务口径：./feedback-reports）。
/// The default `--out-dir` (task convention: ./feedback-reports).
const DEFAULT_OUT_DIR: &str = "feedback-reports";

/// `wiktor feedback` 子命令（§8 CLI 契约）。
/// The `wiktor feedback` subcommands (the §8 CLI contract).
#[derive(Debug, Subcommand)]
pub enum FeedbackCommand {
    /// Analyze the query/feedback window and write the blind-spot report trio
    /// (suggestions go to the review queue only).
    /// 分析查询/反馈窗口并写出盲区报告三件套（建议只进审核队列）。
    Analyze(AnalyzeArgs),
    /// List review-queue items for a domain (optionally by status).
    /// 列出某 domain 的审核队列项（可按状态过滤）。
    List(ListArgs),
    /// Human review transitions (approve / ignore).
    /// 人工审核转换（approve / ignore）。
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Approve a pending review item (supplemental_compile queues a compile
    /// task; query_template is an audit-only approval).
    /// 批准 pending 审核项（supplemental_compile 排队编译任务；
    /// query_template 仅审计批准）。
    Approve(ApproveArgs),
    /// Ignore a pending review item (audit-only transition).
    /// 忽略 pending 审核项（纯审计转换）。
    Ignore(IgnoreArgs),
}

/// `wiktor feedback analyze` 参数（§8 参数表）。
/// `wiktor feedback analyze` arguments (the §8 parameter table).
#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Domain (tenant) name to analyze (mandatory)
    /// 要分析的 domain（租户）名（必填）
    #[arg(long)]
    pub domain: String,
    /// Window start, unix seconds (default: now-24h)
    /// 窗口起点，unix 秒（默认 now-24h）
    #[arg(long)]
    pub from: Option<i64>,
    /// Window end, unix seconds (default: now)
    /// 窗口终点，unix 秒（默认 now）
    #[arg(long)]
    pub to: Option<i64>,
    /// Output directory for the report trio (default ./feedback-reports)
    /// 三件套报告输出目录（默认 ./feedback-reports）
    #[arg(long)]
    pub out_dir: Option<PathBuf>,
    /// Low-quality minimum feedback events per page (default 5; range 1..=1000)
    /// 低质量判据的每页最小反馈事件数（默认 5；范围 1..=1000）
    #[arg(long, default_value_t = MIN_EVENTS_DEFAULT)]
    pub min_events: u32,
    /// Emit the report JSON (single object) on stdout; logs go to stderr
    /// stdout 输出报告 JSON（单对象）；日志走 stderr
    #[arg(long)]
    pub json: bool,
}

/// `wiktor feedback list` 参数（§8 参数表）。
/// `wiktor feedback list` arguments (the §8 parameter table).
#[derive(Debug, Args)]
pub struct ListArgs {
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Domain (tenant) name (mandatory)
    /// domain（租户）名（必填）
    #[arg(long)]
    pub domain: String,
    /// Filter by review status
    /// 按审核状态过滤
    #[arg(long)]
    pub status: Option<CliReviewStatus>,
    /// Max rows to return (default 1000; range 1..=1000)
    /// 最多返回行数（默认 1000；范围 1..=1000）
    #[arg(long, default_value_t = LIMIT_MAX)]
    pub limit: u32,
    /// Emit a JSON array instead of the human-readable table
    /// 输出 JSON 数组而非人类可读表格
    #[arg(long)]
    pub json: bool,
}

/// `wiktor feedback review approve` 参数（§8 参数表）。
/// `wiktor feedback review approve` arguments (the §8 parameter table).
#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Review-queue row id (mandatory)
    /// 审核队列行 id（必填）
    #[arg(long)]
    pub review_id: i64,
    /// Operator name recorded in the audit columns (mandatory)
    /// 记入审计字段的操作人名（必填）
    #[arg(long)]
    pub by: String,
}

/// `wiktor feedback review ignore` 参数（§8 参数表）。
/// `wiktor feedback review ignore` arguments (the §8 parameter table).
#[derive(Debug, Args)]
pub struct IgnoreArgs {
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Review-queue row id (mandatory)
    /// 审核队列行 id（必填）
    #[arg(long)]
    pub review_id: i64,
    /// Operator name recorded in the audit columns (mandatory)
    /// 记入审计字段的操作人名（必填）
    #[arg(long)]
    pub by: String,
}

/// CLI 侧状态枚举（clap ValueEnum；`ReviewStatus` 定义在 core，孤儿规则不允许
/// 异地实现 trait，故此处镜像四个合法值后显式映射）。
/// The CLI-side status enum (clap ValueEnum; `ReviewStatus` lives in core and
/// the orphan rule forbids implementing a foreign trait there, so the four
/// legal values are mirrored and mapped explicitly).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum CliReviewStatus {
    Pending,
    Approved,
    Ignored,
    Failed,
}

impl From<CliReviewStatus> for ReviewStatus {
    fn from(value: CliReviewStatus) -> Self {
        match value {
            CliReviewStatus::Pending => ReviewStatus::Pending,
            CliReviewStatus::Approved => ReviewStatus::Approved,
            CliReviewStatus::Ignored => ReviewStatus::Ignored,
            CliReviewStatus::Failed => ReviewStatus::Failed,
        }
    }
}

/// 命令入口：返回进程退出码（main 据此 `std::process::exit`）。
/// Command entry: returns the process exit code (main calls
/// `std::process::exit` with it).
pub async fn run(command: FeedbackCommand) -> Result<i32> {
    match command {
        FeedbackCommand::Analyze(args) => run_analyze(args).await,
        FeedbackCommand::List(args) => run_list(args),
        FeedbackCommand::Review { command } => match command {
            ReviewCommand::Approve(args) => run_approve(args),
            ReviewCommand::Ignore(args) => run_ignore(args),
        },
    }
}

/// 当前 unix 秒。系统时钟早于 1970 属环境级病理：退化为 0 起点（默认窗口
/// `[-86400, 0]` 仍是合法 from ≤ to），绝不 panic。
/// The current unix second. A pre-1970 system clock is a pathological
/// environment: degrade to a 0 epoch (the default window `[-86400, 0]` stays a
/// legal from <= to), never panic.
fn unix_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

/// 用法校验：窗口 from ≤ to 且跨度 ≤ 31 天（§8；越界 = CLI 用法错误 → 2）。
/// 缺省侧用系统时间补齐（from → now-24h，to → now）。
/// Usage validation: from <= to with a span of at most 31 days (§8; out of
/// range = a CLI usage error → 2). Missing sides are filled from the system
/// clock (from → now-24h, to → now).
fn validate_window(
    from: Option<i64>,
    to: Option<i64>,
    now: i64,
) -> std::result::Result<(i64, i64), String> {
    let to = to.unwrap_or(now);
    let from = from.unwrap_or(now - WINDOW_DEFAULT_SECS);
    if from > to {
        return Err(format!(
            "--from {from} must be <= --to {to} (the window is the inclusive range [from, to])"
        ));
    }
    if to - from > WINDOW_MAX_SPAN_SECS {
        return Err(format!(
            "window span {}s exceeds the maximum of {WINDOW_MAX_SPAN_SECS}s (31 days)",
            to - from
        ));
    }
    Ok((from, to))
}

/// 用法校验：--min-events 1..=[`MIN_EVENTS_MAX`]（越界 → 2）。
/// Usage validation: --min-events within 1..=[`MIN_EVENTS_MAX`] (out of range → 2).
fn validate_min_events(n: u32) -> std::result::Result<u32, String> {
    if (1..=MIN_EVENTS_MAX).contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "--min-events {n} out of range 1..={MIN_EVENTS_MAX}"
        ))
    }
}

/// 用法校验：--limit 1..=[`LIMIT_MAX`]（§8；越界 → 2，先于一切 IO/DB）。
/// Usage validation: --limit within 1..=[`LIMIT_MAX`] (§8; out of range → 2,
/// before any IO/DB).
fn validate_limit(n: u32) -> std::result::Result<u32, String> {
    if (1..=LIMIT_MAX).contains(&n) {
        Ok(n)
    } else {
        Err(format!("--limit {n} out of range 1..={LIMIT_MAX}"))
    }
}

/// `feedback analyze`：见模块文档流程。
/// `feedback analyze`: see the module-doc flow.
async fn run_analyze(args: AnalyzeArgs) -> Result<i32> {
    let command = "feedback analyze";

    // —— 用法阶段：窗口/min-events 越界 → 退出码 2（先于一切 IO/DB）——
    // —— Usage phase: out-of-range window / min-events → exit 2 (before any
    //       IO/DB) ——
    let (from, to) = match validate_window(args.from, args.to, unix_now()) {
        Ok(w) => w,
        Err(msg) => {
            eprintln!("wiktor {command}: {msg}");
            return Ok(super::EXIT_USAGE);
        }
    };
    let min_events = match validate_min_events(args.min_events) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("wiktor {command}: {msg}");
            return Ok(super::EXIT_USAGE);
        }
    };

    // —— 打开 DB（会迁移；迁移 → 3、数据库故障 → 1，分类器裁决）——
    // —— Open the DB (with migration; migration → 3, database faults → 1, the
    //       classifier decides) ——
    let kernel = match SqliteKernel::open(&args.db) {
        Ok(k) => k,
        Err(e) => return Ok(report_error(command, e)),
    };
    let store: &dyn FeedbackStore = &kernel;

    // —— 窗口读取（D11 分析输入；数据库故障 → 1）——
    // —— Window read (the D11 analysis input; database faults → 1) ——
    let (logs, events) = match store.load_window(&args.domain, from, to) {
        Ok(x) => x,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 分析（纯函数；归一化契约违反/超报告上限 → Validation → 3）——
    // —— Analyze (pure function; normalization-contract violations / over-cap
    //       reports → Validation → 3) ——
    let analyzer = StandardFeedbackAnalyzer::with_min_events(min_events as usize);
    let window = FeedbackWindow {
        domain: args.domain.clone(),
        from,
        to,
        logs,
        events,
    };
    let report = match analyzer.analyze(window).await {
        Ok(r) => r,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 建议入队（UNIQUE 冲突跳过 → 重复分析幂等，A12；序列化失败 → 4）——
    // —— Enqueue suggestions (UNIQUE conflicts skipped → repeated analysis is
    //       idempotent, A12; serialization failure → 4) ——
    let created_at = unix_now();
    let inputs = match report
        .suggested_reviews
        .iter()
        .map(|s| s.to_review_suggestion_input(created_at))
        .collect::<wiktor_core::types::error::Result<Vec<_>>>()
    {
        Ok(i) => i,
        Err(e) => return Ok(report_error(command, e)),
    };
    let new_ids = match store.insert_review_suggestions(&args.domain, &inputs) {
        Ok(ids) => ids,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 报告三件套落盘（写盘失败 = 运行失败 → 1，§8）——
    // —— Write the report trio (a write failure is a run failure → 1, §8) ——
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_OUT_DIR));
    let paths = match write_report_files(&out_dir, &report) {
        Ok(p) => p,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 输出：--json → stdout 单份报告 JSON（日志走 stderr）；否则人类摘要 ——
    // —— Output: --json → exactly one report JSON on stdout (logs to stderr);
    //       otherwise the human summary ——
    if args.json {
        let json = match render_json(&report) {
            Ok(j) => j,
            Err(e) => return Ok(report_error(command, e)),
        };
        println!("{json}");
        return Ok(EXIT_OK);
    }
    print_analyze_summary(&report, new_ids.len(), &paths);
    Ok(EXIT_OK)
}

/// analyze 人类摘要：窗口/输入行数/三类计数/建议（含本轮新入队数）/报告路径。
/// The analyze human summary: window / input rows / per-class counts /
/// suggestions (including how many were newly enqueued this run) / report paths.
fn print_analyze_summary(
    report: &wiktor_feedback::FeedbackReport,
    new_suggestions: usize,
    paths: &[String; 3],
) {
    println!(
        "domain: {}  window: {} .. {}",
        report.domain, report.from, report.to
    );
    println!(
        "input rows: query_logs={}, feedback_events={}",
        report.counts.input_log_rows, report.counts.input_event_rows
    );
    println!(
        "zero_recall: {}  rewrite_failures: {}  low_quality: {}",
        report.counts.zero_recall_queries,
        report.counts.rewrite_failure_queries,
        report.counts.low_quality_pages
    );
    println!(
        "suggested_reviews: total={} new={}",
        report.counts.suggested_reviews, new_suggestions
    );
    println!("report_hash: {}", report.report_hash);
    println!("reports:");
    for path in paths {
        println!("  {path}");
    }
}

/// `feedback list`：见模块文档流程。
/// `feedback list`: see the module-doc flow.
fn run_list(args: ListArgs) -> Result<i32> {
    let command = "feedback list";

    // —— 用法阶段：limit 越界 → 退出码 2（先于一切 IO/DB）——
    // —— Usage phase: an out-of-range limit → exit 2 (before any IO/DB) ——
    let limit = match validate_limit(args.limit) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("wiktor {command}: {msg}");
            return Ok(super::EXIT_USAGE);
        }
    };

    let kernel = match SqliteKernel::open(&args.db) {
        Ok(k) => k,
        Err(e) => return Ok(report_error(command, e)),
    };
    let store: &dyn FeedbackStore = &kernel;
    let items = match store.list_reviews(&args.domain, args.status.map(Into::into), limit) {
        Ok(i) => i,
        Err(e) => return Ok(report_error(command, e)),
    };

    if args.json {
        let json = match render_reviews_json(&items) {
            Ok(j) => j,
            Err(e) => return Ok(report_error(command, e)),
        };
        println!("{json}");
        return Ok(EXIT_OK);
    }
    print_review_table(&items);
    Ok(EXIT_OK)
}

/// list 的 JSON 数组（整行 `ReviewItem` 契约字段；status 为 snake_case 串）。
/// The list JSON array (the full `ReviewItem` contract fields; status is a
/// snake_case string).
fn render_reviews_json(items: &[ReviewItem]) -> wiktor_core::types::error::Result<String> {
    Ok(serde_json::to_string_pretty(items)?)
}

/// list 人类表格：review_id/action/status/created_at/subject 摘要/
/// compile_task_id（`-` = 无）。
/// The list human table: review_id/action/status/created_at/subject summary/
/// compile_task_id (`-` = none).
fn print_review_table(items: &[ReviewItem]) {
    if items.is_empty() {
        println!("no review items");
        return;
    }
    println!("review_id | action | status | created_at | subject | compile_task_id");
    for item in items {
        println!(
            "{:<10} {:<20} {:<9} {:<12} {:<32} {}",
            item.review_id,
            item.action,
            item.status.as_str(),
            item.created_at,
            subject_summary(&item.subject_json, 32),
            item.compile_task_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
}

/// subject 摘要：优先 `normalized_query` / `page_id` 键（分析器两类建议的
/// 确定性主键），否则截断原始 JSON；从不 panic。
/// The subject summary: prefers the `normalized_query` / `page_id` keys (the
/// deterministic primary keys of the analyzer's two suggestion shapes), else
/// truncates the raw JSON; never panics.
fn subject_summary(subject_json: &str, max_chars: usize) -> String {
    let raw = truncate_chars(subject_json, max_chars);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(subject_json) else {
        return raw;
    };
    let key = value
        .get("normalized_query")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("page_id").and_then(|v| v.as_str()));
    match key {
        Some(s) => truncate_chars(s, max_chars),
        None => raw,
    }
}

/// 按 Unicode scalar 截断（超长补省略号；摘要仅展示用，不参与任何判定）。
/// Truncates by Unicode scalar (an ellipsis marks the cut; the summary is
/// display-only and never feeds any decision).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// `feedback review approve`：见模块文档流程（错误分类：非 pending / subject
/// 缺字段 / 协议错误 → Validation → 3；存储 → 1；未分类 → 4）。
/// `feedback review approve`: see the module-doc flow (error classification:
/// non-pending / missing subject fields / protocol errors → Validation → 3;
/// store faults → 1; unclassified → 4).
fn run_approve(args: ApproveArgs) -> Result<i32> {
    let command = "feedback review approve";
    let kernel = match SqliteKernel::open(&args.db) {
        Ok(k) => k,
        Err(e) => return Ok(report_error(command, e)),
    };
    let store: &dyn FeedbackStore = &kernel;
    let outcome = match store.approve_review(args.review_id, &args.by, unix_now()) {
        Ok(o) => o,
        Err(e) => return Ok(report_error(command, e)),
    };
    println!("review_id: {}", outcome.review_id);
    println!("status: {}", outcome.status.as_str());
    match outcome.compile_task_id {
        Some(task_id) => println!("compile_task_id: {task_id}"),
        // query_template 的审计批准：无 compile task，注明语义（A16）。
        // The audit-only query_template approval: no compile task, the
        // semantics spelled out (A16).
        None => println!("compile_task_id: - (audit-only approval; no compile task created)"),
    }
    Ok(EXIT_OK)
}

/// `feedback review ignore`：纯审计转换（A16）。
/// `feedback review ignore`: a pure audit transition (A16).
fn run_ignore(args: IgnoreArgs) -> Result<i32> {
    let command = "feedback review ignore";
    let kernel = match SqliteKernel::open(&args.db) {
        Ok(k) => k,
        Err(e) => return Ok(report_error(command, e)),
    };
    let store: &dyn FeedbackStore = &kernel;
    if let Err(e) = store.ignore_review(args.review_id, &args.by, unix_now()) {
        return Ok(report_error(command, e));
    }
    println!("review_id: {}", args.review_id);
    println!("status: ignored");
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiktor_core::compile::config::CompilePolicy;
    use wiktor_core::kernel::feedback_store::{
        FeedbackEventInput, FeedbackKind, ReviewSuggestionInput,
    };
    use wiktor_core::kernel::QueryLogInsert;
    use wiktor_core::traits::{EntitySchema, FieldDefinition, FieldType};
    use wiktor_core::types::{CompileContext, EntityId, RawEntity};
    use wiktor_feedback::report::{REPORT_FILE_EN, REPORT_FILE_JSON, REPORT_FILE_ZH};

    const DOMAIN: &str = "milk-tea";

    fn base_db(dir: &std::path::Path) -> PathBuf {
        dir.join("wiktor.db")
    }

    fn analyze_args(
        db: &std::path::Path,
        out_dir: &std::path::Path,
        from: i64,
        to: i64,
    ) -> AnalyzeArgs {
        AnalyzeArgs {
            db: db.to_path_buf(),
            domain: DOMAIN.into(),
            from: Some(from),
            to: Some(to),
            out_dir: Some(out_dir.to_path_buf()),
            min_events: MIN_EVENTS_DEFAULT,
            json: false,
        }
    }

    /// 插入一条 query_logs 行（kernel 批2 契约 API，返回 log_id；timestamp 由
    /// kernel 取当前时钟，因此窗口必须围绕真实 now 构造）。
    /// Inserts one query_logs row (the kernel's batch-2 contract API returning
    /// the log_id; the timestamp comes from the kernel's clock, so the analysis
    /// window must be built around the real now).
    fn insert_log(
        kernel: &SqliteKernel,
        query_text: &str,
        hits: i64,
        rewrite_failure: bool,
        empty_initial: bool,
        relax_attempted: bool,
        relax_succeeded: bool,
    ) -> i64 {
        kernel
            .insert_query_log(&QueryLogInsert {
                query_text,
                query_json: "{}",
                rewritten_json: None,
                rewrite_failure,
                hit_count: hits,
                latency_ms: 12,
                domain: DOMAIN,
                candidate_empty_initial: empty_initial,
                relaxation_attempted: relax_attempted,
                relaxation_succeeded: relax_succeeded,
            })
            .unwrap()
    }

    /// 插入一条反馈事件（走 FeedbackStore 契约面；received_at = now 保证落在
    /// 测试窗口内）。
    /// Inserts one feedback event (through the FeedbackStore contract face;
    /// received_at = now keeps it inside the test window).
    fn insert_event(
        kernel: &SqliteKernel,
        key: &str,
        log_id: i64,
        kind: FeedbackKind,
        page: &str,
        now: i64,
    ) {
        let input = FeedbackEventInput {
            idempotency_key: key.into(),
            domain: DOMAIN.into(),
            log_id,
            kind,
            page_id: Some(page.into()),
            rating: None,
            metadata: serde_json::json!({}),
        };
        kernel.insert_idempotent(&input, now).unwrap();
    }

    /// 直插一条审核建议（list/review 测试夹具；analyze 测试由命令自身入队）。
    /// Inserts one review suggestion directly (a list/review fixture; the
    /// analyze tests enqueue via the command itself).
    fn insert_suggestion(kernel: &SqliteKernel, action: &str, subject_json: &str) -> i64 {
        kernel
            .insert_review_suggestions(
                DOMAIN,
                &[ReviewSuggestionInput {
                    action: action.into(),
                    source_log_ids_json: "[1]".into(),
                    subject_json: subject_json.into(),
                    reason_json: r#"{"signal":"zero_recall"}"#.into(),
                    created_at: 1000,
                }],
            )
            .unwrap()[0]
    }

    // ===== 用法校验（先于一切 IO/DB，可直接测纯函数）=====
    // ===== Usage validation (before any IO/DB; the pure functions are tested
    //       directly) =====

    // 窗口：from > to 与跨度 > 31 天 = 用法错误 → 2；恰好 31 天合法。
    // Window: from > to and a span over 31 days are usage errors → 2; exactly
    // 31 days is legal.
    #[tokio::test]
    async fn window_violations_are_usage_errors() {
        assert_eq!(
            run(analyze_args_invalid_window(100, 50)).await.unwrap(),
            super::super::EXIT_USAGE
        );
        assert_eq!(
            run(analyze_args_invalid_window(0, WINDOW_MAX_SPAN_SECS + 1))
                .await
                .unwrap(),
            super::super::EXIT_USAGE
        );
        // 边界：恰好 31 天通过校验（校验函数纯测，不触 DB）。
        // Boundary: exactly 31 days passes (pure-function check, no DB touch).
        assert_eq!(
            validate_window(Some(0), Some(WINDOW_MAX_SPAN_SECS), 10_000).unwrap(),
            (0, WINDOW_MAX_SPAN_SECS)
        );
        // 缺省补齐：from → now-24h，to → now。
        // Defaults: from → now-24h, to → now.
        assert_eq!(
            validate_window(None, None, 1_000_000).unwrap(),
            (1_000_000 - WINDOW_DEFAULT_SECS, 1_000_000)
        );
    }

    /// 构造带指定窗口的 analyze 参数（db/out_dir 指向临时目录，校验先于 IO
    /// 时不会触达）。
    /// Builds analyze args with the given window (db/out_dir stay inside a temp
    /// dir and are never reached when validation fires first).
    fn analyze_args_invalid_window(from: i64, to: i64) -> FeedbackCommand {
        let dir = tempfile::tempdir().unwrap();
        FeedbackCommand::Analyze(AnalyzeArgs {
            db: base_db(dir.path()),
            domain: DOMAIN.into(),
            from: Some(from),
            to: Some(to),
            out_dir: Some(dir.path().join("reports")),
            min_events: MIN_EVENTS_DEFAULT,
            json: false,
        })
    }

    // limit 越界（0 / 1001）= 用法错误 → 2；边界 1 与 1000 合法。
    // An out-of-range limit (0 / 1001) is a usage error → 2; the boundaries 1
    // and 1000 are legal.
    #[tokio::test]
    async fn limit_violations_are_usage_errors() {
        for bad in [0u32, LIMIT_MAX + 1] {
            let dir = tempfile::tempdir().unwrap();
            let args = FeedbackCommand::List(ListArgs {
                db: base_db(dir.path()),
                domain: DOMAIN.into(),
                status: None,
                limit: bad,
                json: false,
            });
            assert_eq!(run(args).await.unwrap(), super::super::EXIT_USAGE);
        }
        assert_eq!(validate_limit(1).unwrap(), 1);
        assert_eq!(validate_limit(LIMIT_MAX).unwrap(), LIMIT_MAX);
    }

    // min-events 越界（0 / 1001）= 用法错误 → 2。
    // Out-of-range min-events (0 / 1001) is a usage error → 2.
    #[tokio::test]
    async fn min_events_violations_are_usage_errors() {
        for bad in [0u32, MIN_EVENTS_MAX + 1] {
            let dir = tempfile::tempdir().unwrap();
            let args = FeedbackCommand::Analyze(AnalyzeArgs {
                db: base_db(dir.path()),
                domain: DOMAIN.into(),
                from: Some(0),
                to: Some(1000),
                out_dir: Some(dir.path().join("reports")),
                min_events: bad,
                json: false,
            });
            assert_eq!(run(args).await.unwrap(), super::super::EXIT_USAGE);
        }
    }

    // ===== analyze =====
    // ===== analyze =====

    // A13/A14：空窗口 → exit 0、三件套落盘、JSON 契约字段全零、无建议。
    // A13/A14: an empty window → exit 0, the trio on disk, all-zero JSON
    // contract fields, no suggestions.
    #[tokio::test]
    async fn analyze_empty_window_exits_zero_with_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let out_dir = dir.path().join("reports");

        let args = FeedbackCommand::Analyze(analyze_args(&db, &out_dir, 0, 10_000));
        assert_eq!(run(args).await.unwrap(), EXIT_OK);

        let zh = out_dir.join(REPORT_FILE_ZH);
        let en = out_dir.join(REPORT_FILE_EN);
        let json = out_dir.join(REPORT_FILE_JSON);
        assert!(zh.is_file() && en.is_file() && json.is_file());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(value["domain"], DOMAIN);
        assert_eq!(value["from"], 0);
        assert_eq!(value["to"], 10_000);
        assert_eq!(value["thresholds"]["min_events"], 5);
        assert_eq!(value["thresholds"]["adoption_threshold"], 0.2);
        assert_eq!(value["counts"]["input_log_rows"], 0);
        assert_eq!(value["counts"]["input_event_rows"], 0);
        assert!(value["suggested_reviews"].as_array().unwrap().is_empty());
        assert_eq!(value["report_hash"].as_str().unwrap().len(), 64);
    }

    // A10/A11/A13/A12：造日志（含滤空标记组合）+ 事件 → 报告三分类正确、建议
    // 入队；重复 analyze 不重复插入（UNIQUE 幂等）。
    // A10/A11/A13/A12: logs (including the filter-empty flag combination) plus
    // events → the three report classes are correct and suggestions are
    // enqueued; a repeated analyze inserts nothing new (UNIQUE idempotency).
    #[tokio::test]
    async fn analyze_flags_blind_spots_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let out_dir = dir.path().join("reports");

        // —— 灌库：4 条日志 + 10 条事件；窗口围绕真实 now（kernel 落库时钟）。——
        // —— Seed: 4 logs + 10 events; the window surrounds the real now (the
        //       kernel's write clock). ——
        let now = unix_now();
        let kernel = SqliteKernel::open(&db).unwrap();
        // log 1：普通零命中 → zero_recall。
        // log 1: a plain zero hit → zero_recall.
        insert_log(&kernel, "boba milk tea", 0, false, false, false, false);
        // log 2：滤空且放宽仍失败 → 排除（≠ 盲区，D11/A10）。
        // log 2: filter-empty with a failed relaxation → excluded (not a blind
        //        spot, D11/A10).
        insert_log(&kernel, "taro milk tea", 0, false, true, true, false);
        // log 3：改写失败但有命中 → 仅 rewrite_failures。
        // log 3: a rewrite failure with hits → rewrite_failures only.
        insert_log(&kernel, "oolong latte", 2, true, false, false, false);
        // log 4：健康查询 → 不进任何类。
        // log 4: a healthy query → enters no class.
        insert_log(&kernel, "jasmine tea", 5, false, false, false, false);

        // page drink:boba：1 adopt + 4 click = 5 事件，采纳率恰 0.20 → 不触发
        // （A11 边界：等于不触发）。
        // page drink:boba: 1 adopt + 4 clicks = 5 events, rate exactly 0.20 →
        // no trigger (the A11 boundary: equality does not trigger).
        insert_event(&kernel, "k-1", 1, FeedbackKind::Adopt, "drink:boba", now);
        for k in 2..=5 {
            insert_event(
                &kernel,
                &format!("k-{k}"),
                1,
                FeedbackKind::Click,
                "drink:boba",
                now,
            );
        }
        // page drink:latte：5 click，采纳率 0 → 触发。
        // page drink:latte: 5 clicks, rate 0 → triggers.
        for k in 6..=10 {
            insert_event(
                &kernel,
                &format!("k-{k}"),
                3,
                FeedbackKind::Click,
                "drink:latte",
                now,
            );
        }
        drop(kernel);

        let args = FeedbackCommand::Analyze(analyze_args(&db, &out_dir, now - 60, now + 60));
        assert_eq!(run(args).await.unwrap(), EXIT_OK);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out_dir.join(REPORT_FILE_JSON)).unwrap())
                .unwrap();
        assert_eq!(value["counts"]["input_log_rows"], 4);
        assert_eq!(value["counts"]["input_event_rows"], 10);
        assert_eq!(value["counts"]["zero_recall_queries"], 1);
        assert_eq!(value["counts"]["rewrite_failure_queries"], 1);
        assert_eq!(value["counts"]["low_quality_pages"], 1);
        assert_eq!(value["counts"]["suggested_reviews"], 3);
        // 滤空日志（taro milk tea）绝不出现在报告中（A10）。
        // The filter-empty log (taro milk tea) never appears in the report (A10).
        assert!(
            !value.to_string().contains("taro milk tea"),
            "filter-empty-with-failed-relaxation must be excluded"
        );
        assert_eq!(value["zero_recall"][0]["normalized_query"], "boba milk tea");
        assert_eq!(
            value["rewrite_failures"][0]["normalized_query"],
            "oolong latte"
        );
        assert_eq!(value["low_quality"][0]["page_id"], "drink:latte");

        // 三条建议入队（supplemental_compile x2 + query_template x1）。
        // Three suggestions enqueued (supplemental_compile x2 + query_template x1).
        let kernel = SqliteKernel::open(&db).unwrap();
        let reviews = kernel.list_reviews(DOMAIN, None, LIMIT_MAX).unwrap();
        assert_eq!(reviews.len(), 3);
        assert_eq!(
            reviews
                .iter()
                .filter(|r| r.action == "supplemental_compile")
                .count(),
            2
        );
        assert_eq!(
            reviews
                .iter()
                .filter(|r| r.action == "query_template")
                .count(),
            1
        );

        // 重复 analyze：exit 0 且零新插入（A12 幂等）。
        // A repeated analyze: exit 0 with zero new inserts (A12 idempotency).
        let args = FeedbackCommand::Analyze(analyze_args(&db, &out_dir, now - 60, now + 60));
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let reviews = kernel.list_reviews(DOMAIN, None, LIMIT_MAX).unwrap();
        assert_eq!(reviews.len(), 3, "repeated analysis must not re-insert");
    }

    // --min-events 贯通：min_events=1 时 2 事件的页面（采纳率 0）触发低质量；
    // 默认 5 时不触发（阈值真正从 CLI 流入分析器与报告 thresholds）。
    // --min-events end-to-end: with min_events=1 a 2-event page (adoption 0)
    // triggers low quality; with the default 5 it does not (the threshold
    // genuinely flows from the CLI into the analyzer and the report's
    // thresholds).
    #[tokio::test]
    async fn analyze_min_events_flows_into_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let out_dir = dir.path().join("reports");
        let now = unix_now();

        // 健康日志（无零召回/改写失败）+ 一个 2 click 的页面。
        // A healthy log (no zero recall / rewrite failure) + one 2-click page.
        let kernel = SqliteKernel::open(&db).unwrap();
        insert_log(&kernel, "jasmine tea", 3, false, false, false, false);
        insert_event(&kernel, "t-1", 1, FeedbackKind::Click, "drink:tiny", now);
        insert_event(&kernel, "t-2", 1, FeedbackKind::Click, "drink:tiny", now);
        drop(kernel);

        // 默认 min_events=5：不触发，零建议。
        // Default min_events=5: no trigger, zero suggestions.
        let args = FeedbackCommand::Analyze(analyze_args(&db, &out_dir, now - 60, now + 60));
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out_dir.join(REPORT_FILE_JSON)).unwrap())
                .unwrap();
        assert_eq!(value["counts"]["low_quality_pages"], 0);
        assert_eq!(value["counts"]["suggested_reviews"], 0);

        // min_events=1：触发且报告阈值同步为 1，新建议入队。
        // min_events=1: triggers, the report threshold syncs to 1, the new
        // suggestion is enqueued.
        let mut lowered = analyze_args(&db, &out_dir, now - 60, now + 60);
        lowered.min_events = 1;
        assert_eq!(
            run(FeedbackCommand::Analyze(lowered)).await.unwrap(),
            EXIT_OK
        );
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out_dir.join(REPORT_FILE_JSON)).unwrap())
                .unwrap();
        assert_eq!(value["thresholds"]["min_events"], 1);
        assert_eq!(value["counts"]["low_quality_pages"], 1);
        assert_eq!(value["low_quality"][0]["page_id"], "drink:tiny");
        let kernel = SqliteKernel::open(&db).unwrap();
        assert_eq!(
            kernel.list_reviews(DOMAIN, None, LIMIT_MAX).unwrap().len(),
            1
        );
    }

    // A14：报告写盘失败（out_dir 是已存在文件）= 运行失败 → 1。
    // A14: a report-write failure (out_dir is an existing file) is a run
    // failure → 1.
    #[tokio::test]
    async fn analyze_report_write_failure_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let args = FeedbackCommand::Analyze(analyze_args(&db, &blocker, 0, 10_000));
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_RUN_FAILURE);
    }

    // ===== list =====
    // ===== list =====

    // A14：状态过滤与 --json 形状（整行契约字段 + snake_case status）。
    // A14: status filtering and the --json shape (full contract fields +
    // snake_case status).
    #[tokio::test]
    async fn list_filters_by_status_and_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let kernel = SqliteKernel::open(&db).unwrap();

        // 审计批准走 query_template（supplemental_compile 的 approve 会做
        // subject 五字段闸门，与本测试无关）；ignore 不校验 subject，故
        // supplemental 夹具可安全忽略。
        // The audit approval goes through query_template (approving a
        // supplemental_compile triggers the five-subject-field gate, irrelevant
        // here); ignore never validates the subject, so the supplemental fixture
        // is safely ignorable.
        let a = insert_suggestion(
            &kernel,
            "query_template",
            r#"{"normalized_query":"boba milk tea"}"#,
        );
        let b = insert_suggestion(
            &kernel,
            "supplemental_compile",
            r#"{"page_id":"drink:boba","feedback_count":5}"#,
        );
        kernel.approve_review(a, "alice", 2000).unwrap();
        kernel.ignore_review(b, "bob", 2000).unwrap();

        // pending：无 → 人类模式 no review items，exit 0（§8 空集是成功）。
        // pending: none → the human mode prints "no review items", exit 0 (an
        // empty set is a success per §8).
        let args = FeedbackCommand::List(ListArgs {
            db: db.clone(),
            domain: DOMAIN.into(),
            status: Some(CliReviewStatus::Pending),
            limit: LIMIT_MAX,
            json: false,
        });
        assert_eq!(run(args).await.unwrap(), EXIT_OK);

        // approved：恰 1 条（query_template 审计批准）。
        // approved: exactly one (the audit-approved query_template).
        let approved = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Approved), LIMIT_MAX)
            .unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].action, "query_template");

        // --json：数组形状（直接测渲染函数，stdout 捕获不属于单测范围）。
        // --json: the array shape (the render function is tested directly;
        // stdout capture is out of unit-test scope).
        let all = kernel.list_reviews(DOMAIN, None, LIMIT_MAX).unwrap();
        assert_eq!(all.len(), 2);
        let json = render_reviews_json(&all).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let statuses: Vec<&str> = arr.iter().map(|v| v["status"].as_str().unwrap()).collect();
        assert!(statuses.contains(&"approved") && statuses.contains(&"ignored"));
        assert!(arr.iter().all(|v| v["domain"] == DOMAIN));
        assert!(arr.iter().all(|v| v["review_id"].is_i64()));
        assert!(arr.iter().all(|v| v["subject_json"].is_string()));

        // 空 domain → 人类模式 no review items（exit 0）。
        // An empty domain → the human mode prints "no review items" (exit 0).
        let args = FeedbackCommand::List(ListArgs {
            db: db.clone(),
            domain: "no-such-domain".into(),
            status: None,
            limit: LIMIT_MAX,
            json: false,
        });
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
    }

    // ===== review approve / ignore =====
    // ===== review approve / ignore =====

    /// 四字段源 schema（与 kernel approve 测试同款夹具；raw 必须全覆盖声明字段）。
    /// The four-field source schema (the same fixture as the kernel approve
    /// tests; the raw entity must cover every declared field).
    fn schema() -> EntitySchema {
        EntitySchema {
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
                FieldDefinition {
                    name: "category".into(),
                    field_type: FieldType::Text,
                    filterable: true,
                },
                FieldDefinition {
                    name: "price".into(),
                    field_type: FieldType::Numeric,
                    filterable: true,
                },
            ],
        }
    }

    fn ctx() -> CompileContext {
        CompileContext {
            domain_pack_version: "0.1.0".into(),
            prompt_template: "SYSTEM source-ref-v1\nTEMPLATE".into(),
            model_version: "mock-v1".into(),
            embedding_model: "none".into(),
            quality_threshold: 0.75,
            require_source_refs: true,
        }
    }

    fn policy() -> CompilePolicy {
        CompilePolicy {
            knowledge_fields: vec!["name".into(), "description".into()],
            ..CompilePolicy::default()
        }
    }

    /// 合法 supplemental subject（与 kernel 夹具同形状：五必需字段 + 内部一致；
    /// dependencies_json = compile_tasks.dependencies_json 的 {context, policy,
    /// schema} 形状）。
    /// A legal supplemental subject (the kernel-fixture shape: the five
    /// required fields plus internal coherence; dependencies_json = the
    /// {context, policy, schema} shape of compile_tasks.dependencies_json).
    fn supplemental_subject() -> String {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        fields.insert(
            "category".to_string(),
            serde_json::json!("milk-tea:drink:boba"),
        );
        fields.insert("price".to_string(), serde_json::json!(19.0));
        let raw = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let deps = serde_json::json!({
            "context": ctx(),
            "policy": policy(),
            "schema": {
                "entity_type": "drink",
                "fields": schema()
                    .fields
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "name": f.name,
                            "field_type": f.field_type,
                            "filterable": f.filterable,
                        })
                    })
                    .collect::<Vec<_>>(),
            },
        });
        serde_json::json!({
            "entity_id": raw.id.to_key(),
            "source_revision": raw.source_revision,
            "domain_pack_version": ctx().domain_pack_version,
            "source_json": serde_json::to_string(&raw).unwrap(),
            "dependencies_json": deps.to_string(),
        })
        .to_string()
    }

    // A15：approve pending supplemental_compile 成功 → compile_tasks 排队 +
    // 回填 task_id；重复 approve → exit 3。
    // A15: approving a pending supplemental_compile queues a compile task and
    // backfills the task_id; a repeated approve → exit 3.
    #[tokio::test]
    async fn review_approve_supplemental_backfills_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let kernel = SqliteKernel::open(&db).unwrap();
        let review_id = insert_suggestion(&kernel, "supplemental_compile", &supplemental_subject());
        drop(kernel);

        let args = FeedbackCommand::Review {
            command: ReviewCommand::Approve(ApproveArgs {
                db: db.clone(),
                review_id,
                by: "alice".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), EXIT_OK);

        let kernel = SqliteKernel::open(&db).unwrap();
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 1);
        let approved = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Approved), LIMIT_MAX)
            .unwrap();
        assert_eq!(approved.len(), 1);
        assert!(approved[0].compile_task_id.is_some(), "task_id backfilled");

        // 重复 approve：非 pending → Validation → 3。
        // A repeated approve: non-pending → Validation → 3.
        let args = FeedbackCommand::Review {
            command: ReviewCommand::Approve(ApproveArgs {
                db: db.clone(),
                review_id,
                by: "alice".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_CONFIG);
    }

    // A15/spec §8：subject 缺字段 → 数据协议校验错误 → 3（不猜造任务）。
    // A15/spec §8: a subject with missing fields → a data-protocol validation
    // error → 3 (no fabricated tasks).
    #[tokio::test]
    async fn review_approve_missing_subject_fields_exits_three() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let kernel = SqliteKernel::open(&db).unwrap();
        let review_id = insert_suggestion(
            &kernel,
            "supplemental_compile",
            r#"{"signal":"zero_recall"}"#,
        );
        drop(kernel);

        let args = FeedbackCommand::Review {
            command: ReviewCommand::Approve(ApproveArgs {
                db,
                review_id,
                by: "alice".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_CONFIG);
    }

    // A16：query_template 审计批准成功（无 task、无 compile_tasks 行）；
    // ignore 纯审计转换成功；已审核项再 ignore/approve → 3。
    // A16: the query_template audit approval succeeds (no task, no
    // compile_tasks rows); ignore is a successful pure-audit transition; an
    // already-reviewed item rejects both → 3.
    #[tokio::test]
    async fn review_query_template_audit_and_ignore_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let db = base_db(dir.path());
        let kernel = SqliteKernel::open(&db).unwrap();
        let review_id = insert_suggestion(
            &kernel,
            "query_template",
            r#"{"normalized_query":"boba milk tea"}"#,
        );
        drop(kernel);

        let args = FeedbackCommand::Review {
            command: ReviewCommand::Approve(ApproveArgs {
                db: db.clone(),
                review_id,
                by: "ops".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 0);
        let approved = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Approved), LIMIT_MAX)
            .unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].compile_task_id, None);
        drop(kernel);

        // ignore 另一条 pending → status ignored（A16）。
        // ignore another pending row → status ignored (A16).
        let kernel = SqliteKernel::open(&db).unwrap();
        let other = insert_suggestion(
            &kernel,
            "query_template",
            r#"{"normalized_query":"oolong latte"}"#,
        );
        drop(kernel);
        let args = FeedbackCommand::Review {
            command: ReviewCommand::Ignore(IgnoreArgs {
                db: db.clone(),
                review_id: other,
                by: "bob".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        let ignored = kernel
            .list_reviews(DOMAIN, Some(ReviewStatus::Ignored), LIMIT_MAX)
            .unwrap();
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].reviewed_by.as_deref(), Some("bob"));
        drop(kernel);

        // 已审核（ignored）再 ignore → 3；approve 同样 → 3。
        // Re-ignoring an ignored row → 3; approving it → 3 as well.
        let args = FeedbackCommand::Review {
            command: ReviewCommand::Ignore(IgnoreArgs {
                db: db.clone(),
                review_id: other,
                by: "bob".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_CONFIG);
        let args = FeedbackCommand::Review {
            command: ReviewCommand::Approve(ApproveArgs {
                db,
                review_id: other,
                by: "alice".into(),
            }),
        };
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_CONFIG);
    }
}
