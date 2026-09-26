# Step 14 (P4) Design Spec: Single-Process Multi-Domain Serve + Presentation-Layer Convergence

> Version: v1.0 (2026-09-26)
> Authority: `docs/MASTER-PLAN.md` §16 MVP (single process, single binary, single SQLite), §5.4 full query path, §17 delivery dependencies
> Implemented object: `wiktor-server` multi-domain serve wiring + `wiktor-console`/TUI multi-domain data plane (implemented by the lead model; this document registers the spec)

## 1. Background and Goal

The current serve is **single-domain**: `ServeOptions.domain_pack` is a single `Option` → one process = one db + at most one domain pack + one compile worker + one query engine + one qdrant collection. P4 has two halves:

1. **Single-process multi-domain serve**: one serve process serves multiple domains at once (each with its own retrieval and compile surfaces, sharing a single SQLite library file).
2. **Presentation-layer convergence**: the console (Web) and TUI drop the `milk-tea` hard-code, provide a multi-domain data plane with domain selection, and converge onto the same read surface.

## 2. Verified Status Quo (Design Premises)

- **Serve wires a single domain**: `serve.rs::assemble_state_and_search` reads `config.name` from the single `domain_pack` (a parse failure silently falls back to `"default"`, serve.rs:128-136), builds **one** engine + `ensure_collection(&domain_name, ...)` (serve.rs:155-158). `run_server` starts at most **1** `CompileWorker` (serve.rs:78-92).
- **The DB retrieval surface is already column-level multi-domain**: `pages`/`query_logs`/`feedback_events`/`review_queue`/`feedback_rejections`/`qug_builds` all carry a domain column; `kernel.search` filters by domain, `list_domains()` already exists (sqlite.rs:378). `SqliteKernel` is constructed by db path; domain is a method argument / table column, **not a constructor parameter**.
- **The compile surface has no domain column (the deep-water zone)**: `facts`/`fact_refs`/`page_quality`/`page_sections`/`compile_tasks`/`qug_edges` have no domain column and rely on entity_id global uniqueness (`review_domain_of` reverse-derives the domain from the `domain:type:slug` prefix).
  **The key mitigation**: compile claim operates on a **`task_ids` array** (the admission set passed in when building the run), **not a global scan by domain column** (compile_store.rs `claim_in_transaction`, `SQL_CLAIM_NEXT` expands the passed task_ids via json_each). Therefore, with **one worker per domain, each admitting only its own source from its own domain.yaml**, tasks are naturally isolated per domain — **no domain column needs to be added to compile_tasks**. This is the basis on which this document dares to "do both halves".
- **qdrant is named per domain**: `ensure_collection(&domain_name, ...)`, so the collection name = domain_name, naturally isolated.
- **Requests already carry a domain and are key-validated**: HTTP `/search`, `POST /feedback` and gRPC Search all require `params.domain == authed.domain` else 403; `Query.domain = Some(params.domain)`.
- **The presentation layer is hard-coded**: `wiktor-console/src/lib.rs:64-67` and `tui/state.rs:177-193` both hard-code `ensure_collection("milk-tea",...)` + `QueryEngine::new(...,"milk-tea",...)`; the Web console panels are full-library aggregates with only `/api/search` able to carry an optional domain; `/api/domains` can already discover multiple domains.

## 3. Decisions

### D-P4-1: Multi-domain assembly shape
`ServeOptions.domain_pack` changes from `Option<PathBuf>` (single) to **`Vec<PathBuf>` (multi, possibly empty)**; empty → no compile worker and no retrieval surface (a pure status/supervision service). Backward compatible: the old single `--domain <path>` merges into the Vec. The CLI `wiktor serve --domain` becomes a repeatable flag; the legacy bin's `WIKTOR_DOMAIN_PACK` env supports colon-separated packs.

### D-P4-2: Multi-engine organization
`ServerState.engine` changes from a single `Arc<QueryEngine<dyn VectorStore>>` to **`HashMap<String, Arc<QueryEngine<dyn VectorStore>>>`** (key = domain_name). Each domain gets its own engine (own domain name, candidate_multiplier, QUG, collection name). The vector store / embedder share their connection instances; the different collection names isolate them.

