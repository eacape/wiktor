# console_ui — Web console design system and real-data wiring

> This directory holds the visual design assets of the Wiktor Web console (Step 11 B5) plus the real-data wiring guide. The Chinese `README.md` is authoritative; this file mirrors it section by section.

## 1. How the three files relate

| File | Role |
|---|---|
| `DESIGN.md` | The Obsidian-theme design system (palette / typography / spacing tokens) — the **single source of truth** for visuals |
| `code.html` | The full Web prototype (single file, inline CSS/JS), built on `DESIGN.md` tokens with the overview / compile tasks / quality radar / reviews / retrieval breakdown / QUG panels |
| `screen.png` | A rendered screenshot of the prototype, for review and regression comparison |

Relationship: `DESIGN.md` defines tokens → `code.html` consumes them to present the prototype → `screen.png` snapshots the result. Change visuals in `DESIGN.md` first, then sync `code.html`, so the two never drift.

## 2. From static prototype to real data

The prototype itself fetches nothing; real data comes from `wiktor console` (the new `wiktor-console` crate, Step 11 B5) — a local HTTP service that reads `SqliteKernel` in-process and serves `code.html` verbatim at `/`:

```bash
# 1) Create a DB and load data (pick any domain pack)
wiktor seed --db ./wiktor.db --domain examples/tech-docs/domain.yaml

# 2) Start the Web console (the console feature is off by default)
cargo build -p wiktor-cli --features console
./target/debug/wiktor console --db ./wiktor.db --listen 127.0.0.1:8081

# 3) Open http://127.0.0.1:8081/ in a browser (prototype visuals)
#    The JSON APIs are the real data surface:
curl http://127.0.0.1:8081/api/overview          # schema + row_counts + review_pending
curl http://127.0.0.1:8081/api/tasks             # due compile tasks
curl http://127.0.0.1:8081/api/reviews           # review queue
curl http://127.0.0.1:8081/api/qug               # QUG generation/publish state
curl -X POST -H 'Content-Type: application/json' \
     -d '{"q":"retrieval","top_k":5}' \
     http://127.0.0.1:8081/api/search            # hybrid retrieval + QueryDiagnostics breakdown
```

For the endpoint contract and implementation deviations (STEP11-001..005, covering the absent tower-http and the deferred TUI) see `docs/design/step11-console(.en).md` §7; for the performance baseline see `docs/design/step11-benchmarks(.en).md`.

## 3. Boundaries

- The console is a **read-only supervisory surface**: `POST /api/search` goes through the same QueryEngine as CLI `wiktor search` (the query log is persisted to `query_logs` as usual); other than that it writes nothing — compile/review writes stay in the CLI/server.
- **`code.html` is wired to real data (Step12 B2)**: when served by `wiktor console`, the panels poll `/api/*` via vanilla fetch and render (15s interval + the live search box); when the API is unreachable they fall back to the prototype's static copy with an `offline` badge — opening the file directly from disk always shows the prototype state.
- The TUI (`wiktor tui`, ratatui, feature `tui`) shipped in Step12 B1: the same data plane as the Web console, four tabs with keyboard navigation; see `docs/design/step12-tui-console-prod(.en).md`.
