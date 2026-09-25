# Step 13 设计规范：生产就绪（副本/真实检索/反馈闭环/可观测/CI）

> 版本：v1.0（2026-09-25）
> 权威依据：`docs/MASTER-PLAN.md` §5.5 可靠性契约、§17 落地依赖 #12 后续；`docs/design/real-backend-experiment.md`（qdrant + HttpEmbedder 实测配置）；`deploy/`（litestream 资产）；step10 插件边界
> 中文为权威设计；英文逐节对应 `step13-production-ready.en.md`。

## 1. 目标与非目标

把生产实例从"部署起来了"推进到"可长期运维"：真实向量检索、反馈闭环生产走通、备份副本、可观测与 CI。

- **B1 serve 真实检索**：`wiktor serve` 支持注入 qdrant 向量后端 + HttpEmbedder + 持久化 QUG（env 驱动，缺省回退 mock/确定性，离线可跑）；HTTP `GET /search` 从裸 kernel FTS 切换到与 gRPC 同一 QueryEngine（落 query_logs、返回真实 diagnostics/log_id，响应形状不变）。
- **B2 console 占位清理**：kernel 新增只读 `quality_summary()`/`list_domains()`；console `/api/quality`（五维均值）、`/api/domains`（真实发现）落地；UI 概览页增质量面板与领域列表。
- **B3 备份副本**：litestream file:// 副本接生产（独立路径），对生产库执行恢复演练；SFTP/S3 待用户凭据后切换。
- **B4 可观测 + CI**：Prometheus 抓取 `/metrics`（最小告警）；GitHub Actions 跑 fmt/clippy/test（含 tui feature）。
- **B5 反馈闭环 E2E**：生产链路 search（log_id）→ POST /feedback → analyze → review approve → worker 编译发布 → 复检。

非目标：Raft 多副本、集群分片落地、真实 LLM provider 进生产（env 预留，key 由用户提供）、Grafana/告警通道。

## 2. 现状约束（探查确认）

- `run_server(ServeOptions)` 固定装配 `SearchService<MockVectorStore>` + `DeterministicEmbedder(768)` + qug=None（STEP7-012）；HTTP `GET /search` 走裸 `kernel.search`（log_id=None、diagnostics={}）——反馈事件无法引用 HTTP 检索日志。
- `VectorStore`/`QueryEmbedder` 均 async-trait 对象安全（`Arc<dyn>` 可用）；server crate 不得依赖 wiktor-vector-qdrant（Step10 插件边界，CLI 是装配者）。
- `HttpEmbedder::from_env()` 存在；维度探测模式 = 首次 embed 取 len（`cmd_vector_build` 已用）。
- `/metrics` 已是 Prometheus 文本格式（6 个固定指标）；`page_quality` 五维列（coverage/citation/schema_compliance/density/consistency?/overall）；pages 表含 domain 列。
- litestream 0.5.17 已装且演练脚本 PASS；`litestream.service` ReadWritePaths 已含 `/srv/backup/wiktor/litestream`。

## 3. 决策 D1–D6

| ID | 决策 | 理由与边界 |
|---|---|---|
| D1 | 向量后端/嵌入器/QUG 图由 **CLI（装配者）注入**：`ServeOptions` 增 `vector_store: Option<Arc<dyn VectorStore>>`、`embedder: Option<Arc<dyn QueryEmbedder>>`、`qug: Option<Arc<QugGraph>>`；None 时 server 保持现状（Mock+确定性）。env：`WIKTOR_VECTOR_BACKEND=mock\|qdrant`（默认 mock）+ `WIKTOR_QDRANT_URL/API_KEY`；嵌入沿用 `WIKTOR_EMBEDDING_*`（无则确定性） | 保持 Step10 插件边界（server 不依赖 qdrant 插件）；缺省离线可跑 |
| D2 | server 统一 `QueryEngine<dyn VectorStore>`：engine 装配后进 `ServerState`，gRPC SearchService 与 HTTP /search 共享同一实例；`GET /search` 响应**形状不变**，diagnostics_json/log_id 由占位变真实（行为增强，向后兼容） | 反馈闭环的数据前提；一处装配两形态零漂移 |
| D3 | kernel 新增只读聚合读 API `quality_summary()`（page_quality 五维 + overall 均值与计数）与 `list_domains()`（pages 按 domain 分组计数）；console 端点薄透传 | console 是读面，聚合 SQL 归 kernel；不新增写路径 |
| D4 | litestream 生产副本 = file://（`/srv/backup/wiktor/litestream`，与 DB 不同目录树）；恢复演练对生产库执行；SFTP/S3 为配置切换（runbook 提供步骤），待用户提供凭据 | 同盘副本防误删/损坏，不防整盘——登记局限，外部副本凭据属用户输入 |
| D5 | 可观测 = Prometheus（Debian apt 包）抓取 + 最小告警规则（`up==0`）；配置文件入 `deploy/` | /metrics 已就绪；2G 内存可承受轻量抓取 |
| D6 | CI = GitHub Actions：fmt --check、clippy --workspace --all-targets -D warnings（含 console tui feature）、cargo test --workspace + tui feature；ubuntu-latest + protobuf-compiler | 防回归；runner 环境与本机门禁一致 |

