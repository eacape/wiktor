# Step 6 Design Specification: Feedback Loop and `POST /feedback`

> Version: v1.0 (2026-09-22)  
> Predecessors: Step 4 `step4-compile-pipeline.md`, Step 5 `step5-qug-build.md`, Step 3 QueryEngine  
> Implementation owner: `wiktor-builder`; independent acceptance owner: `test-engineer`  
> This is the authoritative English counterpart of `step6-feedback-loop.md`. Domain literals remain unchanged.

## 1. Goals and non-goals

Step 6 implements the data and control planes of the compile–retrieve feedback loop: an upper-layer application returns adoption signals through an authenticated HTTP API; the analyzer aggregates query logs and feedback into three blind-spot classes; suggestions enter a manual review queue and only an explicit approval may admit a task into the existing `compile_tasks` pipeline. This step also establishes the minimal HTTP surface of `wiktor-server`; #7 owns the extension surface.

Goals:

- Create `wiktor-feedback` (analysis, reports, review queue access) and `wiktor-server` (axum HTTP). Both may depend on core; core must not depend on server.
- Add migration 0005 for feedback facts, review queue, rejection accounting, and query-log empty-filter/retry state.
- Implement `POST /feedback`, `GET /health`, and `GET /metrics`; authentication, domain scope, rate limiting, idempotency, and input budgets are hard contracts.
- Add `wiktor feedback analyze|list|review`, retaining exit codes 0/1/2/3/4.

Non-goals: gRPC/tonic, automatic compilation or domain-pack mutation, review UI, LLM analysis, distributed rate limiting, and automatic QUG rule generation. Feedback analysis must not call `admit_compile`; only an explicit human approval command may do so.

## 2. Terms and existing constraints

- **query log**: a row in 0001 `query_logs`: `query_text/query_json/rewritten_json/rewrite_failure/hit_count/latency_ms/timestamp`.
- **filter-empty**: fact-plane pushdown yields no candidates. Per §5.4, one relaxation/retry must happen first; only a second empty result is a knowledge blind spot. The current QueryEngine returns immediately and must be corrected in Batch 3.
- **feedback event**: `click`, `adopt`, or `rate` from a client for a query log; `hit` is not an API event because hit count is already in the query log.
- **review item**: an analyzer suggestion that has not changed compilation task state; `review_queue` is the control-plane fact.
- **tenant**: for MVP, the `domain` is the tenant boundary. An API key is bound to one or more domains; request-body `domain` must be allowed by that key.
- **quarantine queue**: an over-budget request is not inserted into the normal feedback table. It increments `feedback_rejections` and returns 413, providing the minimal auditable implementation of reliability contract #7.

Diesel/SQLite kernel APIs remain synchronous. Async exists in QueryEngine and CLI orchestration; database locks must never cross an await. SQLite runs in WAL mode. All writes are short transactions with bound parameters, and all errors propagate.

## 3. Decisions D1–D12

