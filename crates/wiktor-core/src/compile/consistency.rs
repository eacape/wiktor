//! 一致性仲裁维度与确定性 source-ref 比较（Step 8 spec §6.1/§6.2，决策 D1–D4）。
//! Consistency-arbitration dimension and deterministic source-ref comparison
//! (Step 8 spec §6.1/§6.2, decisions D1–D4).
//!
//! 契约要点：
//! - 核心只依赖 [`ConsistencyArbiter`] 抽象；默认 [`SourceRefConsistencyArbiter`]
//!   是确定性、可离线的实现（X5：不做自由语义推断）：只比较 `compare_pointers`
//!   显式声明且满足 RFC6901 `/fields/...` 形状的 source-ref，按
//!   `(entity_id, pointer)` 分组、canonical_json 后精确比较；无可比较证据返回
//!   `score=None`（STEP8-003：不 fail-closed 也不判 0）——空指针表因此自然为
//!   `None` 且不产生任何 finding。
//! - [`ConsistencyCandidateProvider`] 有界召回相关页：默认 SQLite FTS 实现按
//!   bm25/page_id 稳定排序并 LIMIT top_k，绝不先全量加载再内存截断（A4/A8）。
//! - 诊断只存 BLAKE3 摘要，不落原文；重复 ref、无 evidence、旧 seed 页均跳过
//!   （A7）。未来 LLM 仲裁只能实现同一 trait，不能绕过证据/top-k/预算契约。
//!
//! Contract highlights:
//! - The core depends only on the [`ConsistencyArbiter`] abstraction; the default
//!   [`SourceRefConsistencyArbiter`] is deterministic and offline (X5: no free
//!   semantic inference): it compares only source-refs whose pointers are
//!   explicitly declared in `compare_pointers` and shaped as RFC6901
//!   `/fields/...`, grouped by `(entity_id, pointer)` and compared exactly after
//!   canonical_json; with no comparable evidence it returns `score=None`
//!   (STEP8-003: neither fail-closed nor zero) — an empty pointer table therefore
//!   naturally yields `None` with no findings.
//! - [`ConsistencyCandidateProvider`] performs bounded recall of related pages:
//!   the default SQLite FTS implementation sorts stably by bm25/page_id with
//!   LIMIT top_k, never loading everything and truncating in memory (A4/A8).
//! - Diagnostics store BLAKE3 digests only, never raw values; duplicate refs,
//!   pages without evidence and legacy seed pages are skipped (A7). A future LLM
//!   arbiter can only implement the same trait and cannot bypass the
//!   evidence/top-k/budget contracts.

use crate::compile::config::ConsistencyPolicy;
use crate::compile::hash::canonical_json;
use crate::kernel::SqliteKernel;
use crate::types::error::{Error, Result};
use crate::types::CompiledPage;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// 稳定诊断 code：同一比较键下出现不同规范化值（§6.1）。
/// Stable diagnostic code: distinct canonical values under one comparison key
/// (§6.1).
pub const VALUE_DIVERGENCE: &str = "VALUE_DIVERGENCE";

/// 稳定质量 issue code：一致性得分低于 `min_consistency`（§6.2/D4）。
/// Stable quality-issue code: the consistency score is below `min_consistency`
/// (§6.2/D4).
pub const CONSISTENCY_BELOW_THRESHOLD: &str = "CONSISTENCY_BELOW_THRESHOLD";

/// 稳定诊断 code（Step8 §5.2 canonical 形状）：一致性冲突诊断——attempt 的
/// `consistency_json` 与死信/冲突审核的 `reason_json` 共享同一编码。
/// Stable diagnostic code (Step8 §5.2 canonical shape): the consistency-conflict
/// diagnostic — shared verbatim by the attempt's `consistency_json` and the
/// dead-letter/conflict review's `reason_json`.
pub const CONSISTENCY_CONFLICT: &str = "CONSISTENCY_CONFLICT";

/// 稳定诊断 code（Step8 §5.2）：发布诊断——accepted 页在 publish 事务内写入
/// `compile_attempts.consistency_json` 的编码（score/计数/findings 摘要）。
/// Stable diagnostic code (Step8 §5.2): the publish diagnostic — the encoding
/// written into `compile_attempts.consistency_json` by the publish transaction
/// for accepted pages (score/counts/findings summary).
pub const CONSISTENCY_SCORE: &str = "CONSISTENCY_SCORE";

/// 相关页召回的硬上限（D2：top_k 默认 8、上限 32；provider 侧防御性钳制）。
/// Hard cap for related-page recall (D2: top_k defaults to 8 with a cap of 32;
/// defensively clamped on the provider side too).
pub const MAX_CONSISTENCY_TOP_K: u32 = 32;

/// 默认 provider 生成的 FTS 查询总长度上限（字符数，含引号与 OR 分隔的预算）。
/// Total length cap (in chars, budget including quotes and OR separators) for
/// the FTS query built by the default provider.
pub const MAX_FTS_QUERY_CHARS: usize = 256;

/// 比较键（§6.1 签名）：`(entity_id, pointer)` 精确对应，不做语义归并。
/// Claim key (§6.1 signature): exact `(entity_id, pointer)` matching, no
/// semantic merging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimKey {
    pub entity_id: String,
    pub pointer: String,
}

/// 单条分歧诊断：只含稳定 code、比较键与新旧值的 BLAKE3 摘要（不落原文）。
/// One divergence diagnostic: stable code, claim key and BLAKE3 digests of both
/// values only (raw values never stored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyFinding {
    pub code: String, // stable code, e.g. VALUE_DIVERGENCE
    pub key: ClaimKey,
    pub candidate_value_hash: String,
    pub evidence_value_hash: String,
}

/// 仲裁报告（§6.1 签名）：`score=None` 表示无可比较证据（不参与 overall）。
/// Arbitration report (§6.1 signature): `score=None` means no comparable
/// evidence (excluded from the overall average).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConsistencyReport {
    pub score: Option<f32>,
    pub compared_claims: u32,
    pub findings: Vec<ConsistencyFinding>,
    /// 参与仲裁的相关页数量（provider 候选集大小，≤ top_k）。
    /// Number of related pages that took part (provider candidate set size,
    /// bounded by top_k).
    pub candidate_count: u32,
}

/// 相关页提供器（§6.1 签名）：返回不超过 `limit` 的已发布 accepted 页，候选页
/// 自身不重复计入；实现禁止全量加载后内存截断。
/// Related-page provider (§6.1 signature): returns at most `limit` published
/// accepted pages, never double-counting the candidate itself; implementations
/// must not load everything and truncate in memory.
#[async_trait]
pub trait ConsistencyCandidateProvider: Send + Sync {
    async fn top_k_related(
        &self,
        candidate: &CompiledPage,
        limit: u32,
    ) -> Result<Vec<CompiledPage>>;
}

