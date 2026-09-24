# Step 7 Design Specification: wiktor-server gRPC + HTTP Service Surface

> Version: v1.0 (2026-09-23)  
> Continues: Step 4 `step4-compile-pipeline.md`, Step 6 `step6-feedback-loop.md`, Step 8 `step8-consistency-state-machine.md`  
> Implementation owner: `wiktor-builder`; independent acceptance owner: `test-engineer`  
> Chinese is authoritative; this document corresponds section by section to `step7-server-grpc.md`.

## 1. Goals and non-goals

This step delivers `wiktor-server` as one process with two listeners: tonic gRPC is the primary protocol; axum keeps the Step 6 feedback, health, and metrics endpoints and adds a read-only search endpoint. The server layer performs protocol adaptation, authentication, rate limiting, task scheduling, and dependency assembly. Core remains a synchronous kernel API, and tonic/axum do not enter core.

Goals:

- Expose six gRPC services: `Search`, `Compile`, `QugBuild`, `Review`, `Compatibility`, and `Status`.
- Make Compile admission create only `compile_tasks` and return a summary; a resident `CompileWorker` consumes tasks asynchronously and `Compile.Status` polls their state.
- Unify API-key handling, method authorization, domain isolation, rate limiting, and error semantics across HTTP and gRPC.
- Preserve explicit QUG fallback, publish transactions, content hashes, lease fencing, dead-letter review, and all Step 6 HTTP behavior.
- Enforce hard request budgets; wrap synchronous kernel calls with `spawn_blocking`; never hold a DB mutex or connection guard across an await.

Non-goals: SSE, WebSocket, TUI, Web UI, distributed rate limiting, a multi-worker protocol, server-side LLM query analysis, and adding tonic/prost to core.

## 2. Terms and current constraints

### 2.1 Confirmed facts X1–X10

| ID | Fixed fact |
|---|---|
| X1 | The server is `crates/wiktor-server`; its current `ServerState` contains `kernel`, `keys`, `limiter`, `metrics`, `clock`, and `health`; `build_router` already provides the Step 6 HTTP surface. |
| X2 | Delivered HTTP routes are `POST /feedback`, `GET /health`, and `GET /metrics`; the error shape is `{"error":{"code":"...","message":"..."}}` and must regress cleanly. |
| X3 | Current auth parses `WIKTOR_FEEDBACK_API_KEYS={"domain":"secret"}`; Bearer matching is exact; the key label is truncated BLAKE3 hex; raw keys must never enter logs, metrics, or responses. |
| X4 | The new variable is `WIKTOR_API_KEYS={"domain":{"secret":"...","methods":[...]}}`; the old variable is compatibility input only and old keys authorize `feedback` only. If the new variable exists, it wins; the two sources must not be silently merged to enlarge permissions. |
| X5 | `QueryEngine::new(kernel, vector_store, qug, embedder, collection, candidate_multiplier, rrf_k)` and async `search(&Query) -> QueryResult` are public. QUG rewrite failure is explicit and uses hybrid-search fallback. |
| X6 | Qdrant is the default vector backend; offline tests may use `MockVectorStore` and `DeterministicEmbedder`; HTTP embedding requires the `embedding-http` feature. The service assembles once at startup and shares `Arc<QueryEngine>`. |
| X7 | `SqliteKernel` exposes admission, claim, heartbeat, lease recovery, publish, and failure methods; Step 4 `PipelineExecutor` owns single-task processing, budgets, brakes, and fencing. |
| X8 | Step 8 provides core `LeaseReaper` with a 30-second default and one shutdown drain; the server orchestrates it and does not duplicate SQL or the state machine. |
| X9 | QUG uses `build_and_publish_qug_from_bytes`; reviews use `list_reviews/approve_review/ignore_review`, with the six actions and fail-closed transaction semantics; compatibility uses read-only `check_domain_compatibility`/`StandardCompatibilityChecker`. |
| X10 | tonic 0.14.6 and prost 0.14.4 are already locked; no `.proto` or `build.rs` exists. Local protoc is 36.2. CI that only pushes does not need protoc, but every environment that actually runs cargo build must provide it. |