| ID | Decision | Rationale | Rejected alternative |
|---|---|---|---|
| D1 | Create `wiktor-feedback` and `wiktor-server`; CLI calls library functions and server constructors. Server exposes `POST /feedback`, `GET /health`, `GET /metrics`. | Matches MASTER-PLAN §9 and separates HTTP lifecycle from CLI state; plugins depend only on core. | Embedding the server in CLI couples networking, runtime, and command state and prevents reuse of a service library. |
| D2 | Use Tokio multi-thread runtime; `axum = "0.7"`, `tower = "0.5"` only for the minimal existing middleware layer; do not add tonic. | Axum 0.7 fits Rust 1.85 and Tokio 1.40. Multi-threading prevents one analysis/SQLite request from blocking accept. | Single-thread runtime has poorer isolation; axum 0.8 would enlarge the upgrade surface. |
| D3 | MVP event kinds are `click`, `adopt`, and `rate`; `rate` is 1..=5. Every event has `log_id`; `click/adopt` require `page_id`. | Existing hit count covers hits. These three signals cover click, adoption, and explicit quality without inventing behavioral enums. | `hit` duplicates an existing fact; favorites, skips, and free text expand noise and privacy semantics. |
| D4 | Migration 0005 creates `feedback_events`, `review_queue`, and `feedback_rejections`; do not add review columns to `compile_tasks`. | A suggestion is not an executable task. An independent table provides auditability and approve/ignore lifecycle without corrupting Step 4 states. | Adding `needs_review` would break the existing pending/running/succeeded/failed/dead contract. |
| D5 | Enforce UNIQUE `(domain, idempotency_key)`. A duplicate returns 200 and the original event ID/time; payload is not updated. | Retries are normal network behavior; replay makes clients resilient. | 409 turns recoverable retries into business failures; overwrite destroys audit history. |
| D6 | Read static keys from `WIKTOR_FEEDBACK_API_KEYS`, JSON `{ "domain": "secret" }`; never read them from domain.yaml or store them. Authenticate with `Authorization: Bearer <secret>`. | Secrets are not domain knowledge. One startup environment is suitable for the single-node MVP; reject empty/duplicate/invalid configuration. | YAML risks committing credentials; a separate file adds deployment and permission surface. |
| D7 | Tenant validation is an exact match between key-allowed domain and body `domain`; `*` is forbidden and the canonical domain name is used. | Prevents cross-domain writes and avoids leaking domain existence; return 403. | Global key authorization is too broad; host/header inference is easy to bypass. |
| D8 | In-process fixed window: 120 authorized requests per `(domain,key_hash)` per 60 seconds. Exceeding returns 429 with `Retry-After` seconds remaining; restart clears state. | No heavy dependency, deterministic, suitable for single-node MVP. Key plaintext never becomes a metric label; use a truncated BLAKE3 identifier. | SQLite counting adds write contention and cleanup; distributed limiting belongs to #7/deployment. |
| D9 | Request body max is 64 KiB UTF-8 bytes; at most 100 events; each query text max 4096 Unicode scalars. Over-limit returns 413 and records `feedback_rejections(reason,payload_bytes,created_at)`. | Implements input-budget contract before unbounded parsing. Rejected requests remain observable. | Truncation creates false feedback; silent dropping violates reliability. |
| D10 | Query-log fields are `candidate_empty_initial`, `relaxation_attempted`, and `relaxation_succeeded`. On initial empty candidates, QueryEngine calls an injected `FilterRelaxer` at most once. All three fields are written with the same query-log transaction. | Separates filter-empty from blind spots and makes §5.4 auditable. | A single `zero_recall` flag couples engine behavior to analysis and loses retry evidence. |
| D11 | Analyzer reads query logs and feedback events. Zero recall = `hit_count=0 AND NOT(candidate_empty_initial=1 AND relaxation_succeeded=0)`; rewrite failure = `rewrite_failure=1`; low quality is per-page with at least five `click/adopt/rate` events and adoption rate `< 0.20`. | Deterministic/replayable without LLM. Five-event minimum avoids one noisy signal; `adopt` and `rate>=4` count as adoption, click only as denominator. | Click-only quality mistakes engagement for adoption; no minimum produces noisy reviews. |
| D12 | Suggestions insert only into `review_queue`, with action `supplemental_compile/query_template/ignore`. Approving `supplemental_compile` validates pending state and invokes existing `admit_compile` in one `BEGIN IMMEDIATE`, rolling back all on failure. | Preserves manual review and the Step 4 state machine. | Direct admission would let noisy feedback contaminate the knowledge base. |

## 4. Architecture and data flow

```text
client
  └─ POST /feedback + Bearer key + Idempotency-Key
       └─ axum: size/auth/tenant/rate-limit/schema
            └─ FeedbackStore (short SQLite transaction)
                 ├─ duplicate → 200 replay
                 └─ new → feedback_events

query → QUG/fallback → filter pushdown
      ├─ candidates → retrieve
      └─ empty → FilterRelaxer once → retrieve or final empty
      └─ query_logs(rewrite + filter state + hits)

wiktor feedback analyze --from/--to
  └─ FeedbackAnalyzer(query_logs + feedback_events)
       ├─ FeedbackReport (JSON + bilingual Markdown)
       └─ review_queue (suggestions; no compile call)

wiktor feedback review approve <id>
  └─ BEGIN IMMEDIATE: claim review item → admit_compile → approved / rollback
```

