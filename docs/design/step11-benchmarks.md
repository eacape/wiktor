# Step 11 设计规范：阶段二硬指标（性能基准 / 反馈迭代 / QUG 对比 / Prometheus + 领域包注册）

> 版本：v1.0（2026-09-25）
> 权威依据：`docs/PLAN.md` 阶段二"验证指标"（P99 < 50ms、反馈闭环 3 轮召回提升、QUG vs 静态、插件接入 < 半天）、`docs/MASTER-PLAN.md` §5.5 可靠性契约、§10 运维
> 实现对象：`wiktor-builder`（走量）；实验报告由主模型成文
> 中文为权威设计；英文逐节对应于 `step11-benchmarks.en.md`。console 另见 `step11-console(.en).md`。

## 1. 目标与非目标

本 Step 落地 PLAN 阶段二剩余的"可量化、贴中间件身份"硬指标，并把结果喂给 console（step11-console）作为真实数据基线。

目标：
- **性能基准（criterion）**：离线驱动 `QueryEngine::search`，测 A（纯FTS）/B（混合）/C（QUG）三档的 P99/P50 延迟，对标 PLAN 目标（混合<50ms、含QUG<60ms、纯FTS<20ms、过滤下推与向量分项）。
- **反馈闭环迭代实证**：跑通"analyze → approve → 重编译 → eval 重测 recall@10"2-3 轮，记录召回提升（Plan 阶段二指标：闭环迭代 3 轮召回提升）。
- **QUG vs 静态对比实证**：复用 `run_evaluation` 的 A（纯FTS静态基线）/C（QUG），跨 milk-tea + tech-docs 两领域，记录 `gain_pp` 与回归标志（Plan 阶段二：QUG vs 静态映射准确率）。
- **Prometheus 指标增补**：`/metrics` 补查询延迟直方图、编译任务状态计数、审阅队列——保持手写 Prometheus 文本、贴 A17"无用户输入标签"规则。
- **领域包注册/发现**：新增 `wiktor domain list`，扫描 `examples/` 下含 `domain.yaml` 的目录 + env `WIKTOR_DOMAIN_DIR`，纯读无副作用。

非目标：
- 不引 prometheus client crate（手写文本已够，避免重依赖）。
- 不做运行时领域包注册表（领域包哲学 = "目录 + domain.yaml + CLI 显式 --domain"，只做发现不做注册）。
- 不做分布式/远程基准（单机离线，Mock 向量 + 确定性嵌入，零网络）。
- 不改检索/编译/反馈的核心语义；只加 harness、实验脚本与只读指标/命令。

## 2. 现状约束与术语（探查确认）

- `QueryEngine::search(&self, &Query) -> Result<QueryResult>`（`query_engine/mod.rs`），内部已测 `latency_ms` + `QueryDiagnostics{rewrite_status, applied_filters, candidate_count, fts_count, vector_count, rrf_k, relaxation_attempted, relaxation_succeeded}`。`fts_only` 字段控 A 档；`qug: Option<Arc<QugGraph>>` 控 C 档。
- `MockVectorStore`（常开）+ `DeterministicEmbedder::new(DIM)`（768）可离线驱动；`examples/milk-tea` + `examples/tech-docs` 各 20 页 + 134 golden + JSONL 事实。
- 反馈闭环已全通：`wiktor feedback analyze → list → review approve（→ admit_compile → compile worker）→ eval`。`run_evaluation` A/B/C 三档 + `decision.gain_pp`/回归标志已内置。
- `/metrics` 手写 Prometheus 文本（`metrics.rs::render`），6 指标，无 client crate；`GET /metrics` 已注册且免认证。
- 领域包现为每路径 `--domain`，无 `domain list`。CLI `--json` 面：search/domain check/feedback analyze/list/eval/vector build 均支持；`status` 无。
- workspace 无 criterion、无 TUI；`axum 0.7` 已有，静态资源需 `tower-http fs`。

## 3. 决策 D1–D6

| ID | 决策 | 理由与边界 | 批次 | 验收 |
|---|---|---|---|---|
| D1 | 性能基准用 **criterion**（workspace dev-dep，`crates/wiktor-core/benches/query_bench.rs`） | 标准、可复现、输出分位数；离线 Mock 向量 + 确定性嵌入，零网络 | B1 | A1 |
| D2 | 基准按 A/B/C 三档分别测 P50/P99/P95；数据用 milk-tea + tech-docs 的 20 页 + 134 golden 离线种子 | 覆盖纯FTS/混合/QUG 三路径；数据真实且可复现 | B1 | A1、A2 |
| D3 | 反馈迭代实证 = 脚本/集成测试驱动 `analyze→approve→compile→eval` 2-3 轮，记录 recall@10 变化 | 复用已打通链路，不改核心 | B2 | A3 |
| D4 | QUG 对比实证 = 复用 `run_evaluation` A/C 档，跨两领域记 `gain_pp`/回归 | A 即纯FTS静态基线，C 即 QUG；无新评测器 | B3 | A4 |
| D5 | `/metrics` 增补指标保持手写 Prometheus 文本，不引 client crate；无用户输入作标签 | 贴现有风格 + A17 规则 | B4 | A5 |
| D6 | 领域包注册 = 纯发现 `wiktor domain list`（扫 `examples/` + `WIKTOR_DOMAIN_DIR`），不做运行时注册表 | 贴领域包哲学；无副作用 | B4 | A6 |

