//! 确定性查询嵌入器（本地基线，不宣称语义质量；从 wiktor-cli/src/embed.rs
//! 迁入供 server/CLI 共享，Step7 §3 X6）。
//! Deterministic query embedder (a local baseline making no semantic-quality
//! claims; moved in from wiktor-cli/src/embed.rs so server and CLI share it,
//! Step7 §3 X6).
//!
//! 仅用于本地闭环：把文本映射为固定维度的确定性 token 哈希向量，保证同一
//! 文本恒定输出（幂等、可复现、零网络）。它**不是**嵌入模型——真实语义召回
//! 需注入 [`crate::embedding::HttpEmbedder`]（embedding-http feature）或 BGE
//! 类实现。
//! Used only for the local loop: maps text to a fixed-dimension deterministic
//! token-hash vector, so the same text always yields the same vector
//! (idempotent, reproducible, offline). It is **not** an embedding model — real
//! semantic recall requires injecting [`crate::embedding::HttpEmbedder`] (the
//! embedding-http feature) or a BGE-style implementation.

use crate::query_engine::QueryEmbedder;
use crate::types::error::Result;
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 默认维度（与 qdrant collection 保持一致；CLI/seed 默认 768）。
/// Default dimension (aligned with the qdrant collection; the CLI/seed default
/// is 768).
pub const DIM: usize = 768;

/// 确定性 token 哈希嵌入器。
/// Deterministic token-hash embedder.
#[derive(Debug, Clone, Default)]
pub struct DeterministicEmbedder {
    pub dim: usize,
}

impl DeterministicEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl QueryEmbedder for DeterministicEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // 逐 Unicode 字符哈希到桶，桶值为稳定伪随机分量（非零，避免零向量）。
        // Hash each Unicode char into a bucket with a stable pseudo-random component
        // (non-zero, to avoid a zero vector).
        let mut vec = vec![0.0_f32; self.dim];
        for ch in text.chars() {
            let mut h = DefaultHasher::new();
            ch.hash(&mut h);
            let bucket = (h.finish() as usize) % self.dim;
            let mut h2 = DefaultHasher::new();
            (ch, text.len()).hash(&mut h2);
            vec[bucket] += ((h2.finish() >> 8) % 1000) as f32 / 1000.0;
        }
        // 归一化（余弦距离要求单位向量；空文本返回零向量）。
        // Normalize (cosine distance needs unit vectors; empty text yields zero).
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        Ok(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 确定性：同一文本恒定输出；不同文本向量不同；长度恒 DIM；空文本为零向量。
    // Deterministic: the same text yields the same vector; different texts
    // differ; the length is always DIM; empty text yields the zero vector.
    #[tokio::test]
    async fn deterministic_and_stable() {
        let e = DeterministicEmbedder::new(16);
        let a = e.embed("珍珠奶茶").await.unwrap();
        let b = e.embed("珍珠奶茶").await.unwrap();
        assert_eq!(a, b, "same text must embed identically");
        assert_eq!(a.len(), 16);
        let c = e.embed("啵啵").await.unwrap();
        assert_ne!(a, c, "different texts must differ");
        let empty = e.embed("").await.unwrap();
        assert!(empty.iter().all(|v| *v == 0.0), "empty text → zero vector");
    }
}
