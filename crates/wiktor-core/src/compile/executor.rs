//! PipelineExecutor（Step 4 spec §3，决策 D2/D5/D7）：编排
//! `DataSource → prepare_source → admit → claim → Compiler → validate →
//! score → publish/failure` 的逐批执行器。
//! PipelineExecutor (Step 4 spec §3, decisions D2/D5/D7): the batch-wise
//! orchestrator for `DataSource → prepare_source → admit → claim → Compiler →
//! validate → score → publish/failure`.
//!
//! 编排契约要点：
//! - 同步 kernel 事务一律 [`tokio::task::spawn_blocking`] 包裹（§8.2：不把
//!   guard 返回异步层，不跨 await 持锁）。
//! - run_id = Unix 秒 + UUID v4；dry_run 为特殊值 `dry-run`。
//! - 每个 scanned 实体只计一次最终分类（A1 统计契约）：skipped（含
//!   duplicate_in_run）/ accepted / quarantined / failed / deferred；
//!   dry_run 为 scanned = skipped + would_compile + quarantined + failed。
//! - claim=None 且存在到期任务 → 预算熔断（§8.4）：circuit_open=true、剩余
//!   任务 deferred、本 run 停止调度；全部未到期 → 按退避短等待，累计不超过
//!   60 秒（§9），之后剩余 deferred，不忙等。
//! - attempts 按每次模型请求（含重编译）累计；reserved_tokens 复用 kernel
//!   claim 的同一保守预留公式（§8.4，禁止双实现漂移）；reported_tokens 由
//!   evidence.usage 累计。
//! - 空知识源 / 知识输入超 64 KiB → preflight quarantine，不 admit、不请求
//!   LLM、不占 token（§6/§8.3；偏差说明见 `run_real` 内注释）。
//! - Step8 §6.4/D11（批 B5）：首次 admission 前自动运行兼容 preflight（默认
//!   `StandardCompatibilityChecker`，与 domain check 同一入口）；不兼容时真实
//!   run 幂等插入 `compatibility_conflict` 审核后整 run 以配置错误结束——不改
//!   facts、不建任务（A15/A16；legacy 无矩阵直通，偏差 STEP8-028）。
//! - Compiler 的 quality/content_hash/metadata/sections 不可信（§4/§5.2.4/
//!   §5.2.5）：publish 前 executor 用 canonical renderer + 共享 splitter +
//!   content_hash() 重算并写回。
//!
//! Orchestration contract highlights:
//! - Synchronous kernel transactions are always wrapped in
//!   [`tokio::task::spawn_blocking`] (§8.2: never hand guards back to the async
//!   layer, never hold a lock across await).
//! - run_id = Unix seconds + UUID v4; dry_run uses the special value `dry-run`.
//! - Every scanned entity is classified exactly once (A1 stats contract):
//!   skipped (incl. duplicate_in_run) / accepted / quarantined / failed /
//!   deferred; dry_run holds scanned = skipped + would_compile + quarantined +
//!   failed.
//! - claim=None while a due task exists → budget circuit breaker (§8.4):
//!   circuit_open=true, remaining tasks deferred, scheduling stops for this
//!   run; when nothing is due → bounded backoff waits capped at 60s cumulative
//!   (§9), leftovers deferred, never busy-waiting.
//! - attempts accumulate per model request (including recompiles);
//!   reserved_tokens reuse the exact conservative estimator of the kernel claim
//!   (§8.4, no drifting duplicate); reported_tokens accumulate evidence.usage.
//! - Empty knowledge / knowledge input over 64 KiB → preflight quarantine:
//!   no admission, no LLM, no tokens (§6/§8.3; see the deviation note inside
//!   `run_real`).
//! - Step8 §6.4/D11 (batch B5): the compatibility preflight runs automatically
//!   before the first admission (the default `StandardCompatibilityChecker`,
//!   shared with the domain check through the same entry); on incompatibility a
//!   real run idempotently inserts a `compatibility_conflict` review and then
//!   ends as a config error — no fact writes, no task created (A15/A16; a
//!   missing legacy matrix passes through, deviation STEP8-028).
//! - The Compiler's quality/content_hash/metadata/sections are untrusted
//!   (§4/§5.2.4/§5.2.5): the executor recomputes them via the canonical
//!   renderer + shared splitter + content_hash() before publish.

use crate::compile::compatibility::{
    check_domain_compatibility, compatibility_conflict_payloads, CompatibilityChecker,
    StandardCompatibilityChecker, COMPATIBILITY_REJECTED_PREFIX,
};
use crate::compile::config::{
    prepare_source, Admission, Clock, CommitOutcome, CompilePolicy, CompileStats, DomainIdentity,
    FailureDisposition, RunOptions, TaskLease,
};
use crate::compile::consistency::{
    ConsistencyArbiter, ConsistencyCandidateProvider, ConsistencyReport,
};
use crate::compile::contract::{
    render_canonical_markdown, CompileFailure, RefReport, SourceRefValidator,
};
use crate::compile::hash::{content_hash, HashDependencies};
use crate::compile::lease::{
    lease_heartbeat_interval, HeartbeatHandle, LeaseHeartbeat, LeaseReaper,
};
use crate::compile::quality::{validate_score_finite, RuleScorer, ScoreReport};
use crate::kernel::compile_store::estimate_budget_units;
use crate::kernel::SqliteKernel;
use crate::query_engine::qug::QugGraph;
use crate::seed::split_sections;
use crate::traits::{Compiler, DataSource, EntitySchema};
use crate::types::error::{Error, Result};
use crate::types::{CompileContext, CompiledPage, Cursor, PageMetadata, QualityScore, RawEntity};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 退避累计等待上限（§9：executor 最多等待 60s，之后剩余 deferred，不忙等）。
/// Cumulative backoff-wait cap (§9: the executor waits at most 60s, leftovers
/// become deferred, never busy-waiting).
const MAX_BACKOFF_WAIT_SECONDS: u64 = 60;

/// 睡眠唤醒余量（毫秒）：`clock.unix_seconds()` 按秒取整，额外 50ms 保证唤醒
/// 后 now >= next_attempt_at，避免误判预算熔断。
/// Wake margin (ms): `clock.unix_seconds()` floors to seconds; the extra 50ms
/// guarantees now >= next_attempt_at after waking, avoiding a false
/// circuit-open.
const WAKE_MARGIN_MS: u64 = 50;

/// 每个知识输入的 canonical bytes 上限（§3.1：≤64 KiB，超限 preflight quarantine）。
/// Canonical-bytes cap per knowledge input (§3.1: ≤64 KiB; oversize goes to
/// preflight quarantine).
const MAX_KNOWLEDGE_INPUT_BYTES: usize = 64 * 1024;

/// qug_edges 有效性检查用的构图深度（§10：只做 from_edges 构造校验；取 §3.1
/// 领域默认深度 2，不代表领域实际 qug.max_depth 配置）。
/// Graph depth used for the qug_edges validity check (§10: a from_edges
/// construction check only; the §3.1 domain default depth 2, not the domain's
/// actual qug.max_depth setting).
const QUG_EDGE_VALIDATION_DEPTH: usize = 2;

/// PipelineExecutor（§3 契约字段；Step8 §4：一致性仲裁器与相关页提供器为可
/// 注入协作对象，`Option` 默认 `None` = 不仲裁，核心状态机不依赖具体实现）。
/// PipelineExecutor (§3 contract fields; Step8 §4: the consistency arbiter and
/// related-page provider are injectable collaborators, `Option` defaulting to
/// `None` = no arbitration; the core state machine never depends on concrete
/// implementations).
pub struct PipelineExecutor {
    pub kernel: Arc<SqliteKernel>,
    pub compiler: Arc<dyn Compiler>,
    pub scorer: Arc<dyn RuleScorer>,
    pub validator: Arc<dyn SourceRefValidator>,
    pub clock: Arc<dyn Clock>,
    pub policy: CompilePolicy,
    /// Step8 §4/D1：一致性仲裁器（`consistency.enabled` 且本字段与 provider 均
    /// 注入才参与调度，否则走 None 路径）。
    /// Step8 §4/D1: the consistency arbiter (arbitration participates only when
    /// `consistency.enabled` and both this and the provider are wired; otherwise
    /// the None path runs).
    pub consistency_arbiter: Option<Arc<dyn ConsistencyArbiter>>,
    /// Step8 §4/D2：有界相关页提供器（与 arbiter 成对注入）。
    /// Step8 §4/D2: the bounded related-page provider (wired in pairs with the
    /// arbiter).
    pub candidate_provider: Option<Arc<dyn ConsistencyCandidateProvider>>,
    /// Step8 §6.3/D8：可选周期租约回收器（`None` = 不启动，保持 Step4 行为——
    /// 仅 run 开头一次同步 recover）。经 [`Self::with_lease_reaper`] /
    /// [`Self::with_reaper_interval`] 接线。
    /// Step8 §6.3/D8: the optional periodic lease reaper (`None` = not started,
    /// keeping the Step4 behavior of a single run-start synchronous recover).
    /// Wired via [`Self::with_lease_reaper`] / [`Self::with_reaper_interval`].
    pub lease_reaper: Option<LeaseReaper>,
    /// Step8 §6.4/D11（批 B5）：兼容 preflight 检查器。默认
    /// [`StandardCompatibilityChecker`]（自动 preflight，D11），可用 builder 替换
    /// 同一 trait 的实现（与 domain check 共用同一入口，绝不旁路）。
    /// Step8 §6.4/D11 (batch B5): the compatibility-preflight checker. Defaults
    /// to [`StandardCompatibilityChecker`] (the automatic preflight, D11) and is
    /// replaceable via the builder with any implementation of the same trait —
    /// shared with the domain check through the same entry, never bypassed.
    pub compatibility_checker: Arc<dyn CompatibilityChecker>,
    /// 心跳间隔测试注入面（`pub(crate)`：A12 用小间隔 + 手动推进注入 Clock，
    /// 不依赖真实睡眠；生产路径恒 `None`，按 D9 用 `lease_seconds/2`）。
    /// The heartbeat-interval test injection surface (`pub(crate)`: A12 drives a
    /// small interval plus a manually advanced injected Clock with no real
    /// sleeping; production always leaves it `None`, using `lease_seconds/2` per
    /// D9).
    pub(crate) heartbeat_interval_override: Option<Duration>,
}

/// 单个租约的调度结果：终态（已计数）或退避重试（留在队列）。
/// Scheduling outcome of one lease: finalized (already counted) or retrying
/// with backoff (stays queued).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutcome {
    Finalized,
    RetryAt(i64),
}

/// Step7 B4 `admit_batch` 的结果：admission 统计 + 入队的 task_ids（worker 消费）。
/// The `admit_batch` outcome (Step7 B4): admission stats plus the queued
/// task_ids (consumed by the worker).
#[derive(Debug, Clone)]
pub struct AdmitOutcome {
    pub stats: CompileStats,
    pub task_ids: Vec<i64>,
}

impl PipelineExecutor {
    /// 组装执行器（字段与 §3 契约一致；Step8 注入面默认 `None`）。
    /// Assembles the executor (fields match the §3 contract; the Step8 injection
    /// surface defaults to `None`).
    pub fn new(
        kernel: Arc<SqliteKernel>,
        compiler: Arc<dyn Compiler>,
        scorer: Arc<dyn RuleScorer>,
        validator: Arc<dyn SourceRefValidator>,
        clock: Arc<dyn Clock>,
        policy: CompilePolicy,
    ) -> Self {
        Self {
            kernel,
            compiler,
            scorer,
            validator,
            clock,
            policy,
            consistency_arbiter: None,
            candidate_provider: None,
            lease_reaper: None,
            compatibility_checker: Arc::new(StandardCompatibilityChecker),
            heartbeat_interval_override: None,
        }
    }

    /// Step8 §4：注入一致性仲裁器（builder；与 [`Self::with_candidate_provider`]
    /// 成对使用，二者齐备且 `consistency.enabled` 才走仲裁路径）。
    /// Step8 §4: wires the consistency arbiter (builder; pair with
    /// [`Self::with_candidate_provider`] — arbitration engages only when both are
    /// present and `consistency.enabled`).
    pub fn with_consistency_arbiter(mut self, arbiter: Arc<dyn ConsistencyArbiter>) -> Self {
        self.consistency_arbiter = Some(arbiter);
        self
    }

    /// Step8 §4：注入有界相关页提供器（builder；上限仍由策略 top_k 与
    /// MAX_CONSISTENCY_TOP_K 钳制）。
    /// Step8 §4: wires the bounded related-page provider (builder; the cap is
    /// still clamped by the policy top_k and MAX_CONSISTENCY_TOP_K).
    pub fn with_candidate_provider(
        mut self,
        provider: Arc<dyn ConsistencyCandidateProvider>,
    ) -> Self {
        self.candidate_provider = Some(provider);
        self
    }

    /// Step8 §6.3/D8：接线周期租约回收器（builder；间隔取策略
    /// `lease_reaper_interval_seconds`，默认 30s）。run 期间 spawn，run 结束
    /// （含取消/错误路径）cancel + join——`run_until_cancelled` 在 cancel 后
    /// drain 一次再返回。CLI run 装配保持不接线（单次 recover 行为不变）。
    /// Step8 §6.3/D8: wires the periodic lease reaper (builder; the interval comes
    /// from the policy `lease_reaper_interval_seconds`, default 30s). Spawned for
    /// the duration of a run; cancel + join at run end (including cancel/error
    /// paths) — `run_until_cancelled` drains once after cancel before returning.
    /// The CLI run assembly stays unwired (the single-recover behavior is
    /// unchanged).
    pub fn with_lease_reaper(mut self) -> Self {
        let interval = Duration::from_secs(u64::from(self.policy.lease_reaper_interval_seconds));
        self.lease_reaper = Some(LeaseReaper::new(
            self.kernel.clone(),
            self.clock.clone(),
            interval,
        ));
        self
    }

    /// Step8 §6.3/D8：以显式间隔接线周期租约回收器（builder；A11 用小间隔 +
    /// 手动推进注入 Clock，不依赖真实睡眠）。
    /// Step8 §6.3/D8: wires the periodic lease reaper with an explicit interval
    /// (builder; A11 drives a small interval plus a manually advanced injected
    /// Clock with no real sleeping).
    pub fn with_reaper_interval(mut self, interval: Duration) -> Self {
        self.lease_reaper = Some(LeaseReaper::new(
            self.kernel.clone(),
            self.clock.clone(),
            interval,
        ));
        self
    }

