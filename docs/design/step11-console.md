# Step 11 设计规范：Wiktor Console（Web + TUI）

> 版本：v1.0（2026-09-25）
> 权威依据：`docs/MASTER-PLAN.md` §9/§十二（TUI 为可选 feature、Web UI 远期）、`docs/PLAN.md` 阶段二/四、`docs/console_ui/DESIGN.md`（Web 视觉设计系统）、`docs/console_ui/code.html`（Web 原型）、`docs/design/step11-benchmarks.md`（硬指标数据源）
> 实现对象：`wiktor-builder`（走量）
> 中文为权威设计；英文逐节对应于 `step11-console.en.md`。硬指标/性能基线另见 `step11-benchmarks(.en).md`。

## 1. 目标与非目标

把 console 从"静态设计/原型"（`docs/console_ui/`）推进为**对接真实 Wiktor 数据的可运行界面**，双形态：Web（复用已有 Obsidian 视觉 + code.html 原型）与 TUI（ratatui）。

目标：
- **Web console**：本地 HTTP 服务（`axum 0.7` + `tower-http fs` 静态服务 code.html 视觉），JSON API in-process 对接 `SqliteKernel`，面板显示真实数据（仪表盘/编译任务/质量雷达/审阅/检索 RRF 分路/QUG）。
- **TUI console**：`wiktor tui`（ratatui+crossterm），贴近单机命令行形态，同源数据读 API。
- 面板数据来自 `step11-benchmarks` 的性能基线 + `SqliteKernel` 真实读 API。
- 交付 `docs/console_ui/README` 说明如何从静态原型到真实数据。

非目标：
- 不做 Tauri 桌面壳（远期，阶段四）；Web 先做本地浏览器访问。
- 不改 server/核心检索语义；console 是读界面（编译/审阅等操作为只读展示，不新写写路径，除非复用现有 server gRPC）。
- 不做复杂权限/多用户；本地单机访问。

## 2. 现状约束与术语（探查确认）

- Web 经网络只能访问 gRPC 六服务 + 4 个 HTTP 端点（`GET /search` FTS-only、`/health`、`/metrics`、`POST /feedback`）；状态/任务/审阅/QUG/质量无 HTTP 端点 → 需 gRPC 或 **in-process `SqliteKernel`**。
- `SqliteKernel` 真实读 API：`row_counts`/`schema_version`、`compile_task_status`/`list_due_compile_task_ids`、`list_reviews`（含 compile_dead_letter/consistency_conflict action）、`load_feedback_window`、`accepted_page_vectors`、`compatibility_page_identities/task_snapshots`、`active_build_identity`（QUG）、`execute_batch`（任意 SQL）。
- 质量评分无专用读 API，仅 `page_quality` 表（经 SQL 读）。
- 检索 RRF 分路：`QueryDiagnostics`（每查询返回 fts_count/vector_count/rrf_k）。
- 服务器 `run_server` 单进程绑 HTTP+gRPC，需 `WIKTOR_API_KEYS`；CLI `wiktor serve`。
- `docs/console_ui/` 已有 DESIGN.md（Obsidian 主题设计系统）+ code.html（Web 原型）+ screen.png。
- workspace 无 TUI 代码；`axum 0.7` 已有；静态资源需加 `tower-http = { features = ["fs"] }`。

## 3. 决策 D1–D6

