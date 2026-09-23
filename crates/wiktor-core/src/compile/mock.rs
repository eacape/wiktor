//! MockCompiler（Step 4 spec §4，决策 D1）：无 key、无网络、离线确定性编译。
//! MockCompiler (Step 4 spec §4, decision D1): keyless, networkless, offline
//! deterministic compilation.
//!
//! 契约要点：
//! - 与真实 LLM 走**完全相同**的输出契约：从知识快照允许字段确定性生成
//!   source-ref-v1 envelope（断言 text=字段值、ref 指向该字段指针、quote=字段
//!   原值），canonical Markdown 一并写入 envelope，再经 [`decode_response`]
//!   自洽解码，usage=None。
//! - content_hash/quality/metadata 填占位（§4：模型产物不可信，由 executor
//!   重算/填写）；Mock 不填 content_hash（空串）。
//! - 空知识源 → 模拟模型返回 error envelope，经 decode 映射为
//!   `CompileFailure::InvalidOutput`（与真实模型行为同构）。
//! - [`MockBehavior`] 脚本式变体供 A11/A12 注入测试：低质候选（缺抽取式
//!   一致性）/ 可重试传输失败 / 永久失败。Mock 不走网络；预算计量在 claim
//!   事务（kernel），Mock 不可能绕过刹车。
//!
//! Contract highlights:
//! - Follows the **exact** output contract of a real LLM: deterministically
//!   generates a source-ref-v1 envelope from the knowledge snapshot's allowed
//!   fields (assertion text=field value, ref points at that field pointer,
//!   quote=the original value), embeds the canonical Markdown into the
//!   envelope, then round-trips through [`decode_response`], with usage=None.
//! - content_hash/quality/metadata are placeholders (§4: model output is
//!   untrusted and recomputed/filled by the executor); the Mock leaves
//!   content_hash empty.
//! - An empty knowledge source mimics a model error envelope mapped through
//!   decode into `CompileFailure::InvalidOutput` (isomorphic to a real model).
//! - [`MockBehavior`] scripted variants serve A11/A12 injection tests:
//!   low-quality candidates (breaking the extractive constraint) / retryable
//!   transport failure / permanent failure. The Mock never touches the network;
//!   budget metering lives in the claim transaction (kernel), so the Mock
//!   cannot bypass the brakes.

use crate::compile::config::{CompilePolicy, ENVELOPE_SCHEMA_VERSION};
use crate::compile::contract::{
    decode_response, render_canonical_markdown, Assertion, CompileEvidence, CompileFailure,
    EvidenceSection, OutputWiki, SourceRef,
};
use crate::seed::split_sections;
use crate::traits::Compiler;
use crate::types::error::{Error, Result};
use crate::types::{CompileContext, CompiledPage, PageMetadata, QualityScore, RawEntity, WikiPage};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// ref id 起点（`r[1-9][0-9]{0,5}`，从 r1 递增）。
/// Ref-id start (`r[1-9][0-9]{0,5}`, increasing from r1).
const FIRST_REF_INDEX: usize = 1;

/// Mock 行为脚本（A11/A12 注入测试用）。
/// Mock behavior script (for A11/A12 injection tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockBehavior {
    /// 确定性正常输出（默认）。
    /// Deterministic normal output (default).
    Normal,
    /// 低质候选：断言 text 偏离抽取式约束（decode 合法但 validator 拒绝），
    /// 消耗 recompile 次数。
    /// Low-quality candidate: assertion text breaks the extractive constraint
    /// (decode-legal but validator-rejected), consuming recompiles.
    LowQuality,
    /// 可重试传输失败（§4：Retry-After 可选）。
    /// Retryable transport failure (§4: optional Retry-After).
    Retryable {
        code: String,
        retry_after_seconds: Option<u32>,
    },
    /// 永久失败（如 401）。
    /// Permanent failure (e.g. 401).
    Permanent { code: String },
}

/// 确定性 Mock 编译器。
/// The deterministic mock compiler.
pub struct MockCompiler {
    pub policy: CompilePolicy,
    pub behavior: MockBehavior,
    /// 模型调用计数（A2/A13 断言"模型 0 次"用）。
    /// Model-call counter (asserts "0 model calls" in A2/A13).
    calls: AtomicU64,
    /// 收到的知识快照输入（A19 断言敏感字段不进 Prompt/任务输入）。
    /// Received knowledge-snapshot inputs (A19 asserts sensitive fields never
    /// enter the prompt/task input).
    inputs: Mutex<Vec<RawEntity>>,
}