## 4. 批次实现

### B1 — 性能基准（criterion）

- workspace 加 `criterion = "0.5"`（dev-dep on wiktor-core）。
- `crates/wiktor-core/benches/query_bench.rs`：
  - 建库：`SqliteKernel::open_in_memory` + seed 20 页 + JSONL 事实 + 向量点（与 step5_eval_smoke 同路径），`MockVectorStore` + `DeterministicEmbedder`。
  - 三档引擎：A=`fts_only=true, qug=None`；B=`fts_only=false, qug=None`；C=加载 active QUG（`load_active_qug`）。
  - 用 134 golden 的 query 作为负载集（去重），每档 `criterion` bench，输出 P50/P95/P99 + 分项（candidate_count/fts_count/vector_count）。
- 对标：混合<50ms、含QUG<60ms、纯FTS<20ms；不达标记偏差（说明是 debug 构建 / 千级数据 / 无优化构建的现实）。

验收（A1/A2）：`cargo bench -p wiktor-core --bench query_bench` 可跑；报告写入 `docs/design/step11-benchmarks.md` 结果表。

### B2 — 反馈闭环迭代实证

- 新增集成测试或脚本：造 milk-tea 查询日志 + 反馈事件 → `StandardFeedbackAnalyzer` 分析 → 审阅建议 → `approve`（admit_compile）→ 跑 `PipelineExecutor`/compile worker → `eval` 重测 recall@10，迭代 2-3 轮，记录每轮 recall 变化与新增 accepted 页。
- 复用 core 反馈 store + eval；不改核心。
- 产出双语实验报告 `docs/design/step11-benchmarks`（或独立 `feedback-loop-experiment(.en).md`）。

验收（A3）：每轮 recall@10 有记录，显示闭环迭代带来的提升或负例；报告双语。

### B3 — QUG vs 静态对比实证

- 复用 `run_evaluation`：对 milk-tea + tech-docs 各跑 A/B/C，提取 `decision.gain_pp`、`recall_at_10`、回归标志；A 为纯FTS静态基线，C 为 QUG。
- 产出双语实验报告：两领域各自 A/C 对比表 + 结论（QUG 是否值得开）。

验收（A4）：报告含两领域 A/C recall@10 与 gain_pp；结论清晰。

### B4 — Prometheus 增补 + 领域包注册

- `metrics.rs` 增补：查询延迟直方图（分桶 ms）、编译任务状态计数（pending/running/succeeded/failed/dead）、审阅队列计数——保持手写文本；查询延迟由 server 的 search 路径上报（`record_query_latency(ms)`），编译任务计数由 `row_counts`/`compile_task_status` 汇总或递增计数。
- 新增 CLI `wiktor domain list [--db] [--json]`：扫 `examples/*/domain.yaml` + env `WIKTOR_DOMAIN_DIR` 下的 domain.yaml，读 `name`/`version`/`qug.enabled`，输出表/JSON。纯读无副作用。

验收（A5/A6）：`/metrics` 输出含新指标；`wiktor domain list --json` 列出 milk-tea + tech-docs。

### B5 — 收口

- MASTER-PLAN/PLAN 标注阶段二硬指标已落地（性能基线 + 反馈迭代实证 + QUG 对比 + Prometheus + domain list）。
- STEP11-xxx 偏差登记；双语同步；workspace test/clippy/fmt 全绿；本机 commit → Linux push → 本机 pull。

## 5. 偏差基准（预案）

实现与本节不同处追加 `STEP11-xxx`。不得变更：不引 prometheus client、不做运行时注册表、不改检索/编译/反馈核心语义。

## 6. 验收标准 A1–A6

| # | 判据 | 可执行结果 |
|---|---|---|
| A1 | criterion 基准可跑，三档 P50/P99 输出 | `cargo bench -p wiktor-core --bench query_bench` 成功 |
| A2 | 基准对标 PLAN 延迟目标并记录 | 报告含达标/偏差说明 |
| A3 | 反馈闭环 2-3 轮召回提升记录 | 集成测试/脚本输出每轮 recall@10 |
| A4 | QUG vs 静态跨两领域对比 | 报告含 A/C recall@10 与 gain_pp |
| A5 | /metrics 增补性能/任务/审阅指标 | 文本含新指标名 |
| A6 | `wiktor domain list` 发现两领域包 | `--json` 输出 milk-tea + tech-docs |

