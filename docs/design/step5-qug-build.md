# Step 5 Spec：QUG 五类边构建、持久化、Golden 扩容与三档评测

> 版本：v1.0（2026-09-22）  
> 上承接：Step 3 `step3-query-engine.md`、Step 4 `step4-compile-pipeline.md`  
> 实现对象：`wiktor-builder`；独立验收对象：`test-engineer`  
> 本文是中文权威设计；英文版 `step5-qug-build.en.md` 按本文件逐节对应。

## 1. 目标与非目标

Step 5 把 Step 3 的内存 QUG 构建升级为可审计、可复用、可失效重建的 SQLite 派生索引，并用不少于 100 条 golden 查询正式决定 QUG 是否默认启用。默认查询仍零 LLM、零远程调用。

交付范围：五类边的确定性构建；BLAKE3 source hash 与失败保护；页面边和无页面归属意图边的持久化；seed 与 Step4 accepted 编译页合并；`golden-queries.jsonl` 扩容；`wiktor qug build`；`wiktor eval`；A/B/C 评测与退出判定。

非目标：LLM 抽边、查询时建图、qdrant 实现、rerank、反馈自动编译、自动改写 golden、跨进程热交换。QUG 构建错误不得静默发布半成品图。

## 2. 术语与既有约束

- 页面边：由 accepted 页 frontmatter 产生的 Synonym 或 Hyponym。
- 配置边：由 `intents.yaml` 产生的 AttributePropagation、IntentTemplate 或 Negation；没有 page_id。
- accepted 页：`pages.status='accepted'` 的页面；包含 generation=1、artifact=`seed-v1` 的 legacy seed 页和 Step4 发布的 accepted compiled 页。
- active build：一个 domain/version 下唯一可读的 `published` QUG 构建代次。
- source_hash：覆盖领域版本、QUG 配置、意图文件、参与页面身份的 BLAKE3 哈希。
- fallback：rewrite 无匹配、QUG disabled、active 图缺失或当前 hash 不一致时，直接走 Step3 混合检索；必须写 `rewrite_failure` 或 `disabled` 诊断。

Step3 的 `QugEdge`、normalization、最长短语、冲突处理、max depth、candidate multiplier 与 RRF 语义继续有效。Step5 只改变图的来源、存储和评测，不重新定义 rewrite。

## 3. 决策 D1–D7

### D1：边来源与触发

`Synonym` 与 `Hyponym` 机械派生自 accepted 页 frontmatter：每个 alias 生成 `alias -> title` 的 Synonym；每个 tag 生成 `title -> tag` 的 Hyponym；不自动 alias 全连接、不推断 tag 层级。`AttributePropagation`、`IntentTemplate`、`Negation` 逐条派生自 `intents.yaml`，复用并收敛 Step3 的 `extract_page_edges`/`intent_edges` 校验逻辑。禁止 LLM 抽边。

触发采用显式 `wiktor qug build`。compile publish 不隐式维护全图；这样单页失败不会产生混合代次，构建有明确审计边界。备选是 compile 后增量更新或 LLM 长尾抽边；它们增加一致性和不可重复风险，本步不采用。

### D2：source_hash 与重建

source_hash 的规范化输入为：`domain name/version`、builder 版本 `qug-build-v1`、QugConfig canonical JSON、intents.yaml 原始 bytes、按 UTF-8 page_id 排序的 `(page_id,generation,content_hash,artifact_version,frontmatter_json,status)`，并采用固定前缀和长度域编码后计算 BLAKE3 小写 hex。有效 edge 的 canonical JSON 和排序结果也纳入哈希。

hash 相同且 active build 为 published 时复用；任一输入变化即新建 build。重建在一个 SQLite 事务中完成：创建 `building` → 写两类边 → 校验五类计数与 hash → 新 build `published`、旧 build `superseded`。任一解析、验证、SQL 或进程失败全部 rollback，旧 published build 保持可读。孤立 `building` 不可被查询加载，下次 build 标记 failed 或清理。

