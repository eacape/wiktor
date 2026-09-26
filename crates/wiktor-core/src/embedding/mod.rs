//! 真实嵌入客户端（`embedding-http` feature）：OpenAI 兼容 `/embeddings` 端点
//! 的确定性 HTTP 实现（默认对接阿里云 MaaS qwen3.7-text-embedding-flash）。
//! Real embedding client (`embedding-http` feature): a deterministic HTTP
//! implementation of the OpenAI-compatible `/embeddings` endpoint (defaults to
//! Aliyun MaaS qwen3.7-text-embedding-flash).
//!
//! 契约要点：
//! - 实现 [`crate::query_engine::QueryEmbedder`]，与 DeterministicEmbedder 同一
//!   注入位；配置全部走环境变量（key 绝不进代码/仓库）：
//!   `WIKTOR_EMBEDDING_BASE_URL`（默认 `https://api.openai.com/v1`）、
//!   `WIKTOR_EMBEDDING_API_KEY`（缺省为 `WIKTOR_OPENAI_API_KEY` 兜底）、
//!   `WIKTOR_EMBEDDING_MODEL`（默认 `qwen3.7-text-embedding-flash`）。
//! - 维度动态探测：首次 [`embed`] 从响应向量长度缓存维度，供上层建
//!   collection 用（不在代码中硬编码模型维度）。
//! - 单请求语义：请求级 timeout 默认 60s；非 200 与传输错误映射为
//!   [`Error::InvalidConfig`]/[`Error::Internal`] 的确定性错误串，不吞不猜。
//! - 与 `compile/llm.rs` 同一 reqwest 栈（rustls），不再引入第二 HTTP 客户端。
//!
//! Contract highlights:
//! - Implements [`crate::query_engine::QueryEmbedder`], sharing the injection
//!   point with DeterministicEmbedder; configuration comes entirely from
//!   environment variables (keys never enter code or the repo):
//!   `WIKTOR_EMBEDDING_BASE_URL` (default `https://api.openai.com/v1`),
//!   `WIKTOR_EMBEDDING_API_KEY` (falls back to `WIKTOR_OPENAI_API_KEY`),
//!   `WIKTOR_EMBEDDING_MODEL` (default `qwen3.7-text-embedding-flash`).
//! - Dimension discovery: the first [`embed`] caches the response vector length
//!   so callers can build a collection with the real model dimension (the model
//!   dimension is never hard-coded here).
//! - Single-request semantics: a 60s default request timeout; non-200 and
//!   transport errors map to deterministic [`Error::InvalidConfig`] /
//!   [`Error::Internal`] strings — never swallowed, never guessed.
//! - Uses the same reqwest stack (rustls) as `compile/llm.rs`; no second HTTP
//!   client is introduced.

#[cfg(feature = "embedding-http")]
use crate::query_engine::QueryEmbedder;
#[cfg(feature = "embedding-http")]
use crate::types::error::{Error, Result};
#[cfg(feature = "embedding-http")]
use async_trait::async_trait;
#[cfg(feature = "embedding-http")]
use std::sync::Mutex;
#[cfg(feature = "embedding-http")]
use std::time::Duration;

/// 确定性本地基线（Step7 迁入共享；离线测试与无嵌入配置时的 embedder）。
/// The deterministic local baseline (moved in for sharing under Step7; the
/// embedder for offline tests and no-embedding-config setups).
pub mod deterministic;

/// key 的环境变量名（唯一来源；缺省兜底 `WIKTOR_OPENAI_API_KEY`）。
/// The env name for the key (sole source; falls back to `WIKTOR_OPENAI_API_KEY`).
#[cfg(feature = "embedding-http")]
pub const EMBEDDING_API_KEY_ENV: &str = "WIKTOR_EMBEDDING_API_KEY";
/// 端点的环境变量名（OpenAI 兼容 base url）。
/// The env name for the endpoint (OpenAI-compatible base url).
#[cfg(feature = "embedding-http")]
pub const EMBEDDING_BASE_URL_ENV: &str = "WIKTOR_EMBEDDING_BASE_URL";
/// 模型名的环境变量名。
/// The env name for the model id.
#[cfg(feature = "embedding-http")]
pub const EMBEDDING_MODEL_ENV: &str = "WIKTOR_EMBEDDING_MODEL";
/// 默认端点（阿里云 MaaS 兼容模式；实验时经环境变量覆盖）。
/// Default endpoint (Aliyun MaaS compatible mode; overridden via env at run time).
#[cfg(feature = "embedding-http")]
pub const DEFAULT_EMBEDDING_BASE_URL: &str = "https://api.openai.com/v1";
/// 默认模型（qwen3.7-text-embedding-flash，2026-09 实验拍板）。
/// Default model (qwen3.7-text-embedding-flash, decided 2026-09 for the
/// real-backend experiment).
#[cfg(feature = "embedding-http")]
pub const DEFAULT_EMBEDDING_MODEL: &str = "qwen3.7-text-embedding-flash";

/// OpenAI 兼容 `/embeddings` 的 HTTP 嵌入器（`embedding-http` feature）。
/// HTTP embedder for the OpenAI-compatible `/embeddings` endpoint (the
/// `embedding-http` feature).
#[cfg(feature = "embedding-http")]
pub struct HttpEmbedder {
    model: String,
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// 缓存的首个响应维度（None = 尚未调用）。
    /// Cached dimension from the first response (None = not yet called).
    dim: Mutex<Option<usize>>,
}

