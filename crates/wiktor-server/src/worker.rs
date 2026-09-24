//! Step7 B4 常驻编译 worker（spec step7 §3 D4/D5、§6.2；STEP7-002）：消费
//! compile_tasks 的 pending 队列——启动时回收一次过期租约，之后周期
//! claim + 处理，复用 core `PipelineExecutor::process_claimed_task` 窄接口
//! （compile → validate → score → publish/failure + 心跳/fencing + 预算/刹车/
//! 死信全在 core 内部，本模块**不复制任何发布/失败 SQL**）。随 cancel 退出，
//! 退出前 drain 一次。
//! Step7 B4 resident compile worker (spec step7 §3 D4/D5, §6.2; STEP7-002):
//! consumes the compile_tasks pending queue — one lease recovery at startup,
//! then periodic claim + process, reusing the core
//! `PipelineExecutor::process_claimed_task` narrow interface (compile →
//! validate → score → publish/failure + heartbeat/fencing +
//! budget/brake/dead-letter all live inside core; this module **copies no
//! publish/failure SQL**). Exits with the cancel token, draining once first.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wiktor_core::compile::config::{Clock, SystemClock};
use wiktor_core::compile::contract::DefaultSourceRefValidator;
use wiktor_core::compile::executor::{LeaseOutcome, PipelineExecutor};
use wiktor_core::compile::mock::MockCompiler;
use wiktor_core::compile::quality::RuleBasedScorer;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::EntitySchema;

use crate::services::compile::{assemble_source, AssembledSource};

/// 默认轮询间隔（无到期任务时睡眠，不忙等）。
/// Default poll interval (sleep when nothing is due — never busy-wait).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// worker 每次最多 claim 的任务数上限（WIKTOR_COMPILE_WORKERS 1..=8）。
/// Max tasks a worker may claim per pass (WIKTOR_COMPILE_WORKERS is 1..=8).
pub const MAX_WORKERS: usize = 8;
/// worker 使用的 run_id 前缀（claim 记账用；同一 worker 生命周期内轮次共享）。
/// The run_id prefix the worker claims under (bookkeeping only; shared across
/// passes within one worker lifetime).
pub const WORKER_RUN_PREFIX: &str = "server-worker";

/// 常驻编译 worker（B4）。`schema` 与 Admit 同一域的 EntitySchema（worker 不
/// 持有 DataSource，schema 在装配时构造并缓存）。
/// The resident compile worker (B4). `schema` is the EntitySchema of the same
/// domain as Admit (the worker holds no DataSource; the schema is built and
/// cached at assembly time).
#[derive(Clone)]
pub struct CompileWorker {
    kernel: Arc<SqliteKernel>,
    executor: Arc<PipelineExecutor>,
    schema: EntitySchema,
    clock: Arc<dyn Clock>,
    poll_interval: Duration,
    concurrency: usize,
}

impl CompileWorker {
    /// 装配 worker（concurrency = WIKTOR_COMPILE_WORKERS，1..=8，缺省 1）。
    /// Assembles the worker (concurrency = WIKTOR_COMPILE_WORKERS, 1..=8,
    /// default 1).
    pub fn new(
        kernel: Arc<SqliteKernel>,
        executor: Arc<PipelineExecutor>,
        schema: EntitySchema,
    ) -> Self {
        let workers = std::env::var("WIKTOR_COMPILE_WORKERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, MAX_WORKERS);
        Self {
            kernel,
            executor,
            schema,
            clock: Arc::new(SystemClock),
            poll_interval: DEFAULT_POLL_INTERVAL,
            concurrency: workers,
        }
    }

    /// 与 Compile.Admit **同源装配**（STEP7-002）：从同一个 domain.yaml 装配
    /// policy/schema/ctx，使 worker 处理任务时重算的 content_hash 与 admit 时的
    /// desired_hash 一致（publish fencing 通过）。compiler 用 MockCompiler
    /// （B4 离线；真实 LLM 装配留 serve 装配层按 feature 切换）。
    /// Assembled from the **same source as Compile.Admit** (STEP7-002): the same
    /// domain.yaml yields the same policy/schema/ctx, so the worker's recomputed
    /// content_hash matches the admit-time desired_hash (publish fencing passes).
    /// The compiler is a MockCompiler (B4 offline; real-LLM assembly stays at
    /// the serve assembly layer, feature-switched).
    pub fn from_domain_pack(
        kernel: Arc<SqliteKernel>,
        domain_pack_path: &str,
        source_path: Option<&str>,
        domain_pack_version: Option<&str>,
        options_json: &str,
    ) -> Result<Self, String> {
        let AssembledSource { policy, schema, .. } = assemble_source(
            domain_pack_path,
            source_path,
            domain_pack_version,
            options_json,
        )?;
        let executor = Arc::new(PipelineExecutor::new(
            kernel.clone(),
            Arc::new(MockCompiler::new(policy.clone())),
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            Arc::new(SystemClock),
            policy,
        ));
        Ok(Self::new(kernel, executor, schema))
    }

    /// 启动 worker 任务（回收一次 → 常驻循环），随 cancel 退出并 drain。
    /// Spawns the worker task (one recovery → resident loop), exiting with the
    /// cancel token after a final drain.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let now = self.clock.unix_seconds();
            match self.kernel.recover_compile_leases(now) {
                Ok(s) => tracing::info!(
                    recovered = s.recovered_pending,
                    dead = s.dead_failed,
                    "worker startup lease recovery"
                ),
                Err(e) => tracing::warn!(error = %e, "worker startup lease recovery failed"),
            }
            self.run_loop(cancel).await;
        })
    }

    async fn run_loop(self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            let now = self.clock.unix_seconds();
            let due = match self
                .kernel
                .list_due_compile_task_ids(now, self.concurrency as u32)
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(error = %e, "worker list_due failed");
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(self.poll_interval) => {}
                    }
                    continue;
                }
            };
            if due.is_empty() {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(self.poll_interval) => {}
                }
                continue;
            }
            let kernel = self.kernel.clone();
            let ids = due.clone();
            let run_id = format!("{WORKER_RUN_PREFIX}-{now}");
            let lease =
                match tokio::task::spawn_blocking(move || kernel.claim_compile(&ids, &run_id, now))
                    .await
                {
                    Ok(Ok(Some(lease))) => lease,
                    Ok(Ok(None)) => {
                        // 本轮无可用租约（预算熔断或全部未到期）→ 退避。
                        // No claimable lease this pass (budget circuit or nothing
                        // due) → back off.
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(self.poll_interval) => {}
                        }
                        continue;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "worker claim failed");
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(self.poll_interval) => {}
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "worker claim task join failed");
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(self.poll_interval) => {}
                        }
                        continue;
                    }
                };
            let schema = self.schema.clone();
            match self.executor.process_claimed_task(lease, &schema).await {
                Ok(LeaseOutcome::Finalized) => {}
                Ok(LeaseOutcome::RetryAt(_)) => {
                    // 失败任务已由 finish_compile_failure 置 next_attempt_at；
                    // 下一轮 list_due 自然拾回。本处无需额外动作。
                    // A failed task already got its next_attempt_at from
                    // finish_compile_failure; the next list_due pass naturally
                    // picks it back up — nothing extra needed here.
                }
                Err(e) => tracing::warn!(error = %e, "worker task processing failed"),
            }
        }
    }
}