## 4. 批次实现

### B1 — serve 真实检索
- `ServeOptions` 扩展（D1）+ `assemble_search` 支持注入与维度探测（probe 字符串 embed → len）；`run_server` 顺序调整为先装配 engine 再建 ServerState。
- `SearchService<MockVectorStore>` → `SearchService<dyn VectorStore>`（grpc router 签名随动）。
- `http_search.rs`：走 `state.engine.search(&Query)`，响应字段一一对应填充（rewritten→json、diagnostics→json）。
- CLI `wiktor serve`：按 env 组装 store/embedder/qug（QUG 复用 CLI search 的持久化加载路径：domain config enabled → load_active，stale/缺 → fallback）。
- 测试：server 现有 36+ 测试保持绿；新增 HTTP search 返回 log_id/diagnostics 的断言；mock 注入路径测试。

### B2 — console 占位清理
- kernel：`quality_summary()` / `list_domains()`（只读 SQL；consistency 空值 → 均值忽略 NULL 并标注样本数）。
- console：`GET /api/quality`、`GET /api/domains` 真实实现；UI 概览页增"质量五维均值"条形面板与领域列表。
- 测试：空库/有数据两态。

### B3 — 备份副本
- 渲染 `/etc/litestream.yml`（file:// → /srv/backup/wiktor/litestream，sync-interval db 级）→ `systemctl enable --now litestream` → `litestream status` 验证复制 → `restore-drill.sh` 对生产库（隔离恢复）。
- runbook 增补：SFTP/S3 切换步骤 + 局限说明（同盘不防整盘）。

### B4 — 可观测 + CI
- `deploy/prometheus.yml`（scrape 127.0.0.1:8080/metrics）+ `deploy/alerts-wiktor.yml`（up==0）；Debian apt 安装 prometheus，验证 target up 与指标可见。
- `.github/workflows/ci.yml`（D6）。

### B5 — 反馈闭环 E2E + 收口
- `deploy/smoke-feedback.sh`（本机执行）：HTTP search 拿 log_id → POST /feedback（zero_recall 事件）→ `wiktor feedback analyze` → `wiktor feedback review approve` → worker 编译 → 复检检索与行数。
- 生产实测记录 + 偏差双语 + MASTER-PLAN #13 + 提交推送。

## 5. 偏差基准（预案）

不得变更：server 不依赖具体向量插件（装配在 CLI）、HTTP /search 响应形状、kernel 只读聚合不新增写路径、备份副本不引入外部凭据硬编码。实现偏离处记 `STEP13-xxx`（双语同步）。

## 6. 验收标准 A1–A8

| # | 判据 | 可执行结果 |
|---|---|---|
| A1 | spec 双语 | step13-production-ready(.en).md 存在 |
| A2 | serve 装配可注入 | mock 缺省绿 + qdrant/http env 注入生产实测 |
| A3 | HTTP /search 真实日志 | 生产响应 diagnostics_json 非空、log_id 非空 |
| A4 | 反馈闭环生产走通 | smoke-feedback.sh 全 PASS |
| A5 | 副本与恢复 | litestream active + restore drill PASS |
| A6 | 可观测 + CI | Prometheus target up；CI workflow 绿 |
| A7 | console 真实聚合 | /api/quality /api/domains 非占位 |
| A8 | 收口 | workspace test/clippy/fmt 全绿 + 偏差双语 + MASTER-PLAN #13 |

## 7. 生产实测与偏差（2026-09-25）

### 实测记录（2026-09-25）

