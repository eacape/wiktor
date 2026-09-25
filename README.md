# Wiktor — 知识编译与检索中间件

> 一个"编译型"检索数据库：把非结构化知识源编译成带质量门禁的 Wiki 页面 + 事实平面（双平面存储），在单机 SQLite 之上提供 QUG 查询理解、混合检索（FTS5 + 向量 + RRF）、过滤下推、反馈闭环与 gRPC/HTTP 服务。体验对标 Meilisearch，可靠性对标 etcd。

中文文档为权威版本，英文逐节对应（`*.en.md`）。

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
# 构建 CLI（默认 feature 含 llm-openai/embedding-http/vector-qdrant；离线跑 mock 即可）
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
- [`docs/design/`](docs/design/) — Step 1–11 设计 spec + 实现偏差（双语）
- [`docs/domain-pack-guide.md`](docs/domain-pack-guide.md) — 领域包贡献指南（第三插件点）
- [`docs/console_ui/`](docs/console_ui/) — Web console 设计系统与真实数据接入
- [`examples/milk-tea/`](examples/milk-tea/)、[`examples/tech-docs/`](examples/tech-docs/) — 两个官方领域包（种子 Wiki + 事实平面 + golden 评测集）

## 状态

Step 1–11 全部交付（2026-09-25）：schema/内核 → 查询闭环 → 编译管线 → QUG → 反馈闭环 → gRPC/HTTP 服务 → 一致性状态机 → litestream HA → 插件生态 + 第二领域包 → 性能实证 + Web console。真实后端效果实验（qwen 编译 + qwen 嵌入 + qdrant）确认混合检索 recall@10 从 0.446（纯 FTS）提升到 0.889（混合）、0.962（含 QUG）。

测试：`cargo test --workspace`；基准：`cargo bench -p wiktor-core --bench query_bench`。

## License

MIT OR Apache-2.0