## 3. Decisions D1–D12

| ID | Decision | Reason and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Use one `proto/wiktor.proto`, package `wiktor.v1`, with six capability-oriented services. | One version surface simplifies generated clients while service handlers remain isolated. | B1 | A1–A3 |
| D2 | API keys carry a method set; legacy keys remain valid but authorize `feedback` only. | Extends auth without breaking Step 6; fail closed on method mismatch. | B2 | A4–A6 |
| D3 | Use a gRPC interceptor and the same `AuthConfig`/`authorize` function in HTTP middleware. | Authentication semantics are implemented once; identity contains only domain, label, and methods. | B2 | A4–A7 |
| D4 | Search uses one startup-built `Arc<QueryEngine<ConfiguredVectorStore>>`; configuration comes from environment variables. | Per-request construction would reload connections and QUG; startup makes configuration failures explicit. | B3 | A8–A11 |
| D5 | Compile RPC only admits; the worker reuses `PipelineExecutor` single-task processing and core `LeaseReaper`. Default concurrency is 1, configurable up to 8; status is read-only. | SQLite writes stay controlled and LLM cost remains bounded; Step 4/8 state machines are not copied. | B4 | A12–A17 |
| D6 | QUG Build is synchronous and performs one bounded build/publish; `force` is passed through unchanged. | QUG build does not enter the Compile queue; core protects publication transactionally. | B3 | A18 |
| D7 | Review List/Approve/Ignore are synchronous; reviewer is taken from authenticated identity, never from the client. | Audit identity cannot be forged; core keeps the six-action fail-closed transaction. | B3 | A19 |
| D8 | Compatibility Check is read-only and writes neither tasks nor reviews; it returns an itemized report. | Checks are safely repeatable; real admission still performs Step 8 preflight. | B3 | A20 |
| D9 | Map every core Error through one mapping to gRPC `Status` and HTTP status; HTTP keeps `error_json`. | Clients consume stable codes rather than English error strings. | B2 | A21 |
| D10 | Add CLI `wiktor serve` with `--listen-grpc`, `--listen-http`, and `--db`; retain the old `wiktor-server` binary as a compatibility wrapper over the same `run_server`. | Provides the single-binary product surface without breaking existing deployments. | B5 | A22 |
| D11 | Use two ports in one process; every synchronous DB handler call uses `spawn_blocking`; worker, reaper, and handlers share one `Arc<SqliteKernel>`. | No lock crosses await; SQLite serialization remains in core. | B4 | A23 |
| D12 | Add `grpc_requests_total{service,method,code}`, `grpc_inflight`, and `compile_worker_*`; labels use only fixed method/code/domain values and never keys. Logs contain labels and IDs, never secrets, raw fields, or payloads. | Adds observability without leaking auth or knowledge data. tonic/prost/build.rs stay in server. | B2/B6 | A24–A26 |

### 3.1 Method authorization map

| RPC/HTTP | Permission |
|---|---|
| `Search.Search`, `GET /search` | `search` |
| `Compile.Admit`, `Compile.Status` | `compile` |
| `QugBuild.Build` | `qug_build` |
| `Review.List`, `Review.Approve`, `Review.Ignore` | `review` |
| `Compatibility.Check` | `compatibility` |
| `Status.Get`, `GET /health`, `GET /metrics` | `status` |
| `POST /feedback` | `feedback` |

Missing or invalid Bearer credentials return gRPC `UNAUTHENTICATED` (16) and HTTP 401 `UNAUTHENTICATED`. A valid identity without the method returns gRPC `PERMISSION_DENIED` (7) and HTTP 403 `FORBIDDEN`. Domain mismatch has the same result. Rate limiting returns gRPC `RESOURCE_EXHAUSTED` (8) and HTTP 429 with `Retry-After`.