Module boundaries:

```text
crates/wiktor-feedback/src/{lib,model,store,analyzer,report}.rs
crates/wiktor-server/src/{lib,main,state,auth,rate_limit,metrics}.rs
crates/wiktor-cli/src/commands/feedback.rs
crates/wiktor-core/src/query_engine/mod.rs       # FilterRelaxer + query-log fields
crates/wiktor-core/migrations/0005_feedback_loop/{up,down}.sql
```

`wiktor-feedback` depends on `wiktor-core`, serde/serde_json, async-trait, and tracing. `wiktor-server` depends on feedback, core, axum, Tokio, serde/serde_json, tracing, and blake3. Feedback must not depend on server. HTTP handlers must not construct SQL strings.

## 5. Data model and migration 0005 draft

Migration file: `crates/wiktor-core/migrations/0005_feedback_loop/up.sql`. The migration runs under Diesel's migration transaction. `down` is development-only and must first verify that no Step 6 rows exist; production down must fail rather than silently destroy audit data.

```sql
ALTER TABLE query_logs ADD COLUMN domain TEXT NOT NULL DEFAULT '__legacy__';
ALTER TABLE query_logs ADD COLUMN candidate_empty_initial INTEGER NOT NULL DEFAULT 0
  CHECK (candidate_empty_initial IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_attempted INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_attempted IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_succeeded INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_succeeded IN (0,1));
CREATE INDEX idx_query_logs_domain_time ON query_logs(domain, timestamp);

CREATE TABLE feedback_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  idempotency_key TEXT NOT NULL,
  domain TEXT NOT NULL,
  log_id INTEGER NOT NULL REFERENCES query_logs(log_id) ON DELETE RESTRICT,
  kind TEXT NOT NULL CHECK (kind IN ('click','adopt','rate')),
  page_id TEXT,
  rating INTEGER CHECK (rating IS NULL OR rating BETWEEN 1 AND 5),
  metadata_json TEXT NOT NULL DEFAULT '{}',
  received_at INTEGER NOT NULL,
  CHECK ((kind = 'rate' AND rating IS NOT NULL) OR
         (kind IN ('click','adopt') AND rating IS NULL)),
  CHECK ((kind IN ('click','adopt') AND page_id IS NOT NULL) OR
         (kind = 'rate')),
  UNIQUE(domain, idempotency_key)
);
CREATE INDEX idx_feedback_log ON feedback_events(domain, log_id);
CREATE INDEX idx_feedback_page ON feedback_events(domain, page_id) WHERE page_id IS NOT NULL;
CREATE INDEX idx_feedback_received ON feedback_events(domain, received_at);

CREATE TABLE review_queue (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN ('supplemental_compile','query_template','ignore')),
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending','approved','ignored','failed')),
  source_log_ids_json TEXT NOT NULL,
  subject_json TEXT NOT NULL,
  reason_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  reviewed_at INTEGER,
  reviewed_by TEXT,
  compile_task_id INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT,
  UNIQUE(domain, action, subject_json)
);
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);

CREATE TABLE feedback_rejections (
  rejection_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT,
  reason TEXT NOT NULL CHECK (reason IN ('payload_too_large','event_count_too_large','field_too_large')),
  payload_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_feedback_rejections_time ON feedback_rejections(created_at);
```

`metadata_json` is an object no larger than 4 KiB and does not affect MVP decisions; parse failure returns 422. `log_id` must exist and have the same domain. If `page_id` is present, it must belong to the query's acceptable result snapshot (using `query_json`/`rewritten_json` where available); absent a snapshot, reject strictly rather than trusting the client. Enable foreign keys with `PRAGMA foreign_keys=ON` on every connection.

## 6. Rust types and trait contracts

Within core/feedback, `Result` means the existing error type or an explicit `FeedbackError`. Database, JSON, and authentication failures must never become an empty report.

