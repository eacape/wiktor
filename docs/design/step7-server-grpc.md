# Step 7 设计规范：wiktor-server gRPC + HTTP 服务面

> 版本：v1.0（2026-09-23）  
> 上承接：Step 4 `step4-compile-pipeline.md`、Step 6 `step6-feedback-loop.md`、Step 8 `step8-consistency-state-machine.md`  
> 实现对象：`wiktor-builder`；独立验收对象：`test-engineer`  
> 中文为权威设计；英文逐节对应于 `step7-server-grpc.en.md`。

## 1. 目标与非目标

本步把 `wiktor-server` 落成同进程双监听的服务面：tonic gRPC 是主协议，axum HTTP 保留 Step 6 的反馈、健康、指标端点并增加只读搜索端点。服务层只做协议适配、认证、限流、任务调度和依赖装配；core 继续是同步 kernel API，tonic/axum 不进入 core。

目标：

- 提供 `Search`、`Compile`、`QugBuild`、`Review`、`Compatibility`、`Status` 六个 gRPC service。
- Compile admission 只创建 `compile_tasks` 并返回汇总；常驻 `CompileWorker` 异步消费，状态由 `Compile.Status` 查询。
- 统一 HTTP/gRPC 的 API key、方法级授权、domain 隔离、限流和错误语义。
- 保证 QUG 失败显式回退、发布事务、内容哈希、租约 fencing、死信审核和 Step 6 HTTP 行为不变。
- 所有网络请求设置硬预算；同步 kernel 调用通过 `spawn_blocking`，DB mutex/连接 guard 绝不跨 await。

非目标：SSE、WebSocket、TUI、Web UI、分布式限流、多 worker 协议、server-side LLM 查询分析、把 tonic/prost 引入 core。

## 2. 术语与现状约束

### 2.1 已确认事实 X1–X10

| ID | 固定事实 |
|---|---|
| X1 | server 位于 `crates/wiktor-server`，当前 `ServerState{kernel, keys, limiter, metrics, clock, health}`，`build_router` 已提供 Step 6 HTTP 面。 |
| X2 | 已交付 HTTP 是 `POST /feedback`、`GET /health`、`GET /metrics`；错误形状为 `{"error":{"code":"...","message":"..."}}`，必须回归。 |
| X3 | 当前认证解析 `WIKTOR_FEEDBACK_API_KEYS={"domain":"secret"}`，Bearer 精确匹配，key label 为 BLAKE3 截断 hex，原文不得进入日志、指标、响应。 |
| X4 | 新认证变量为 `WIKTOR_API_KEYS={"domain":{"secret":"...","methods":[...]}}`；旧变量仅作为兼容输入，旧 key 默认只授权 `feedback`。新变量存在时优先，不能把两者静默合并为扩大权限。 |
| X5 | `QueryEngine::new(kernel, vector_store, qug, embedder, collection, candidate_multiplier, rrf_k)`，`search(&Query)` 异步返回 `QueryResult`；QUG rewrite 失败由结果显式标记并走 hybrid fallback。 |
| X6 | qdrant 是默认向量后端；离线测试可用 `MockVectorStore` 与 `DeterministicEmbedder`；HTTP embedder 需要 `embedding-http` feature。服务启动时一次装配并共享 `Arc<QueryEngine>`，每请求不重建。 |
| X7 | `SqliteKernel` 已提供 `admit_compile/claim_compile/heartbeat_compile/recover_compile_leases/publish_compile/finish_compile_failure`；Step 4 `PipelineExecutor` 已负责单任务处理、预算、刹车、fencing。 |
| X8 | Step 8 已提供 core `LeaseReaper`（30 秒默认、取消时 drain 一次）和 heartbeat；server 只编排它，不复制 SQL 或状态机。 |
| X9 | QUG 入口是 `build_and_publish_qug_from_bytes`；review 是 `list_reviews/approve_review/ignore_review`，approve 的六动作和事务 fail-closed 语义照搬；compatibility 是只读 `check_domain_compatibility`/`StandardCompatibilityChecker`。 |
| X10 | tonic 0.14.6、prost 0.14.4 已在 lock；目前无 `.proto`/`build.rs`。本机 protoc 36.2 可用；CI 只 push 不构建时不要求 CI 安装 protoc，但任何实际 cargo build 环境必须提供 protoc。 |