/// 仲裁器（§6.1 签名，Step14 P3 起为 async）：candidate + related → 报告。
/// The arbiter (§6.1 signature; async since Step14 P3): candidate + related →
/// report.
///
/// Step14 P3-A 把仲裁面从同步纯函数改为 async，以便 LLM 仲裁器（内部做网络
/// 调用）实现同一 trait——文档契约「LLM 仲裁只能实现同一 trait，不能绕过证据/
/// top-k/预算契约」因此保持成立。确定性实现 [`SourceRefConsistencyArbiter`]
/// 的方法体不变，仅签名加 `async`。
/// Step14 P3-A turns the arbitration surface from a synchronous pure function
/// into an async one so an LLM arbiter (which performs network calls internally)
/// implements the same trait — the documented contract "an LLM arbiter can only
/// implement the same trait and cannot bypass the evidence/top-k/budget contracts"
/// therefore stays intact. The deterministic [`SourceRefConsistencyArbiter`]
/// keeps its body unchanged, only the signature gains `async`.
#[async_trait]
pub trait ConsistencyArbiter: Send + Sync {
    async fn arbitrate(
        &self,
        candidate: &CompiledPage,
        related: &[CompiledPage],
        policy: &ConsistencyPolicy,
    ) -> Result<ConsistencyReport>;
}

/// 默认确定性仲裁器（D1/D3）：只比较显式声明且 `/fields/...` 形状的 source-ref。
/// The default deterministic arbiter (D1/D3): compares only explicitly declared,
/// `/fields/...`-shaped source-refs.
///
/// 算法（§6.1 逐条固定）：
/// 1. 收集 candidate 与 related 页 evidence 中的 source-ref；pointer 不在
///    `compare_pointers` 或不满足 `/fields/...` 形状的引用直接跳过。
/// 2. 页内按 `(entity_id, pointer, 规范化值)` 去重（重复 ref 跳过；同键不同值
///    仍是两次出现，页面自相矛盾可被检出）。
/// 3. 跨页按 `(entity_id, pointer)` 分组；组内只有一次值出现（没有比较对象）
///    不算比较。
/// 4. 同组值 canonical_json 后精确比较：全部相同 → equal；任意不同 →
///    `VALUE_DIVERGENCE`。
/// 5. `compared_claims=0` → `score=None`；否则 `score = equal_groups /
///    compared_groups`。值摘要为 `BLAKE3(canonical_json(value))` hex。
///
/// Algorithm (fixed item-by-item by §6.1):
/// 1. Collect source-refs from candidate and related pages' evidence; refs whose
///    pointer is absent from `compare_pointers` or not `/fields/...`-shaped are
///    skipped.
/// 2. Dedupe within a page by `(entity_id, pointer, canonical value)` (duplicate
///    refs are skipped; the same key with distinct values still counts as two
///    occurrences so self-contradiction is detectable).
/// 3. Group across pages by `(entity_id, pointer)`; a group holding a single
///    value occurrence (nothing to compare against) is not a comparison.
/// 4. Compare the group's values exactly after canonical_json: all equal →
///    equal; any difference → `VALUE_DIVERGENCE`.
/// 5. `compared_claims=0` → `score=None`; otherwise `score = equal_groups /
///    compared_groups`. Value digests are `BLAKE3(canonical_json(value))` hex.
#[derive(Debug, Clone, Copy, Default)]
pub struct SourceRefConsistencyArbiter;

impl SourceRefConsistencyArbiter {
    /// 构造默认仲裁器（无配置、无外部状态）。
    /// Constructs the default arbiter (no config, no external state).
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ConsistencyArbiter for SourceRefConsistencyArbiter {
    async fn arbitrate(
        &self,
        candidate: &CompiledPage,
        related: &[CompiledPage],
        policy: &ConsistencyPolicy,
    ) -> Result<ConsistencyReport> {
        // BTreeMap 迭代序 = (entity_id, pointer) 字节序 → findings 顺序稳定。
        // BTreeMap iteration order = byte order of (entity_id, pointer) → stable
        // findings order.
        let mut groups: BTreeMap<(String, String), Vec<ClaimOccurrence>> = BTreeMap::new();
        collect_page_refs(candidate, true, policy, &mut groups)?;
        for page in related {
            collect_page_refs(page, false, policy, &mut groups)?;
        }

        let mut compared_groups = 0u32;
        let mut equal_groups = 0u32;
        let mut findings: Vec<ConsistencyFinding> = Vec::new();
        for ((entity_id, pointer), occs) in &groups {
            // 组内只有一次值出现：没有比较对象，不算比较（§6.1）。
            // A single value occurrence in a group: nothing to compare against,
            // not a comparison (§6.1).
            if occs.len() < 2 {
                continue;
            }
            compared_groups += 1;
            let mut distinct: Vec<&Vec<u8>> = occs.iter().map(|o| &o.value).collect();
            distinct.sort();
            distinct.dedup();
            if distinct.len() == 1 {
                equal_groups += 1;
                continue;
            }
            // 任意不同值 → VALUE_DIVERGENCE；候选值取候选页出现的最小规范化值
            // （候选缺席时取全组最小，防御分支），证据值取与其不同的最小值。
            // 哈希输入是 canonical_json 字节，原文不进诊断。
            // Any distinct value → VALUE_DIVERGENCE; the candidate value is the
            // smallest canonical value occurring on the candidate page (the group
            // minimum in the defensive no-candidate branch), and the evidence
            // value is the smallest distinct one. Hash input is the
            // canonical_json bytes — raw values never reach diagnostics.
            let candidate_value = occs
                .iter()
                .filter(|o| o.from_candidate)
                .map(|o| &o.value)
                .min()
                .unwrap_or(distinct[0]);
            let Some(evidence_value) = distinct.iter().find(|v| *v != &candidate_value) else {
                // 不可达（组内 ≥2 个不同值且候选值必属其中）；防御式跳过不 panic。
                // Unreachable (a group holds ≥2 distinct values and the candidate
                // value is one of them); defensively skipped instead of panicking.
                continue;
            };
            findings.push(ConsistencyFinding {
                code: VALUE_DIVERGENCE.to_string(),
                key: ClaimKey {
                    entity_id: entity_id.clone(),
                    pointer: pointer.clone(),
                },
                candidate_value_hash: blake3::hash(candidate_value).to_hex().to_string(),
                evidence_value_hash: blake3::hash(evidence_value).to_hex().to_string(),
            });
        }

        let score = if compared_groups == 0 {
            None
        } else {
            Some(equal_groups as f32 / compared_groups as f32)
        };
        Ok(ConsistencyReport {
            score,
            compared_claims: compared_groups,
            findings,
            candidate_count: related.len() as u32,
        })
    }
}

/// 单个可比较 source-ref 证据（共享给确定性与 LLM 仲裁器；值保持原文供语义判断）。
/// One comparable source-ref evidence (shared by the deterministic and LLM
/// arbiters; the value stays raw for semantic judgement).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparableRef {
    pub entity_id: String,
    pub pointer: String,
    pub value: serde_json::Value,
}