    /// Step8 §6.4/D11（批 B5）：替换兼容 preflight 检查器（builder）。默认已装
    /// [`StandardCompatibilityChecker`]；替换实现必须继续走
    /// [`check_domain_compatibility`] 同一入口（「同一 preflight」契约）。
    /// Step8 §6.4/D11 (batch B5): swaps the compatibility-preflight checker
    /// (builder). [`StandardCompatibilityChecker`] is installed by default; any
    /// replacement must still flow through the single
    /// [`check_domain_compatibility`] entry (the "same preflight" contract).
    pub fn with_compatibility_checker(mut self, checker: Arc<dyn CompatibilityChecker>) -> Self {
        self.compatibility_checker = checker;
        self
    }

    /// Step8 §6.4/D11（批 B5）：首次 admission 前的兼容 preflight。流程：当前
    /// 身份五元组（domain 取首实体、版本取 ctx/policy）→
    /// [`check_domain_compatibility`]（与 domain check 同一 checker/入口）→
    /// `compatible=false` 时真实 run 在独立 BEGIN IMMEDIATE 内幂等插入
    /// `compatibility_conflict` 审核（A16；dry-run 完全只读，不写审核），随后
    /// 整 run 以带 [`COMPATIBILITY_REJECTED_PREFIX`] 的配置错误结束——不 admit、
    /// 不改 facts、不建任务（A15）。
    ///
    /// 偏差说明（§5.1，STEP8-028）：`policy.compatibility=None`（legacy 默认
    /// 策略）时 preflight 直通——§5.1「缺失矩阵 + 已有 Step4 数据的真实 compile
    /// 视为配置错误」不在本批 executor 强制，保持 Step4/6 行为与既有 338 测试
    /// 基线；留上层拍板（建议 domain check 显式报告缺失矩阵）。
    ///
    /// Step8 §6.4/D11 (batch B5): the compatibility preflight before the first
    /// admission. Flow: the current identity five-tuple (domain from the first
    /// entity, versions from ctx/policy) → [`check_domain_compatibility`] (the
    /// same checker/entry as the domain check) → on `compatible=false` a real run
    /// idempotently inserts a `compatibility_conflict` review inside one
    /// standalone BEGIN IMMEDIATE (A16; a dry-run stays fully read-only and
    /// writes no review), then the whole run ends with a config error carrying
    /// [`COMPATIBILITY_REJECTED_PREFIX`] — no admission, no fact writes, no task
    /// created (A15).
    ///
    /// Deviation note (§5.1, STEP8-028): with `policy.compatibility=None` (the
    /// legacy default policy) the preflight passes through — §5.1's "a real
    /// compile against existing Step 4 data without a matrix is a config error"
    /// is not enforced in the executor this batch, preserving Step4/6 behavior
    /// and the existing 338-test baseline; left for an upstream ruling (the
    /// domain check is the suggested place to report a missing matrix
    /// explicitly).
    async fn run_compatibility_preflight(
        &self,
        domain: &str,
        ctx: &CompileContext,
        dry_run: bool,
    ) -> Result<()> {
        if !self.policy.compatibility_preflight || self.policy.compatibility.is_none() {
            // 运行期开关关闭（CLI --skip-compatibility-check 的空库放行路径）或
            // legacy 无矩阵 → 直通（偏差 STEP8-028）。
            // The runtime toggle is off (the CLI --skip-compatibility-check
            // empty-DB path) or the legacy matrix is absent → pass through
            // (deviation STEP8-028).
            return Ok(());
        }
        // 当前身份五元组：CLI 在 domain.yaml 解析期已校验 strict semver；非 YAML
        // 直构调用方（测试夹具）版本非法时在此 fail-closed（A13）。
        // The current identity five-tuple: the CLI already validated strict
        // semver at domain.yaml parse time; direct non-YAML callers (test
        // fixtures) with an invalid version fail closed here (A13).
        let identity = DomainIdentity::parse(
            domain,
            &ctx.domain_pack_version,
            ctx.schema_version.as_deref(),
            ctx.prompt_version.as_deref(),
            &self.policy.artifact_version,
        )
        .map_err(|e| Error::InvalidConfig(format!("compatibility preflight: {e}")))?;
        let spec = self.policy.compatibility.clone();
        let checker = self.compatibility_checker.clone();
        let kernel = self.kernel.clone();
        let current = identity.clone();
        let report = blocking(move || {
            check_domain_compatibility(&kernel, checker.as_ref(), &current, spec.as_ref())
        })
        .await?;
        if report.compatible {
            return Ok(());
        }
        if !dry_run {
            // §6.4/§8/A16：兼容告警在独立 BEGIN IMMEDIATE 写事务内幂等插入
            // （UNIQUE(domain,action,subject_json) DO NOTHING；admission 被拒、
            // 无 task 可回填，compile_task_id 恒 NULL）。
            // §6.4/§8/A16: the compatibility alert is idempotently inserted in
            // one standalone BEGIN IMMEDIATE write transaction (UNIQUE
            // (domain,action,subject_json) DO NOTHING; admission was refused so
            // there is no task to backfill — compile_task_id stays NULL).
            let (subject_json, reason_json) = compatibility_conflict_payloads(&identity, &report)?;
            let kernel = self.kernel.clone();
            let review_domain = identity.domain.clone();
            let now = self.clock.unix_seconds();
            blocking(move || {
                kernel.insert_compatibility_conflict_review(
                    &review_domain,
                    &subject_json,
                    &reason_json,
                    now,
                )
            })
            .await?;
        }
        Err(Error::InvalidConfig(format!(
            "{COMPATIBILITY_REJECTED_PREFIX}: domain {domain:?} has {} violation(s) \
             across {} accepted page(s) and {} task(s); admission refused \
             (no facts/tasks written)",
            report.violations.len(),
            report.checked_pages,
            report.checked_tasks,
        )))
    }

    /// 一次 run：fetch → admission → 调度 → 发布/失败（§3 契约签名）。
    /// One run: fetch → admission → scheduling → publish/failure (§3 signature).
    pub async fn run(
        &self,
        source: &dyn DataSource,
        ctx: &CompileContext,
        options: RunOptions,
    ) -> Result<CompileStats> {
        self.policy.validate()?;
        options.validate()?;
        let schema = source.schema();
        let mut stats = CompileStats {
            run_id: if options.dry_run {
                // dry-run 特殊 run_id（§9：完全只读，无 run 行）。
                // Special dry-run run_id (§9: fully read-only, no run row).
                "dry-run".to_string()
            } else {
                format!(
                    "{}-{}",
                    self.clock.unix_seconds(),
                    uuid::Uuid::new_v4().simple()
                )
            },
            dry_run: options.dry_run,
            ..CompileStats::default()
        };
        if options.dry_run {
            self.run_dry(source, ctx, options, &schema, &mut stats)
                .await?;
        } else {
            self.run_real(source, ctx, options, &schema, &mut stats)
                .await?;
        }
        Ok(stats)
    }

    /// Step7 B4 窄接口（spec step7 §3 D5 / STEP7-002）：单个已 claim 租约的全
    /// 流程处理（compile → validate → score → publish/failure + 心跳/fencing +
    /// 预算/刹车/死信）。供 server `CompileWorker` 消费队列时复用 `run` 的
    /// 同一内部逻辑（`process_lease`），禁止在 server 复制发布/失败 SQL。
    /// `schema` 由调用方提供（worker 从 EntityConfig 构造，无需 DataSource）；
    /// stats 计数为 run 级，本接口内部使用临时统计、不暴露。
    /// Step7 B4 narrow interface (spec step7 §3 D5 / STEP7-002): the full
    /// processing of one already-claimed lease (compile → validate → score →
    /// publish/failure + heartbeat/fencing + budget/brake/dead-letter). Lets
    /// the server `CompileWorker` reuse the exact internal logic of `run`
    /// (`process_lease`) when consuming the queue — the server must never copy
    /// publish/failure SQL. The caller supplies `schema` (the worker builds it
    /// from an `EntityConfig`, without a `DataSource`); stats are run-level, so
    /// this interface uses a throwaway `CompileStats` internally.
    pub async fn process_claimed_task(
        &self,
        lease: crate::compile::config::TaskLease,
        schema: &crate::traits::EntitySchema,
    ) -> Result<LeaseOutcome> {
        let mut stats = CompileStats::default();
        self.process_lease(&lease, schema, &mut stats).await
    }

    /// Step7 B4 只 admission 入口（spec step7 §3 D4/D5）：扫描数据源、做兼容
    /// preflight 与投影，调用 `kernel.admit_compile` 把实体入队，**不 claim、
    /// 不调模型**。返回 admitted task_ids 供常驻 CompileWorker 异步消费。
    /// 复用 `run` 的同一 admission 语义（preflight/预算/去重/force），不复制 SQL。
    /// Step7 B4 admit-only entry (spec step7 §3 D4/D5): scans the source, runs the
    /// compatibility preflight and projection, and calls `kernel.admit_compile`
    /// to queue entities — **no claim, no model call**. Returns the admitted
    /// task_ids for the resident CompileWorker to consume asynchronously. Reuses
    /// the exact admission semantics of `run` (preflight/budget/dedup/force)
    /// without copying SQL.
    pub async fn admit_batch(
        &self,
        source: &dyn crate::traits::DataSource,
        ctx: &CompileContext,
        options: crate::compile::config::RunOptions,
    ) -> Result<AdmitOutcome> {
        self.policy.validate()?;
        options.validate()?;
        let schema = source.schema();
        let mut stats = CompileStats {
            run_id: format!("{}-admit", self.clock.unix_seconds()),
            ..CompileStats::default()
        };
        let mut task_ids: Vec<i64> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut offset = 0usize;
        let mut remaining = options.limit;
        let mut compat_checked = false;
        loop {
            let requested = remaining.min(options.batch_size);
            let batch = source
                .fetch(Some(Cursor {
                    offset,
                    batch_size: requested,
                }))
                .await?;
            enforce_fetch_protocol(&batch, requested)?;
            if batch.is_empty() {
                break;
            }
            remaining -= batch.len();
            offset += batch.len();
            if !compat_checked {
                compat_checked = true;
                self.run_compatibility_preflight(&batch[0].id.domain, ctx, false)
                    .await?;
            }
            for raw in batch {
                stats.scanned += 1;
                let key = raw.id.to_key();
                if !seen.insert(key) {
                    stats.skipped += 1;
                    continue;
                }
                let prepared = match prepare_source(&raw, &schema, &self.policy) {
                    Ok(p) => p,
                    Err(_) => {
                        stats.failed += 1;
                        continue;
                    }
                };
                let kernel = self.kernel.clone();
                let ctx_clone = ctx.clone();
                let policy = self.policy.clone();
                let schema_clone = schema.clone();
                let force = options.force;
                match blocking(move || {
                    kernel.admit_compile(&prepared, &ctx_clone, &policy, &schema_clone, force)
                })
                .await?
                {
                    Admission::Queued(task_id) => task_ids.push(task_id),
                    Admission::Skipped => stats.skipped += 1,
                    Admission::Deferred => stats.deferred += 1,
                    Admission::Rejected(_) => stats.failed += 1,
                }
            }
            if remaining == 0 {
                break;
            }
        }
        Ok(AdmitOutcome { stats, task_ids })
    }