Strict new-key shape: `{"milk-tea":{"secret":"s3cret","methods":["search","compile"]}}`. Domains and secrets are non-empty and control-character-free; methods are non-empty, allowed names only, and duplicates are rejected. The legacy shape `{"milk-tea":"s3cret"}` becomes `methods:["feedback"]`. If both variables exist, the new variable is used; a new-variable parse failure aborts startup.

## 4. Data model and proto draft

Field numbers are fixed once released and must never be reused; new fields are appended. Server budgets are domain 128 chars, query text 16 KiB, filter JSON 16 KiB, page size 1..=100, and positive task/review IDs. Comments in the actual proto must be bilingual.

```proto
syntax = "proto3";
package wiktor.v1;

service Search { rpc Search(SearchRequest) returns (SearchResponse); }
service Compile { rpc Admit(CompileAdmitRequest) returns (CompileAdmitResponse); rpc Status(CompileStatusRequest) returns (CompileStatusResponse); }
service QugBuild { rpc Build(QugBuildRequest) returns (QugBuildResponse); }
service Review { rpc List(ReviewListRequest) returns (ReviewListResponse); rpc Approve(ReviewDecisionRequest) returns (ReviewDecisionResponse); rpc Ignore(ReviewDecisionRequest) returns (ReviewDecisionResponse); }
service Compatibility { rpc Check(CompatibilityCheckRequest) returns (CompatibilityCheckResponse); }
service Status { rpc Get(StatusRequest) returns (StatusResponse); }

message SearchRequest { string domain = 1; string text = 2; string filters_json = 3; uint32 top_k = 4; }
message SearchResponse { repeated SearchHit hits = 1; optional RewrittenQuery rewritten = 2; bool rewrite_failure = 3; string diagnostics_json = 4; uint64 latency_ms = 5; int64 log_id = 6; }
message SearchHit { string page_id = 1; string entity_id = 2; float score = 3; string title = 4; }
message RewrittenQuery { repeated string expanded_terms = 1; string filters_json = 2; repeated string boost_entity_ids = 3; }

message CompileAdmitRequest { string domain = 1; string source_path = 2; string source_format = 3; string domain_pack_path = 4; string domain_pack_version = 5; bool force = 6; uint32 max_entities = 7; string options_json = 8; }
message CompileAdmitResponse { CompileTaskSummary summary = 1; }
message CompileTaskSummary { string run_id = 1; uint32 scanned = 2; uint32 admitted = 3; uint32 skipped = 4; repeated int64 task_ids = 5; }
message CompileStatusRequest { int64 task_id = 1; string domain = 2; }
message CompileStatusResponse { int64 task_id = 1; string domain = 2; string entity_id = 3; string status = 4; string result = 5; uint32 attempt_count = 6; uint32 retry_count = 7; uint32 recompile_count = 8; string error_code = 9; int64 updated_at = 10; int64 lease_expires_at = 11; }

message QugBuildRequest { string domain = 1; string domain_version = 2; string qug_config_json = 3; bytes intents_yaml = 4; string domain_config_json = 5; bool force = 6; }
message QugBuildResponse { string domain = 1; string source_hash = 2; uint32 edge_count = 3; bool published = 4; bool reused = 5; }
message ReviewListRequest { string domain = 1; string status = 2; uint32 limit = 3; }
message ReviewItem { int64 review_id = 1; string domain = 2; string action = 3; string status = 4; string subject_json = 5; string reason_json = 6; int64 created_at = 7; int64 reviewed_at = 8; string reviewed_by = 9; int64 compile_task_id = 10; }
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

Mapping rules: `filters_json` must deserialize to core `Filters` with unknown fields rejected. `diagnostics_json` is canonical core `QueryDiagnostics`. Server reads source and domain configuration paths only under the configured root and never accepts compiler/LLM implementations from RPC. Compile admission returns admission statistics and task IDs only; it does not promise that execution has begun. Missing task status is `NOT_FOUND`.

## 5. Module boundaries

```text
crates/wiktor-server/
  proto/wiktor.proto
  build.rs
  src/
    lib.rs                         # build_router and run_server assembly
    auth.rs                         # AuthConfig, authorize, HTTP middleware, gRPC interceptor
    state.rs                        # ServerState and QueryEngine/Worker handles
    grpc.rs                         # tonic transport registration
    http_search.rs                  # GET /search DTO and QueryEngine call
    services/                       # search, compile, qug_build, review, compatibility, status
    worker.rs                       # CompileWorker pending-task consumer
    error.rs                        # ErrorCode and core Error mappings
    config.rs                       # environment/serve config and budgets
    metrics.rs                      # Step6 plus gRPC/worker metrics
