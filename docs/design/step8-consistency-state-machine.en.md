# Step 8 Design Specification: Consistency Arbitration and Compile Task State-Machine Completion

> Version: v1.0 (2026-09-23)  
> Upstream: Step 4 `step4-compile-pipeline.md`, Step 6 `step6-feedback-loop.md`  
> Implementation target: `wiktor-builder`; independent acceptance target: `test-engineer`  
> This English document corresponds section by section to `step8-consistency-state-machine.md`.

## 1. Goals and non-goals

Step 8 closes the two gaps in MASTER-PLAN §5.1/§5.5: it turns consistency from `NULL` into a pluggable, offline-verifiable source-reference comparison; and completes the existing task state machine with lease reaping, dead-letter review, and domain-pack compatibility preflight.

Goals:

- `page_quality.consistency` supports `NULL` or `[0,1]`; the default implementation compares explicit source-ref evidence only and performs no free-form semantic inference.
- Consistency checks consume a bounded top-k of related pages and never perform an all-pages pairwise comparison; a detected conflict blocks publication.
- Tasks ending in `dead/quarantined`, page-brake exhaustion, lease exhaustion, and compatibility failures all produce auditable human-review items.
- A single worker periodically reaps expired leases; each model wait has a heartbeat renewal, and a fenced worker cannot write state after losing ownership.
- Domain-pack semver, Schema/Prompt versions, and `artifact_version` are checked before compatibility-sensitive admission; incompatible data is rejected and produces a compatibility review alert.
- Migration 0006 preserves all 0001–0005 data and keeps the existing review-queue lifecycle and `UNIQUE(domain,action,subject_json)` semantics.

Non-goals:

- No gRPC/tonic and no changes to the Step 6 axum HTTP surface.
- No consistency LLM arbitration. A future LLM may implement the same trait without bypassing evidence, top-k, budgets, or review rules.
- No rewrite of Step 4 admission, epoch fencing, exponential backoff, token reservations, or publish transactions.
- No all-pages consistency matrix, cross-page natural-language inference, or automatic domain-pack modification.
- No new review UI, distributed worker protocol, or second database connection stack.

## 2. Terms and current-state constraints

- **Candidate page**: the `CompiledPage` about to be published.
- **Related pages**: accepted, published pages returned by `ConsistencyCandidateProvider`, no more than policy `top_k`; the candidate is excluded from its own result.
- **Comparable evidence**: two source refs correspond under an explicitly declared comparison key and their values can be compared as the same type; no comparable evidence yields `None`.
- **Conflict/divergence**: different source-evidence values for the same explicit comparison key, or a candidate reference differing from the current fact-plane value for the same key. Textual similarity and different titles are not conflicts.
- **Dead letter**: a compile task that reached a non-automatically recoverable terminal state after quality recompiles, transport retries, lease retries, or another terminal failure. SQL remains `dead`; business results distinguish `failed` and `quarantined`.
- **Compatibility preflight**: before admission, read persisted artifact/dependency snapshots and validate the current domain pack's semver ranges, Schema/Prompt versions, and artifact version.

The following confirmed facts are fixed and must not be reinterpreted:

- **X1**: current `review_queue.action` contains only `supplemental_compile/query_template/ignore`; migration 0006 must widen its CHECK. Dead letters deduplicate by `task_id`, not by accidental equality of subject text.
- **X2**: current `page_quality.consistency` is always `NULL`; the four rule dimensions and overall are already persisted.
- **X3**: `compile_store.rs` promises that page-brake exhaustion enters the human queue, but currently inserts nothing; this step must fulfill that promise.
- **X4**: Step 4 already delivered claim/recover/heartbeat/fencing/backoff/superseded; this step fills actual gaps only. The current executor calls `recover_compile_leases` only at the beginning of a real run and has no periodic background reaper; `process_lease` has no heartbeat while the model is waiting.
- **X5**: MVP excludes consistency LLM arbitration. The default implementation must use existing source refs and source evidence, forbid free semantic inference, and expose a trait replaceable by a future LLM implementation without changing the core state machine.
- **X6**: compatibility checking includes domain-pack semver, Schema/Prompt compatibility, and `artifact_version`; Step 4's full-dependency BLAKE3 hash naturally triggers recompilation for a new hash.
- **X7**: every decision has an implementation batch and offline acceptance criterion; acceptance uses no LLM, network, or external service.

## 3. Decisions D1–D12

