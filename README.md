<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/brand/wiktor-web-dark.svg">
  <img src="docs/brand/wiktor-web-light.svg" alt="Wiktor" width="280">
</picture>

<h1>Wiktor</h1>

<h3>A compiling knowledge-retrieval database — Meilisearch-grade retrieval, etcd-grade reliability</h3>

<p>
  <img src="https://img.shields.io/badge/Rust-1.85%2B-dea584?style=flat&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/storage-SQLite%20%2B%20FTS5-003B57?style=flat&logo=sqlite&logoColor=white" alt="Storage">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-green?style=flat" alt="License"></a>
  <a href="https://github.com/eacape/wiktor/actions"><img src="https://img.shields.io/github/actions/workflow/status/eacape/wiktor/ci.yml?branch=main&style=flat&label=CI" alt="CI"></a>
  <a href="https://crates.io/crates/wiktor-core"><img src="https://img.shields.io/crates/v/wiktor-core?style=flat" alt="crates.io"></a>
  <a href="https://docs.rs/wiktor-core"><img src="https://img.shields.io/docsrs/wiktor-core?style=flat" alt="docs.rs"></a>
</p>

[中文](README.CN.md) | **English**

</div>

---

Wiktor is a "compiling" retrieval database: it compiles unstructured knowledge sources into quality-gated Wiki pages plus a fact plane (the two-plane store) and serves QUG query understanding, hybrid retrieval (FTS5 + vector + RRF), filter pushdown, a feedback loop, and gRPC/HTTP services on top of single-node SQLite. Docs in `docs/` are bilingual (`*.md` Chinese-authoritative, `*.en.md` mirrors).

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
# Install the CLI (crates.io; add --features server,console for the service/console surface)
cargo install wiktor-cli
# or build from source:
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
- [`docs/design/`](docs/design/) — Step 1–14 design specs + implementation deviations (bilingual)
- [`docs/domain-pack-guide.md`](docs/domain-pack-guide.md) — the domain-pack contribution guide (the third plugin point)
- [`docs/console_ui/`](docs/console_ui/) — the Web console design system and real-data wiring
- [`docs/brand/`](docs/brand/) — brand assets (the spider-web logo above)
- [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`CHANGELOG.md`](CHANGELOG.md) — contributing guide and per-release changelog
- [`deploy/README.md`](deploy/README.md) — production deployment runbook (install, ops, backup/restore, rollback)
- [`examples/milk-tea/`](examples/milk-tea/) and [`examples/tech-docs/`](examples/tech-docs/) — the two official domain packs (seed wikis + fact plane + golden eval sets)

## Wiktor in context

Wiktor's engineering baseline is "Meilisearch-grade retrieval, etcd-grade reliability",
and its kernel mirrors how rqlite wraps SQLite (we do not reinvent storage/BM25).
The table below positions it among the closest ecosystems; the "open source" column
is the honest license posture, and the "compile observability" column reflects how a
quality-gated compilation pipeline is (or isn't) surfaced.

| Project | What it is | Compile observability | Plugin ecosystem | Open source | Retrieval / focus |
|---|---|---|---|---|---|
| **Wiktor** | Compiling knowledge-retrieval DB: LLM compiles sources into quality-gated Wiki pages + a fact plane, over single-node SQLite | First-class: per-query `QueryDiagnostics`, five-dimension quality scoring, quarantine state machine, feedback loop that ships a report with each blind spot | Three points: data-source adapter / domain pack / vector backend (+ retrieval-outlet plugin) | Apache-2.0, fully open (core + all plugins) | Hybrid FTS5 + vector + QUG; feedback-proven recall@1 0→1 on the closed loop |
| **Meilisearch** | Instant, typo-tolerant search engine (Rust) | Limited: relevance tuning knobs, not a compile-or-quality model | Plugins/engines integrations | MIT, open | Fast full-text `search-as-you-type`; no LLM compilation, no two-plane store |
| **qdrant** (as vector backend) | Dedicated vector database | Vector-level only (collections/points/payload) | Broad client ecosystem | Apache-2.0, open | ANN similarity; no knowledge compilation or retrieval orchestration |
| **Vectara** | Hosted RAG: index + retrieve + grounded summaries | Hosted dashboard, API metrics | SaaS API only | Closed (hosted) | Managed RAG with grounded citations; not self-hostable OSS |
| **Pinecone** | Hosted vector database | Vector/usage metrics, no compilation model | SDKs + integrations | Closed (hosted) | Managed ANN similarity; no compile pipeline or fact plane |
| **Weaviate** | Vector database (open + hosted) | Vector schema + modules | Modules (vectorizers, hybrid) | BSD-3, open core | Vector + hybrid; no LLM knowledge-compilation stage |
| **LangChain** | LLM-app orchestration framework (not a DB) | App-layer; you wire retrieval yourself | Very large integration surface | MIT, open | Framework composing LLM + retrievers; no quality gate or persistence engine by itself |
| **WeKnora** | RAG middleware/platform (Tencent) | Some pipeline/metrics | Plugin-ish | Open-sourced parts | Hybrid retrieval + rerank for RAG; heavier middleware stack |
| **rqlite** (as kernel analog) | Distributed SQLite (Raft) | DB-level only | None needed | MIT, open | Distributed SQLite; no retrieval/compile layer |

The takeaway: Wiktor is **not another vector store**. Its slot is the LLM
knowledge-compilation layer + quality gate + feedback-observable closed loop that
sits *in front of* storage and retrieval — a "compiling retrieval database" rather
than a similarity index. Vector backends (qdrant) and retrieval outlets
(Meilisearch) are plug-in surfaces, not the product identity. See
[`docs/MASTER-PLAN.md`](docs/MASTER-PLAN.md) for the full architecture and design
decisions.

## Status

Steps 1–14 are all delivered (2026-09-27): schema/kernel → query loop → compile pipeline → QUG → feedback loop → gRPC/HTTP services → consistency state machine → litestream HA → plugin ecosystem + second domain pack → performance evidence + Web console → feedback semantic-match abstraction → single-process multi-domain serve + presentation convergence → ops/open-source face. The real-backend effect experiment (qwen compile + qwen embeddings + qdrant) confirmed hybrid retrieval lifts recall@10 from 0.446 (pure FTS) to 0.889 (hybrid) and 0.962 (with QUG).

Tests: `cargo test --workspace` (417 green); benchmarks: `cargo bench -p wiktor-core --bench query_bench`.

## Roadmap

- **Production hardening**: multi-node HA (litestream replication → Raft) and cluster sharding (see `docs/design/step10-cluster-sharding.md`).
- **Real-benchmark release**: publish a reproducible benchmark on the official domain packs.
- **Community & plugins**: a third-party plugin walkthrough, then a release pipeline (GitHub Releases + prebuilt binaries + crates.io publish).
- **Feature-gated ecosystem**: TUI/console/server/Meilisearch already feature-gated; the remaining optional vector backends (lancedb / pgvector / …) stay behind the `VectorStore` trait.

Everything is tracking MOSS-observable, single-node-first: each release is self-hostable with SQLite + optional qdrant.

## Getting help

- [Issue tracker](https://github.com/eacape/wiktor/issues) — bugs, feature requests, and design discussions.
- [Contributing](CONTRIBUTING.md) — how to build, test, and open a pull request.
- [Code of Conduct](CODE_OF_CONDUCT.md) and [Security](SECURITY.md) — expectations and the responsible-disclosure policy.
- [Changelog](CHANGELOG.md) — notable changes per release.

## License

Licensed under [Apache-2.0](LICENSE) — free to use, modify, and distribute, including in commercial products.
