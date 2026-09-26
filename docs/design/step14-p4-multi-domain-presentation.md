# Step 14 (P4) 设计规范：单进程多领域 serve + 展示层收敛

> 版本：v1.0（2026-09-26）
> 权威依据：`docs/MASTER-PLAN.md` §16 MVP（单进程、单二进制、单 SQLite 库）、§5.4 查询全链路、§17 落地依赖
> 实现对象：`wiktor-server` serve 多领域装配 + `wiktor-console`/TUI 多域数据面（主模型实现，本文档登记规范）

## 1. 背景与目标

现状 serve 是**单领域**：`ServeOptions.domain_pack` 是 `Option` 单值 → 一个进程 = 一个 db + 至多一个 domain pack + 一个 compile worker + 一个 query engine + 一个 qdrant collection。P4 两半：

1. **单进程多领域 serve**：一个 serve 进程同时服务多个 domain（各自独立的检索面与编译面、共享单个 SQLite 库文件）。
2. **展示层收敛**：console（Web）/ TUI 去掉 `milk-tea` 硬编码，提供多域数据面与 domain 选择，收敛到同一套读面。

## 2. 现状核实（设计前提）

- **serve 装配单领域**：`serve.rs::assemble_state_and_search` 从单个 `domain_pack` 读 `config.name`（解析失败静默回退 `"default"`，serve.rs:128-136），建**单个** engine + `ensure_collection(&domain_name, ...)`（serve.rs:155-158）。`run_server` 至多起 **1 个** `CompileWorker`（serve.rs:78-92）。
- **DB 检索面已列级多域**：`pages`/`query_logs`/`feedback_events`/`review_queue`/`feedback_rejections`/`qug_builds` 都有 domain 列；`kernel.search` 按 domain 过滤、`list_domains()` 已存在（sqlite.rs:378）。`SqliteKernel` 构造按 db 路径，domain 是方法参数/表内列，**无 domain 构造参数**。
- **编译面无 domain 列**（深水区）：`facts`/`fact_refs`/`page_quality`/`page_sections`/`compile_tasks`/`qug_edges` 无 domain 列，依赖 entity_id 全局唯一（`review_domain_of` 从 `domain:type:slug` 前缀反解）。
  **关键缓解**：compile claim 是**按 `task_ids` 数组**（构造时传入该 run 的 admission 集）领取，**不是按 domain 列全局扫**（compile_store.rs `claim_in_transaction`，`SQL_CLAIM_NEXT` 用 json_each 展开传入的 task_ids）。因此**每 domain 一个 worker、各自用自己 domain.yaml 只 admit 自己的源**时，task 天然按域隔离，**不需要给 compile_tasks 加 domain 列**。这是本文档敢于"两半都做"的依据。
- **qdrant 按域命名**：`ensure_collection(&domain_name, ...)`，collection 名 = domain_name，天然隔离。
- **请求已自带 domain 且经 key 校验**：HTTP `/search`、`POST /feedback`、gRPC Search 都要求 `params.domain == authed.domain` 否则 403；`Query.domain = Some(params.domain)`。
- **展示层 hardcode**：`wiktor-console/src/lib.rs:64-67` 与 `tui/state.rs:177-193` 都硬编码 `ensure_collection("milk-tea",...)` + `QueryEngine::new(...,"milk-tea",...)`；Web console 面板是全库聚合、仅 `/api/search` 可带可选 domain；`/api/domains` 已能发现多 domain。

## 3. 决策点

### D-P4-1：多 domain 的装配形态
`ServeOptions.domain_pack` 从 `Option<PathBuf>`（单值）改为 **`Vec<PathBuf>`（多值，可空）**；空 → 无 compile worker、无检索面（纯监督/状态服务）。兼容：旧 `--domain <path>` 单值 → 合并进 Vec。CLI `wiktor serve --domain` 改为可重复 flag；旧 bin 的 `WIKTOR_DOMAIN_PACK` env 支持以路径分隔符（`:`）分隔多个 pack。

### D-P4-2：多 engine 的组织
`ServerState.engine` 从单 `Arc<QueryEngine<dyn VectorStore>>` 改为 **`HashMap<String, Arc<QueryEngine<dyn VectorStore>>>`**（key = domain_name）。每个 domain 独立装配 engine（自己的 domain 名、candidate_multiplier、QUG、collection 名）。向量 store/embedder 共享连接实例，collection 名不同隔离。