```

The server depends only on core public types and methods; core does not depend on server/tonic/prost. CLI adds `Command::Serve(ServeArgs)` and calls the server crate's public `run_server(config)`. The old `crates/wiktor-server/src/main.rs` remains, parses legacy `--listen`, and fills the same config. If source reading or compiler assembly is currently CLI-specific, extract pure assembly into core or a shared module; server must never depend on CLI.

## 6. Concurrency, transactions, and security

Every handler enforces body/field budgets, then authentication, method authorization, domain validation, and rate limiting before service execution. HTTP middleware and gRPC interceptors share `authorize(identity, method, requested_domain)`. Auth context contains only `KeyIdentity { domain, label, methods }`. A key can access only its domain; Status may omit domain and uses the key domain.

The QueryEngine is assembled once at startup. Requests clone its `Arc` and await `search`; core writes query logs. HTTP `GET /search` takes required `domain` and `q`, optional `top_k` defaulting to 5 within 1..=100, and optional JSON `filters`; its JSON response maps one-to-one to gRPC SearchResponse. Errors continue through `error_json`.

`CompileWorker { kernel, executor_factory, reaper, concurrency, poll_interval, cancel }` recovers leases once at startup, then starts core `LeaseReaper::run_until_cancelled`. It discovers pending tasks, claims them, and passes each claimed task to `PipelineExecutor`. Because the current executor is batch-oriented, expose a narrow core single-task method or extract the existing method; do not reproduce publish/failure SQL in server:

```rust
pub async fn process_claimed_task(
    &self, lease: TaskLease, cancel: CancellationToken,
) -> Result<CompileTaskOutcome>;
```

Worker concurrency defaults to 1 and reads `WIKTOR_COMPILE_WORKERS`, which must be 1..=8. Each cycle claims at most `concurrency` tasks ordered by `(next_attempt_at, task_id)`; with no task it sleeps 250 ms. Budgets, exponential backoff, recompilation limits, dead letters, and heartbeat remain core decisions. Shutdown stops admission, cancels active tasks, joins them, cancels the reaper so it drains once, and returns.

`WIKTOR_GRPC_ADDR` defaults to `127.0.0.1:50051`; `WIKTOR_HTTP_ADDR` defaults to `127.0.0.1:8080`; CLI flags override environment values. Both listeners bind before serving; a bind failure closes the already-bound listener and aborts startup. HTTP and gRPC run under `tokio::select!`; a fatal server error cancels the process. Health means SQLite schema is readable. Worker errors are observable and do not make health falsely green.

### 6.1 Error mapping

| Core/service error | Stable code | gRPC | HTTP |
|---|---|---:|---:|
| Missing/invalid auth | `UNAUTHENTICATED` | 16 | 401 |
| Missing method permission/domain boundary | `PERMISSION_DENIED` | 7 | 403 |
| Invalid parameter/JSON/YAML/range | `INVALID_ARGUMENT` | 3 | 400 |
| Missing resource | `NOT_FOUND` | 5 | 404 |
| State conflict/repeated review | `FAILED_PRECONDITION` | 9 | 409 |
| Budget/rate/capacity | `RESOURCE_EXHAUSTED` | 8 | 429 |
| QUG/compile business failure | `FAILED_PRECONDITION` | 9 | 422 |
| DB, migration, qdrant, internal failure | `INTERNAL` | 13 | 500 |
| Worker unavailable/shutting down | `UNAVAILABLE` | 14 | 503 |

Messages are fixed and safe. Database details, SQL, secrets, raw metadata, and raw source never leave the process through response or logs.

## 7. Acceptance criteria A1–A26

| ID | Offline assertion |
|---|---|
| A1 | `cargo build -p wiktor-server` generates `wiktor.v1` Rust code with stable proto field numbers. |
| A2 | All six services register; unknown RPC returns `UNIMPLEMENTED`. |
| A3 | Proto requests/responses round-trip core Query/QueryResult, task, review, and report data without loss. |
| A4 | New key format parses; empty secret, duplicate domain/secret, unknown or duplicate methods fail closed. |
| A5 | Legacy keys receive only feedback; new variable wins; BLAKE3 labels contain no secret. |
| A6 | Missing/wrong gRPC credentials return 16; missing permission returns 7; HTTP gives 401/403 with the same semantics. |
| A7 | Domain boundary, rate limit, Retry-After, and fixed error shape are assertable. |
| A8 | QueryEngine is assembled once and requests share one Arc without reloading QUG/vector state. |
| A9 | gRPC Search and HTTP `/search` have equivalent fields and retain `rewrite_failure`. |
| A10 | An unmatched QUG returns a successful hybrid result with `rewrite_failure=true` and no LLM call. |
| A11 | Missing qdrant/embedding configuration is a startup error or explicit deterministic test config; requests never silently make remote calls. |
| A12 | Compile.Admit writes tasks and returns a summary only; compiler/LLM is not called. |
| A13 | Worker consumes one pending task; terminal state is succeeded/dead; only core APIs mutate task state. |
| A14 | Restart recovers an expired lease; reaper cancellation performs one drain. |
| A15 | After a stale heartbeat, publish/failure fencing rejects the old owner. |
| A16 | Concurrency 1..=8 works; out-of-range startup fails; concurrent handlers have no deadlock. |
| A17 | Compile.Status returns a task snapshot; missing task is 5/404; source is not leaked. |
| A18 | QUG Build reuses the hash with force=false and passes force=true to core; codes are stable. |
| A19 | Review List/Approve/Ignore work; reviewer comes from auth label and client reviewer is ignored or rejected. |
| A20 | Six approve actions remain core fail-closed; repeat decisions do not create a second write. |
| A21 | Compatibility Check is read-only and creates no task/review on incompatibility. |
| A22 | `wiktor serve --db --listen-grpc --listen-http` starts both ports; old server binary still starts HTTP. |
| A23 | All DB handler calls use spawn_blocking; code review finds no guard across await. |
| A24 | gRPC/worker metrics exist and labels contain no raw key or payload. |
| A25 | Step6 `POST /feedback`, `GET /health`, and `GET /metrics` oneshot tests pass with unchanged JSON errors. |
| A26 | Tests run without network, qdrant, or external LLM using mock vector/deterministic embedder; workspace checks pass. |

## 8. Implementation batches

| Batch | Work | Required acceptance | Independent verification |
|---|---|---|---|
| B1 | Add workspace dependencies, server `build.rs`, proto, generated module, six service shells, and registration. | A1–A3 | `cargo check -p wiktor-server` |
| B2 | Extract `AuthConfig`, support both env formats, unify interceptor/middleware/error/metrics. | A4–A7, A21, A24 | auth/unit and HTTP oneshot tests |
| B3 | Assemble QueryEngine; implement Search, `/search`, QUG, Compatibility, and Status handlers. | A8–A11, A17–A18, A21 | mock-vector tower/tonic tests |
| B4 | Add worker, narrow claimed-task core API, reaper wiring, Compile Admit/Status. | A12–A16 | temporary SQLite single-task consumption |
| B5 | Review handlers, CLI `serve`, old-binary wrapper, and two-listener lifecycle. | A19–A23 | CLI parsing and two-port smoke |
| B6 | Regression, bilingual comments, metrics, offline tests, and deviation entries. | A24–A26 | workspace test/check |

Every batch must leave the workspace compilable. Do not remove old HTTP routes or the old binary before the replacement is wired.

## 9. Risk and predeclared deviation table

Implementation deviations append rows; they do not rewrite existing rows or hide missing behavior behind “temporary” or “later”.

| ID | Predeclared deviation/risk | Discipline |
|---|---|---|
| STEP7-001 | Generated tonic code needs protoc; environments without it cannot cargo build. | Use local 36.2; push-only CI may omit it; any build CI must install/cache protoc and report its version. |
| STEP7-002 | `PipelineExecutor::run` is batch-oriented while the worker needs one task. | Extract a public narrow core API and reuse internals; never copy publish/failure SQL into server. |
| STEP7-003 | qdrant/HTTP embedding are external dependencies. | Make production configuration explicit; inject mock vector/deterministic embedder offline; never silently switch to remote calls per request. |
| STEP7-004 | Legacy `WIKTOR_FEEDBACK_API_KEYS` has no methods. | Map only to `feedback`; never grant search/status automatically; new variable wins. |
| STEP7-005 | One SQLite connection can contend on writes. | Default worker concurrency 1, maximum 8; all kernel calls are short, spawn_blocking transactions with no guard across await. |
| STEP7-006 | gRPC trailers and HTTP JSON errors differ. | Use stable gRPC Status code/details; HTTP always uses `error_json`. |
| STEP7-007 | CLI and old standalone server flags differ. | Keep the old binary and route both through `run_server`; breaking flag changes require a new entry. |
| STEP7-008 | API method spelling or empty sets are invalid. | Fail startup closed; every new permission updates proto mapping, tables, tests, and this English spec. |
| STEP7-009 | Worker shutdown can leave a task running. | Cancel then let reaper drain/recover; lease-token fencing decides the terminal owner; never mark succeeded directly. |
| STEP7-010 | External vector index may lag SQLite generation. | Follow the MASTER-PLAN generation-alignment and rebuildability contract; Search does not repair vectors itself. |

### Actual implementation deviations (registered 2026-09-23, after B4/B5 landed)

| ID | Deviation | Handling |
|---|---|---|
| STEP7-011 | The server-side Compile.Admit/Worker compiler is pinned to MockCompiler (the offline acceptance baseline). | B4 is explicitly offline: `assemble_source` is a server-local assembly entry (DOMAIN.yaml → policy/schema/ctx, semantics identical to the CLI, no dependency on the CLI crate); the real-LLM provider assembly path (via `--provider`) stays on the serve-layer feature switch and does not affect compiler-free services such as search/status/review. |
| STEP7-012 | The serve search assembly currently uses MockVectorStore + QUG=None. | Offline acceptance injects mocks (STEP7-003 discipline); the real qdrant/HTTP-embedding assembly reuses the CLI `vector build` env path and is not re-wired at the serve layer; the request path never silently switches remote services. |
| STEP7-013 | The `process_claimed_task` narrow-interface signature carries no CancellationToken (spec STEP7-002 suggested one). | Cancellation semantics rest with the upper worker (outer `spawn` cancel + reaper drain); the core narrow interface stays minimal; a task's terminal state is decided by the publish/failure kernel fencing CAS, never bypassed by cancellation. |
| STEP7-014 | Legacy `WIKTOR_SERVER` env: the old bin's gRPC address/domain pack come via `WIKTOR_GRPC_ADDR` / `WIKTOR_DOMAIN_PACK` / `WIKTOR_SOURCE_PATH`, not a new --grpc flag. | The old bin's `--db --listen` surface is preserved (STEP7-007); all extensions go through env vars; the CLI `serve` exposes explicit flags. |