impl MockCompiler {
    /// 默认 Normal 行为。
    /// Default `Normal` behavior.
    pub fn new(policy: CompilePolicy) -> Self {
        Self {
            policy,
            behavior: MockBehavior::Normal,
            calls: AtomicU64::new(0),
            inputs: Mutex::new(Vec::new()),
        }
    }

    /// 指定行为脚本（A11/A12）。
    /// With an explicit behavior script (A11/A12).
    pub fn with_behavior(policy: CompilePolicy, behavior: MockBehavior) -> Self {
        Self {
            policy,
            behavior,
            calls: AtomicU64::new(0),
            inputs: Mutex::new(Vec::new()),
        }
    }

    /// 已发生的模型调用次数。
    /// Number of model calls so far.
    pub fn call_count(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// 收到的知识快照列表（测试断言用）。
    /// Received knowledge snapshots (for test assertions).
    pub fn received_inputs(&self) -> Vec<RawEntity> {
        self.inputs.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// 章节标题：领域必需标题的第一个（缺省 `概述`，§5.2.1 标题集合约束）。
    /// Section heading: the first required domain heading (default `概述`,
    /// per the §5.2.1 heading-set constraint).
    fn heading(&self) -> String {
        self.policy
            .required_headings
            .first()
            .cloned()
            .unwrap_or_else(|| "概述".to_string())
    }

    /// 收集知识快照的非空 string leaf（pointer, 原值）；BTreeMap 字节序稳定。
    /// Collects non-empty string leaves (pointer, original value) from the
    /// knowledge snapshot; BTreeMap byte order keeps it stable.
    fn string_leaves(raw: &RawEntity) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (name, value) in &raw.fields {
            match value {
                serde_json::Value::String(s) if !s.is_empty() => {
                    out.push((format!("/fields/{name}"), s.clone()));
                }
                serde_json::Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        if let serde_json::Value::String(s) = item {
                            if !s.is_empty() {
                                out.push((format!("/fields/{name}/{i}"), s.clone()));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 确定性编译：断言/refs → canonical Markdown → envelope JSON →
    /// [`decode_response`] → CompiledPage（占位字段）。
    /// Deterministic compilation: assertions/refs → canonical Markdown →
    /// envelope JSON → [`decode_response`] → CompiledPage (placeholder fields).
    fn build_page(&self, raw: RawEntity, low_quality: bool) -> Result<CompiledPage> {
        let leaves = Self::string_leaves(&raw);
        if leaves.is_empty() {
            // 空知识源：模拟模型 error envelope（decode 自洽映射为 InvalidOutput，
            // 不伪造 accepted 页）。
            // Empty knowledge: mimic a model error envelope (decode maps it to
            // InvalidOutput consistently; never fakes an accepted page).
            let envelope = serde_json::json!({
                "schema_version": ENVELOPE_SCHEMA_VERSION,
                "status": "error",
                "error": {"code": "MISSING_SOURCE_REFS", "missing_pointers": []},
            });
            // 错误 envelope 恒定解码为 InvalidOutput；若解出 Ok 分支属内部错误。
            // An error envelope always decodes to InvalidOutput; an Ok branch
            // would be an internal error.
            return match decode_response(&serde_json::to_string(&envelope)?) {
                Ok(_) => Err(Error::Internal(
                    "mock error envelope decoded as ok output".into(),
                )),
                Err(failure) => Err(Error::CompileFailure(failure)),
            };
        }
        let entity_key = raw.id.to_key();
        let heading = self.heading();
        let mut assertions = Vec::with_capacity(leaves.len());
        let mut refs = Vec::with_capacity(leaves.len());
        for (i, (pointer, value)) in leaves.iter().enumerate() {
            let id = format!("r{}", i + FIRST_REF_INDEX);
            // 正常：text=字段原值（抽取式一致）；低质：注入改写文本，触发
            // ASSERTION_UNSUPPORTED（decode 仍合法，validator 拒绝）。
            // Normal: text=the original value (extractively consistent); low
            // quality: an injected rewrite triggers ASSERTION_UNSUPPORTED
            // (still decode-legal, validator-rejected).
            let text = if low_quality {
                format!("低质量改写：{value}")
            } else {
                value.clone()
            };
            assertions.push(Assertion {
                text,
                ref_ids: vec![id.clone()],
            });
            refs.push(SourceRef {
                id,
                entity_id: entity_key.clone(),
                source_revision: raw.source_revision,
                pointer: pointer.clone(),
                value: serde_json::Value::String(value.clone()),
                quote: value.clone(),
            });
        }
        // 标题取第一个允许字段值（必然出现在其 quote 内，§5.2.3）。
        // Title takes the first allowed field value (guaranteed inside its
        // quote, §5.2.3).
        let title = leaves[0].1.clone();
        // 先渲染 canonical Markdown，再把它写进 envelope（保证 validator 的
        // 字节比对自洽）。
        // Render the canonical Markdown first, then embed it in the envelope
        // (keeps the validator's byte comparison self-consistent).
        let draft = CompileEvidence {
            schema_version: ENVELOPE_SCHEMA_VERSION.to_string(),
            wiki: OutputWiki {
                title: title.clone(),
                aliases: Vec::new(),
                tags: Vec::new(),
                markdown: String::new(),
            },
            sections: vec![EvidenceSection {
                heading,
                assertions,
                refs,
            }],
            usage: None,
        };
        let markdown = render_canonical_markdown(&draft);
        let envelope = serde_json::json!({
            "schema_version": ENVELOPE_SCHEMA_VERSION,
            "status": "ok",
            "wiki": {
                "title": title,
                "aliases": [],
                "tags": [],
                "markdown": markdown,
            },
            "sections": draft.sections,
        });
        let evidence =
            decode_response(&serde_json::to_string(&envelope)?).map_err(Error::CompileFailure)?;
        let content = render_canonical_markdown(&evidence);
        Ok(CompiledPage {
            wiki: WikiPage {
                page_id: raw.id.to_key(),
                entity_id: raw.id.clone(),
                title: evidence.wiki.title.clone(),
                content: content.clone(),
                // sections 由共享 splitter 生成（executor 仍会按 §5.2.5 重算）。
                // sections via the shared splitter (the executor recomputes per
                // §5.2.5 anyway).
                sections: split_sections(&content),
                // 占位 metadata：executor 以冻结 context + 时钟重算（§4）。
                // Placeholder metadata: the executor recomputes from the frozen
                // context + clock (§4).
                metadata: PageMetadata {
                    domain_pack_version: String::new(),
                    compiled_at: 0,
                    model_version: String::new(),
                    embedding_model: String::new(),
                },
                aliases: evidence.wiki.aliases.clone(),
                tags: evidence.wiki.tags.clone(),
            },
            // 占位零分：不能短路评分（§4）。
            // Placeholder zeros: must never short-circuit scoring (§4).
            quality: QualityScore {
                coverage: 0.0,
                citation: 0.0,
                schema_compliance: 0.0,
                density: 0.0,
                consistency: None,
            },
            // D8：本步 Mock 不提供 QUG 边。
            // D8: the Mock provides no QUG edges in this step.
            qug_edges: Vec::new(),
            // §4：compile 阶段不计算 content_hash，executor publish 前重算。
            // §4: content_hash is not computed at compile time; the executor
            // recomputes it before publish.
            content_hash: String::new(),
            evidence: Some(evidence),
        })
    }
}

#[async_trait]
impl Compiler for MockCompiler {
    async fn compile(&self, raw: RawEntity, _ctx: &CompileContext) -> Result<CompiledPage> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut inputs) = self.inputs.lock() {
            inputs.push(raw.clone());
        }
        match &self.behavior {
            MockBehavior::Retryable {
                code,
                retry_after_seconds,
            } => Err(Error::CompileFailure(CompileFailure::Retryable {
                code: code.clone(),
                retry_after_seconds: *retry_after_seconds,
            })),
            MockBehavior::Permanent { code } => {
                Err(Error::CompileFailure(CompileFailure::Permanent {
                    code: code.clone(),
                }))
            }
            MockBehavior::Normal => self.build_page(raw, false),
            MockBehavior::LowQuality => self.build_page(raw, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::contract::{
        DefaultSourceRefValidator, SourceRefValidator, ASSERTION_UNSUPPORTED,
    };
    use crate::types::EntityId;
    use std::collections::BTreeMap;

    fn raw() -> RawEntity {
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
            "test-v1", "S", "mock-v1", "none", 0.75, true, None, None,
        )
    }

    // Mock 与 decode 自洽：合法 envelope 解出证据，usage=None，canonical 正文，
    // validator 机械零 issue。
    // Mock/decode self-consistency: a legal envelope decodes with usage=None,
    // canonical body and zero mechanical validator issues.
    #[tokio::test]
    async fn normal_output_round_trips() {
        let mock = MockCompiler::new(CompilePolicy::default());
        let page = mock.compile(raw(), &ctx()).await.unwrap();
        let evidence = page.evidence.as_ref().unwrap();
        assert_eq!(evidence.schema_version, "source-ref-v1");
        assert!(evidence.usage.is_none());
        assert_eq!(page.content_hash, "");
        // 标题 = 第一个允许字段值（BTreeMap 字节序：description 在 name 前）。
        // Title = the first allowed field value (BTreeMap byte order: description
        // precedes name).
        assert_eq!(page.wiki.title, "珍珠奶茶");
        assert_eq!(evidence.wiki.markdown, page.wiki.content);
        assert_eq!(page.wiki.sections.len(), 1);
        assert_eq!(page.wiki.sections[0].heading, "概述");
        assert_eq!(mock.call_count(), 1);
        let report = DefaultSourceRefValidator.validate(&raw(), evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    }

    // reflist 元素逐项成为断言与 ref（pointer 带索引）。
    // Reflist elements become per-item assertions/refs (indexed pointers).
    #[tokio::test]
    async fn reflist_elements_are_indexed() {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!("啵啵"));
        fields.insert(
            "ingredients".to_string(),
            serde_json::json!(["珍珠", "椰果"]),
        );
        let list_raw = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 2,
        };
        let mock = MockCompiler::new(CompilePolicy::default());
        let page = mock.compile(list_raw.clone(), &ctx()).await.unwrap();
        let evidence = page.evidence.unwrap();
        assert_eq!(evidence.sections[0].refs.len(), 3);
        // BTreeMap 字节序：ingredients 在 name 前（指针带索引）。
        // BTreeMap byte order: ingredients precedes name (indexed pointers).
        assert_eq!(
            evidence.sections[0].refs[0].pointer,
            "/fields/ingredients/0"
        );
        assert_eq!(
            evidence.sections[0].refs[1].pointer,
            "/fields/ingredients/1"
        );
        assert_eq!(evidence.sections[0].refs[2].pointer, "/fields/name");
        let report = DefaultSourceRefValidator.validate(&list_raw, &evidence, true);
        assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    }

    // 空知识源 → error envelope → InvalidOutput（不伪造 accepted 页）。
    // Empty knowledge → error envelope → InvalidOutput (never fakes an accepted page).
    #[tokio::test]
    async fn empty_knowledge_maps_to_invalid_output() {
        let mut fields = BTreeMap::new();
        fields.insert("name".to_string(), serde_json::json!(""));
        let empty_raw = RawEntity {
            id: EntityId::new("milk-tea", "drink", "boba").unwrap(),
            fields,
            source_revision: 1,
        };
        let mock = MockCompiler::new(CompilePolicy::default());
        let err = mock.compile(empty_raw, &ctx()).await.unwrap_err();
        match err {
            Error::CompileFailure(CompileFailure::InvalidOutput { code, .. }) => {
                assert_eq!(code, "ERROR_ENVELOPE_MISSING_SOURCE_REFS");
            }
            other => panic!("expected InvalidOutput, got {other:?}"),
        }
    }

    // 低质候选：decode 合法但断言偏离抽取式约束 → ASSERTION_UNSUPPORTED。
    // Low quality: decode-legal but the assertion breaks the extractive
    // constraint → ASSERTION_UNSUPPORTED.
    #[tokio::test]
    async fn low_quality_candidate_fails_extraction() {
        let mock = MockCompiler::with_behavior(CompilePolicy::default(), MockBehavior::LowQuality);
        let source = raw();
        let page = mock.compile(source.clone(), &ctx()).await.unwrap();
        let evidence = page.evidence.unwrap();
        let report = DefaultSourceRefValidator.validate(&source, &evidence, true);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.code == ASSERTION_UNSUPPORTED),
            "issues: {:?}",
            report.issues
        );
    }

    // 脚本式传输失败映射为类型化 CompileFailure。
    // Scripted transport failures map to typed CompileFailure.
    #[tokio::test]
    async fn scripted_failures_are_typed() {
        let retry = MockCompiler::with_behavior(
            CompilePolicy::default(),
            MockBehavior::Retryable {
                code: "RATE_LIMITED".into(),
                retry_after_seconds: Some(7),
            },
        );
        match retry.compile(raw(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::Retryable {
                code,
                retry_after_seconds,
            }) => {
                assert_eq!(code, "RATE_LIMITED");
                assert_eq!(retry_after_seconds, Some(7));
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
        let perm = MockCompiler::with_behavior(
            CompilePolicy::default(),
            MockBehavior::Permanent {
                code: "UNAUTHORIZED".into(),
            },
        );
        match perm.compile(raw(), &ctx()).await.unwrap_err() {
            Error::CompileFailure(CompileFailure::Permanent { code }) => {
                assert_eq!(code, "UNAUTHORIZED");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }
}
