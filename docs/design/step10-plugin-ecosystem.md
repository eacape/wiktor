# Step 10 设计规范：插件生态、第二官方领域包与集群分片规划

> 版本：v1.0（2026-09-24）
> 权威依据：`docs/MASTER-PLAN.md` v3.2 §5.6、§9、§12 决策 #8、§十四 已决 #1/#2、§十七 落地依赖 #10
> 实现对象：`wiktor-builder`（走量）；设计级（集群分片）由主模型成文
> 中文为权威设计；英文逐节对应于 `step10-plugin-ecosystem.en.md`；集群分片另见 `step10-cluster-sharding(.en).md`。

## 1. 目标与非目标

本 Step 把落地依赖 #10"集群分片 + 插件生态（Meilisearch 出口、外部向量库）+ 第二官方领域包（技术文档）"拆为四项子交付。

目标：
- **第二官方领域包（技术文档）**：新增 `examples/tech-docs/`，证明领域包插件点对非电商域可扩展。
- **golden 过滤器泛化**：让 `GoldenFilters` 不再硬编码 milk-tea 字段（price/sugar/size/ingredients），改为领域通用的事实字段过滤表达，机械迁移 milk-tea 的 134 条 golden 并保持断言全绿。
- **外部向量库拆分 crate**：把 `QdrantVectorStore` 从 core 拆出为独立插件 `wiktor-vector-qdrant`，验证"插件只依赖 core、不依赖 server"与"接入成本 < 半天"。
- **Meilisearch 出口**：新增可选插件 `wiktor-adapter-meilisearch` + CLI `wiktor export meilisearch`，把 accepted 页镜像到外部检索引擎。
- **集群分片规划**：只做设计文档（shard key / 路由 / 重平衡 / 跨片一致性 / 与远期 rqlite-Raft 衔接），不实现。

非目标：
- 不做真实集群分片/跨机 shard（2 GiB 单机 + Raft 明确远期）。
- 不把 Meilisearch 接入默认查询路径（外部引擎是可选插件，不是产品身份）。
- 不预建空壳插件 crate；只拆出当前真实使用的 qdrant 后端。
- 不改变单写库、CAS、内容哈希、QUG、发布事务等既有契约。

## 2. 现状约束与术语

- workspace 现 4 crate：`wiktor-core/cli/feedback/server`；无插件 crate。
- `VectorStore` trait（`traits/vector_store.rs`）是**泛型 `QueryEngine<V>`** 而非 `Box<dyn>`；`QdrantVectorStore`（core 内 feature `vector-qdrant`）+ `MockVectorStore`（常开）。
- embedding 缝是 `Arc<dyn QueryEmbedder>`（object-safe），干净可换。
- `kernel::accepted_page_vectors(domain)` 返回 accepted 页（page_id/entity_id/title/content/content_hash/generation），是外部搜索导出的现成读面。
- `GoldenFilters`（`eval/mod.rs:160`）是 milk-tea 硬编码扁平结构；`golden_filters_to_filters`（`eval/runner.rs:116`）映射到领域无关的 `FilterCondition`（`types/mod.rs:82`：NumericRange/TextEquals/RefContains/RefExcludes）。
- 领域包 = `examples/milk-tea/`（无 `domains/` 目录，CLI `--domain <path>` 显式指）。
- 网络：本机 GitHub 直连被墙，GitHub 上传由 Linux SSH remote push；下载走 Clash 代理 127.0.0.1:7897。

## 3. 决策 D1–D8

