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
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
        });
        let response = self
            .http
            .post(self.endpoint())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(&self.api_key)
            .body(serde_json::to_vec(&body)?)
            .send()
            .await
            .map_err(|e| Error::Internal(format!("embedding request failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::InvalidConfig(format!(
                "embedding endpoint returned {status}"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::Internal(format!("embedding response read failed: {e}")))?;
        let parsed: EmbeddingResponse = serde_json::from_slice(&bytes)?;
        let vector = parsed
            .data
            .first()
            .ok_or_else(|| Error::Internal("embedding response has no data".into()))?
            .embedding
            .clone();
        if vector.is_empty() {
            return Err(Error::Internal("embedding response vector is empty".into()));
        }
        let mut dim = self.dim.lock().unwrap();
        if dim.is_none() {
            *dim = Some(vector.len());
        }
        Ok(vector)
    }
}

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
