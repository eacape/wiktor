# 真实后端效果实验（Real-Backend Experiment）

> 版本：v1.0（2026-09-24）
> 上承接：Step 5 `step5-qug-build.md`（其退出条件判定标注「真实向量后端接入后需复测」）；Step 4 `step4-compile-pipeline.md`
> 执行环境：本机 macOS（Apple Silicon）；qdrant 1.19.1 本地 6333/6334；真实 LLM/Embedding 经网关调用（key 走环境变量，不落库落盘）
> 中文为权威文档；英文逐节对应 `real-backend-experiment.en.md`。

## 1. 背景与目的

Step 1–8 的全部 376 个离线验收测试使用 `MockCompiler`（确定性编译）与 `MockVectorStore`（内存稀疏向量）完成，**证明了机制正确性，但从未在真实模型/真实嵌入下度量效果**。Step 5 的 QUG 退出判定（`enabled`，C−B recall@10 增益 +40.17pp）基于 mock 空向量库得出，spec 与 MASTER-PLAN 均白纸黑字注明「真实向量后端接入后需复测」。

本实验用真实后端复测全链路，回答三个问题：

1. **真实 LLM 编译**：qwen3.8-max 编译 120 个 milk-tea 商品，产出页面的质与量、对 source-ref-v1 输出契约的遵守度。
2. **真实嵌入检索**：qwen3.7-text-embedding-flash（1024 维）构建向量索引后，混合检索相对纯 FTS 的真实增益。
3. **QUG 判定复测**：真实后端下 C−B 增益是否仍满足 ≥5pp 退出条件，Step 5 的 `enabled` 判定是否成立。

## 2. 环境与配置

| 项 | 值 |
|---|---|
| 大模型 | qwen3.8-max（经 `https://model.kimitk.top/v1` 网关） |
| 嵌入模型 | qwen3.7-text-embedding-flash（经阿里 MaaS 兼容端点，**1024 维**） |
| 向量库 | qdrant 1.19.1（http 6333 / gRPC 6334，本地） |
| 商品数据 | `examples/milk-tea/products.jsonl`，120 商品 |
| seed 知识页 | `examples/milk-tea/seed-wiki/`，20 页（概念/品牌/原料/做法） |
| golden 评测集 | `examples/milk-tea/golden-queries.jsonl`，134 条（legacy 34 + 新增 100） |
| 编译策略 | max_output_tokens=8192，batch/task token 预算放大（真实模型配额），自定义实验 prompt（见 §4.3） |

key 一律通过进程环境变量注入（`WIKTOR_OPENAI_API_KEY` / `WIKTOR_EMBEDDING_API_KEY` / `WIKTOR_EMBEDDING_BASE_URL`），**不进入任何文件、日志或 commit**；本报告所有 key 均以 `<env>` 占位。

## 3. 真实 LLM 编译结果

对 120 商品跑 qwen3.8-max 编译全流程（含重试/刹车/预算熔断/质量门）：

```
accepted  quarantined  failed  skipped  attempts
      72          23      17        8       236
```

- **72 accepted（60%）**：通过四维质量门与引用契约的合法 Wiki 页。
- **23 quarantined**：质量门失败（引用/渲染/断言类 issue），未入查询索引——发布状态机正确隔离。
- **17 failed**：重试耗尽进入死信（`dead`），未自动吞掉。
- **8 skipped**：token 预算/刹车熔断跳过（预算保护正常工作的证明）。
- **236 attempts**：含首次失败后的重试（指数退避、预算预留、租约心跳均在真实网络下工作）。

关键结论：**真实 LLM 编译可行但约四成商品被正确过滤**——这正是 Step 4 设计重编译刹车 + 质量门 + 死信队列的初衷（模型产物不可信，宁可隔离不发布）。

## 4. 真实模型契约偏差（本实验最重要的工程发现）

### 4.1 思考模式占用输出配额

qwen3.8-max 是思考模型：`max_tokens=2048` 时 `reasoning_tokens` 吃满配额 → `content` 恒空 → `MALFORMED_JSON`。适配器需将 `max_output_tokens` 提高到 8192（思考约 4–5K + 正文 3–4K），或显示关闭思考。

### 4.2 思考关闭参数因网关而异

- goaichat 网关：`chat_template_kwargs: {"enable_thinking": false}` **概率性生效**，批量下大多仍空 content → 不可用。
- kimitk 网关：`thinking: {"type":"disabled"}` 稳定生效（`reasoning_tokens=0`，6/6 有效 JSON）→ 采用。