    /// dry-run（§9）：只读配置/源/现有 DB，算 hash/计划；不写事实/任务、
    /// 不占预算、不请求模型。
    /// Dry-run (§9): reads config/source/existing DB only, computing hashes and
    /// the plan; no facts/tasks, no budget, no model requests.
    async fn run_dry(
        &self,
        source: &dyn DataSource,
        ctx: &CompileContext,
        options: RunOptions,
        schema: &EntitySchema,
        stats: &mut CompileStats,
    ) -> Result<()> {
        // 已接受页 hash 索引（按首个实体的 domain 加载一次；artifact_version 参与了
        // content_hash，因此哈希相等即版本一致，无需单独比较）。
        // Accepted-page hash index (loaded once for the first entity's domain;
        // artifact_version is hashed into content_hash, so equal hashes imply
        // equal versions — no separate comparison needed).
        let mut accepted_hashes: HashMap<String, String> = HashMap::new();
        let mut accepted_loaded = false;
        let mut seen: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        let mut remaining = options.limit;
        // Step8 D11（批 B5）：dry-run 同样做兼容 preflight（只读；失败不写审核）。
        // Step8 D11 (batch B5): a dry-run runs the compatibility preflight too
        // (read-only; a failure writes no review).
        let mut compat_checked = false;
        loop {
            let requested = remaining.min(options.batch_size);
            let batch = source
                .fetch(Some(Cursor {
                    offset,
                    batch_size: requested,
                }))
                .await?;
            enforce_fetch_protocol(&batch, requested)?;
            if batch.is_empty() {
                break;
            }
            remaining -= batch.len();
            offset += batch.len();
            // Step8 D11（批 B5）：首个非空批次即定域并执行兼容 preflight（域取
            // 首实体，复用既有 load_accepted_pages 的「首实体定域」惯例）。
            // Step8 D11 (batch B5): the first non-empty batch fixes the domain
            // and triggers the compatibility preflight (the domain comes from
            // the first entity, reusing the established first-entity convention
            // of load_accepted_pages).
            if !compat_checked {
                compat_checked = true;
                self.run_compatibility_preflight(&batch[0].id.domain, ctx, true)
                    .await?;
            }
            if !accepted_loaded {
                if let Some(first) = batch.first() {
                    let kernel = self.kernel.clone();
                    let domain = first.id.domain.clone();
                    for page in blocking(move || kernel.load_accepted_pages(&domain)).await? {
                        accepted_hashes.insert(page.wiki.entity_id.to_key(), page.content_hash);
                    }
                }
                accepted_loaded = true;
            }
            for raw in batch {
                stats.scanned += 1;
                let key = raw.id.to_key();
                if !seen.insert(key.clone()) {
                    tracing::debug!(entity = %key, "dry-run skipped duplicate_in_run");
                    stats.skipped += 1;
                    continue;
                }
                let prepared = match prepare_source(&raw, schema, &self.policy) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(entity = %key, error = %e, "dry-run: prepare_source failed");
                        stats.failed += 1;
                        continue;
                    }
                };
                if !has_coverable_units(&prepared.knowledge) {
                    tracing::warn!(entity = %key, "dry-run: empty knowledge source quarantined (preflight)");
                    stats.quarantined += 1;
                    continue;
                }
                if canonical_text(&prepared.knowledge)?.len() > MAX_KNOWLEDGE_INPUT_BYTES {
                    tracing::warn!(entity = %key, "dry-run: knowledge input exceeds 64KiB, quarantined (preflight)");
                    stats.quarantined += 1;
                    continue;
                }
                let hash = content_hash(HashDependencies {
                    source: &prepared.knowledge,
                    context: ctx,
                    policy: &self.policy,
                    source_schema: schema,
                })?;
                if !options.force && accepted_hashes.get(&key) == Some(&hash) {
                    stats.skipped += 1;
                } else {
                    stats.would_compile += 1;
                }
            }
            if remaining == 0 {
                break;
            }
        }
        Ok(())
    }

    /// 真实 run：run 开头一次同步 recover（D8 保留）→ 可选周期 reaper spawn →
    /// 逐批 admission + 调度 → 结束时 cancel + join reaper（cancel 后 drain 一次）。
    /// Real run: one run-start synchronous recover (D8 kept) → optional periodic
    /// reaper spawn → batch-wise admission + scheduling → cancel + join the reaper
    /// at the end (which drains once after cancel).
    async fn run_real(
        &self,
        source: &dyn DataSource,
        ctx: &CompileContext,
        options: RunOptions,
        schema: &EntitySchema,
        stats: &mut CompileStats,
    ) -> Result<()> {
        let run_id = stats.run_id.clone();
        // §8.3：claim 前先回收过期租约（expired running 必须先经 recover）。
        // §8.3: recover expired leases before claiming (expired running must go
        // through recover first).
        let kernel = self.kernel.clone();
        let now = self.clock.unix_seconds();
        let recovery = blocking(move || kernel.recover_compile_leases(now)).await?;
        if recovery.recovered_tasks() > 0 {
            tracing::info!(?recovery, "recovered expired compile leases");
        }

        // —— Step8 §6.3/D8：run 期间 spawn 周期 reaper；结束时 cancel + join
        //    （`run_until_cancelled` 在 cancel 后 drain 一次再返回，join 完成即
        //    保证 drain 已发生）。spawn 要求 'static：LeaseReaper 字段均为
        //    Arc/Duration，可 Clone。
        // —— Step8 §6.3/D8: spawn the periodic reaper for the duration of the
        //    run; cancel + join at the end (`run_until_cancelled` drains once
        //    after cancel before returning, so a completed join proves the drain
        //    happened). spawn requires 'static: every LeaseReaper field is
        //    Arc/Duration, hence Clone.
        let reaper_cancel = CancellationToken::new();
        let reaper_join = self.lease_reaper.as_ref().map(|reaper| {
            let reaper = reaper.clone();
            let cancel = reaper_cancel.clone();
            tokio::spawn(async move { reaper.run_until_cancelled(cancel).await })
        });
        let result = self
            .run_real_scheduling(run_id, source, ctx, options, schema, stats)
            .await;
        reaper_cancel.cancel();
        if let Some(handle) = reaper_join {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "lease reaper ended with an error");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "lease reaper task panicked");
                }
            }
        }
        result
    }

    /// 真实 run 的调度主体：逐批 admission + 调度（§3/§8）。由 [`Self::run_real`]
    /// 包裹（reaper 生命周期在其两侧），本函数不含租约回收职责。
    /// The scheduling body of a real run: batch-wise admission + scheduling
    /// (§3/§8). Wrapped by [`Self::run_real`] (the reaper lifetime brackets it);
    /// lease recovery is not this function's responsibility.
    async fn run_real_scheduling(
        &self,
        run_id: String,
        source: &dyn DataSource,
        ctx: &CompileContext,
        options: RunOptions,
        schema: &EntitySchema,
        stats: &mut CompileStats,
    ) -> Result<()> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        let mut remaining = options.limit;
        // Step8 D11（批 B5）：首次 admission 前恰好一次兼容 preflight（同
        // dry-run：首个非空批次定域）。
        // Step8 D11 (batch B5): exactly one compatibility preflight before the
        // first admission (same as the dry-run: the first non-empty batch fixes
        // the domain).
        let mut compat_checked = false;
        // 退避累计等待预算（§9：最多 60s，不忙等）。
        // Cumulative backoff-wait budget (§9: at most 60s, no busy waiting).
        let mut wait_budget_ms: u64 = MAX_BACKOFF_WAIT_SECONDS * 1000;
        'fetch: loop {
            let requested = remaining.min(options.batch_size);
            let batch = source
                .fetch(Some(Cursor {
                    offset,
                    batch_size: requested,
                }))
                .await?;
            enforce_fetch_protocol(&batch, requested)?;
            if batch.is_empty() {
                break;
            }
            remaining -= batch.len();
            offset += batch.len();

            // —— Step8 D11（批 B5）：首次 admission 前的兼容 preflight ——
            // 不兼容时在写入任何 facts/tasks 之前整 run 失败（A15）。
            // —— Step8 D11 (batch B5): the compatibility preflight before the
            // first admission — on incompatibility the run fails before any
            // facts/tasks are written (A15).
            if !compat_checked {
                compat_checked = true;
                self.run_compatibility_preflight(&batch[0].id.domain, ctx, false)
                    .await?;
            }

            // —— admission：投影 → preflight → admit 分类（§7）——
            // —— admission: projection → preflight → admit classification (§7) ——
            let mut queued: Vec<i64> = Vec::new();
            for raw in batch {
                stats.scanned += 1;
                let key = raw.id.to_key();
                if !seen.insert(key.clone()) {
                    // 重复源记录 → skipped(duplicate_in_run)（§9）。
                    // Duplicate source record → skipped(duplicate_in_run) (§9).
                    tracing::debug!(entity = %key, "skipped duplicate_in_run");
                    stats.skipped += 1;
                    continue;
                }
                let prepared = match prepare_source(&raw, schema, &self.policy) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(entity = %key, error = %e, "prepare_source failed");
                        stats.failed += 1;
                        continue;
                    }
                };
                // preflight quarantine：空知识源 / 输入超限 → 不请求 LLM、不占
                // token（§6/§8.3）。
                // preflight quarantine: empty knowledge / oversize input → no
                // LLM, no tokens (§6/§8.3).
                if !has_coverable_units(&prepared.knowledge) {
                    tracing::warn!(entity = %key, "empty knowledge source quarantined (preflight)");
                    stats.quarantined += 1;
                    continue;
                }
                if canonical_text(&prepared.knowledge)?.len() > MAX_KNOWLEDGE_INPUT_BYTES {
                    tracing::warn!(entity = %key, "knowledge input exceeds 64KiB, quarantined (preflight)");
                    stats.quarantined += 1;
                    continue;
                }
                let kernel = self.kernel.clone();
                let policy = self.policy.clone();
                let ctx_clone = ctx.clone();
                let schema_clone = schema.clone();
                let force = options.force;
                // 偏差说明：spec §8.3 的 preflight quarantine 经
                // kernel.quarantine_compile_preflight 记为 dead/quarantined 任务，
                // 但 `Admission::Queued` 不返回 epoch，executor 无法在 admit 后
                // 定位 (task_id, epoch)；且该方法在领取前调用会缺少任务行。本批
                // 实现为 admit 前拦截（无任务行、无 token、无 LLM，行为等价于
                // "空知识源直接 quarantine"），持久化任务行的路径留待
                // Admission 返回 epoch 后切换。
                // Deviation note: spec §8.3 routes preflight quarantine through
                // kernel.quarantine_compile_preflight as a dead/quarantined task,
                // but `Admission::Queued` carries no epoch, so the executor
                // cannot locate (task_id, epoch) after admit, and calling the
                // kernel method pre-admit would lack a task row. This batch
                // intercepts before admission (no task row, no tokens, no LLM —
                // behaviorally equivalent to "empty knowledge quarantines
                // directly"); the persisted-task path switches once Admission
                // returns the epoch.
                match blocking(move || {
                    kernel.admit_compile(&prepared, &ctx_clone, &policy, &schema_clone, force)
                })
                .await?
                {
                    Admission::Queued(task_id) => queued.push(task_id),
                    Admission::Skipped => stats.skipped += 1,
                    Admission::Deferred => stats.deferred += 1,
                    Admission::Rejected(reason) => {
                        // source_revision_conflict 等输入契约冲突 → failed
                        //（非预算类，不 deferred）。
                        // Input-contract conflicts such as
                        // source_revision_conflict → failed (not budget-class,
                        // hence not deferred).
                        tracing::warn!(entity = %key, reason = %reason, "admission rejected");
                        stats.failed += 1;
                    }
                }
            }

            // —— 调度循环：claim → compile → validate → score → publish/failure ——
            // —— Scheduling loop: claim → compile → validate → score →
            //     publish/failure ——
            let mut retry_at: HashMap<i64, i64> = HashMap::new();
            while !queued.is_empty() {
                let now = self.clock.unix_seconds();
                let kernel = self.kernel.clone();
                let ids = queued.clone();
                let rid = run_id.clone();
                let lease = blocking(move || kernel.claim_compile(&ids, &rid, now)).await?;
                match lease {
                    Some(lease) => {
                        queued.retain(|t| *t != lease.task_id);
                        // 预算计量：复用 claim 事务的同一保守预留公式（§8.4），
                        // `lease.context`/`lease.source` 即冻结依赖，重序列化字节
                        // 与 claim 时的 source_json 一致。
                        // Budget metering: reuse the claim transaction's exact
                        // estimator (§8.4); `lease.context`/`lease.source` are the
                        // frozen dependencies whose re-serialized bytes match the
                        // claim-time source_json.
                        let input_json = canonical_text(&lease.source)?;
                        let units = estimate_budget_units(
                            &lease.context.prompt_template,
                            &input_json,
                            self.policy.max_output_tokens,
                        )?;
                        stats.reserved_tokens += units as u64;
                        match self.process_lease(&lease, schema, stats).await? {
                            LeaseOutcome::Finalized => {
                                retry_at.remove(&lease.task_id);
                            }
                            LeaseOutcome::RetryAt(at) => {
                                retry_at.insert(lease.task_id, at);
                                queued.push(lease.task_id);
                            }
                        }
                    }
                    None => {
                        // claim=None：有到期任务却领不到 → 预算熔断（§8.4）；
                        // 全部未到期 → 按退避等待（受累计预算约束）。
                        // claim=None: a due task exists but cannot be claimed →
                        // budget circuit (§8.4); nothing due → bounded backoff
                        // wait.
                        let now = self.clock.unix_seconds();
                        // 从未失败过的任务视为立即到期（无 retry_at 记录）。
                        // Tasks never failed are due immediately (no retry_at).
                        let earliest = queued
                            .iter()
                            .filter_map(|t| retry_at.get(t).copied())
                            .min()
                            .unwrap_or(now);
                        if earliest > now && wait_budget_ms > 0 {
                            let wait_ms = ((((earliest - now) as u64) * 1000) + WAKE_MARGIN_MS)
                                .min(wait_budget_ms);
                            tracing::debug!(wait_ms, "backoff wait before next claim");
                            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                            wait_budget_ms -= wait_ms;
                        } else {
                            // 预算熔断或退避超预算：剩余任务 deferred，本 run
                            // 停止调度（§8.4/§9：不把预算不足当质量失败）。
                            // Circuit open or backoff budget exhausted: remaining
                            // tasks deferred, scheduling stops for this run
                            // (§8.4/§9: budget shortage is never a quality
                            // failure).
                            stats.circuit_open = true;
                            stats.deferred += queued.len() as u64;
                            tracing::warn!(
                                deferred = queued.len() as u64,
                                "budget circuit open or backoff budget exhausted; deferring remaining tasks"
                            );
                            break 'fetch;
                        }
                    }
                }
            }
            if remaining == 0 {
                break;
            }
        }
        Ok(())
    }

    /// 处理一个租约：心跳 → compile → validator → scorer → publish /
    /// finish_failure（Step8 §6.3/D9：模型调用前启动心跳任务，模型返回后先
    /// 停止并 join 再进入发布/失败事务）。
    /// Processes one lease: heartbeat → compile → validator → scorer → publish /
    /// finish_failure (Step8 §6.3/D9: the heartbeat task starts before the model
    /// call; once the model returns it is stopped and joined before the
    /// publish/failure transactions).
    async fn process_lease(
        &self,
        lease: &TaskLease,
        schema: &EntitySchema,
        stats: &mut CompileStats,
    ) -> Result<LeaseOutcome> {
        // 每次模型请求计一次 attempt（含重编译；§3）。
        // One attempt per model request, recompiles included (§3).
        stats.attempts += 1;
        // —— D9：模型调用前启动心跳——每 `lease_seconds/2`（最小 1s）续租一次；
        //    返回 false（stale）或错误即停止。stale 只代表不再续租，任务的最终
        //    归属由 publish/failure 的 kernel fencing CAS 最终裁决；本处不直接
        //    改任务状态。
        // —— D9: start the heartbeat before the model call — renew every
        //    `lease_seconds/2` (minimum 1s); stop on false (stale) or on error.
        //    Stale only means "no more renewals"; the task's final owner is
        //    arbitrated by the publish/failure kernel fencing CAS — nothing is
        //    mutated here.
        let heartbeat = self.start_heartbeat(lease);
        let compile_result = self
            .compiler
            .compile(lease.source.clone(), &lease.context)
            .await;
        // —— D9：模型返回后先停止并 join 心跳，再进入 publish/failure——保证
        //    不与发布事务并发取连接（心跳是 spawn_blocking 短调用）。
        // —— D9: once the model returns, stop and join the heartbeat before
        //    publish/failure — guarantees no concurrent connection acquisition
        //    against the publish transaction (the heartbeat is a short
        //    spawn_blocking call).
        heartbeat.stop().await;
        match compile_result {
            Err(err) => {
                let failure = classify_compile_error(err)?;
                tracing::warn!(
                    task_id = lease.task_id,
                    epoch = lease.epoch,
                    failure = %failure,
                    "compile request failed"
                );
                let report = self.zero_score_report(lease);
                self.fail_lease(lease, &failure, None, &report, None, stats)
                    .await
            }
            Ok(mut page) => {
                let evidence = match page.evidence.as_ref() {
                    Some(e) => e,
                    None => {
                        // §4：经 PipelineExecutor 发布时 evidence=None 视为
                        // schema 失败（低质量候选，不占重试）。
                        // §4: publishing via PipelineExecutor treats
                        // evidence=None as a schema failure (a low-quality
                        // candidate, not a transport retry).
                        tracing::warn!(
                            task_id = lease.task_id,
                            "compiler returned no evidence; treated as schema failure"
                        );
                        let failure = CompileFailure::invalid("EVIDENCE_MISSING", "");
                        let report = self.zero_score_report(lease);
                        return self
                            .fail_lease(lease, &failure, None, &report, None, stats)
                            .await;
                    }
                };
                // usage 报告（§3：reported_tokens 为真实 token 累计）。
                // Usage reporting (§3: reported_tokens accumulate real tokens).
                if let Some(usage) = evidence.usage {
                    stats.reported_tokens += usage.input.saturating_add(usage.output);
                }
                // §5：机械验证（validator 只读知识快照，不访问网络/当前 facts）。
                // §5: mechanical validation (the validator reads only the
                // knowledge snapshot, no network/current facts).
                let refs = self.validator.validate(
                    &lease.source,
                    evidence,
                    lease.context.require_source_refs,
                );
                // §6：executor 在所有 Compiler 实现之后运行 validator + scorer；
                // Compiler 的占位 quality 不能短路评分。Step8 §4：validate →
                // top-k related → 仲裁 → score（enabled 且两者注入才仲裁，否则
                // None 路径保持 Step4 四维行为）。
                // §6: the executor runs validator + scorer after every Compiler;
                // the compiler's placeholder quality never short-circuits scoring.
                // Step8 §4: validate → top-k related → arbitrate → score
                // (arbitration engages only when enabled and both are wired;
                // otherwise the None path keeps the Step4 four-dim behavior).
                let consistency = self.arbitrate_consistency(lease, &page).await?;
                let report = match &consistency {
                    Some(c) => self.scorer.score_with_consistency(
                        &lease.source,
                        Some(&page),
                        &refs,
                        true,
                        c,
                        &lease.context,
                        &self.policy,
                    ),
                    None => self.scorer.score(
                        &lease.source,
                        Some(&page),
                        &refs,
                        true,
                        &lease.context,
                        &self.policy,
                    ),
                };
                // NaN/Inf/越界分数是插件 bug → 内部错误（§6，禁止 clamp）。
                // NaN/Inf/out-of-range scores are plugin bugs → internal error
                // (§6, never clamped).
                validate_score_finite(&report)?;
                if report.accepted {
                    // §10：可信 Compiler 注入的非空 qug_edges 只做有效性检查
                    //（QugGraph::from_edges 构造校验——短语归一化/图构造可完成，
                    // 不启动图服务）；非法载荷按 schema 失败处置（InvalidOutput，
                    // 消耗 recompile 次数），绝不发布不可重建的边。
                    // §10: non-empty qug_edges injected by a trusted Compiler get
                    // a validity check only (a QugGraph::from_edges construction
                    // check — phrase normalization/graph build must succeed; no
                    // graph service is started). Illegal payloads take the schema
                    // failure path (InvalidOutput, consuming recompiles) and are
                    // never published.
                    if !page.qug_edges.is_empty() {
                        match QugGraph::from_edges(
                            page.qug_edges.iter().cloned(),
                            QUG_EDGE_VALIDATION_DEPTH,
                        ) {
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    task_id = lease.task_id,
                                    error = %e,
                                    "compiler-injected qug_edges failed graph validation"
                                );
                                let failure = CompileFailure::invalid("QUG_EDGES_INVALID", "");
                                return self
                                    .fail_lease(
                                        lease,
                                        &failure,
                                        None,
                                        &report,
                                        consistency.as_ref(),
                                        stats,
                                    )
                                    .await;
                            }
                        }
                    }
                    let now = self.clock.unix_seconds();
                    normalize_page(&mut page, lease, now, &self.policy, schema, report.quality)?;
                    let kernel = self.kernel.clone();
                    let lease_clone = lease.clone();
                    let page_clone = page.clone();
                    let report_clone = report.clone();
                    let consistency_clone = consistency.clone();
                    match blocking(move || {
                        kernel.publish_compile(
                            &lease_clone,
                            &page_clone,
                            &report_clone,
                            consistency_clone.as_ref(),
                            now,
                        )
                    })
                    .await?
                    {
                        CommitOutcome::Accepted { generation } => {
                            tracing::info!(task_id = lease.task_id, generation, "page accepted");
                            stats.accepted += 1;
                            Ok(LeaseOutcome::Finalized)
                        }
                        // fencing：租约/source head 已前进；任务归新 admission
                        // 处理，本 run 只计 deferred（§8.3 stale 语义）。
                        // fencing: lease/source head moved on; the task belongs
                        // to the newer admission and this run counts deferred
                        // (§8.3 stale semantics).
                        CommitOutcome::Stale => {
                            tracing::warn!(
                                task_id = lease.task_id,
                                "publish stale (lease fenced); counted as deferred"
                            );
                            stats.deferred += 1;
                            Ok(LeaseOutcome::Finalized)
                        }
                    }
                } else {
                    // 质量候选失败 → InvalidOutput，消耗 recompile 次数（§8.3）。
                    // Quality-candidate failure → InvalidOutput, consuming
                    // recompiles (§8.3).
                    tracing::info!(
                        task_id = lease.task_id,
                        issues = report.issues.len(),
                        "candidate below publish gates"
                    );
                    let failure = CompileFailure::InvalidOutput {
                        code: quality_failure_code(&report),
                        response_prefix: String::new(),
                    };
                    self.fail_lease(
                        lease,
                        &failure,
                        Some(&page),
                        &report,
                        consistency.as_ref(),
                        stats,
                    )
                    .await
                }
            }
        }
    }

    /// 失败处置事务（§8.3；Step8 批 B3）：RetryAt → 留队等待；Quarantined/Failed
    /// → 终态计数。`consistency` 透传给 kernel——dead 终态据此在同一事务写
    /// consistency_json 并按 D5/D7 入队死信/一致性冲突审核。
    /// The failure-disposition transaction (§8.3; Step8 batch B3): RetryAt →
    /// stay queued; Quarantined/Failed → terminal counting. `consistency` flows
    /// through to the kernel — a dead terminal uses it to write consistency_json
    /// and enqueue the dead-letter/consistency-conflict reviews in the same
    /// transaction (D5/D7).
    async fn fail_lease(
        &self,
        lease: &TaskLease,
        failure: &CompileFailure,
        candidate: Option<&CompiledPage>,
        report: &ScoreReport,
        consistency: Option<&ConsistencyReport>,
        stats: &mut CompileStats,
    ) -> Result<LeaseOutcome> {
        let now = self.clock.unix_seconds();
        let kernel = self.kernel.clone();
        let lease_clone = lease.clone();
        let failure_clone = failure.clone();
        let candidate_clone = candidate.cloned();
        let report_clone = report.clone();
        let consistency_clone = consistency.cloned();
        match blocking(move || {
            kernel.finish_compile_failure(
                &lease_clone,
                &failure_clone,
                candidate_clone.as_ref(),
                &report_clone,
                consistency_clone.as_ref(),
                now,
            )
        })
        .await?
        {
            FailureDisposition::RetryAt(at) => {
                tracing::info!(
                    task_id = lease.task_id,
                    next_attempt_at = at,
                    "task requeued with backoff"
                );
                Ok(LeaseOutcome::RetryAt(at))
            }
            FailureDisposition::Quarantined => {
                stats.quarantined += 1;
                Ok(LeaseOutcome::Finalized)
            }
            FailureDisposition::Failed => {
                stats.failed += 1;
                Ok(LeaseOutcome::Finalized)
            }
        }
    }

    /// 启动租约心跳任务（D9）：间隔 = `lease_seconds/2`（最小 1s），测试可经
    /// `heartbeat_interval_override` 注入小间隔（生产路径恒 None）。心跳任务
    /// 自身不做 SQLite 长事务——每次续租是一次 spawn_blocking 短调用。
    /// Starts the lease heartbeat task (D9): interval = `lease_seconds/2`
    /// (minimum 1s); tests may inject a small interval via
    /// `heartbeat_interval_override` (production always leaves it None). The
    /// heartbeat task itself never holds a long SQLite transaction — each renewal
    /// is one short spawn_blocking call.
    fn start_heartbeat(&self, lease: &TaskLease) -> HeartbeatHandle {
        let interval = self
            .heartbeat_interval_override
            .unwrap_or_else(|| lease_heartbeat_interval(self.policy.lease_seconds));
        let runner = LeaseHeartbeat::new(
            self.kernel.clone(),
            self.clock.clone(),
            lease.clone(),
            interval,
        );
        HeartbeatHandle::spawn(runner)
    }

    /// Step8 §4 数据流段：candidate → top-k related（有界召回）→ 仲裁报告。
    /// `consistency.enabled` 且仲裁器与提供器**均**注入才执行；否则返回 None
    /// （保持 Step4 四维/无仲裁行为，consistency 列保持 NULL）。召回/仲裁错误
    /// fail-closed 向上传播（停止 run，不当单页质量失败吞掉——与内部错误同
    /// 一处理面）。
    /// The Step8 §4 data-flow segment: candidate → top-k related (bounded
    /// recall) → arbitration report. Runs only when `consistency.enabled` and
    /// **both** the arbiter and provider are wired; otherwise returns None
    /// (keeping the Step4 four-dim, un-arbitrated behavior with a NULL
    /// consistency column). Recall/arbitration errors propagate fail-closed
    /// (stopping the run instead of being swallowed as a single-page quality
    /// failure — the same surface as internal errors).
    async fn arbitrate_consistency(
        &self,
        lease: &TaskLease,
        page: &CompiledPage,
    ) -> Result<Option<ConsistencyReport>> {
        if !self.policy.consistency.enabled {
            return Ok(None);
        }
        let (Some(arbiter), Some(provider)) = (&self.consistency_arbiter, &self.candidate_provider)
        else {
            tracing::debug!(
                task_id = lease.task_id,
                "consistency enabled but arbiter/provider not wired; skipping arbitration"
            );
            return Ok(None);
        };
        let related = provider
            .top_k_related(page, self.policy.consistency.top_k)
            .await?;
        let report = arbiter
            .arbitrate(page, &related, &self.policy.consistency)
            .await?;
        Ok(Some(report))
    }

    /// 零分报告（§6：无法解码时仍调用 scorer 生成可观测零分）。
    /// Zero-score report (§6: the scorer still runs on undecodable output for an
    /// observable zero score).
    fn zero_score_report(&self, lease: &TaskLease) -> ScoreReport {
        self.scorer.score(
            &lease.source,
            None,
            &RefReport::default(),
            false,
            &lease.context,
            &self.policy,
        )
    }
}