| ID | 决策 | 理由与边界 | 批次 | 验收 |
|---|---|---|---|---|
| D1 | Web console **in-process 嵌入 `SqliteKernel`**，新 `wiktor console --db --port [--static <code.html>]` 子命令（feature `console`）；gRPC 远程为可选后续 | 状态/任务/审阅/QUG/质量无 HTTP 端点，in-process 让面板读到全部真实数据；免 WIKTOR_API_KEYS | B5 | A3 |
| D2 | 静态资源复用 `docs/console_ui/code.html`（原型视觉），console 服务 serve 它 + JSON API 端点 | 复用已有设计，不重做 UI；code.html 已是完整 Web 原型 | B5 | A3 |
| D3 | JSON API 端点集 = 仪表盘/编译任务/质量雷达/审阅/检索分路/QUG/domain 列表，映射到 `SqliteKernel` 真实读 API；检索分路经 `QueryEngine::search`（in-process）取 QueryDiagnostics | 面板数据真实；不发明 mock | B5 | A3、A4 |
| D4 | TUI 用 **ratatui + crossterm**，`wiktor tui`（feature `tui`），复用同源读 API（仪表盘/任务/审阅/查询终端） | PLAN 阶段四 TUI 拉入；贴近命令行形态 | B6 | A5 |
| D5 | 编译/审阅等操作为**只读展示**（复用现有 server gRPC 或直接只读），不新写写路径 | console 是监督/诊断界面；写操作留 CLI/server | B5/B6 | A3 |
| D6 | 性能面板数据来自 `step11-benchmarks` 报告（离线基准），非实时探针 | 避免给 console 引入计时负担；基准为准 | B5 | A2 |

## 4. 批次实现

### B5 — Web console

- workspace 加 `tower-http = { version = "0.6", features = ["fs"] }`。
- 新 crate `crates/wiktor-console`（feature-gated，依赖 core + axum + tower-http + tokio + serde_json）：
  - `ConsoleState { kernel: Arc<SqliteKernel> }`。
  - JSON API（in-process 读 kernel）：
    - `GET /api/overview` → schema_version + row_counts + 编译任务状态计数（list_due + compile_task_status 汇总）+ 审阅 pending 数。
    - `GET /api/tasks` → `list_due_compile_task_ids` + 每任务 `compile_task_status`。
    - `GET /api/reviews` → `list_reviews(domain?, None, limit)`。
    - `GET /api/quality` → 读 `page_quality` 表（SQL）聚合五维（coverage/citation/schema/density/consistency）。
    - `GET /api/qug` → `active_build_identity(domain, version)` + source_hash。
    - `POST /api/search` → `QueryEngine::search`（in-process，Mock 向量 + 确定性嵌入，离线）返回 QueryResult + QueryDiagnostics。
    - `GET /api/domains` → 领域包列表（复用 `wiktor domain list` 发现逻辑）。
  - 静态：`GET /` → serve `docs/console_ui/code.html`（复刻原型视觉）；`GET /assets/*` → 其余静态。
  - CLI `wiktor console --db <db> --port <port> [--static <dir>]`（feature `console` 转发）。
- 验收（A3）：浏览器打开看到真实数据面板（非 mock）；`POST /api/search` 返回真实检索诊断。

### B6 — TUI

- workspace 加 `ratatui = "0.29"` + `crossterm = "0.28"`（dev/feature 视需求）。
- 新 `crates/wiktor-console` 或 CLI 内 `wiktor tui`（feature `tui`）：
  - 布局：仪表盘（schema/row_counts/任务计数）/ 编译任务表（list_due + compile_task_status）/ 审阅队列（list_reviews）/ 查询终端（QueryEngine::search + QueryDiagnostics 分路）。
  - 复用 Web 同源读 API（kernel 读）。
- 验收（A5）：`wiktor tui` 启动显示真实数据；键盘导航。

### B7 — console_ui/README + 收口

- `docs/console_ui/README.md`：说明 DESIGN.md / code.html / screen.png 的关系，以及如何跑 `wiktor console` 把它接真实数据。
- MASTER-PLAN 标注 TUI/Web console 拉入（TUI 为可选 feature 已定义，Web 从远期拉入本地形态）。
- STEP11-xxx 偏差登记；双语同步；workspace test/clippy/fmt 全绿；本机 commit → Linux push → 本机 pull。

## 5. 偏差基准（预案）

实现与本节不同处追加 `STEP11-xxx`。不得变更：in-process 嵌入而非只做 gRPC 客户端、复用 code.html 视觉而非重做 UI、console 为只读监督界面、性能面板以离线基准为准。

## 6. 验收标准 A1–A5