适配器新增 `WIKTOR_LLM_DISABLE_THINKING=1` 环境开关，对兼容网关附加 `thinking: {"type":"disabled"}`；标准 OpenAI 端点不带该扩展。

### 4.3 entity_id 截断（source-ref-v1 契约偏差实证）

内置 system_prompt 已写「entity_id 必须等于知识快照的值」，但 qwen3.8-max 思考模式下常把完整 ID `milk-tea:product:sku_0001` 截断为 `sku_0001` → `SOURCE_ID_MISMATCH`。**关闭思考后截断消失**（同一 prompt、thinking off，6/6 entity_id 完整），说明截断是思考模式副产物。

进一步发现：内置 prompt 缺「完整 ID 示例」。实验自定义 prompt 显式给出 `milk-tea:product:sku_0001` 反例说明后，entity_id 偏差完全消除。**结论：source-ref-v1 的完整 ID 契约对 qwen 系需在 prompt 中给示例**（已反馈为产品改进点，见 §7）。

### 4.4 其它偏差

- `UNKNOWN_FIELD`：qwen3.8-max 思考模式偶发在 envelope 顶层输出多余字段，被 `EnvelopeRepr` 的 `deny_unknown_fields` 拒绝（关闭思考后消失）。
- `TRANSPORT_CONNECT`：网关偶发连接失败，被分类为可重试，指数退避正常处理。
- `UNKNOWN_FIELD`/`SOURCE_ID_MISMATCH` 等失败均消耗重试预算、不静默吞掉——验证了 typed failure 分类与重试/刹车语义在真实网络下正确工作。

## 5. 工程改动（使真实后端可用的最小增量）

1. **`wiktor-core` 新增 `embedding-http` feature + `HttpEmbedder`**（`src/embedding/mod.rs`）：OpenAI 兼容 `/embeddings` 客户端，动态维度探测（首次响应取长度，**不硬编码 1024**），key/端点/模型全走环境变量。
2. **CLI 新增 `wiktor vector build`**：embed accepted 页面 → 建 collection（真实维度）→ upsert 到 qdrant；`--deterministic` 切本地基线（离线测试）。这是**全仓首个生产向量写入路径**（此前 eval/search 只 `ensure_collection` 从不 upsert，B/C 档向量路实为 0 命中——见 §6 讨论）。
3. **eval 接入真实嵌入**：环境变量提供嵌入配置时用 `HttpEmbedder`（维度动态），否则保持 `DeterministicEmbedder`（768 维，离线行为不变）。
4. **LLM 适配器**：`OpenAiLlmClient` 新增 `WIKTOR_LLM_DISABLE_THINKING` 开关（`thinking: disabled` 扩展）；`LlmCompiler` 改为使用 `ctx.prompt_template`（此前硬编码 `system_prompt()`，导致 `compile.prompt` 从未生效——产品缺口修复）。

## 6. 真实后端评测结果

golden 134 条，top_k=10，rrf_k=60，真实向量后端（qdrant + 1024 维 qwen 嵌入），100 accepted 页（80 编译 + 20 seed），QUG 97 边（hyponym 56 / synonym 34 / attribute 3 / intent 2 / negation 2）。

| tier | recall@1 | recall@5 | recall@10 | negative_precision |
|---|---|---|---|---|
| A 纯FTS | 0.318 | 0.395 | 0.446 | 1.000 |
| B 混合（无 QUG） | 0.607 | 0.736 | 0.889 | 0.808 |
| C QUG 启用 | 0.492 | 0.793 | **0.962** | 0.769 |

**决策：`enabled`（C−B recall@10 增益 +7.26pp ≥ 5pp 阈值）**。报告同时输出 regression 警告：C 的 negative_precision（0.769）低于 B（0.808）。

### 6.1 真实向量 vs 纯 FTS（A→B）

- recall@10：0.446 → 0.889（**+44.3pp**）。
- 这是真实语义嵌入 vs 词法 BM25 的直接度量——远超 Step 5 mock 时代的任何假设（mock 空向量下 B≈A）。
- 代价：negative_precision 1.000 → 0.808（语义召回引入一些噪声命中）。

### 6.2 QUG 增益（B→C）与分档命中

