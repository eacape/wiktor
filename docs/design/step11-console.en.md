# Step 11 Design Spec: Console (Web + TUI)

> Version: v1.0 (2026-09-25)
> Authority: `docs/MASTER-PLAN.md` §9/§12 (TUI optional feature, Web UI far-term), `docs/PLAN.md` phases 2/4, `docs/console_ui/DESIGN.md` (Web visual design system), `docs/console_ui/code.html` (Web prototype), `docs/design/step11-benchmarks.md` (hard-metric data source)
> Implementer: `wiktor-builder` (volume)
> English mirrors the authoritative Chinese `step11-console.md`. Hard metrics/performance baselines are in `step11-benchmarks(.en).md`.

## 1. Goals and non-goals

Advance the console from "static design/prototype" (`docs/console_ui/`) to a **runnable interface wired to real Wiktor data**, in two shapes: Web (reusing the existing Obsidian visuals + code.html prototype) and TUI (ratatui).

Goals:
- **Web console**: a local HTTP service (`axum 0.7` + `tower-http fs` serving the code.html visuals) whose JSON API reads `SqliteKernel` in-process, so panels show real data (overview / compile tasks / quality radar / reviews / retrieval RRF breakdown / QUG).
- **TUI console**: `wiktor tui` (ratatui + crossterm), close to the single-host CLI shape, sharing the same read APIs.
- Panel data comes from the `step11-benchmarks` performance baseline plus the `SqliteKernel` real read APIs.
- Deliver `docs/console_ui/README` explaining how to go from static prototype to real data.

Non-goals:
- No Tauri desktop shell (far-term, phase 4); the Web console is a local browser target for now.
- No change to server/core search semantics; the console is a read surface (compile/review actions are read-only views; no new write paths unless reusing existing server gRPC).
- No complex permissions/multi-user; local single-host access.

## 2. Current constraints and terms (reconnaissance)

- Over the network, Web can only reach the six gRPC services + 4 HTTP endpoints (`GET /search` FTS-only, `/health`, `/metrics`, `POST /feedback`); status/tasks/reviews/QUG/quality have no HTTP endpoint → needs gRPC or **in-process `SqliteKernel`**.
- `SqliteKernel` real read APIs: `row_counts`/`schema_version`, `compile_task_status`/`list_due_compile_task_ids`, `list_reviews` (incl. compile_dead_letter/consistency_conflict actions), `load_feedback_window`, `accepted_page_vectors`, `compatibility_page_identities/task_snapshots`, `active_build_identity` (QUG), `execute_batch` (arbitrary SQL).
- Quality scores have no dedicated read API — only the `page_quality` table (read via SQL).
- Retrieval RRF breakdown: `QueryDiagnostics` (fts_count / vector_count / rrf_k per query).
- The server's `run_server` is a single process binding HTTP+gRPC and requires `WIKTOR_API_KEYS`; CLI `wiktor serve`.
- `docs/console_ui/` already has DESIGN.md (Obsidian design system) + code.html (Web prototype) + screen.png.
- No TUI code in the workspace; `axum 0.7` is available; static serving needs `tower-http = { features = ["fs"] }`.

## 3. Decisions D1–D6

