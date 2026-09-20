# Wiktor 方案总纲（v3 定稿）

> v2（移除 DSL 版）+ 评审修正 + 品牌决策的合并版。
> v3 相对 v2 的实质变化见文末「变更记录」。
>
> **v3.1 同步说明（2026-09-20）**：架构分叉已拍板（最终形态 = 数据库），存储内核改为 SQLite 一体化（WAL/FTS5/sqlite-vec/litestream，弃 redb + 自写 openraft log storage），全文检索用 FTS5，serde_yaml → serde_yaml_ng，另合入盲审团三席共识修正（重编译刹车、过滤下推、可靠性契约、MVP 定义等）。**设计以 `MASTER-PLAN.md`（v3.1）为准**；本文的阶段划分与验收标准继续有效，但文中 redb / openraft / 自研倒排 / tantivy 时点等表述按 v3.1 的已决事项解读。

## 一、项目概述

**项目名**：Wiktor（wiki + vector，读作"维克托"）
**定位**：知识编译与检索中间件
**一句话**：像数据库一样提供 API、集群和高可用能力，用 LLM 在摄入时将原始数据编译为结构化 Wiki，用混合检索在查询时做语义召回。

**核心叙事**：
- 通用中间件底座 + 可插拔领域包
- 核心层只认识实体、类型、字段、关系；商品、文档、代码都是领域包
- 商品是第一个官方领域包（Reference Implementation）
- 读写彻底分离：写入慢（LLM 编译，异步），查询快（内存索引，零 LLM）

**对标**：etcd 的可靠性 + Redis 的查询体验 + Meilisearch 的检索质量

## 二、问题与动机

**现有方案的痛点**：
- 传统 RAG 每次查询都要重新检索和推理，知识无法积累
- LLM Wiki 编译质量不稳定，没有质量评估和迭代机制
- 查询改写依赖静态同义词表，无法处理复杂意图
- 编译和检索是单向管道，检索时发现的知识盲区无法反馈
- 没有中间件级的开源项目把"编译 + 检索"整合成可集群、可插件、可领域扩展的系统

**Wiktor 的解法**：把知识编译从一次性 LLM 调用升级为**可观测、可迭代、可反馈的工程系统**。

## 三、核心设计原则

1. **两平面数据模型**（v3 新增，最高优先级）：
   - **知识平面（Wiki）**：定义、同义词、品类层级、配料关系、意图模板——慢变化，LLM 编译，纯 Markdown，人类可读。这才是 Wiki 页面。
   - **事实平面（元数据存储）**：price、stock、糖度等 filterable 字段——快变化，普通 ETL 直写，完全不走 LLM。
   - **铁律：SKU 高频变动字段永不进 Wiki 页面**，否则每次改价 = 一次 LLM 重编译，成本荒谬。
2. **Wiki 是知识的事实来源，向量是派生索引**。Markdown 可版本控制、可迁移；删掉向量索引后能从 Markdown 完整重建。
3. **查询路径零 LLM、零 async 外部调用**。QUG 图遍历 + 内存索引 + rayon 并行，纯 CPU。
4. **QUG 必须有 fallback**：QUG 无法处理的查询退回纯混合检索，这是一等查询路径，不是日志副产品。
5. **领域假设不进核心层**。品类层级、属性键值、同义词映射全部下沉到领域包。
6. **插件只依赖 core，不依赖 server**。向量库、数据源适配器都是 core trait 的实现者。
7. **单二进制分发**。CLI + TUI + 服务端打包在一起，下载即用。
8. **领域包用 YAML + Prompt 模板 + 页面模板**，不引入自定义 DSL。

## 四、三大核心创新

### 创新一：编译质量可观测（v3 拆分了成本模型）

五维度分两类实现：

| 维度 | 本质 | 阶段 | 评估方式 | 阈值触发 |
|------|------|------|---------|---------|
| 覆盖度 | **规则可算** | 一 | 源数据字段被 Wiki 引用的比例（依赖 Prompt 输出契约强制带 source 引用） | < 60% 重编译 |
| 引用完整性 | **规则可算** | 一 | 每个断言是否有 source 字段支撑，机械比对 | 无引用 → 标记待验证 |
| 结构合规 | **规则可算** | 一 | 是否符合领域包 Schema，serde 校验 | 不合规 → 拒绝入库 |
| 信息密度 | **近似规则** | 一 | 有效信息 token / 总 token | < 40% 压缩重编译 |
| 一致性 | **需要 LLM** | 二 | 与已有页面矛盾检测；O(N) 成本，必须用"嵌入召回 top-k 相关页 + LLM 仲裁"近似，禁止全量比对 | 矛盾 → 人工审核队列 |