/// spawn_blocking 包装：panics 映射为 `Error::Internal`（不新增 unwrap）。
/// `pub(crate)`：lease.rs 的 reaper/心跳运行器复用同一包装（Step8 §6.3，
/// 禁止双实现漂移）。
/// spawn_blocking wrapper: panics map to `Error::Internal` (no new unwraps).
/// `pub(crate)`: the lease.rs reaper/heartbeat runners reuse the same wrapper
/// (Step8 §6.3; no drifting duplicates).
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Internal(format!("blocking kernel task panicked: {e}")))?
}

/// fetch 协议校验（§3.1）：adapter 返回数量不得超过请求量。
/// Fetch-protocol check (§3.1): an adapter must not return more than requested.
fn enforce_fetch_protocol(batch: &[RawEntity], requested: usize) -> Result<()> {
    if batch.len() > requested {
        return Err(Error::Validation(format!(
            "data source protocol error: fetched {} entities but requested {requested}",
            batch.len()
        )));
    }
    Ok(())
}

/// U 判定（§6）：知识快照中是否存在非空 string leaf。
/// U predicate (§6): whether the knowledge snapshot has any non-empty string leaf.
fn has_coverable_units(source: &RawEntity) -> bool {
    source.fields.values().any(|v| match v {
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|i| matches!(i, serde_json::Value::String(s) if !s.is_empty())),
        _ => false,
    })
}

/// canonical JSON 文本（与 kernel `canonical_text` 同义：to_value 后紧凑序列化，
/// BTreeMap 保键序）。
/// Canonical JSON text (equivalent to the kernel's `canonical_text`:
/// to_value then compact serialization, BTreeMap key order).
fn canonical_text<T: serde::Serialize + ?Sized>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&value)?)
}

/// compile 错误 → 类型化失败（§4）。返回 Err 表示停止 run（数据库/内部故障）。
/// compile error → typed failure (§4). Err means stop the run (database /
/// internal fault).
fn classify_compile_error(err: Error) -> Result<CompileFailure> {
    match err {
        // 类型化失败直通（§4）。
        // Typed failures pass through (§4).
        Error::CompileFailure(failure) => Ok(failure),
        // 旧 Error::Compilation(String) 默认永久错误，禁止解析字符串推断状态（§4）。
        // Legacy Error::Compilation(String) defaults to permanent; never infer
        // states by parsing strings (§4).
        Error::Compilation(_) => Ok(CompileFailure::Permanent {
            code: "COMPILATION_ERROR".to_string(),
        }),
        Error::ContentHashMismatch { .. } => Ok(CompileFailure::Permanent {
            code: "CONTENT_HASH_MISMATCH".to_string(),
        }),
        // 数据库损坏/迁移失败/内部错误停止 run，不当单页质量问题吞掉（§13）。
        // Database corruption/migration failures/internal errors stop the run —
        // never swallowed as single-page quality issues (§13).
        Error::Database(_) | Error::Migration(_) | Error::Internal(_) => Err(err),
        // 其余（Validation/Serialization/InvalidConfig 等）为编译器插件契约错误 →
        // 永久失败（code 用稳定字面量，不携带错误文本，禁止字符串推断）。
        // Everything else (Validation/Serialization/InvalidConfig, ...) is a
        // compiler-plugin contract error → permanent (stable literal code, no
        // error text carried, no string inference).
        _other => Ok(CompileFailure::Permanent {
            code: "COMPILER_CONTRACT_ERROR".to_string(),
        }),
    }
}

/// 质量失败 code：取首个 issue code，无 issue 时退回阈值 code。
/// Quality-failure code: the first issue code, falling back to the threshold
/// code when there are no issues.
fn quality_failure_code(report: &ScoreReport) -> String {
    report
        .issues
        .first()
        .map(|i| i.code.clone())
        .unwrap_or_else(|| "QUALITY_BELOW_THRESHOLD".to_string())
}