## 3. 决策 D1–D12

| ID | 拍板决策 | 理由与边界 | 实现批次 | 验收 |
|---|---|---|---|---|
| D1 | 单一 `proto/wiktor.proto`，包名 `wiktor.v1`；六 service 按能力分组。 | 一个版本面便于客户端生成与兼容；service 边界仍隔离 handler。 | B1 | A1–A3 |
| D2 | 新 API key 采用方法集合；旧变量兼容且仅 `feedback`。 | 扩展认证不破坏 Step 6；fail-closed，方法名不匹配即拒绝。 | B2 | A4–A6 |
| D3 | gRPC 用 interceptor，HTTP 用同一 `AuthConfig`/`authorize` 核心函数。 | 认证语义只实现一次；身份只携带 domain、label、methods。 | B2 | A4–A7 |
| D4 | Search 依赖启动时一次装配的 `Arc<QueryEngine<ConfiguredVectorStore>>`；配置来自环境变量。 | 每请求装配会重复连接和加载 QUG；启动失败可明确暴露配置错误。 | B3 | A8–A11 |
| D5 | Compile RPC 只 admit；worker 复用 `PipelineExecutor` 单任务处理和 core `LeaseReaper`。worker 并发度默认 1、上限配置 8；任务状态查询只读。 | SQLite 写串行化、LLM 成本可控；不复制 Step 4/8 状态机。 | B4 | A12–A17 |
| D6 | QUG Build 是同步 RPC，但只执行一次 bounded build/publish；`force` 原样传递。 | QUG build 不进入 Compile 队列；发布由 core 单事务保护。 | B3 | A18 |
| D7 | Review List/Approve/Ignore 均是同步 RPC；reviewer 必须取认证 label，不接受客户端 reviewer。 | 审计身份不可伪造；approve 六动作继续由 core 事务 fail-closed。 | B3 | A19 |
| D8 | Compatibility Check 只读，不创建任务、不写 review；返回逐项 report。 | 检查可安全重复，真实 compile admission 仍遵守 Step 8 preflight。 | B3 | A20 |
| D9 | 所有 core Error 通过统一映射到 gRPC `Status` 和 HTTP 状态；HTTP 沿用 `error_json`。 | 客户端按稳定 code 处理，不解析英文错误字符串。 | B2 | A21 |
| D10 | `wiktor serve` 加入 CLI，参数 `--listen-grpc`、`--listen-http`、`--db`；旧 `wiktor-server` bin 保留为兼容入口并委托同一 `run_server`。 | 既满足单二进制产品面，也不破坏已有部署脚本。 | B5 | A22 |
| D11 | 双端口同进程；gRPC/HTTP handler 所有同步 kernel 读写走 `spawn_blocking`；worker/reaper 与 handler 共用一个 `Arc<SqliteKernel>`。 | 不跨 await 持锁；SQLite 的写串行化留给 kernel。 | B4 | A23 |
| D12 | metrics 增加 `grpc_requests_total{service,method,code}`、`grpc_inflight`、`compile_worker_*`，label 只用固定 method/code/domain，不用 key。日志只记 label、domain、task/review id，不记 secret、原始字段或 payload。 | 可观测但不泄露认证与知识数据。tonic/prost/build.rs 仅 server crate。 | B2/B6 | A24–A26 |

### 3.1 方法权限映射

| RPC/HTTP | 权限名 |
|---|---|
| `Search.Search`、`GET /search` | `search` |
| `Compile.Admit`、`Compile.Status` | `compile` |
| `QugBuild.Build` | `qug_build` |
| `Review.List`、`Review.Approve`、`Review.Ignore` | `review` |
| `Compatibility.Check` | `compatibility` |
| `Status.Get`、`GET /health`、`GET /metrics` | `status` |
| `POST /feedback` | `feedback` |

认证失败：缺失/错误 Bearer 为 gRPC `UNAUTHENTICATED`（16）、HTTP 401 `UNAUTHENTICATED`；身份有效但无 methods 权限为 gRPC `PERMISSION_DENIED`（7）、HTTP 403 `FORBIDDEN`。domain 不匹配也为 403/`PERMISSION_DENIED`。限流为 gRPC `RESOURCE_EXHAUSTED`（8）和 HTTP 429，带 `Retry-After`。

