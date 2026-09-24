//! 确定性查询嵌入器（本地基线）——Step7 起实现迁入
//! `wiktor-core::embedding::deterministic`，本模块为薄 re-export，保持既有
//! `embed::DIM` / `embed::DeterministicEmbedder` 引用不变。
//! Deterministic query embedder (local baseline) — since Step7 the
//! implementation lives in `wiktor-core::embedding::deterministic`; this module
//! is a thin re-export keeping the existing `embed::DIM` /
//! `embed::DeterministicEmbedder` references unchanged.

pub use wiktor_core::embedding::deterministic::{DeterministicEmbedder, DIM};