```rust
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind { Click, Adopt, Rate }

pub struct FeedbackEventInput {
    pub idempotency_key: String, // ASCII/visible, 1..=128 chars
    pub domain: String,
    pub log_id: i64,
    pub kind: FeedbackKind,
    pub page_id: Option<String>,
    pub rating: Option<u8>,
    pub metadata: serde_json::Value,
}
pub struct FeedbackIngested { pub event_id: i64, pub replayed: bool, pub received_at: i64 }

#[async_trait]
pub trait FeedbackStore: Send + Sync {
    fn insert_idempotent(&self, input: &FeedbackEventInput, now: i64)
        -> Result<FeedbackIngested>;
    fn load_window(&self, domain: &str, from: i64, to: i64)
        -> Result<(Vec<QueryLogSnapshot>, Vec<FeedbackEvent>)>;
    fn list_reviews(&self, domain: &str, status: Option<ReviewStatus>, limit: u32)
        -> Result<Vec<ReviewItem>>;
    fn approve_review(&self, review_id: i64, reviewer: &str, now: i64)
        -> Result<ReviewOutcome>;
    fn ignore_review(&self, review_id: i64, reviewer: &str, now: i64)
        -> Result<()>;
}

#[async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    async fn analyze(&self, input: FeedbackWindow) -> Result<FeedbackReport>;
}
pub struct FeedbackWindow {
    pub domain: String, pub from: i64, pub to: i64,
    pub logs: Vec<QueryLogSnapshot>, pub events: Vec<FeedbackEvent>,
}
pub struct FeedbackReport {
    pub schema_version: String, pub domain: String, pub from: i64, pub to: i64,
    pub zero_recall: Vec<BlindSpotQuery>,
    pub low_quality: Vec<LowQualityPage>,
    pub rewrite_failures: Vec<BlindSpotQuery>,
    pub suggested_reviews: Vec<ReviewSuggestion>,
    pub counts: FeedbackCounts,
}
```

The analyzer creates deterministic `subject_json`: zero-recall and rewrite-failure samples are deduplicated by `(domain, normalized query_text)`, retaining log IDs, count, and average latency; low-quality rows are by page ID and contain `feedback_count/adopted_count/adoption_rate`. Suggestions are deduplicated within an analysis run; the review table's unique key makes repeated analysis idempotent. JSON must contain window, thresholds, input counts, all three sample classes, and `report_hash` (BLAKE3 of canonical JSON). Markdown outputs are `step6-feedback-report.md` and `.en.md`; numbers, IDs, and hashes match, and literals such as `啵啵` and `珍珠` remain untranslated.

`FilterRelaxer`:

```rust
pub trait FilterRelaxer: Send + Sync {
    fn relax_once(&self, filters: &Filters) -> Result<Option<Filters>>;
}
```

Add optional `filter_relaxer: Option<Arc<dyn FilterRelaxer>>` to `QueryEngine`. No filter or `None` from `relax_once` means no retry; exactly one retry is allowed. Query-log insertion must return the actual `log_id` (the current `log_query` is fire-and-warn; Step 6 changes it to return the insert ID). A log-write failure is a query `Err`, not a warning-only success, because feedback references the log. New rows take domain from Query; a missing domain uses `__default__`, never the legacy migration default.

## 7. HTTP contract

### 7.1 `POST /feedback`

Required headers: `Authorization: Bearer <secret>`, `Idempotency-Key: <same as body.idempotency_key>`, and `Content-Type: application/json`. The two idempotency keys must be byte-equal; otherwise return 422. Body:

```json
{
  "domain":"ecommerce",
  "events":[
    {"idempotency_key":"checkout-7-1","log_id":42,"kind":"click","page_id":"drink:boba","metadata":{}},
    {"idempotency_key":"checkout-7-2","log_id":42,"kind":"adopt","page_id":"drink:boba","metadata":{}},
    {"idempotency_key":"checkout-7-3","log_id":42,"kind":"rate","rating":5,"metadata":{}}
  ]
}
```

Limits: body ≤64 KiB; events 1..=100; key 1..=128 Unicode scalars and no control characters; domain 1..=128; metadata object ≤4 KiB; `log_id > 0`; click/adopt page ID non-empty and ≤512; rate requires rating 1..=5. If any event fails, the entire batch rolls back. A batch containing duplicates and new events must roll back all new events and return an error; no partial success.

Success 200:

```json
{"domain":"ecommerce","events":[
  {"idempotency_key":"checkout-7-1","event_id":91,"replayed":false,"received_at":1780000000}
],"accepted":3}
```

All-duplicate requests still return 200, with `replayed=true` and original time. Service errors use `application/json` `{ "error": {"code":"...","message":"..."} }`; messages must not contain secrets, SQL, or raw metadata.

Error codes:

| HTTP | code | Meaning |
|---:|---|---|
| 400 | `INVALID_JSON` | Invalid JSON/top-level shape |
| 401 | `UNAUTHENTICATED` | Missing or invalid Bearer key |
| 403 | `TENANT_FORBIDDEN` | Key does not allow domain, or log/domain differs |
| 409 | not used | Idempotency is replayed as 200 per D5 |
| 413 | `PAYLOAD_TOO_LARGE` | Body, event count, or field budget exceeded; rejection count recorded |
| 422 | `INVALID_FEEDBACK` | Event kind/rating, header/body key, log, or page validation failure |
| 429 | `RATE_LIMITED` | Fixed-window limit; includes Retry-After |
| 500 | `STORE_ERROR` | SQLite/transaction failure |
| 503 | `SERVER_NOT_READY` | Migration or key configuration incomplete |

### 7.2 Health and metrics

`GET /health` is unauthenticated. Healthy response: 200 `{"status":"ok","schema_version":"step6-v1"}`; unavailable SQLite returns 503. `GET /metrics` emits minimal Prometheus text without domain, query text, or key: `wiktor_feedback_ingested_total`, `wiktor_feedback_replayed_total`, `wiktor_feedback_rejected_total{reason=...}`, `wiktor_feedback_rate_limited_total`, `wiktor_feedback_store_errors_total`, and `wiktor_feedback_review_pending`. Metrics use read-only transaction/cache and fixed label values only; user input must never become a label.

## 8. CLI contract and exit codes

As in Step 5: 0 success (including empty report/no suggestions); 1 runtime/storage failure; 2 argument failure; 3 data, migration, protocol, or report validation failure; 4 uncategorized internal failure. With `--json`, stdout contains one JSON object and logs go to stderr.

```text
wiktor feedback analyze --db <path> --domain <name> --from <unix> --to <unix>
    [--out-dir <dir>] [--min-events <n>] [--json]
wiktor feedback list --db <path> --domain <name> [--status pending|approved|ignored|failed]
    [--limit 1..=1000] [--json]
wiktor feedback review approve --db <path> --review-id <id> --by <operator>
wiktor feedback review ignore  --db <path> --review-id <id> --by <operator>
```

The window requires `from <= to` and is at most 31 days; default is `now-24h` to `now`. `analyze` writes suggestions and the report; report files use temp-file then rename, and write failure returns 1. `approve` permits only pending items. `supplemental_compile` requires `entity_id/source_revision/domain_pack_version/source_json/dependencies_json` in subject and reuses `CompileStore::admit_compile`; missing fields return 3, never fabricate a task. `query_template` may only become an approved audit item and must not modify QUG/config. `ignore` records ignored.

## 9. Concurrency, transactions, and port semantics

Server and CLI may open the same SQLite database concurrently: WAL and a 5-second busy timeout; read windows use ordinary read transactions, while ingestion and review conversion use `BEGIN IMMEDIATE`. Public kernel/store methods take the connection mutex once; no guard crosses await. Busy/constraint errors return explicit 1/500; never retry around idempotency.

The server is the only HTTP listener. CLI feedback commands open the DB directly and do not connect to or bind the server; a future `wiktor serve` calls `wiktor-server::build_router`. Step 6 has no server-side compile worker. The server writes feedback and review suggestions and reads query logs; only CLI approval writes `compile_tasks`.

## 10. Acceptance criteria A1–A18