`WIKTOR_API_KEYS` 的严格形状：`{"milk-tea":{"secret":"s3cret","methods":["search","compile"]}}`。domain、secret 非空且无控制符；methods 非空、只允许上述权限名、重复权限拒绝。旧形状 `{"milk-tea":"s3cret"}` 解析为 `methods:["feedback"]`。两变量同时存在时使用新变量；新变量解析失败直接启动失败。

## 4. 数据模型与 proto 草案

字段号固定后不得复用；新增字段只追加。所有字符串有服务端预算：domain 128、query text 16 KiB、filters JSON 16 KiB、page size 1..=100、task id/review id 为正整数。proto 注释双语。

```proto
syntax = "proto3";
package wiktor.v1;

// 主 gRPC service definitions / 主 gRPC 服务定义。
service Search { rpc Search(SearchRequest) returns (SearchResponse); }
service Compile { rpc Admit(CompileAdmitRequest) returns (CompileAdmitResponse); rpc Status(CompileStatusRequest) returns (CompileStatusResponse); }
service QugBuild { rpc Build(QugBuildRequest) returns (QugBuildResponse); }
service Review { rpc List(ReviewListRequest) returns (ReviewListResponse); rpc Approve(ReviewDecisionRequest) returns (ReviewDecisionResponse); rpc Ignore(ReviewDecisionRequest) returns (ReviewDecisionResponse); }
service Compatibility { rpc Check(CompatibilityCheckRequest) returns (CompatibilityCheckResponse); }
service Status { rpc Get(StatusRequest) returns (StatusResponse); }

message SearchRequest {
  string domain = 1; string text = 2; string filters_json = 3; uint32 top_k = 4;
}
message SearchResponse {
  repeated SearchHit hits = 1; optional RewrittenQuery rewritten = 2;
  bool rewrite_failure = 3; string diagnostics_json = 4; uint64 latency_ms = 5;
  int64 log_id = 6;
}
message SearchHit { string page_id = 1; string entity_id = 2; float score = 3; string title = 4; }
message RewrittenQuery { repeated string expanded_terms = 1; string filters_json = 2; repeated string boost_entity_ids = 3; }

message CompileAdmitRequest {
  string domain = 1; string source_path = 2; string source_format = 3;
  string domain_pack_path = 4; string domain_pack_version = 5; bool force = 6;
  uint32 max_entities = 7; string options_json = 8;
}
message CompileAdmitResponse { CompileTaskSummary summary = 1; }
message CompileTaskSummary { string run_id = 1; uint32 scanned = 2; uint32 admitted = 3; uint32 skipped = 4; repeated int64 task_ids = 5; }
message CompileStatusRequest { int64 task_id = 1; string domain = 2; }
message CompileStatusResponse {
  int64 task_id = 1; string domain = 2; string entity_id = 3; string status = 4;
  string result = 5; uint32 attempt_count = 6; uint32 retry_count = 7;
  uint32 recompile_count = 8; string error_code = 9; int64 updated_at = 10;
  int64 lease_expires_at = 11;
}

message QugBuildRequest {
  string domain = 1; string domain_version = 2; string qug_config_json = 3;
  bytes intents_yaml = 4; string domain_config_json = 5; bool force = 6;
}
message QugBuildResponse { string domain = 1; string source_hash = 2; uint32 edge_count = 3; bool published = 4; bool reused = 5; }

message ReviewListRequest { string domain = 1; string status = 2; uint32 limit = 3; }
message ReviewItem {
  int64 review_id = 1; string domain = 2; string action = 3; string status = 4;
  string subject_json = 5; string reason_json = 6; int64 created_at = 7;
  int64 reviewed_at = 8; string reviewed_by = 9; int64 compile_task_id = 10;
}
message ReviewListResponse { repeated ReviewItem items = 1; }
message ReviewDecisionRequest { int64 review_id = 1; string domain = 2; }
message ReviewDecisionResponse { ReviewItem item = 1; }

message CompatibilityCheckRequest { string domain = 1; string domain_config_json = 2; bool include_artifacts = 3; }
message CompatibilityCheckResponse { bool compatible = 1; CompatibilityReport report = 2; }
message CompatibilityReport { repeated CompatibilityFinding findings = 1; string current_versions_json = 2; }
message CompatibilityFinding { string code = 1; string component = 2; string expected = 3; string actual = 4; string message = 5; }

message StatusRequest {}
message StatusResponse { string schema_version = 1; bool healthy = 2; map<string,uint64> row_counts = 3; string server_version = 4; }
```

