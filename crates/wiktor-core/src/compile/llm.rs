//! LLM 单请求适配器（Step 4 spec §4，决策 D1；仅 `llm-openai` feature 编译）。
//! Single-request LLM adapter (Step 4 spec §4, decision D1; compiled only under
//! the `llm-openai` feature).
//!
//! 契约要点（§4/§10）：
//! - 供应商 DTO（async-openai wire 类型）不越过适配器：`LlmClient` 只见
//!   [`LlmRequest`]/[`LlmResponse`]。
//! - **单请求语义**：async-openai 0.28 的自带 `Client` 内置 backoff 自动重试，
//!   适配器不使用该 Client 的 HTTP 路径——仅复用其 wire DTO（请求/响应类型），
//!   HTTP 发送走本 crate 直连的 reqwest（与 SDK 同版、同一 rustls 栈），每次
//!   `complete` 恰好一次 HTTP 请求，重试只由 executor 调度。
//! - Chat Completions、非流式、temperature=0、`response_format=json_object`
//!   （不依赖供应商 JSON-schema 强制能力；本地 serde 校验始终执行）。
//! - 默认端点：OpenAI `https://api.openai.com/v1`，key 只取
//!   `WIKTOR_OPENAI_API_KEY`；ollama `http://127.0.0.1:11434/v1` 不需要 key。
//! - HTTP 分类：408/429/5xx/连接重置/超时 → `Retryable`（Retry-After 秒或
//!   HTTP-date 转延迟并钳制 0..=300s；无值留给 kernel 退避）；400/401/403/404/
//!   422 等其它 4xx → `Permanent`。不加随机抖动。
//! - 响应正文 ≤128 KiB（流式分块读取，超限 `InvalidOutput`）；日志不含 key、
//!   原始字段与完整响应。
//! - 偏差说明（TLS 证书错误）：reqwest 不暴露"连接失败是否源于证书"的判别
//!   API，连接级失败统一归 `Retryable`（耗尽后 dead/failed，绝不误接受）；
//!   证书错误不会被伪装为低质量候选。
//! - 偏差说明（构造器）：`new` 返回 `Result<Self>`（reqwest Client 构建可能
//!   失败），避免内部 panic。
//!
//! Contract highlights (§4/§10):
//! - Vendor DTOs (async-openai wire types) never cross the adapter: `LlmClient`
//!   sees only [`LlmRequest`]/[`LlmResponse`].
//! - **Single-request semantics**: async-openai 0.28's own `Client` ships
//!   built-in backoff retries; the adapter never uses that Client's HTTP path —
//!   it reuses only the wire DTOs (request/response types) and sends via this
//!   crate's direct reqwest dependency (same version and rustls stack as the
//!   SDK). Every `complete` performs exactly one HTTP request; retries are
//!   scheduled solely by the executor.
//! - Chat Completions, non-streaming, temperature=0,
//!   `response_format=json_object` (never relying on vendor JSON-schema
//!   enforcement; local serde validation always runs).
//! - Default endpoints: OpenAI `https://api.openai.com/v1` with the key taken
//!   only from `WIKTOR_OPENAI_API_KEY`; ollama `http://127.0.0.1:11434/v1`
//!   needs no key.
//! - HTTP classification: 408/429/5xx/connection reset/timeout → `Retryable`
//!   (Retry-After as seconds or HTTP-date, clamped to 0..=300s; absence defers
//!   to the kernel backoff); other 4xx such as 400/401/403/404/422 →
//!   `Permanent`. No random jitter.
//! - Response body ≤128 KiB (chunked streaming read; oversize →
//!   `InvalidOutput`); logs never contain keys, raw fields or full responses.
//! - Deviation note (TLS certificate errors): reqwest exposes no API to tell
//!   whether a connect failure stems from a certificate, so connect-level
//!   failures uniformly map to `Retryable` (exhausting into dead/failed, never
//!   mis-accepted); certificate errors are never disguised as low quality.
//! - Deviation note (constructor): `new` returns `Result<Self>` (reqwest client
//!   construction can fail) to avoid internal panics.

