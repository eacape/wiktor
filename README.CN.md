<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/brand/wiktor-web-dark.svg">
  <img src="docs/brand/wiktor-web-light.svg" alt="Wiktor" width="280">
</picture>

<h1>Wiktor</h1>

<h3>编译型知识检索数据库 — 检索体验对标 Meilisearch，可靠性对标 etcd</h3>

<p>
  <img src="https://img.shields.io/badge/Rust-1.85%2B-dea584?style=flat&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/storage-SQLite%20%2B%20FTS5-003B57?style=flat&logo=sqlite&logoColor=white" alt="Storage">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-green?style=flat" alt="License"></a>
  <a href="https://github.com/eacape/wiktor/actions"><img src="https://img.shields.io/github/actions/workflow/status/eacape/wiktor/ci.yml?branch=main&style=flat&label=CI" alt="CI"></a>
  <a href="https://crates.io/crates/wiktor-core"><img src="https://img.shields.io/crates/v/wiktor-core?style=flat" alt="crates.io"></a>
  <a href="https://docs.rs/wiktor-core"><img src="https://img.shields.io/docsrs/wiktor-core?style=flat" alt="docs.rs"></a>
</p>

**中文** | [English](README.md)

</div>

---

Wiktor 是一个"编译型"检索数据库：把非结构化知识源编译成带质量门禁的 Wiki 页面 + 事实平面（双平面存储），在单机 SQLite 之上提供 QUG 查询理解、混合检索（FTS5 + 向量 + RRF）、过滤下推、反馈闭环与 gRPC/HTTP 服务。`docs/` 下文档双语（`*.md` 中文为权威、`*.en.md` 为英文对应），本文件与英文版 [README.md](README.md) 内容一致。

## 核心特性

- **双平面数据模型**：知识平面（可编译 Wiki 页）+ 事实平面（结构化、可过滤），同一实体双写。
- **编译管线**：LLM 把数据源编译为页面；`require_source_refs` 引用契约 + 四规则质量评分 + 页级/任务级重编译刹车 + BLAKE3 全依赖内容哈希（增量跳过）。
- **QUG 查询理解图**：五类边（同义/意图/否定/属性过滤/反向）离线构建、单事务原子发布、代次审计；增益 <5pp 自动退出为 disabled，显式 fallback 混合检索。
- **混合检索**：QUG 改写（可选）→ 事实平面过滤下推 → FTS5 + 向量两路召回 → RRF 融合；每查询返回分路诊断（QueryDiagnostics）。
- **反馈闭环**：`POST /feedback`（认证/限流/幂等）→ 标准分析器浮现零召回盲点 → 补编译任务 → 召回提升（集成测试实证 recall@1 0→1）。
- **可靠性契约**：任务状态机（租约/幂等/死信/兼容检查）、发布状态机（quarantine 不入索引）、litestream 持续 WAL 复制单机 HA。
- **插件生态**：向量后端（qdrant）与检索出口（Meilisearch）都是独立插件 crate，core 零感知；领域包（domain pack）是第三插件点。

## 快速开始

```bash
# 安装 CLI（crates.io；如需 server/console 面加 --features server,console）
cargo install wiktor-cli
# 或从源码构建：
cargo build -p wiktor-cli

# 1) 从领域包建库 + 灌数据（两个官方领域包任选）
./target/debug/wiktor seed --db ./wiktor.db --domain examples/tech-docs/domain.yaml

# 2) 编译（MockCompiler 离线可跑；真实 provider 需 llm-openai + key）
./target/debug/wiktor compile --db ./wiktor.db --domain examples/tech-docs/domain.yaml --provider mock

# 3) 检索（混合 + 分路诊断）
./target/debug/wiktor search --db ./wiktor.db "retrieval pipeline" --json

# 4) 常驻服务（gRPC 六服务 + HTTP /search /health /metrics /feedback）
cargo build -p wiktor-cli --features server
WIKTOR_API_KEYS='{"tech-docs":{"secret":"...","methods":["Search","Feedback"]}}' \
./target/debug/wiktor serve --db ./wiktor.db --listen-http 127.0.0.1:8080 --listen-grpc 127.0.0.1:50051 --domain examples/tech-docs/domain.yaml

# 5) Web console（本地只读监督界面）
cargo build -p wiktor-cli --features console
./target/debug/wiktor console --db ./wiktor.db --listen 127.0.0.1:8081
# 浏览器打开 http://127.0.0.1:8081/ ；JSON API 见 docs/console_ui/README.md
```

