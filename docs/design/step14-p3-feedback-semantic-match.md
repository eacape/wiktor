# Step 14 (P3-C) 设计规范：反馈语义匹配抽象（查询键归一化可插拔）

> 版本：v1.0（2026-09-26）
> 权威依据：`docs/MASTER-PLAN.md` §5.3 编译-检索双向反馈、§8 核心抽象；Step 6 spec `step6-feedback-loop.md` §6（analyzer 契约）
> 实现对象：`FeedbackKeyMatcher` 抽象 + 默认实现（复用现有 normalize）+ analyzer 注入（主模型实现，本文档登记规范）

## 1. 背景：原规划表述 vs 现状落差

P3 原规划子项「反馈语义匹配抽象」，原始表述为：*「反馈语义匹配当前是归一化标题全等，要抽象成可插拔」*。

**核实现状后，必须澄清落差**：

1. **反馈事件存储层没有「标题全等匹配」**。`FeedbackEventInput`（`feedback_store.rs`）直接带 `page_id: Option<String>`，click/adopt 必填 ≤512（D3）；插入事务 `insert_feedback_idempotent_on_conn` 只做 log_id 存在 + domain 匹配 + `page_exists`（page_id 是该 domain 的 accepted 页）校验。**页面归因由上游在构造事件时完成，不是存储层做的事。**
2. **反馈分析器已经是一个可插拔 trait**。`FeedbackAnalyzer { async fn analyze(FeedbackWindow) -> Result<FeedbackReport> }`，标准实现 `StandardFeedbackAnalyzer`（只携带 `min_events`）。
3. **真正的耦合点**：analyzer 用 `use wiktor_core::query_engine::qug::normalize;`（`analyzer.rs:62`）**硬编码直引**归一化函数，把 `(domain, normalized query_text)` 作为**信号一/二（零召回/改写失败）的死绑去重键**（`normalized_log_query`，`analyzer.rs:401`）。这里正是「归一化=匹配/去重键」被钉死的所在。信号三（低质量）按 `page_id` 聚合；`rate` 事件可无 page_id（DDL 允许）无法归因到页（`analyzer.rs:272-282`）。

**结论**：P3-C 的「可插拔」落点不在存储层，而在 **analyzer 的查询键归一化**——把 `normalize` 这处硬编码直引抽成可插拔 trait，默认实现保持现有行为（离线、确定、复用 Step 3 §3.1 契约），未来可替换为语义/向量匹配而不改 analyzer 规则。

## 2. 决策点：做什么、不做什么

### 方向 A（推荐，本子项交付）——抽象查询键归一化
把「QueryLogSnapshot → 去重键」的归一化抽成 trait，默认实现 = 现有 `normalize`。理由：
- **贴合现状**：这是唯一真实的硬编码耦合，抽象它零行为变化、零风险。
- **低成本**：trait 薄壳 + 默认 impl 包一层 `normalize`，不动 analyzer 三段信号规则。
- **真可插拔**：未来要换成 LLM/向量语义键，只新增一个 impl + 装配处换注入，不改规则逻辑。
- **稳守离线基线**：默认实现保持确定性、无网络、纯函数——符合用户「少造轮子」与项目「默认路径零 LLM」铁律。

### 方向 B（明确不做，登记为后续）——补语义匹配
为 `rate` 无 page_id 或未来自由文本反馈引入「从反馈内容/query 语义匹配到页面」的抽象。**本子项不做**的理由：
- 现有事件 page_id 由上游（server/CLI）构造时已带，SQLite 校验足够；`rate` 无 page_id 是少数，降级路径（无法归因→忽略该事件于页面判据）已存在且合理。
- 语义/LLM 匹配成本高、对生产反馈闭环的**实际增益未验证**，贸然上会把 `rate` 误归因到错页，比不归因更糟。
- 方向 A 的 trait 已为它预留扩展点：新增 impl 即可，无需重排本 spec。

## 3. 抽象形状

```rust
/// 反馈去重键匹配器：把日志行归一到用于信号一/二去重聚合的稳定键。
/// 默认实现复用 `query_engine::qug::normalize`（Step 3 §3.1 契约）。
#[async_trait::async_trait]
pub trait FeedbackKeyMatcher: Send + Sync {
    /// 归一化一条查询日志为稳定去重键；失败 → `analyze` 该行 fail-closed。
    fn normalize(&self, query_text: &str) -> Result<String>;
}
```

- **默认实现** `StandardKeyMatcher`：包 `qug::normalize`，行为与现状逐字节一致（trim + 折叠空白 + 小写 + 非空 + ≤MAX_PHRASE_SCALARS）。
- **注入点**：`StandardFeedbackAnalyzer` 增持有一个 `Box<dyn FeedbackKeyMatcher>`（默认 `StandardKeyMatcher`），`analyze_window_with` 改用 `self.matcher.normalize(...)`，替换 `normalized_log_query` 内的直引 `normalize`。
- **错误语义**：归一化失败维持现状——该行 fail-closed（analyzer 返回 Err），不静默丢弃。

## 4. 与既有面的协作

- **`feedback_store`·`page_exists`**：不动。页面归因仍在存储层按 page_id 校验 accepted 页；本抽象只管 analyzer 的信号一/二去重键。
- **信号一/二**：用注入的 matcher 生成 `(domain, normalized query_text)` 键。
- **信号三（低质量）**：仍按 `page_id` 聚合，不涉及本抽象（rate 无 page_id 的降级路径保持）。

## 5. 边界与不变量

- **幂等**：matcher 是纯函数，同一 query_text 恒出同键（默认 impl 保证）；语义替代须维持该不变量。
- **确定性**：报告输出顺序仍按归一化键/page_id 字节序，matcher 不改变排序契约。
- **离线基线**：默认 impl 零网络、零模型、纯内存。

## 6. 涉及文件

- `crates/wiktor-feedback/src/analyzer.rs`：新增 `FeedbackKeyMatcher` trait + `StandardKeyMatcher`；`StandardFeedbackAnalyzer` 增持 matcher；`normalized_log_query` / 信号一/二改用注入。
- `crates/wiktor-feedback/src/lib.rs`：re-export `FeedbackKeyMatcher`、`StandardKeyMatcher`（可选）。
- 测试：既有 analyzer 单测须保持绿（默认 impl 行为逐字节一致）；新增 1 个替换性测试（自定制 matcher 注入后信号一/二按新键聚合，证明可插拔）。

## 7. 不做清单

- 不做方向 B（语义/LLM 归因），理由见 §2，已登记为后续扩展点。
- 不改存储层 page_id 校验、不改信号三规则、不改 `insert_feedback_idempotent`。