| ID | 决策 | 理由与边界 | 批次 | 验收 |
|---|---|---|---|---|
| D1 | 第二领域包放 `examples/tech-docs/`，与 milk-tea 并列，复用同一 `domain.yaml` 契约与 seed/eval 路径。 | 领域包是"目录 + domain.yaml + 数据"，无需新增注册机制。 | B1 | A1 |
| D2 | tech-docs 事实平面 = 文档（docs-as-code 条目）：`topic`（主知识页）/`level`/`format`/`audience_years`/`tags` filterable，与 milk-tea 的 price/sugar/size/ingredient 完全不同。 | 证明 golden 过滤泛化的必要性。 | B1/B2 | A1、A3 |
| D3 | `GoldenFilters` 由 milk-tea 硬编码结构改为**领域通用过滤表达**，与 `FilterCondition` 同构（tagged 枚举，按事实字段名表达），`golden_filters_to_filters` 退化为逐条映射；milk-tea 134 条 golden 用脚本机械迁移并保持 134+配额+34 legacy 断言。 | 移除字段硬编码是第二领域能真正跑 eval 的前提；同构让翻译零心智开销。 | B2 | A3、A4 |
| D4 | 只把 qdrant（外部向量库）拆出为 `wiktor-vector-qdrant` 插件 crate；`MockVectorStore` 留 core 作评测基线（符合 §394"内存暴力扫描先作 core 模块、外部向量库再拆 crate"）。 | 把对 391 测试的改动风险压到最小，同时完整证明插件模式。 | B3 | A5 |
| D5 | 插件 crate 只依赖 core（+ 各自传输依赖），不依赖 server/feedback；CLI 是装配者，负责依赖插件。 | MASTER-PLAN §8 依赖规则。 | B3/B4 | A5、A6 |
| D6 | Meilisearch 出口为**导出/同步**（不是默认搜索）：`wiktor export meilisearch` 读 accepted 页 → PATCH 到 `${WIKTOR_MEILISEARCH_URL}/indexes/{domain}/documents`（env 配置、可选 api key）。 | 外部检索引擎是可选插件，不与默认路径混用（§154）。 | B4 | A6 |
| D7 | 集群分片只产出设计文档（`step10-cluster-sharding(.en).md`），不建 crate/不写实现。 | 单机硬件 + Raft 远期，真实分片不可落地。 | B5 | A7 |
| D8 | 新增特性均为**可选**（CLI feature 转发），不改变 `wiktor-core` 默认 feature 的默认查询路径；任何失败不得影响既有 391 测试。 | 插件可插拔、默认路径稳定。 | 全批 | A2–A6 回归 |

## 4. 批次实现

### B1 —— 第二官方领域包 `examples/tech-docs/`

新增目录，内容：
- `domain.yaml`：`name: tech-docs`；实体 `document`，事实字段 `title/description/topic(reflist)/level/text/format/text/audience_years/numeric/tags(reflist)`，filterable = topic/level/format/audience_years/tags；`query.filters` 对应；compile 0.75/2；qug enabled 指向 `intents.yaml`。
- `seed-wiki/*.md`：约 20 页技术文档知识页（concept/technology/practice），frontmatter 契约 `tech-docs:<type>:<id>`（title/aliases/tags/entity_type/entity_id/page_id），正文 `##` 分节。
- `gen_docs.py` + `docs.jsonl`：确定性生成约 120 条文档事实记录（entity_id `tech-docs:document:doc_XXXX`）。
- `intents.yaml`：expansion/attribute/negation 三类，字段引用 tech-docs 事实字段。
- `golden-queries.jsonl`：约 134 条（34 legacy + 100 new：synonym25/intent20/negation20/attribute_filter20/negative15），由 `gen_golden.py` 确定性生成并命中配额。
- `README.md`（可选）：tech-docs 领域包说明。

验收（A1）：`wiktor seed --domain examples/tech-docs/domain.yaml --db <tmp>` 成功；`wiktor eval --golden examples/tech-docs/golden-queries.jsonl --domain .../domain.yaml` 跑绿（A/B/C 三档与 QUG 决策可执行）。

### B2 —— golden 过滤器泛化（core eval 改动）

