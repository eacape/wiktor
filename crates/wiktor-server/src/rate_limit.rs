//! 进程内固定窗口限流器（spec `step6-feedback-loop.md` §3 D8、§7.2、§10 A8）：
//! 每 `(domain, key_label)` 60 秒窗口最多 120 次**已认证请求**；超限 429，
//! `Retry-After` 为窗口剩余秒；重启清空（纯内存）。
//! The in-process fixed-window limiter (spec `step6-feedback-loop.md` §3 D8,
//! §7.2, §10 A8): at most 120 **authenticated requests** per
//! `(domain, key_label)` 60-second window; over the limit → 429 with
//! `Retry-After` set to the window's remaining seconds; cleared on restart
//! (pure in-memory).
//!
//! key 标签是 BLAKE3 截断 hex（D8）——原文永不作为限流键。窗口按 epoch 对齐
//! （`start = now - now % window`），因此窗口边界与重启无关，`Retry-After`
//! 确定可测（A8 时间注入）。
//! Key labels are BLAKE3 truncated hex (D8) — raw secrets are never used as
//! limiter keys. Windows align to the epoch (`start = now - now % window`), so
//! window boundaries are restart-independent and `Retry-After` is deterministic
//! and testable (A8 clock injection).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::state::Clock;

/// 单个 (domain, key_label) 的窗口状态。
/// Per-(domain, key_label) window state.
#[derive(Debug, Clone, Copy)]
struct WindowState {
    window_start: i64,
    count: u64,
}

/// 固定窗口限流器。条目集合以「key 配置 × 域」为上界（限流只在租户校验后
/// 执行，body domain 必须等于 key 的绑定 domain），无需淘汰。
/// The fixed-window limiter. The entry set is bounded by "key config ×
/// domains" (limiting runs only after the tenant check, where the body domain
/// must equal the key's bound domain), so no eviction is needed.
pub struct FixedWindowLimiter {
    windows: Mutex<HashMap<(String, String), WindowState>>,
    clock: Arc<dyn Clock>,
    window_secs: u64,
    max_per_window: u32,
}

impl FixedWindowLimiter {
    pub fn new(clock: Arc<dyn Clock>, window_secs: u64, max_per_window: u32) -> Self {
        FixedWindowLimiter {
            windows: Mutex::new(HashMap::new()),
            clock,
            window_secs: window_secs.max(1),
            max_per_window: max_per_window.max(1),
        }
    }

    /// 尝试占一个名额：允许 → `Ok(())`（计数 +1）；超限 → `Err(剩余秒)`
    /// （响应 `Retry-After`）。超限请求不加计数（固定窗口标准语义）。
    /// Tries to acquire one slot: allowed → `Ok(())` (count +1); over the
    /// limit → `Err(remaining seconds)` (the response `Retry-After`).
    /// Over-limit requests do not add to the count (standard fixed-window
    /// semantics).
    ///
    /// 锁纪律：同步短临界区（HashMap 读写），不跨 await。
    /// Lock discipline: a short synchronous critical section (HashMap read/
    /// write), never across await.
    pub fn try_acquire(&self, domain: &str, key_label: &str) -> Result<(), u64> {
        let now = self.clock.now_secs();
        let window = self.window_secs as i64;
        let start = now - now.rem_euclid(window);
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        let state = windows
            .entry((domain.to_string(), key_label.to_string()))
            .or_insert(WindowState {
                window_start: start,
                count: 0,
            });
        if state.window_start != start {
            // 新窗口：计数清零（固定窗口翻转；重启后同式重算，等效清空）。
            // New window: reset the count (fixed-window rollover; after a
            // restart the same formula recomputes, equivalent to clearing).
            state.window_start = start;
            state.count = 0;
        }
        if state.count >= u64::from(self.max_per_window) {
            return Err((state.window_start + window - now).max(0) as u64);
        }
        state.count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MockClock;

    // D8/A8：窗口内第 max+1 个请求 429，Retry-After 为剩余秒；窗口翻转恢复。
    // D8/A8: the (max+1)-th request in a window is 429 with Retry-After in
    // remaining seconds; the rollover recovers.
    #[test]
    fn limits_within_window_and_recovers_on_rollover() {
        let clock = MockClock::new(3600); // 3600 % 60 == 0 → 窗口 [3600, 3660) / window [3600, 3660)
        let limiter = FixedWindowLimiter::new(clock.clone(), 60, 3);
        for _ in 0..3 {
            assert!(limiter.try_acquire("milk-tea", "label-a").is_ok());
        }
        assert_eq!(limiter.try_acquire("milk-tea", "label-a"), Err(60));
        clock.set(3659);
        assert_eq!(limiter.try_acquire("milk-tea", "label-a"), Err(1));
        clock.set(3660);
        assert!(limiter.try_acquire("milk-tea", "label-a").is_ok());

        // 不同 (domain, label) 计数独立。
        // Different (domain, label) pairs count independently.
        assert!(limiter.try_acquire("milk-tea", "label-b").is_ok());
        assert!(limiter.try_acquire("other", "label-a").is_ok());
    }

    // A8：重启清空——新限流器实例对同 (domain, label) 从零计数。
    // A8: restart clears — a fresh limiter instance counts the same
    // (domain, label) from zero.
    #[test]
    fn fresh_instance_counts_from_zero() {
        let clock = MockClock::new(3600);
        let limiter = FixedWindowLimiter::new(clock.clone(), 60, 1);
        assert!(limiter.try_acquire("milk-tea", "label-a").is_ok());
        assert_eq!(limiter.try_acquire("milk-tea", "label-a"), Err(60));
        let restarted = FixedWindowLimiter::new(clock, 60, 1);
        assert!(restarted.try_acquire("milk-tea", "label-a").is_ok());
    }
}