/// 收集单页可比较 source-ref（仅允许列表内且 `/fields/...` 形状，D3），值保持
/// 原文。供 [`LlmConsistencyArbiter`] 组装 LLM 输入——证据过滤在核心侧完成，
/// LLM 只做矛盾判定，不绕过证据契约。
/// Collects a page's comparable source-refs (allowlisted AND `/fields/...`
/// shaped, D3) with raw values. Used by [`LlmConsistencyArbiter`] to build its
/// LLM input — evidence filtering happens on the core side, the LLM only judges
/// contradiction, never bypassing the evidence contract.
pub(crate) fn comparable_refs(
    page: &CompiledPage,
    policy: &ConsistencyPolicy,
) -> Vec<ComparableRef> {
    let mut out = Vec::new();
    let Some(evidence) = &page.evidence else {
        return out;
    };
    for section in &evidence.sections {
        for r in &section.refs {
            if !is_comparable_pointer(&r.pointer, &policy.compare_pointers) {
                continue;
            }
            out.push(ComparableRef {
                entity_id: r.entity_id.clone(),
                pointer: r.pointer.clone(),
                value: r.value.clone(),
            });
        }
    }
    out
}

/// LLM 一致性仲裁器（Step14 P3-A）：复用 [`crate::compile::llm::LlmClient`] 做
/// 语义矛盾判定。**不绕过证据/top-k/预算契约**——只把核心侧过滤后的可比较
/// source-ref（`compare_pointers` 声明且 `/fields/...` 形状）送给模型；模型按
/// `(entity_id, pointer)` 判定 equal/divergent；核心据此计算 score 并对 divergent
/// 值算 BLAKE3 摘要组装 finding（原文不落诊断）。单次请求输出恰好一次。
/// LLM consistency arbiter (Step14 P3-A): reuses
/// [`crate::compile::llm::LlmClient`] for semantic contradiction judgement. It
/// **does not bypass the evidence/top-k/budget contracts** — only core-side
/// filtered comparable source-refs (allowlisted AND `/fields/...` shaped) reach
/// the model; the model judges equal/divergent per `(entity_id, pointer)`, and
/// the core computes the score and the BLAKE3 digests for divergent values
/// (raw values never reach diagnostics). Exactly one request per arbitration.
#[cfg(feature = "llm-openai")]
pub struct LlmConsistencyArbiter {
    client: Arc<dyn crate::compile::llm::LlmClient>,
    model: String,
    max_output_tokens: u32,
}

#[cfg(feature = "llm-openai")]
impl LlmConsistencyArbiter {
    /// 绑定 LLM 客户端与模型构造仲裁器。
    /// Builds the arbiter bound to an LLM client and model.
    pub fn new(
        client: Arc<dyn crate::compile::llm::LlmClient>,
        model: String,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            client,
            model,
            max_output_tokens,
        }
    }
}

#[cfg(feature = "llm-openai")]
#[async_trait]
impl ConsistencyArbiter for LlmConsistencyArbiter {
    async fn arbitrate(
        &self,
        candidate: &CompiledPage,
        related: &[CompiledPage],
        policy: &ConsistencyPolicy,
    ) -> Result<ConsistencyReport> {
        // 核心侧过滤证据（同一 D3 规则）：LLM 只见可比较 source-ref。
        // Evidence filtered on the core side (the same D3 rule): the LLM only
        // ever sees comparable source-refs.
        let candidate_refs = comparable_refs(candidate, policy);
        let related_refs: Vec<ComparableRef> = related
            .iter()
            .flat_map(|p| comparable_refs(p, policy))
            .collect();
        let input = serde_json::json!({
            "candidate_refs": candidate_refs,
            "related_refs": related_refs,
            "compare_pointers": policy.compare_pointers,
        });
        let request = crate::compile::llm::LlmRequest {
            system: CONSISTENCY_SYSTEM_PROMPT.to_string(),
            input_json: serde_json::to_string(&input)?,
            model: self.model.clone(),
            max_output_tokens: self.max_output_tokens,
            timeout_seconds: crate::compile::llm::DEFAULT_TIMEOUT_SECONDS,
        };
        let response = self
            .client
            .complete(request)
            .await
            .map_err(Error::CompileFailure)?;
        let verdicts = parse_consistency_verdicts(&response.json)?;
        // 核心按判定组装报告：equal/divergent 计数 → score；divergent 组对
        // candidate 出现的最小规范化值算 BLAKE3 摘要（不落原文）。
        // The core assembles the report from the verdicts: equal/divergent counts
        // → score; divergent groups hash the candidate's smallest canonical value
        // (raw never reaches diagnostics).
        let mut compared = 0u32;
        let mut equal = 0u32;
        let mut findings: Vec<ConsistencyFinding> = Vec::new();
        for v in verdicts {
            compared += 1;
            if v.status == VerdictStatus::Equal {
                equal += 1;
                continue;
            }
            let candidate_hash = candidate_refs
                .iter()
                .filter(|r| r.entity_id == v.entity_id && r.pointer == v.pointer)
                .map(|r| canonical_json(&r.value))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min()
                .unwrap_or_default();
            let evidence_hash = related_refs
                .iter()
                .filter(|r| r.entity_id == v.entity_id && r.pointer == v.pointer)
                .map(|r| canonical_json(&r.value))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .min()
                .unwrap_or_default();
            findings.push(ConsistencyFinding {
                code: VALUE_DIVERGENCE.to_string(),
                key: ClaimKey {
                    entity_id: v.entity_id,
                    pointer: v.pointer,
                },
                candidate_value_hash: blake3::hash(&candidate_hash).to_hex().to_string(),
                evidence_value_hash: blake3::hash(&evidence_hash).to_hex().to_string(),
            });
        }
        let score = if compared == 0 {
            None
        } else {
            Some(equal as f32 / compared as f32)
        };
        Ok(ConsistencyReport {
            score,
            compared_claims: compared,
            findings,
            candidate_count: related.len() as u32,
        })
    }
}

/// LLM 裁决 JSON 契约（§6.1 形状兼容）：模型对每个比较键给 equal/divergent。
/// LLM verdict JSON contract (§6.1-shape compatible): the model gives
/// equal/divergent per comparison key.
#[cfg(feature = "llm-openai")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum VerdictStatus {
    Equal,
    Divergent,
}

#[cfg(feature = "llm-openai")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsistencyVerdict {
    entity_id: String,
    pointer: String,
    #[serde(rename = "status")]
    status: VerdictStatus,
}

/// 解析模型响应为裁决列表（serde 强校验；不可解析 → `Validation`，不静默跳过）。
/// Parses the model response into verdicts (strict serde; unparseable →
/// `Validation`, never silently skipped).
#[cfg(feature = "llm-openai")]
fn parse_consistency_verdicts(json: &str) -> Result<Vec<ConsistencyVerdict>> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Validation(format!("consistency LLM verdicts: {e}")))?;
    let verdicts = v
        .get("verdicts")
        .ok_or_else(|| {
            Error::Validation("consistency LLM verdicts missing `verdicts`".to_string())
        })?
        .as_array()
        .ok_or_else(|| Error::Validation("consistency LLM `verdicts` not an array".to_string()))?;
    let list: Vec<ConsistencyVerdict> =
        serde_json::from_value(serde_json::Value::Array(verdicts.clone()))
            .map_err(|e| Error::Validation(format!("consistency LLM verdict entry: {e}")))?;
    Ok(list)
}