- 新增 `GoldenFilterCondition`（tagged enum，`#[serde(tag="type", rename_all="snake_case")]`）变体与 `FilterCondition` 对齐：`NumericRange{field,min,max}`/`TextEquals{field,value}`/`RefContains{field,refs}`/`RefExcludes{field,refs}`；`GoldenFilters { conditions: Vec<GoldenFilterCondition> }`。
- `golden_filters_to_filters` 改为逐条映射 `GoldenFilterCondition → FilterCondition`（字段名直接透传，去掉 price→price / sugar→sugar_level / ingredients→ingredient_ids 的硬编码映射）。
- `filter_signature` 同步改用通用表达。
- 迁移脚本：把 milk-tea 134 条 golden 的 `filters` 从扁平键改写为通用表达（price_min/price_max→`{"type":"numeric_range","field":"price",...}`；size→text_equals；ingredients→ref_contains；exclude_ingredients→ref_excludes；`{}`→`[]`）。保持 134 总数、34 legacy、各 kind 配额不变。
- 新增 tech-docs eval 集成测试（复用既有 golden 评测 harness，覆盖 tech-docs 领域 + 通用过滤器）。

验收（A3/A4）：workspace `cargo test` 全绿；`eval.rs` 的 134/配额/legacy 断言仍成立；tech-docs eval 绿。

### B3 —— qdrant 向量后端拆成 `crates/wiktor-vector-qdrant`

- 新 crate `crates/wiktor-vector-qdrant`（feature `server`? 不——只依赖 core + qdrant-client）：迁移 `QdrantVectorStore`、`point_id`、`collection_name`、`from_config`/`connect`。
- core 移除：`vector-qdrant` feature、qdrant-client 可选依赖、`kdrant_vector` 模块及其 re-export；`tests/qdrant_integration.rs` 迁到新 crate。
- CLI：`wiktor-cli` 依赖 `wiktor-vector-qdrant`（feature 转发，`default` 不强制），`cmd_vector_build` 改用插件类型；`vector-qdrant` 转为 CLI feature 转发到插件。
- workspace 根把 qdrant-client 依赖归位到插件 crate。

验收（A5）：workspace 构建/测试全绿；新 crate 独立 `cargo build` 通过且 Cargo.toml 只依赖 core；`wiktor vector build` 在 mock/真实 qdrant 均可用。

### B4 —— Meilisearch 出口 `crates/wiktor-adapter-meilisearch`

- 新 crate 依赖 core + reqwest（复用 rustls 栈，不加新 HTTP crate）：`MeilisearchExporter` 消费 `kernel::accepted_page_vectors` 构造 documents，`PATCH {base}/indexes/{domain_or_index}/documents`；env `WIKTOR_MEILISEARCH_URL`（默认 http://localhost:7700）、`WIKTOR_MEILISEARCH_API_KEY`（可选）。
- CLI 子命令 `wiktor export meilisearch --db <db> --domain <path>`（feature `export-meilisearch`）；export 前先建/确保 index（`PUT /indexes/{name}`）。
- 测试：本地 mock HTTP server（axum 或阻塞 tcp）捕获 PATCH，断言文档体含 page_id/title/content/entity_id/generation，且仅 accepted 页导出。

验收（A6）：`wiktor export meilisearch` 将 accepted 页镜像到 mock Meilisearch；未过审（quarantine）页不导出；接入成本路径（env + 子命令）独立可验证。

### B5 —— 集群分片规划（仅设计文档）

`docs/design/step10-cluster-sharding(.en).md`：
- 分片键选择（domain/entity_id 哈希 vs 范围）、路由层（server 现有 gRPC/HTTP 之上）、跨片一致性（单写库边界、两平面可重建）、重平衡（generation/epoch）、与远期 rqlite Raft（WAL 兼作 Raft log）的衔接、迁移路径、明确不实现的边界。
- 无代码、无新 crate。

验收（A7）：设计文档交付，MASTER-PLAN #10 标注"集群分片=设计规划"。

### B6 —— 收口、双语与推送