### D3：意图边独立持久化

保留 `qug_edges` 的 page FK 语义；新增 `qug_intent_edges`，并以 `qug_builds` 作为代次父表。`qug_edges` 增加 `build_id`，新写入必须非空。查询只读取 active published build。

理由：哨兵 page 会污染 pages 生命周期、generation 和级联删除；完全不持久化则每次启动解析 YAML。独立表是可审计和可靠重建的最小方案。需要 0004 migration。

### D4：seed 与编译页衔接

构图输入是指定 domain 下所有 accepted 页面：legacy seed 与 accepted compiled 页并集。相同 page_id 只取 accepted head；不同 page_id 均保留。candidate、quarantined、deleted、孤立旧 generation 不参与。QUG build 不修改 pages、FTS、generation，也不改变现有 34 条 golden。

### D5：Golden 文件与配额

继续使用单文件 `examples/milk-tea/golden-queries.jsonl`，保留现有 34 条原文和期望，新增记录使总数至少 100。最低配额：`synonym` 25、`intent` 20、`negation` 20、`attribute_filter` 20、`negative` 15。单文件便于现有 loader、版本审查和总数校验；分类字段承担分层统计。备选拆文件不采用。

每行至少含：`id`、`query`、`kind`、`expected_entity_ids`；可含 `must_exclude_entity_ids`、`filters`、`notes`。正例 expected 集合由人工依据当前 accepted 页面和事实平面确认；negative 可为空且必须有排除/零命中语义。禁止用评测结果反向标注；同一规范化 query 最多出现两次。

### D6：三档指标与退出条件

A 为纯 FTS5 BM25；B 为 Step3 FTS+Vector+RRF；C 为启用 QUG 后执行 rewrite、过滤下推和同一混合检索，QUG 无匹配显式 fallback。A/B/C 使用同一数据库快照、accepted generation、facts、向量输入、排序 tie-break。

固定报告 `recall@1`、`recall@5`、`recall@10`。正例 recall 定义为 `|top_k ∩ expected| / |expected|` 的 macro mean；negative 单独报告 `negative_precision`，结果不得包含 `must_exclude`，空期望集的 negative 不计入正例 recall。主判定为 C 相对 B 的 recall@10：`gain_pp=(C-B)*100`，未四舍五入判定。C 的 active QUG 样本不足 1 条也视为无增益。`gain_pp < 5.0` 写 `qug_decision=disabled`；`>=5.0` 写 `enabled`。disabled 是合格交付，评测命令仍返回 0；C 的正例 recall 或 negative precision 低于 B 必须在报告中突出展示，但不把合格关闭改判为运行失败。

### D7：CLI 与退出码

`wiktor qug build --domain <domain.yaml> --db <path> [--force] [--dry-run] [--json]`。默认 hash 命中复用；force 忽略 hash；dry-run 只解析、筛选、计算 hash 和计数，不写库。成功输出 build_id/source_hash/页面数/五类边计数/复用或重建状态。

`wiktor eval --domain <domain.yaml> --db <path> --golden <path> --out-dir <dir> [--top-k <n>] [--json] [--no-qdrant]`。固定计算 @1/@5/@10；top-k 默认 10，范围 10..100；`--no-qdrant` 使用已有 deterministic/mock VectorStore。生成中文报告、英文报告和 JSON 结果。

退出码固定并覆盖 build/eval：0 成功（包括 disabled）；1 运行失败；2 CLI 用法错误；3 配置、迁移、golden 或边协议校验错误；4 未分类内部错误。不得把 QUG 无收益当作 1。

## 4. 架构与接口契约

### 4.1 模块边界

```text
crates/wiktor-core/src/query_engine/qug.rs  # 纯边提取、规范化、图构造
crates/wiktor-core/src/kernel/qug_store.rs  # hash、事务发布、加载
crates/wiktor-core/src/eval/                # golden loader、A/B/C、指标、报告
crates/wiktor-cli/src/commands/qug.rs       # qug build
crates/wiktor-cli/src/commands/eval.rs      # eval
```