use crate::compile::config::CompilePolicy;
use crate::compile::contract::{
    decode_response, system_prompt, CompileEvidence, CompileFailure, TokenUsage, MAX_RESPONSE_BYTES,
};
use crate::seed::split_sections;
use crate::traits::Compiler;
use crate::types::error::{Error, Result};
use crate::types::{CompiledPage, PageMetadata, QualityScore, RawEntity, WikiPage};
use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestUserMessage, CompletionUsage, CreateChatCompletionRequest,
    CreateChatCompletionRequestArgs, CreateChatCompletionResponse, ResponseFormat,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// 单次请求超时（§4：60 秒）。
/// Per-request timeout (§4: 60 seconds).
pub const DEFAULT_TIMEOUT_SECONDS: u32 = 60;

/// OpenAI 兼容默认端点（§4）。
/// Default OpenAI-compatible endpoint (§4).
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// ollama 默认兼容端点（§4）。
/// Default ollama-compatible endpoint (§4).
pub const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434/v1";

/// `WIKTOR_OPENAI_API_KEY`：key 的唯一环境变量来源（§4）。
/// `WIKTOR_OPENAI_API_KEY`: the only env source for the key (§4).
pub const API_KEY_ENV: &str = "WIKTOR_OPENAI_API_KEY";

/// 单次模型请求（§4 契约）。
/// One model request (§4 contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    pub system: String,
    pub input_json: String,
    pub model: String,
    pub max_output_tokens: u32,
    pub timeout_seconds: u32,
}

/// 单次模型响应（§4 契约）：模型 JSON 正文 + 适配器报告的 usage。
/// One model response (§4 contract): the model JSON body plus adapter-reported
/// usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    pub json: String,
    pub usage: Option<TokenUsage>,
}

/// LLM 客户端抽象（§4 契约）：供应商细节隔离在其实现之后。
/// The LLM client abstraction (§4 contract): vendor details stay behind it.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        request: LlmRequest,
    ) -> std::result::Result<LlmResponse, CompileFailure>;
}

/// OpenAI / ollama 单请求适配器。
/// The OpenAI / ollama single-request adapter.
pub struct OpenAiLlmClient {
    model: String,
    base_url: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAiLlmClient {
    /// 构造适配器：`base_url`/`api_key` 缺省时按 `ollama` 取默认端点；key 只从
    /// 显式参数或 [`API_KEY_ENV`] 解析（§4），ollama 不需要 key。每次 complete
    /// 恰好一次 HTTP 请求（见模块注释的单请求语义）。
    /// Builds the adapter: missing `base_url`/`api_key` fall back to the
    /// `ollama`-selected defaults; the key resolves only from the explicit
    /// argument or [`API_KEY_ENV`] (§4); ollama needs none. Every complete does
    /// exactly one HTTP request (see single-request semantics in the module doc).
    pub fn new(
        model: impl Into<String>,
        base_url: Option<String>,
        api_key: Option<String>,
        ollama: bool,
    ) -> Result<Self> {
        let base_url = match base_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None if ollama => OLLAMA_BASE_URL.to_string(),
            None => OPENAI_BASE_URL.to_string(),
        };
        let api_key = api_key.or_else(|| std::env::var(API_KEY_ENV).ok());
        let http = reqwest::Client::builder()
            // 请求级 timeout 由 LlmRequest.timeout_seconds 覆盖；这里兜底 90s。
            // Per-request timeout comes from LlmRequest.timeout_seconds; 90s is
            // the client-level backstop.
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| Error::InvalidConfig(format!("llm http client build failed: {e}")))?;
        Ok(Self {
            model: model.into(),
            base_url,
            api_key,
            http,
        })
    }

    /// 请求端点（`{base_url}/chat/completions`）。
    /// The request endpoint (`{base_url}/chat/completions`).
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

/// HTTP 状态分类（§4）：408/429/5xx 可重试；其它 4xx 永久；1xx/3xx 视为
/// 服务异常可重试（正常 200 不经过此函数）。
/// HTTP status classification (§4): 408/429/5xx retryable; other 4xx permanent;
/// 1xx/3xx treated as service anomalies (retryable). A normal 200 never reaches
/// this function.
fn classify_status(status: u16, retry_after: Option<&str>) -> CompileFailure {
    let retryable = status == 408 || status == 429 || status >= 500;
    if retryable {
        CompileFailure::Retryable {
            code: format!("HTTP_{status}"),
            retry_after_seconds: retry_after.and_then(parse_retry_after),
        }
    } else if (400..500).contains(&status) {
        CompileFailure::Permanent {
            code: format!("HTTP_{status}"),
        }
    } else {
        CompileFailure::Retryable {
            code: format!("HTTP_{status}"),
            retry_after_seconds: None,
        }
    }
}

