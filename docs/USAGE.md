# Wiktor 使用手册 / Operation Manual

> 目标读者：想把 Wiktor 跑起来"真用一次"的人。分两条路径：**A. 本机离线试玩**
> （Mac/Linux 通用，能跑通全流程，检索是 mock 语义）、**B. 生产环境真实使用**
> （Linux + qdrant + 真实 LLM，检索是真实效果，**推荐**）。本文件已按实测验证。
>
> Audience: someone who wants to actually run Wiktor. Two paths: **A. local
> offline demo** (Mac/Linux, runs the whole flow, mock retrieval semantics) and
> **B. production real use** (Linux + qdrant + real LLM, real retrieval —
> **recommended**). Verified on real hardware.

---

## 前置 / Prerequisites

- Rust 工具链（1.85+）+ `protoc`（gRPC 代码生成需要）。
- 从源码构建 CLI（crates.io 发布尚未进行，`cargo install wiktor-cli` 目前不可用，
  见 README 的占位说明）：
  ```bash
  git clone https://github.com/eacape/wiktor
  cd wiktor
  cargo build -p wiktor-cli --release
  # 产物：target/release/wiktor
  ```

---

## A. 本机离线试玩（mock，不联网） / Local offline demo (mock, offline)

适合先感受"编译 + 检索"的完整流程，无需任何 key 或外部服务。用第二个官方领域包
`tech-docs`（技术文档语料）。全程离线。

```bash
WIK=./target/release/wiktor
DB=./demo.db
DOMAIN=examples/tech-docs/domain.yaml

# 1) 建库 + 导入领域包（seed-wiki + 事实平面数据）
$WIK seed --db $DB --domain $DOMAIN
#   → seeded: pages=20 facts=840 …

# 2) 编译成 Wiki 页（mock provider 离线可跑）
$WIK compile --db $DB --domain $DOMAIN --provider mock
#   → accepted=52 …（mock 编译器离线生成占位语义页；token 预算熔断会留部分 deferred，正常）

# 3) 检索（混合检索：QUG 改写 → 过滤下推 → FTS5 + 向量 → RRF 融合）
$WIK search --db $DB "gRPC"
#   → 返回 hits + 分路诊断（rewrite/expanded_terms/candidates/fts/vector/rrf_k）

# 4) 质量探针（五维评分，只读）
$WIK quality --db $DB --json | head

# 5) Web console（只读监督界面；需 console feature）
cargo build -p wiktor-cli --features console
./target/release/wiktor console --db $DB --listen 127.0.0.1:8081
# 浏览器打开 http://127.0.0.1:8081/
```

> **注意**：mock 编译生成的页是确定性占位内容，**检索召回不代表真实语义质量**。
> 想看到"真实效果"，走 B 路径。
>
> Note: mock-compiled pages are deterministic placeholders; retrieval recall here
> does **not** reflect real semantic quality. For real results use path B.

---

## B. 生产环境真实使用（Linux + qdrant + 真实 LLM，推荐） / Real use

### B0. 环境准备（一次性的生产部署）

参见 `deploy/README.md` 的 Bring-up runbook（装机 + systemd 双单元 + litestream +
Prometheus）。核心：

```bash
sudo ./deploy/install-wiktor.sh          # 构建 + 装单元 + 起服务（生成 /etc/wiktor/wiktor.env）
sudo ./deploy/smoke-deploy.sh            # 冒烟：/health 200、带/不带 key 的 200/401、console 200
```

真实检索需要 `qdrant`（向量服务）+ 真实嵌入/LLM。env 在 `/etc/wiktor/wiktor.env` 配：
`WIKTOR_VECTOR_BACKEND=qdrant`、`WIKTOR_QDRANT_URL`、`WIKTOR_QDRANT_API_KEY`、
`WIKTOR_EMBEDDING_BASE_URL/API_KEY/MODEL`、`WIKTOR_OPENAI_API_KEY`（编译用）。

### B1. 用 HTTP 服务检索（真实接口，推荐）

```bash
# 从 env 取该 domain 的 API key
KEY=$(python3 -c "import json,re; s=open('/etc/wiktor/wiktor.env').read(); \
  d=json.loads(re.search(r'WIKTOR_API_KEYS=(.+)', s).group(1)); print(d['tech-docs']['secret'])")

# GET /search（domain + query 参数；Bearer 认证）
curl -s -H "Authorization: Bearer $KEY" \
  "http://127.0.0.1:8080/search?domain=tech-docs&q=gRPC" | python3 -m json.tool
```

**实测返回**（2026-09-27，Linux 生产机，真实 qdrant + qwen 嵌入）：命中 5 条 gRPC
相关页（含 `IDL 先行的性能远程调用`、`grpc`、`tonic`、`qdrant`、`tokio`），并带
`diagnostics`（候选数 / FTS / 向量分路 / RRF 融合）。

### B2. 用 CLI 检索（直连库）

```bash
/usr/local/bin/wiktor search --db /srv/wiktor/data/wiktor.db "gRPC" --json
```

> **CLI 直查 vs HTTP 的关键差异**：CLI 直查默认用 mock 向量（不加载 serve 的 env），
> 结果里 `vector_count=0`；**HTTP 走 serve 才是真实 qdrant + 真实嵌入**。要看真实
> 效果用 HTTP，CLI 适合对离线/只读库做脚本化查询。
>
> CLI vs HTTP: CLI queries default to the mock vector (no serve env), so
> `vector_count=0`; HTTP through `serve` uses the real qdrant + real embeddings.

### B3. 反馈闭环（可选，展示"零召回 → 补编译"）

```bash
# 上报反馈事件（HTTP POST /feedback，Bearer 认证）
curl -s -H "Authorization: Bearer $KEY" -X POST http://127.0.0.1:8080/feedback \
  -d @feedback.json
# 分析出零召回盲点 → 生成补编译任务 → approve → worker 补编译 → 复检
/usr/local/bin/wiktor feedback analyze --db /srv/wiktor/data/wiktor.db --domain-pack ...
```

### B4. Web console 与监控

```bash
# console（生产 systemd 单元已起；SSH 隧道访问，不暴露公网）
ssh -L 8081:127.0.0.1:8081 root@<host>
# 本机浏览器打开 http://127.0.0.1:8081/

# 监控
curl -s http://127.0.0.1:8080/health    # 服务存活
curl -s http://127.0.0.1:8080/metrics   # Prometheus 指标（8 个 family）
```

---

## 常用命令速查 / Command quick reference

| 命令 | 作用 |
|---|---|
| `wiktor seed --db X --domain D.yaml` | 从领域包建库导入 |
| `wiktor compile --db X --domain D.yaml --provider mock\|openai` | 编译成 Wiki 页 |
| `wiktor search --db X "query" [--json]` | 混合检索 + 分路诊断 |
| `wiktor quality --db X --json` | 逐页五维质量探针（只读） |
| `wiktor qug build --db X` / `wiktor eval` | QUG 构建 / A/B/C 评测 |
| `wiktor feedback analyze\|list\|review` | 反馈闭环分析/列表/审阅 |
| `wiktor status --db X` | schema 版本 + 各表行数 |
| `wiktor serve --db X --listen-http .. --listen-grpc .. --domain D.yaml` | 常驻 gRPC+HTTP 服务 |
| `wiktor console --db X --listen 127.0.0.1:8081` | Web 只读监督界面 |
| `wiktor tui --db X` | 终端仪表盘 |

每个子命令用 `wiktor <cmd> --help` 看完整参数。