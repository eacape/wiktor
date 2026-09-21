//! 数据源适配器。
//!
//! 插件点 1（MASTER-PLAN §5.6）：JSONL 起步，postgres 同接口另实现。
//! Plugin point 1 (MASTER-PLAN §5.6): starts with JSONL; postgres can implement the same interface.
//! 当前提供 [`JsonlDataSource`]：按 `jsonl://` URI 读取 JSON Lines 文件，
//! Currently provides [`JsonlDataSource`], which reads JSON Lines files via `jsonl://` URIs,
//! 以行号作游标分页读取，坏行 fail-fast。
//! paginates by line-number cursor, and fails fast on malformed lines.

pub mod jsonl;

pub use jsonl::{JsonlDataSource, DEFAULT_BATCH_SIZE};
