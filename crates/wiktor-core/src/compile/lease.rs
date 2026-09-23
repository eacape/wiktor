//! 周期租约回收与心跳保活（Step8 spec §6.3/§8，决策 D8/D9，验收 A11/A12）。
//! Periodic lease reaping and heartbeat keep-alive (Step8 spec §6.3/§8,
//! decisions D8/D9, acceptance A11/A12).
//!
//! 契约要点：
//! - [`LeaseReaper`] 是库级周期回收器：每个 interval 做一次同步
//!   `recover_compile_leases`（spawn_blocking 包裹，不持锁跨 await）；cancel 后
//!   **drain 一次**再返回 Ok（D8：关闭时 drain 一次）。executor 只在显式接线
//!   （`with_lease_reaper`）时启动它；run 开始的一次性同步 recover 语义不变。
//! - 心跳任务（D9）为**短单语句写事务**的循环调用方：每 interval 调一次
//!   `heartbeat_compile`，返回 false（stale）即退出；错误也退出——stale 只代表
//!   不再续租，任务的最终归属仍由 publish/failure 的 kernel fencing CAS 裁决。
//! - 两个运行器都不做 SQLite 长事务、不持 guard 跨 await：kernel 方法是同步
//!   的，一律经 [`super::executor::blocking`]（spawn_blocking）调用。
//!
//! Contract highlights:
//! - [`LeaseReaper`] is a library-level periodic reaper: one synchronous
//!   `recover_compile_leases` per interval (wrapped in spawn_blocking, never a
//!   lock across await); after cancel it **drains once** before returning Ok
//!   (D8: drain once on shutdown). The executor starts it only when explicitly
//!   wired (`with_lease_reaper`); the run-start one-shot recover is unchanged.
//! - The heartbeat task (D9) loops over **short single-statement write
//!   transactions**: one `heartbeat_compile` call per interval, exiting on false
//!   (stale) and on errors — stale only means "no more renewals"; the task's
//!   final owner is still arbitrated by the publish/failure kernel fencing CAS.
//! - Neither runner holds a long SQLite transaction nor a guard across await:
//!   kernel methods are synchronous and always invoked via
//!   [`super::executor::blocking`] (spawn_blocking).

use crate::compile::config::Clock;
use crate::compile::config::TaskLease;
use crate::compile::executor::blocking;
use crate::kernel::compile_store::RecoveryStats;
use crate::kernel::SqliteKernel;
use crate::types::error::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 周期租约回收器（Step8 §6.3/D8）：对 [`SqliteKernel::recover_compile_leases`]
/// 的可取消周期编排。字段为 spec §6.3 逐字形状（kernel/clock/interval）。
/// The periodic lease reaper (Step8 §6.3/D8): a cancellable periodic orchestration
/// of [`SqliteKernel::recover_compile_leases`]. The fields match spec §6.3
/// verbatim (kernel/clock/interval).
#[derive(Clone)]
pub struct LeaseReaper {
    /// 回收所用的内核（同一 SQLite 单连接，CLI 与后台 reaper 共用）。
    /// The kernel used for recovery (the same single SQLite connection shared by
    /// the CLI and the background reaper).
    pub kernel: Arc<SqliteKernel>,
    /// 回收时刻的时钟（验收用注入 Clock）。
    /// The clock for recovery instants (an injected Clock for acceptance tests).
    pub clock: Arc<dyn Clock>,
    /// 相邻两次回收的间隔（D8 默认 30 秒；来自策略
    /// `lease_reaper_interval_seconds`，不入 content_hash）。
    /// The interval between consecutive recovery passes (D8 default 30s; sourced
    /// from the policy `lease_reaper_interval_seconds`, excluded from the
    /// content_hash).
    pub interval: Duration,
}

impl LeaseReaper {
    /// 组装回收器（字段即 spec §6.3 形状）。
    /// Assembles the reaper (the fields are the spec §6.3 shape).
    pub fn new(kernel: Arc<SqliteKernel>, clock: Arc<dyn Clock>, interval: Duration) -> Self {
        Self {
            kernel,
            clock,
            interval,
        }
    }

    /// 周期回收主循环（§6.3）：先等待一个 interval 再回收（run 开始的一次性
    /// recover 仍由 executor 同步执行，不在此重复）；每个 interval 一次同步
    /// recover；cancel 后 **drain 一次**再返回 Ok（D8/A11）。回收错误（数据库
    /// 故障）向上传播并终止循环——调用方（executor/CLI）负责 join 并记录。
    /// The periodic reaping loop (§6.3): waits one interval before the first pass
    /// (the run-start one-shot recover stays a synchronous executor duty and is
    /// not duplicated here); one synchronous recover per interval; after cancel
    /// it **drains once** before returning Ok (D8/A11). Recovery errors (database
    /// faults) propagate and end the loop — the caller (executor/CLI) joins and
    /// logs them.
    pub async fn run_until_cancelled(&self, cancel: CancellationToken) -> Result<()> {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // D8：关闭时 drain 一次——最后一次同步回收后再返回。
                    // D8: drain once on shutdown — one final synchronous recover
                    // before returning.
                    let stats = self.recover_once().await?;
                    if stats.recovered_tasks() > 0 {
                        tracing::info!(?stats, "lease reaper drained on shutdown");
                    }
                    return Ok(());
                }
                _ = tokio::time::sleep(self.interval) => {
                    let stats = self.recover_once().await?;
                    if stats.recovered_tasks() > 0 {
                        tracing::debug!(?stats, "lease reaper pass");
                    }
                }
            }
        }
    }

    /// 单次回收（spawn_blocking 包裹的同步 kernel 事务，锁不跨 await）。
    /// One recovery pass (the synchronous kernel transaction wrapped in
    /// spawn_blocking; the lock never crosses an await).
    async fn recover_once(&self) -> Result<RecoveryStats> {
        let kernel = self.kernel.clone();
        let now = self.clock.unix_seconds();
        blocking(move || kernel.recover_compile_leases(now)).await
    }
}