映射规则：`filters_json` 必须反序列化为 core `Filters`，不接受未知字段；SearchResponse 的 `diagnostics_json` 使用 core `QueryDiagnostics` canonical JSON。Compile admit 的 source/domain config 由 server 按路径读取并限制在配置根目录；不得从 RPC 传入 compiler/LLM 实现。task summary 只返回 admission 统计和 task ids，不承诺已开始执行。Status 读取单任务快照；不存在返回 `NOT_FOUND`。

## 5. 模块边界

```text
crates/wiktor-server/
  proto/wiktor.proto
  build.rs                         # tonic_build + protoc
  src/
    lib.rs                         # build_router + run_server 装配
    auth.rs                         # AuthConfig, authorize, HTTP middleware, gRPC interceptor
    state.rs                        # ServerState + QueryService/Worker handles
    grpc.rs                         # tonic transport service registration
    http_search.rs                  # GET /search DTO 与 QueryEngine 调用
    services/
      search.rs
      compile.rs
      qug_build.rs
      review.rs
      compatibility.rs
      status.rs
    worker.rs                      # CompileWorker admission queue consumer
    error.rs                        # ErrorCode + core Error mappings
    config.rs                       # env/serve configuration and budgets
    metrics.rs                      # Step6 counters plus gRPC/worker metrics
```

server 只依赖 core 的公开类型和方法；core 不依赖 server/tonic/prost。CLI 改动：新增 `Command::Serve(ServeArgs)`，抽取 `run_server(config)` 到 server crate 的公共装配函数；原 `crates/wiktor-server/src/main.rs` 保留，解析旧 `--listen` 并填入同一 config。若 compile source 读取和 compiler 装配需要 CLI 专属代码，抽出 `wiktor-cli/src/compile.rs` 的纯装配函数到 core 或 server 可依赖的共享模块，禁止 server 依赖 CLI。

## 6. 并发、事务与安全

### 6.1 请求路径

每个 handler 先做 body/字段预算，再认证、方法授权、domain 校验、限流，最后调用服务。HTTP middleware 和 gRPC interceptor 共享 `authorize(identity, method, requested_domain)`；认证上下文只放 `KeyIdentity { domain, label, methods }`。同一 domain 的 key 才能访问该 domain；Status 可不带 domain，使用 key domain。

QueryEngine 是启动时构造的只读编排对象。每次请求 clone `Arc`，await `search`；查询日志由 core 负责。HTTP `GET /search` 参数为 `domain`（必填）、`q`（必填）、`top_k`（默认 5，1..=100）、`filters`（可选 JSON）；返回 `SearchResponse` JSON，字段与 gRPC 一一对应。错误继续用 `error_json`。

### 6.2 CompileWorker

`CompileWorker { kernel, executor_factory, reaper, concurrency, poll_interval, cancel }` 启动时先调用一次 `recover_compile_leases`，再启动 core `LeaseReaper::run_until_cancelled`；worker 自己只负责发现 pending task、claim 和把已 claim 的单任务交给 `PipelineExecutor`。由于现有 executor 的 `run` 是批次编排，server 必须新增一个 core 公开的 `run_claimed_task` 等价窄接口，或把现有单任务处理提取为公共方法；不得在 server 重写 publish/failure SQL。推荐提取：

```rust
pub async fn process_claimed_task(
    &self, lease: TaskLease, cancel: CancellationToken,
) -> Result<CompileTaskOutcome>;
```

