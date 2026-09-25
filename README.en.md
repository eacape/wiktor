# Wiktor — knowledge compilation & retrieval middleware

> A "compiling" retrieval database: it compiles unstructured knowledge sources into quality-gated Wiki pages plus a fact plane (the two-plane store) and serves QUG query understanding, hybrid retrieval (FTS5 + vector + RRF), filter pushdown, a feedback loop, and gRPC/HTTP services on top of single-node SQLite. Experience benchmark: Meilisearch; reliability benchmark: etcd.

The Chinese `README.md` is authoritative; this file mirrors it section by section.

## Core features

- **Two-plane data model**: the knowledge plane (compilable Wiki pages) + the fact plane (structured, filterable), dual-written per entity.
- **Compilation pipeline**: an LLM compiles sources into pages; the `require_source_refs` citation contract + four-rule quality scoring + page/task-level recompile brakes + BLAKE3 whole-dependency content hashing (incremental skip).
- **QUG (query understanding graph)**: five edge kinds (synonym/intent/negation/attribute-filter/reverse), offline build, single-transaction atomic publish, audited generations; below a 5pp gain it auto-retires to `disabled` with an explicit hybrid fallback.
- **Hybrid retrieval**: QUG rewrite (optional) → fact-plane filter pushdown → FTS5 + vector two-way recall → RRF fusion; every query returns per-path diagnostics (QueryDiagnostics).
- **Feedback loop**: `POST /feedback` (auth / rate-limit / idempotent) → the standard analyzer surfaces zero-recall blind spots → supplementary compile tasks → recall improves (integration-test proven recall@1 0→1).
- **Reliability contract**: the task state machine (lease / idempotency / dead-letter / compatibility checks), the publish state machine (quarantine never enters the index), and litestream continuous-WAL single-node HA.
- **Plugin ecosystem**: the vector backend (qdrant) and the retrieval outlet (Meilisearch) are independent plugin crates invisible to core; domain packs are the third plugin point.

## Quick start

```bash
# Build the CLI (default features include llm-openai/embedding-http/vector-qdrant; the offline mock path needs none of them)
cargo build -p wiktor-cli

# 1) Create a DB from a domain pack (either official pack)
./target/debug/wiktor seed --db ./wiktor.db --domain examples/tech-docs/domain.yaml

# 2) Compile (MockCompiler runs offline; real providers need llm-openai + keys)
./target/debug/wiktor compile --db ./wiktor.db --domain examples/tech-docs/domain.yaml --provider mock

# 3) Search (hybrid, with per-path diagnostics)
./target/debug/wiktor search --db ./wiktor.db "retrieval pipeline" --json

# 4) Long-running services (six gRPC services + HTTP /search /health /metrics /feedback)
cargo build -p wiktor-cli --features server
WIKTOR_API_KEYS='{"tech-docs":{"secret":"...","methods":["Search","Feedback"]}}' \
./target/debug/wiktor serve --db ./wiktor.db --listen-http 127.0.0.1:8080 --listen-grpc 127.0.0.1:50051 --domain examples/tech-docs/domain.yaml

# 5) Web console (a local read-only supervisory surface)
cargo build -p wiktor-cli --features console
./target/debug/wiktor console --db ./wiktor.db --listen 127.0.0.1:8081
# Open http://127.0.0.1:8081/ in a browser; JSON APIs in docs/console_ui/README.md
```

## Crate layout

| Crate | Responsibility |
|---|---|
| `wiktor-core` | The two-plane SQLite kernel (diesel + FTS5), QueryEngine, QUG, the compile pipeline, consistency/compatibility arbiters, the Mock vector baseline |
| `wiktor-cli` | The single `wiktor` binary: seed/compile/search/qug/eval/feedback/domain/export/status, with optional server/console assembly |
| `wiktor-feedback` | The feedback store trait + the standard analyzer (zero-recall / low-quality-recall / rewrite-failure signals) |
| `wiktor-server` | The unified gRPC (tonic, six services) + HTTP (axum) service face with method-level API keys |
| `wiktor-vector-qdrant` | The qdrant vector-backend plugin |
| `wiktor-adapter-meilisearch` | The Meilisearch retrieval-outlet plugin |
| `wiktor-console` | The Web console (a local HTTP read-only supervisory surface on the Obsidian visuals) |

Feature boundaries: default builds carry no TUI/console/server/Meilisearch; every external service (qdrant, Meilisearch, litestream) is optional.

## Documentation map

- [`docs/MASTER-PLAN.md`](docs/MASTER-PLAN.md) — the master plan (v3.2): architecture, invariants, decisions, delivery dependencies
- [`docs/PLAN.md`](docs/PLAN.md) — the phased execution plan and acceptance criteria
- [`docs/design/`](docs/design/) — Step 1–11 design specs + implementation deviations (bilingual)
- [`docs/domain-pack-guide.md`](docs/domain-pack-guide.md) — the domain-pack contribution guide (the third plugin point)
- [`docs/console_ui/`](docs/console_ui/) — the Web console design system and real-data wiring
- [`examples/milk-tea/`](examples/milk-tea/) and [`examples/tech-docs/`](examples/tech-docs/) — the two official domain packs (seed wikis + fact plane + golden eval sets)

## Status

Steps 1–11 are all delivered (2026-09-25): schema/kernel → query loop → compile pipeline → QUG → feedback loop → gRPC/HTTP services → consistency state machine → litestream HA → plugin ecosystem + second domain pack → performance evidence + Web console. The real-backend effect experiment (qwen compile + qwen embeddings + qdrant) confirmed hybrid retrieval lifts recall@10 from 0.446 (pure FTS) to 0.889 (hybrid) and 0.962 (with QUG).

Tests: `cargo test --workspace`; benchmarks: `cargo bench -p wiktor-core --bench query_bench`.

## License

MIT OR Apache-2.0