**关键机制**：Prompt 输出契约要求每个断言携带 source 字段引用——这一条让四个维度从"LLM 评估"变成"机械校验"，质量评分器一周可落地而非一月。

**质量仪表盘**：按品类、按时间、按模型展示质量趋势。低质量页面自动进重编译队列，形成"编译 → 评分 → 重编译"循环。

### 创新二：查询理解图（QUG）

五类语义边，编译时构建、查询时只读遍历、毫秒级：

| 边类型 | 示例 | 查询时行为 |
|--------|------|-----------|
| 同义边 | 啵啵 ↔ 珍珠/波霸 | 直接替换 |
| 上下位边 | 奶绿 → 奶茶 → 饮品 | 品类扩展召回 |
| 属性传播边 | "不甜的" → 糖度 ≤ 30% | 转为结构化过滤（**落事实平面**） |
| 意图模板边 | "适合冬天喝的" → 热饮 + 高热量 | 展开为复合查询 |
| 否定边 | "不要珍珠" → 排除配料含珍珠 | 生成排除过滤器 |

**v3 补充**：
- **意图模板边的来源双轨制**：高频模板由领域包作者手写进 YAML（模板即领域知识），LLM 抽取只做长尾增量。
- **显式 fallback**：QUG 无匹配路径 → 直接走混合检索，并在查询日志标记 `rewrite_failure` 供反馈层分析。

### 创新三：编译-检索双向反馈

```
编译 → 索引 → 检索 → 查询日志 → 盲区分析 → 补充编译
  ↑                                              │
  └──────────────────────────────────────────────┘
```

三个盲区信号：零召回查询 / 低质量召回（点击·采纳率低）/ 查询改写失败。

**v3 补充**：
- **Relevance Feedback API**（`POST /feedback`）进阶段二交付物——中间件没有 UI，采纳信号必须由上层应用回传。
- 反馈任务进人工审核队列，不自动执行，防止噪音污染。

## 五、架构总览（v3 修订：显式两平面 + fallback）

```
┌─────────────────────────────────────────────────────────────┐
│  客户端层                                                     │
│  CLI  │  TUI（质量仪表盘 + 查询调试） │  Web UI（阶段四）      │
└──────────────────────────┬──────────────────────────────────┘
                           │ gRPC / HTTP / WebSocket / SSE
┌──────────────────────────▼──────────────────────────────────┐
│  API 层    tonic (gRPC) │ axum (HTTP) │ POST /feedback      │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  QueryEngine（纯内存，同步，零 LLM）                          │
│  QUG 图遍历 ──(失败→fallback)──→ 混合检索                    │
│  → RRF 融合 → 结构化过滤【事实平面】 → 结果                   │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  索引层（内存常驻，原子 swap 替换，可重建）                    │
│  向量索引 │ BM25 倒排 │ QUG 图 │ 同义词哈希 │ 关系图          │
└──────────────────────────┬──────────────────────────────────┘
                           │ 异步同步
┌──────────────────────────▼──────────────────────────────────┐
│  存储层（Raft 复制，阶段三）                                   │
│  【知识平面】Wiki Markdown + 质量评分 + 查询日志 + 任务队列    │
│  【事实平面】结构化元数据（ETL 直写，不走 LLM）                │
└──────────────────────────┬──────────────────────────────────┘
                           │ 任务驱动
┌──────────────────────────▼──────────────────────────────────┐
│  编译层（异步 Worker，可水平扩展）                             │
│  YAML 解析 → 内容哈希增量判断 → LLM 编译 → 质量评分 → 入库     │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│  反馈层（异步分析）                                            │
│  查询日志 + feedback API → 盲区分析 → 补充编译任务（人工审核）  │
└─────────────────────────────────────────────────────────────┘
```