worker 并发默认 1；读取 `WIKTOR_COMPILE_WORKERS`，要求 1..=8，否则启动失败。每轮最多 claim `concurrency` 个，按 `(next_attempt_at, task_id)`；无可领取任务 sleep 250ms。预算熔断、指数退避、最大重编译次数、死信和 heartbeat 均由 core 逻辑决定。worker shutdown 顺序为停止接收 → cancel 当前任务 → join → cancel reaper（由 reaper drain 一次）→ 返回。

### 6.3 配置与端口

`WIKTOR_GRPC_ADDR` 默认 `127.0.0.1:50051`，`WIKTOR_HTTP_ADDR` 默认 `127.0.0.1:8080`；CLI flag 优先环境变量。两个 listener 必须先 bind，任一失败则关闭已绑定 listener 并启动失败。HTTP 与 gRPC 使用 `tokio::select!`，任一 server fatal error 触发全局 cancel。健康端点反映 SQLite schema 可读；worker 错误计数和状态进入 metrics，不使健康端点伪装为健康。

### 6.4 错误映射

| core/服务错误 | stable code | gRPC | HTTP |
|---|---|---|---|
| 认证缺失/错误 | `UNAUTHENTICATED` | 16 | 401 |
| 无方法权限/domain 越界 | `PERMISSION_DENIED` | 7 | 403 |
| 参数/JSON/YAML/范围错误 | `INVALID_ARGUMENT` | 3 | 400 |
| 资源不存在 | `NOT_FOUND` | 5 | 404 |
| 状态冲突/重复审核 | `FAILED_PRECONDITION` | 9 | 409 |
| 预算/限流/容量 | `RESOURCE_EXHAUSTED` | 8 | 429 |
| QUG/编译业务失败 | `FAILED_PRECONDITION` | 9 | 422 |
| DB、迁移、qdrant、内部错误 | `INTERNAL` | 13 | 500 |
| worker 尚未可用/关闭中 | `UNAVAILABLE` | 14 | 503 |

错误 message 为固定安全文案；数据库、SQL、secret、原始 metadata、原始 source 不出响应或日志。

## 7. 验收标准 A1–A26

| ID | 离线断言 |
|---|---|
| A1 | `cargo build -p wiktor-server` 生成 `wiktor.v1` Rust 代码，proto 字段号稳定。 |
| A2 | 六 service 全注册；未知 RPC 返回 gRPC `UNIMPLEMENTED`。 |
| A3 | proto request/response 与 core Query/QueryResult、task、review、report JSON 往返无信息丢失。 |
| A4 | 新 key 格式解析；空 secret、重复 domain/secret、未知/重复 methods fail-closed。 |
| A5 | 旧 key 格式只获得 feedback；新变量优先；BLAKE3 label 不含 secret。 |
| A6 | gRPC interceptor 缺 key/错 key 为 16，有 key 无权限为 7；HTTP 同语义 401/403。 |
| A7 | domain 越界、限流、Retry-After 和固定错误形状可断言。 |
| A8 | 启动一次装配 QueryEngine；两次请求共享同一 Arc，不重复加载 QUG/vector。 |
| A9 | gRPC Search 的 fields 与 HTTP `/search` 等价，`rewrite_failure` 保留。 |
| A10 | QUG 无匹配仍返回成功结果且 rewrite_failure=true；不调用 LLM。 |
| A11 | qdrant/embedding 配置缺失按启动错误或显式 deterministic test config 处理，禁止每请求隐式外连。 |
| A12 | Compile.Admit 只写任务、返回 summary；未调用 compiler/LLM。 |
| A13 | worker 能消费一个 pending task，状态最终 succeeded/dead，任务状态仅通过 core API 改变。 |
| A14 | worker 重启可 recover expired lease；reaper cancel 会 drain 一次。 |
| A15 | heartbeat stale 后 publish/failure 被 fencing 拒绝，不覆盖新 owner。 |
| A16 | 并发度 1..=8 生效，超范围启动失败；多 handler 并发 smoke 无死锁。 |
| A17 | Compile.Status 返回任务快照，不存在为 5/404；不泄露 source。 |
| A18 | QUG Build force=false 复用 hash；force=true 走 core force；错误码稳定。 |
| A19 | Review List/Approve/Ignore 可用；reviewer 来自认证 label，客户端 reviewer 被忽略/拒绝。 |
| A20 | approve 的六动作仍由 core fail-closed；重复 approve/ignore 不产生第二次写。 |
| A21 | Compatibility Check 只读，兼容失败返回 report，不创建 task/review。 |
| A22 | `wiktor serve --db --listen-grpc --listen-http` 双端口启动；旧 server bin 仍可启动 HTTP。 |
| A23 | 所有 DB handler 调用经 spawn_blocking；代码审查确认 guard 不跨 await。 |
| A24 | gRPC/worker metrics 计数存在，label 无 raw key/domain payload。 |
| A25 | `POST /feedback`、`GET /health`、`GET /metrics` 的 Step6 oneshot 测试全通过，错误 JSON 不变。 |
| A26 | 无网络、无 qdrant、无外部 LLM 的测试可用 mock vector/deterministic embedder；workspace check 与 server tests 通过。 |

