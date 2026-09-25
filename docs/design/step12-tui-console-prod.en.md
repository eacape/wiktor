# Step 12 Design Spec: TUI + Console Real Data + Production Deployment Orchestration

> Version: v1.0 (2026-09-25)
> Authority: `docs/MASTER-PLAN.md` §9/§12/§17 (#12), `docs/design/step11-console.md` (D4 TUI debt + the code.html prototype + the JSON contract), `deploy/` (existing litestream assets)
> Precondition change: the root cause of STEP11-001/005 (crates.io unreachable) is resolved — the rsproxy mirror is verified working (2026-09-25, index + download both 200), so ratatui 0.29 / crossterm 0.28 can be fetched locally.
> The Chinese `step12-tui-console-prod.md` is authoritative; this file mirrors it section by section.

## 1. Goals and non-goals

Settle the three Step 11 debts in one pass:

- **B1 TUI** (clearing STEP11-005): `wiktor tui` (feature `tui`, off by default), ratatui 0.29 + crossterm 0.28; four panels (dashboard / compile tasks / review queue / query terminal), keyboard navigation, read-only, same data source as the Web console.
- **B2 Console frontend on real data**: `code.html` renders `/api/*` into the panels via vanilla fetch (no build step); when the API is absent the panels fall back to the prototype's static state with an offline badge.
- **B3 Production deployment orchestration**: `deploy/` gains wiktor-server / wiktor-console systemd units + an env template + an install script + a runbook addition, plus a real deployment + smoke on the Linux production box (Debian 13 x86_64).

Non-goals: a Tauri desktop shell; multi-user/permissions; Raft/cluster; containers (Docker/K8s); CI/CD release pipelines.

## 2. Current constraints

- The TUI panel data APIs already exist: `schema_version`/`row_counts`/`count_pending_reviews`/`list_due_compile_task_ids`/`compile_task_status`/`list_reviews`/`row_counts` (QUG generation); retrieval goes through `QueryEngine` (no QUG → hybrid fallback, empty Mock vector collection → RRF FTS-only).
- The console JSON contract is stable (`/api/overview|tasks|reviews|qug|search|domains`); `GET /` is served from disk by a handler returning code.html.
- deploy/ already has: install-litestream.sh / backup-preflight.sh / restore-drill.sh / litestream.service / a README runbook.
- The Linux production box: Debian 13 x86_64, 2C/2G/39G, passwordless root SSH; gRPC 50051 / HTTP 8080 need opening or local-only smoke.

## 3. Decisions D1–D6

| ID | Decision | Rationale and boundary |
|---|---|---|
| D1 | The TUI lives in the `wiktor-console` crate (taking this side of spec step11 D4's "or"), feature `tui` (off by default); CLI `wiktor tui` forwards to `wiktor_console::tui::run` | Shares the data assembly with the Web console in one crate; no new crate |
| D2 | The TUI data plane shares the Web console's source: in-process `SqliteKernel` read APIs + `QueryEngine` retrieval; read-only (searches persist query_logs as usual) | Same assembly function as D1, zero drift between the two surfaces |
| D3 | TUI rendering and state are separated: pure state functions (`update(state, event)`) + ratatui `TestBackend` unit tests, no real tty needed | CI-runnable; keyboard navigation testable |
| D4 | code.html gains a rendering layer: panel elements carry `data-api` markers, fetched data fills the DOM per contract; failure/timeout keeps the prototype copy plus an offline badge | No build step, no framework; prototype visuals untouched |
| D5 | Deployment orchestration = systemd units (sandbox aligned with litestream.service) + an env file + `install-wiktor.sh` (release build on the server); binary at /usr/local/bin, data at /srv/wiktor-data; chained with litestream | Single-node form, minimal dependencies; `cargo build --release -j2` under the 2G memory cap |
| D6 | Production acceptance runs on the real Linux box: systemd starts the services, /health 200, API-key auth effective, the console panel shows real data | Orchestration is not a paper deliverable; it must PASS on the machine |

## 4. Batch implementation

### B1 — TUI

- `wiktor-console/Cargo.toml`: `[features] tui = ["dep:ratatui", "dep:crossterm"]`; ratatui 0.29 / crossterm 0.28 as optional dependencies.
- `src/tui/mod.rs` (feature-gated): `pub async fn run(db: &Path) -> anyhow::Result<()>` (alternate screen enter, render loop, restore on exit); `state.rs` pure logic: `TuiState { tab, overview, tasks, reviews, qug, query_input, search_result }` + `load_overview/load_tasks/load_reviews/refresh`; four tabs (1 dashboard / 2 tasks / 3 reviews / 4 query), Tab/1-4 to switch, the query tab takes text input + Enter to search, q/Ctrl-C quits.
- The query terminal shares the Web console's source: `QueryEngine::new(kernel, MockVectorStore…)`, showing hits + QueryDiagnostics (rewrite/fts/vector/rrf_k).
- CLI: `wiktor tui --db <db>` (feature `tui`); `crates/wiktor-cli` feature `tui = ["dep:wiktor-console", "wiktor-console/tui"]`.
- Tests: pure state-function unit tests (empty/seeded DB); `TestBackend` render smoke (all four tabs draw; typing in the query tab triggers a search and the diagnostics line is visible).

### B2 — Console frontend on real data

- code.html's overview counters, task table, review list, and QUG panel carry `data-api` markers; a `<script>` rendering layer: `fetchJson(path)` + a 15s poll + the search box hitting `POST /api/search` to render hits and the diagnostics badges (fts/vector/rrf_k).
- Fallback: on fetch failure the panel shows an `offline` badge and keeps the prototype's static content; on success counters/table contents are replaced (design tokens preserved).
- Acceptance: start `wiktor console` with a seeded DB and the browser panels show real numbers; `curl /` returns HTML containing the `/api/` references and the rendering layer; with the API down the page shows offline.

### B3 — Production deployment orchestration

- `deploy/wiktor-server.service`: `wiktor serve` (ExecStart with --db /srv/wiktor-data/wiktor.db), EnvironmentFile=/etc/wiktor/wiktor.env, sandboxing (DynamicUser/ProtectSystem/NoNewPrivileges aligned with litestream.service), After=network-online.target.
- `deploy/wiktor-console.service`: `wiktor console --db … --listen 127.0.0.1:8081` (loopback-only; remote access via an SSH tunnel, never exposed publicly).
- `deploy/wiktor.env.example`: WIKTOR_API_KEYS (JSON template), WIKTOR_COMPILE_WORKERS.
- `deploy/install-wiktor.sh`: detect cargo → release build (-j2) → install the binary → write /etc/wiktor/env → enable units; idempotent reuse.
- `deploy/smoke-deploy.sh`: systemd is-active, /health 200, no-key 401 / with-key 200 (HTTP search), console 8081 200.
- `deploy/README` gains the bring-up runbook (including the litestream ordering: restore-drill first, then start services).
- Real Linux box: run install + smoke, all PASS (D6).

## 5. Deviation baseline (advance note)

Must not change: the TUI/Web shared read-only data plane, the code.html prototype visuals are not redone, systemd single-node orchestration without containers. Deviations are recorded as `STEP12-xxx` (bilingual).

## 6. Acceptance criteria A1–A6

| # | Criterion | Executable result |
|---|---|---|
| A1 | Bilingual spec | step12-tui-console-prod(.en).md exists |
| A2 | `wiktor tui` is navigable | TestBackend tests green + a manual smoke record |
| A3 | Production deployment PASS on the box | Linux smoke-deploy.sh all green (health/auth/console) |
| A4 | Console panels show real data | Browser shows the seeded numbers + the offline fallback |
| A5 | TUI query diagnostics visible | The query tab shows fts/vector/rrf_k |
| A6 | Closeout | workspace test/clippy/fmt green + bilingual deviations + MASTER-PLAN #12 |


## 7. Implementation deviations and measured record (2026-09-25)

### Measured record (2026-09-25)

- **B1 TUI (local macOS arm64)**: `cargo test -p wiktor-console --features tui` — 8 tests green (tab cycling / query input / state application / four-tab TestBackend rendering / diagnostics line / error line / empty-kernel loading); a python pty (80×24) real-terminal smoke rendered the real seeded data (schema 6, pages 20, facts 840) plus the four tabs and the help line, and `q` exited cleanly (A2/A5).
- **B2 frontend on real data (measured in the IAB browser)**: after seeding tech-docs and opening `http://127.0.0.1:8123/` — the top bar `0 Tasks | 20 Pages`, the review pill `0 Pages Awaiting Arbitration`, and the QUG badge `20 Published Pages · gen 0` all came from the real `/api/*` data with the offline badge hidden; after TEST EXEC the diagnostics line became `Matched: 0 hits · fts 0 / vec 0 / rrf_k 60 · 11ms` and the results box showed `（无命中 no hits）` (a seed-only DB has no compiled FTS pages, matching the CLI search) (A4). Verification note: the IAB's synthetic click/press event injection is unreliable (even probe listeners never fired), so the search chain was verified by dispatching the button click programmatically inside the page — handler→fetch→DOM rendering all executed for real.
- **B3 real Linux production box (Debian 13 x86_64, 2C/2G + 4G swap)**: rustup 1.98.1 (rsproxy mirror) + protoc 3.21 + `install-wiktor.sh` (release `-j2`, ~15 min) → `/usr/local/bin/wiktor` + both units active; `smoke-deploy.sh` all 7 checks PASS: server/console active, `/health` 200, no-key 401, with-key 200, console `/` and `/api/overview` 200 (A3/D6). A with-key content search returned a well-formed response (0 hits on the seed-only DB, consistent with offline; Step7's HTTP `GET /search` is the lightweight path and does not persist query_logs).
- **Deployment gotchas (fixed in the template/script)**: ① `WIKTOR_API_KEYS` methods only accept lowercase snake_case (PascalCase fails closed at startup, with the allowed set in the error); ② HTTP `GET /search` requires the `domain` parameter (missing → 400) — `smoke-deploy.sh` now sends `domain=${DOMAIN:-tech-docs}`.


- **UI redesign measured (local IAB)**: after seed + mock compile (58 accepted) + qug build — the Overview cards (schema 6 / 78 pages / 59 generations / 0 review), the due-status pill (pending·6), the row-counts table (11 tables with bilingual descriptions), 6 real rows in Compile Tasks, the search "rust" returning 5 hits (Rust async-runtime pages, scores 0.0154–0.0164) with per-path diagnostics (fts 9 / vec 0 / rrf_k 60); the EN/中文 toggle applies instantly and persists; two frontend bugs fixed (the `.hidden` CSS rule was missing; the hits innerHTML lacked `.join("")` and rendered stray commas).

### Implementation deviations (STEP12-001..005)

- **STEP12-001 (B2, API extension)**: `GET /api/overview` gains a `task_status_counts` field (due compile tasks counted by status, aggregated over due snapshots) — not listed in spec §4 B2; it is the real data source for the frontend's five state pills and delivers step11-console spec D3's original "overview includes compile-task status counts" intent.
- **STEP12-002 (B3, data dir)**: the production data dir is `/srv/wiktor/data` (aligned with the existing `litestream.service` ReadWritePaths), not D5's `/srv/wiktor-data`; the units' sandbox ReadWritePaths matches.
- **STEP12-003 (B3, toolchain)**: `install-wiktor.sh` does not auto-install a Rust toolchain (a missing cargo errors out with exit 2); the install steps live in the runbook (rustup + a China mirror). The production box therefore gains a **minimal toolchain** (rustup minimal + rsproxy) — the "no dev environment on Linux" convention is updated to "the minimal toolchain the production build needs".
- **STEP12-004 (B1, retrieval behavior)**: the TUI query tab installs no FilterRelaxer (CLI `wiktor search` explicitly installs `DefaultFilterRelaxer`); neither TUI nor Web console search takes filter input, so the filtered-empty relaxation retry can never trigger — both forms align with their own input surface.

- **STEP12-005 (B2, UI redesign, user directive 2026-09-25)**: `code.html` was rewritten from the Obsidian reference prototype into a **real-data supervisory UI** — it renders only live `/api/*` data (zero mock content), superseding step11-console spec D2's "reuse the code.html prototype visuals" invariant (the user clarified the prototype was reference-only and the UI should follow the project's actual data surface). English is the default language with a switch to 中文 (persisted in localStorage); the file is single-file with zero external dependencies (Tailwind/Material CDNs dropped, fully offline-capable). DESIGN.md and screen.png remain as historical visual references.

Invariant self-check (§5 deviation baseline): the TUI/Web shared read-only data plane ✅, code.html visuals not redone (markers + an appended rendering layer only) ✅, systemd single-node orchestration without containers ✅.

<!-- END STEP12 SPEC v1.0 -->