/// publish 前归一化（§4/§5.2.4/§5.2.5）：页身份、canonical 正文、H2 sections、
/// 元数据与 content_hash 全部由 executor 重算，不信任 Compiler 填值。
/// Pre-publish normalization (§4/§5.2.4/§5.2.5): page identity, canonical body,
/// H2 sections, metadata and content_hash are all recomputed by the executor —
/// the Compiler's values are never trusted.
fn normalize_page(
    page: &mut CompiledPage,
    lease: &TaskLease,
    now: i64,
    policy: &CompilePolicy,
    schema: &EntitySchema,
    quality: QualityScore,
) -> Result<()> {
    let evidence = match page.evidence.as_ref() {
        Some(e) => e,
        None => return Err(Error::Compilation("evidence required for publish".into())),
    };
    page.wiki.page_id = lease.source.id.to_key();
    page.wiki.entity_id = lease.source.id.clone();
    // §5.2.4：正文由断言确定性重建（canonical renderer）。
    // §5.2.4: the body is deterministically rebuilt from assertions (canonical
    // renderer).
    let content = render_canonical_markdown(evidence);
    page.wiki.content = content.clone();
    // §5.2.5：sections 用既有 H2 切章语义（content 含 H2），由共享 splitter 生成。
    // §5.2.5: sections follow the existing H2-splitting semantics (content
    // includes the H2), produced by the shared splitter.
    page.wiki.sections = split_sections(&content);
    page.wiki.title = evidence.wiki.title.clone();
    page.wiki.aliases = evidence.wiki.aliases.clone();
    page.wiki.tags = evidence.wiki.tags.clone();
    page.wiki.metadata = PageMetadata {
        domain_pack_version: lease.context.domain_pack_version.clone(),
        compiled_at: now,
        model_version: lease.context.model_version.clone(),
        embedding_model: lease.context.embedding_model.clone(),
    };
    page.quality = quality;
    // D6：content_hash 以 knowledge 快照重算（与 admit 的 desired_hash 同一函数
    // 与同一输入；publish 事务校验一致）。
    // D6: content_hash recomputed over the knowledge snapshot (same function and
    // inputs as admit's desired_hash; the publish transaction verifies equality).
    page.content_hash = content_hash(HashDependencies {
        source: &lease.source,
        context: &lease.context,
        policy,
        source_schema: schema,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::config::build_context;
    use crate::compile::contract::DefaultSourceRefValidator;
    use crate::compile::mock::{MockBehavior, MockCompiler};
    use crate::compile::quality::RuleBasedScorer;
    use crate::data::jsonl::JsonlDataSource;
    use crate::kernel::RecoveryStats;
    use crate::traits::{EntityConfig, EntityStore};
    use crate::types::{EntityId, FactValue, FieldDefinition, FieldType, Filters};
    use diesel::prelude::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    /// 短 Prompt 模板（预算估算可控）。
    /// Short prompt template (keeps budget estimates predictable).
    const TEST_PROMPT: &str = "SYS";

    fn schema() -> EntitySchema {
        EntitySchema {
            entity_type: "drink".to_string(),
            fields: vec![
                FieldDefinition {
                    name: "name".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "description".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                },
                FieldDefinition {
                    name: "category".to_string(),
                    field_type: FieldType::Text,
                    filterable: true,
                },
                FieldDefinition {
                    name: "price".to_string(),
                    field_type: FieldType::Numeric,
                    filterable: true,
                },
            ],
        }
    }

    fn raw(id_slug: &str, revision: u64, price: f64) -> RawEntity {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        fields.insert(
            "category".to_string(),
            serde_json::json!("milk-tea:drink:boba"),
        );
        fields.insert("price".to_string(), serde_json::json!(price));
        RawEntity {
            id: EntityId::new("milk-tea", "drink", id_slug).unwrap(),
            fields,
            source_revision: revision,
        }
    }

    fn ctx() -> CompileContext {
        build_context(
            "test-v1",
            TEST_PROMPT,
            "mock-v1",
            "none",
            0.75,
            true,
            None,
            None,
        )
    }

    fn jsonl_line(id: &str, revision: u64, price: f64) -> String {
        format!(
            r#"{{"entity_id":"{id}","name":"啵啵","description":"珍珠奶茶","category":"milk-tea:drink:boba","price":{price},"source_revision":{revision}}}"#
        )
    }

    fn jsonl_source(path: &std::path::Path) -> JsonlDataSource {
        JsonlDataSource::from_config(
            &EntityConfig {
                name: "drink".to_string(),
                source: format!("jsonl://{}", path.display()),
                id_field: "entity_id".to_string(),
                type_field: "entity_type".to_string(),
                fields: schema().fields,
            },
            path.parent().unwrap(),
        )
        .unwrap()
    }

    fn executor(
        kernel: Arc<SqliteKernel>,
        compiler: Arc<dyn Compiler>,
        policy: CompilePolicy,
    ) -> PipelineExecutor {
        PipelineExecutor::new(
            kernel,
            compiler,
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            Arc::new(crate::compile::config::SystemClock),
            policy,
        )
    }

    fn executor_with_clock(
        kernel: Arc<SqliteKernel>,
        compiler: Arc<dyn Compiler>,
        policy: CompilePolicy,
        clock: Arc<dyn Clock>,
    ) -> PipelineExecutor {
        PipelineExecutor::new(
            kernel,
            compiler,
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            clock,
            policy,
        )
    }

    struct FixedClock(AtomicI64);

    impl FixedClock {
        fn new(t: i64) -> Self {
            Self(AtomicI64::new(t))
        }
    }

    impl Clock for FixedClock {
        fn unix_seconds(&self) -> i64 {
            self.0.load(AtomicOrdering::SeqCst)
        }
    }

    /// 测试内预算估算（与 kernel estimate_budget_units 同一公式）。
    /// Test-side budget estimate (the same formula as the kernel's
    /// estimate_budget_units).
    fn budget_units(policy: &CompilePolicy, entity: &RawEntity) -> u64 {
        let prepared = prepare_source(entity, &schema(), policy).unwrap();
        let input_json = canonical_text(&prepared.knowledge).unwrap();
        (TEST_PROMPT.len() + input_json.len() + 256 + policy.max_output_tokens as usize) as u64
    }

    fn write_jsonl(lines: &[String]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entities.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        (dir, path)
    }

    fn file_db(dir: &tempfile::TempDir) -> (Arc<SqliteKernel>, PathBuf) {
        let path = dir.path().join("wiktor.db");
        (Arc::new(SqliteKernel::open(&path).unwrap()), path)
    }

    #[derive(QueryableByName)]
    struct GenRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        generation: i64,
    }

    fn read_generation(db_path: &std::path::Path, page_id: &str) -> i64 {
        use diesel::prelude::*;
        let mut conn =
            diesel::sqlite::SqliteConnection::establish(db_path.to_str().expect("db path utf8"))
                .unwrap();
        // 与 kernel establish 同一 pragma 面（spec step6 §9 busy timeout 5s）：
        // 读代数时内核连接可能正在写，等待而非立即 BUSY。
        // Same pragma surface as the kernel establish (spec step6 §9, 5s busy
        // timeout): the kernel connection may be writing while generations are
        // read — wait instead of failing with an immediate BUSY.
        diesel::connection::SimpleConnection::batch_execute(
            &mut conn,
            crate::schema::BUSY_TIMEOUT_PRAGMA_SQL,
        )
        .unwrap();
        diesel::sql_query("SELECT generation FROM pages WHERE page_id = ?")
            .bind::<diesel::sql_types::Text, _>(page_id)
            .get_result::<GenRow>(&mut conn)
            .unwrap()
            .generation
    }

    // A1：Mock 全链路 —— pages/sections/quality/frontmatter/generation 与
    // succeeded task 一致，FTS 可搜，四维由真实 scorer 产生。
    // A1: Mock full pipeline — pages/sections/quality/frontmatter/generation
    // consistent with the succeeded task, FTS searchable, four dimensions from
    // the real scorer.
    #[tokio::test]
    async fn a1_mock_full_pipeline_publishes_searchable_page() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, _db) = file_db(&dir);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert!(stats.run_id.len() > 8, "real run_id = timestamp + uuid");
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.quarantined, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.deferred, 0);
        assert!(stats.reserved_tokens > 0);
        assert!(!stats.circuit_open);
        assert_eq!(mock.call_count(), 1);

        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.wiki.page_id, "milk-tea:drink:boba");
        // 标题 = 第一个允许字段值（BTreeMap 序：description 在前）。
        // Title = the first allowed field value (BTreeMap order: description first).
        assert_eq!(page.wiki.title, "珍珠奶茶");
        assert_eq!(
            page.wiki.content,
            "## 概述\n\n- 珍珠奶茶[[ref:r1]]\n- 啵啵[[ref:r2]]\n"
        );
        assert_eq!(page.wiki.sections.len(), 1);
        assert_eq!(page.wiki.sections[0].heading, "概述");
        assert!(page.wiki.sections[0].content.contains("## 概述"));
        // 四维由真实 scorer 产生（两条断言两个合法 refs → 全 1）。
        // Four dimensions from the real scorer (two assertions/two valid refs →
        // all ones).
        assert!((page.quality.coverage - 1.0).abs() < 1e-6);
        assert!((page.quality.citation - 1.0).abs() < 1e-6);
        assert!((page.quality.schema_compliance - 1.0).abs() < 1e-6);
        assert!((page.quality.density - 1.0).abs() < 1e-6);
        assert!(page.quality.consistency.is_none());
        // metadata 来自冻结 context；evidence 从 artifact_json 还原。
        // metadata from the frozen context; evidence restored from artifact_json.
        assert_eq!(page.wiki.metadata.model_version, "mock-v1");
        assert_eq!(page.wiki.metadata.domain_pack_version, "test-v1");
        let evidence = page.evidence.as_ref().expect("evidence restored");
        assert!(evidence.usage.is_none());
        assert_eq!(evidence.sections[0].refs.len(), 2);
        // frontmatter aliases/tags（Mock 恒空）。
        // frontmatter aliases/tags (empty for the Mock).
        assert!(page.wiki.aliases.is_empty());
        assert!(page.wiki.tags.is_empty());
        // FTS 可搜（trigram：≥3 字符查询）。
        // FTS searchable (trigram: ≥3-char query).
        let hits = kernel
            .search("珍珠奶茶", &Filters::empty(), 5, Some("milk-tea"))
            .unwrap();
        assert!(
            hits.iter().any(|h| h.page_id == "milk-tea:drink:boba"),
            "expected FTS hit, got {hits:?}"
        );
        let _ = jdir;
    }

    // A2：相同数据/依赖重跑 —— 模型 0 次、skipped=1、generation/sections/attempts 不增。
    // A2: rerun with identical data/deps — zero model calls, skipped=1,
    // generation/sections/attempts unchanged.
    #[tokio::test]
    async fn a2_identical_rerun_skips_without_model_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let first = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(first.accepted, 1);
        let gen_before = read_generation(&db_path, "milk-tea:drink:boba");
        let sections_before = kernel.load_accepted_pages("milk-tea").unwrap()[0]
            .wiki
            .sections
            .len();

        let second = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.skipped, 1);
        assert_eq!(second.accepted, 0);
        assert_eq!(second.attempts, 0);
        assert_eq!(second.would_compile, 0);
        // 模型调用 0 次；generation 与 sections 不增。
        // Zero model calls; generation and sections unchanged.
        assert_eq!(mock.call_count(), 1);
        assert_eq!(read_generation(&db_path, "milk-tea:drink:boba"), gen_before);
        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages[0].wiki.sections.len(), sections_before);
        let _ = jdir;
    }

    // A11：页级刹车 —— 连续差输出 max_recompiles=2 → 最多 3 个候选
    // （attempts=3）后 dead/quarantined；max_recompiles=0 → 1 个。
    // A11: page-level brake — consecutive bad output with max_recompiles=2 →
    // at most 3 candidates (attempts=3) then dead/quarantined;
    // max_recompiles=0 → 1.
    #[tokio::test]
    async fn a11_page_brake_quarantines_after_recompiles() {
        for (max_recompiles, expected_attempts) in [(2u32, 3u64), (0, 1)] {
            let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
            let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
            let source = jsonl_source(&jpath);
            let policy = CompilePolicy {
                max_recompiles,
                ..CompilePolicy::default()
            };
            let mock = Arc::new(MockCompiler::with_behavior(
                policy.clone(),
                MockBehavior::LowQuality,
            ));
            let ex = executor(kernel.clone(), mock.clone(), policy);
            let stats = ex
                .run(&source, &ctx(), RunOptions::default())
                .await
                .unwrap();
            assert_eq!(stats.scanned, 1, "max_recompiles={max_recompiles}");
            assert_eq!(stats.quarantined, 1, "max_recompiles={max_recompiles}");
            assert_eq!(stats.failed, 0);
            assert_eq!(stats.accepted, 0);
            assert_eq!(stats.attempts, expected_attempts);
            assert_eq!(stats.deferred, 0);
            assert_eq!(mock.call_count(), expected_attempts);
            // 无 accepted 页（隔离版本不入索引）。
            // No accepted page (quarantined versions never enter the index).
            assert!(kernel.load_accepted_pages("milk-tea").unwrap().is_empty());
            let _ = jdir;
        }
    }

    // A12：任务刹车 —— 3 次可重试传输失败 → dead/failed（每失败计一次，
    // claim/心跳不增加 retry_count）；401 首次 terminal。
    // A12: task brake — 3 retryable transport failures → dead/failed (one count
    // per failure; claim/heartbeat never add retry_count); 401 terminal at once.
    #[tokio::test]
    async fn a12_task_brake_on_transport_failures() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let mock = Arc::new(MockCompiler::with_behavior(
            policy.clone(),
            MockBehavior::Retryable {
                code: "RATE_LIMITED".into(),
                retry_after_seconds: None,
            },
        ));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.attempts, 3, "each transport failure counts once");
        assert_eq!(stats.quarantined, 0);
        assert_eq!(stats.deferred, 0);
        assert_eq!(mock.call_count(), 3);
        assert!(kernel.load_accepted_pages("milk-tea").unwrap().is_empty());
        let _ = jdir;
    }

    #[tokio::test]
    async fn a12_permanent_failure_terminal_on_first_attempt() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let mock = Arc::new(MockCompiler::with_behavior(
            policy.clone(),
            MockBehavior::Permanent {
                code: "UNAUTHORIZED".into(),
            },
        ));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.failed, 1);
        assert_eq!(
            stats.attempts, 1,
            "401-style permanent error terminates at once"
        );
        assert_eq!(stats.deferred, 0);
        assert_eq!(mock.call_count(), 1);
        let _ = jdir;
    }

    // A13（情形 1）：剩余额度不足一次预留 → 无模型调用、pending/deferred、
    // circuit_open。
    // A13 (case 1): remaining budget below one reservation → no model calls,
    // pending/deferred, circuit_open.
    #[tokio::test]
    async fn a13_batch_budget_circuit_blocks_all_model_calls() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy {
            batch_token_budget: 10,
            ..CompilePolicy::default()
        };
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.deferred, 1);
        assert!(stats.circuit_open);
        assert_eq!(stats.attempts, 0);
        assert_eq!(stats.accepted, 0);
        assert_eq!(stats.reserved_tokens, 0);
        assert_eq!(mock.call_count(), 0);
        let _ = jdir;
    }

    // A13（情形 2）：run 预算恰好一次预留，第二个实体（更短 id → 更小预留）
    // 跨 fetch 仍被熔断 → 预算不清零。
    // A13 (case 2): the run budget fits exactly one reservation; the second
    // entity (shorter id → smaller reservation) still trips the circuit across
    // fetches → budgets never reset between fetches.
    #[tokio::test]
    async fn a13_run_budget_persists_across_fetches() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let boba = raw("boba", 1, 19.0);
        let tea = raw("tea", 1, 19.0);
        let (jdir, jpath) = write_jsonl(&[
            jsonl_line("milk-tea:drink:boba", 1, 19.0),
            jsonl_line("milk-tea:drink:tea", 1, 19.0),
        ]);
        let source = jsonl_source(&jpath);
        let b = budget_units(&CompilePolicy::default(), &boba);
        assert!(b > budget_units(&CompilePolicy::default(), &tea));
        let policy = CompilePolicy {
            batch_token_budget: b,
            ..CompilePolicy::default()
        };
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let stats = ex
            .run(
                &source,
                &ctx(),
                RunOptions {
                    batch_size: 1,
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.deferred, 1);
        assert!(stats.circuit_open);
        assert_eq!(stats.attempts, 1);
        assert_eq!(stats.reserved_tokens, b);
        assert_eq!(mock.call_count(), 1);
        let _ = jdir;
    }

    // A14：日预算 —— 两 run 共享日限额不超；省略日配置不可绕过已存限额
    //（注入 Clock 固定 utc_day）。
    // A14: daily budget — two runs sharing the day limit never over-reserve;
    // omitting the daily config cannot bypass an existing day row (injected
    // Clock pins utc_day).
    #[tokio::test]
    async fn a14_daily_budget_shared_across_runs() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let clock = Arc::new(FixedClock::new(1_760_000_000));
        let b = budget_units(&CompilePolicy::default(), &raw("boba", 1, 19.0));
        let policy = CompilePolicy {
            daily_token_budget: Some(b),
            ..CompilePolicy::default()
        };

        // run A：boba → 接受，日预留 = b。
        // run A: boba → accepted, day reservation = b.
        let (jdir, jpath) = write_jsonl(&[
            jsonl_line("milk-tea:drink:boba", 1, 19.0),
            jsonl_line("milk-tea:drink:oolong", 1, 19.0),
            jsonl_line("milk-tea:drink:latte", 1, 19.0),
        ]);
        let source_a = jsonl_source(&jpath);
        let mock_a = Arc::new(MockCompiler::new(policy.clone()));
        let ex_a = executor_with_clock(
            kernel.clone(),
            mock_a.clone(),
            policy.clone(),
            clock.clone(),
        );
        // run A：limit=1 只扫 boba → 接受，日预留 = b。
        // run A: limit=1 scans boba only → accepted, day reservation = b.
        let stats_a = ex_a
            .run(
                &source_a,
                &ctx(),
                RunOptions {
                    limit: 1,
                    batch_size: 1,
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(stats_a.accepted, 1);
        assert_eq!(stats_a.reserved_tokens, b);
        assert!(!stats_a.circuit_open);

        // run B：同日扫 boba+oolong → boba skip，oolong 领不到（日预算不足）→
        // deferred + 熔断。
        // run B: scans boba+oolong the same day → boba skipped, oolong
        // unclaimable (daily budget) → deferred + circuit.
        let source_b = jsonl_source(&jpath);
        let mock_b = Arc::new(MockCompiler::new(policy.clone()));
        let ex_b = executor_with_clock(
            kernel.clone(),
            mock_b.clone(),
            policy.clone(),
            clock.clone(),
        );
        let stats_b = ex_b
            .run(
                &source_b,
                &ctx(),
                RunOptions {
                    limit: 2,
                    batch_size: 2,
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap();
        // run B 从 run A 已接受页 skip 到 boba（skipped），oolong 因日预算 deferred。
        // run B skips boba (accepted by run A) and defers oolong on the daily
        // budget.
        assert_eq!(stats_b.scanned, 2);
        assert_eq!(stats_b.skipped, 1);
        assert_eq!(stats_b.deferred, 1);
        assert!(stats_b.circuit_open);
        assert_eq!(stats_b.attempts, 0);
        assert_eq!(mock_b.call_count(), 0);

        // 并发/跨 run 预留总和不超过日限额。
        // Reservations across runs never exceed the daily limit.
        assert!(stats_a.reserved_tokens + stats_b.reserved_tokens <= b);

        // run C：省略日配置 → 已存日限额仍然生效（不可绕过）；oolong/latte 均
        // deferred。
        // run C: daily config omitted → the stored day limit still binds;
        // oolong/latte both deferred.
        let policy_no_daily = CompilePolicy {
            daily_token_budget: None,
            ..CompilePolicy::default()
        };
        let source_c = jsonl_source(&jpath);
        let mock_c = Arc::new(MockCompiler::new(policy_no_daily.clone()));
        let ex_c = executor_with_clock(
            kernel.clone(),
            mock_c.clone(),
            policy_no_daily,
            clock.clone(),
        );
        let stats_c = ex_c
            .run(
                &source_c,
                &ctx(),
                RunOptions {
                    limit: 3,
                    batch_size: 3,
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(stats_c.scanned, 3);
        assert_eq!(stats_c.skipped, 1, "boba still skipped by hash");
        assert_eq!(
            stats_c.deferred, 2,
            "oolong/latte still deferred by stored day row"
        );
        assert!(stats_c.circuit_open);
        assert_eq!(stats_c.attempts, 0);
        assert_eq!(mock_c.call_count(), 0);
        let _ = jdir;
    }

    // A19：仅 price/stock + revision 改动 → facts CAS 更新而模型 0 次，页
    // 保留旧引用 revision；敏感字段不进模型输入但允许本地事实存储。
    // A19: price/stock-only change with a revision bump → facts CAS updated
    // with zero model calls, the page keeps its original revision refs; the
    // sensitive field never enters model inputs but may be stored as facts.
    #[tokio::test]
    async fn a19_fact_only_changes_skip_model_and_keep_page_revision() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let policy = CompilePolicy {
            sensitive_fields: vec!["secret_recipe".to_string()],
            ..CompilePolicy::default()
        };
        // 敏感字段 schema：text 非过滤（进入事实平面，不进知识平面）。
        // Sensitive schema field: non-filterable text (fact plane only).
        let schema_with_secret = EntitySchema {
            entity_type: "drink".to_string(),
            fields: {
                let mut v = schema().fields;
                v.push(FieldDefinition {
                    name: "secret_recipe".to_string(),
                    field_type: FieldType::Text,
                    filterable: false,
                });
                v
            },
        };
        // revision 1/2 的 JSONL 均包含敏感字段（事实平面要求 schema 全字段；
        // 敏感字段允许本地存储，不进知识平面）。
        // Both JSONL revisions include the sensitive field (the fact plane
        // requires every declared schema field; sensitive fields may be stored
        // locally, never entering the knowledge plane).
        let line_v1 = r#"{"entity_id":"milk-tea:drink:boba","name":"啵啵","description":"珍珠奶茶","category":"milk-tea:drink:boba","price":19.0,"secret_recipe":"祖传配方","source_revision":1}"#;
        let line_v2 = r#"{"entity_id":"milk-tea:drink:boba","name":"啵啵","description":"珍珠奶茶","category":"milk-tea:drink:boba","price":21.0,"secret_recipe":"祖传配方","source_revision":2}"#;
        let (jdir, jpath) = write_jsonl(&[line_v1.to_string()]);
        let source_cfg = EntityConfig {
            name: "drink".to_string(),
            source: format!("jsonl://{}", jpath.display()),
            id_field: "entity_id".to_string(),
            type_field: "entity_type".to_string(),
            fields: schema_with_secret.fields.clone(),
        };
        let source = JsonlDataSource::from_config(&source_cfg, jpath.parent().unwrap()).unwrap();

        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy.clone());
        let first = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(first.accepted, 1);
        assert_eq!(first.attempts, 1);
        assert_eq!(mock.call_count(), 1);

        // revision 2：仅 price 变化（知识平面不变 → content_hash 不变 → skip）。
        // revision 2: price-only change (knowledge plane unchanged → same
        // content_hash → skip).
        std::fs::write(&jpath, line_v2).unwrap();
        let second = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.skipped, 1);
        assert_eq!(second.accepted, 0);
        assert_eq!(
            second.attempts, 0,
            "model never called for fact-only change"
        );
        assert_eq!(mock.call_count(), 1, "model calls unchanged");

        // facts CAS 已更新到 revision 2。
        // facts CAS advanced to revision 2.
        let entity = EntityId::from_key("milk-tea:drink:boba").unwrap();
        let facts = kernel
            .get_facts(&entity)
            .await
            .unwrap()
            .expect("facts exist");
        assert_eq!(facts.source_revision, 2);
        assert_eq!(facts.fields.get("price"), Some(&FactValue::Numeric(21.0)));

        // 页/generation 保留旧引用 revision（evidence refs 仍为 revision 1）。
        // Page/generation keep the original revision (evidence refs stay at 1).
        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        let evidence = pages[0].evidence.as_ref().unwrap();
        assert!(
            evidence
                .sections
                .iter()
                .flat_map(|s| s.refs.iter())
                .all(|r| r.source_revision == 1),
            "page must keep its original revision refs"
        );

        // 敏感字段不进模型输入（知识快照），但允许本地事实存储。
        // The sensitive field never enters model inputs (knowledge snapshot)
        // but is stored as local facts.
        for input in mock.received_inputs() {
            assert!(
                !input.fields.contains_key("secret_recipe"),
                "sensitive field leaked into model input"
            );
        }
        assert!(facts.fields.contains_key("secret_recipe"));
        let _ = jdir;
    }

    /// 脚本式 Compiler：Mock 正常产物 + 注入一条 qug_edge（§10 可信注入通道）。
    /// A scripted Compiler: a normal Mock artifact plus one injected qug_edge
    /// (the §10 trusted-injection channel).
    struct EdgeInjector {
        policy: CompilePolicy,
        edge: crate::types::QugEdge,
    }

    #[async_trait::async_trait]
    impl Compiler for EdgeInjector {
        async fn compile(
            &self,
            raw: RawEntity,
            ctx: &crate::types::CompileContext,
        ) -> Result<CompiledPage> {
            let mock = MockCompiler::new(self.policy.clone());
            let mut page = mock.compile(raw, ctx).await?;
            page.qug_edges = vec![self.edge.clone()];
            Ok(page)
        }
    }

    // §10：合法注入边经 from_edges 构造校验后随接受事务持久化（canonical edge
    // JSON 的 BLAKE3 edge_hash 去重由 PK (page_id, edge_hash) 承担），并可由
    // load_accepted_pages 还原。
    // §10: a legal injected edge passes the from_edges construction check, is
    // persisted with the accept transaction (BLAKE3 edge_hash dedup over
    // canonical edge JSON via PK (page_id, edge_hash)) and restores through
    // load_accepted_pages.
    #[tokio::test]
    async fn injected_qug_edges_persist_and_round_trip() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let edge = crate::types::QugEdge::Synonym {
            from: "波霸奶茶".into(),
            to: vec!["珍珠奶茶".into()],
        };
        let ex = executor(
            kernel.clone(),
            Arc::new(EdgeInjector {
                policy: policy.clone(),
                edge,
            }),
            policy,
        );
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.quarantined, 0);

        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].qug_edges.len(), 1, "edge persisted and restored");
        match &pages[0].qug_edges[0] {
            crate::types::QugEdge::Synonym { from, to } => {
                assert_eq!(from, "波霸奶茶");
                assert_eq!(to, &vec!["珍珠奶茶".to_string()]);
            }
            other => panic!("unexpected edge {other:?}"),
        }
        let _ = jdir;
    }

    // §10：非法注入边（空短语 → normalize 拒绝 → from_edges 构造失败）走 schema
    // 失败路径，重编译耗尽后隔离，绝不发布。
    // §10: an illegal injected edge (empty phrase → normalize rejects →
    // from_edges fails) takes the schema failure path and quarantines once
    // recompiles are exhausted — never published.
    #[tokio::test]
    async fn injected_qug_edges_invalid_are_quarantined_without_publish() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let edge = crate::types::QugEdge::Synonym {
            from: "   ".into(),
            to: vec!["珍珠奶茶".into()],
        };
        let ex = executor(
            kernel.clone(),
            Arc::new(EdgeInjector {
                policy: policy.clone(),
                edge,
            }),
            policy,
        );
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.quarantined, 1);
        assert_eq!(stats.accepted, 0);
        assert_eq!(
            stats.attempts, 3,
            "max_recompiles=2 → exactly 3 quality candidates"
        );
        assert!(kernel.load_accepted_pages("milk-tea").unwrap().is_empty());
        let _ = jdir;
    }

    // ===== Step8 批 B3：A9/A10（一致性刹车 → 死信与审核原子接入）=====
    // ===== Step8 batch B3: A9/A10 (the consistency brake → atomic dead-letter
    // and review wiring) =====

    use crate::compile::config::ConsistencyPolicy;
    use crate::compile::consistency::{
        ClaimKey, ConsistencyArbiter, ConsistencyCandidateProvider, ConsistencyFinding,
        VALUE_DIVERGENCE,
    };

    /// 固定报告仲裁器（trait 对象注入面）。
    /// A fixed-report arbiter (the trait-object injection surface).
    struct PresetArbiter {
        report: ConsistencyReport,
    }

    #[async_trait::async_trait]
    impl ConsistencyArbiter for PresetArbiter {
        async fn arbitrate(
            &self,
            _candidate: &CompiledPage,
            _related: &[CompiledPage],
            _policy: &ConsistencyPolicy,
        ) -> Result<ConsistencyReport> {
            Ok(self.report.clone())
        }
    }

    /// 空候选提供器（无相关页；召回边界由 consistency.rs 的默认 provider 测试覆盖）。
    /// An empty candidate provider (no related pages; recall bounds are covered by
    /// the default-provider tests in consistency.rs).
    struct EmptyProvider;

    #[async_trait::async_trait]
    impl ConsistencyCandidateProvider for EmptyProvider {
        async fn top_k_related(
            &self,
            _candidate: &CompiledPage,
            _limit: u32,
        ) -> Result<Vec<CompiledPage>> {
            Ok(Vec::new())
        }
    }

    fn conflict_report() -> ConsistencyReport {
        ConsistencyReport {
            score: Some(0.0),
            compared_claims: 1,
            findings: vec![ConsistencyFinding {
                code: VALUE_DIVERGENCE.into(),
                key: ClaimKey {
                    entity_id: "milk-tea:ingredient:pearl".into(),
                    pointer: "/fields/name".into(),
                },
                candidate_value_hash: "a".repeat(64),
                evidence_value_hash: "b".repeat(64),
            }],
            candidate_count: 0,
        }
    }

    fn passing_report() -> ConsistencyReport {
        ConsistencyReport {
            score: Some(1.0),
            compared_claims: 1,
            findings: vec![],
            candidate_count: 0,
        }
    }

    fn consistency_policy() -> CompilePolicy {
        CompilePolicy {
            consistency: ConsistencyPolicy {
                enabled: true,
                compare_pointers: vec!["/fields/name".into()],
                ..ConsistencyPolicy::default()
            },
            ..CompilePolicy::default()
        }
    }

    fn wired_executor(
        kernel: Arc<SqliteKernel>,
        mock: Arc<MockCompiler>,
        policy: CompilePolicy,
        report: ConsistencyReport,
    ) -> PipelineExecutor {
        PipelineExecutor::new(
            kernel,
            mock,
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            Arc::new(crate::compile::config::SystemClock),
            policy,
        )
        .with_consistency_arbiter(Arc::new(PresetArbiter { report }))
        .with_candidate_provider(Arc::new(EmptyProvider))
    }

    // A9：一致性冲突作为质量候选失败，max_recompiles=2 下最多 3 个候选后
    // dead/quarantined；同一事务一条 compile_dead_letter（subject 恰为
    // {"task_id":N}、compile_task_id 回填）+ 一条 consistency_conflict；A10：
    // 重复 recover 不再产生第二行。
    // A9: a consistency conflict is a quality-candidate failure; with
    // max_recompiles=2 at most 3 candidates run before dead/quarantined; the
    // same transaction holds one compile_dead_letter (subject exactly
    // {"task_id":N}, compile_task_id backfilled) plus one consistency_conflict.
    // A10: a repeated recovery never produces a second row.
    #[tokio::test]
    async fn a9_consistency_conflicts_brake_into_dead_letter_and_review() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = consistency_policy();
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = wired_executor(kernel.clone(), mock, policy, conflict_report());
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.quarantined, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.accepted, 0);
        assert_eq!(stats.attempts, 3, "max_recompiles=2 → at most 3 candidates");

        // 审核 list 可读（Step6 公共 API）：一条死信 + 一条一致性冲突；死信
        // subject 恰为 {"task_id":N} 且 compile_task_id 回填为同一任务。
        // The review list is readable (the Step6 public API): one dead letter +
        // one consistency conflict; the dead-letter subject is exactly
        // {"task_id":N} and compile_task_id backfills the same task.
        let rows = kernel.list_reviews("milk-tea", None, 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(kernel.row_counts().unwrap()["compile_tasks"], 1);
        for action in ["compile_dead_letter", "consistency_conflict"] {
            let row = rows
                .iter()
                .find(|r| r.action == action)
                .unwrap_or_else(|| panic!("missing {action} row"));
            let task_id = row.compile_task_id.expect("compile_task_id backfilled");
            assert_eq!(row.subject_json, format!(r#"{{"task_id":{task_id}}}"#));
            if action == "consistency_conflict" {
                let reason: serde_json::Value = serde_json::from_str(&row.reason_json).unwrap();
                assert_eq!(reason["code"], "CONSISTENCY_CONFLICT");
                assert_eq!(reason["task_id"], task_id);
                assert_eq!(reason["findings"].as_array().map(Vec::len), Some(1));
            }
        }

        // —— A10：重复 recover（无 running 任务）→ 零回收，审核行不变 ——
        // —— A10: a repeated recovery (no running tasks) recovers nothing and
        //    leaves the review rows unchanged ——
        assert_eq!(
            kernel.recover_compile_leases(9_999_999).unwrap(),
            RecoveryStats::default()
        );
        assert_eq!(kernel.list_reviews("milk-tea", None, 100).unwrap().len(), 2);
        let _ = jdir;
    }

    // A6/Step8 组合（executor 面）：仲裁 Some(1.0) → 五维门槛通过、页发布，
    // page_quality.consistency 精确为 1.0，无死信/冲突审核行。
    // A6/Step8 combination (executor surface): a Some(1.0) arbitration passes the
    // five-dim gates and publishes; page_quality.consistency is exactly 1.0 and
    // no dead-letter/conflict review rows exist.
    #[tokio::test]
    async fn consistency_pass_publishes_with_five_dim_value() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = consistency_policy();
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = wired_executor(kernel.clone(), mock, policy, passing_report());
        let stats = ex
            .run(&source, &ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.quarantined, 0);
        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].quality.consistency, Some(1.0));
        let _ = jdir;
    }

    // ===== Step8 批 B4：A11/A12（周期回收与心跳编排）=====
    // ===== Step8 batch B4: A11/A12 (periodic reaping and heartbeat orchestration)
    // =====

    /// 可推进注入时钟（A11/A12：claim → 手动推进 → reaper/心跳按注入时间动作，
    /// 不依赖真实睡眠）。
    /// A manually advanceable injected clock (A11/A12: claim → manual advance →
    /// reaper/heartbeat act on injected time with no real sleeping).
    impl FixedClock {
        fn set(&self, t: i64) {
            self.0.store(t, AtomicOrdering::SeqCst);
        }
    }

    /// 门控编译器：放行前挂起（模拟长模型请求；等待期间心跳由 executor 维持），
    /// 放行后委托 MockCompiler 的正常输出。
    /// A gated compiler: suspends until released (simulating a long model
    /// request; the heartbeat is maintained by the executor while waiting), then
    /// delegates to MockCompiler's normal output.
    struct GatedCompiler {
        inner: MockCompiler,
        gate: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait::async_trait]
    impl Compiler for GatedCompiler {
        async fn compile(
            &self,
            raw_entity: RawEntity,
            ctx: &CompileContext,
        ) -> Result<CompiledPage> {
            let mut gate = self.gate.clone();
            while !*gate.borrow() {
                gate.changed()
                    .await
                    .map_err(|_| Error::Internal("compile gate dropped".into()))?;
            }
            self.inner.compile(raw_entity, ctx).await
        }
    }

    /// 直接 admission + claim 造一个 running 租约（绕过完整 run；A11/A12 种子）。
    /// Seeds a running lease via direct admission + claim (bypassing a full run;
    /// the A11/A12 seed).
    fn seed_running_lease(
        kernel: &SqliteKernel,
        policy: &CompilePolicy,
        now: i64,
    ) -> (i64, TaskLease) {
        let prepared = prepare_source(&raw("boba", 1, 19.0), &schema(), policy).unwrap();
        let task_id = match kernel.admit_compile(&prepared, &ctx(), policy, &schema(), false) {
            Ok(Admission::Queued(id)) => id,
            other => panic!("expected admission Queued, got {other:?}"),
        };
        let lease = kernel
            .claim_compile(&[task_id], "run-seed", now)
            .unwrap()
            .expect("seed lease must claim");
        (task_id, lease)
    }

    /// 任务探针行（独立连接读，与 read_generation 同模式）。
    /// Task probe row (read over an independent connection, the read_generation
    /// pattern).
    #[derive(QueryableByName)]
    struct TaskProbeRow {
        #[diesel(sql_type = diesel::sql_types::Text)]
        status: String,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        retry_count: i64,
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        next_attempt_at: i64,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
        lease_token: Option<String>,
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
        lease_expires_at: Option<i64>,
    }

    /// 独立连接读任务行（busy timeout 等待内核写，不与 kernel 连接争抢）。
    /// Reads a task row over an independent connection (busy-timeout waits for
    /// kernel writes; no contention with the kernel connection).
    fn read_task(db_path: &std::path::Path, task_id: i64) -> TaskProbeRow {
        use diesel::prelude::*;
        let mut conn =
            diesel::sqlite::SqliteConnection::establish(db_path.to_str().expect("db path utf8"))
                .unwrap();
        diesel::connection::SimpleConnection::batch_execute(
            &mut conn,
            crate::schema::BUSY_TIMEOUT_PRAGMA_SQL,
        )
        .unwrap();
        diesel::sql_query(
            "SELECT status, retry_count, next_attempt_at, lease_token, lease_expires_at
             FROM compile_tasks WHERE task_id = ?",
        )
        .bind::<diesel::sql_types::BigInt, _>(task_id)
        .get_result(&mut conn)
        .unwrap()
    }

    /// 独立连接按实体键读租约到期时刻（A12 观察运行中任务的续租）。
    /// Reads a lease's expiry instant by entity key over an independent
    /// connection (A12 observes renewals of the run's own task).
    fn read_lease_expires(db_path: &std::path::Path, entity_key: &str) -> Option<i64> {
        use diesel::prelude::*;
        let mut conn =
            diesel::sqlite::SqliteConnection::establish(db_path.to_str().expect("db path utf8"))
                .unwrap();
        diesel::connection::SimpleConnection::batch_execute(
            &mut conn,
            crate::schema::BUSY_TIMEOUT_PRAGMA_SQL,
        )
        .unwrap();
        diesel::sql_query("SELECT lease_expires_at FROM compile_tasks WHERE entity_id = ?")
            .bind::<diesel::sql_types::Text, _>(entity_key)
            .get_result::<TaskExpiresRow>(&mut conn)
            .ok()
            .and_then(|r| r.lease_expires_at)
    }

    #[derive(QueryableByName)]
    struct TaskExpiresRow {
        #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::BigInt>)]
        lease_expires_at: Option<i64>,
    }

    /// 轮询等待注入时钟驱动的异步效果落库（5s 上限，防死等）。
    /// Polls until an injected-clock-driven async effect lands in the database
    /// (5s cap; guards against dead waits).
    async fn wait_for(mut predicate: impl FnMut() -> bool, label: &str) {
        for _ in 0..1000 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {label}");
    }

    // A11：不再启动新 compile run 的过期 running，被周期 reaper 在注入时间到期后
    // 回收为 pending（retry_count+1、退避、清 lease）；cancel + join 正常返回。
    // A11: an expired running task outside any new compile run is requeued as
    // pending by the periodic reaper once injected time passes expiry
    // (retry_count+1, backoff, lease cleared); cancel + join returns cleanly.
    #[tokio::test]
    async fn a11_reaper_recovers_expired_running_periodically() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let (task_id, _lease) = seed_running_lease(&kernel, &CompilePolicy::default(), 1000);

        let reaper = LeaseReaper::new(kernel.clone(), clock.clone(), Duration::from_millis(20));
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move { reaper.run_until_cancelled(token).await });

        // 注入时间越窗（1000+300 过期）→ 周期回收 → pending/退避。
        // Injected time crosses the window (expired at 1000+300) → periodic
        // recovery → pending/backoff.
        clock.set(1301);
        let db = db_path.clone();
        wait_for(
            move || read_task(&db, task_id).status == "pending",
            "reaper to requeue the expired task",
        )
        .await;
        let t = read_task(&db_path, task_id);
        assert_eq!(t.retry_count, 1);
        assert_eq!(t.next_attempt_at, 1302, "backoff(1)=1s");
        assert!(t.lease_token.is_none());
        assert_eq!(t.lease_expires_at, Some(0));

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    // A11：重试耗尽的 running 被周期 reaper 回收为 dead/failed，且死信审核行
    // 在同一事务入队。
    // A11: a running task with exhausted retries is recovered into dead/failed by
    // the periodic reaper, with its dead-letter review enqueued in the same
    // transaction.
    #[tokio::test]
    async fn a11_reaper_sends_retry_exhausted_to_dead() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let policy = CompilePolicy {
            max_retries: 1,
            ..CompilePolicy::default()
        };
        let (task_id, _lease) = seed_running_lease(&kernel, &policy, 1000);

        let reaper = LeaseReaper::new(kernel.clone(), clock.clone(), Duration::from_millis(20));
        let cancel = CancellationToken::new();
        let handle = {
            let cancel = cancel.clone();
            tokio::spawn(async move { reaper.run_until_cancelled(cancel).await })
        };

        clock.set(1301);
        let db = db_path.clone();
        wait_for(
            move || read_task(&db, task_id).status == "dead",
            "reaper to dead the retry-exhausted task",
        )
        .await;
        let t = read_task(&db_path, task_id);
        assert_eq!(t.retry_count, 1);
        assert_eq!(t.lease_expires_at, Some(0));
        // 死信审核行恰一条（subject {"task_id":N}，kernel 级断言见 A10）。
        // Exactly one dead-letter review row (subject {"task_id":N}; the
        // kernel-level assertions live in A10).
        let rows = kernel.list_reviews("milk-tea", None, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action, "compile_dead_letter");

        cancel.cancel();
        handle.await.unwrap().unwrap();
    }

    // A11：关闭时 drain 一次——长间隔（无周期 tick）下 cancel 后的最后一次同步
    // 回收仍把过期 running 收回 pending。
    // A11: drain once on shutdown — with a long interval (no periodic tick), the
    // final synchronous recovery after cancel still requeues the expired running
    // task.
    #[tokio::test]
    async fn a11_reaper_drains_once_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let (task_id, _lease) = seed_running_lease(&kernel, &CompilePolicy::default(), 1000);

        // 间隔远超测试时长 → 本次回收只能来自 cancel 后的 drain。
        // An interval far beyond the test duration → the only possible recovery
        // is the post-cancel drain.
        let reaper = LeaseReaper::new(kernel.clone(), clock.clone(), Duration::from_secs(3600));
        let cancel = CancellationToken::new();
        let handle = {
            let cancel = cancel.clone();
            tokio::spawn(async move { reaper.run_until_cancelled(cancel).await })
        };

        clock.set(1301);
        cancel.cancel();
        handle.await.unwrap().unwrap();

        let state = read_task(&db_path, task_id);
        assert_eq!(state.status, "pending");
        assert_eq!(state.retry_count, 1);
    }

    // A11：executor 接线（with_reaper_interval）——run 悬挂于门控模型期间，
    // 周期 reaper 仍按注入时间回收 run 外的过期任务；run 结束 cancel+join 收尾。
    // A11: the executor wiring (with_reaper_interval) — while the run hangs on
    // the gated model, the periodic reaper still recovers run-external expired
    // tasks on injected time; cancel+join finalizes at run end.
    #[tokio::test]
    async fn a11_reaper_runs_during_executor_run() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        // 种子：上一 run 遗留的 running（1000 领取，1300 过期；不属于本 run 的
        // admission 集合）。
        // Seed: a running task left by a previous run (claimed at 1000, expired at
        // 1300; outside this run's admission set).
        let (stale_id, _lease) = seed_running_lease(&kernel, &CompilePolicy::default(), 1000);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:latte", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let compiler = Arc::new(GatedCompiler {
            inner: MockCompiler::new(policy.clone()),
            gate: gate_rx,
        });
        let mut ex = executor_with_clock(kernel.clone(), compiler, policy, clock.clone());
        ex.heartbeat_interval_override = Some(Duration::from_millis(10));
        let ex = ex.with_reaper_interval(Duration::from_millis(20));

        // run 起始 recover 在 1200（种子任务 1300 才过期）→ 起始回收不会动它；
        // 门控期间推进到 1301 → 只能是周期 reaper 回收。
        // The run-start recover runs at 1200 (the seed expires only at 1300) → it
        // cannot touch the seed; advancing to 1301 while gated → only the periodic
        // reaper can recover it.
        clock.set(1200);
        let db = db_path.clone();
        let compile_ctx = ctx();
        let helper = async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            clock.set(1301);
            wait_for(
                move || read_task(&db, stale_id).status == "pending",
                "reaper to recover the stale task mid-run",
            )
            .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            gate_tx.send(true).unwrap();
        };
        let (stats, ()) =
            tokio::join!(ex.run(&source, &compile_ctx, RunOptions::default()), helper);
        let stats = stats.unwrap();
        assert_eq!(stats.accepted, 1, "this run's own task publishes");
        // 种子任务保持 pending（retry 1），join 后无残留 running。
        // The seed task stays pending (retry 1); no lingering running after join.
        let t = read_task(&db_path, stale_id);
        assert_eq!(t.status, "pending");
        assert_eq!(t.retry_count, 1);
        let _ = jdir;
    }

    // A12：fake model 等待越过 300s 租约窗口仍因心跳保持 lease——executor 在模型
    // 调用前启动心跳、注入时间越窗后续租（expires 恒未来），模型返回后 publish
    // 命中活租约（accepted 而非 stale/deferred）。
    // A12: a fake model waiting past the 300s lease window still keeps its lease
    // via heartbeats — the executor starts the heartbeat before the model call and
    // renews across the window (expires stays in the future); after the model
    // returns, publish hits a live lease (accepted, not stale/deferred).
    #[tokio::test]
    async fn a12_heartbeat_keeps_lease_alive_during_long_model_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);
        let policy = CompilePolicy::default();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let compiler = Arc::new(GatedCompiler {
            inner: MockCompiler::new(policy.clone()),
            gate: gate_rx,
        });
        let mut ex = executor_with_clock(kernel.clone(), compiler, policy, clock.clone());
        ex.heartbeat_interval_override = Some(Duration::from_millis(10));

        let db = db_path.clone();
        let compile_ctx = ctx();
        let helper = async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            // 模拟模型等待越过 300s 窗口：注入时钟按 200s 步进（< 300s 续租窗，
            // 心跳 SQL 要求 lease_expires_at > now——过期租约只能经 recover 回收，
            // 不能被复活），每步等待心跳把 expires 续到 now+300。
            // Simulate the model waiting past the 300s window: injected time moves
            // in 200s steps (< the 300s renewal window; the heartbeat SQL requires
            // lease_expires_at > now — an expired lease is only recoverable, never
            // revivable), and each step waits for the heartbeat to renew expires
            // to now+300.
            for step in 1..=3i64 {
                let now = 1000 + 200 * step;
                let renewed = now + 300;
                clock.set(now);
                let db = db.clone();
                wait_for(
                    move || read_lease_expires(&db, "milk-tea:drink:boba") == Some(renewed),
                    "heartbeat to renew the lease past the window",
                )
                .await;
            }
            // now=1600、expires=1900：模型总等待 600s > 300s 窗口，租约仍活。
            // now=1600, expires=1900: the model has waited 600s total (> the 300s
            // window) with the lease still alive.
            tokio::time::sleep(Duration::from_millis(20)).await;
            gate_tx.send(true).unwrap();
        };
        let (stats, ()) =
            tokio::join!(ex.run(&source, &compile_ctx, RunOptions::default()), helper);
        let stats = stats.unwrap();
        // 心跳使 publish 命中活租约：accepted=1、deferred=0（无心跳则
        // expires=1300 < now=1401 → Stale → deferred=1）。
        // The heartbeat lets publish hit a live lease: accepted=1, deferred=0
        // (without heartbeats expires=1300 < now=1401 → Stale → deferred=1).
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.deferred, 0);
        assert_eq!(stats.attempts, 1);
        assert_eq!(kernel.load_accepted_pages("milk-tea").unwrap().len(), 1);
        let _ = jdir;
    }

    // A12：取消后心跳停止——join 返回后推进注入时间，租约不再被续租，任务状态
    // 未被心跳改动。
    // A12: the heartbeat stops after cancellation — after the join returns and
    // injected time advances, the lease is no longer renewed and the task state is
    // untouched by the heartbeat.
    #[tokio::test]
    async fn a12_heartbeat_runner_stops_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let (task_id, lease) = seed_running_lease(&kernel, &CompilePolicy::default(), 1000);

        let runner = LeaseHeartbeat::new(
            kernel.clone(),
            clock.clone(),
            lease.clone(),
            Duration::from_millis(10),
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let join = tokio::spawn(async move { runner.run_until_cancelled(token).await });

        clock.set(1100);
        let db = db_path.clone();
        wait_for(
            move || read_lease_expires(&db, "milk-tea:drink:boba") == Some(1400),
            "heartbeat to renew at the advanced clock",
        )
        .await;
        cancel.cancel();
        join.await.unwrap().unwrap();

        // 停止后推进时间：不再续租（1400 不动），状态仍 running。
        // After stopping, advance time: no further renewal (1400 stays), status
        // still running.
        clock.set(1300);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            read_lease_expires(&db_path, "milk-tea:drink:boba"),
            Some(1400)
        );
        assert_eq!(read_task(&db_path, task_id).status, "running");
    }

    // A12：心跳 false（stale）即停止——过期租约的首次心跳返回 false 后运行器
    // 自行退出（无需 cancel），且不直接改任务状态。
    // A12: a false heartbeat (stale) stops the runner — on an expired lease the
    // first heartbeat returns false and the runner exits on its own (no cancel
    // needed) without mutating the task state.
    #[tokio::test]
    async fn a12_heartbeat_runner_stops_when_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, db_path) = file_db(&dir);
        let clock = Arc::new(FixedClock::new(1000));
        let (task_id, lease) = seed_running_lease(&kernel, &CompilePolicy::default(), 1000);

        // 租约已过期（1300 < 2000）→ 首次心跳 false → 运行器自行退出。
        // The lease is already expired (1300 < 2000) → the first heartbeat is
        // false → the runner exits on its own.
        clock.set(2000);
        let runner = LeaseHeartbeat::new(
            kernel.clone(),
            clock.clone(),
            lease.clone(),
            Duration::from_millis(10),
        );
        tokio::time::timeout(
            Duration::from_secs(5),
            runner.run_until_cancelled(CancellationToken::new()),
        )
        .await
        .expect("stale heartbeat must stop the runner")
        .unwrap();

        // 旧租约续约 false（fencing），任务状态未被心跳改动。
        // The stale lease cannot renew (fencing) and the task state is untouched.
        assert!(!kernel.heartbeat_compile(&lease, 2000).unwrap());
        assert_eq!(read_task(&db_path, task_id).status, "running");
    }

    // ===== Step8 批 B5（A15/A16）：首次 admission 前的兼容 preflight =====
    // ===== Step8 batch B5 (A15/A16): the compatibility preflight before the
    // first admission =====

    use crate::compile::config::CompatibilitySpec;
    use crate::types::{PageMetadata, PublishStatus, WikiPage};

    /// strict semver 版本的 ctx（带 schema/prompt 版本身份；executor 生产路径由
    /// CLI 在解析期校验后构造同一形状）。
    /// A strict-semver ctx (with the schema/prompt version identity; the
    /// production path builds the same shape after CLI parse-time validation).
    fn semver_ctx() -> CompileContext {
        build_context(
            "0.1.0",
            TEST_PROMPT,
            "mock-v1",
            "none",
            0.75,
            true,
            Some("2.1.0"),
            Some("3.0.0"),
        )
    }

    /// 带兼容矩阵的策略（domain_pack/schema/prompt 放行 0.0.1+；artifact 允许
    /// 列表单值；artifact_version 与列表一致）。
    /// A policy with a compatibility matrix (domain_pack/schema/prompt admit
    /// 0.0.1+; a single-entry artifact allowlist; artifact_version agrees with
    /// the list).
    fn compat_policy(artifact_version: &str) -> CompilePolicy {
        let spec: CompatibilitySpec = serde_yaml_ng::from_str(&format!(
            "domain_pack: \">=0.0.1\"\n\
             schema: \">=0.0.1\"\n\
             prompt: \">=0.0.1\"\n\
             artifact: [\"{artifact_version}\"]"
        ))
        .unwrap();
        CompilePolicy {
            artifact_version: artifact_version.into(),
            compatibility: Some(spec),
            ..CompilePolicy::default()
        }
    }

    /// 直插一页 legacy seed accepted 页（dpv 可控；artifact_version 列走
    /// 'seed-v1' 默认、frontmatter 无 quality_policy——矩阵下必然违规）。
    /// Plants a legacy seed accepted page directly (controllable dpv; the
    /// artifact_version column defaults to 'seed-v1' and the frontmatter lacks
    /// quality_policy — always a violation under a matrix).
    fn plant_seed_page(kernel: &SqliteKernel, domain_pack_version: &str) {
        let page = WikiPage {
            page_id: "seed-1".into(),
            entity_id: EntityId::new("milk-tea", "drink", "legacy").unwrap(),
            title: "legacy".into(),
            content: "legacy content".into(),
            sections: Vec::new(),
            metadata: PageMetadata {
                domain_pack_version: domain_pack_version.into(),
                compiled_at: 0,
                model_version: "mock-v1".into(),
                embedding_model: "none".into(),
            },
            aliases: Vec::new(),
            tags: Vec::new(),
        };
        kernel
            .seed_pages(&page, "milk-tea", PublishStatus::Accepted)
            .unwrap();
    }

    // A15：不兼容 → 整 run 以带稳定前缀的配置错误结束；无 facts/tasks 写入、
    // 无模型调用；同一事务产生一条 compatibility_conflict（A16），重复 run 不
    // 新增。
    // A15: incompatible → the run ends with a config error carrying the stable
    // prefix; no facts/tasks written, no model call; one compatibility_conflict
    // is produced in one transaction (A16) and repeated runs never add another.
    #[tokio::test]
    async fn a15_incompatible_preflight_refuses_admission_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, _db) = file_db(&dir);
        plant_seed_page(&kernel, "0.0.0");
        let policy = compat_policy("wiki-v1");
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);

        let err = ex
            .run(&source, &semver_ctx(), RunOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidConfig(msg) if msg.contains(COMPATIBILITY_REJECTED_PREFIX)),
            "expected the compatibility-rejection config error, got {err}"
        );
        let counts = kernel.row_counts().unwrap();
        assert_eq!(counts["compile_tasks"], 0, "no task may be created");
        assert_eq!(counts["facts"], 0, "no facts may be written");
        assert_eq!(counts["pages"], 1, "only the pre-seeded page remains");
        assert_eq!(mock.call_count(), 0, "no model call on rejection");
        assert_eq!(counts["review_queue"], 1, "one compatibility_conflict row");

        // 重复 run：告警幂等不新增（A16），仍拒绝 admission。
        // Repeated run: the alert is idempotent (A16) and admission stays
        // refused.
        let err2 = ex
            .run(&source, &semver_ctx(), RunOptions::default())
            .await
            .unwrap_err();
        assert!(
            matches!(&err2, Error::InvalidConfig(msg) if msg.contains(COMPATIBILITY_REJECTED_PREFIX))
        );
        assert_eq!(kernel.row_counts().unwrap()["review_queue"], 1);
        let _ = jdir;
    }

    // A15：兼容矩阵与既有产物一致 → preflight 通过、正常发布；重跑时已发布页
    // 全部满足矩阵（dpv/artifact/quality_policy）→ preflight 再次通过、按 hash
    // 幂等 skipped。证明 compile 在首次 admission 前调用的是同一 checker。
    // A15: a matrix consistent with existing artifacts → the preflight passes
    // and publishing proceeds; on rerun the published pages all satisfy the
    // matrix (dpv/artifact/quality_policy) → the preflight passes again and the
    // hash hit skips idempotently. Proves the compile path calls the same
    // checker before the first admission.
    #[tokio::test]
    async fn a15_compatible_preflight_publishes_then_rerun_skips() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, _db) = file_db(&dir);
        let policy = compat_policy("wiki-v1");
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);

        // 空库 + 矩阵：空 report 视为 compatible（A15）。
        // Empty database with a matrix: an empty report counts as compatible
        // (A15).
        let stats = ex
            .run(&source, &semver_ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats.accepted, 1, "empty DB passes the preflight");

        // 重跑：published 页满足矩阵，preflight 再次通过；hash 命中 → skipped。
        // Rerun: the published page satisfies the matrix, the preflight passes
        // again; the hash hit → skipped.
        let stats2 = ex
            .run(&source, &semver_ctx(), RunOptions::default())
            .await
            .unwrap();
        assert_eq!(stats2.scanned, 1);
        assert_eq!(stats2.skipped, 1);
        assert_eq!(stats2.accepted, 0);
        assert_eq!(mock.call_count(), 1, "no extra model call");
        let _ = jdir;
    }

    // A15/A20：dry-run 只读 —— 不兼容时同样拒绝（fail-closed）但绝不写审核、
    // 不写任何行。
    // A15/A20: a dry-run is read-only — incompatibility is still refused
    // (fail-closed) but never writes a review or any row.
    #[tokio::test]
    async fn a15_dry_run_rejects_incompatibility_without_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, _db) = file_db(&dir);
        plant_seed_page(&kernel, "0.0.0");
        let policy = compat_policy("wiki-v1");
        let mock = Arc::new(MockCompiler::new(policy.clone()));
        let ex = executor(kernel.clone(), mock.clone(), policy);
        let (jdir, jpath) = write_jsonl(&[jsonl_line("milk-tea:drink:boba", 1, 19.0)]);
        let source = jsonl_source(&jpath);

        let before = kernel.row_counts().unwrap();
        let err = ex
            .run(
                &source,
                &semver_ctx(),
                RunOptions {
                    dry_run: true,
                    ..RunOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidConfig(msg) if msg.contains(COMPATIBILITY_REJECTED_PREFIX))
        );
        assert_eq!(
            kernel.row_counts().unwrap(),
            before,
            "dry-run writes nothing"
        );
        assert_eq!(mock.call_count(), 0);
        let _ = jdir;
    }
}
