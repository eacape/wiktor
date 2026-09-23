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
//! - The Compiler's quality/content_hash/metadata/sections are untrusted
//!   (§4/§5.2.4/§5.2.5): the executor recomputes them via the canonical
//!   renderer + shared splitter + content_hash() before publish.

use crate::compile::config::{
    prepare_source, Admission, Clock, CommitOutcome, CompilePolicy, CompileStats,
    FailureDisposition, RunOptions, TaskLease,
};
use crate::compile::contract::{
    render_canonical_markdown, CompileFailure, RefReport, SourceRefValidator,
};
use crate::compile::hash::{content_hash, HashDependencies};
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

/// PipelineExecutor（§3 契约字段）。
/// PipelineExecutor (§3 contract fields).
pub struct PipelineExecutor {
    pub kernel: Arc<SqliteKernel>,
    pub compiler: Arc<dyn Compiler>,
    pub scorer: Arc<dyn RuleScorer>,
    pub validator: Arc<dyn SourceRefValidator>,
    pub clock: Arc<dyn Clock>,
    pub policy: CompilePolicy,
}

/// 单个租约的调度结果：终态（已计数）或退避重试（留在队列）。
/// Scheduling outcome of one lease: finalized (already counted) or retrying
/// with backoff (stays queued).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseOutcome {
    Finalized,
    RetryAt(i64),
}

impl PipelineExecutor {
    /// 组装执行器（字段与 §3 契约一致）。
    /// Assembles the executor (fields match the §3 contract).
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
        }
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

    /// 真实 run：逐批 admission + 调度（§3/§8）。
    /// Real run: batch-wise admission + scheduling (§3/§8).
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
        let recovered = blocking(move || kernel.recover_compile_leases(now)).await?;
        if recovered > 0 {
            tracing::info!(recovered, "recovered expired compile leases");
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        let mut remaining = options.limit;
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

    /// 处理一个租约：compile → validator → scorer → publish / finish_failure。
    /// Processes one lease: compile → validator → scorer → publish /
    /// finish_failure.
    async fn process_lease(
        &self,
        lease: &TaskLease,
        schema: &EntitySchema,
        stats: &mut CompileStats,
    ) -> Result<LeaseOutcome> {
        // 每次模型请求计一次 attempt（含重编译；§3）。
        // One attempt per model request, recompiles included (§3).
        stats.attempts += 1;
        let compile_result = self
            .compiler
            .compile(lease.source.clone(), &lease.context)
            .await;
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
                self.fail_lease(lease, &failure, None, &report, stats).await
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
                        return self.fail_lease(lease, &failure, None, &report, stats).await;
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
                // Compiler 的占位 quality 不能短路评分。
                // §6: the executor runs validator + scorer after every Compiler;
                // the compiler's placeholder quality never short-circuits scoring.
                let report = self.scorer.score(
                    &lease.source,
                    Some(&page),
                    &refs,
                    true,
                    &lease.context,
                    &self.policy,
                );
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
                                    .fail_lease(lease, &failure, None, &report, stats)
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
                    match blocking(move || {
                        kernel.publish_compile(&lease_clone, &page_clone, &report_clone, now)
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
                    self.fail_lease(lease, &failure, Some(&page), &report, stats)
                        .await
                }
            }
        }
    }

    /// 失败处置事务（§8.3）：RetryAt → 留队等待；Quarantined/Failed → 终态计数。
    /// The failure-disposition transaction (§8.3): RetryAt → stay queued;
    /// Quarantined/Failed → terminal counting.
    async fn fail_lease(
        &self,
        lease: &TaskLease,
        failure: &CompileFailure,
        candidate: Option<&CompiledPage>,
        report: &ScoreReport,
        stats: &mut CompileStats,
    ) -> Result<LeaseOutcome> {
        let now = self.clock.unix_seconds();
        let kernel = self.kernel.clone();
        let lease_clone = lease.clone();
        let failure_clone = failure.clone();
        let candidate_clone = candidate.cloned();
        let report_clone = report.clone();
        match blocking(move || {
            kernel.finish_compile_failure(
                &lease_clone,
                &failure_clone,
                candidate_clone.as_ref(),
                &report_clone,
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
/// spawn_blocking wrapper: panics map to `Error::Internal` (no new unwraps).
async fn blocking<T, F>(f: F) -> Result<T>
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
        build_context("test-v1", TEST_PROMPT, "mock-v1", "none", 0.75, true)
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
}
