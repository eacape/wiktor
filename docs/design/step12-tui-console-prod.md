# Step 12 设计规范：TUI + Console 真实数据 + 生产部署编排

> 版本：v1.0（2026-09-25）
> 权威依据：`docs/MASTER-PLAN.md` §9/§12/§17（#12）、`docs/design/step11-console.md`（D4 TUI 欠账 + code.html 原型 + JSON 契约）、`deploy/`（litestream 既有资产）
> 前置变化：STEP11-001/005 的根因（crates.io 不可达）已解除——rsproxy 镜像实测可用（2026-09-25，index + 下载均 200），本机可拉 ratatui 0.29 / crossterm 0.28。
> 中文为权威设计；英文逐节对应 `step12-tui-console-prod.en.md`。

## 1. 目标与非目标

把 Step 11 的三笔欠账一次结清：

- **B1 TUI**（补 STEP11-005）：`wiktor tui`（feature `tui`，默认关），ratatui 0.29 + crossterm 0.28；四面板（仪表盘 / 编译任务 / 审阅队列 / 查询终端），键盘导航，只读，数据面与 Web console 同源。
- **B2 Console 前端接真实数据**：`code.html` 通过 vanilla fetch 渲染 `/api/*` 到面板（无构建步骤）；API 缺席时面板回退原型静态态并显示 offline 徽标。
- **B3 生产部署编排**：`deploy/` 增 wiktor-server / wiktor-console systemd 单元 + env 模板 + 安装脚本 + runbook 增补，并在 Linux 生产机（Debian 13 x86_64）实机部署 + smoke。

非目标：Tauri 桌面壳；多用户/权限；Raft/集群；容器化（Docker/K8s）；CI/CD 发布流水线。

## 2. 现状约束

- TUI 面板数据 API 已存在：`schema_version`/`row_counts`/`count_pending_reviews`/`list_due_compile_task_ids`/`compile_task_status`/`list_reviews`/`row_counts`（QUG 代次）；检索走 `QueryEngine`（无 QUG → 混合 fallback，Mock 向量空集合 → RRF 仅 FTS）。
- console JSON 契约稳定（`/api/overview|tasks|reviews|qug|search|domains`）；`GET /` 由 handler 从磁盘返回 code.html。
- deploy/ 已有：install-litestream.sh / backup-preflight.sh / restore-drill.sh / litestream.service / README runbook。
- Linux 生产机：Debian 13 x86_64，2C/2G/39G，root SSH 免密，gRPC 50051 / HTTP 8080 需放行或本机 smoke。

## 3. 决策 D1–D6

| ID | 决策 | 理由与边界 |
|---|---|---|
| D1 | TUI 落 `wiktor-console` crate（spec step11 D4 的"或"取此选项），feature `tui`（默认关）；CLI `wiktor tui` 转发 `wiktor_console::tui::run` | 与 Web console 同 crate 共享数据装配；不新增 crate |
| D2 | TUI 数据面与 Web console 同源：in-process `SqliteKernel` 读 API + `QueryEngine` 检索；只读（查询照常落 query_logs） | 与 D1 同一装配函数，双形态零漂移 |
| D3 | TUI 渲染与状态分离：state 纯函数（`update(state, event)`）+ ratatui `TestBackend` 单测，不依赖真实 tty | CI 可跑；键盘导航可测 |
| D4 | code.html 增加渲染层：面板元素挂 `data-api` 标记，fetch 后按契约填 DOM；失败/超时保留原型文案 + offline 徽标 | 无构建步骤、无框架；原型视觉零改动 |
| D5 | 部署编排 = systemd 单元（沙箱对齐 litestream.service）+ env 文件 + `install-wiktor.sh`（服务器上 release 构建）；二进制 /usr/local/bin，数据 /srv/wiktor-data；与 litestream 串联 | 单机形态、最小依赖；2G 内存限制下 `cargo build --release -j2` |
| D6 | 生产部署验收在 Linux 实机执行：systemd 起服务、/health 200、API key 授权生效、console 面板出真数据 | 编排不做纸面交付，必须实机 PASS |

## 4. 批次实现