### D-P4-3: Request routing (dispatch layer)
gRPC `SearchService` and HTTP `/search` dispatch **by the request `domain` against the map**: on a hit → the matching engine; an unserved domain → `NOT_FOUND` (distinct from the auth 403: key-authorized but the domain is not wired). `ServerState` exposes `engine_for(domain) -> Option<Arc<QueryEngine<dyn VectorStore>>>`. Aggregate read surfaces such as `/api/overview` return multi-domain summaries.

### D-P4-4: One compile worker per domain
`run_server` iterates the domain packs, building one `CompileWorker::from_domain_pack` per pack and `.spawn`-ing it; collects them into a Vec joined on shutdown. **Safety basis in §2's deep-water mitigation** (claim is isolated by task_ids).

### D-P4-5: Presentation-layer convergence
- **Drop the milk-tea hard-code**: `wiktor-console`/`tui` no longer fix `"milk-tea"`; they discover compiled domains from `SqliteKernel::list_domains()` and build an engine per domain (or share the kernel, using it directly for the hyper tables).
- **Domain selector**: the Web console gains a global domain dropdown at the top (data from `/api/domains`); `/api/search`'s domain input is linked to it; the TUI gains domain selection (a shortcut) and `run_search` sends the selected domain.
- **Unified data plane**: the overview/tasks/review/search panels all go through the kernel aggregate read surface with optional domain filtering, and Web and TUI share the same source (continuing STEP12 B1's "shared data plane").

## 4. Files Involved

| Layer | File | Change |
|---|---|---|
| serve | `crates/wiktor-server/src/serve.rs` | `ServeOptions.domain_pack`→Vec; `assemble_state_and_search` builds an engine per domain into a map; `run_server` spawns/joins multiple workers |
| server state | `crates/wiktor-server/src/state.rs` | `engine` field becomes a map; add `engine_for`; `ServerState::new` signature takes the map |
| gRPC | `crates/wiktor-server/src/services/search.rs` | `SearchService` holds the map, dispatches by req.domain |
| HTTP | `crates/wiktor-server/src/http_search.rs` | `/search` dispatches by params.domain; `/api/domains` returns served domains |
| CLI | `crates/wiktor-cli/src/main.rs` | `wiktor serve --domain` repeatable; `WIKTOR_DOMAIN_PACK` colon-separated; legacy bin synced |
| console | `crates/wiktor-console/src/lib.rs` | drop milk-tea hard-code, multi-domain engine map + domain-selector backend |
| TUI | `crates/wiktor-console/src/tui/state.rs` + `mod.rs` | drop milk-tea hard-code, multi-domain read surface + domain selection |
| Web console | `docs/console_ui/code.html` | top domain dropdown, linked to /api/search |

## 5. Boundaries and Invariants

- **Compile isolation**: each domain worker admits only its own domain.yaml source; claim uses its own task_ids; entity_id keeps the `domain:` prefix for global uniqueness → no new domain column.
- **Retrieval isolation**: the request domain must equal the key's domain (403, unchanged); it must hit a wired engine (else NOT_FOUND, new).
- **Offline baseline**: with no injection and empty domain packs, the server can start as a pure status/supervision service; with packs, each domain uses the mock/deterministic defaults.
- **Idempotency/determinism**: multi-domain does not change single-domain engine behavior; tasks/publish semantics for a domain without one are unchanged.
- **Compat**: single-domain usage (the old single `--domain`/`WIKTOR_DOMAIN_PACK` value) behaves identically to today.

## 6. Phased Landing

- **Phase A (core multi-domain serve)**: ServeOptions/state/engine map + SearchService/HTTP dispatch + CLI multi-flag + single/multi worker. Phase A completion = "single-process multi-domain serve".
- **Phase B (presentation-layer convergence)**: console/TUI drop hard-code + domain discovery/selector + code.html dropdown.
- The two phases are independently acceptable; landed in order, each with a green gate. Implementation deviations are registered retroactively in this document (per the STEP convention) → append a change log.