## 六、领域包配置

```yaml
# domains/ecommerce/domain.yaml
name: ecommerce
version: 1.0

entities:
  - name: product
    source: jsonl://examples/milk-tea/products.jsonl   # 阶段一 JSONL，阶段二加 postgres://
    id_field: sku_id
    type_field: category_path
    fields:
      - name: price
        type: numeric
        filterable: true      # filterable/numeric → 事实平面，永不进 Wiki
      - name: ingredients
        type: list<alias>
        alias_source: ingredient_aliases   # 语义字段 → 知识平面，进编译

types:
  - name: category
    parent_field: parent_category
    compile: true
    template: templates/category_page.md
  - name: brand
    compile: true

relations:
  - name: belongs_to
    from: product
    to: category
    extract: source_field
  - name: pairs_with
    from: ingredient
    to: ingredient
    extract: llm

qug:
  intent_templates: templates/intents.yaml   # 手写高频意图模板
  extract_edges: llm                          # LLM 抽取长尾边

compile:
  prompt: prompts/product_compile.md
  output_contract: require_source_refs        # 强制断言带 source 引用
  quality_threshold: 0.75
  on_low_quality: recompile
  incremental: content_hash                   # v3 补回：内容哈希增量编译

query:
  rewrite: qug
  fallback: hybrid_search                     # v3 显式 fallback
  filters: [price, category, ingredients]
  rerank: cross_encoder                       # 阶段二
```

## 七、核心抽象（Rust trait）

```rust
// 数据源适配器
trait DataSource {
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;
    fn schema(&self) -> EntitySchema;
}

// 事实平面存储（v3 新增）
trait EntityStore {
    async fn upsert_facts(&self, id: &EntityId, facts: &Facts) -> Result<()>;
    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>>;
}

// 编译器（带质量评分）
trait Compiler {
    async fn compile(&self, raw: RawEntity, ctx: &CompileContext) -> Result<CompiledPage>;
}

struct CompiledPage {
    wiki: WikiPage,
    quality: QualityScore,
    qug_edges: Vec<QugEdge>,
    content_hash: u64,   // 增量编译依据
}

struct QualityScore {
    coverage: f32,          // 规则
    citation: f32,          // 规则
    schema_compliance: f32, // 规则
    density: f32,           // 规则
    consistency: Option<f32>, // LLM，阶段二
}

// 查询理解图
trait QueryUnderstandingGraph {
    /// 返回 None = QUG 无法处理，调用方必须 fallback 到混合检索
    fn rewrite(&self, query: &Query) -> Option<RewrittenQuery>;
    fn traverse(&self, node: &str, depth: usize) -> Vec<QugPath>;
}

enum QugEdge {
    Synonym { from: String, to: Vec<String> },
    Hyponym { from: String, to: String },
    AttributePropagation { phrase: String, filter: Filter },  // 落事实平面
    IntentTemplate { phrase: String, expansion: Query },
    Negation { phrase: String, exclusion: Filter },
}

// 领域包
trait DomainPack {
    fn name(&self) -> &str;
    fn config(&self) -> &DomainConfig;
    fn compiler(&self) -> Box<dyn Compiler>;
    fn qug_builder(&self) -> Box<dyn QugBuilder>;
    fn reranker(&self) -> Option<Box<dyn Reranker>>;
}

// 反馈分析器
trait FeedbackAnalyzer {
    async fn analyze(&self, logs: &[QueryLog]) -> FeedbackReport;
}

struct FeedbackReport {
    zero_recall_queries: Vec<Query>,
    low_quality_hits: Vec<PageId>,
    rewrite_failures: Vec<Query>,
    suggested_compilations: Vec<CompileTask>,  // 进人工审核队列
}

// 向量库适配器
trait VectorStore {
    async fn upsert(&self, collection: &str, ids: &[String], vectors: &[Vec<f32>], metadata: &[Metadata]) -> Result<()>;
    async fn search(&self, collection: &str, query: &[f32], top_k: usize, filters: Option<&Filters>) -> Result<Vec<SearchHit>>;
    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()>;
}
```

## 八、仓库结构