- **B1/B2 生产实测（Linux Debian 13，二进制 2026-09-25 17:37 构建）**：`wiktor serve --domain …/tech-docs/domain.yaml` 双单元 active；HTTP `GET /search` 返回真实 `log_id` 与完整 `diagnostics_json`（rewrite_status/fts_count/vector_count/rrf_k 等），`query_logs` 落库（A2/A3）；console `/api/quality` `/api/domains` 真实聚合（A7，B2 批次本机测试 + 生产端点实测非占位）。
- **B3 副本与恢复演练（生产库）**：`/etc/litestream.yml` = file:// `/srv/backup/wiktor/litestream`（db 级 sync-interval 1s），`litestream status` = ok（txid 8）；**隔离恢复演练 PASS**——先经 HTTP search 触发探针写入，等待同步后 `litestream restore` 到 `/srv/wiktor/restore/`：integrity_check=ok、foreign_key_check=0、pages=20/query_logs=11/feedback_events=1 与生产逐表对账一致、探针行（max log_id）在副本中，演练产物清理（A5）。
- **B4 可观测 + CI**：apt 安装 Prometheus active，target `wiktor-server`(127.0.0.1:8080) up==1、`prometheus` up==1；`deploy/prometheus.yml` + `alerts-wiktor.yml`（up==0）入库；`.github/workflows/ci.yml` 随本批提交（A6 的 CI 绿以 push 后 Actions 运行为准）。
- **B5 反馈闭环 E2E（`deploy/smoke-feedback.sh`）**：本机（macOS debug 构建 + 临时库）与生产实测全 PASS——baseline search（0 命中，log_id 落库）→ POST /feedback（rate=2）→ `feedback analyze --domain-pack`（zero_recall 盲点浮现 + STEP13-002 实体增强）→ `review approve`（supplemental_compile 排队编译）→ worker 编译发布 → 复检命中 0→1；本机 6 秒闭环（A4）。
- **门禁**：workspace `cargo test` 全绿、clippy `-D warnings` 0 警告、fmt 干净（2026-09-25 收口实测，A8）。

### 实现偏差（STEP13-001..003）

- **STEP13-001（B5，反馈闭环桥接，设计拍板）**：step6 spec 自身存在缺口——L200 规定分析器生成**报告形 subject**（normalized_query/log_ids/次数/latency），L272 却要求 approve 的 supplemental_compile subject 含**五必需字段**（entity_id/source_revision/domain_pack_version/source_json/dependencies_json），两者之间无任何组件桥接；生产 smoke 首跑 approve 即 Validation 失败暴露此缺口（Step11 集成测试是 analyze 后直接 `CompileService::admit`，未经过 approve，因此未覆盖）。拍板：`wiktor feedback analyze` 新增**可选 `--domain-pack <PATH>`**——对 zero_recall 建议按「归一化查询 == 归一化实体 `title` 字段；命中多个取 entity_id 字典序最小」做确定性匹配，回填五字段 subject（保留 normalized_query/log_ids/occurrences/avg_latency_ms 原键供审计）；未命中保持报告形（approve 拒绝 = fail-closed，内容必须先行存在）。注意：增强改变 UNIQUE(domain,action,subject_json) 幂等键成分——同一模式内幂等，混用两种模式可并存报告形/实体形两种建议行（报告形行留作盲点记录）。不新增 kernel 写路径、不改变 approve 校验、审核人工门禁不变。
- **STEP13-002（B5，smoke 查询口径）**：原 B5 smoke 用查询词 `aerospike`——tech-docs 语料（seed-wiki 20 页 + docs.jsonl 120 实体）中不存在该词，任何补编译都无法使命中提升，闭环前提不成立；改为「域包内未编译 document 实体的标题」（`gRPC 快速上手（concept）`），使「盲点 → 补编译 → 命中提升」语义真实可闭合。
- **STEP13-003（B3，恢复演练形态）**：step6/Step9 的 `restore-drill.sh` 自建 disposable 测试库且按 A9 拒绝生产路径，无法直接验证**生产副本链**；Step13 生产演练改为等价隔离流程：在线 `litestream restore` 生产副本 → integrity/外键/逐表行数对账 → 探针行验证 → 清理，全程不触碰生产库本体（spec B3 的「对生产库（隔离恢复）」按此执行）。

<!-- END STEP13 SPEC v1.0 -->