## Crate 结构

| Crate | 职责 |
|---|---|
| `wiktor-core` | 两平面 SQLite 内核（diesel + FTS5）、QueryEngine、QUG、编译管线、一致性/兼容仲裁、Mock 向量基线 |
| `wiktor-cli` | `wiktor` 单二进制：seed/compile/search/qug/eval/feedback/domain/export/status，可选装配 server/console |
| `wiktor-feedback` | 反馈存储 trait + 标准分析器（零召回/低质召回/改写失败三信号） |
| `wiktor-server` | gRPC（tonic 六服务）+ HTTP（axum）统一服务面，方法级 API key |
| `wiktor-vector-qdrant` | qdrant 向量后端插件 |
| `wiktor-adapter-meilisearch` | Meilisearch 检索出口插件 |
| `wiktor-console` | Web console（本地 HTTP 只读监督界面，Obsidian 视觉原型） |

Feature 边界：默认构建不携带 TUI/console/server/Meilisearch；外部服务（qdrant、Meilisearch、litestream）全部可选。

## 文档导航

- [`docs/MASTER-PLAN.md`](docs/MASTER-PLAN.md) — 总体规划（v3.2）：架构、铁律、决策、落地依赖
- [`docs/PLAN.md`](docs/PLAN.md) — 分阶段执行计划与验收标准
- [`docs/design/`](docs/design/) — Step 1–14 设计 spec + 实现偏差（双语）
- [`docs/domain-pack-guide.md`](docs/domain-pack-guide.md) — 领域包贡献指南（第三插件点）
- [`docs/console_ui/`](docs/console_ui/) — Web console 设计系统与真实数据接入
- [`docs/brand/`](docs/brand/) — 品牌资产（上方蛛网 logo）
- [`CONTRIBUTING.md`](CONTRIBUTING.md) 与 [`CHANGELOG.md`](CHANGELOG.md) — 贡献指南与按版本变更日志
- [`deploy/README.md`](deploy/README.md) — 生产部署 runbook（安装、运维、备份/恢复、回滚）
- [`examples/milk-tea/`](examples/milk-tea/)、[`examples/tech-docs/`](examples/tech-docs/) — 两个官方领域包（种子 Wiki + 事实平面 + golden 评测集）

## 对标与定位

Wiktor 的工程基线是"检索体验对标 Meilisearch、可靠性对标 etcd"，内核沿袭 rqlite 包装 SQLite 的思路（不重复造存储/BM25 轮子）。下表把它放在相近生态中定位；"开源"列是诚实的许可证形态，"编译可观测性"列反映其有无质量门禁的编译管线被显式暴露。