```
wiktor/                          # Cargo workspace 单仓
├── Cargo.toml
├── crates/
│   ├── wiktor-core/             # 核心引擎（trait + QueryEngine + QUG + 两平面模型）
│   ├── wiktor-quality/          # 质量评分器（阶段一：4 规则维度）
│   ├── wiktor-feedback/         # 反馈分析器（阶段二）
│   ├── wiktor-server/           # 服务端 gRPC + HTTP（阶段二）
│   ├── wiktor-cli/              # CLI + TUI（同一二进制）
│   ├── wiktor-vector-bruteforce/ # 阶段一：暴力扫描（500 商品足够）
│   ├── wiktor-vector-hnsw/      # 阶段二
│   ├── wiktor-vector-qdrant/    # 阶段四
│   ├── wiktor-vector-lancedb/   # 阶段四
│   ├── wiktor-vector-pgvector/  # 阶段二第一个真插件
│   └── wiktor-adapter-jsonl/    # 阶段一数据源（postgres 适配器阶段二）
├── domains/
│   └── ecommerce/               # 官方商品领域包
│       ├── domain.yaml
│       ├── prompts/
│       └── templates/
├── docs/
│   ├── PLAN.md                  # 本文档
│   ├── brand/                   # 品牌资产（见 docs/brand/）
│   ├── domain-pack.md
│   ├── quality-metrics.md
│   └── qug-design.md
└── examples/
    └── milk-tea/
        ├── products.jsonl       # 100-500 个商品
        ├── golden-queries.jsonl # 评测集：查询 → 期望命中商品列表（一等交付物）
        └── seed-wiki/           # 手工编译的 20 个种子 Wiki 页面
```

## 九、技术栈（v3 修正）

| 层级 | 选型 | 说明 |
|------|------|------|
| 语言 | Rust | 核心、服务端、CLI/TUI |
| 异步运行时 | tokio | |
| gRPC / HTTP | tonic / axum | 阶段二 |
| Raft | openraft | 阶段三；redb 无现成 log storage，需自写，预留数月 |
| 嵌入式存储 | **redb**（去掉 sled，其维护已停滞） | 单机存储引擎 |
| 向量索引 | 阶段一暴力扫描 → 阶段二 hnsw_rs 或 arroy | 500 商品不需要 HNSW |
| 全文检索 | 阶段一简单内存倒排 → 阶段二 tantivy | 同上 |
| 图结构 | petgraph | QUG |
| 配置解析 | serde_yaml | 领域包 |
| 并发哈希 | dashmap | 同义词映射 |
| 缓存 | moka | 查询结果缓存 |
| TUI | ratatui + crossterm | 仪表盘 |
| Web UI | Tauri + React | 阶段四 |
| LLM 调用 | rig 或 async-openai | 编译层，trait 隔离可替换 |
| 嵌入模型 | fastembed-rs | 本地 BGE；嵌入粒度：页面摘要 + 章节级双索引（阶段二） |
| 序列化 | serde + prost | |
| 可观测性 | tracing + prometheus | |

## 十、路线图（v3 重排阶段一）

### 阶段一：单机验证版（1-2 个月，三步走）

**第一步（第 1 周）：手工编译验证数据模型——先不接 LLM**
- `wiktor-core` trait 定义 + 两平面数据模型
- 手工编写 20 个奶茶种子 Wiki 页面（`examples/milk-tea/seed-wiki/`）
- JSONL 事实平面 + 暴力扫描向量检索 + 简单倒排
- **目的：让数据模型问题不被 prompt 调试绑架**

**第二步（第 2-4 周）：LLM 编译管线 + 质量评分**
- `wiktor-quality` 四个规则维度（coverage / citation / schema / density）
- Prompt 输出契约（require_source_refs）
- 内容哈希增量编译
- `wiktor-cli`：`compile`、`search`、`status`（JSON 输出，TUI 后置）

**第三步（第 5-8 周）：QUG + 评测**
- QUG 五类边构建（手写模板 + LLM 抽取）+ 显式 fallback
- golden-queries.jsonl 评测集
- `wiktor tui` 质量仪表盘（如时间允许）

**验证指标**：
- 质量评分与人工评估相关性 > 0.7
- 奶茶场景召回率：纯向量 vs 混合 vs QUG 增强（基于 golden queries）
- YAML 领域包能否描述奶茶领域全部规则

