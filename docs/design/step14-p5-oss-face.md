# Step 14 (P5) 设计规范：运维 / 开源门面收口

> 版本：v1.0（2026-09-27）
> 权威依据：`docs/MASTER-PLAN.md` §「开源冷启动难」风险对策、遗留事项「竞品对比表（写入 README 前完成）」
> 实现对象：README 开源首屏重构 + 竞品对标表 + 贡献基础设施 + crates.io 元数据 + 运维口径收口（主模型实现，本文档登记规范）

## 1. 背景与目标

Wiktor 的"运维/工程"底子已相当扎实自足：`deploy/` 含 install、systemd 双单元、litestream 备份/恢复、smoke、Prometheus/告警、bring-up runbook；CI（fmt/clippy/test）已跑；双语 MASTER-PLAN + 20 份 step spec。P5 的真正差距集中在**开源门面**四处 + **运维口径收口**一处，本 Step 一次交付。

五块目标：
1. **竞品/对标对比表**——勾掉 MASTER-PLAN §十四 遗留「竞品对比表（写入 README 前完成）」。
2. **README 开源首屏重构**——补齐标准开源 README 缺项（安装方式、issue/讨论入口、roadmap、CI/crates.io badge、license 细节），中英同步，并刷新过时 Status。
3. **贡献基础设施**——CONTRIBUTING / CODE_OF_CONDUCT / SECURITY / CHANGELOG + CI badge。
4. **crates.io 元数据**——workspace-package 补 description/homepage/documentation，各 crate description 补齐并英文化。
5. **运维一致性收口**——deploy README 补回滚/env 全表/观测指引，修正 metrics 口径偏差（6→8 个 family）。

## 2. 现状核实（设计前提）

- **README.md（英文主 + README.CN.md 镜像）**：结构 = 顶部 logo/tagline/3 badge + 简介 + Core features(7) + Quick start(5步) + Crate layout + Documentation map + Status + License。链路无死链。**Status 已过时**：写「Steps 1–11 … 397 green」，实为 **Step 1–14 全交付、workspace 400+ green**（HEAD=62510b9）。
- **缺**：Screencast/demo、`cargo install` 预编译二进制/Docker 安装段、Issues/讨论入口、竞品对比、roadmap、贡献指南入口、CI/crates.io/docs.rs badge、license 细节。
- **贡献面**：顶层无 CONTRIBUTING/CODE_OF_CONDUCT/SECURITY/CHANGELOG；`.github/` 仅 `workflows/ci.yml`。
- **crates.io 元数据**：workspace `[workspace.package]` 有 version/edition/rust-version/license/repository，**缺 description/homepage/documentation**；7 个 crate 里 4 个（core/cli/feedback/server）无 description，3 个 description 为中文（adapter/console/vector-qdrant）——crates.io 国际受众需英文。
- **运维**：`deploy/README.md` 已 cover Quick start/Ops/灾难恢复/Bring-up/升级，但缺回滚流程、完整 env 全表、Metrics/告警查看指引、`/health` 语义；`metrics.rs` 注释与 `prometheus.yml` 声称「6 个固定指标」实为 **8 个 family**（+ `wiktor_query_latency_ms_bucket` + `wiktor_compile_status_total`）。
- **竞品表**：全仓库 grep 仅 MASTER-PLAN 遗留行，无成表。

## 3. 竞品对比表（第 1 块，最高优先）

MASTER-PLAN 明确「写入 README 前完成」。维度：**编译可观测性 / 插件化 / 开源 vs 托管 / 检索质量 / 部署形态**（配 Meilisearch 作基线）。列：Wiktor、Meilisearch、Vectara、Pinecone、Weaviate、LangChain、WeKnora、rqlite，加上"工程对标"的 etcd/Redis（研发依赖叙述，不入表）。

**表放 README 新 <h2> "Wiktor in context"**，用 Markdown 表格 + 一行为主，权责清晰：
- 对每列给 1 句摘要，不吹牛，承认"检索质量 Not peer-reviewed / 实测 recall@1 0→1（反馈闭环）"。
- 结论句落在「Wiktor 的独特位 = LLM 知识编译 + 质量门 + 反馈可观测闭环，而非又一个向量库」。

**实现载体**：README 新增段落 + MASTER-PLAN 遗留勾销（§遗留事项 加 `✅ P5`）。中英同步。

## 4. README 首屏重构（第 2 块）

在保留现有 logo/tagline/badge 结构的前提下补齐（中英两个文件同步改）：

- **badge 行**追加：`CI status`（GitHub Actions）、`crates.io`、`docs.rs`。
- **Quick start** 补 `cargo install wiktor`（提示 feature 与 server/console 需 `--features`）+「预编译二进制」节（指向 GitHub Releases，标注"占位，首版随 CHANGELOG 护航，P5 不实际发版"）。
- 新增 **"Roadmap"** <h2>：从 Status 拆出「下一步」——生产审计（Raft 高可用、集群分片）、真实基准发布、第二方插件、release 自动化。
- 新增 **"Getting help / Community"**：GitHub Issues 入口 + Discussions 计划。
- **Status 修正**：Steps 1–11 → **Steps 1–14**（含 Step14 反馈语义匹配/多领域），测试数 397 → **cargo test --workspace 435+**（以实测为准），quoted recall 数据保留。
- **License 段**补一句 Apache-2.0 的可商用/署名（no 例外）。

