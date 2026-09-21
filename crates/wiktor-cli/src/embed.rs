//! 确定性查询嵌入器（本地基线，不宣称语义质量）。
//! Deterministic query embedder (local baseline; makes no semantic-quality claims).
//!
//! 仅用于 CLI/测试的本地闭环：把文本映射为固定维度的确定性 token 哈希向量，
//! 保证同一文本恒定输出（幂等、可复现、零网络）。它**不是**嵌入模型——真实
//! 语义召回需注入 fastembed/BGE 类实现（见 `VectorStore` 默认 qdrant 部署方向）。
//! Used only for the local loop in the CLI/tests: maps text to a fixed-dimension
//! deterministic token-hash vector, so the same text always yields the same vector
//! (idempotent, reproducible, offline). It is **not** an embedding model — real
//! semantic recall requires injecting a fastembed/BGE-style implementation (see the
//! default-qdrant deployment direction of `VectorStore`).

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use wiktor_core::QueryEmbedder;
use wiktor_core::Result;

/// 默认维度（与 qdrant collection 保持一致；CLI 默认 768）。
/// Default dimension (aligned with the qdrant collection; CLI defaults to 768).
pub const DIM: usize = 768;

/// 确定性 token 哈希嵌入器。
/// Deterministic token-hash embedder.
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