### 阶段二：中间件化（2-3 个月）
- `wiktor-server`：gRPC + HTTP + **POST /feedback**
- 混合检索（BM25 + 向量 + RRF）；tantivy + hnsw_rs/arroy
- QUG 一致性维度（嵌入召回 top-k + LLM 仲裁）
- `wiktor-feedback` + postgres 数据源适配器 + pgvector 插件
- Prometheus 指标 + 结构化日志 + 领域包注册机制

**验证指标**：QUG vs 静态映射准确率；反馈闭环迭代 3 轮的召回提升；查询 P99 < 50ms；插件接入成本 < 半天

### 阶段三：高可用（3-6 个月）
- openraft 存储层复制（自写 redb log storage）+ 索引层异步同步
- CLI `cluster` 命令组；主故障切换 < 10s

### 阶段四：集群与生态（6 个月+）
- 命名空间分片 + 路由；Qdrant/LanceDB/Milvus 插件；Web UI（Tauri）
- 第二个官方领域包（技术文档）+ 领域包贡献指南

## 十一、性能目标

| 查询类型 | 目标 P99 | 路径 |
|---------|---------|------|
| 缓存命中 | < 5ms | moka |
| 纯结构化过滤 | < 10ms | 事实平面内存索引 |
| 纯向量检索 | < 20ms | 阶段二 HNSW（阶段一暴力扫描放宽） |
| 混合检索 + RRF | < 50ms | 向量 + BM25 + 融合 |
| 含 QUG 改写 | < 60ms | 图遍历 + 混合检索 |
| QUG fallback | 同混合检索 | 无惩罚 |
| 写入路径 | 分钟级 | 异步任务 |

## 十二、关键设计决策

1. **两平面数据模型**：知识进 Wiki（LLM 编译），事实进元数据（ETL 直写）。filterable/numeric 字段永不进 Wiki。
2. **查询路径零 LLM、零 async 外部调用**，QUG 有显式 fallback。
3. **质量评分四规则维度 + 一 LLM 维度**，Prompt 输出契约（require_source_refs）是四规则维度的前提。
4. **QUG 编译时构建、查询时只读**，意图模板手写 + LLM 长尾双轨。
5. **增量编译靠内容哈希**，索引替换原子 swap。
6. **反馈闭环异步 + 人工审核**，relevance feedback API 是阶段二交付物。
7. **阶段一先手工编译后接 LLM**，评测集（golden queries）与数据集同为一等交付物。
8. **领域包 YAML + Prompt + 模板，无 DSL**。
9. **前期单仓 Cargo workspace**，向量插件与 UI 保持可迁移依赖。

## 十三、风险与应对

| 风险 | 应对 |
|------|------|
| 质量评分与人工评估偏差大 | 阶段一重点验证，评分模型可替换 |
| QUG 图构建成本高 | 增量构建 + 内容哈希 |
| YAML 表达力不足 | 第一步手工编译即验证，必要时有限扩展 |
| 反馈闭环噪音 | 人工审核队列，不自动执行 |
| 规模上限（Wiki 体积爆炸） | 品类分级：核心深度编译，长尾轻量索引 |
| Rust LLM 生态薄弱 | trait 隔离，rig 不满足换 async-openai |
| 工程复杂度失控 | 严格分阶段，阶段一不做 Raft/集群/Web UI/一致性维度 |
| openraft + redb 集成难度 | 预留数月；若失败降级为主从复制 |
| 开源冷启动难 | 奶茶领域包做成"教科书级" |

## 十四、品牌

- **吉祥物**：蜘蛛。动作级契合：结网=编译、振动感知=零 LLM 检索、网可重织=向量索引可重建。舍弃强行映射（8 腿=协议、8 眼=质量维度）。
- **主 logo**：**纯几何蛛网 + 中心 W**（不画蜘蛛）——规避两个在先占用：Tarantool（数据库，蜘蛛品牌）与 Scrapy（爬虫框架，spider 即抓取类），同时避免"蜘蛛+搜索"被误读为爬虫工具。
- **蜘蛛为次级角色**：文档插画、release note、TUI 彩蛋 `wiktor spider`；腹部带 `#` 呼应 Markdown。
- **色板**：深藏青 `#101A2E` / 暖橙 `#E8833A`（蛛丝）/ 米白 `#F5F0E8`（节点与 W）/ 米色 `#FAF7F2`（浅底）。
- **TUI 启动画面 ASCII**（辐条 + 双螺旋环 + W）：

