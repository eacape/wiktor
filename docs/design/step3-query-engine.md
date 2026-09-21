# Step 3 Spec：最小查询闭环（QUG + QueryEngine + 混合检索）

> 版本：v1.0（2026-09-21）  
> 上承接：Step 2 `step2-seed-wiki-query-loop.md`  
> 实现对象：`wiktor-builder`  
> 本文是中文版权威设计；英文版 `step3-query-engine.en.md` 按本文件逐节对应。

## 1. 目标与范围

Step 3 把 Step 2 的单平面 FTS 查询升级为可测量的最小查询闭环：`Query → QUG rewrite/fallback → 事实过滤预筛 → FTS5 + Vector → RRF → QueryResult → query_logs`。查询路径继续零 LLM、零远程模型调用；QUG 在编译/seed 阶段建好，查询阶段只读。

本步包含：

- 用 petgraph 实现五类 QUG 边，来源为 seed-wiki frontmatter、`intents.yaml` 和领域包规则。
- 扩展 `DomainConfig` 的 `qug` 段，并新增奶茶领域包意图模板。
- 在 `crates/wiktor-core/src/query_engine/` 增加 `QueryEngine` 编排层。
- 在召回前合并用户过滤和 QUG 过滤，调用 `EntityStore::filter` 形成候选实体域。
- FTS5 BM25 与 `VectorStore` 两路召回，使用 RRF（常数 60）融合；MockVectorStore 可在无 qdrant 环境完成全链路。
- CLI search 改走 QueryEngine，并展示改写状态与应用过滤。
- golden 评测同时报告纯 FTS、混合检索、QUG 混合检索；QUG 增益低于 5% 时默认关闭且不使构建失败。

范围外：LLM 边抽取、cross-encoder rerank、查询缓存、远程 API、TUI、反馈任务自动生成。它们保持既有总纲边界。

## 2. 关键设计决策（拍板记录）

| # | 决策 | 具体规则与理由 |
|---|---|---|
| D1 | QUG 使用 `petgraph::graph::DiGraph` | 图是编译期产物，查询只读；NodeIndex 适合边遍历，节点 payload 保存归一化短语。HashMap 只作为 phrase→NodeIndex 索引，不承担图语义。 |
| D2 | QUG 节点为归一化词/短语，边保持原始 `QugEdge` | `trim`、Unicode 小写、连续空白折叠；中文不做大小写以外的分词。边 payload 仍保留结构化 Filter/Query，避免查询时重新解析 YAML。 |
| D3 | `rewrite` 返回 `Result<Option<_>>` | `Some` 表示至少匹配一条可执行边；`None` 表示没有边或输入非法但可安全 fallback。图损坏、过滤类型不合法等内部错误返回 `Err`，不伪装成业务 fallback。 |
| D4 | 过滤先下推，再分别召回 | 用户过滤与 QUG 过滤按 AND 合并；事实平面先得到候选实体集合，候选域同时传入 FTS SQL 和 `VectorStore::search`，避免 top-k 后过滤造成漏召回。 |
| D5 | FTS 与向量各取过采样候选，再 RRF | `candidate_k = max(top_k * 5, 50)`，上限 500；最终只返回 `top_k`。RRF 常数 `k=60`，同一实体多页面取最高贡献。 |
| D6 | 默认提供 Mock 向量闭环，qdrant 为可选后端 | 未配置嵌入器时，QueryEngine 接收调用方生成的 query vector；CLI 默认使用确定性的 token hash 向量，仅用于本地闭环，不宣称语义质量。qdrant 仍遵守 v3.2 默认部署方向。 |
| D7 | QUG 功能以领域配置开关控制 | `qug.enabled=false` 或 golden 增益 `<5%` 时，engine 不调用 rewrite，诊断状态为 `disabled`；这满足 MASTER-PLAN 退出条件，不阻塞混合检索交付。 |
| D8 | 日志写入由 QueryEngine 统一负责 | Step2 kernel.search 的旧日志路径保留给兼容调用；QueryEngine 调用新的不写日志的双路检索函数，再写完整 `rewritten_json`、failure、hits。避免一条查询写两条日志。 |

## 3. QUG 图数据结构与构建