## 8. 实现批次

| 批次 | 内容 | 必须完成的验收 | 可独立验证 |
|---|---|---|---|
| B1 | 加 workspace 依赖、server `build.rs`、proto、生成模块、六 service 空壳和注册。 | A1–A3 | `cargo check -p wiktor-server` |
| B2 | 抽取 `AuthConfig`，兼容两种 env，统一 interceptor/middleware/error/metrics。 | A4–A7、A21、A24 | auth/unit + HTTP oneshot |
| B3 | QueryEngine 装配、gRPC Search、HTTP `/search`、QUG/Compatibility/Status handler。 | A8–A11、A17–A18、A21 | mock vector tower/tonic tests |
| B4 | Compile worker、claimed-task core 窄接口、reaper 接线、Compile Admit/Status。 | A12–A16 | 临时 SQLite 单任务消费 |
| B5 | Review handlers、CLI `serve`、旧 bin 委托、双 listener 生命周期。 | A19–A23 | CLI parse + two-port smoke |
| B6 | 回归、文档注释中英双语、指标、离线测试与偏差登记。 | A24–A26 | workspace test/check |

每批结束必须保持 workspace 可编译；不得先删除旧 HTTP 路由或旧 bin 再补回。

## 9. 风险与预置偏差表

实现偏差必须追加新行，不修改既有行，不用“暂时”“后续再定”掩盖未实现行为。

| ID | 预置偏差/风险 | 处理纪律 |
|---|---|---|
| STEP7-001 | tonic 生成代码依赖 protoc；无 protoc 的环境不能 cargo build。 | 本机用 36.2；CI 仅 push 不构建可不装；任何构建 CI 必须显式安装/缓存 protoc 并在日志报告版本。 |
| STEP7-002 | `PipelineExecutor::run` 是批次接口，worker 需要单任务入口。 | 提取 core 公共窄接口并复用内部逻辑；禁止 server 复制发布/失败 SQL。 |
| STEP7-003 | qdrant/HTTP embedding 是外部依赖。 | 生产启动配置明确；离线测试注入 MockVectorStore/DeterministicEmbedder；禁止请求路径静默切换远程服务。 |
| STEP7-004 | 旧 `WIKTOR_FEEDBACK_API_KEYS` 无 methods。 | 仅映射 `feedback`，不得自动授予 search/status；新变量优先。 |
| STEP7-005 | 单 SQLite 连接写入竞争。 | worker 默认单并发，最大 8；所有 kernel 操作短事务、spawn_blocking、无 guard 跨 await。 |
| STEP7-006 | gRPC trailer 与 HTTP JSON 错误形状不同。 | gRPC 使用稳定 Status code/details；HTTP 永远沿用 `error_json`。 |
| STEP7-007 | server CLI 与旧独立 bin 参数不同。 | 保留旧 bin，统一调用 `run_server`；任何破坏性参数改动需另行登记。 |
| STEP7-008 | API key methods 字符串拼写错误或空集合。 | 启动 fail-closed；新增权限必须同时更新 proto mapping、表、测试和英文 spec。 |
| STEP7-009 | worker shutdown 时任务处于 running。 | cancel 后由 reaper drain/recover；最终由 lease token fencing 裁决，不直接标 succeeded。 |
| STEP7-010 | 外部向量索引滞后 SQLite generation。 | 继续遵循 MASTER-PLAN generation 对齐与可重建契约；Search handler 不自行补写向量。 |
