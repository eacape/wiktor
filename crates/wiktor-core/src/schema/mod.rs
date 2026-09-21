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
mod tasks;

pub use migrations::{migrate, schema_version};