### B1 — TUI

- `wiktor-console/Cargo.toml`：`[features] tui = ["dep:ratatui", "dep:crossterm"]`；ratatui 0.29 / crossterm 0.28 为 optional 依赖。
- `src/tui/mod.rs`（feature-gated）：`pub async fn run(db: &Path) -> anyhow::Result<()>`（进入 alternate screen、循环渲染、退出恢复）；`state.rs` 纯逻辑：`TuiState { tab, overview, tasks, reviews, qug, query_input, search_result }` + `load_overview/load_tasks/load_reviews/refresh`；四 Tab（1 仪表盘 / 2 任务 / 3 审阅 / 4 查询），Tab/1-4 切换，查询 Tab 文本输入 + Enter 检索，q/Ctrl-C 退出。
- 查询终端与 Web console 同源：`QueryEngine::new(kernel, MockVectorStore…)`，展示 hits + QueryDiagnostics（rewrite/fts/vector/rrf_k）。
- CLI：`wiktor tui --db <db>`（feature `tui`）；`crates/wiktor-cli` feature `tui = ["dep:wiktor-console", "wiktor-console/tui"]`。
- 测试：state 纯函数单测（空库/有数据）；`TestBackend` 渲染冒烟（四 Tab 均可绘制、查询 Tab 输入触发 search 后 diagnostics 行可见）。

### B2 — Console 前端接真实数据

- code.html 头部面板（overview 计数）、任务表、审阅列表、QUG 面板挂 `data-api` 标记；`<script>` 渲染层：`fetchJson(path)` + 每 15s 轮询 + 搜索框接 `POST /api/search` 渲染 hits 与 diagnostics 徽标（fts/vector/rrf_k）。
- 失败回退：fetch 失败 → 面板显示 `offline` 徽标，保留原型静态内容；成功 → 替换计数/表格内容（保留设计 tokens）。
- 验收：起 `wiktor console` + seed 库，浏览器面板出真实数字；`curl /` 返回的 HTML 含 `/api/` 引用与渲染层；API 关停后页面显示 offline。

### B3 — 生产部署编排

- `deploy/wiktor-server.service`：`wiktor serve`（ExecStart 带 --db /srv/wiktor-data/wiktor.db），EnvironmentFile=/etc/wiktor/wiktor.env，沙箱（DynamicUser/ProtectSystem/NoNewPrivileges 对齐 litestream.service），After=network-online.target。
- `deploy/wiktor-console.service`：`wiktor console --db … --listen 127.0.0.1:8081`（仅本机绑定；远程访问经 SSH 隧道，不暴露公网）。
- `deploy/wiktor.env.example`：WIKTOR_API_KEYS（JSON 模板）、WIKTOR_COMPILE_WORKERS。
- `deploy/install-wiktor.sh`：检测 cargo→release 构建（-j2）→ 安装二进制 → 写 /etc/wiktor/env → enable 单元；幂等复用。
- `deploy/smoke-deploy.sh`：systemd is-active、/health 200、无 key 401/有 key 200（HTTP search）、console 8081 200。
- `deploy/README` 增补 bring-up runbook（含与 litestream 的顺序：先 restore-drill 后起服务）。
- Linux 实机：跑 install + smoke 全 PASS（D6）。

## 5. 偏差基准（预案）

不得变更：TUI/Web 同源只读数据面、code.html 原型视觉不重做、systemd 单机编排不引入容器化。实现偏离处记 `STEP12-xxx`（双语同步）。

## 6. 验收标准 A1–A6

| # | 判据 | 可执行结果 |
|---|---|---|
| A1 | spec 双语 | step12-tui-console-prod(.en).md 存在 |
| A2 | `wiktor tui` 可导航 | TestBackend 测试绿 + 手动冒烟记录 |
| A3 | 生产部署实机 PASS | Linux smoke-deploy.sh 全绿（health/auth/console） |
| A4 | console 面板真实数据 | 浏览器可见 seed 库数字 + offline 回退 |
| A5 | TUI 查询诊断可见 | 查询 Tab 显示 fts/vector/rrf_k |
| A6 | 收口 | workspace test/clippy/fmt 全绿 + 偏差双语 + MASTER-PLAN #12 |