/// 心跳间隔（D9）：`lease_seconds/2`，最小 1 秒（租约窗口 300s → 默认 150s 续租）。
/// The heartbeat interval (D9): `lease_seconds/2`, minimum 1s (a 300s lease
/// window → the default 150s renewal).
pub(crate) fn lease_heartbeat_interval(lease_seconds: u32) -> Duration {
    Duration::from_secs(u64::from(lease_seconds / 2).max(1))
}

/// 心跳任务运行器（D9/A12）：租约的循环续租器——每 [`Self::interval`] 调一次
/// `heartbeat_compile`（短单语句写事务，spawn_blocking）；false（stale）或错误
/// 即停止。不做任何其他任务状态写入——fencing 归 publish/failure CAS。
/// The heartbeat-task runner (D9/A12): a looping lease renewer — one
/// `heartbeat_compile` call per [`Self::interval`] (a short single-statement
/// write transaction via spawn_blocking); stops on false (stale) or on error.
/// It never writes any other task state — fencing belongs to the
/// publish/failure CAS.
pub(crate) struct LeaseHeartbeat {
    kernel: Arc<SqliteKernel>,
    clock: Arc<dyn Clock>,
    lease: TaskLease,
    interval: Duration,
}

impl LeaseHeartbeat {
    /// 组装心跳运行器。
    /// Assembles the heartbeat runner.
    pub(crate) fn new(
        kernel: Arc<SqliteKernel>,
        clock: Arc<dyn Clock>,
        lease: TaskLease,
        interval: Duration,
    ) -> Self {
        Self {
            kernel,
            clock,
            lease,
            interval,
        }
    }

    /// 心跳主循环（D9）：select 在 cancel 与 interval 睡眠之间；每次醒来做一次
    /// 心跳。false（stale）→ Ok 退出；kernel 错误 → Err 退出（仍允许模型返回，
    /// 发布由 fencing CAS 裁决——§6.3）。cancel 命中时立即返回；若 cancel 与
    /// 一次心跳并发，本次心跳完成后的下一轮循环立即退出（多续租一次无害，
    /// 此时租约仍属本 worker）。
    /// The heartbeat loop (D9): selects between cancel and the interval sleep;
    /// each wake performs one heartbeat. false (stale) → exit with Ok; a kernel
    /// error → exit with Err (the model return is still allowed and publish stays
    /// arbitrated by the fencing CAS — §6.3). Cancel takes effect immediately;
    /// when cancel races one in-flight heartbeat, the next loop iteration exits
    /// right after it completes (one extra renewal is harmless — the lease still
    /// belongs to this worker at that point).
    pub(crate) async fn run_until_cancelled(&self, cancel: CancellationToken) -> Result<()> {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(self.interval) => {
                    let kernel = self.kernel.clone();
                    let lease = self.lease.clone();
                    let now = self.clock.unix_seconds();
                    if !blocking(move || kernel.heartbeat_compile(&lease, now)).await? {
                        tracing::debug!(
                            task_id = self.lease.task_id,
                            epoch = self.lease.epoch,
                            "heartbeat stale; stopping renewals"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// 心跳任务句柄（D9）：executor 在模型返回后 `stop()`——先 cancel 再 join，
/// 保证 join 完成后才进入 publish/failure。
/// The heartbeat-task handle (D9): the executor calls `stop()` once the model
/// returns — cancel first, then join; publish/failure proceed only after the
/// join completes.
pub(crate) struct HeartbeatHandle {
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl HeartbeatHandle {
    /// 启动心跳任务（模型调用前调用；D9）。runner 移入 spawn 的 future
    /// （'static）。
    /// Starts the heartbeat task (called before the model request; D9). The
    /// runner is moved into the spawned future ('static).
    pub(crate) fn spawn(runner: LeaseHeartbeat) -> Self {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let join = tokio::spawn(async move { runner.run_until_cancelled(task_cancel).await });
        Self { cancel, join }
    }

    /// 停止并等待心跳结束（D9：先 cancel 再 join）。join 结果仅记录——心跳
    /// 失败不改变 run 的结果面，publish/failure 的 fencing CAS 是最终裁决。
    /// Stops and awaits the heartbeat (D9: cancel first, then join). The join
    /// result is only logged — a heartbeat failure never alters the run's
    /// outcome surface; the publish/failure fencing CAS is the final arbiter.
    pub(crate) async fn stop(self) {
        self.cancel.cancel();
        match self.join.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "heartbeat task ended with an error");
            }
            Err(e) => {
                tracing::warn!(error = %e, "heartbeat task panicked");
            }
        }
    }
}