query_engine 不持有可变连接；kernel 使用现有 Diesel raw SQL + bind；CLI 负责参数、领域包加载和格式化。复用 petgraph、diesel、serde_yaml_ng、serde_json、blake3、gray_matter 及既有 VectorStore，不引入新的重依赖。

### 4.2 类型契约

```rust
pub const QUG_BUILDER_VERSION: &str = "qug-build-v1";

pub struct QugPageInput {
    pub page_id: String,
    pub generation: i64,
    pub content_hash: String,
    pub artifact_version: String,
    pub frontmatter_json: String,
}

pub struct QugSourceSnapshot {
    pub domain: String,
    pub domain_version: String,
    pub qug_config_json: String,
    pub intents_bytes: Vec<u8>,
    pub pages: Vec<QugPageInput>,
}

pub struct PersistedPageEdge {
    pub page_id: String, pub edge: QugEdge, pub edge_hash: String,
    pub generation: i64, pub content_hash: String,
}
pub struct PersistedIntentEdge { pub edge: QugEdge, pub edge_hash: String }

pub struct QugBuildStats {
    pub build_id: i64, pub reused: bool, pub source_hash: String,
    pub accepted_page_count: usize, pub edge_count: usize,
    pub by_type: BTreeMap<String, usize>,
}

pub enum QugBuildOutcome { Reused(QugBuildStats), Published(QugBuildStats) }

pub trait QugStore: Send + Sync {
    fn active_source_hash(&self, domain: &str, version: &str) -> Result<Option<String>>;
    fn publish_build(&self, snapshot: &QugSourceSnapshot,
        page_edges: &[PersistedPageEdge], intent_edges: &[PersistedIntentEdge])
        -> Result<QugBuildStats>;
    fn load_active_edges(&self, domain: &str, version: &str) -> Result<Vec<QugEdge>>;
}

pub fn build_and_publish_qug(store: &dyn QugStore,
    input: QugBuildInput<'_>, force: bool) -> Result<QugBuildOutcome>;
pub fn load_active_qug(store: &dyn QugStore,
    domain: &DomainConfig) -> Result<Option<BuiltQug>>;
```

空 accepted 页面允许构建，配置边仍可发布；非法 frontmatter、空/超长 phrase、未白名单 field、无法序列化的 edge 返回 Validation；数据库错误返回存储/内部错误。任何错误都不返回部分图。

### 4.3 发布与加载补充约束

`qug_page_snapshots` 是 active loader 的唯一页面边来源；`qug_edges` 保留为 Step4 可替换的发布载荷，在 QUG build 事务内按本 domain 页删除重写镜像。Step4 发布的新载荷允许 build_id=NULL，不代表有效完整图。发布前在 BEGIN IMMEDIATE 内重新读取整个 domain 的 accepted 页面清单，与输入 snapshot 比较；变化则 rollback，返回 `source_changed`（exit 1），不得旧快照覆盖新来源。图提取在锁外执行。force 新建相同 hash 的代次，故 hash 不加 UNIQUE。并发 build 在写事务内再次检查 active hash，非 force 可复用另一 builder 的结果。

启动加载必须在同一读取事务验证当前页面清单、边计数和 payload hash，输入冻结的 domain/intents bytes；比较 source_hash 后才构图。保留 Step3 `QugBuildInput` 兼容，用新函数接收 `QugSourceSnapshot`、config、intents，而不是从 CompiledPage 猜 generation。错误的 persisted JSON 是内部错误（exit 4）；普通查询可以显式 fallback 并记录原因，eval 必须失败，不能把缺图当有效 C。

### 4.4 运行时加载

启动或 domain reload 时读取 active published build 的两类 edge_json，按 edge_hash 稳定排序，解码后调用 `QugGraph::from_edges`，再包进 `Arc`。active 缺失、hash 不一致或图损坏时设置 `qug=None`，诊断为 stale/disabled，走混合 fallback；查询线程不解析 YAML、不加写锁。build 成功后的 reload 由调用方显式触发。