/// LLM 一致性裁决的 system prompt（约束只判可比较 source-ref 的语义矛盾）。
/// The LLM consistency-judgement system prompt (constraining to semantic
/// contradiction over comparable source-refs only).
#[cfg(feature = "llm-openai")]
pub const CONSISTENCY_SYSTEM_PROMPT: &str = "You are a fact-consistency arbiter. \
You are given a candidate page's source-refs and related pages' source-refs, \
each keyed by (entity_id, pointer). Judge, per distinct (entity_id, pointer) \
that appears in BOTH the candidate and at least one related page, whether the \
semantic values agree. Respond with a single JSON object of the shape \
{\"verdicts\":[{\"entity_id\":\"...\",\"pointer\":\"/fields/...\",\"status\":\"equal\"|\"divergent\"}]}. \
Only include keys that appear in both candidate and related refs. Do not add \
prose. If nothing is comparable, return {\"verdicts\":[]}.";

/// 组内一次值出现：规范化字节 + 是否来自候选页。
/// One value occurrence within a group: canonical bytes plus candidate origin.
struct ClaimOccurrence {
    value: Vec<u8>,
    from_candidate: bool,
}

/// 收集单页 evidence 中可比较的 source-ref（§6.1 步骤 1–2）。
/// Collects the comparable source-refs from one page's evidence (§6.1 items 1-2).
///
/// 无 evidence（旧 seed/legacy 页）整页跳过；canonical_json 拒绝非有限数值 →
/// `Validation`（fail-closed，不猜测等价）。
/// Pages without evidence (legacy seed) are skipped whole; canonical_json
/// rejects non-finite numbers → `Validation` (fail-closed, no guessed
/// equivalence).
fn collect_page_refs(
    page: &CompiledPage,
    from_candidate: bool,
    policy: &ConsistencyPolicy,
    groups: &mut BTreeMap<(String, String), Vec<ClaimOccurrence>>,
) -> Result<()> {
    let Some(evidence) = &page.evidence else {
        return Ok(());
    };
    // 页内去重：同键同值只计一次出现；同键不同值各自保留。
    // Within-page dedupe: same key + value counts once; same key with distinct
    // values are all kept.
    let mut per_page: BTreeMap<(String, String), BTreeSet<Vec<u8>>> = BTreeMap::new();
    for section in &evidence.sections {
        for r in &section.refs {
            if !is_comparable_pointer(&r.pointer, &policy.compare_pointers) {
                continue;
            }
            let bytes = canonical_json(&r.value).map_err(|e| {
                Error::Validation(format!(
                    "consistency arbitration: ref {} at {}: {e}",
                    r.id, r.pointer
                ))
            })?;
            per_page
                .entry((r.entity_id.clone(), r.pointer.clone()))
                .or_default()
                .insert(bytes);
        }
    }
    for ((entity_id, pointer), values) in per_page {
        let group = groups.entry((entity_id, pointer)).or_default();
        for value in values {
            group.push(ClaimOccurrence {
                value,
                from_candidate,
            });
        }
    }
    Ok(())
}

/// 可比较指针判定（D3）：在显式允许列表内且满足 RFC6901 `/fields/...` 形状。
/// Comparable-pointer test (D3): inside the explicit allowlist AND shaped as an
/// RFC6901 `/fields/...` pointer.
pub(crate) fn is_comparable_pointer(pointer: &str, compare_pointers: &[String]) -> bool {
    const FIELDS_PREFIX: &str = "/fields/";
    pointer.len() > FIELDS_PREFIX.len()
        && pointer.starts_with(FIELDS_PREFIX)
        && compare_pointers.iter().any(|p| p == pointer)
}

/// 默认相关页提供器：SQLite FTS 有界召回（§6.1 provider 段）。
/// The default related-page provider: bounded SQLite FTS recall (§6.1 provider
/// paragraph).
///
/// 从 candidate 标题与 aliases 生成经 [`MAX_FTS_QUERY_CHARS`] 长度上限的 FTS
/// 查询，SQL 全落 kernel（[`SqliteKernel::top_k_related_pages`]，锁一次不跨
/// await）；`limit` 在此钳制到 1..=[`MAX_CONSISTENCY_TOP_K`]（A4）。
/// Builds an FTS query capped at [`MAX_FTS_QUERY_CHARS`] from the candidate's
/// title and aliases; all SQL lives in the kernel
/// ([`SqliteKernel::top_k_related_pages`], one lock, never across await); the
/// `limit` is clamped to 1..=[`MAX_CONSISTENCY_TOP_K`] here (A4).
#[derive(Clone)]
pub struct SqliteFtsCandidateProvider {
    kernel: Arc<SqliteKernel>,
}