| 项目 | 是什么 | 编译可观测性 | 插件生态 | 开源 | 检索 / 侧重 |
|---|---|---|---|---|---|
| **Wiktor** | 编译型知识检索数据库：LLM 把数据源编译成带质量门禁的 Wiki 页 + 事实平面，运行于单机 SQLite | 一等公民：每查询 `QueryDiagnostics`、五维质量评分、quarantine 状态机、反馈闭环随每个盲点出具报告 | 三点：数据源适配器 / 领域包 / 向量后端（+检索出口插件） | Apache-2.0 全开源（core + 全部插件） | 混合 FTS5 + 向量 + QUG；闭环实证 recall@1 0→1 |
| **Meilisearch** | 即时、容错拼写的检索引擎（Rust） | 有限：相关度调参旋钮，无编译/质量模型 | 插件与引擎集成 | MIT 开源 | 快速全文本"即打即搜"；无 LLM 编译、无双平面 |
| **qdrant**（作向量后端） | 专用向量数据库 | 仅向量层（collection/point/payload） | 广泛客户端生态 | Apache-2.0 开源 | ANN 相似度；无知识编译或检索编排 |
| **Vectara** | 托管 RAG：索引 + 检索 + 带引用的摘要 | 托管控制台、API 指标 | 仅 SaaS API | 闭源（托管） | 托管 RAG 带引用溯源；不可自托管 OSS |
| **Pinecone** | 托管向量数据库 | 向量/用量指标，无编译模型 | SDK + 集成 | 闭源（托管） | 托管 ANN 相似度；无编译管线或事实平面 |
| **Weaviate** | 向量数据库（开源 + 托管） | 向量 schema + 模块 | 模块（向量化、混合） | BSD-3 开源核心 | 向量 + 混合；无 LLM 知识编译阶段 |
| **LangChain** | LLM 应用编排框架（非数据库） | 应用层；检索自行编排 | 集成面极大 | MIT 开源 | 编排 LLM + 检索器；本身无质量门或持久化引擎 |
| **WeKnora** | RAG 中间件/平台（腾讯） | 有一定管线/指标 | 近似插件 | 部分开源 | 混合检索 + 重排，服务于 RAG；中间件栈较重 |
| **rqlite**（作内核类比） | 分布式 SQLite（Raft） | 仅 DB 层 | 无需 | MIT 开源 | 分布式 SQLite；无检索/编译层 |

结论：Wiktor **不是又一个向量库**。它的定位是"编译 + 质量门 + 反馈可观测闭环"的 LLM 知识编译层，位于存储与检索**之前**——一个"编译型检索数据库"，而非相似度索引。向量后端（qdrant）与检索出口（Meilisearch）是插件面而非产品身份。架构与设计决策详见 [`docs/MASTER-PLAN.md`](docs/MASTER-PLAN.md)。

## 状态

Step 1–14 全部交付（2026-09-27）：schema/内核 → 查询闭环 → 编译管线 → QUG → 反馈闭环 → gRPC/HTTP 服务 → 一致性状态机 → litestream HA → 插件生态 + 第二领域包 → 性能实证 + Web console → 反馈语义匹配抽象 → 单进程多领域 serve + 展示层收敛 → 运维/开源门面。真实后端效果实验（qwen 编译 + qwen 嵌入 + qdrant）确认混合检索 recall@10 从 0.446（纯 FTS）提升到 0.889（混合）、0.962（含 QUG）。

测试：`cargo test --workspace`（417 全绿）；基准：`cargo bench -p wiktor-core --bench query_bench`。

## 路线图

- **生产加固**：多节点 HA（litestream 复制 → Raft）与集群分片（见 `docs/design/step10-cluster-sharding.md`）。
- **真实基准发布**：在官方领域包上发布可复现的评测基准。
- **社区与插件**：第三方插件走查后，收口发布管线（GitHub Releases + 预编译二进制 + crates.io 发布）。
- **Feature 化生态**：TUI/console/server/Meilisearch 已 feature 化；其余可选向量后端（lancedb / pgvector / …）继续藏在 `VectorStore` trait 之后。

始终以"可观测、单机优先"为原则推进：每次发布都能用 SQLite + 可选 qdrant 自托管。

## 寻求帮助

- [问题追踪](https://github.com/eacape/wiktor/issues) — bug、功能请求与设计讨论。
- [贡献指南](CONTRIBUTING.md) — 如何构建、测试并提交 PR。
- [行为准则](CODE_OF_CONDUCT.md) 与 [安全政策](SECURITY.md) — 预期与负责任披露流程。
- [变更日志](CHANGELOG.md) — 每次发布的重要变更。

## License

基于 [Apache-2.0](LICENSE) 开源——可免费使用、修改与分发，包括商用产品。