## 5. 0004 migration DDL 草案

文件：`crates/wiktor-core/migrations/0004_qug_persistence/up.sql`；down 必须先检查不存在 Step5 数据再删除新结构，避免破坏性丢失。

```sql
CREATE TABLE qug_builds (
  build_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain_name TEXT NOT NULL,
  domain_version TEXT NOT NULL,
  builder_version TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('building','published','superseded','failed')),
  page_count INTEGER NOT NULL DEFAULT 0,
  edge_count INTEGER NOT NULL DEFAULT 0,
  counts_json TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  published_at INTEGER
);
CREATE UNIQUE INDEX uq_qug_active
  ON qug_builds(domain_name, domain_version) WHERE status='published';

ALTER TABLE qug_edges ADD COLUMN build_id INTEGER
  REFERENCES qug_builds(build_id) ON DELETE CASCADE;
CREATE INDEX idx_qug_edges_build ON qug_edges(build_id, page_id);

-- 完整代次副本；避免原 (page_id,edge_hash) 主键阻止跨代保留。
CREATE TABLE qug_page_snapshots (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  page_id TEXT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  generation INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  PRIMARY KEY(build_id,page_id,edge_hash)
);

CREATE TABLE qug_intent_edges (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  PRIMARY KEY(build_id, edge_hash)
);
CREATE INDEX idx_qug_intent_edges_build ON qug_intent_edges(build_id);
```

旧 0003 `qug_edges` 行的 build_id 可为空但不得被 active loader 读取；首次 Step5 build 全量重建。新行必须带 build_id。事务内先写 building 和边，校验后将旧 published 标 superseded、将新行 published；读者看到事务前或事务后状态。

## 6. 评测报告契约

报告目录由 `--out-dir` 指定：`step5-qug-evaluation.md`、`step5-qug-evaluation.en.md`、`step5-qug-evaluation.json`。双语报告逐节对应，数字、query id、decision 和 dataset hash 相同；“啵啵”“珍珠”等领域字面量保留中文。

JSON 至少包含 `schema_version`、domain/version、dataset_hash、source_hash、A/B/C 的 @1/@5/@10、negative_precision、按 kind 分层结果、fallback_count、失败样本、`qug_decision`、`reason`、vector_backend、运行命令。单条 query error 不吞掉，汇总为运行失败；数据协议错误为退出码 3。

## 7. 验收标准 A1–A14

- **A1**：同一输入二次 build 命中 source_hash，`reused=true`，不产生新的 active 边代次。
- **A2**：页面 content_hash/frontmatter/generation、intents bytes、domain version/config 或 builder version 任一变化都会产生新 hash并重建。
- **A3**：fixture 可生成五类边各至少一条，计数、canonical JSON、反序列化一致，重复边去重稳定。
- **A4**：仅 accepted seed 与 accepted compiled 页参与；candidate/quarantined/deleted/孤立旧 generation 不产生或加载边。
- **A5**：意图边独立表无 page_id；页面边保留 pages FK；删除 page 不删除意图边。
- **A6**：任一写入、计数或 publish 失败均 rollback，旧 active 图可加载且无半套新边。
- **A7**：启动 hash 一致加载图；不一致、无 active 或图损坏时不读旧图临时顶替，走显式 fallback。
- **A8**：seed 与 compiled accepted 页共存，Step4 空 `CompiledPage.qug_edges` 不阻断；现有 34 条 golden 内容与期望不变。
- **A9**：单文件 golden 总数≥100，配额至少 25/20/20/20/15；loader 拒绝重复 id、未知 kind、虚假 entity、非法 JSON和不足配额。
- **A10**：A/B/C 在相同 snapshot 输出 recall@1/@5/@10、negative_precision、分层结果和 fallback；连续运行结果一致。
- **A11**：C-B recall@10 <5.0pp 时 `qug_decision=disabled`、exit 0；达到阈值时 enabled、exit 0。
- **A12**：C 的 recall@10 低于 B 时报告回退及 disabled、exit 0；运行或数据错误不得伪装为 disabled。
- **A13**：build/eval 支持 hash 复用、force、dry-run、json、out-dir；报告生成中英文 Markdown 和 JSON。
- **A14**：退出码准确为 0/1/2/3/4；不引入重依赖；`cargo check --workspace`、现有 QUG/compile 测试及离线评测可通过。