### D-P4-3：请求路由（分发层）
gRPC `SearchService` 与 HTTP `/search` **按请求 `domain` 查 map**：命中 → 调对应 engine；未服务的 domain → `NOT_FOUND`（区别于认证 403：key 授权通过但该域未装配）。`ServerState` 暴露 `engine_for(domain) -> Option<Arc<QueryEngine<dyn VectorStore>>>`。`/api/overview` 等聚合读面返回多域汇总。

### D-P4-4：编译 worker 每域一个
`run_server` 遍历 domain_packs，为**每个** pack 建一个 `CompileWorker::from_domain_pack` 并 `.spawn`；collect 成 Vec 在退出时全部 join。**安全依据见 §2 深水区缓解**（claim 按 task_ids 隔离）。

### D-P4-5：展示层收敛
- **去 milk-tea 硬编码**：`wiktor-console`/`tui` 不再固定 `"milk-tea"`；改为从 `SqliteKernel::list_domains()` 发现已编译 domain，多 domain 各建 engine（或共享 kernel，仅 hyper 表用 kernel）。
- **domain 选择器**：Web console 顶部加全局 domain 下拉（数据来自 `/api/domains`）；`/api/search` 的 domain 输入与其联动；TUI 加 domain 选择（快捷键），`run_search` 传选中 domain。
- **数据面统一**：概览/任务/审阅/检索面板全部走 kernel 聚合读面 + 可选 domain 过滤，Web 与 TUI 共享同源（STEP12 B1"共享数据面"延续）。

## 4. 涉及文件清单

| 层 | 文件 | 改动 |
|---|---|---|
| serve | `crates/wiktor-server/src/serve.rs` | `ServeOptions.domain_pack`→Vec；`assemble_state_and_search` 逐域建 engine 收集 map；`run_server` 多 worker spawn/join |
| server state | `crates/wiktor-server/src/state.rs` | `engine` 字段改 map；增 `engine_for`；`ServerState::new` 签名改 map |
| gRPC | `crates/wiktor-server/src/services/search.rs` | `SearchService` 持 map，按 req.domain 分发 |
| HTTP | `crates/wiktor-server/src/http_search.rs` | `/search` 按 params.domain 分发；`/api/domains` 返回已服务域 |
| CLI | `crates/wiktor-cli/src/main.rs` | `wiktor serve --domain` 可重复；`WIKTOR_DOMAIN_PACK` 分隔；旧 bin 同步 |
| console | `crates/wiktor-console/src/lib.rs` | 去 milk-tea hardcode，多域 engine map + domain 选择器后端 |
| TUI | `crates/wiktor-console/src/tui/state.rs` + `mod.rs` | 去 milk-tea hardcode，多域读面 + domain 选择 |
| Web console | `docs/console_ui/code.html` | 顶部 domain 下拉，与 /api/search 联动 |

## 5. 边界与不变量

- **编译隔离**：每域 worker 只 admit 自己 domain.yaml 的源；claim 按各自 task_ids；entity_id 带 `domain:` 前缀全局唯一 → 不新增 domain 列。
- **检索隔离**：请求 domain 必须 = key domain（403，不变）；必须命中已装配 engine（否则 NOT_FOUND，新增）。
- **离线基线**：无注入 + 空 domain_packs 时 server 可起，作为纯状态/监督服务；有 pack 时每域走 mock/deterministic 缺省。
- **幂等/确定性**：多域不改变单域 engine 行为；尚无域的任务/publish 语义不变。
- **兼容**：单域用法（旧 `--domain`/`WIKTOR_DOMAIN_PACK` 单值）行为与现状一致。

## 6. 分阶段落地

- **阶段 A（内核 serve 多域）**：ServeOptions/state/engine map + ServeService/HTTP 分发 + CLI 多 flag + 单/多 worker。本阶段完成即"单进程多领域 serve"。
- **阶段 B（展示层收敛）**：console/TUI 去 hardcode + domain 发现/选择器 + code.html 下拉。
- 两阶段独立可验收；先后落地，各跑门禁全绿。实现偏差事后登记本文档（如 STEP 惯例）→ 追加变更记录。