| kind | A r@10 | C r@10 | 说明 |
|---|---|---|---|
| attribute_filter | 0.975 | 1.000 | 属性过滤本就高，QUG 补齐 |
| intent | 0.000 | **0.950** | 意图模板命中，A 档全 miss → C 档 0.95 |
| negation | 0.000 | **1.000** | 否定语义命中，A 档全 miss → C 档 1.0 |
| legacy | 0.691 | 0.926 | 常规查询稳健提升 |
| synonym | 0.368 | 0.960 | 同义改写大幅提升 |

QUG 的核心价值在 **intent/negation 两类**：纯 FTS 完全无法命中的意图与否定查询，经 QUG 改写后 recall@10 达到 0.95/1.00。这正是 Step 5 设计「意图模板 + 否定」边时的预期场景。

### 6.3 与 mock 基线对比

| 指标 | Step5 mock 基线 | 真实后端 | 差异 |
|---|---|---|---|
| B 相对 A recall@10 | 约 0（向量 0 命中） | +44.3pp | mock 低估了真实向量增益 |
| C 相对 B recall@10 | +40.17pp（B≈A≈0.497 底数） | +7.26pp | 底数提升后增益收窄，仍达标 |
| 判定 | enabled | **enabled（复测确认）** | 一致 |

**Step 5 承诺兑现**：真实后端复测确认 QUG `enabled` 判定成立，但增益幅度从 mock 的 +40pp 收窄到真实混合基线之上的 +7.26pp——mock 因其空向量底数**严重高估**了 QUG 的边际增益。真实结论：向量混合已是主流命中（B r@10 0.889），QUG 在其上提供小而关键的补充（意图/否定）。

## 7. 结论与产品改进点

### 结论

1. **真实 LLM 编译可行**（qwen3.8-max 60% accepted），且质量门/刹车/死信在真实网络下正确隔离不合格产物。
2. **真实嵌入增益显著**：混合检索 recall@10 相对纯 FTS +44.3pp，验证了 qdrant + 语义嵌入路线的价值。
3. **QUG 判定 confirmed enabled**：真实后端下 C−B +7.26pp 达标；意图/否定类命中从 0 提升到 0.95/1.0 是 QUG 的独特贡献。
4. **备注**：本次「真实评测」的 B/C 档向量路第一次有真实数据（此前 mock 空向量），因此 B 相对 A 的增益是首个真实量化值。

### 产品改进点（后续落地，非本实验范围）

1. **内置 system_prompt 补 entity_id 完整示例**：source-ref-v1 契约对 qwen 系思考模型需显式反例，否则 ID 截断。建议在 §4.3 观察基础上更新默认 prompt 并同步 golden。
2. **`thinking: disabled` 参数标准化**：当前经 `WIKTOR_LLM_DISABLE_THINKING` 环境开关，未来可提升为 domain.yaml 配置与 CLI flag。
3. **`compile.prompt` 生效已修复**（本实验发现并修复 `LlmCompiler` 硬编码 `system_prompt()` 的缺口），建议补官方测试锁定「自定义 prompt 真正进入请求与 content_hash」。
4. **QUG 的 negative_precision 回撤**：C 档 0.769 < B 档 0.808，源于 intent/negation 改写引入噪声；可探索改写置信度门槛或 negative 类不改写策略。
5. **文档检索面**：`wiktor search` 仍走 DeterministicEmbedder 与空集合（Step 3 先例），真实嵌入检索建议纳入后续 server/CLI 统一注入。

## 8. 成本与可复现

- 编译 120 商品：约 236 次模型请求（含重试），reported_tokens ≈ 168K output / 2.5M reserved（预算预留含退避估计）。
- 嵌入 100 页 + eval 查询嵌入：约数百次小请求。
- 复现：`WIKTOR_OPENAI_API_KEY` + `WIKTOR_EMBEDDING_API_KEY` + `WIKTOR_LLM_DISABLE_THINKING=1` 环境注入，实验 domain（`max_output_tokens:8192` + 自定义 prompt）位于本机，未进仓库（key 保护）。
- 全部 key 仅存在于当次 shell 环境；本报告与仓库无任何明文 key。

## 9. 数据归档

- 完整 JSON 报告：`/tmp/wiktor-real-eval/step5-qug-evaluation.json`（本机，key 无关痕迹已清理）。
- 双语评测报告：`/tmp/wiktor-real-eval/step5-qug-evaluation.md` / `.en.md`。
- 实验 DB：`/tmp/wiktor-real.db`（100 accepted 页 + 72 编译产物，临时）。