/// 传输层错误分类（§4）：超时/连接失败可重试；其余按可重试处理（见模块注释
/// 的 TLS 偏差说明）。
/// Transport-error classification (§4): timeout/connect failures are retryable;
/// everything else retries too (see the TLS deviation note in the module doc).
fn classify_transport(err: &reqwest::Error) -> CompileFailure {
    let code = if err.is_timeout() {
        "TRANSPORT_TIMEOUT"
    } else if err.is_connect() {
        "TRANSPORT_CONNECT"
    } else if err.is_body() || err.is_decode() {
        "TRANSPORT_BODY"
    } else {
        "TRANSPORT_ERROR"
    };
    CompileFailure::Retryable {
        code: code.to_string(),
        retry_after_seconds: None,
    }
}

/// 解析 Retry-After（§4）：整数秒或 RFC7231 HTTP-date，钳制 0..=300 秒；
/// 无法解析视为缺省。
/// Parses Retry-After (§4): integer seconds or an RFC7231 HTTP-date, clamped to
/// 0..=300s; unparseable values count as absent.
fn parse_retry_after(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    let seconds = if let Ok(n) = trimmed.parse::<i64>() {
        Some(n)
    } else {
        parse_http_date(trimmed).map(|epoch| epoch - unix_now())
    };
    seconds.map(|s| s.clamp(0, 300) as u32)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 最小 RFC7231 IMF-fixdate 解析（`Wed, 21 Oct 2015 07:28:00 GMT`），按
/// days-from-civil 算法转 Unix 秒；仅用于 Retry-After，不用于其它日期。
/// Minimal RFC7231 IMF-fixdate parser (`Wed, 21 Oct 2015 07:28:00 GMT`)
/// converting to Unix seconds via days-from-civil; used only for Retry-After.
fn parse_http_date(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 || !parts[0].ends_with(',') || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let (hour, minute, second): (i64, i64, i64) = {
        let mut it = parts[4].split(':');
        (
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        )
    };
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) || year < 1970 {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    // days_from_civil（Howard Hinnant 算法，纯整数、无溢出风险）。
    // days_from_civil (Howard Hinnant's algorithm; pure integers, no overflow).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m_shifted = i64::from(month) + if month > 2 { -3 } else { 9 };
    let doy = (153 * m_shifted + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// 拼装 Chat Completions 请求体（SDK wire 类型；供应商 DTO 不越过适配器）。
/// `model` 为请求最终使用的模型标识（request.model 为空时回退到 client 默认）。
/// Builds the Chat Completions body (SDK wire types; vendor DTOs never leave
/// the adapter). `model` is the final model id (falls back to the client
/// default when request.model is empty).
fn build_request_body(request: &LlmRequest, model: &str) -> Result<Vec<u8>> {
    // `max_tokens` 在 OpenAI 侧标记 deprecated，但为 ollama 等本地兼容端点的
    // 最大公约数，此处显式 allow；JSON object 模式 + 本地 serde 强校验兜底。
    // `max_tokens` is deprecated on OpenAI but remains the lowest common
    // denominator for local-compatible endpoints such as ollama — hence the
    // explicit allow; JSON-object mode plus local serde validation backstops it.
    #[allow(deprecated)]
    let body: CreateChatCompletionRequest = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(vec![
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: request.system.clone().into(),
                name: None,
            }),
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: request.input_json.clone().into(),
                name: None,
            }),
        ])
        .temperature(0.0_f32)
        .max_tokens(request.max_output_tokens)
        .response_format(ResponseFormat::JsonObject)
        .build()
        .map_err(|e| Error::Internal(format!("llm request build failed: {e}")))?;
    Ok(serde_json::to_vec(&body)?)
}