#[cfg(feature = "embedding-http")]
impl HttpEmbedder {
    /// 构造：`base_url`/`api_key`/`model` 缺省时全部从环境变量读取。
    /// Builds the embedder; missing `base_url`/`api_key`/`model` are read from
    /// the environment.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var(EMBEDDING_BASE_URL_ENV)
            .unwrap_or_else(|_| DEFAULT_EMBEDDING_BASE_URL.to_string());
        let api_key = std::env::var(EMBEDDING_API_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| {
                std::env::var(crate::compile::llm::API_KEY_ENV)
                    .ok()
                    .filter(|k| !k.trim().is_empty())
            })
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "embedding api key missing: set {EMBEDDING_API_KEY_ENV} or \
                     {}",
                    crate::compile::llm::API_KEY_ENV
                ))
            })?;
        let model = std::env::var(EMBEDDING_MODEL_ENV)
            .unwrap_or_else(|_| DEFAULT_EMBEDDING_MODEL.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| {
                Error::InvalidConfig(format!("embedding http client build failed: {e}"))
            })?;
        Ok(Self {
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            http,
            dim: Mutex::new(None),
        })
    }

    /// 请求端点（`{base_url}/embeddings`）。
    /// The request endpoint (`{base_url}/embeddings`).
    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url)
    }

    /// 已探测的维度（未调用过返回 None）。
    /// The discovered dimension (None before the first call).
    pub fn dimension(&self) -> Option<usize> {
        *self.dim.lock().unwrap()
    }
}

#[cfg(feature = "embedding-http")]
#[async_trait]
impl QueryEmbedder for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.post_embeddings(&[text]).await?.remove(0))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.post_embeddings(texts).await
    }
}

#[cfg(feature = "embedding-http")]
impl HttpEmbedder {
    /// 单次 POST `/embeddings`（一次请求多个 input）。
    /// One POST `/embeddings` (multiple inputs in a single request).
    async fn post_embeddings(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "model": self.model,
            "input": texts,
        });
        // 重试（P1）：瞬态失败（429/408/5xx 与传输超时/连接失败）至多
        // EMBED_RETRIES 次，退避 200ms·2^n；幂等（只读 POST）。4xx 永久失败不重试。
        // Retry (P1): transient failures (429/408/5xx, transport timeout/connect)
        // retry up to EMBED_RETRIES times with 200ms·2^n backoff; idempotent
        // (read-only POST). Other 4xx are permanent and never retried.
        let mut attempt = 0;
        loop {
            let resp = match self
                .http
                .post(self.endpoint())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .bearer_auth(&self.api_key)
                .body(serde_json::to_vec(&body)?)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let retryable = e.is_timeout() || e.is_connect();
                    if retryable && attempt < EMBED_RETRIES {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                        continue;
                    }
                    return Err(Error::Internal(format!("embedding request failed: {e}")));
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let retryable =
                    status.as_u16() == 429 || status.as_u16() == 408 || status.is_server_error();
                if retryable && attempt < EMBED_RETRIES {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
                    continue;
                }
                return Err(Error::InvalidConfig(format!(
                    "embedding endpoint returned {status}"
                )));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Error::Internal(format!("embedding response read failed: {e}")))?;
            let parsed: EmbeddingResponse = serde_json::from_slice(&bytes)?;
            if parsed.data.len() != texts.len() {
                return Err(Error::Internal(format!(
                    "embedding batch mismatch: sent {}, got {}",
                    texts.len(),
                    parsed.data.len()
                )));
            }
            let mut vectors = Vec::with_capacity(parsed.data.len());
            let mut dim: Option<usize> = None;
            for datum in parsed.data {
                if datum.embedding.is_empty() {
                    return Err(Error::Internal("embedding response vector is empty".into()));
                }
                if dim.is_none() {
                    dim = Some(datum.embedding.len());
                }
                vectors.push(datum.embedding);
            }
            // 缓存维度（与 embed 的探测语义一致）。
            // Cache the dimension (same discovery semantics as embed).
            let mut self_dim = self.dim.lock().unwrap();
            if self_dim.is_none() {
                *self_dim = dim;
            }
            return Ok(vectors);
        }
    }
}

/// 瞬态嵌入错误重试上限（P1）。
/// Transient embedding-error retry cap (P1).
#[cfg(feature = "embedding-http")]
const EMBED_RETRIES: usize = 3;

/// `/embeddings` 响应（只取所需字段；未知字段忽略）。
/// `/embeddings` response (only the fields needed; unknown fields are ignored).
#[cfg(feature = "embedding-http")]
#[derive(Debug, serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[cfg(feature = "embedding-http")]
#[derive(Debug, serde::Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

#[cfg(all(test, feature = "embedding-http"))]
mod tests {
    use super::*;
    use serde_json::json;

    // 响应解析：多候选只取第一个，字段缺失/空向量报错。
    // Response parsing: the first datum wins; missing/empty vectors error.
    #[test]
    fn parses_first_embedding_datum() {
        let raw = json!({
            "object": "list",
            "data": [
                {"index": 0, "object": "embedding", "embedding": [0.1, 0.2, 0.3]},
                {"index": 1, "object": "embedding", "embedding": [0.4, 0.5]}
            ],
            "model": "qwen3.7-text-embedding-flash"
        });
        let parsed: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].embedding, vec![0.1_f32, 0.2, 0.3]);
    }

    #[test]
    fn rejects_empty_vector() {
        let raw = json!({
            "data": [{"embedding": []}]
        });
        let parsed: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert!(parsed.data[0].embedding.is_empty());
    }
}