### 3.1 结构

```rust
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct QugNode {
    pub phrase: String,              // normalized lookup key
}

pub struct QugGraph {
    pub graph: DiGraph<QugNode, QugEdge>,
    pub by_phrase: HashMap<String, NodeIndex>,
    pub max_depth: usize,
}

impl QugGraph {
    pub fn from_edges(edges: impl IntoIterator<Item = QugEdge>, max_depth: usize)
        -> Result<Self>;
    pub fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>>;
    pub fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath>;
}
```

`from_edges` 对同一归一化 `from/phrase/child` 复用节点；边去重键为 `(source, target, discriminant, serialized_payload)`。`max_depth` 默认 2，硬上限 4；超过上限返回配置错误。图构建完成后包进 `Arc<QugGraph>`，不在查询期间加锁写入。

归一化函数契约：去首尾空白、连续空白折叠、ASCII 字母转小写；保留中文、数字、标点。空短语和超过 128 个 Unicode scalar 的短语拒绝构图。`to` 列表中的每个词各建一条有向边；同义关系若需双向，构建器显式生成反向边。

### 3.2 边来源

1. **seed-wiki aliases → Synonym**：每页 frontmatter 的 `aliases` 中每个 alias 指向该页的 `title` 和 `entity_id` 可检索文本。至少生成 `Synonym { from: alias, to: [title] }`；同一页多个 alias 互相不自动全连接，避免边数爆炸。
2. **seed-wiki tags → Hyponym**：标签作为 parent，页面 `title` 和 `entity_type` 作为 child，生成 `Hyponym { child: title, parent: tag }`。标签之间不推断层级。
3. **intents.yaml → IntentTemplate / Negation / AttributePropagation**：按 §4 schema 直接转换，解析阶段校验 field 在 `query.filters` 白名单内。
4. **规则匹配边**：模板条目的 `phrases` 是显式规则；不允许在查询时执行正则或调用 LLM。Step3 只支持 exact phrase 和多词 substring 扫描。

构建输入接口：

```rust
pub struct QugBuildInput<'a> {
    pub pages: &'a [CompiledPage],
    pub config: &'a DomainConfig,
    pub intents: &'a IntentConfig,
}

pub struct BuiltQug {
    pub graph: Arc<QugGraph>,
    pub source_hash: String, // BLAKE3 of normalized edges + domain version
}

pub fn build_qug(input: QugBuildInput<'_>) -> Result<BuiltQug>;
```

`source_hash` 是 QUG 派生缓存/重建依据，至少包含 domain version、`qug` 配置文件 bytes、每页 page_id/content_hash；输入改变必须重新建图。Step3 不持久化图到 SQLite。

### 3.3 rewrite 语义与优先级

扫描 query.text 的所有 Unicode 字符起点，优先匹配最长 phrase；同长度按配置文件顺序稳定处理。最多应用 16 个匹配，expanded term 最多 64 个。原始 query text 始终保留为检索词之一，防止改写损失精确命中。

```text
rewrite(query):
  if text empty or top_k == 0: return None
  matches = longest_phrase_matches(normalize(text), graph.by_phrase)
  if matches empty: return None
  out_terms = unique([query.text])
  out_filters = query.filters.conditions.clone()
  boosted = []
  intent_seen = false

  for match in matches sorted by (length desc, config order):
    for edge in outgoing(match.node):
      match edge:
        Negation:
          append exclusion if field/value does not conflict
        AttributePropagation:
          append range/equals if compatible
        IntentTemplate:
          merge expansion.text terms, expansion.filters; intent_seen = true
        Synonym:
          append every `to` as expanded term
        Hyponym:
          append child and parent as terms; parent is category expansion

  resolve_conflicts(out_filters):
    - user filter wins over QUG filter on same field when compatible
    - incompatible ranges => empty candidate scope, do not drop a user filter
    - RefExcludes wins over RefContains intersection; record diagnostic conflict
    - duplicate conditions are deduplicated
  if out_terms == [query.text] && out_filters == query.filters && !intent_seen:
      return None
  return Some(RewrittenQuery { expanded_terms: out_terms, filters: Filters { conditions: out_filters }, boost_entities: boosted })
```

