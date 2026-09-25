# Step 13 Design Spec: Production Readiness (replica / real retrieval / feedback loop / observability / CI)

> Version: v1.0 (2026-09-25)
> Authority: `docs/MASTER-PLAN.md` §5.5 reliability contract, §17 delivery dependencies #12 follow-ups; `docs/design/real-backend-experiment.md` (the measured qdrant + HttpEmbedder configuration); `deploy/` (litestream assets); the step10 plugin boundary
> The Chinese `step13-production-ready.md` is authoritative; this file mirrors it section by section.

## 1. Goals and non-goals

Move the production instance from "deployed" to "operable long-term": real vector retrieval, the feedback loop running in production, a backup replica, observability, and CI.

- **B1 Real retrieval in serve**: `wiktor serve` accepts an injected qdrant vector backend + HttpEmbedder + the persistent QUG graph (env-driven, falling back to mock/deterministic offline by default); HTTP `GET /search` switches from the bare kernel FTS to the same QueryEngine as gRPC (persisting query_logs, returning real diagnostics/log_id, with the response shape unchanged).
- **B2 Console placeholder cleanup**: the kernel gains read-only `quality_summary()`/`list_domains()`; the console's `/api/quality` (five-dimension averages) and `/api/domains` (real discovery) land; the UI overview page gains a quality panel and a domain list.
- **B3 Backup replica**: a litestream file:// replica goes live in production (a separate path), with a restore drill against the production DB; SFTP/S3 switch over once user credentials exist.
- **B4 Observability + CI**: Prometheus scrapes `/metrics` (a minimal alert); GitHub Actions runs fmt/clippy/tests (including the tui feature).
- **B5 Feedback-loop E2E**: the production chain search (log_id) → POST /feedback → analyze → review approve → worker compile & publish → re-check.

Non-goals: Raft multi-replica, cluster sharding implementation, a real LLM provider in production (env reserved; keys are user-supplied), Grafana/alerting channels.

## 2. Current constraints (reconnaissance)

- `run_server(ServeOptions)` hardwires `SearchService<MockVectorStore>` + `DeterministicEmbedder(768)` + qug=None (STEP7-012); HTTP `GET /search` goes through bare `kernel.search` (log_id=None, diagnostics={}) — feedback events cannot reference HTTP retrieval logs.
- `VectorStore`/`QueryEmbedder` are both async-trait and object-safe (`Arc<dyn>` works); the server crate must not depend on wiktor-vector-qdrant (the Step10 plugin boundary — the CLI is the assembler).
- `HttpEmbedder::from_env()` exists; the dimension-probing pattern = embed once and take the length (already used by `cmd_vector_build`).
- `/metrics` is already Prometheus text format (6 fixed metrics); `page_quality` has the five dimensions (coverage/citation/schema_compliance/density/consistency?/overall); the pages table carries a domain column.
- litestream 0.5.17 is installed with passing drill scripts; `litestream.service` ReadWritePaths already includes `/srv/backup/wiktor/litestream`.

## 3. Decisions D1–D6

| ID | Decision | Rationale and boundary |
|---|---|---|
| D1 | The vector backend/embedder/QUG graph are **injected by the CLI (the assembler)**: `ServeOptions` gains `vector_store: Option<Arc<dyn VectorStore>>`, `embedder: Option<Arc<dyn QueryEmbedder>>`, `qug: Option<Arc<QugGraph>>`; with None the server keeps today's behavior (Mock + deterministic). Env: `WIKTOR_VECTOR_BACKEND=mock\|qdrant` (default mock) + `WIKTOR_QDRANT_URL/API_KEY`; embeddings follow the existing `WIKTOR_EMBEDDING_*` (deterministic when absent) | Preserves the Step10 plugin boundary (server never depends on the qdrant plugin); the default path runs offline |
| D2 | The server unifies on `QueryEngine<dyn VectorStore>`: once assembled the engine goes into `ServerState`, shared by the gRPC SearchService and HTTP /search; the `GET /search` response **shape is unchanged**, with diagnostics_json/log_id going from placeholders to real values (a behavioral enhancement, backward compatible) | The data prerequisite for the feedback loop; one assembly, zero drift across both surfaces |
| D3 | The kernel adds read-only aggregate read APIs `quality_summary()` (five-dimension + overall averages and counts from page_quality) and `list_domains()` (pages grouped and counted by domain); the console endpoints are thin pass-throughs | The console is a read surface; aggregate SQL belongs to the kernel; no new write paths |
| D4 | The production litestream replica = file:// (`/srv/backup/wiktor/litestream`, a different directory tree from the DB); the restore drill runs against the production DB; SFTP/S3 are configuration switches (runbook steps) pending user credentials | A same-host replica protects against deletion/corruption but not full-disk loss — the limitation is registered; external replica credentials are user input |
| D5 | Observability = Prometheus (the Debian apt package) scraping + a minimal alert rule (`up==0`); the config files live in `deploy/` | /metrics is already in place; 2G of memory tolerates a light scraper |
| D6 | CI = GitHub Actions: fmt --check, clippy --workspace --all-targets -D warnings (including the console tui feature), cargo test --workspace + the tui feature; ubuntu-latest + protobuf-compiler | Regression protection; the runner matches the local gates |