| ID | Decision | Reason and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Add `ConsistencyArbiter`; default `SourceRefConsistencyArbiter`; feed its result into a scorer composition after the existing scorer | Core depends on an abstraction; default is deterministic and offline. Future LLM replaces the implementation only | B2 | A3–A7 |
| D2 | Related pages come from bounded `ConsistencyCandidateProvider`; the default SQLite FTS provider returns at most `top_k`, default 8, cap 32 | Preserves top-k cost boundary and forbids all-page comparison; no candidates or no overlapping evidence yields `NULL` | B2 | A4, A8 |
| D3 | Comparison keys are explicit Schema/config source-ref pointers; default key is `(entity_id,pointer)`, with values compared by canonical JSON | Names, aliases, and natural-language similarity are never facts; revisions and evidence drift remain detectable | B1/B2 | A5 |
| D4 | `consistency=None` does not lower four-dimension overall; with comparable evidence overall is equal-weight five-dimension average, and accepted requires `min_consistency` (default 1.0) | Preserves old seed/no-comparison behavior while every explicit conflict blocks publication | B2 | A6 |
| D5 | A consistency conflict is a quality-candidate failure and consumes `recompile_count`; when the brake is exhausted, `dead/quarantined` and two review actions, `compile_dead_letter` and `consistency_conflict`, are inserted in the same transaction | Contradictions are never auto-accepted and no new compile state is invented | B3 | A9 |
| D6 | Reuse `review_queue`; add `compile_dead_letter`, `consistency_conflict`, `compatibility_conflict` actions | Step 6 already provides review lifecycle, reviewers, and transaction patterns; a second table is unnecessary | B3 | A10 |
| D7 | `compile_dead_letter.subject_json` is exactly `{"task_id":N}`; one row per domain/task and `compile_task_id` is backfilled | Deduplicates by task ID rather than reason text; repeated recovery/restarts do not duplicate alerts | B3 | A10 |
| D8 | Keep recovery at each run start and add a periodic `LeaseReaper`; default interval 30 seconds, with one final drain on shutdown | A single worker must recover abandoned running tasks without relying on HTTP/gRPC | B4 | A11 |
| D9 | Executor starts a heartbeat task per lease, period `lease_seconds/2`, minimum 1 second; stop and join it before publish | A 300-second lease renews by default every 150 seconds; no lock crosses await; final publish CAS decides staleness | B4 | A12 |
| D10 | Store the compatibility matrix in the `compatibility` section of `domain.yaml`, and persist it in `dependencies_json`; do not create a table | The domain pack is authoritative and the database stores a compile-time snapshot, avoiding dual writes | B1/B5 | A13–A15 |
| D11 | `wiktor domain check` runs read-only full preflight; real `compile` automatically runs the same preflight before first admission; incompatible data returns 3, rejects admission, and idempotently inserts `compatibility_conflict` during the compile transaction | CLI provides an audit surface and compilation fails closed; the new hash still controls recompilation | B5/B6 | A14–A16 |
| D12 | 0006 down first checks for Step 8 action rows, consistency data, and compatibility audit data, then removes new indexes/columns; any such data blocks downgrade | Step 8 audit is never destroyed; existing Step 6 audit is also not silently removed | B1 | A2, A17 |

## 4. Architecture and data flow

```text
compile admission
  └─ compatibility preflight (read all persisted dependency/artifact snapshots)
       ├─ incompatible → BEGIN IMMEDIATE: compatibility review upsert → reject
       └─ compatible
            └─ claim lease
                 └─ heartbeat loop ──┐
                  compiler/model ────┤
                  source-ref validate ┘
                         ↓
             top-k related accepted pages
                         ↓
             ConsistencyArbiter → None | score + findings
                         ↓
                  RuleScorer / five-dim gates
                  ├─ accepted → publish transaction
                  └─ candidate failure → retry or dead
                                      └─ same transaction writes review_queue

LeaseReaper timer → recover expired running tasks → retry/backoff or dead/review
review approve(dead letter) → BEGIN IMMEDIATE → re-admit with epoch fence
```

Module boundaries:

```text
crates/wiktor-core/src/compile/consistency.rs
crates/wiktor-core/src/compile/compatibility.rs
crates/wiktor-core/src/kernel/compile_store.rs       # lease/review atomic helpers
crates/wiktor-core/src/kernel/feedback_store.rs      # new review actions + list/review
crates/wiktor-core/migrations/0006_step8_consistency/up.sql
crates/wiktor-core/migrations/0006_step8_consistency/down.sql
crates/wiktor-cli/src/commands/domain.rs              # domain check
crates/wiktor-cli/src/commands/compile.rs             # preflight/status output
```

No new crate is added. Core does not depend on feedback/server. The async executor wraps each synchronous kernel public method with `spawn_blocking`; the connection mutex never crosses an await.

## 5. Data model and 0006 DDL draft

### 5.1 Domain YAML extension

```yaml
name: ecommerce
version: 1.2.0
schema_version: 2.1.0
prompt_version: 3.0.0
compile:
  artifact_version: wiki-v2
  consistency:
    enabled: true
    top_k: 8
    min_consistency: 1.0
    compare_pointers: ["/fields/name", "/fields/description"]
compatibility:
  domain_pack: ">=1.0.0,<2.0.0"
  schema: ">=2.0.0,<3.0.0"
  prompt: ">=3.0.0,<4.0.0"
  artifact: ["wiki-v1", "wiki-v2"]
```

`version`, `schema_version`, and `prompt_version` must be strict semver. Ranges use proper semver comparison and never lexicographic ordering. `compatibility.artifact` is an explicit allowlist. If `compatibility` is absent, a read-only check is allowed only for a legacy first startup; a real compile with existing Step 4 artifacts treats the configuration as invalid rather than guessing compatibility.