## 7. 实现偏差与实测记录（2026-09-25）

### 实测记录（2026-09-25）

- **B1 TUI（本机 macOS arm64）**：`cargo test -p wiktor-console --features tui` 8 测试绿（Tab 循环/查询输入/状态装配/四 Tab TestBackend 渲染/诊断行/错误行/空 kernel 装载）；python pty（80×24）实机冒烟：真实 seed 数据渲染（schema 6、pages 20、facts 840）+ 四 Tab + 帮助行，`q` 干净退出（A2/A5）。
- **B2 前端接真实数据（本机 IAB 浏览器实测）**：seed tech-docs 后打开 `http://127.0.0.1:8123/`——顶栏 `0 Tasks | 20 Pages`、审阅 pill `0 Pages Awaiting Arbitration`、QUG 徽标 `20 Published Pages · gen 0` 全部来自 `/api/*` 真实数据，offline 徽标隐藏；TEST EXEC 触发检索后诊断行变为 `Matched: 0 hits · fts 0 / vec 0 / rrf_k 60 · 11ms`、结果框显示 `（无命中 no hits）`（seed-only 库无编译 FTS 页，与 CLI search 一致）（A4）。验证备注：IAB 的合成 click/press 事件注入不可靠（探针监听器也不触发），检索链路用页面内程序化派发 button click 验证——handler→fetch→DOM 渲染全链路真实执行。
- **B3 Linux 生产实机（Debian 13 x86_64，2C/2G+4G swap）**：rustup 1.98.1（rsproxy 镜像）+ protoc 3.21 + `install-wiktor.sh`（release `-j2`，约 15 分钟）→ `/usr/local/bin/wiktor` + 双单元 active；`smoke-deploy.sh` 7 项全 PASS：server/console active、`/health` 200、无 key 401、带 key 200、console `/` 与 `/api/overview` 200（A3/D6）。带 key 内容检索返回正常响应体（seed-only 库 0 命中，与离线一致；Step7 的 HTTP `GET /search` 为轻量路径不落 query_logs）。
- **部署 gotcha（已修进模板/脚本）**：①`WIKTOR_API_KEYS` 的 methods 只接受小写 snake_case（PascalCase 启动即 fail-closed 报错，错误体列出允许集）；②HTTP `GET /search` 必须带 `domain` 参数（缺失 400）——`smoke-deploy.sh` 已补 `domain=${DOMAIN:-tech-docs}`。


### 实现偏差（STEP12-001..004）

- **STEP12-001（B2，API 扩展）**：`GET /api/overview` 新增 `task_status_counts` 字段（due 编译任务按 status 计数，遍历 due 快照聚合）——spec §4 B2 未列该字段；它是前端状态机五胶囊的真实数据源，并兑现 step11-console spec D3"overview 含编译任务状态计数"的原意。
- **STEP12-002（B3，数据目录）**：生产数据目录为 `/srv/wiktor/data`（对齐既有 `litestream.service` 的 ReadWritePaths），而非 spec D5 写的 `/srv/wiktor-data`；units 沙箱 ReadWritePaths 同步。
- **STEP12-003（B3，工具链）**：`install-wiktor.sh` 不自动安装 Rust 工具链（缺 cargo 即报错退出 2），安装步骤放 runbook（rustup + 国内镜像）；生产机因此引入**最小工具链**（rustup minimal + rsproxy）——"Linux 不装开发环境"约定更新为"生产机构建所需最小工具链"。
- **STEP12-004（B1，检索行为）**：TUI 查询 Tab 不装配 FilterRelaxer（CLI `wiktor search` 显式装配 `DefaultFilterRelaxer`）；TUI/Web console 检索均无过滤输入，滤空放宽重试不会触发——两形态与自身输入面对齐。

不变项自查（§5 偏差基准）：TUI/Web 同源只读数据面 ✅、code.html 原型视觉不重做（仅挂标记 + 追加渲染层）✅、systemd 单机编排不引入容器化 ✅。

<!-- END STEP12 SPEC v1.0 -->