冲突处理是确定性的：用户显式条件优先；同字段 QUG 属性仅在与用户条件可交集时合并。`RefExcludes` 与 `RefContains` 的交集被从允许集合移除；若集合为空，候选域为空并返回零命中，不放宽用户条件。意图模板的 expansion.filters 按同一规则合并。`boost_entities` 在 Step3 只接受构建器提供的显式实体 key，未实现实体推断则为空。

`traverse(node, max_depth)` 返回从匹配节点出发、长度 1..depth 的简单路径；不返回零长度路径；禁止重复 NodeIndex，按边插入顺序稳定返回；未知 node 返回空 Vec。它是诊断和未来扩展 API，rewrite 不依赖不可控的全图遍历。

## 4. intents.yaml 与 domain.yaml

`DomainConfig` 新增：

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct QugConfig {
    #[serde(default = "default_qug_enabled")]
    pub enabled: bool,
    #[serde(default = "default_qug_max_depth")]
    pub max_depth: usize,
    #[serde(default = "default_qug_candidate_multiplier")]
    pub candidate_multiplier: usize,
    pub intent_templates: Option<String>,
}

// DomainConfig
pub qug: QugConfig;
```

缺少 `qug` 段时使用 `enabled=false`、`max_depth=2`、`candidate_multiplier=5`、`intent_templates=None`，保证 Step2 domain.yaml 完全兼容。未知字段继续忽略；路径相对 domain.yaml 所在目录解析。`candidate_multiplier` 限制为 1..20，超限报 `Error::Validation`。

`domain.yaml` 增量：

```yaml
qug:
  enabled: true
  max_depth: 2
  candidate_multiplier: 5
  intent_templates: intents.yaml
