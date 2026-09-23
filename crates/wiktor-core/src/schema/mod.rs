//! SQLite Schema 模块。
//! SQLite Schema module.
//!
//! 定义两平面 + 队列 + 日志 + 倒排的 DDL 与版本迁移：`knowledge`（知识
//! 平面）、`facts`（事实平面）、`tasks`（编译任务队列）、`query_log`
//! （查询日志）与 `migrations`（diesel 迁移入口）。
//! Defines DDL and version migrations for the two planes + queue + logs + inverted
//! index: `knowledge` (knowledge plane), `facts` (fact plane), `tasks` (compile task
//! queue), `query_log` (query log) and `migrations` (diesel migration entry point).

pub mod facts;
mod knowledge;
mod migrations;
mod query_log;
pub mod tasks;

pub use migrations::{migrate, schema_version};

// 连接级 busy timeout pragma（spec step6 §9；kernel establish 与 migrate 共用，
// 避免两处字面量漂移）。crate 内使用，不对外暴露。
// The connection-level busy-timeout pragma (spec step6 §9; shared by the kernel
// establish and migrate so the literal cannot drift between the two sites).
// Crate-internal use, not part of the public API.
pub(crate) use migrations::BUSY_TIMEOUT_PRAGMA_SQL;