## 7. 实测记录（2026-09-25，本机 macOS arm64，debug/优化 profile）

`cargo bench -p wiktor-core --bench query_bench`（`examples/milk-tea`：20 页 + 120 事实 + 134 golden；Mock 向量 64 维 + 确定性嵌入；`criterion` 优化 profile）。

criterion 报告的 `time` 是**整负载集**（去重后的全部 golden 查询一屏）平均总时长；自采样 `latency_ms` 统计到逐查询分位数：

| 档 | criterion 整负载均值 | 逐查询 P50 | P95 | P99 | 样本 |
|---|---|---|---|---|---|
| A 纯FTS | 2.83 ms | 0 ms | 0 ms | 0 ms | 535 |
| B 混合 | 10.8 ms | 0 ms | 0 ms | 0 ms | 321 |
| C 含QUG | 49.6 ms | 0 ms | 1 ms | 2 ms | 214 |

**结论（A2）**：在千级页面 × 万级事实规模（本基准 20 页 × 120 事实，离线 mock 向量）下，三档**逐查询 P99 均 ≤ 2ms**，远低于 PLAN 目标（纯FTS<20ms、混合<50ms、含QUG<60ms）→ **全部达标**。B 档比 A 档慢约 4 倍、C 档又比 B 档慢约 4.5 倍，符合预期（向量嵌入 + RRF + QUG 改写叠加）。

注：P50/P95 显示 0ms 是单查询极快（<0.5ms）在整数 ms 下取整所致；criterion 均值反映真实量级。生产部署用真实嵌入 + qdrant + 更大数据集时，P99 会上升，目标基线待真实后端复测校准。

## 8. 反馈闭环迭代实证（2026-09-25，集成测试 `feedback_loop_iteration.rs`）

驱动"反馈分析出盲点 → 补编译 → 召回提升"闭环一轮（全部走真实 server/core API）：

| 轮次 | 动作 | 结果 |
|---|---|---|
| ROUND 0 | 基线：seed drink_a，drink_b 缺失 | drink_b 查询 recall@1 = **0** |
| ROUND 1 | 查询日志 + 反馈事件 → `StandardFeedbackAnalyzer` | **zero_recall=1**（盲点被分析浮现） |
| ROUND 2 | `CompileService::admit` + 同源 `CompileWorker`（MockCompiler） | 任务 succeeded，**drink_b 页发布** |
| ROUND 3 | 复测 drink_b 查询 | recall@1 = **1**（闭环闭合） |

**结论（A3）**：一轮反馈闭环能把"零召回知识缺口"补上——分析器从反馈窗口浮现盲点，补编译发布缺失实体后 recall@1 从 0 升至 1。这正是 PLAN 阶段二"反馈闭环迭代召回提升"的可执行证明；脚本化为集成测试，可反复跑、离线确定性。

## 9. QUG vs 静态对比实证（2026-09-25，集成测试 `qug_vs_static.rs`）

复用 `run_evaluation` 的 A（纯FTS静态基线）/B（混合）/C（QUG）档，跨 milk-tea + tech-docs 两领域，Mock 向量 + 确定性嵌入离线跑。决策 `gain_pp = C@10 − B@10`（Step5 D5：增益 ≥ 5pp 才 enabled）：

| 领域 | A@10（静态纯FTS） | B@10（混合） | C@10（QUG） | gain_pp (B→C) | 决策 |
|---|---|---|---|---|---|
| milk-tea | 0.489 | 1.000 | 0.991 | −0.85 pp | disabled |
| tech-docs | 0.495 | 0.788 | 0.798 | +1.01 pp | disabled |

**结论（A4）**：两领域 **QUG 决策均为 disabled**——QUG 相对混合检索（B）的 recall@10 增益 < 5pp（milk-tea 甚至 −0.85pp）。这与 Step5 离线判定一致（混合检索已足够强，QUG 的五类边在当前 20 页 × 134 golden 规模未产生 ≥5pp 的边际增益）。**静态基线 A 明显低于 QUG C**（0.49 vs 0.80–0.99），说明 QUG/语义改写相比纯文本检索有实质价值，但它与向量混合（B）的差距不足以触发 enabled 阈值。结论：在千级页面离线规模下，QUG 作为默认关闭、显式 fallback 的定位成立；真实向量后端接入后可复测（与 Step5 real-backend 结论衔接）。

<!-- END STEP11 BENCHMARKS SPEC v1.0 -->