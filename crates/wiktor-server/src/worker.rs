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
use wiktor_core::compile::config::{Clock, CompilePolicy, SystemClock};
use wiktor_core::compile::consistency::{
    ConsistencyArbiter, SourceRefConsistencyArbiter, SqliteFtsCandidateProvider,
};
use wiktor_core::compile::contract::DefaultSourceRefValidator;
use wiktor_core::compile::executor::{LeaseOutcome, PipelineExecutor};
use wiktor_core::compile::mock::MockCompiler;
use wiktor_core::compile::quality::RuleBasedScorer;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::{Compiler, EntitySchema};

use crate::services::compile::{assemble_source, AssembledSource};

/// 默认轮询间隔（无到期任务时睡眠，不忙等）。
/// Default poll interval (sleep when nothing is due — never busy-wait).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// serve 真实 LLM 编译缺省模型（Step0b；被 `WIKTOR_LLM_MODEL` 覆盖）。
/// Default model for serve real-LLM compile (Step0b; overridden by
/// `WIKTOR_LLM_MODEL`).
const DEFAULT_LLM_MODEL: &str = "qwen3.8-max";
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
        let consistency_enabled = policy.consistency.enabled;
        let mut executor = PipelineExecutor::new(
            kernel.clone(),
            Self::build_server_compiler(policy.clone())?,
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            Arc::new(SystemClock),
            policy,
        );
        // Step14 P3-A：consistency 装配与 CLI 同源（env 驱动仲裁器 + SQLite FTS
        // 有界候选提供器）；关闭时走 None 路径（不仲裁）。
        // Step14 P3-A: consistency wiring is assembler-source-identical to the
        // CLI (env-driven arbiter + bounded SQLite FTS candidate provider); with
        // consistency disabled the None path is kept (no arbitration).
        if consistency_enabled {
            executor = executor
                .with_consistency_arbiter(Self::build_server_consistency_arbiter())
                .with_candidate_provider(Arc::new(SqliteFtsCandidateProvider::new(kernel.clone())));
        }
        Ok(Self::new(kernel, Arc::new(executor), schema))
    }

    /// Step14 P3-A：按 env 装配常驻 worker 的一致性仲裁器（与 CLI 同源逻辑）。
    /// `WIKTOR_CONSISTENCY_LLM=1` 且设了非空 `WIKTOR_LLM_BASE_URL` 时用
    /// `LlmConsistencyArbiter`（模型取 `WIKTOR_LLM_MODEL`，缺省
    /// [`DEFAULT_LLM_MODEL`]）；否则回退确定性 `SourceRefConsistencyArbiter`
    /// （离线基线）。一致性仲裁是新增面，显式 opt-in 而非设 URL 即启用。
    /// Step14 P3-A: assembles the resident worker's consistency arbiter per env
    /// (the same logic as the CLI). With `WIKTOR_CONSISTENCY_LLM=1` AND a non-empty
    /// `WIKTOR_LLM_BASE_URL` it uses `LlmConsistencyArbiter` (model from
    /// `WIKTOR_LLM_MODEL`, defaulting to [`DEFAULT_LLM_MODEL`]); otherwise it
    /// falls back to the deterministic `SourceRefConsistencyArbiter` (the offline
    /// baseline). Consistency arbitration is a new surface and opts in
    /// explicitly rather than enabling on URL presence.
    fn build_server_consistency_arbiter() -> Arc<dyn ConsistencyArbiter> {
        let opt_in = std::env::var("WIKTOR_CONSISTENCY_LLM")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let base_url = std::env::var("WIKTOR_LLM_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty());
        if opt_in {
            if let Some(url) = base_url {
                #[cfg(feature = "llm-openai")]
                {
                    use wiktor_core::compile::consistency::LlmConsistencyArbiter;
                    use wiktor_core::compile::llm::{OpenAiLlmClient, API_KEY_ENV};
                    let model = std::env::var("WIKTOR_LLM_MODEL")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string());
                    let api_key = std::env::var(API_KEY_ENV)
                        .ok()
                        .filter(|k| !k.trim().is_empty());
                    if let Ok(client) =
                        OpenAiLlmClient::new(model.clone(), Some(url), api_key, false)
                    {
                        return Arc::new(LlmConsistencyArbiter::new(Arc::new(client), model, 512));
                    }
                    eprintln!(
                        "wiktor: consistency LLM arbiter construction failed; \
                         falling back to deterministic"
                    );
                }
                #[cfg(not(feature = "llm-openai"))]
                {
                    eprintln!(
                        "wiktor: consistency LLM arbiter requires the `llm-openai` feature; \
                         falling back to deterministic"
                    );
                }
            }
        }
        Arc::new(SourceRefConsistencyArbiter::new())
    }

    /// 按 env 装配常驻 worker 的编译器（Step0b）：设了非空
    /// `WIKTOR_LLM_BASE_URL` 时组装 OpenAI 兼容 `LlmCompiler`（模型取
    /// `WIKTOR_LLM_MODEL`，缺省 [`DEFAULT_LLM_MODEL`]；key 只从
    /// `WIKTOR_OPENAI_API_KEY` 读取——缺 key 是配置错误，**不降级 Mock**，D7）。
    /// env 未设 → 回退 `MockCompiler`（离线基线）。gRPC `CompileService` 的
    /// Mock 不受影响（不在本函数范围）。
    /// Assembles the resident worker's compiler per env (Step0b): with a non-empty
    /// `WIKTOR_LLM_BASE_URL` it wires an OpenAI-compatible `LlmCompiler` (model
    /// from `WIKTOR_LLM_MODEL`, defaulting to [`DEFAULT_LLM_MODEL`]; the key comes
    /// only from `WIKTOR_OPENAI_API_KEY` — a missing key is a config error and
    /// **never degrades to Mock**, D7). With the env unset it falls back to
    /// `MockCompiler` (the offline baseline). The gRPC `CompileService`'s Mock is
    /// unaffected (out of scope here).
    fn build_server_compiler(policy: CompilePolicy) -> Result<Arc<dyn Compiler>, String> {
        Self::assemble_server_compiler(
            policy,
            std::env::var("WIKTOR_LLM_BASE_URL").ok(),
            std::env::var("WIKTOR_LLM_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            std::env::var("WIKTOR_OPENAI_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty()),
        )
    }

    /// `build_server_compiler` 的纯函数内核（参数显式传入，便于离线单测）：
    /// `llm_base_url` 非空 → LlmCompiler（`llm_api_key` 缺失则报错）；否则 Mock。
    /// The pure core of `build_server_compiler` (params passed explicitly for
    /// offline unit tests): a non-empty `llm_base_url` → `LlmCompiler` (an
    /// `llm_api_key` missing is a hard error); otherwise `MockCompiler`.
    fn assemble_server_compiler(
        policy: CompilePolicy,
        llm_base_url: Option<String>,
        llm_model: Option<String>,
        llm_api_key: Option<String>,
    ) -> Result<Arc<dyn Compiler>, String> {
        match llm_base_url {
            Some(url) if !url.trim().is_empty() => {
                #[cfg(feature = "llm-openai")]
                {
                    use wiktor_core::compile::llm::{LlmCompiler, OpenAiLlmClient, API_KEY_ENV};
                    match llm_api_key {
                        None => Err(format!(
                            "serve real-LLM compile requires {API_KEY_ENV} (set WIKTOR_LLM_BASE_URL to opt in)"
                        )),
                        Some(key) => {
                            let model = llm_model.unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string());
                            let client = OpenAiLlmClient::new(model, Some(url), Some(key), false)
                                .map_err(|e| e.to_string())?;
                            Ok(Arc::new(LlmCompiler {
                                client: Arc::new(client),
                                policy,
                            }))
                        }
                    }
                }
                #[cfg(not(feature = "llm-openai"))]
                {
                    Err("serve real-LLM compile requires the `llm-openai` feature \
                         (rebuild with default features)"
                        .to_string())
                }
            }
            _ => Ok(Arc::new(MockCompiler::new(policy))),
        }
    }

    /// 启动 worker 任务（启动时回收一次 + spawn 周期 LeaseReaper → 常驻消费
    /// 循环），随 cancel 退出并 drain（reaper cancel + join）。
    /// Spawns the worker task (one startup recovery + a periodic LeaseReaper →
    /// the resident consume loop), exiting with the cancel token after a final
    /// drain (the reaper is cancelled and joined).
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
            // 周期 reaper（spec step7 §6.2 / Step8 §6.3 D8）：崩溃/失败遗留的
            // running 租约必须周期回收，否则永久卡死。cancel + join 与
            // executor::run 同一模式。
            // Periodic reaper (spec step7 §6.2 / Step8 §6.3 D8): running leases
            // orphaned by crashes/failures must be recovered periodically or they
            // wedge forever. Cancel + join, the same pattern as executor::run.
            let reaper_cancel = CancellationToken::new();
            let reaper = wiktor_core::compile::lease::LeaseReaper::new(
                self.kernel.clone(),
                self.clock.clone(),
                std::time::Duration::from_secs(30),
            );
            let reaper_handle = {
                let reaper = reaper.clone();
                let cancel = reaper_cancel.clone();
                tokio::spawn(async move { reaper.run_until_cancelled(cancel).await })
            };
            self.run_loop(cancel).await;
            reaper_cancel.cancel();
            match reaper_handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "worker lease reaper ended with error"),
                Err(e) => tracing::warn!(error = %e, "worker lease reaper panicked"),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 未设 LLM base_url → Mock 回退（不报错，离线）。
    /// No LLM base_url → Mock fallback (no error, offline).
    #[test]
    fn no_llm_base_url_falls_back_to_mock() {
        let c = CompileWorker::assemble_server_compiler(CompilePolicy::default(), None, None, None)
            .expect("mock fallback must succeed");
        let _ = c; // Arc<dyn Compiler>；路径本身即验收（不降级到 Err）。
    }

    /// 空（空白）base_url → Mock 回退。
    /// A blank base_url → Mock fallback.
    #[test]
    fn blank_llm_base_url_falls_back_to_mock() {
        CompileWorker::assemble_server_compiler(
            CompilePolicy::default(),
            Some("   ".to_string()),
            None,
            None,
        )
        .expect("blank base_url must fall back to mock");
    }

    /// 设了 base_url 但缺 key → 配置错误（D7，绝不降级 Mock）。
    /// A base_url set but no key → config error (D7, never a Mock downgrade).
    #[test]
    fn llm_base_url_without_key_errors() {
        let err = match CompileWorker::assemble_server_compiler(
            CompilePolicy::default(),
            Some("http://llm.example/v1".to_string()),
            None,
            None,
        ) {
            Err(e) => e,
            Ok(_) => panic!("missing key must be a hard error, got a compiler"),
        };
        assert!(
            err.contains("WIKTOR_OPENAI_API_KEY"),
            "error should name the missing key env, got: {err}"
        );
    }
}