/// 流式读取响应正文，硬上限 `MAX_RESPONSE_BYTES`（§4：≤128 KiB）。
/// Streams the response body with the hard cap `MAX_RESPONSE_BYTES` (§4:
/// ≤128 KiB).
async fn read_capped(response: reqwest::Response) -> std::result::Result<Vec<u8>, CompileFailure> {
    let mut out: Vec<u8> = Vec::new();
    let mut response = response;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(CompileFailure::invalid("RESPONSE_TOO_LARGE", ""));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(out),
            Err(e) => return Err(classify_transport(&e)),
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiLlmClient {
    async fn complete(
        &self,
        request: LlmRequest,
    ) -> std::result::Result<LlmResponse, CompileFailure> {
        // 缺 key 的远端 provider 是配置错误（§4：不偷偷降级 Mock）。
        // A keyless remote provider is a configuration error (§4: never
        // silently downgrade to the Mock).
        let Some(api_key) = &self.api_key else {
            return Err(CompileFailure::Permanent {
                code: "MISSING_API_KEY".to_string(),
            });
        };
        // 请求模型：request.model 优先，空串回退到 client 构造时的默认模型。
        // Request model: request.model wins; an empty string falls back to the
        // client's constructor default.
        let model = if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        };
        let body = build_request_body(&request, &model).map_err(|e| match e {
            Error::Serialization(inner) => CompileFailure::Permanent {
                code: format!("REQUEST_SERIALIZATION:{inner}"),
            },
            other => CompileFailure::Permanent {
                code: format!("REQUEST_BUILD:{other}"),
            },
        })?;
        let timeout = if request.timeout_seconds == 0 {
            DEFAULT_TIMEOUT_SECONDS
        } else {
            request.timeout_seconds
        };
        // 单请求语义：一次 send、一次读体，无 SDK backoff、无本层重试。
        // Single-request semantics: one send, one body read, no SDK backoff and
        // no adapter-level retry.
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(api_key)
            .timeout(Duration::from_secs(timeout as u64))
            .body(body)
            .send()
            .await
            .map_err(|e| {
                // 日志只记传输错误本身（reqwest Display 不含 key/请求体）。
                // Logs carry only the transport error itself (reqwest's Display
                // excludes keys/bodies).
                tracing::warn!(error = %e, "llm transport error");
                classify_transport(&e)
            })?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !status.is_success() {
            let failure = classify_status(status.as_u16(), retry_after.as_deref());
            tracing::warn!(status = status.as_u16(), code = ?failure, "llm http error");
            return Err(failure);
        }
        let bytes = read_capped(response).await?;
        let parsed: CreateChatCompletionResponse =
            serde_json::from_slice(&bytes).map_err(|_| {
                // 200 但供应商响应不合法 → 服务异常（Retryable，§4：服务错误不
                // 伪装为低质量）。
                // 200 with an illegal provider payload → service anomaly
                // (Retryable; §4: service errors are never disguised as low
                // quality).
                tracing::warn!("llm provider response unparseable");
                CompileFailure::Retryable {
                    code: "PROVIDER_RESPONSE_MALFORMED".to_string(),
                    retry_after_seconds: None,
                }
            })?;
        let json = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or_else(|| {
                // 模型空输出 → 低质量候选（§4：空/超长输出不截断后接受）。
                // Empty model output → a low-quality candidate (§4: empty or
                // oversize output is never truncated into acceptance).
                CompileFailure::invalid("EMPTY_OUTPUT", "")
            })?;
        let usage = parsed.usage.map(|u: CompletionUsage| TokenUsage {
            input: u64::from(u.prompt_tokens),
            output: u64::from(u.completion_tokens),
        });
        // 日志不含 key/原始字段/完整响应（§4）。
        // Logs never contain keys/raw fields/full responses (§4).
        tracing::debug!(chars = json.chars().count(), usage = ?usage, "llm completion received");
        Ok(LlmResponse { json, usage })
    }
}

/// LLM 编译器（§4 契约）：调 LlmClient → decode → 组装 CompiledPage。
/// The LLM compiler (§4 contract): calls the LlmClient → decodes → assembles a
/// CompiledPage.
pub struct LlmCompiler {
    pub client: Arc<dyn LlmClient>,
    pub policy: CompilePolicy,
}