### 5.2 Migration files

Migration path: `crates/wiktor-core/migrations/0006_step8_consistency/{up,down}.sql`. The following columns and constraints are mandatory; existing tables are not dropped or rebuilt except for the required CHECK-preserving review-queue copy, and the Step 4 triple UNIQUE is unchanged.

```sql
-- Migration 0006: Step 8 consistency, dead-letter review, compatibility checks

ALTER TABLE compile_tasks ADD COLUMN consistency_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (consistency_status IN ('unchecked','not_comparable','consistent','conflict'));
ALTER TABLE compile_tasks ADD COLUMN compatibility_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (compatibility_status IN ('unchecked','compatible','incompatible'));

-- Deterministic diagnostics only; source plaintext stays in compile_attempts artifacts
ALTER TABLE compile_attempts ADD COLUMN consistency_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE compile_attempts ADD COLUMN compatibility_json TEXT NOT NULL DEFAULT '{}';

CREATE INDEX idx_compile_tasks_dead_review
  ON compile_tasks(status, result, updated_at);
CREATE INDEX idx_compile_tasks_preflight
  ON compile_tasks(domain_pack_version, compatibility_status);

-- SQLite cannot directly alter an existing CHECK, so copy the existing table.
CREATE TABLE review_queue_step8_new (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN (
    'supplemental_compile','query_template','ignore',
    'compile_dead_letter','consistency_conflict','compatibility_conflict'
  )),
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
INSERT INTO review_queue_step8_new
  SELECT review_id,domain,action,status,source_log_ids_json,subject_json,
         reason_json,created_at,reviewed_at,reviewed_by,compile_task_id
  FROM review_queue;
DROP TABLE review_queue;
ALTER TABLE review_queue_step8_new RENAME TO review_queue;
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);
CREATE INDEX idx_review_task ON review_queue(domain, action, compile_task_id)
  WHERE compile_task_id IS NOT NULL;
```

`consistency_json` and `compatibility_json` are canonical JSON, each capped at 64 KiB; they contain codes, comparison keys, BLAKE3 hashes of old/new values, versions, and counts, never sensitive source plaintext. Step 8's `source_log_ids_json` is `[]` for compile reviews.

Canonical dead-letter subject/reason:

```json
{"task_id":42}
```

```json
{
  "code":"CONSISTENCY_CONFLICT",
  "task_id":42,
  "epoch":3,
  "attempt_no":3,
  "entity_id":"ecommerce:drink:boba",
  "desired_hash":"...",
  "findings":[{"key":"/fields/description","old_hash":"...","new_hash":"..."}]
}
```

A compatibility subject must include the current domain and domain/schema/prompt/artifact versions; one upgrade combination produces one `compatibility_conflict` row.

### 5.3 down.sql guard

The down migration must first use a temporary CHECK guard to require: zero rows in `review_queue` with any of the three new actions; zero `compile_tasks` rows with `consistency_status <> 'unchecked'` or `compatibility_status <> 'unchecked'`; and zero `compile_attempts` rows with either Step 8 JSON different from `{}`. Any nonzero count aborts without mutation. After the guard passes, remove Step 8 indexes/columns, then copy `review_queue` back to the original three-action CHECK and preserve its unique constraint. Never delete Step 6 audit facts or silently clear columns.

`db_schema.rs` adds the four columns and `SUPPORTED_SCHEMA_VERSION` changes from 5 to 6; migration-version assertions also change to 6. `row_counts` continues exposing `review_queue`; no network or external state is added.

## 6. Rust types and trait contracts