- MASTER-PLAN 中英 #10 标记 ✅（注明第二领域包/向量拆分/Meilisearch 实装 + 集群分片设计规划）。
- 新增/补全领域包贡献指南（"如何加第三个领域包"），可并入 README 或独立 `docs/domain-pack-guide(.en).md`。
- step10-plugin-ecosystem spec 偏差登记（STEP10-xxx，实现阶段回写）。
- 所有新增/改动文档中英同步；workspace `cargo test` + clippy + fmt 全绿；本机 commit → tar over SSH（排除 `._*`）→ Linux push → 本机 pull。

## 5. 实现偏差基准（预案）

实现发现与本节不同处追加 `STEP10-xxx`，说明原因、接口影响与验收变化；不得变更：golden 过滤器泛化的"字段名直传、不再硬编码"方向、插件只依赖 core、Meilisearch 为可选导出、集群分片不实现。

### 5.1 实际实现偏差（2026-09-24 登记）

| ID | 偏差 | 原因与处理 |
|---|---|---|
| STEP10-001 | `GoldenFilters` 序列化形态为 `{"conditions":[...]}` 而非 spec 初稿的裸数组 | 保持与既有结构体 serde 约定一致（`.conditions` 字段）；机械迁移脚本与 `gen_golden.py` 均产出该形态；空过滤用 `{}`。 |
| STEP10-002 | core `error.rs` 新增 `Error::External(String)` 变体 | Meilisearch 等外部出口需要类型化错误且不冒充 `VectorStore` 类错误；该非穷尽匹配变体顺带在 `wiktor-server` 两处与 `wiktor-feedback`/CLI 的 `classify_error` 触发穷尽 match 补 arm（映射为 503/UNAVAILABLE / run-failure）。 |
| STEP10-003 | core 默认 feature 由 `["vector-qdrant"]` 降为 `[]`，`vector-qdrant` 成为 CLI 的 feature 转发到插件 | qdrant 拆出后 core 不再携带 qdrant-client；装配责任归装配者（CLI/服务）——与"插件只依赖 core"一致；`wiktor vector ping/build` 与 `wiktor eval` 的 qdrant 路径均以 `feature vector-qdrant` 守卫，缺失时明确报错而非静默回退。 |

## 6. 验收标准 A1–A7

| # | 判据 | 可执行结果 |
|---|---|---|
| A1 | tech-docs 领域包可 seed + eval | `wiktor seed` + `wiktor eval` 跑绿 |
| A2 | 既有 milk-tea 链路回归 | 391 测试全绿；milk-tea 134/配额/legacy 断言不变 |
| A3 | golden 过滤器跨领域通用 | milk-tea 与 tech-docs 两种不同事实字段的 golden 都能跑 eval |
| A4 | 迁移无内容损失 | 迁移脚本幂等；134 条 golden diff 仅 filters 键形态变化 |
| A5 | qdrant 插件只依赖 core | 新 crate 独立构建；core/server 不再依赖 qdrant |
| A6 | Meilisearch 出口可用 | 导出命令把 accepted 页镜像到 mock Meilisearch；quarantine 不导出 |
| A7 | 集群分片规划文档交付 | step10-cluster-sharding(.en).md 存在，无代码改动 |

## 7. 风险与边界

| 风险 | 处理 |
|---|---|
| golden 泛化破坏 eval 断言 | 迁移脚本幂等 + 逐步跑 `cargo test`，先迁移后校验；失败即停不盲推进 |
| 拆 crate 破坏编译（qdrant_client 依赖散落） | 一次到位迁移 qdrant 相关全部符号与测试；`cargo check --all-features` 先跑 |
| tech-docs 数据量大/手工易错 | golden 与 facts 均由确定性生成脚本产出，复用 milk-tea 的 jsonl 生成范式 |
| Meilisearch 无真实实例 | mock HTTP server 验证协议，真实实例留给部署演练 |

<!-- END STEP10 SPEC v1.0 -->