impl SqliteFtsCandidateProvider {
    /// 绑定内核构造默认 provider。
    /// Builds the default provider bound to a kernel.
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

#[async_trait]
impl ConsistencyCandidateProvider for SqliteFtsCandidateProvider {
    async fn top_k_related(
        &self,
        candidate: &CompiledPage,
        limit: u32,
    ) -> Result<Vec<CompiledPage>> {
        // 防御性钳制：即使调用方越过策略校验，provider 最多请求 32（A4）。
        // Defensive clamp: even if a caller bypassed policy validation, the
        // provider requests at most 32 (A4).
        let limit = limit.clamp(1, MAX_CONSISTENCY_TOP_K);
        let terms = fts_query_terms(&candidate.wiki.title, &candidate.wiki.aliases);
        if terms.is_empty() {
            // 无可用查询词：FTS 无从命中，直接返回空集合（不发起 SQL）。
            // No usable query terms: FTS cannot match, return empty without SQL.
            return Ok(Vec::new());
        }
        let kernel = self.kernel.clone();
        let exclude_page_id = candidate.wiki.page_id.clone();
        // 同步 kernel 调用走 spawn_blocking（仓库惯例：不持锁跨 await）。
        // The synchronous kernel call goes through spawn_blocking (repo
        // convention: never hold a lock across await).
        tokio::task::spawn_blocking(move || {
            kernel.top_k_related_pages(&terms, &exclude_page_id, limit)
        })
        .await
        .map_err(|e| Error::Internal(format!("consistency provider task panicked: {e}")))?
    }
}

/// 从标题与 aliases 生成 FTS 查询词（§6.1 provider 段）。
/// Builds FTS query terms from the title and aliases (§6.1 provider paragraph).
///
/// 规则：标题优先、aliases 保序跟随；按字符数去重；短于 3 字符的词跳过
/// （trigram 分词器 MATCH 下限）；累计预算（含引号与 ` OR ` 分隔）超过
/// [`MAX_FTS_QUERY_CHARS`] 即停止（前缀截断，确定性）。返回原始词，引号转义由
/// kernel 的 SQL 构造统一处理。
/// Rules: title first, aliases in order; deduped by chars; terms shorter than 3
/// chars are skipped (the trigram MATCH floor); once the running budget
/// (including quotes and ` OR ` separators) exceeds [`MAX_FTS_QUERY_CHARS`] the
/// build stops (deterministic prefix truncation). Raw terms are returned; quote
/// escaping is handled uniformly by the kernel's SQL construction.
pub fn fts_query_terms(title: &str, aliases: &[String]) -> Vec<String> {
    let mut candidates: Vec<&str> = Vec::new();
    let title = title.trim();
    if !title.is_empty() {
        candidates.push(title);
    }
    for alias in aliases {
        let alias = alias.trim();
        if !alias.is_empty() {
            candidates.push(alias);
        }
    }
    let mut terms: Vec<String> = Vec::new();
    let mut used = 0usize;
    for raw in candidates {
        if raw.chars().count() < 3 {
            continue;
        }
        if terms.iter().any(|t| t == raw) {
            continue;
        }
        let cost = raw.chars().count() + 2; // 两侧引号 / surrounding quotes
        let separator = if terms.is_empty() { 0 } else { 4 }; // " OR "
        if used + separator + cost > MAX_FTS_QUERY_CHARS {
            break;
        }
        used += separator + cost;
        terms.push(raw.to_string());
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::contract::{CompileEvidence, EvidenceSection, OutputWiki, SourceRef};
    use crate::types::{EntityId, PageMetadata, PublishStatus, QualityScore, Section, WikiPage};

    const PEARL: &str = "milk-tea:ingredient:pearl";
    const NAME_PTR: &str = "/fields/name";
    const DESC_PTR: &str = "/fields/description";

    // —— 构造辅助 / construction helpers ——

    fn source_ref(id: &str, entity: &str, pointer: &str, value: &str) -> SourceRef {
        SourceRef {
            id: id.to_string(),
            entity_id: entity.to_string(),
            source_revision: 1,
            pointer: pointer.to_string(),
            value: serde_json::json!(value),
            quote: value.to_string(),
        }
    }

    fn evidence(refs: Vec<SourceRef>) -> CompileEvidence {
        CompileEvidence {
            schema_version: "source-ref-v1".to_string(),
            wiki: OutputWiki {
                title: String::new(),
                aliases: vec![],
                tags: vec![],
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading: "概述".to_string(),
                assertions: vec![],
                refs,
            }],
            usage: None,
        }
    }

    fn page(page_id: &str, title: &str, ev: Option<CompileEvidence>) -> CompiledPage {
        CompiledPage {
            wiki: WikiPage {
                page_id: page_id.to_string(),
                entity_id: EntityId::from_key(page_id).unwrap(),
                title: title.to_string(),
                content: String::new(),
                sections: vec![Section {
                    heading: "概述".to_string(),
                    content: String::new(),
                }],
                metadata: PageMetadata {
                    domain_pack_version: "0.1.0".to_string(),
                    compiled_at: 0,
                    model_version: "mock-v1".to_string(),
                    embedding_model: "none".to_string(),
                },
                aliases: vec![],
                tags: vec![],
            },
            quality: QualityScore {
                coverage: 0.0,
                citation: 0.0,
                schema_compliance: 0.0,
                density: 0.0,
                consistency: None,
            },
            qug_edges: Vec::new(),
            content_hash: String::new(),
            evidence: ev,
        }
    }

    fn policy(pointers: &[&str]) -> ConsistencyPolicy {
        ConsistencyPolicy {
            enabled: true,
            top_k: 8,
            min_consistency: 1.0,
            compare_pointers: pointers.iter().map(|s| s.to_string()).collect(),
        }
    }

    // —— A5：精确证据 / exact evidence ——

    // A5：相同 (entity_id, pointer) 相同 canonical value → 得分 1、无 finding。
    // A5: identical (entity_id, pointer) with identical canonical value → score 1,
    // no finding.
    #[tokio::test]
    async fn identical_values_score_one() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![source_ref("r1", PEARL, NAME_PTR, "珍珠")])),
        );
        let related = vec![page(
            "milk-tea:drink:milk-tea",
            "奶茶",
            Some(evidence(vec![source_ref("r9", PEARL, NAME_PTR, "珍珠")])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, Some(1.0));
        assert_eq!(report.compared_claims, 1);
        assert!(report.findings.is_empty());
        assert_eq!(report.candidate_count, 1);
    }

    // A5：不同 value → VALUE_DIVERGENCE、得分 0；诊断只含 64 位小写 hex 摘要，
    // 原文不落报告。
    // A5: distinct values → VALUE_DIVERGENCE with score 0; diagnostics carry
    // 64-char lowercase hex digests only, raw values never reach the report.
    #[tokio::test]
    async fn divergent_values_produce_finding_with_hashes_only() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![source_ref("r1", PEARL, NAME_PTR, "珍珠")])),
        );
        let related = vec![page(
            "milk-tea:drink:milk-tea",
            "奶茶",
            Some(evidence(vec![source_ref("r9", PEARL, NAME_PTR, "波霸")])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, Some(0.0));
        assert_eq!(report.compared_claims, 1);
        assert_eq!(report.findings.len(), 1);
        let finding = &report.findings[0];
        assert_eq!(finding.code, VALUE_DIVERGENCE);
        assert_eq!(finding.key.entity_id, PEARL);
        assert_eq!(finding.key.pointer, NAME_PTR);
        for hash in [&finding.candidate_value_hash, &finding.evidence_value_hash] {
            assert_eq!(hash.len(), 64);
            assert!(hash
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
        assert_ne!(finding.candidate_value_hash, finding.evidence_value_hash);
        assert!(!format!("{report:?}").contains("珍珠"));
    }

    // A5：标题相似但无可比较证据重叠 → None（不判 0、不 fail-closed）。
    // A5: similar titles without overlapping comparable evidence → None (neither
    // zero nor fail-closed).
    #[tokio::test]
    async fn similar_titles_without_ref_overlap_score_none() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵奶茶",
            Some(evidence(vec![source_ref("r1", PEARL, NAME_PTR, "珍珠")])),
        );
        let related = vec![page(
            "milk-tea:drink:boba-tea",
            "啵啵奶茶王",
            Some(evidence(vec![source_ref("r9", PEARL, DESC_PTR, "珍珠")])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR, DESC_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert_eq!(report.compared_claims, 0);
        assert!(report.findings.is_empty());
    }

    // compare_pointers 为空（enabled=true 但指针表空）→ 确定性 None、零 finding
    // （无可比较证据即无结论，§6.1 compared_claims=0 → None）。
    // Empty compare_pointers (enabled=true with an empty pointer table) →
    // deterministic None with zero findings (no comparable evidence means no
    // conclusion, §6.1 compared_claims=0 → None).
    #[tokio::test]
    async fn empty_pointer_table_is_deterministic_none() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![source_ref("r1", PEARL, NAME_PTR, "珍珠")])),
        );
        let related = vec![page(
            "milk-tea:drink:milk-tea",
            "奶茶",
            Some(evidence(vec![source_ref("r9", PEARL, NAME_PTR, "珍珠")])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert_eq!(report.compared_claims, 0);
        assert!(report.findings.is_empty());
    }

    // —— A7：证据安全 / evidence safety ——

    // A7：不同 domain 的实体键各自成组 → 不比较。
    // A7: entity keys from different domains form separate groups → no
    // comparison.
    #[tokio::test]
    async fn cross_domain_keys_never_compare() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![source_ref(
                "r1",
                "milk-tea:ingredient:pearl",
                NAME_PTR,
                "珍珠",
            )])),
        );
        let related = vec![page(
            "shop:drink:tea",
            "奶茶",
            Some(evidence(vec![source_ref(
                "r9",
                "shop:ingredient:pearl",
                NAME_PTR,
                "珍珠",
            )])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert!(report.findings.is_empty());
    }

    // A7：非 /fields/ 形状的 pointer 即使在允许列表内也跳过。
    // A7: pointers outside the /fields/ shape are skipped even when allowlisted.
    #[tokio::test]
    async fn non_fields_pointers_are_skipped() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![source_ref(
                "r1",
                PEARL,
                "/meta/name",
                "珍珠",
            )])),
        );
        let related = vec![page(
            "milk-tea:drink:milk-tea",
            "奶茶",
            Some(evidence(vec![source_ref(
                "r9",
                PEARL,
                "/meta/name",
                "珍珠",
            )])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR, "/meta/name"]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert!(report.findings.is_empty());
    }

    // A7：无 evidence（旧 seed 页）整页跳过，不猜测等价关系。
    // A7: pages without evidence (legacy seed) are skipped whole; equivalence is
    // never guessed.
    #[tokio::test]
    async fn pages_without_evidence_are_skipped() {
        let candidate = page("milk-tea:drink:boba", "啵啵", None);
        let related = vec![page("milk-tea:drink:seed", "seed", None)];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert_eq!(report.compared_claims, 0);
        assert_eq!(report.candidate_count, 1);
    }

    // 页内重复同值 ref 去重：候选单独携带重复 ref 只算一次出现 → 不算比较。
    // Within-page duplicate same-value refs dedupe: a candidate alone with
    // duplicated refs yields one occurrence → not a comparison.
    #[tokio::test]
    async fn duplicate_refs_within_one_page_are_not_self_compared() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![
                source_ref("r1", PEARL, NAME_PTR, "珍珠"),
                source_ref("r2", PEARL, NAME_PTR, "珍珠"),
            ])),
        );
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &[], &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, None);
        assert_eq!(report.compared_claims, 0);
    }

    // 候选同键两个不同值 → 自相矛盾被检出（比较发生且产生分歧）。
    // Two distinct values for one key on the candidate → self-contradiction
    // detected (a comparison happened and diverged).
    #[tokio::test]
    async fn self_contradicting_candidate_is_detected() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![
                source_ref("r1", PEARL, NAME_PTR, "珍珠"),
                source_ref("r2", PEARL, NAME_PTR, "波霸"),
            ])),
        );
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &[], &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, Some(0.0));
        assert_eq!(report.compared_claims, 1);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].code, VALUE_DIVERGENCE);
    }

    // 候选重复 ref + 相关页同值 → 去重后与相关页各一次出现 → 得分 1。
    // Candidate duplicates plus an agreeing related page → one occurrence each
    // after dedupe → score 1.
    #[tokio::test]
    async fn candidate_duplicates_plus_agreeing_related_score_one() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![
                source_ref("r1", PEARL, NAME_PTR, "珍珠"),
                source_ref("r2", PEARL, NAME_PTR, "珍珠"),
            ])),
        );
        let related = vec![page(
            "milk-tea:drink:milk-tea",
            "奶茶",
            Some(evidence(vec![source_ref("r9", PEARL, NAME_PTR, "珍珠")])),
        )];
        let report = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(report.score, Some(1.0));
    }

    // 多组比较：一组相等一组分歧 → 0.5；findings 按 (entity_id, pointer) 稳定
    // 排序；重复仲裁逐字段相同（确定性）。
    // Mixed groups: one equal, one divergent → 0.5; findings stably ordered by
    // (entity_id, pointer); repeated arbitration matches field-by-field
    // (determinism).
    #[tokio::test]
    async fn mixed_groups_and_determinism() {
        let candidate = page(
            "milk-tea:drink:boba",
            "啵啵",
            Some(evidence(vec![
                source_ref("r1", "a:ingredient:pearl", NAME_PTR, "珍珠"),
                source_ref("r2", "b:ingredient:pearl", NAME_PTR, "波霸"),
            ])),
        );
        let related = vec![
            page(
                "milk-tea:drink:milk-tea",
                "奶茶",
                Some(evidence(vec![source_ref(
                    "r9",
                    "a:ingredient:pearl",
                    NAME_PTR,
                    "珍珠",
                )])),
            ),
            page(
                "milk-tea:drink:cheese",
                "奶盖",
                Some(evidence(vec![source_ref(
                    "r8",
                    "b:ingredient:pearl",
                    NAME_PTR,
                    "奶盖",
                )])),
            ),
        ];
        let first = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        let second = SourceRefConsistencyArbiter
            .arbitrate(&candidate, &related, &policy(&[NAME_PTR]))
            .await
            .unwrap();
        assert_eq!(first.score, Some(0.5));
        assert_eq!(first.compared_claims, 2);
        assert_eq!(first.findings.len(), 1);
        assert_eq!(first.findings[0].key.entity_id, "b:ingredient:pearl");
        assert_eq!(first.score, second.score);
        assert_eq!(first.compared_claims, second.compared_claims);
        assert_eq!(first.candidate_count, second.candidate_count);
        for (a, b) in first.findings.iter().zip(&second.findings) {
            assert_eq!(a.code, b.code);
            assert_eq!(a.key, b.key);
            assert_eq!(a.candidate_value_hash, b.candidate_value_hash);
            assert_eq!(a.evidence_value_hash, b.evidence_value_hash);
        }
    }

    // —— A3：trait 可替换 / trait substitutability ——

    struct FakeArbiter {
        report: ConsistencyReport,
    }

    #[async_trait]
    impl ConsistencyArbiter for FakeArbiter {
        async fn arbitrate(
            &self,
            _candidate: &CompiledPage,
            _related: &[CompiledPage],
            _policy: &ConsistencyPolicy,
        ) -> Result<ConsistencyReport> {
            Ok(self.report.clone())
        }
    }

    struct FixedProvider {
        pages: Vec<CompiledPage>,
        seen_limits: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    #[async_trait]
    impl ConsistencyCandidateProvider for FixedProvider {
        async fn top_k_related(
            &self,
            _candidate: &CompiledPage,
            limit: u32,
        ) -> Result<Vec<CompiledPage>> {
            self.seen_limits.lock().unwrap().push(limit);
            Ok(self.pages.clone())
        }
    }

    // A3：fake arbiter 经 trait 对象返回 None/1/0，核心组合面不依赖具体实现。
    // A3: a fake arbiter behind a trait object returns None/1/0; the core
    // composition surface depends on no concrete implementation.
    #[tokio::test]
    async fn fake_arbiter_is_substitutable() {
        let none = Arc::new(FakeArbiter {
            report: ConsistencyReport::default(),
        }) as Arc<dyn ConsistencyArbiter>;
        let one = Arc::new(FakeArbiter {
            report: ConsistencyReport {
                score: Some(1.0),
                compared_claims: 1,
                findings: vec![],
                candidate_count: 0,
            },
        }) as Arc<dyn ConsistencyArbiter>;
        let zero = Arc::new(FakeArbiter {
            report: ConsistencyReport {
                score: Some(0.0),
                compared_claims: 1,
                findings: vec![],
                candidate_count: 0,
            },
        }) as Arc<dyn ConsistencyArbiter>;
        let candidate = page("milk-tea:drink:boba", "啵啵", None);
        let pol = policy(&[NAME_PTR]);
        assert_eq!(
            none.arbitrate(&candidate, &[], &pol).await.unwrap().score,
            None
        );
        assert_eq!(
            one.arbitrate(&candidate, &[], &pol).await.unwrap().score,
            Some(1.0)
        );
        assert_eq!(
            zero.arbitrate(&candidate, &[], &pol).await.unwrap().score,
            Some(0.0)
        );
    }

    // A4（前半）：provider 收到的请求量即策略 top_k；fake provider 可注入预构造页。
    // A4 (first half): the provider receives exactly the policy top_k; a fake
    // provider can inject pre-built pages.
    #[tokio::test]
    async fn fake_provider_receives_policy_top_k() {
        let seen_limits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FixedProvider {
            pages: vec![page("milk-tea:drink:x", "x", None)],
            seen_limits: seen_limits.clone(),
        }) as Arc<dyn ConsistencyCandidateProvider>;
        let candidate = page("milk-tea:drink:boba", "啵啵", None);
        let related = provider
            .top_k_related(&candidate, policy(&[NAME_PTR]).top_k)
            .await
            .unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(seen_limits.lock().unwrap().as_slice(), &[8]);
    }

    // —— FTS 查询词构造 / FTS query-term construction ——

    // 标题优先、aliases 保序跟随；页内去重；<3 字符跳过。
    // Title first, aliases in order; deduped; sub-3-char terms skipped.
    #[test]
    fn fts_terms_title_first_then_aliases() {
        let terms = fts_query_terms(
            "啵啵奶茶",
            &[
                "波霸奶茶".to_string(),
                "啵啵奶茶".to_string(),
                "茶".to_string(),
            ],
        );
        assert_eq!(terms, vec!["啵啵奶茶".to_string(), "波霸奶茶".to_string()]);
    }

    // 预算：累计引号 + OR 分隔不超上限；超出即前缀截断（确定性）。
    // Budget: running quotes + OR separators stay under the cap; overflow stops
    // with a deterministic prefix.
    #[test]
    fn fts_terms_respect_length_budget() {
        let aliases: Vec<String> = (0..64).map(|i| format!("别名{i:04}xxxxxxxxxxxx")).collect();
        let terms = fts_query_terms("珍珠奶茶", &aliases);
        assert!(!terms.is_empty());
        assert!(terms.len() < 64);
        let total: usize =
            terms.iter().map(|t| t.chars().count() + 2).sum::<usize>() + (terms.len() - 1) * 4;
        assert!(total <= MAX_FTS_QUERY_CHARS, "total {total} over budget");
        // 重复调用结果一致。/ Repeated calls agree.
        assert_eq!(terms, fts_query_terms("珍珠奶茶", &aliases));
    }

    // 空输入 / 空白 / 过短词 → 空集合（provider 不发起 SQL）。
    // Empty input / whitespace / too-short terms → empty (the provider issues no
    // SQL).
    #[test]
    fn fts_terms_empty_inputs() {
        assert!(fts_query_terms("", &[]).is_empty());
        assert!(fts_query_terms("  ", &["  ".to_string()]).is_empty());
        assert!(fts_query_terms("茶", &[]).is_empty());
    }

    // —— 默认 provider 的 SQLite 面（A4/A8）——
    // —— The default provider's SQLite surface (A4/A8) ——

    fn seed_page(
        kernel: &SqliteKernel,
        page_id: &str,
        title: &str,
        content: &str,
        status: PublishStatus,
    ) {
        let wiki = WikiPage {
            page_id: page_id.to_string(),
            entity_id: EntityId::from_key(page_id).unwrap(),
            title: title.to_string(),
            content: content.to_string(),
            sections: vec![Section {
                heading: "概述".to_string(),
                content: content.to_string(),
            }],
            metadata: PageMetadata {
                domain_pack_version: "0.1.0".to_string(),
                compiled_at: 0,
                model_version: "mock-v1".to_string(),
                embedding_model: "none".to_string(),
            },
            aliases: vec![],
            tags: vec![],
        };
        kernel.seed_pages(&wiki, "milk-tea", status).unwrap();
    }

    // A4：100 个匹配页只返回 top_k（LIMIT 行为证明）；provider 侧钳制最多 32；
    // 候选自身不重复计入；同题同文平分按 page_id 升序、两次调用同序。
    // A4: 100 matching pages yield only top_k (behavioral proof of LIMIT); the
    // provider clamps at 32; the candidate itself is never double-counted; with
    // identical titles/content ties order by page_id ascending, stable across
    // calls.
    #[tokio::test]
    async fn default_provider_is_bounded_and_stable() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        for i in 0..100 {
            seed_page(
                &kernel,
                &format!("milk-tea:drink:b{i:03}"),
                "珍珠奶茶",
                "特调",
                PublishStatus::Accepted,
            );
        }
        let provider = SqliteFtsCandidateProvider::new(kernel);
        let candidate = page("milk-tea:drink:boba", "珍珠奶茶", None);
        let related = provider.top_k_related(&candidate, 8).await.unwrap();
        assert_eq!(related.len(), 8);
        assert!(related
            .iter()
            .all(|p| p.wiki.page_id != candidate.wiki.page_id));
        let ids: Vec<&str> = related.iter().map(|p| p.wiki.page_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "ties must order by page_id ascending");
        let again = provider.top_k_related(&candidate, 8).await.unwrap();
        let again_ids: Vec<&str> = again.iter().map(|p| p.wiki.page_id.as_str()).collect();
        assert_eq!(ids, again_ids, "ordering must be stable across calls");
        // 上限钳制：请求 100 也最多返回 32（A4：provider 收到且最多请求 32）。
        // Cap clamp: requesting 100 still returns at most 32 (A4: the provider
        // receives and requests at most 32).
        let capped = provider.top_k_related(&candidate, 100).await.unwrap();
        assert_eq!(capped.len(), 32);
        // 无查询词（空白标题与别名）→ 空集合、不发起 SQL。
        // No query terms (blank title/aliases) → empty set, no SQL issued.
        let blank = page("milk-tea:drink:blank", "  ", None);
        assert!(provider.top_k_related(&blank, 8).await.unwrap().is_empty());
    }

    // A8：quarantined / candidate 状态页与候选自身（旧 generation 行）不进
    // related；seed 页无 evidence 原样返回（不伪造回填）。
    // A8: quarantined / candidate-status pages and the candidate's own (old
    // generation) row never become related; seed pages return evidence=None
    // verbatim (never fabricated back).
    #[tokio::test]
    async fn default_provider_excludes_non_accepted_and_candidate() {
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        seed_page(
            &kernel,
            "milk-tea:drink:boba",
            "珍珠奶茶",
            "旧代",
            PublishStatus::Accepted,
        );
        seed_page(
            &kernel,
            "milk-tea:drink:quar",
            "珍珠奶茶",
            "隔离",
            PublishStatus::Quarantined,
        );
        seed_page(
            &kernel,
            "milk-tea:drink:cand",
            "珍珠奶茶",
            "候选",
            PublishStatus::Candidate,
        );
        for i in 0..10 {
            seed_page(
                &kernel,
                &format!("milk-tea:drink:g{i}"),
                "珍珠奶茶",
                "特调",
                PublishStatus::Accepted,
            );
        }
        let provider = SqliteFtsCandidateProvider::new(kernel);
        let candidate = page("milk-tea:drink:boba", "珍珠奶茶", None);
        let related = provider.top_k_related(&candidate, 32).await.unwrap();
        let ids: std::collections::HashSet<&str> =
            related.iter().map(|p| p.wiki.page_id.as_str()).collect();
        assert_eq!(ids.len(), related.len(), "page_ids must be unique");
        assert!(!ids.contains("milk-tea:drink:boba"));
        assert!(!ids.contains("milk-tea:drink:quar"));
        assert!(!ids.contains("milk-tea:drink:cand"));
        assert_eq!(related.len(), 10);
        assert!(related.iter().all(|p| p.evidence.is_none()));
    }

    // —— Step14 P3-A：LLM 仲裁器（feature llm-openai）——
    // —— Step14 P3-A: the LLM arbiter (feature llm-openai) ——

    #[cfg(feature = "llm-openai")]
    mod llm_arbiter {
        use super::*;
        use crate::compile::llm::{LlmClient, LlmRequest, LlmResponse};
        use crate::compile::CompileFailure;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockLlm {
            response: std::result::Result<String, CompileFailure>,
            calls: AtomicUsize,
            last_input: std::sync::Mutex<Option<String>>,
        }

        impl MockLlm {
            fn ok(json: &str) -> Self {
                Self {
                    response: Ok(json.to_string()),
                    calls: AtomicUsize::new(0),
                    last_input: std::sync::Mutex::new(None),
                }
            }
        }

        #[async_trait]
        impl LlmClient for MockLlm {
            async fn complete(
                &self,
                request: LlmRequest,
            ) -> std::result::Result<LlmResponse, CompileFailure> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                *self.last_input.lock().unwrap() = Some(request.input_json);
                match &self.response {
                    Ok(json) => Ok(LlmResponse {
                        json: json.clone(),
                        usage: None,
                    }),
                    Err(f) => Err(f.clone()),
                }
            }
        }

        // 构造预置的裁决 JSON。
        // Builds a canned verdicts JSON.
        fn verdict_json(statuses: &[(&str, &str, &str)]) -> String {
            let arr: Vec<serde_json::Value> = statuses
                .iter()
                .map(|(e, p, s)| serde_json::json!({ "entity_id": e, "pointer": p, "status": s }))
                .collect();
            serde_json::to_string(&serde_json::json!({ "verdicts": arr })).unwrap()
        }

        fn llm_arbiter(mock: Arc<MockLlm>, tokens: u32) -> LlmConsistencyArbiter {
            LlmConsistencyArbiter::new(mock, "qwen3.8-max".to_string(), tokens)
        }

        // P3-A2：LLM 判定 equal → 得分 1、无 finding；判定 divergent → 得分 0、
        // finding 只含 BLAKE3 摘要（原文不落诊断）。
        // P3-A2: an LLM verdict of equal → score 1 with no findings; divergent →
        // score 0 with a finding carrying BLAKE3 digests only (raw never logged).
        #[tokio::test]
        async fn llm_verdicts_map_to_report() {
            let mock = Arc::new(MockLlm::ok(&verdict_json(&[
                ("milk-tea:ingredient:pearl", "/fields/name", "equal"),
                (
                    "milk-tea:ingredient:pearl",
                    "/fields/description",
                    "divergent",
                ),
            ])));
            let arbiter = llm_arbiter(mock.clone(), 512);
            let candidate = page(
                "milk-tea:drink:boba",
                "啵啵",
                Some(evidence(vec![
                    source_ref("r1", PEARL, NAME_PTR, "珍珠"),
                    source_ref("r2", PEARL, DESC_PTR, "波霸"),
                ])),
            );
            let related = vec![page(
                "milk-tea:drink:milk-tea",
                "奶茶",
                Some(evidence(vec![
                    source_ref("r9", PEARL, NAME_PTR, "珍珠"),
                    source_ref("r8", PEARL, DESC_PTR, "黑珍珠"),
                ])),
            )];
            let report = arbiter
                .arbitrate(&candidate, &related, &policy(&[NAME_PTR, DESC_PTR]))
                .await
                .unwrap();
            assert_eq!(report.score, Some(0.5));
            assert_eq!(report.compared_claims, 2);
            assert_eq!(report.findings.len(), 1);
            let f = &report.findings[0];
            assert_eq!(f.code, VALUE_DIVERGENCE);
            assert_eq!(f.key.entity_id, PEARL);
            assert_eq!(f.key.pointer, DESC_PTR);
            assert_eq!(f.candidate_value_hash.len(), 64);
            assert_eq!(f.evidence_value_hash.len(), 64);
            assert!(!format!("{report:?}").contains("波霸"));
            assert_eq!(mock.calls.load(Ordering::Relaxed), 1);
        }

        // P3-A2：无可比较键（LLM 返回空 verdicts）→ score None、零 finding。
        // P3-A2: no comparable keys (LLM returns empty verdicts) → score None,
        // zero findings.
        #[tokio::test]
        async fn llm_empty_verdicts_score_none() {
            let mock = Arc::new(MockLlm::ok(&verdict_json(&[])));
            let arbiter = llm_arbiter(mock.clone(), 512);
            let candidate = page("milk-tea:drink:boba", "啵啵", None);
            let report = arbiter
                .arbitrate(&candidate, &[], &policy(&[NAME_PTR]))
                .await
                .unwrap();
            assert_eq!(report.score, None);
            assert_eq!(report.compared_claims, 0);
            assert!(report.findings.is_empty());
        }

        // P3-A2：不可解析的模型响应 → Validation（fail-closed，不静默跳过）。
        // P3-A2: an unparseable model response → Validation (fail-closed, never
        // silently skipped).
        #[tokio::test]
        async fn llm_unparseable_response_is_validation() {
            let mock = Arc::new(MockLlm::ok("not json"));
            let arbiter = llm_arbiter(mock.clone(), 512);
            let candidate = page("milk-tea:drink:boba", "啵啵", None);
            let err = arbiter
                .arbitrate(&candidate, &[], &policy(&[NAME_PTR]))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        }

        // P3-A2：证据过滤在核心侧——不在 compare_pointers 或非 /fields/ 形状的
        // ref 不进 LLM 输入（不绕过证据契约）。
        // P3-A2: evidence filtering stays on the core side — refs outside
        // compare_pointers or non-/fields/ shaped never reach the LLM input
        // (the evidence contract is not bypassed).
        #[tokio::test]
        async fn llm_input_only_carries_comparable_refs() {
            let mock = Arc::new(MockLlm::ok(&verdict_json(&[])));
            let arbiter = llm_arbiter(mock.clone(), 512);
            let candidate = page(
                "milk-tea:drink:boba",
                "啵啵",
                Some(evidence(vec![
                    source_ref("r1", PEARL, NAME_PTR, "珍珠"),
                    source_ref("r2", PEARL, "/meta/x", "不该送"),
                    source_ref("r3", PEARL, "/fields/sugar", "不在允许表"),
                ])),
            );
            let report = arbiter
                .arbitrate(&candidate, &[], &policy(&[NAME_PTR]))
                .await
                .unwrap();
            assert_eq!(report.score, None);
            let input = mock.last_input.lock().unwrap().clone().unwrap();
            assert!(input.contains("/fields/name"));
            assert!(!input.contains("/meta/x"));
            assert!(!input.contains("不在允许表"));
            assert!(!input.contains("不该送"));
        }
    }
}