```

新增 `examples/milk-tea/intents.yaml`：

```yaml
version: "0.1.0"
intents:
  - id: cold_drink
    phrases: ["冰的", "冷饮"]
    expansion:
      text: "冰饮"
      filters: []
  - id: low_sugar
    phrases: ["不甜的", "少糖", "低糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠", "不加珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
```

实现级 serde schema：

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct IntentConfig { pub version: String, pub intents: Vec<IntentEntry> }
#[derive(Debug, Clone, Deserialize)]
pub struct IntentEntry {
    pub id: String,
    pub phrases: Vec<String>,
    #[serde(default)] pub expansion: Option<TemplateExpansion>,
    #[serde(default)] pub attribute: Option<AttributeRule>,
    #[serde(default)] pub negation: Option<NegationRule>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateExpansion { pub text: String, #[serde(default)] pub filters: Filters }
#[derive(Debug, Clone, Deserialize)]
pub struct AttributeRule { pub field: String, pub min: Option<f64>, pub max: Option<f64>, pub equals: Option<String> }
#[derive(Debug, Clone, Deserialize)]
pub struct NegationRule { pub field: String, pub refs: Vec<String> }
```

每个 entry 必须至少有一个非空 phrase 和恰好一个 rule（`expansion`、`attribute`、`negation`）；attribute 必须指定 min/max/equals 之一，negation 的 field 必须为 reflist，所有 field 必须在白名单中。非法配置在 `seed`/engine 构造阶段失败并说明文件、entry id、字段。

## 5. QueryEngine 编排与类型契约

模块布局：

```text
crates/wiktor-core/src/query_engine/
├── mod.rs       // QueryEngine, QueryResult, diagnostics
├── qug.rs       // QugGraph, builder, normalization, rewrite
├── hybrid.rs    // FTS/vector candidate retrieval and RRF
└── tests.rs      // focused unit/integration helpers (builder adds tests)
```

公开类型：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RewriteStatus { Applied, Fallback, Disabled }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryDiagnostics {
    pub rewrite_status: RewriteStatus,
    pub applied_filters: Filters,
    pub candidate_count: usize,
    pub fts_count: usize,
    pub vector_count: usize,
    pub rrf_k: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub hits: Vec<SearchHit>,
    pub rewritten: Option<RewrittenQuery>,
    pub rewrite_failure: bool,
    pub diagnostics: QueryDiagnostics,
    pub latency_ms: u64,
}

pub struct QueryEngine<V: VectorStore> {
    pub kernel: Arc<SqliteKernel>,
    pub entity_store: Arc<dyn EntityStore>,
    pub vector_store: Arc<V>,
    pub qug: Option<Arc<QugGraph>>,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub collection: String,
    pub candidate_multiplier: usize,
    pub rrf_k: u32,
}

#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

impl<V: VectorStore> QueryEngine<V> {
    pub async fn search(&self, query: &Query) -> Result<QueryResult>;
}
```

> **实现偏差记录（2026-09-21，struct-style-guard 巡检发现）**：
> 原设计 `QueryEngine` 含 `entity_store: Arc<dyn EntityStore>`，候选域用
> `entity_store.filter(filters)` 获取。实测该语义有误：`EntityStore::filter`
> 返回的是**事实平面实体（SKU，`milk-tea:product:*`）**，而知识页候选域需要
> **category 值（`milk-tea:drink:*`）**——直接用 SKU id 作 `pages.entity_id IN`
> 白名单会恒空（Tier B 实测 75.86% → 修后 100%）。
> 实现改为：`QueryEngine` 不再持有 `entity_store` 字段，候选域由
> `SqliteKernel::filter_page_candidates(filters)` 计算（SKU 过滤 → category
> 值集合，与 `search` 内联下推同语义）。`EntityStore` trait 保留供 ETL/未来
> 独立存储实现使用，但 QueryEngine 的候选域路径固定走 kernel 方法。
> **实现偏差记录（英文）**：见 `step3-query-engine.en.md` §5 同节。

`search` 内部步骤：

```text
validate top_k (1..=100), text length <= 4096
if qug disabled: rewritten=None, status=Disabled, filters=query.filters
else match qug.rewrite(query):
  Some(r): rewritten=Some(r), status=Applied, filters=merge result
  None: rewritten=None, rewrite_failure=true, status=Fallback, filters=query.filters
candidate_ids = entity_store.filter(filters)
if filters empty: candidate_ids=None (unbounded)
if candidate_ids is Some(empty): hits=[], write log, return
k = min(max(top_k * multiplier, 50), 500)
fts_hits = kernel.search_candidates(terms, filters, k, domain)
vector = embedder.embed(join terms)
vector_hits = vector_store.search(collection, vector, k, candidate_ids)
hits = rrf_merge(fts_hits, vector_hits, top_k, rrf_k=60)
write query_logs with rewritten/failure/hits
return QueryResult
```

`SqliteKernel::search_candidates` 是 Step2 `search` 的抽取版，不写 query_logs，签名为：

```rust
pub fn search_candidates(
    &self,
    terms: &[String],
    filters: &Filters,
    top_k: usize,
    domain: Option<&str>,
    candidate_ids: Option<&[EntityId]>,
) -> Result<Vec<SearchHit>>;
```

它对每个 term 独立执行 FTS/LIKE，按页面取最高 BM25 分；`candidate_ids` 通过参数化临时 `IN` 条件传入。filters 仍使用 Step2 `facts::filter_where`；engine 已预筛时，kernel 不重复查询事实表，只限制 entity_id。空 terms 使用原始 query text。旧 `search` 保留兼容，内部调用 `search_candidates` 后写旧格式日志。

## 6. 混合检索与 RRF

FTS 以 `expanded_terms`（包含原文）逐词召回，向量以去重后的 terms 用空格连接生成 embedding。两路都使用 `candidate_ids`。无向量服务时必须注入 `MockVectorStore`；向量错误默认返回 `Error::VectorStore`，不静默当作纯 FTS，CLI 提供 `--no-vector` 显式降级并在诊断中标记。

RRF 公式：对每个实体的每个来源排名 `r`，贡献 `1 / (60 + r)`，排名从 1 开始。VectorHit 的 `metadata.entity_id` 映射到 entity；多个 page hit 以 page_id 为最终去重键，贡献按同一 `page_id` 累加。最终按 `(rrf_score DESC, page_id ASC)` 排序，映射为 `SearchHit.score=rrf_score`。若只有一路有结果，仍按该路贡献计算，不能改回原始分数。

```rust
pub fn rrf_merge(
    fts: &[SearchHit],
    vectors: &[VectorHit],
    top_k: usize,
    k: u32,
) -> Vec<SearchHit>;
```

`candidate_ids` 为空表示无过滤；非空表示严格白名单。FTS 结果中的 page entity 必须属于白名单，否则丢弃；这是防止事实与页面代际短暂不一致时越界的第二道保护。generation/content_hash 对齐由向量写入契约负责，Step3 不修改发布状态机。

## 7. 查询日志与诊断

`rewritten_json` 序列化完整 `RewrittenQuery`，包含 `expanded_terms`、合并后的 `filters`、`boost_entities`；未改写时为 NULL。原始 `Query` 仍写入 `query_json`。`rewrite_failure=1` 只在 QUG 已启用且 `rewrite` 返回 `Ok(None)` 时写入；配置 disabled 不算失败，写 0。输入校验错误和存储错误可写失败日志但 `rewrite_failure` 按上述语义保持 0。

QueryEngine 日志必须在成功和可恢复的空结果路径写入，字段为：原文、query_json、rewritten_json、rewrite_failure、hit_count、latency_ms、timestamp。日志写入失败不改变已得到的查询结果，但通过 tracing 报错；查询本身的数据库错误仍返回 Err。

CLI 使用 `QueryResult.diagnostics` 展示 `rewrite_status`、`candidate_count`、`applied_filters`。`applied_filters` 是最终 AND 条件，便于诊断 QUG 是否真正落到事实平面。

## 8. CLI 展示契约

命令：

```text
wiktor search "不要珍珠的低糖奶茶" --db wiktor.db --domain examples/milk-tea/domain.yaml --top-k 5
wiktor search "波霸奶茶" --db wiktor.db --top-k 5 --json
```

默认输出必须精确遵循以下字段与顺序（数值允许实际变化）：

```text
query: 不要珍珠的低糖奶茶
rewrite: applied
expanded_terms: 不要珍珠的低糖奶茶, 奶茶
filters: sugar_level<=30; ingredient_ids not_in=milk-tea:ingredient:pearl
candidates: 7  fts: 5  vector: 5  rrf_k: 60
score  entity_id                           title
0.0310 milk-tea:drink:low-sugar-tea         低糖奶茶
```

fallback 示例：

```text
query: 火星口味
rewrite: fallback (no matching QUG path)
expanded_terms: 火星口味
filters: none
candidates: all  fts: 0  vector: 0  rrf_k: 60
no hits
```

无命中固定输出 `no hits`。`--json` 输出单个 JSON 对象，字段为 `query`、`hits`、`rewritten`、`rewrite_failure`、`diagnostics`、`latency_ms`，不混入人类可读行；退出码仍为 0（查询成功但无命中）。

## 9. golden 评测与验收判据

测试使用同一 seed 数据库和同一 query 集，分别运行：

- **A：纯 FTS**：`search_candidates` 只用原文，不向量。
- **B：混合**：原文 FTS + MockVectorStore，不 QUG。
- **C：QUG**：QueryEngine 完整路径。

每条 golden 按 Step2 规则，以 `命中实体集合 ∩ 期望集合非空` 计通过；报告 pass rate 和相对 B 的增益 `(C-B)/max(B,1)`。退出判定为 C 比 B 的绝对通过率提升 `<5` 个百分点且没有回退时，输出 `QUG disabled` 诊断并测试通过；若有回退，测试失败并要求修复。

| # | 判据 | 可测断言 |
|---|---|---|
| A1 | QUG 图构建 | aliases/tags/intents 生成五类边所需的边；节点归一化、重复边去重，source_hash 稳定。 |
| A2 | 同义改写 | 查询“啵啵”返回 `milk-tea:drink:boba-milk-tea`，`expanded_terms` 含“波霸奶茶”或“珍珠奶茶”。 |
| A3 | 上下位扩展 | 查询“奶茶”可命中由 tags 关联的饮品页；`traverse("奶茶", 2)` 路径深度不超过 2。 |
| A4 | 属性传播 | “不甜的奶茶”生成 `sugar_level max=30`，最终每个命中实体存在满足条件 SKU。 |
| A5 | 否定 | “不要珍珠”生成 `RefExcludes(ingredient_ids, milk-tea:ingredient:pearl)`，结果不含仅有珍珠 SKU 的页。 |
| A6 | 意图模板 | “冰的”加载 intents.yaml 并加入 expansion term/filter；缺失文件时构造 engine 失败且带路径。 |
| A7 | 显式 fallback | 未知词返回 `rewritten=None`、`rewrite_failure=true`、status=Fallback，仍执行 FTS+vector 并写日志。 |
| A8 | disabled | `qug.enabled=false` 时不调用 rewrite，failure=false、status=Disabled。 |
| A9 | 过滤合并 | 用户 `price<=20` 与 QUG `sugar_level<=30` 同时下推，条件是 AND；冲突用户条件不被覆盖。 |
| A10 | 候选域传导 | EntityStore 返回的 entity IDs 同时约束 FTS 与 Vector；Mock vector 不返回白名单外实体。 |
| A11 | RRF | `k=60`、排名从 1 开始；两路同页总分高于单路同排名，结果稳定按 page_id 打破平局。 |
| A12 | 无 qdrant 闭环 | MockVectorStore + deterministic embedder 在无网络环境完成 QueryEngine 查询。 |
| A13 | 日志 | applied/fallback/disabled 三种路径各写一条日志；rewritten_json、failure、hit_count 与结果一致。 |
| A14 | CLI 契约 | 默认输出含 rewrite、filters、candidate counts、表头；`--json` 为可解析单 JSON；无命中为 `no hits`。 |
| A15 | golden 三档 | A/B/C 均产出 pass rate；C 增益≥5% 则 enabled 通过，增益<5%且无回退则 disabled 通过。 |
| A16 | 可靠性边界 | top_k、文本长度、depth、候选上限超限返回 Validation；页面 quarantine 不出现在任一路召回。 |
| A17 | 工程规范 | `cargo fmt --check`、`cargo clippy --workspace --all-targets`、`cargo test --workspace` 全绿。 |

## 10. 文件与模块布局

```text
examples/milk-tea/
├── domain.yaml                 # 增加 qug 段
└── intents.yaml                # 新增意图/属性/否定模板

crates/wiktor-core/src/
├── query_engine/
│   ├── mod.rs
│   ├── qug.rs
│   ├── hybrid.rs
│   └── tests.rs
├── traits/domain_pack.rs       # QugConfig + serde 兼容
├── kernel/sqlite.rs            # search_candidates；旧 search 兼容
├── types/query.rs              # 诊断类型（或由 query_engine re-export）
└── lib.rs                      # pub mod query_engine

crates/wiktor-cli/src/
├── main.rs                     # Search 改为 QueryEngine
└── embed.rs                    # deterministic CLI embedder；明确标注本地基线

tests/
└── step3_query_engine.rs       # builder 按 A1-A17 编写集成验收
```

依赖：workspace 增加 `petgraph`；已有 serde/serde_json/serde_yaml_ng/diesel/async-trait 复用。不要增加 sqlite-vec 或远程 qdrant 为测试前置条件。

## 11. 实现顺序建议

1. 扩展 `QugConfig`、实现 intents.yaml serde 校验；运行旧 Step2 测试确认向后兼容。
2. 引入 petgraph，实现归一化、边去重、`traverse`；用纯内存边单测验证 A1/A3。
3. 实现 seed page aliases/tags 和 intents 边提取，新增奶茶 intents.yaml；验证 A2/A4/A5/A6。
4. 抽取 `SqliteKernel::search_candidates`，保持 Step2 `search` 行为与日志兼容；验证 FTS 和候选域限制。
5. 实现 `rrf_merge` 与 deterministic embedder，先用 MockVectorStore 跑 A11/A12。
6. 实现 `QueryEngine::search`、日志和诊断；验证 A7-A10/A13/A16。
7. CLI 接入 QueryEngine、`--json`、展示契约；验证 A14。
8. 升级 golden runner，输出 A/B/C 与退出判定；验证 A15。
9. 运行 fmt、clippy、workspace tests，完成 A17；再由 test-engineer 补齐独立测试。

每一步都应保持可编译；QUG 构建失败不得静默启用部分图，VectorStore 错误不得静默伪装成成功的纯 FTS 结果。