### 6.1 Consistency

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyPolicy {
    pub enabled: bool,
    pub top_k: u32,                 // 1..=32
    pub min_consistency: f32,       // finite, 0..=1
    pub compare_pointers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimKey {
    pub entity_id: String,
    pub pointer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyFinding {
    pub code: String,               // stable code, e.g. VALUE_DIVERGENCE
    pub key: ClaimKey,
    pub candidate_value_hash: String,
    pub evidence_value_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyReport {
    pub score: Option<f32>,
    pub compared_claims: u32,
    pub findings: Vec<ConsistencyFinding>,
    pub candidate_count: u32,
}

#[async_trait]
pub trait ConsistencyCandidateProvider: Send + Sync {
    async fn top_k_related(
        &self, candidate: &CompiledPage, limit: u32,
    ) -> Result<Vec<CompiledPage>>;
}

pub trait ConsistencyArbiter: Send + Sync {
    fn arbitrate(
        &self,
        candidate: &CompiledPage,
        related: &[CompiledPage],
        policy: &ConsistencyPolicy,
    ) -> Result<ConsistencyReport>;
}
```

The default `SourceRefConsistencyArbiter` is fixed as follows: collect source refs from candidate and related accepted-page evidence; keep only pointers in `compare_pointers` with the RFC 6901 `/fields/...` shape; group by `(entity_id,pointer)`; canonicalize values with `canonical_json` and compare them; a different value produces `VALUE_DIVERGENCE`. A group with one value is not compared; `compared_claims=0` returns `score=None`; otherwise `score = equal_groups / compared_groups`. Value diagnostics use BLAKE3 and never persist plaintext. Duplicate refs, absent evidence, and old seed pages are skipped; no equivalence is inferred.

The default provider must perform bounded retrieval: build a length-bounded FTS query from the candidate title and aliases, query `pages_fts` for `status='accepted'`, order deterministically by bm25 and page_id, and LIMIT `top_k`; then load evidence only for those pages. An FTS miss returns an empty set. The implementation must not `SELECT * FROM pages` and truncate in memory; tests may inject pre-built pages.

### 6.2 Scoring and publication

Extend the scoring result or add a composition layer without breaking Step 4's four-dimension interface:

```rust
pub trait QualityScorerV2: Send + Sync {
    fn score_with_consistency(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        consistency: &ConsistencyReport,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport;
}
```

Composition rules: the four dimensions retain Step 4 formulas; with `consistency=None`, overall remains the four-dimension average; with `Some(s)`, `overall=(coverage+citation+schema+density+s)/5`. A `Some(s)` below `min_consistency` adds stable issue `CONSISTENCY_BELOW_THRESHOLD` and cannot be accepted. Every score is finite and within `[0,1]`; `consistency` is written to `page_quality.consistency`; a conflicting page is never written as an accepted page.

`CompilePolicy` adds `consistency: ConsistencyPolicy`, `lease_reaper_interval_seconds`, and `compatibility_preflight: bool`. Policy and version values remain in the content hash; top-k and lease timing do not. Changing comparison pointers, thresholds, or the implementation version must change scorer/policy dependency versions so the new hash triggers recompilation.

### 6.3 Leases and recovery

```rust
pub struct LeaseReaper {
    pub kernel: Arc<SqliteKernel>,
    pub clock: Arc<dyn Clock>,
    pub interval: Duration,
}
impl LeaseReaper {
    pub async fn run_until_cancelled(&self, cancel: CancellationToken) -> Result<()>
}

impl SqliteKernel {
    pub fn recover_compile_leases(&self, now: i64) -> Result<RecoveryStats>;
    pub fn heartbeat_compile(&self, lease: &TaskLease, now: i64) -> Result<bool>;
    pub fn enqueue_dead_letter_on_conn(
        tx: &mut SqliteConnection, task_id: i64, domain: &str,
        epoch: i64, reason_json: &str, now: i64,
    ) -> Result<()>;
}
```

`RecoveryStats` includes at least `recovered_pending`, `dead_failed`, `dead_quarantined`, `superseded`, and `review_inserted`. Existing recovery semantics must remain: old-token CAS, abandoned attempt, unreimbursed reservation, and superseded priority; only `dead` branches call `enqueue_dead_letter_on_conn` in the same transaction. `INSERT ... ON CONFLICT(domain,action,subject_json) DO NOTHING` is the idempotency guard.

The executor starts the reaper. On run cancellation, it cancels heartbeats, joins them, and performs one synchronous final recovery. Heartbeat updates retain `WHERE task_id AND epoch AND lease_token AND status='running' AND lease_expires_at > now`; false means stale and cannot directly mutate task state. During model calls no SQLite guard is held; after a model result, publish/failure fencing remains authoritative.

### 6.4 Compatibility checking

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilitySpec {
    pub domain_pack: VersionReq,
    pub schema: VersionReq,
    pub prompt: VersionReq,
    pub artifact: BTreeSet<String>,
}

pub struct CompatibilityReport {
    pub compatible: bool,
    pub checked_pages: u64,
    pub checked_tasks: u64,
    pub violations: Vec<CompatibilityViolation>,
}

pub trait CompatibilityChecker: Send + Sync {
    fn check(&self, current: &DomainIdentity,
        spec: &CompatibilitySpec, db: &SqliteKernel) -> Result<CompatibilityReport>;
}
```

Check every accepted page's `domain_pack_version/artifact_version/frontmatter quality_policy` and every pending/running/dead task's `dependencies_json` snapshot. Explicitly superseded historical attempts are not checked as current artifacts, but corrupt JSON remains a violation. Missing or invalid semver and artifacts outside the allowlist are incompatible. With `compatible=false`, compile does not call `admit_compile`, does not alter facts, and does not create a compile task; if triggered by real compile, it writes `compatibility_conflict` in the same `BEGIN IMMEDIATE` transaction. Read-only `domain check` never writes.

## 7. CLI contract and exit codes

New commands:

```text
wiktor domain check --domain <domain.yaml> --db <path> [--json]
wiktor compile --domain <domain.yaml> ... [--skip-compatibility-check]
wiktor feedback review approve --db <path> --review-id <id> --by <operator>
wiktor feedback review ignore  --db <path> --review-id <id> --by <operator>
```

`domain check` is read-only by default: no migration, database creation, or review write. A missing database is checked as an empty store. With `--json`, stdout contains only `CompatibilityReport`; human logs go to stderr. Incompatibility returns 3; database/IO returns 1; usage returns 2; other internal faults return 4.

`compile` automatically runs preflight. `--skip-compatibility-check` cannot bypass an incompatible result; it is allowed only for an empty database or when no old artifacts exist, and any discovered old data still returns 3. Preflight failure is a configuration/migration result, not a per-page failed or quarantined count. Dead letters are listed through `feedback list`, preserving action strings. Approving `compile_dead_letter` must parse `task_id`, verify the task is `dead` and the review is `pending`, explicitly admit a new epoch with `force=true` using the old task snapshot, then approve the review and backfill the new task ID in the same transaction. A non-`Queued` admission rolls back everything. `consistency_conflict` may only be approved by creating a new `supplemental_compile` review transition; it cannot publish directly. `compatibility_conflict` may only become an audit approval and cannot bypass preflight.

Exit codes retain Step 4/6 semantics: 0 success; 1 database, IO, and runtime failures; 2 usage/range errors; 3 configuration, migration, compatibility, and data-protocol errors; 4 unclassified internal errors. `--json` never mixes tables or logs into stdout.

## 8. Concurrency, transactions, and port semantics

Every write involving `compile_tasks`, `compile_attempts`, `compile_source_heads`, or `review_queue` takes the connection mutex once and uses `immediate_transaction`; DML is parameter-bound and internal helpers accept only `&mut SqliteConnection`. A transaction never calls another public kernel method that reacquires the mutex, and no lock is held over await.

- **Quality terminal state**: `finish_compile_failure` writes the attempt, updates the task, and inserts dead-letter/consistency reviews in one transaction. Any failure rolls back candidate audit, task state, and review rows together.
- **Recovery terminal state**: old-token attempt abandonment, retry/dead/superseded transition, and dead-letter insertion are one transaction; repeated recovery with zero affected rows neither increments retry count nor inserts a review.
- **Dead-letter approval**: reuse Step 6's `approve_review` transaction pattern; call `admit_compile_on_conn`, never the transaction-owning `admit_compile`; review CAS and epoch fencing share the transaction.
- **Compatibility alert**: insert only from the compile admission write transaction; duplicate `(domain,compatibility_conflict,subject_json)` rows are skipped. Read-only check never writes reviews.
- **Heartbeat**: heartbeat is a short single-statement write transaction; model calls, candidate retrieval, arbitration, and JSON encoding are outside transactions. After expiry, heartbeat returns false and publish/failure has stale semantics.
- **Ports**: Step 8 adds and listens on no port and depends on no axum, HTTP, gRPC, or qdrant; CLI and the reaper use the same SQLite kernel directly.

## 9. Acceptance criteria A1–A18

All criteria use temporary SQLite, injected clocks, pre-built pages, and fake compilers; no key, network, qdrant, or LLM.

| # | Criterion | Executable assertion |
|---|---|---|
| A1 | Migration compatibility | 0001–0005 upgrades to 6; all three old review actions, Step 4 tasks, accepted pages, and FTS remain readable; schema version is 6 |
| A2 | Downgrade guard | Any Step 8 review/consistency/compatibility audit makes down fail with rows unchanged; empty Step 8 data restores old CHECK and indexes |
| A3 | Replaceable trait | A fake arbiter can return None/1/0; executor does not depend on a concrete implementation and future LLM types do not enter the core state machine |
| A4 | Top-k boundary | Provider receives no limit above 32; SQLite SQL contains LIMIT; with 100 pages only 8 are loaded and compared |
| A5 | Exact evidence | Equal canonical values for the same `(entity_id,pointer)` score 1; different values produce `VALUE_DIVERGENCE`; title similarity without ref overlap returns None |
| A6 | Scoring compatibility | `consistency=None` keeps old four-dimension overall; `consistency=1` uses five-way average; zero or below threshold cannot be accepted; SQL stores NULL/0/1 exactly |
| A7 | Evidence safety | Different domains, invalid pointers, absent evidence, and old seed pages produce no comparison; diagnostics contain hashes only, never sensitive plaintext |
| A8 | Default provider | FTS related pages use stable bm25/page_id order; quarantined/dead/old-generation pages never become related |
| A9 | Consistency brake | Repeated conflicts with `max_recompiles=2` produce at most three candidates, then dead/quarantined; the same transaction contains one dead-letter and one consistency review |
| A10 | Dead-letter idempotency | Repeated finish/recovery for one task leaves one `compile_dead_letter` with exact subject `{"task_id":N}` and the correct `compile_task_id` |
| A11 | Periodic recovery | Without starting a new compile run, the reaper recovers a running task after injected expiry; it produces pending/backoff below the limit and dead above it; shutdown drains once |
| A12 | Heartbeat fencing | A fake model waiting beyond 300 seconds retains its lease through heartbeats; cancellation/failure stops the heartbeat; a fenced worker cannot publish, fail, or change counters |
| A13 | Semver matrix | Valid ranges pass; invalid semver, schema/prompt mismatch, and disallowed artifact are rejected; lexical ordering cannot misclassify versions |
| A14 | Full compatibility | Any violating accepted page/task appears in the report with accurate counts; corrupt dependencies JSON is a violation and is never silently skipped |
| A15 | Preflight trigger | `domain check` is read-only; compile calls the same checker before first admission; incompatibility leaves facts/tasks unchanged |
| A16 | Compatibility review idempotency | An incompatible compile writes one `compatibility_conflict` transactionally; repetition adds none; read-only check writes no review |
| A17 | Review transitions | Dead-letter approval starts only from pending and a dead task, reuses `admit_compile_on_conn`, and creates a new epoch; failures roll back review/task together; compatibility review cannot bypass preflight |
| A18 | Engineering regression | `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, workspace tests, and Rust 1.85 compilation pass; Step 4/6 acceptance does not regress |

## 10. Implementation batches for wiktor-builder

1. **B1: configuration, migration, and version model.** Add `CompatibilitySpec`, strict semver parsing, `ConsistencyPolicy` defaults, 0006 up/down, Diesel schema, and schema version 6; complete the pure parsing portions of A1/A2/A13. Do not change old hash semantics; new policy versions must be explicit hash dependencies.
2. **B2: consistency traits and deterministic arbiter.** Add `consistency.rs`, canonical claim/value comparison, the bounded provider trait, default FTS provider, and `QualityScorerV2` composition; use injected pages for A3–A8. Do not change task state in this batch.
3. **B3: atomic quality-failure and review integration.** Extend review-action validation/listing; enqueue reviews in dead branches of `finish_failure_in_transaction`, and persist consistency JSON/task state; complete A9/A10. All SQL uses one transaction and admission SQL is not duplicated.
4. **B4: reaper and heartbeat orchestration.** Add periodic `LeaseReaper`, connect executor cancellation/drain, and wrap model waits with cancellable heartbeat tasks while preserving recovery/fencing/backoff; complete A11/A12. Do not add a port or gRPC.
5. **B5: compatibility checker and compile preflight.** Scan accepted pages/tasks and validate semver/schema/prompt/artifact; implement the read-only report and connect it before compile admission; complete A13–A15. Preflight failure must not write facts/tasks.
6. **B6: compatibility review and dead-letter CLI.** Extend `feedback review list/approve/ignore` with fail-closed semantics for new actions, add `domain check`, JSON output, and exit codes, and implement dead-letter approval as a new epoch admission; complete A16/A17.
7. **B7: concurrency closeout and regression.** Add the downgrade guard, WAL/busy-timeout paths, fault injection, Step 4/6 regression, and bilingual comments; execute A18 and append the implementation-deviation record. Each batch must remain compilable, independently verifiable, and reversible.

## 11. Risks, trade-offs, and seeded deviation table

The default consistency implementation deliberately has limited expressive power: it proves exact divergence of source-ref values, but cannot prove that two different sentences are semantically contradictory. This satisfies the MVP's offline/no-inference boundary; richer comparisons require explicit pointer/key rules or a replacement arbiter.

FTS top-k is an offline related-page provider. A future embedding provider may implement `ConsistencyCandidateProvider`, but must retain the top-k cap, accepted/hash alignment, and no-all-pages rule. qdrant is outside Step 8 acceptance.

Dead letters and consistency reviews share `review_queue`, simplifying lifecycle but requiring fail-closed CLI handling for new actions. Unknown actions indicate database corruption/protocol failure and are never treated as ignore. Dead-letter approval creates a new epoch; old attempts are never reused, and budget is checked by the existing admission rules.

### Seeded deviation table

| ID | Plan/current deviation | Reason | Impact | Compensation | MASTER-PLAN update? |
|---|---|---|---|---|---|
| STEP8-001 | MASTER-PLAN describes top-k+LLM consistency; Step 8 default does not call an LLM | MVP excludes consistency LLM and X5 requires deterministic offline behavior | Default consistency detects explicit evidence divergence only | `ConsistencyArbiter` remains replaceable; a future LLM does not alter core | No |
| STEP8-002 | Default consistency provider uses SQLite FTS rather than vector/embedding retrieval | Step 8 acceptance must work without qdrant/network | Related-page quality depends on lexical retrieval | Provider trait; future qdrant implementation still obeys top-k and generation checks | No |
| STEP8-003 | No comparable claim yields NULL instead of 0 | Missing evidence cannot prove consistency or conflict | A page is not isolated merely because no comparison exists | `None` preserves old overall; five-way gates apply only with evidence | No |
| STEP8-004 | Dead letters reuse review_queue and widen its action CHECK | Step 6 already has review transactions and audit fields | 0006 must rebuild a SQLite CHECK-constrained table | Copy every old column, preserve FK/UNIQUE, and use the new canonical task subject | No |
| STEP8-005 | Current executor recovers only at run start and has no background reaper | Step 4 delivered the method but not a resident scheduler | A stopped worker can leave running tasks hanging | Periodic `LeaseReaper`; recover once at startup and shutdown | No |
| STEP8-006 | Current executor has no heartbeat loop during model waits | Step 4 delivered kernel heartbeat only | Long model calls can expire and cause duplicate paid work | B4 adds a cancellable heartbeat task; token/epoch fencing remains final authority | No |
| STEP8-007 | Compatibility matrix lives in domain.yaml rather than a database table | The domain pack is authoritative; a separate table would drift | The database stores a snapshot and cannot edit the matrix | Compile preflight reads config; dependencies_json stores the snapshot and diagnostics | No |
| STEP8-008 | Compatibility failure rejects compile and idempotently emits review; it does not auto-change hash or recompile | An incompatible upgrade requires human choice | Operations must fix the matrix or approve before running | Read-only `domain check`; compile fail-closed; Step 4 hash still handles new artifacts | No |
| STEP8-009 | 0006 rebuilds review_queue to modify its CHECK | SQLite cannot directly alter an existing CHECK | Migration surface is larger | Copy all columns, preserve FK/UNIQUE, guard down, and let migration transaction roll back failures | No |
| STEP8-010 | B1 sets the compatibility-matrix snapshot carrier to `CompilePolicy.compatibility: Option<CompatibilitySpec>` | D10 requires the matrix to persist inside dependencies_json, and the snapshot has only three carriers {context,policy,schema}; policy is the only config slot; §6.2's field list omitted it | dependencies_json shape extends (serde-default backward compatible) | Enters content hash; B5 consumes the same type | No |
| STEP8-011 | schema/prompt version identity flows through two new `Option<String>` fields (serde default) on CompileContext into hash and snapshot, instead of adding identity parameters to HashDependencies/admit | publish and supplemental re-admission can only restore identity via lease.context; extra parameters would touch the feedback layer (violating the B1 boundary) | Snapshot shape extends | Old rows stay readable via serde default; B5 reads historical versions from snapshots | No |
| STEP8-012 | content_hash_golden re-pinned (22454908… → 2ee5fecd04bfb1…) | The four new identity hash domains change the bytes (encoding and the first ten domains keep their order) | Golden hash changes | New test locks the identity-domain invalidation semantics; the task authorizes the explicit update | No |
| STEP8-013 | §6.2's QualityScorerV2 is not landed as a separate trait; it becomes the `RuleScorer::score_with_consistency` extension method (default implementation = legacy four-way path) | Selection criterion a: zero change to the existing executor four-way path and minimal change when B3 wires the state machine; §6.2 parameter shape and semantics are preserved verbatim | Interface shape differs from the letter of §6.2 | Existing implementations overriding only `score` keep Step 4 behavior automatically; a new test locks the legacy default method ignoring consistency | No |
| STEP8-014 | `QualityScore::overall()` now branches on consistency (None → legacy four-way average byte-identical; Some → five-way equal weight) | §6.2 specifies only the formula, not the carrier; overall needs a single source | Carrier choice | Single-source overall() keeps the gate and the B3-persisted overall column consistent; legacy tests all pass | No |
| STEP8-015 | ConsistencyReport.candidate_count is fixed as "the number of related pages participating in arbitration (related.len())" | §6.1 leaves the field undefined | Field semantics pinned | Comments and tests lock it | No |
| STEP8-016 | Multi-divergence finding hash rule: candidate_value_hash = smallest canonical byte value present on the candidate page (or the group minimum when absent); evidence_value_hash = the smallest distinct value differing from it | Byte order guarantees determinism | Diagnostic hash semantics pinned | Raw text never lands in diagnostics; tests lock it | No |
| STEP8-017 | Provider FTS term construction rules: title first, aliases in order, de-duplicated by char, terms under 3 chars skipped (trigram MATCH floor), 256-char prefix budget, quoted-phrase escaping | Aligned with kernel search conventions and overrun protection | Term-construction rules pinned | SQL stays in the kernel; constant MAX_FTS_QUERY_CHARS=256 | No |
| STEP8-018 | Candidate exclusion implemented as `page_id != ?` (also covering its old generation rows); empty term list / limit=0 returns empty without issuing SQL | Implementation refinement | Exclusion and empty-path semantics pinned | kernel.top_k_related_pages tests lock it | No |
| STEP8-019 | §5.2's dead-letter reason example writes findings[].key as a bare pointer string; the implementation uses a full ClaimKey object {"entity_id","pointer"} | Findings may point at entities other than the candidate page; a bare pointer is not enough to locate them | Diagnostic JSON shape differs from the example | Tests lock the shape; digests stay BLAKE3, no raw text | No |
| STEP8-020 | consistency_conflict review rows fix subject to {"task_id":N} (same discipline as dead letters; UNIQUE+DO NOTHING guarantees one row per task) | §5.2 fixes only the dead-letter subject | Conflict-review subject shape pinned | Tests lock it | No |
| STEP8-021 | enqueue_dead_letter_on_conn is a free pub(super) transaction-internal function rather than §6.3's public associated function on SqliteKernel | Aligns with the existing admit_compile_on_conn precedent; raw tx helpers stay crate-internal | Visibility shape differs from the letter of §6.3 | Semantics unchanged; tests lock idempotency | No |
| STEP8-022 | Stored consistency_json uses the kernel's canonical_text (BTreeMap key-order compact JSON, same discipline as quality_json); hash.rs canonical_json is a type-tagged hash-input encoding, not parseable JSON | "Reuse canonical_json" interpreted as the same canonical discipline | Encoding function choice | Parseable JSON lands in the DB; digests still use the hash-domain encoding | No |
| STEP8-023 | The published consistency value is sourced authoritatively from the arbitration report parameter (same source as report.quality.consistency, behaviorally equivalent) | Avoids dual-write drift | Data source pinned | Tests lock exact SQL NULL/0/1 persistence | No |
| STEP8-024 | Arbitration/retrieval errors fail-closed and stop the run rather than being swallowed as a per-page quality failure | §4's data flow omits the arbitration error branch; unified internal-error handling | Error semantics pinned | Tests lock the propagation path | No |
| STEP8-025 | enqueue_dead_letter_on_conn returns Result<usize> (actual inserted rows) instead of the literal Result<()> | Needed by RecoveryStats.review_inserted for exact counting (DO NOTHING skips are not counted) | Return type changes | Remaining signatures and semantics unchanged; tests lock it | No |
| STEP8-026 | D9's heartbeat period lands as lease_seconds/2 (min 1s); Step 4's existing CompilePolicy.heartbeat_seconds (default 30, validated) is not consumed by this loop | Step 4 already has heartbeat_seconds, coexisting with the D9 suggestion | Two period fields coexist | Main model decides: the heartbeat loop consumes the existing heartbeat_seconds (30s) and the independent lease_heartbeat_interval derivation is removed; policy fields stay single-source | No |
| STEP8-027 | RecoveryStats.dead_quarantined is always 0 on the current recovery path (recovery dead branches only produce failed) | Field kept per §6.3 to stabilize the observability surface | Stable observability surface | Comment documents it; field retained | No |
| STEP8-028 | §5.1 "missing compatibility is tolerated read-only for legacy; real compile with Step 4 data is a config error" — the implementation lets the executor pass through whenever the matrix is missing, without forcing a config error for "missing matrix + existing data" | Step 4/6 baseline tests rerun legacy (no matrix) against existing data; forcing would break the baseline; A13-A15 all assume the matrix exists | Missing matrix does not fail-closed | Main model decides: keep pass-through to preserve the baseline; B6's `domain check` emits an explicit warning for a missing matrix | No |
| STEP8-029 | Violation codes use upper-snake stable codes (CORRUPT_SNAPSHOT etc.) | Aligned with the VALUE_DIVERGENCE convention | Code naming style | Tests lock the stable codes | No |
| STEP8-030 | Preflight runs after the first non-empty batch is fetched and before that batch's admission (domain taken from the first entity) | Reuses the dry-run first-entity domain convention; spec does not specify the executor-side domain determination | Trigger point pinned | Empty-source runs (zero admissions) never trigger | No |
| STEP8-031 | Compatibility-alert subjects render missing schema/prompt versions as "schema_version":null keys | §5.2 only requires including current versions, without specifying the missing form | Missing-form pinned | B3's lenient validation stays compatible | No |
| STEP8-032 | A17's "backfill new task_id" actually backfills the same task id — UNIQUE(entity_id, source_revision, domain_pack_version) guarantees the force re-admission lands on the original task row (epoch+1 is the new identity); a dead-letter approval never creates a second row | Implied by the triple-UNIQUE semantics | Backfill semantics pinned | Tests lock it (same id, epoch 1→2, counters reset, result cleared) | No |
| STEP8-033 | Dead-letter/consistency replay rebuilds PreparedSource along the archived snapshot_hash on the task row (no re-projection), plus a new head-snapshot guard (head missing/revision/snapshot drift → Validation rollback) | Task snapshots store only the knowledge projection; re-projecting computes a different snapshot_hash rejected by the head CAS; the guard also keeps empty facts from triggering a facts CAS write | Replay semantics pinned | Head guard + tests lock it | No |
| STEP8-034 | consistency_conflict approval applies the same strict canonical {"task_id":N} subject validation as dead letters (kernel-generated rows are already canonical; the approve side tightens fail-closed) | B3's insert side only required a JSON object | Validation tightened | Tests lock it | No |
| STEP8-035 | The consistency conversion lands as "create-and-approve": the conversion transaction inserts the supplemental suggestion row + force admission + CAS-approves both rows; the suggestion row is born pending and approved in the same transaction | No separately-approvable intermediate state; complete audit chain | Conversion atomicity | Tests lock it | No |
| STEP8-036 | CompatibilityReport gains a warnings field (skip_serializing_if omits it when empty); missing-matrix warning code=MISSING_COMPATIBILITY_MATRIX | STEP8-028's compensation landing point | Warning carrier | Warningless JSON stays byte-identical to the §6.4 shape | No |

When an implementation finding differs from this specification, append a new `STEP8-xxx` row in the corresponding Chinese and English sections with cause, interface impact, and acceptance change; never silently change D1–D12, the DDL, exit codes, or existing epoch/fencing semantics.

<!-- END STEP8 SPEC v1.0 -->