```
      \  |  /
    .-.\\|//.-.
   (   \\|//   )
  --(--  W  --)--
   (   //|\\   )
    '-'//|\\'-'
      /  |  \

     w i k t o r
```

- 资产：`docs/brand/`（ascii-logo.md、gen_logo.py、wiktor-web[-dark|-light].svg、wiktor-spider-dark.svg）
- **命名检查结果**（2026-09-19）：crates.io `wiktor` ✅ 可用；GitHub `wiktor` ❌ 被 2008 年老账号占用，`wiktor-rs` / `wiktor-db` ✅ 可用。建议 crate 名 `wiktor` + org 名 `wiktor-rs`（二者不要求一致）。域名 wiktor.dev / wiktor.io 注册前自行确认。

## 十五、下一步行动

1. 注册 crates.io `wiktor` + GitHub org `wiktor-rs`（可先占坑）
2. `wiktor-core` trait 定义（含两平面模型、QUG 返回 Option、QualityScore 四+一字段）
3. 领域包 YAML 定稿，用奶茶领域验证表达力
4. Cargo workspace + CI 骨架
5. 奶茶示例：products.jsonl + golden-queries.jsonl + 20 个手工种子 Wiki 页面
6. 跑通最小闭环：手工 Wiki → 索引 → QUG/fallback 查询 → CLI 展示
7. 接 LLM 编译管线 + 四规则维度质量评分
8. 评测：纯向量 vs 混合 vs QUG 的召回率对比报告

## 变更记录

**v3.1（2026-09-20）**：盲审团三席复审一致结论"需修改后合入"，共识项已合并进 `MASTER-PLAN.md` v3.1：①架构分叉拍板——最终形态为全栈中间件（数据库），落地"最小内核先行"，外部引擎（Meilisearch）降为可选插件；②存储栈 SQLite 一体化（WAL/FTS5/sqlite-vec/litestream），弃 redb+openraft 默认路线；③全文检索 FTS5（tantivy 转超规模演进选项）；④serde_yaml → serde_yaml_ng；⑤重编译刹车（页级上限 2 次 + token 预算熔断）；⑥过滤下推修正（原"RRF 后过滤"漏召回）；⑦新增可靠性契约（全依赖 BLAKE3 哈希、任务状态机、source_revision CAS、quarantine 发布状态机、缓存失效、Prompt 注入防护）；⑧评测交付物补全（golden-queries ≥100 条 + 50-100 页人工标注集）+ QUG 退出条件（增益 <5% 默认关闭）；⑨对标叙事修正（体验对标 Meilisearch、可靠性对标 etcd）；⑩MVP 定义（TUI/SSE/Raft/外部插件不进最小完整形态）。本文阶段划分与验收标准继续有效，技术选型表述按上述解读。

**v3（2026-09-19）相对 v2**：
1. 新增**两平面数据模型**（原则 #1、架构图、EntityStore trait）——解决 v2 未回答的"商品分布在 Wiki 中？向量仅是索引？"：知识进 Wiki，事实进元数据，向量索引覆盖两平面。
2. **质量评分拆分**：四规则维度（阶段一，依赖 require_source_refs 契约）+ 一致性 LLM 维度（阶段二，top-k 仲裁禁止全量比对）。
3. **QUG 显式 fallback**（`rewrite` 返回 Option）+ 意图模板手写/抽取双轨 + golden-queries 评测集升为一等交付物。
4. **技术栈修正**：去 sled 用 redb；阶段一去 HNSW/tantivy（暴力扫描 + 简单倒排），数据源先 JSONL。
5. **阶段一重排**：先手工编译 20 页验证数据模型再接 LLM；TUI 后置；补回内容哈希增量、原子 swap、relevance feedback API。
6. **品牌定稿**：蛛网主 logo + 蜘蛛吉祥物（含 Tarantool/Scrapy 占用事实与规避策略），色板与 ASCII 定稿。