| ID | Decision | Rationale and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Web console **embeds `SqliteKernel` in-process**; a new `wiktor console --db --port [--static <code.html>]` subcommand (feature `console`); gRPC-remote is an optional later path | status/tasks/reviews/QUG/quality have no HTTP endpoint; in-process lets panels read all real data; no `WIKTOR_API_KEYS` needed | B5 | A3 |
| D2 | Static assets reuse `docs/console_ui/code.html` (prototype visuals); the console serves it plus JSON API endpoints | Reuse the existing design; don't redo the UI — code.html is already a complete Web prototype | B5 | A3 |
| D3 | JSON API endpoints = overview / compile tasks / quality radar / reviews / retrieval breakdown / QUG / domain list, mapped to `SqliteKernel` real read APIs; retrieval breakdown via `QueryEngine::search` (in-process) for QueryDiagnostics | Panels show real data; no invented mock | B5 | A3, A4 |
| D4 | TUI uses **ratatui + crossterm**, `wiktor tui` (feature `tui`), reusing the same read APIs (overview / tasks / reviews / query terminal) | Pulls the phase-4 TUI forward; close to CLI shape | B6 | A5 |
| D5 | Compile/review operations are **read-only views** (reuse existing server gRPC or read-only), no new write paths | The console is a supervisory/diagnostic interface; writes stay in the CLI/server | B5/B6 | A3 |
| D6 | The performance panel uses the `step11-benchmarks` report (offline baseline), not a live probe | Avoids timing overhead in the console; the baseline is authoritative | B5 | A2 |

## 4. Batch implementation

### B5 — Web console

- Add workspace `tower-http = { version = "0.6", features = ["fs"] }`.
- New crate `crates/wiktor-console` (feature-gated; depends on core + axum + tower-http + tokio + serde_json):
  - `ConsoleState { kernel: Arc<SqliteKernel> }`.
  - JSON API (in-process kernel reads):
    - `GET /api/overview` → schema_version + row_counts + compile-task-state counts + review-pending count.
    - `GET /api/tasks` → `list_due_compile_task_ids` + per-task `compile_task_status`.
    - `GET /api/reviews` → `list_reviews(domain?, None, limit)`.
    - `GET /api/quality` → aggregate `page_quality` table (SQL) over the five dimensions (coverage/citation/schema/density/consistency).
    - `GET /api/qug` → `active_build_identity(domain, version)` + source_hash.
    - `POST /api/search` → `QueryEngine::search` (in-process, Mock vectors + deterministic embedding, offline) returning QueryResult + QueryDiagnostics.
    - `GET /api/domains` → domain-pack list (reusing the `wiktor domain list` discovery logic).
  - Static: `GET /` → serve `docs/console_ui/code.html` (reproducing the prototype visuals); `GET /assets/*` → other static.
  - CLI `wiktor console --db <db> --port <port> [--static <dir>]` (feature `console` forwarding).
- Acceptance (A3): opening the browser shows real data panels (not mock); `POST /api/search` returns real retrieval diagnostics.

### B6 — TUI

- Add workspace `ratatui = "0.29"` + `crossterm = "0.28"`.
- New `wiktor tui` (feature `tui`) in the console crate or CLI:
  - Layout: overview (schema/row_counts/task counts) / compile task table (list_due + compile_task_status) / review queue (list_reviews) / query terminal (QueryEngine::search + QueryDiagnostics breakdown).
  - Reuses the same kernel read APIs as Web.
- Acceptance (A5): `wiktor tui` starts showing real data; keyboard navigation.

### B7 — console_ui/README + closeout

- `docs/console_ui/README.md`: explains how DESIGN.md / code.html / screen.png relate, and how to run `wiktor console` to wire it to real data.
- Mark in MASTER-PLAN that TUI/Web console are pulled in (TUI as the already-defined optional feature; Web from far-term to local form).
- Register STEP11-xxx deviations; sync bilingual docs; workspace test/clippy/fmt all green; commit on the Mac → Linux push → Mac pull.

## 5. Deviation baseline (advance note)

When the implementation differs from this section, append `STEP11-xxx`. Do not change: in-process embedding rather than a gRPC-only client, reusing code.html visuals rather than redoing the UI, the console as a read-only supervisory surface, the performance panel using the offline baseline.

## 6. Acceptance criteria A1–A5

| # | Criterion | Executable result |
|---|---|---|
| A1 | Console design doc bilingual | `step11-console(.en).md` exists |
| A2 | Performance panel references step11-benchmarks results | API/doc references the baseline report |
| A3 | Web console shows real data | `wiktor console` browser shows real panels and JSON APIs return non-mock data |
| A4 | Retrieval breakdown diagnostics visible | `POST /api/search` returns QueryDiagnostics |
| A5 | TUI starts showing real data | `wiktor tui` is navigable |