## 4. Batch implementation

### B1 — Real retrieval in serve
- Extend `ServeOptions` (D1) + `assemble_search` supports injection and dimension probing (embed a probe string → len); `run_server` reorders to assemble the engine before building ServerState.
- `SearchService<MockVectorStore>` → `SearchService<dyn VectorStore>` (the grpc router signature follows).
- `http_search.rs`: go through `state.engine.search(&Query)`, filling the response fields one-for-one (rewritten→json, diagnostics→json).
- CLI `wiktor serve`: assemble the store/embedder/qug per env (QUG reuses the CLI search's persistent-loading path: enabled in the domain config → load_active, stale/missing → fallback).
- Tests: the existing 36+ server tests stay green; new assertions that the HTTP search returns log_id/diagnostics; a test for the mock injection path.

### B2 — Console placeholder cleanup
- Kernel: `quality_summary()` / `list_domains()` (read-only SQL; NULL consistency → the average ignores NULLs and reports the sample count).
- Console: real implementations of `GET /api/quality` and `GET /api/domains`; the UI overview page gains a "five-dimension quality averages" bar panel and the domain list.
- Tests: empty and seeded states.

### B3 — Backup replica
- Render `/etc/litestream.yml` (file:// → /srv/backup/wiktor/litestream, db-level sync-interval) → `systemctl enable --now litestream` → verify replication with `litestream status` → run `restore-drill.sh` against the production DB (isolated restore).
- Runbook additions: the SFTP/S3 switch steps + the limitation note (same-host does not survive full-disk loss).

### B4 — Observability + CI
- `deploy/prometheus.yml` (scrape 127.0.0.1:8080/metrics) + `deploy/alerts-wiktor.yml` (up==0); install Prometheus via apt on Debian and verify the target is up with visible metrics.
- `.github/workflows/ci.yml` (D6).

### B5 — Feedback-loop E2E + closeout
- `deploy/smoke-feedback.sh` (run on the box): HTTP search for a log_id → POST /feedback (a zero-recall event) → `wiktor feedback analyze` → `wiktor feedback review approve` → worker compile → re-check the search and row counts.
- The measured production record + bilingual deviations + MASTER-PLAN #13 + commit/push.

## 5. Deviation baseline (advance note)

Must not change: the server never depends on a concrete vector plugin (assembly stays in the CLI), the HTTP /search response shape, kernel read-only aggregates that add no write paths, backup replicas without hard-coded external credentials. Deviations are recorded as `STEP13-xxx` (bilingual).

## 6. Acceptance criteria A1–A8

| # | Criterion | Executable result |
|---|---|---|
| A1 | Bilingual spec | step13-production-ready(.en).md exists |
| A2 | serve assembly is injectable | the mock default is green + the qdrant/http env injection is measured in production |
| A3 | HTTP /search real logs | the production response has non-empty diagnostics_json and log_id |
| A4 | The feedback loop runs in production | smoke-feedback.sh all PASS |
| A5 | Replica and restore | litestream active + restore drill PASS |
| A6 | Observability + CI | Prometheus target up; the CI workflow green |
| A7 | Real console aggregates | /api/quality and /api/domains are non-placeholder |
| A8 | Closeout | workspace test/clippy/fmt green + bilingual deviations + MASTER-PLAN #13 |

## 7. Production record and deviations (2026-09-25)

### Measured record (2026-09-25)

- **B1/B2 production measurement (Linux Debian 13, binary built 2026-09-25 17:37)**: `wiktor serve --domain …/tech-docs/domain.yaml` — both units active; HTTP `GET /search` returns a real `log_id` and a full `diagnostics_json` (rewrite_status/fts_count/vector_count/rrf_k …), rows land in `query_logs` (A2/A3); console `/api/quality` and `/api/domains` return real aggregates (A7 — B2-batch local tests plus non-placeholder production endpoints).
- **B3 replica and restore drill (production DB)**: `/etc/litestream.yml` = file:// `/srv/backup/wiktor/litestream` (db-level sync-interval 1s), `litestream status` = ok (txid 8); the **isolated restore drill PASSed** — an HTTP search triggers a probe write first, then after the sync `litestream restore` into `/srv/wiktor/restore/`: integrity_check=ok, foreign_key_check=0, pages=20/query_logs=11/feedback_events=1 match production per-table, the probe row (max log_id) is present in the copy, and drill artifacts were cleaned up (A5).
- **B4 observability + CI**: Prometheus installed via apt and active; target `wiktor-server` (127.0.0.1:8080) up==1 and `prometheus` up==1; `deploy/prometheus.yml` + `alerts-wiktor.yml` (up==0) committed; `.github/workflows/ci.yml` lands with this batch (A6's CI-green is judged by the Actions run after push).
- **B5 feedback-loop E2E (`deploy/smoke-feedback.sh`)**: full PASS both locally (macOS debug build + a temp DB) and in production — baseline search (0 hits, log_id recorded) → POST /feedback (rate=2) → `feedback analyze --domain-pack` (the zero-recall blind spot surfaces + the STEP13-002 entity enrichment) → `review approve` (supplemental_compile queues a compile) → the worker compiles and publishes → re-check hits 0→1; the local loop closed in 6 seconds (A4).
- **Gates**: workspace `cargo test` green, clippy `-D warnings` at 0 warnings, fmt clean (measured at the 2026-09-25 closeout, A8).

### Implementation deviations (STEP13-001..003)

- **STEP13-001 (B5, feedback-loop bridge, a design call)**: the step6 spec carries an internal gap — L200 mandates the analyzer emit a **report-shaped subject** (normalized_query/log_ids/occurrences/latency) while L272 requires the supplemental_compile subject approved via `review approve` to carry the **five required fields** (entity_id/source_revision/domain_pack_version/source_json/dependencies_json), and nothing bridges the two; the first production smoke exposed it when approve failed Validation (the Step11 integration test went analyze → straight `CompileService::admit`, skipping approve, so it never covered this). Decision: `wiktor feedback analyze` gains an **optional `--domain-pack <PATH>`** — zero-recall suggestions are deterministically matched ("normalized query == normalized entity `title` field; the lexicographically smallest entity_id wins ties") and the five-field subject is backfilled (the original normalized_query/log_ids/occurrences/avg_latency_ms keys are preserved for audit); on a miss the report shape stays (approve rejects it = fail-closed, content must exist first). Note: enrichment changes part of the UNIQUE(domain,action,subject_json) idempotency key — idempotent within one mode; mixing the two modes can carry report-shaped and entity-shaped rows side by side (the report-shaped row remains the blind-spot record). No new kernel write paths, no change to the approve validation, and the human review gate is untouched.
- **STEP13-002 (B5, smoke query choice)**: the original B5 smoke queried `aerospike` — that word exists nowhere in the tech-docs corpus (20 seed-wiki pages + 120 docs.jsonl entities), so no supplemental compile could ever raise the hit count and the loop premise was unsatisfiable; it is replaced by "the title of an uncompiled document entity in the pack" (`gRPC 快速上手（concept）`), which makes "blind spot → supplemental compile → hit improvement" genuinely closable.
- **STEP13-003 (B3, restore-drill shape)**: the Step6/Step9 `restore-drill.sh` builds its own disposable test DB and refuses production paths per A9, so it cannot validate the **production replica chain** directly; the Step13 production drill used the equivalent isolated procedure: online `litestream restore` of the production replica → integrity/foreign-key/per-table row parity → probe-row verification → cleanup, never touching the production DB itself (spec B3's "against the production DB (isolated restore)" is executed this way).

<!-- END STEP13 SPEC v1.0 -->