- **A1**: workspace includes `wiktor-feedback` and `wiktor-server`; no feedback→server or core→server reverse edge; `cargo check --workspace` passes.
- **A2**: 0005 migrates empty and existing-0004 databases; legacy logs remain readable with deterministic defaults; destructive down is rejected.
- **A3**: DDL CHECK, FK, and UNIQUE constraints work; invalid kind/rating/page combinations are rejected.
- **A4**: Repeating `(domain,idempotency_key)` returns 200 replay with unchanged event ID/time; a different payload cannot overwrite the original.
- **A5**: Missing key, bad key, and unauthorized domain return 401/403; plaintext keys never occur in logs or metrics.
- **A6**: 64 KiB, 100-event, and field-budget boundaries are accepted; over-limit returns 413, records a rejection, and writes no event.
- **A7**: Each batch is all-or-nothing; missing log, domain mismatch, or absent page snapshot returns 422.
- **A8**: Request 121 in a fixed window returns 429 with correct Retry-After; allowance returns after the window; restart clears limits.
- **A9**: No-filter empty candidates do not relax; filtered empty candidates call `FilterRelaxer` at most once and persist all three states accurately.
- **A10**: Filter-empty after failed relaxation is excluded from zero recall; true non-filter-empty `hit_count=0` enters the report; rewrite failure is independent.
- **A11**: A low-quality page requires at least five feedback events and adoption strictly `<0.20`; exactly 0.20 does not trigger.
- **A12**: Two analyses produce identical canonical JSON/hash; review rows do not duplicate; DB failure does not produce an empty report.
- **A13**: Chinese Markdown, English Markdown, and JSON are generated with matching IDs/numbers/hash; report budget overflow errors rather than truncating.
- **A14**: CLI argument/storage/protocol errors map to 2/1/3; `--json` emits one stdout object.
- **A15**: Approving pending `supplemental_compile` calls existing admission in one transaction, writes a compile task and task ID; failure leaves pending with no orphan task.
- **A16**: Approving `query_template` changes no config and writes no compile task; ignore only records audit state; repeated review is rejected.
- **A17**: Health is 200 with usable SQLite and 503 otherwise; metrics include fixed metrics and rejection counters, with no user-controlled labels.
- **A18**: Concurrent server/CLI WAL smoke has no dirty reads or stale overwrites; busy/constraint/migration/authentication errors remain observable and are not swallowed.

## 11. Implementation batches for wiktor-builder

1. **Model and migration**: add workspace crate manifests, 0005 DDL, Diesel/raw-bind mappings, error types, migration registration, and synchronous `FeedbackStore`; verify A1–A4.
2. **Query-log**: implement `FilterRelaxer`, one retry on empty candidates, query-log ID return and three state columns; default relaxer returns None; verify A9–A10.
3. **Analyzer**: implement window reads, three signal aggregations, thresholds, deduplicated suggestions, canonical JSON/hash, and bilingual report model; never call compile; verify A10–A13.
4. **Review**: implement list/approve/ignore; connect supplemental approval to existing `admit_compile` with transactional CAS/status checks; verify A15–A16.
5. **HTTP**: add axum 0.7 state/router, Bearer auth, domain check, body limit, fixed-window limiter, idempotent replay, health, and metrics; verify A5–A8 and A17.
6. **CLI**: add feedback subcommands, time window/output/JSON handling, and exit-code mapping; verify A13–A14.
7. **Concurrency and closeout**: WAL/busy timeout, server+CLI same-DB smoke, sensitive-log audit, workspace check; verify A18 and produce an implementation deviation record.

Each batch must independently compile, test, and revert. Authentication, tenant, budget, migration, SQL, and report-protocol errors must never become “empty report”, “no blind spot”, or “automatically ignored”.

## 12. Risks, trade-offs, and deviation record

Fixed-window limiting protects only one process. Multi-replica deployments need an external limiter or sticky routing in #7; the MVP counter must not be presented as cluster limiting. The low-quality threshold is an explainable baseline, not statistical significance; reports must include sample counts. Old query logs may lack result snapshots, so rejecting unverifiable page feedback is safer than trusting client claims. Having server ingestion and CLI review as two SQLite writers creates contention; WAL and short transactions compensate but do not promise lock-free operation.

