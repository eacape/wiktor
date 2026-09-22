//! Step 4 编译管线：配置/投影/身份（`config`）、全依赖哈希（`hash`）、
//! 输出契约与来源验证（`contract`）、四规则评分（`quality`）。
//! Step 4 compile pipeline: config/projection/identity (`config`), all-dependency
//! hashing (`hash`), output contract & source validation (`contract`), four-rule
//! scoring (`quality`).
//!
//! 本模块承载 Step 4 spec（`docs/design/step4-compile-pipeline.md` §3–§7）
//! 定义的数据契约与纯函数，以及 §3 的执行编排（`executor`）与离线 Mock
//! 编译器（`mock`）；`llm` 为 feature-gated 的 async-openai/ollama 适配器。
//! This module carries the data contracts and pure functions defined by the
//! Step 4 spec (§3–§7) plus the §3 execution orchestration (`executor`) and the
//! offline mock compiler (`mock`); `llm` is the feature-gated
//! async-openai/ollama adapter.

pub mod config;
pub mod contract;
pub mod executor;
pub mod hash;
#[cfg(feature = "llm-openai")]
pub mod llm;
pub mod mock;
pub mod quality;

pub use config::{
    prepare_source, Admission, Clock, CommitOutcome, CompilePolicy, CompileStats,
    FailureDisposition, PreparedSource, RunOptions, SystemClock, TaskLease,
};
pub use contract::{
    decode_response, render_canonical_markdown, system_prompt, Assertion, CompileEvidence,
    CompileFailure, DefaultSourceRefValidator, EnvelopeErrorCode, EvidenceSection, OutputWiki,
    QualityIssue, RefReport, SourceRef, SourceRefValidator, TokenUsage,
};
pub use executor::PipelineExecutor;
pub use hash::{canonical_json, content_hash, snapshot_hash, HashDependencies};
#[cfg(feature = "llm-openai")]
pub use llm::{LlmClient, LlmCompiler, LlmRequest, LlmResponse, OpenAiLlmClient};
pub use mock::{MockBehavior, MockCompiler};
pub use quality::{RuleBasedScorer, RuleScorer, ScoreReport};
