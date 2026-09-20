//! 数据源适配器。
//!
//! 插件点 1（MASTER-PLAN §5.6）：JSONL 起步，postgres 同接口另实现。
//! 当前提供 [`JsonlDataSource`]：按 `jsonl://` URI 读取 JSON Lines 文件，
//! 以行号作游标分页读取，坏行 fail-fast。

pub mod jsonl;

pub use jsonl::{JsonlDataSource, DEFAULT_BATCH_SIZE};
