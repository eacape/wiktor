# Step 14 (P3-A) 设计规范：一致性仲裁 LLM 化（可插拔 + env 驱动，CLI/worker 同源）

> 版本：v1.0（2026-09-26）
> 权威依据：`docs/MASTER-PLAN.md` §8 核心抽象（插件点）、§5.x 编译一致性门禁；Step 8 spec §6.1/§6.2 与决策 D1–D4
> 实现对象：`LlmConsistencyArbiter` + `LlmClient` 抽象 + `wiktor-server`/`wiktor-cli` 同源 env 装配（主模型已实现，本文档登记规范）

## 1. 背景与目标

Step 8 交付的一致性仲裁只有一个实现：确定性 `SourceRefConsistencyArbiter`（X5：不做自由语义推断，只按 `(entity_id, pointer)` 分组 + canonical_json 精确比较）。它由 `compile.rs` 与 `worker.rs` 各自硬编码装配。

本子项（P3-A）把一致性仲裁改造成**可插拔 + env 驱动**：

1. 核心只依赖 `ConsistencyArbiter` 抽象（本来就是）；新增 `LlmConsistencyArbiter` 作为第二个实现，走真实 LLM 裁决；
2. `wiktor-server` 与 `wiktor-cli` 的装配**同源**——同一套 env 契约，缺省回退确定性基线，离线照跑；
3. LLM 仲裁被约束在既有契约内：**不能绕过证据过滤 / top-k / 预算**，一次仲裁恰好一次 LLM 请求。

> 说明：P3 原清单里的「过滤下推」已在 Step 3 交付（`docs/MASTER-PLAN.md` §17 条目3：「索引 → QUG/fallback → 过滤下推 → CLI 展示 ✅」，2026-09-21），非本子项待办。「反馈语义匹配抽象」无落盘 spec，留待设计（见 §7）。

## 2. 交付物

| 编号 | 交付物 | 状态 |
|---|---|---|
| P3-A1 | `LlmClient` 抽象复用（`compile::llm::LlmClient`，async `complete`） | 已完成（Step0 已有） |
| P3-A2 | `LlmConsistencyArbiter`（`consistency.rs`，feature `llm-openai` 门控） | 已完成 |
| P3-A3 | worker/CLI 同源 env 装配（`build_server_consistency_arbiter` / `build_cli_consistency_arbiter`） | 已完成 |
| P3-A4 | 双语规范登记（本文档） | 本文档 |

## 3. 契约（不可绕过项）

- 核心只依赖 `ConsistencyArbiter`；**未来任何仲裁器只能实现同一 trait**，不能绕过 `compare_pointers` 显式声明、top-k 有界召回、证据预算。
- **证据在核心侧过滤（同一 D3 规则）**：`LlmConsistencyArbiter::arbitrate` 先调 `comparable_refs` 只保留可比较 source-ref，LLM **永远只见可比证据**，不与自由语义输入混流。
- **一次仲裁 = 恰好一次 LLM 请求**：装配单个 `LlmRequest`（`CONSISTENCY_SYSTEM_PROMPT` + candidate/related/compare_pointers），`max_output_tokens` 由装配方给定（worker/CLI 传 512）。
- **诊断只存 BLAKE3 摘要，不进原文**（A7 延续）：LLM 返回的判定由核心解析为 findings 后只落 `candidate_value_hash`/`evidence_value_hash`。
- **缺 key 是配置错误，不降级 mock**（D7 延续，与 compiler 装配一致）：但**一致性仲裁是新增面，显式 opt-in**——设 URL 不自动启用，须 `WIKTOR_CONSISTENCY_LLM=1`。

## 4. env 装配契约（worker/CLI 同源）

| env | 语义 |
|---|---|
| `WIKTOR_CONSISTENCY_LLM` | opt-in：`1`/`true`（忽略大小写）才可能启用 LLM 仲裁 |
| `WIKTOR_LLM_BASE_URL` | 非空 → 用 OpenAI 兼容 `OpenAiLlmClient`；空/未设 → 回退确定性 |
| `WIKTOR_LLM_MODEL` | 模型，缺省 `qwen3.8-max` |
| `WIKTOR_OPENAI_API_KEY`（`API_KEY_ENV`） | key；缺 key 则 `OpenAiLlmClient::new` 返回 Err → 回退确定性并 eprintln 提示 |

**决策链**：`opt_in ∧ base_url 非空 ∧ (llm-openai feature)` → 尝试构造 `LlmConsistencyArbiter`；构造失败或任何条件不满足 → `SourceRefConsistencyArbiter`（离线基线，永远可用）。`cfg(feature="llm-openai")` 关闭时静默回退确定性（server 端 eprintln 提示）。

## 5. 装配同源实现

- `crates/wiktor-server/src/worker.rs` `CompileWorker::from_domain_pack`：`consistency.enabled` 时把 `build_server_consistency_arbiter()` + `SqliteFtsCandidateProvider`（kernel.clone()）装配进 executor；关闭走 None 路径（不仲裁）。装配用裸 `PipelineExecutor` 链 `.with_consistency_arbiter(..).with_candidate_provider(..)` 后再 `Arc::new`——与 CLI 同源，避免 `Arc` 内 move。
- `crates/wiktor-cli/src/compile.rs` `build_cli_consistency_arbiter()`：同一 env 决策链。

## 6. 涉及文件与测试

- `crates/wiktor-core/src/compile/consistency.rs`：`ComparableRef`、`LlmConsistencyArbiter`（new：client+model+max_output_tokens）、`CONSISTENCY_SYSTEM_PROMPT`、同 trait 下的确定性实现不变。
- `crates/wiktor-core/src/compile/llm.rs`：`LlmClient` trait（async `complete(LlmRequest)->Result<LlmResponse,CompileFailure>`）、`OpenAiLlmClient`、`API_KEY_ENV`。
- `crates/wiktor-cli/src/compile.rs`、`crates/wiktor-server/src/worker.rs`：同源装配。
- 测试：`compile::consistency` 模块 18 测试全绿——重点是 `fake_arbiter_is_substitutable`（证明 trait 可替换 = 可插拔契约）、`empty_pointer_table_is_deterministic_none`、`cross_domain_keys_never_compare`；Mock 用 `impl LlmClient for MockLlm`。

## 7. 遗留：P3-C 反馈语义匹配抽象（待设计）

现状是「归一化标题全等」把反馈事件匹配到页面。抽象成可插拔 matcher 需先定 spec（trait 形状、注入点、与 `insert_feedback_idempotent` 的协作、离线基线）。本文档不擅自实现——按「先 spec 后实现」排期，待用户确认范围（是否纳入 P3、是否拆分到后续）。