| # | 判据 | 可执行结果 |
|---|---|---|
| A1 | console 设计文档双语 | step11-console(.en).md 存在 |
| A2 | 性能面板引用 step11-benchmarks 结果 | API/文档引用基准报告 |
| A3 | Web console 显示真实数据 | `wiktor console` 浏览器可见真实面板 + JSON API 返回非 mock 数据 |
| A4 | 检索分路诊断可见 | `POST /api/search` 返回 QueryDiagnostics |
| A5 | TUI 启动显示真实数据 | `wiktor tui` 可导航 |

## 7. 实现偏差与实测记录（2026-09-25）

### 实测记录（B5 Web console，本机 macOS arm64，`wiktor console --db /tmp/console-test.db --listen 127.0.0.1:8123`）

- 构建：`cargo build -p wiktor-console` + `cargo build -p wiktor-cli --features console` 通过；`wiktor console` 与 `wiktor-console` bin 两个入口均可启动。
- 数据：`wiktor seed --domain examples/tech-docs/domain.yaml` 灌入 20 页 + 840 事实 + 168 fact_refs。
- `GET /api/overview`：返回 `schema_version:6` + 真实 `row_counts`（pages=20 / facts=840 / fact_refs=168 / page_sections=80 / page_quality=20）+ review_pending —— 真实 kernel 读（A3）。
- `GET /api/tasks`、`GET /api/reviews`、`GET /api/qug`：seed 库上分别返回空任务/空审阅/`{generations:0, published_pages:20}`，结构正确。
- `POST /api/search`（`{"q":"retrieval pipeline","top_k":3}`）：经 QueryEngine（与 CLI `wiktor search` 同源）返回命中 + **完整 QueryDiagnostics**（`rewrite_status:disabled`、`fts_count:0`、`vector_count:0`、`rrf_k:60`、`latency_ms`）—— seed-only 库无编译页 → FTS 0 命中与 CLI 一致（A4）。
- `GET /`：返回 `docs/console_ui/code.html`，与磁盘文件逐字节一致（`cmp` 验证）。
- `GET /api/domains`：返回 `{"domains":[]}`（占位，见 STEP11-004）。

### 实现偏差（STEP11-001..005）

- **STEP11-001（B5，静态服务）**：未引入 `tower-http = { features = ["fs"] }` —— 开发机 crates.io 不可达（sparse index 直连与代理均 403/404，2026-09-25 实测）。静态资源改为 handler 手动返回：`GET /` 从磁盘读 `docs/console_ui/code.html`（读失败回退占位 HTML），`ConsoleState.static_dir` 可覆盖目录；无 `/assets/*` 通配。依赖边界不变（仅 axum）。
- **STEP11-002（B5，CLI 形参）**：`wiktor console` 的监听参数为 `--listen <addr>`（默认 `127.0.0.1:8081`）而非 spec §4 的 `--port <port>`；与 `wiktor serve` 的 `--listen-http/--listen-grpc` 风格对齐，且 `serve(db, listen, static_dir)` 直接接受 addr 字符串。
- **STEP11-003（B5，质量面板）**：`GET /api/quality` 五维聚合端点暂未实现；`page_quality` 行数经 `/api/overview.row_counts` 暴露，五维明细读路径留待 console 迭代（kernel 侧已有表与 SQL 通道，无 schema 变更）。
- **STEP11-004（B5，domain 发现）**：`GET /api/domains` 返回文档化占位 `[]`；真实发现复用 `wiktor domain list` CLI（Step10 B4），不重复实现发现逻辑。
- **STEP11-005（B6，TUI 推迟）**：TUI（D4，ratatui 0.29 + crossterm 0.28，feature `tui`，`wiktor tui`）暂缓 —— crates.io 不可达导致依赖无法拉取（同 STEP11-001 根因）。设计不变；待 crates.io 可达的开发环境（Linux 机）补实现与 A5 验收。

不变项自查（§5 偏差基准）：in-process 嵌入 ✅、复用 code.html 视觉 ✅、只读监督界面（search 走 QueryEngine 属 D3 明示的既有查询路径，query_logs 落库非新写路径）✅、性能面板以离线基准为准（step11-benchmarks §7）✅。

<!-- END STEP11 CONSOLE SPEC v1.0 -->