## 5. 贡献基础设施（第 3 块）

新增顶层文件（中英或英文单份 + 相关章节在 README 链入口）：
- `CONTRIBUTING.md`——build/precheck/test gate 说明 + 贡献流程（issue→PR→review）+ 双语文档约定（docs 双语、README 中英）。
- `CODE_OF_CONDUCT.md`——Contributor Covenant 2.1 英文。
- `SECURITY.md`——上报渠道（GitHub Security advisory / issues）+ 支持范围（当前稳定 main）。
- `CHANGELOG.md`——git-log 驱动的版本清单（当前 0.1.0 → 后续按 release 收口）。

`.github/` 补：`ISSUE_TEMPLATE/bug_report.md` 与 `feature_request.md` + `PULL_REQUEST_TEMPLATE.md`。

## 5. crates.io 元数据（第 4 块）

- 根 `Cargo.toml` `[workspace.package]` 补 `description`（一句英文项目简介）、`homepage`（= GitHub repo）、`documentation = "https://docs.rs/<crate>"`。注意 workspace.package 的 `documentation` 若不支持 `homepage` 风格子 serde，就只补 description/homepage，documentation 落到各 crate。
- 各 crate `Cargo.toml`：补全部缺 description，现有中文 description 英文化（dict/console/vector-qdrant）。
- 不发布 Release（本轮不 publish），仅把门面备好。

## 6. 运维一致性收口（第 5 块）

- `deploy/README.md` 补 4 节：**Rollback**（升级失败 → 备份旧 binary + 回退 litestream 快照）、**Environment variables**（完整 env 索引表，从 wiktor.env.example 与源码 grep 汇总所有 `WIKTOR_*`）、**Observability**（/metrics 展示 8 个 family、Prometheus scrape 目标、`alerts-wikitor.yml` 规则与告警通道）、**Health**（`/health` 语义=服务存活，含 kernel 健康注入）。
- `crates/wiktor-server/src/metrics.rs` 注释 + `deploy/prometheus.yml` L4-5 口径修正：「6 个固定指标」→「8 个 family（…列名）」。

## 7. 验收判据

1. README 首屏含 Comparison 表、Roadmap、Community、cargo install、Status 正确（Steps 1–14、实测 test 数）。
2. 新增 4 顶层文档 + 3 .github 模板，README/Documentation map 链它们。
3. workspace package 补元数据、4 crate 补齐 description 英文、crates.io 元数据备齐。
4. deploy/README 含回滚/env 全表/观测/health 四节；metrics 口径更新为 8。
5. MASTER-PLAN 遗留「竞品对比表」勾销。
6. rust 改动仅限元数据/注释 —— `cargo build --workspace` 全绿（含新增 doc config）、`cargo fmt --check` 干净、新增文档无死链。

## 8. 实施与同步流程

- 纯文档 + Cargo 元数据，无业务逻辑改动，主模型直接实现（不派子智能体）。
- 走既有同步协议：本机改 → tar over SSH（`--exclude='._*'`）→ Linux squash commit → push GitHub → 本机 pull。
- 实现偏差登记本文档 **§9 偏差记录**（STEP-P5-00x）。

## 9. 偏差记录（2026-09-27）

- **STEP-P5-001（竞品表落 README 而非独立文件）**：spec 只要求「表放 README <h2>」，实现放进 README「Wiktor in context」/「对标与定位」节，中英两版各一张表（9 行含 rqlite 作内核类比）。『内核类比』行与『向量后端 qdrant』行是工程对标叙述，不是直接竞品，未与 Vectara 等并列成夸大。保留 MASTER-PLAN 与 README 现有「体验对标 Meilisearch、可靠性对标 etcd」口径，只在 README 表头加工程基线说明。
- **STEP-P5-002（workspace documentation 指向 docs.rs/wiktor）**：根 `[workspace.package].documentation = "https://docs.rs/wiktor"` 仅是**继承引用源**，实际各 crate 都用显式 `documentation = "https://docs.rs/wiktor-<crate>"`，故该 workspace 值永不落到发布元数据（wiktor 本身非独立 crate）。保留为取默认的占位。
- **STEP-P5-003（crate description 全部英文）**：7 个 crate 的 description 全部英文化；`wiktor-console` 从「Web + TUI」改述为强调只读监督面，无功能变化。
- **STEP-P5-004（运维口径修正不含行为）**：`metrics.rs` 与 `prometheus.yml` 注释由「6 个固定指标」改「8 个 family」，纯注释口径；不含 metrics 渲染行为改动（8 个 family 为 Step11 B4 已存在的事实）。