# console_ui — Web console 设计系统与真实数据接入

> 本目录是 Wiktor Web console（Step 11 B5）的视觉设计资产 + 真实数据接入说明。中文为权威版本，英文对应 `README.en.md`。

## 1. 三个文件的关系

| 文件 | 角色 |
|---|---|
| `DESIGN.md` | Obsidian 主题设计系统（色板 / 字体 / 间距 tokens），是视觉的**唯一权威来源** |
| `code.html` | 完整 Web 原型（单文件，内联 CSS/JS），按 `DESIGN.md` 的 tokens 实现仪表盘 / 编译任务 / 质量雷达 / 审阅 / 检索分路 / QUG 面板 |
| `screen.png` | 原型的渲染截图，用于评审与回归对照 |

关系：`DESIGN.md` 定 tokens → `code.html` 消费 tokens 呈现原型 → `screen.png` 是结果快照。改视觉先改 `DESIGN.md`，再同步 `code.html`，避免两张皮。

## 2. 从静态原型到真实数据

原型本身不取数；真实数据由 `wiktor console`（新 crate `wiktor-console`，Step 11 B5）提供——本地 HTTP 服务，in-process 读 `SqliteKernel`，并在 `/` 原样返回 `code.html`：

```bash
# 1) 建库 + 灌数据（领域包任选）
wiktor seed --db ./wiktor.db --domain examples/tech-docs/domain.yaml

# 2) 启动 Web console（feature console，默认关）
cargo build -p wiktor-cli --features console
./target/debug/wiktor console --db ./wiktor.db --listen 127.0.0.1:8081

# 3) 浏览器打开 http://127.0.0.1:8081/（原型视觉）
#    JSON API 是真实数据面：
curl http://127.0.0.1:8081/api/overview          # schema + row_counts + review_pending
curl http://127.0.0.1:8081/api/tasks             # due 编译任务
curl http://127.0.0.1:8081/api/reviews           # 审阅队列
curl http://127.0.0.1:8081/api/qug               # QUG 代次/发布状态
curl -X POST -H 'Content-Type: application/json' \
     -d '{"q":"retrieval","top_k":5}' \
     http://127.0.0.1:8081/api/search            # 混合检索 + QueryDiagnostics 分路诊断
```

端点契约、实现偏差（STEP11-001..005，含 tower-http 缺席与 TUI 推迟）见 `docs/design/step11-console(.en).md` §7；性能基线见 `docs/design/step11-benchmarks(.en).md`。

## 3. 边界

- console 是**只读监督界面**：`POST /api/search` 走与 CLI `wiktor search` 同源的 QueryEngine（查询日志照常落 `query_logs`），除此之外不写任何表；编译/审阅等写操作留在 CLI/server。
- **`code.html` 已接真实数据（Step12 B2）**：由 `wiktor console` 提供服务时，面板经 vanilla fetch 轮询 `/api/*` 并渲染（15s 间隔 + 检索框实时查）；API 不可达时自动回退原型静态文案并显示 `offline` 徽标——直接双击打开本地文件时始终是原型态。
- TUI（`wiktor tui`，ratatui，feature `tui`）于 Step12 B1 交付：与 Web console 同一数据面，四 Tab 键盘导航；见 `docs/design/step12-tui-console-prod(.en).md`。