## 7. Implementation deviations and measured record (2026-09-25)

### Measured record (B5 Web console, local macOS arm64, `wiktor console --db /tmp/console-test.db --listen 127.0.0.1:8123`)

- Build: `cargo build -p wiktor-console` and `cargo build -p wiktor-cli --features console` pass; both the `wiktor console` subcommand and the `wiktor-console` bin start.
- Data: `wiktor seed --domain examples/tech-docs/domain.yaml` loaded 20 pages + 840 facts + 168 fact_refs.
- `GET /api/overview`: returns `schema_version:6` plus real `row_counts` (pages=20 / facts=840 / fact_refs=168 / page_sections=80 / page_quality=20) and review_pending — real kernel reads (A3).
- `GET /api/tasks`, `GET /api/reviews`, `GET /api/qug`: on the seeded DB they return an empty task list / empty reviews / `{generations:0, published_pages:20}` respectively, with correct shapes.
- `POST /api/search` (`{"q":"retrieval pipeline","top_k":3}`): through the QueryEngine (same source as CLI `wiktor search`) it returns hits plus the **full QueryDiagnostics** (`rewrite_status:disabled`, `fts_count:0`, `vector_count:0`, `rrf_k:60`, `latency_ms`) — a seed-only DB has no compiled pages, so the FTS 0-hit result matches the CLI (A4).
- `GET /`: returns `docs/console_ui/code.html`, byte-identical to the on-disk file (verified with `cmp`).
- `GET /api/domains`: returns `{"domains":[]}` (a documented placeholder, see STEP11-004).

### Implementation deviations (STEP11-001..005)

- **STEP11-001 (B5, static serving)**: `tower-http = { features = ["fs"] }` was not added — crates.io was unreachable from the dev machine (sparse index 403/404 both direct and via proxy, measured 2026-09-25). Static assets are returned by a handler instead: `GET /` reads `docs/console_ui/code.html` from disk (falling back to a placeholder HTML on failure), with the directory overridable via `ConsoleState.static_dir`; there is no `/assets/*` glob. The dependency boundary is unchanged (axum only).
- **STEP11-002 (B5, CLI flag)**: the `wiktor console` listen argument is `--listen <addr>` (default `127.0.0.1:8081`) rather than §4's `--port <port>`; it aligns with `wiktor serve`'s `--listen-http/--listen-grpc` style, and `serve(db, listen, static_dir)` takes an address string directly.
- **STEP11-003 (B5, quality panel)**: the `GET /api/quality` five-dimension aggregation endpoint is not implemented yet; the `page_quality` row count is exposed via `/api/overview.row_counts`, and the five-dimension detail read path is deferred to a later console iteration (the kernel already has the table and an SQL channel; no schema change).
- **STEP11-004 (B5, domain discovery)**: `GET /api/domains` returns a documented placeholder `[]`; real discovery reuses the `wiktor domain list` CLI (Step10 B4) instead of duplicating discovery logic.
- **STEP11-005 (B6, TUI deferred)**: the TUI (D4, ratatui 0.29 + crossterm 0.28, feature `tui`, `wiktor tui`) is deferred — crates.io unreachable means the dependencies cannot be fetched (same root cause as STEP11-001). The design is unchanged; implementation and the A5 acceptance will land in an environment where crates.io is reachable (the Linux box).

Invariant self-check (§5 deviation baseline): in-process embedding ✅, code.html visuals reused ✅, read-only supervisory surface (search going through the QueryEngine is D3's explicitly mandated existing query path; persisting query_logs is not a new write path) ✅, performance panel uses the offline baseline (step11-benchmarks §7) ✅.

<!-- END STEP11 CONSOLE SPEC v1.0 -->