| ID | Planned/current deviation | Reason | Impact | Compensation | MASTER-PLAN update? |
|---|---|---|---|---|---|
| STEP6-001 | The illustrative MASTER-PLAN trait accepts only `&[QueryLog]`; this spec takes a query-log + feedback window and returns `Result`. | All three signals require the feedback table, and DB/JSON errors cannot be swallowed. | New public types are required; the illustrative signature is not source-compatible. | Keep the `FeedbackAnalyzer` name and define explicit failure/report schema. | No |
| STEP6-002 | Current 0001 logs lack domain/filter-empty/retry columns, and empty candidates return immediately. | Step 3 implemented the fast empty path before §5.4. | Legacy empty results cannot safely be classified. | Migration defaults legacy; new logs carry all state; no-rule FilterRelaxer explicitly returns None. | No |
| STEP6-003 | MASTER-PLAN §10 says `rusqlite`, while the repository uses Diesel SQLite. | Current workspace and Step 4 already chose Diesel. | A second connection stack is forbidden. | Reuse existing kernel/connection mutex/raw binds. | No |
| STEP6-004 | Target layout names `wiktor-server`, but the workspace has no crate or axum dependency. | This is the first real HTTP surface. | A minimal HTTP dependency is added. | Axum 0.7, no tonic, only three endpoints; any future upgrade gets a new deviation. | No |
| STEP6-005 | A compile task is created only after approval; analysis never admits directly. | Manual review and the Step 4 state machine must both hold. | Closed-loop latency increases. | Approval reuses `admit_compile` and preserves source snapshot/audit. | No |
| STEP6-006 | Spec §4.1 places contract types and the store in wiktor-feedback, but the core→feedback dependency is forbidden and the core SQL implementation must reference the types. | A necessary consequence of STEP6-003 ("no second connection stack"). | File layout differs from the letter of §4.1. | Types are defined once in `core/kernel/feedback_store.rs` (impl SqliteKernel); `wiktor-feedback` fully re-exports them as the public contract surface and hosts the trait/analyzer/report. | No |
| STEP6-007 | §7.1 requires the `Idempotency-Key` header to byte-equal the body key, but a batch body carries one idempotency_key per event, so no header can correspond. | Internal spec contradiction (the header is unimplementable for batches). | One header removed from the API. | Ruling: the per-event body `idempotency_key` is the sole idempotency source; the header is dropped; D5 semantics unchanged. | No |
| STEP6-008 | §5 requires page_id to belong to "that query's result snapshot", but query_logs never stored result sets; taken literally, every click/adopt would 422. | The spec depended on a nonexistent snapshot. | Validation scope changes. | Ruling: page_id must genuinely exist as an accepted page of that domain (exact `page_exists` match); missing log / log-domain mismatch → 422 (the §7.1 table's 403 row narrows to key/domain mismatch). | No |
| STEP6-009 | §7.1's "mixed new+duplicate batches roll back all new events with an error" coexists ambiguously with "existing duplicate keys may replay". | The batch + idempotent-replay combination was underspecified. | Mixed-batch response semantics. | Ruling: within one transaction duplicates replay and new keys insert; if everything is valid the response is 200 (per-event replayed flags); any hard failure rolls back the whole batch (no partial success, unchanged). | No |
| STEP6-010 | §6 leaves FilterRelaxer default rules, the action mapping, and the `relaxation_succeeded` scope undefined. | Details the spec delegated to implementation. | Refined judgment semantics. | Ruling: DefaultFilterRelaxer relaxes only NumericRange (drop max → drop min; an unconstrained range removes the whole condition); Text/Ref conditions never relax; `relaxation_succeeded` = the candidate scope is non-empty after the relaxed retry (not final hits>0); suggestion mapping: zero-recall/low-quality → supplemental_compile, rewrite-failure → query_template. | No |
| STEP6-011 | Batch 4/6 fail-closed strengthenings and parameterization. | Noise prevention and operability. | Small contract-surface extensions. | approve accepts only `Admission::Queued` (Skipped/Rejected → Validation + rollback); the subject gains a cross-check that the entity domain equals the review row's domain; action=ignore cannot be approved; `--min-events` parameterizes StandardFeedbackAnalyzer (default = the D11 constant 5, range 1..=1000). | No |

Implementers must append a row if implementation diverges; they must not silently change D1–D12, DDL, thresholds, error codes, or acceptance semantics.
