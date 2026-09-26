//! Step 4 编译管线：配置/投影/身份（`config`）、全依赖哈希（`hash`）、
//! 输出契约与来源验证（`contract`）、四规则评分（`quality`）。
//! Step 4 compile pipeline: config/projection/identity (`config`), all-dependency
//! hashing (`hash`), output contract & source validation (`contract`), four-rule
//! scoring (`quality`).
//!
//! 本模块承载 Step 4 spec（`docs/design/step4-compile-pipeline.md` §3–§7）
//! 定义的数据契约与纯函数，以及 §3 的执行编排（`executor`）与离线 Mock
//! 编译器（`mock`）；`llm` 为 feature-gated 的 async-openai/ollama 适配器。
//! Step8 追加一致性仲裁维度（`consistency`，spec §6.1/§6.2）、兼容 preflight
//! （`compatibility`，spec §5.1/§6.4，决策 D10/D11）与租约回收/心跳编排
//! （`lease`，spec §6.3，决策 D8/D9）。
//! This module carries the data contracts and pure functions defined by the
//! Step 4 spec (§3–§7) plus the §3 execution orchestration (`executor`) and the
//! offline mock compiler (`mock`); `llm` is the feature-gated
//! async-openai/ollama adapter. Step8 adds the consistency-arbitration dimension
//! (`consistency`, spec §6.1/§6.2), the compatibility preflight
//! (`compatibility`, spec §5.1/§6.4, decisions D10/D11) and the lease-reaper/
//! heartbeat orchestration (`lease`, spec §6.3, decisions D8/D9).

pub mod compatibility;
pub mod config;
pub mod consistency;
pub mod contract;
pub mod executor;
pub mod hash;
pub mod lease;
#[cfg(feature = "llm-openai")]
pub mod llm;
pub mod mock;
pub mod quality;

pub use compatibility::{
    check_domain_compatibility, compatibility_conflict_payloads, CompatibilityChecker,
    CompatibilityReport, CompatibilityViolation, StandardCompatibilityChecker,
    COMPATIBILITY_ARTIFACT_NOT_ALLOWED, COMPATIBILITY_CORRUPT_FRONTMATTER,
    COMPATIBILITY_CORRUPT_SNAPSHOT, COMPATIBILITY_REJECTED_PREFIX, COMPATIBILITY_VERSION_INVALID,
    COMPATIBILITY_VERSION_MISSING, COMPATIBILITY_VERSION_RANGE,
};
pub use config::{
    prepare_source, Admission, Clock, CommitOutcome, CompatibilitySpec, CompilePolicy,
    CompileStats, ConsistencyPolicy, DomainIdentity, FailureDisposition, PreparedSource,
    RunOptions, SystemClock, TaskLease,
};
pub use consistency::{
    fts_query_terms, ClaimKey, ConsistencyArbiter, ConsistencyCandidateProvider,
    ConsistencyFinding, ConsistencyReport, SourceRefConsistencyArbiter, SqliteFtsCandidateProvider,
    CONSISTENCY_BELOW_THRESHOLD, MAX_CONSISTENCY_TOP_K, VALUE_DIVERGENCE,
};
#[cfg(feature = "llm-openai")]
pub use consistency::{ComparableRef, LlmConsistencyArbiter, CONSISTENCY_SYSTEM_PROMPT};
pub use contract::{
    decode_response, render_canonical_markdown, system_prompt, Assertion, CompileEvidence,
    CompileFailure, DefaultSourceRefValidator, EnvelopeErrorCode, EvidenceSection, OutputWiki,
    QualityIssue, RefReport, SourceRef, SourceRefValidator, TokenUsage,
};
pub use executor::PipelineExecutor;
pub use hash::{canonical_json, content_hash, snapshot_hash, HashDependencies};
pub use lease::LeaseReaper;
#[cfg(feature = "llm-openai")]
pub use llm::{LlmClient, LlmCompiler, LlmRequest, LlmResponse, OpenAiLlmClient};
pub use mock::{MockBehavior, MockCompiler};
pub use quality::{RuleBasedScorer, RuleScorer, ScoreReport};