## 8. 给 wiktor-builder 的实现批次

1. **纯函数批次**：抽取 Step3 边来源、规范化、canonical JSON、edge hash、source snapshot/hash；内存 fixture 可编译并验证 A1–A3。
2. **存储批次**：加入 0004、Diesel schema/raw bind、active 查询、事务发布和 rollback；SQLite smoke 覆盖 A5–A6。
3. **运行时批次**：实现 active load、hash 校验、Arc 图注入、stale/disabled/fallback；验证 A4、A7、A8。
4. **Golden 批次**：保留旧文件记录并补足到 ≥100，实现配额、schema、文件 hash 和实体存在性校验；验证 A9。
5. **评测批次**：实现 deterministic/mock A/B/C、@1/@5/@10、negative、安全回退、decision 和报告模型；验证 A10–A12。
6. **CLI 批次**：接入 `qug build`/`eval`、参数、JSON、人类输出、out-dir 和 0/1/2/3/4；验证 A13–A14。

每批必须独立可编译、可测试、可回滚；数据库、hash、配置和协议错误不得转成“QUG disabled”。

## 9. 风险、取舍与偏差记录

全量快照重建优先于复杂页级增量，避免删除边和意图全局规则产生混合代次；未来规模扩大可按 page hash 分片。Mock vector 只证明路径可复现，不证明生产语义质量。意图文件任何变化都会使全图失效，因为局部发布会破坏 source_hash 一致性。边数、alias/tag 数、短语长度和评测 top-k 都必须有硬上限，超限报错而不截断。

| ID | 计划/现状偏差 | 原因 | 影响 | 补偿措施 | 是否需回写 MASTER-PLAN |
|---|---|---|---|---|---|
| STEP5-001 | spec §4.2 `QugPageInput` 未指明 title 来源，而 Synonym/Hyponym 需要 title；Step4 `build_frontmatter_json` 原只写 aliases/tags/refs/quality_policy，legacy seed 页 frontmatter 为 `'{}'` | spec 缺口（批1 实现时发现） | 无 API 变化；`QugPageInput` 结构不变 | 拍板：DB 的 `pages.frontmatter_json` 统一承载 `title/aliases/tags`（batch 1 `page_frontmatter_json` 为规范写法，含 title 必填）。seed 路径与 Step4 `build_frontmatter_json` 均写入 title；snapshot 组装只读 DB 不回读 md 文件；legacy `'{}'` 页零边不阻断（重跑 seed 或重编译后恢复供边）。D2 五元组不变（frontmatter_json 含 title 已被 hash 覆盖） | 否 |
| STEP5-002 | §9 要求的硬上限未给数值，批1 取防御性整数：页 10 万、frontmatter 列表 64、intents bytes 1MiB、intent entries 1000、phrases 64、边 10 万 | spec 只要求"有上限" | 数值为实现细节 | 超限一律 Validation 报错不截断；后续可在 config 化时调整 | 否 |
| STEP5-003 | D5"同一规范化 query 最多出现两次"按纯文本去重会误拒既有记录（"奶茶"以 4 种 filters 上下文出现） | spec 未定义规范化口径 | 去重键收紧为 `(normalized_query, filter_signature)` ≤2 次 | 已在 eval 模块文档声明；配额 25/20/20/20/15 以新增 100 条满足（总数 134，legacy 34 条不计配额、kind=Legacy） | 否 |

实现者发现与本 spec 不一致时必须追加记录，不得静默改动 D1–D7、DDL、退出条件或验收口径。