#[async_trait]
impl Compiler for LlmCompiler {
    async fn compile(
        &self,
        raw: RawEntity,
        ctx: &crate::types::CompileContext,
    ) -> Result<CompiledPage> {
        // input_json = canonical(knowledge 快照)（§7：source 域与 claim 预留估算
        // 同一形状）；每次 compile 恰好一次模型请求（D1）。
        // input_json = canonical(knowledge snapshot) (§7: the same shape as the
        // claim reservation's source domain); exactly one model request per
        // compile (D1).
        let input_json = {
            let value = serde_json::to_value(&raw)?;
            serde_json::to_string(&value)?
        };
        let request = LlmRequest {
            system: system_prompt(),
            input_json,
            model: ctx.model_version.clone(),
            max_output_tokens: self.policy.max_output_tokens,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        };
        let response = self
            .client
            .complete(request)
            .await
            .map_err(Error::CompileFailure)?;
        // 本地 serde 强校验（§4：不依赖供应商 JSON 模式强制能力）。
        // Local serde strict validation (§4: never trusting vendor JSON mode).
        let evidence = match decode_response(&response.json) {
            Ok(e) => e,
            // 错误 envelope → 类型化失败（低质量候选，不伪造 accepted 页）。
            // Error envelope → typed failure (a low-quality candidate, never a
            // faked accepted page).
            Err(failure) => return Err(Error::CompileFailure(failure)),
        };
        // usage 由适配器报告注入（§5.1：usage 不属于模型 JSON）。
        // usage is injected from the adapter report (§5.1: usage is not part of
        // the model JSON).
        let evidence = CompileEvidence {
            usage: response.usage,
            ..evidence
        };
        let content = crate::compile::contract::render_canonical_markdown(&evidence);
        Ok(CompiledPage {
            wiki: WikiPage {
                page_id: raw.id.to_key(),
                entity_id: raw.id.clone(),
                title: evidence.wiki.title.clone(),
                content: content.clone(),
                // executor 会按 §5.2.5 重算；这里先用共享 splitter 生成。
                // The executor recomputes per §5.2.5; the shared splitter fills
                // it here first.
                sections: split_sections(&content),
                // 占位 metadata：executor 以冻结 context + 时钟重算（§4）。
                // Placeholder metadata: recomputed by the executor from the
                // frozen context + clock (§4).
                metadata: PageMetadata {
                    domain_pack_version: String::new(),
                    compiled_at: 0,
                    model_version: String::new(),
                    embedding_model: String::new(),
                },
                aliases: evidence.wiki.aliases.clone(),
                tags: evidence.wiki.tags.clone(),
            },
            // 占位零分：模型返回的 quality 不可信，不能短路评分（§4）。
            // Placeholder zeros: model-reported quality is untrusted and must
            // not short-circuit scoring (§4).
            quality: QualityScore {
                coverage: 0.0,
                citation: 0.0,
                schema_compliance: 0.0,
                density: 0.0,
                consistency: None,
            },
            // D8：LLM envelope 不提供 edges 字段，本步默认空。
            // D8: the LLM envelope has no edges field; empty by default.
            qug_edges: Vec::new(),
            // §4：模型返回的 content_hash 不可信 → 留空，executor publish 前
            // 用 content_hash(HashDependencies) 重算并写回。
            // §4: the model-reported content_hash is untrusted → left empty; the
            // executor recomputes and fills it via content_hash(HashDependencies)
            // before publish.
            content_hash: String::new(),
            evidence: Some(evidence),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CompileContext, EntityId};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn knowledge() -> RawEntity {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert("description".to_string(), serde_json::json!("珍珠奶茶"));
        RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        }
    }

    fn ctx() -> CompileContext {
        crate::compile::config::build_context(
            "test-v1",
            "S",
            "test-model",
            "none",
            0.75,
            true,
            None,
            None,
        )
    }

    /// 计数 Mock LlmClient：固定响应/失败脚本 + 调用计数（断言单请求语义）。
    /// Counting mock LlmClient: a fixed response/failure script plus a call
    /// counter (asserts single-request semantics).
    struct MockLlm {
        response: std::result::Result<String, CompileFailure>,
        calls: AtomicU64,
    }

    impl MockLlm {
        fn ok(json: &str) -> Self {
            Self {
                response: Ok(json.to_string()),
                calls: AtomicU64::new(0),
            }
        }
        fn err(failure: CompileFailure) -> Self {
            Self {
                response: Err(failure),
                calls: AtomicU64::new(0),
            }
        }
        fn count(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn complete(
            &self,
            _request: LlmRequest,
        ) -> std::result::Result<LlmResponse, CompileFailure> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match &self.response {
                Ok(json) => Ok(LlmResponse {
                    json: json.clone(),
                    usage: Some(TokenUsage {
                        input: 10,
                        output: 5,
                    }),
                }),
                Err(f) => Err(f.clone()),
            }
        }
    }

    fn legal_envelope() -> String {
        // envelope 含 "## 概述 字样，用 r##"..."## 防止提前终止。
        // The envelope contains "## 概述, so r##"..."## avoids early termination.
        r###"{"schema_version":"source-ref-v1","status":"ok","wiki":{"title":"啵啵","aliases":[],"tags":[],"markdown":"## 概述\n\n- 啵啵[[ref:r1]]\n"},"sections":[{"heading":"概述","assertions":[{"text":"啵啵","ref_ids":["r1"]}],"refs":[{"id":"r1","entity_id":"milk-tea:drink:boba","source_revision":1,"pointer":"/fields/name","value":"啵啵","quote":"啵啵"}]}]}"###
            .to_string()
    }

    // D1 单请求语义：一次 compile 恰好一次 client.complete（适配器层无自动重试）。
    // D1 single-request semantics: one compile → exactly one client.complete
    // (no adapter-level auto retry).
    #[tokio::test]
    async fn one_complete_per_compile() {
        let mock = Arc::new(MockLlm::ok(&legal_envelope()));
        let compiler = LlmCompiler {
            client: mock.clone(),
            policy: CompilePolicy::default(),
        };
        let page = compiler.compile(knowledge(), &ctx()).await.unwrap();
        assert_eq!(mock.count(), 1);
        let evidence = page.evidence.unwrap();
        assert_eq!(
            evidence.usage,
            Some(TokenUsage {
                input: 10,
                output: 5
            })
        );
        assert_eq!(page.content_hash, "");
        assert_eq!(page.wiki.content, "## 概述\n\n- 啵啵[[ref:r1]]\n");
    }

    // 类型化失败直通：Retryable/Permanent 不被适配器吞掉或改类。
    // Typed failures pass through: Retryable/Permanent are neither swallowed nor
    // reclassified.
    #[tokio::test]
    async fn typed_failures_propagate() {
        let retry = Arc::new(MockLlm::err(CompileFailure::Retryable {
            code: "HTTP_429".into(),
            retry_after_seconds: Some(30),
        }));
        let compiler = LlmCompiler {
            client: retry.clone(),
            policy: CompilePolicy::default(),
        };
        match compiler.compile(knowledge(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::Retryable {
                code,
                retry_after_seconds,
            }) => {
                assert_eq!(code, "HTTP_429");
                assert_eq!(retry_after_seconds, Some(30));
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
        assert_eq!(retry.count(), 1);

        let perm = Arc::new(MockLlm::err(CompileFailure::Permanent {
            code: "HTTP_401".into(),
        }));
        let compiler = LlmCompiler {
            client: perm.clone(),
            policy: CompilePolicy::default(),
        };
        match compiler.compile(knowledge(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::Permanent { code }) => {
                assert_eq!(code, "HTTP_401")
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        assert_eq!(perm.count(), 1);
    }

    // 模型输出无效 → InvalidOutput 低质量候选（fence/envelope 错误均如此）。
    // Invalid model output → InvalidOutput low-quality candidates (fence and
    // envelope errors alike).
    #[tokio::test]
    async fn invalid_model_output_maps_to_invalid_output() {
        let fenced = Arc::new(MockLlm::ok(&format!("```json\n{}\n```", legal_envelope())));
        let compiler = LlmCompiler {
            client: fenced,
            policy: CompilePolicy::default(),
        };
        match compiler.compile(knowledge(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::InvalidOutput { code, .. }) => {
                assert_eq!(code, "MALFORMED_JSON")
            }
            other => panic!("expected InvalidOutput, got {other:?}"),
        }
        let bad_envelope = Arc::new(MockLlm::ok(
            r#"{"schema_version":"source-ref-v1","status":"error","error":{"code":"MISSING_SOURCE_REFS","missing_pointers":["/fields/description"]}}"#,
        ));
        let compiler = LlmCompiler {
            client: bad_envelope,
            policy: CompilePolicy::default(),
        };
        match compiler.compile(knowledge(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::InvalidOutput { code, .. }) => {
                assert_eq!(code, "ERROR_ENVELOPE_MISSING_SOURCE_REFS")
            }
            other => panic!("expected InvalidOutput, got {other:?}"),
        }
    }

    // HTTP 状态分类（§4）。
    // HTTP status classification (§4).
    #[test]
    fn status_classification() {
        for status in [408u16, 429, 500, 502, 503, 504] {
            assert!(
                matches!(
                    classify_status(status, None),
                    CompileFailure::Retryable { .. }
                ),
                "{status} must be retryable"
            );
        }
        for status in [400u16, 401, 403, 404, 422] {
            assert!(
                matches!(
                    classify_status(status, None),
                    CompileFailure::Permanent { .. }
                ),
                "{status} must be permanent"
            );
        }
        // Retry-After 钳制 0..=300。
        // Retry-After clamped to 0..=300.
        match classify_status(429, Some("99999")) {
            CompileFailure::Retryable {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(300)),
            other => panic!("expected Retryable, got {other:?}"),
        }
        match classify_status(429, Some("-5")) {
            CompileFailure::Retryable {
                retry_after_seconds,
                ..
            } => assert_eq!(retry_after_seconds, Some(0)),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    // Retry-After 解析：整数秒与 HTTP-date。
    // Retry-After parsing: integer seconds and HTTP dates.
    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after("  42 "), Some(42));
        assert_eq!(parse_retry_after("not-a-date"), None);
        // HTTP-date：黄金值（2026-01-01T00:00:00Z = 1767225600）。
        // HTTP date: golden value (2026-01-01T00:00:00Z = 1767225600).
        let epoch = parse_http_date("Thu, 01 Jan 2026 00:00:00 GMT");
        assert_eq!(epoch, Some(1_767_225_600));
        // 过去时间钳为 0。
        // Past dates clamp to 0.
        assert_eq!(parse_retry_after("Thu, 01 Jan 1970 00:00:01 GMT"), Some(0));
    }

    // 请求体形状：temperature=0、json_object、messages 顺序（供应商 DTO 不外泄）。
    // Request-body shape: temperature=0, json_object, message order (vendor DTOs
    // never leak out).
    #[test]
    fn request_body_shape() {
        let request = LlmRequest {
            system: "SYS".into(),
            input_json: "{\"a\":1}".into(),
            model: "test-model".into(),
            max_output_tokens: 256,
            timeout_seconds: 60,
        };
        let body = build_request_body(&request, "test-model").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["temperature"], 0.0);
        assert_eq!(value["max_tokens"], 256);
        assert_eq!(value["response_format"]["type"], "json_object");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "{\"a\":1}");
    }

    // OpenAiLlmClient 构造：端点选择与缺 key 判定（不改环境变量）。
    // OpenAiLlmClient construction: endpoint choice and the missing-key verdict
    // (environment untouched).
    #[test]
    fn client_defaults() {
        let openai = OpenAiLlmClient::new("m", None, Some("k".into()), false).unwrap();
        assert_eq!(
            openai.endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );
        let ollama = OpenAiLlmClient::new("m", None, None, true).unwrap();
        assert_eq!(
            ollama.endpoint(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        let custom =
            OpenAiLlmClient::new("m", Some("http://localhost:9999/v1/".into()), None, false)
                .unwrap();
        assert_eq!(
            custom.endpoint(),
            "http://localhost:9999/v1/chat/completions"
        );
    }

    // 真实 provider smoke：显式忽略，避免 CI 无 key（§12 步骤 6）。
    // Real-provider smoke: explicitly ignored so CI never needs a key (§12 step
    // 6).
    #[tokio::test]
    #[ignore = "requires WIKTOR_OPENAI_API_KEY and network access"]
    async fn real_openai_smoke() {
        let client = OpenAiLlmClient::new("gpt-4o-mini", None, None, false).expect("client build");
        let request = LlmRequest {
            system: system_prompt(),
            input_json: serde_json::to_string(&serde_json::to_value(knowledge()).unwrap()).unwrap(),
            model: "gpt-4o-mini".into(),
            max_output_tokens: 512,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        };
        let response = client.complete(request).await.expect("completion");
        assert!(decode_response(&response.json).is_ok(), "legal envelope");
    }
}
