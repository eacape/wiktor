# Step 10 Design Spec: Plugin Ecosystem, Second Official Domain Pack, and Cluster-Sharding Plan

> Version: v1.0 (2026-09-24)
> Authority: `docs/MASTER-PLAN.md` v3.2 §5.6, §9, §12 decision #8, §14 decided #1/#2, §17 landing dependency #10
> Implementer: `wiktor-builder` (volume); the design-level cluster sharding is authored by the main model
> Chinese is the authoritative design; this English file mirrors it section-by-section
> (`step10-plugin-ecosystem.md`). Cluster sharding is a separate doc: `step10-cluster-sharding(.en).md`.

## 1. Goals and non-goals

This Step splits landing dependency #10 "cluster sharding + plugin ecosystem (Meilisearch outlet, external vector libraries) + second official domain pack (technical documentation)" into four sub-deliverables.

Goals:
- **Second official domain pack (technical docs)**: add `examples/tech-docs/`, proving the domain-pack plugin point extends to a non-ecommerce domain.
- **Generalize the golden filters**: `GoldenFilters` no longer hardcodes milk-tea fields (price/sugar/size/ingredients); use a domain-generic fact-field filter expression, mechanically migrate milk-tea's 134 golden records, and keep all assertions green.
- **Split the external vector library into a crate**: move `QdrantVectorStore` out of core into the `wiktor-vector-qdrant` plugin, verifying "plugins depend only on core, not on server" and the "< half a day integration cost" target.
- **Meilisearch outlet**: add the optional `wiktor-adapter-meilisearch` plugin and a `wiktor export meilisearch` CLI, mirroring accepted pages into an external search engine.
- **Cluster-sharding plan**: deliver a design doc only (shard key / routing / rebalance / cross-shard consistency / link to the deferred rqlite-Raft path), no implementation.

Non-goals:
- No real cluster sharding / cross-machine shards (2 GiB single host + Raft explicitly deferred).
- No wiring of Meilisearch into the default query path (the external engine is an optional plugin, not the product identity).
- No empty-shell plugin crates; only split the currently-used qdrant backend.
- Do not change the single-writer, CAS, content-hash, QUG, or publish-transaction contracts.

## 2. Current constraints and terms

- Workspace has 4 crates: `wiktor-core/cli/feedback/server`; no plugin crates yet.
- `VectorStore` trait (`traits/vector_store.rs`) is the generic `QueryEngine<V>`, not `Box<dyn>`; `QdrantVectorStore` (core feature `vector-qdrant`) + `MockVectorStore` (always-on).
- The embedding seam is `Arc<dyn QueryEmbedder>` (object-safe), cleanly swappable.
- `kernel::accepted_page_vectors(domain)` returns accepted pages (page_id/entity_id/title/content/content_hash/generation) — the ready-made read surface for external export.
- `GoldenFilters` (`eval/mod.rs:160`) is a milk-tea-hardcoded flat struct; `golden_filters_to_filters` (`eval/runner.rs:116`) maps it onto the domain-neutral `FilterCondition` (`types/mod.rs:82`: NumericRange/TextEquals/RefContains/RefExcludes).
- A domain pack is `examples/milk-tea/` (there is no `domains/` dir; the CLI takes `--domain <path>` explicitly).
- Network: GitHub is walled on the Mac; uploads go via the Linux SSH remote push; downloads use the Clash proxy 127.0.0.1:7897.

## 3. Decisions D1–D8

| ID | Decision | Rationale and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Second domain pack lives in `examples/tech-docs/`, alongside milk-tea, reusing the same `domain.yaml` contract and seed/eval paths. | A domain pack is a directory + domain.yaml + data; no new registration mechanism. | B1 | A1 |
| D2 | tech-docs fact plane = documents (docs-as-code entries): `topic` (primary knowledge page) / `level` / `format` / `audience_years` / `tags` filterable — entirely different from milk-tea's price/sugar/size/ingredients. | Proves the golden-filter generalization is necessary. | B1/B2 | A1, A3 |
| D3 | `GoldenFilters` changes from a milk-tea-hardcoded struct to a **domain-generic filter expression** isomorphic to `FilterCondition` (tagged enum, keyed by fact-field name); `golden_filters_to_filters` degrades to per-item mapping; milk-tea's 134 golden records are mechanically migrated while keeping 134 + quotas + 34-legacy assertions. | Removing the field hardcode is the precondition for a second domain to run eval; the isomorphism keeps translation cost zero. | B2 | A3, A4 |
| D4 | Only qdrant (the external vector library) is split into the `wiktor-vector-qdrant` plugin crate; `MockVectorStore` stays in core as the evaluation baseline (per §394 "in-memory brute-force first as a core module; external vector libraries split out later"). | Minimizes risk to the 391 tests while fully proving the plugin pattern. | B3 | A5 |
| D5 | Plugin crates depend only on core (+ their own transport), never on server/feedback; the CLI is the assembler and depends on plugins. | MASTER-PLAN §8 dependency rule. | B3/B4 | A5, A6 |
| D6 | The Meilisearch outlet is export/sync (not default search): `wiktor export meilisearch` reads accepted pages and PATCHes them to `${WIKTOR_MEILISEARCH_URL}/indexes/{domain}/documents` (env config, optional api key). | The external search engine is an optional plugin, not mixed into the default path (§154). | B4 | A6 |
| D7 | Cluster sharding only produces a design doc (`step10-cluster-sharding(.en).md`); no crate, no implementation. | Single-host hardware + deferred Raft; real sharding is not landable. | B5 | A7 |
| D8 | New features are all **optional** (CLI feature forwarding); the default query path of `wiktor-core`'s default features is unchanged; any failure must not break the existing 391 tests. | Pluggable plugins, a stable default path. | all | A2–A6 regression |

## 4. Batch implementation

### B1 — Second official domain pack `examples/tech-docs/`

New directory containing:
- `domain.yaml`: `name: tech-docs`; entity `document` with fact fields `title/description/topic(reflist)/level/text/format/text/audience_years/numeric/tags(reflist)`, filterable = topic/level/format/audience_years/tags; `query.filters` matching; compile 0.75/2; qug enabled pointing to `intents.yaml`.
- `seed-wiki/*.md`: ~20 technical-doc knowledge pages (concept/technology/practice) with the frontmatter contract `tech-docs:<type>:<id>` (title/aliases/tags/entity_type/entity_id/page_id), body split by `##` H2.
- `gen_docs.py` + `docs.jsonl`: deterministic ~120 doc fact records (entity_id `tech-docs:document:doc_XXXX`).
- `intents.yaml`: expansion/attribute/negation, referencing tech-docs fact fields.
- `golden-queries.jsonl`: ~134 records (34 legacy + 100 new: synonym25/intent20/negation20/attribute_filter20/negative15), deterministically generated by `gen_golden.py` and hitting the quotas.
- `README.md` (optional): tech-docs domain-pack notes.

Acceptance (A1): `wiktor seed --domain examples/tech-docs/domain.yaml --db <tmp>` succeeds; `wiktor eval --golden examples/tech-docs/golden-queries.jsonl --domain .../domain.yaml` runs green (tiers A/B/C and QUG decision executable).

### B2 — Generalize the golden filters (core eval change)

- Add `GoldenFilterCondition` (tagged enum, `#[serde(tag="type", rename_all="snake_case")]`) with variants aligned to `FilterCondition`: `NumericRange{field,min,max}` / `TextEquals{field,value}` / `RefContains{field,refs}` / `RefExcludes{field,refs}`; `GoldenFilters { conditions: Vec<GoldenFilterCondition> }`.
- `golden_filters_to_filters` becomes per-item mapping `GoldenFilterCondition → FilterCondition` (field names pass through; drop the price→price / sugar→sugar_level / ingredients→ingredient_ids hardcoding).
- `filter_signature` switches to the generic expression.
- Migration script: rewrite the `filters` of milk-tea's 134 golden records from flat keys to the generic form (price_min/price_max→`{"type":"numeric_range","field":"price",...}`; size→text_equals; ingredients→ref_contains; exclude_ingredients→ref_excludes; `{}`→`[]`). Keep the 134 total, 34 legacy, and per-kind quotas unchanged.
- Add a tech-docs eval integration test (reuse the existing golden harness, covering the tech-docs domain + generic filters).

Acceptance (A3/A4): workspace `cargo test` all green; the eval.rs 134/quota/legacy assertions still hold; tech-docs eval green.

### B3 — Split the qdrant vector backend into `crates/wiktor-vector-qdrant`

- New crate `crates/wiktor-vector-qdrant` (depends only on core + qdrant-client): migrate `QdrantVectorStore`, `point_id`, `collection_name`, `from_config`/`connect`.
- core removes: the `vector-qdrant` feature, the qdrant-client optional dependency, the `qdrant_vector` module and its re-export; `tests/qdrant_integration.rs` moves into the new crate.
- CLI: `wiktor-cli` depends on `wiktor-vector-qdrant` (feature forwarding; `default` does not force it), `cmd_vector_build` uses the plugin type; `vector-qdrant` becomes a CLI feature forwarded to the plugin.
- The workspace root moves the qdrant-client dependency into the plugin crate.

Acceptance (A5): workspace build/tests all green; the new crate builds standalone and its Cargo.toml depends only on core; `wiktor vector build` works with mock and real qdrant.

### B4 — Meilisearch outlet `crates/wiktor-adapter-meilisearch`

- New crate depends on core + reqwest (reuse the rustls stack; no new HTTP crate): `MeilisearchExporter` consumes `kernel::accepted_page_vectors` into documents, `PATCH {base}/indexes/{domain_or_index}/documents`; env `WIKTOR_MEILISEARCH_URL` (default http://localhost:7700), `WIKTOR_MEILISEARCH_API_KEY` (optional).
- CLI subcommand `wiktor export meilisearch --db <db> --domain <path>` (feature `export-meilisearch`); ensure/create the index before export (`PUT /indexes/{name}`).
- Test: a local mock HTTP server (axum or blocking tcp) captures the PATCH, asserting the document body contains page_id/title/content/entity_id/generation and that only accepted pages are exported.

Acceptance (A6): `wiktor export meilisearch` mirrors accepted pages into the mock Meilisearch; quarantine pages are not exported; the integration-cost path (env + subcommand) is independently verifiable.

### B5 — Cluster-sharding plan (design doc only)

`docs/design/step10-cluster-sharding(.en).md`:
- Shard-key selection (domain/entity_id hash vs range), routing layer (on top of the existing gRPC/HTTP server), cross-shard consistency (single-writer boundary, two-plane rebuildability), rebalance (generation/epoch), the link to the deferred rqlite Raft (WAL as Raft log), migration path, and an explicit boundary of what is not implemented.
- No code, no new crate.

Acceptance (A7): design doc delivered; MASTER-PLAN #10 marks "cluster sharding = design plan".

### B6 — Closeout, bilingual sync, and push

- Mark MASTER-PLAN #10 ✅ (noting second domain pack / vector split / Meilisearch implemented + cluster sharding as a design plan).
- Add/complete a domain-pack contribution guide ("how to add a third domain pack"), merged into the README or a standalone `docs/domain-pack-guide(.en).md`.
- Register implementation deviations in the step10-plugin-ecosystem spec (STEP10-xxx, written back during implementation).
- Sync all new/changed docs bilingual; workspace `cargo test` + clippy + fmt all green; commit on the Mac → tar over SSH (excluding `._*`) → Linux push → Mac pull.

## 5. Deviation baseline (advance note)

When the implementation differs from this section, append `STEP10-xxx` with cause, interface impact, and acceptance changes; do not change: the golden-filter "pass field names through, no hardcoding" direction, plugins depending only on core, Meilisearch as an optional export, cluster sharding not implemented.

### 5.1 Implemented deviations (registered 2026-09-24)

| ID | Deviation | Cause and treatment |
|---|---|---|
| STEP10-001 | `GoldenFilters` serializes as `{"conditions":[...]}` rather than the spec draft's bare array | Keeps the conventional struct-serde shape (a `.conditions` field); both the migration script and `gen_golden.py` emit this form; empty filters use `{}`. |
| STEP10-002 | core `error.rs` adds an `Error::External(String)` variant | External outlets like Meilisearch need a typed error that must not impersonate the `VectorStore` class; the now non-exhaustive variant triggers exhaustive-match arm additions in `wiktor-server` (×2) and the CLI/`wiktor-feedback` `classify_error` (mapped to 503 / UNAVAILABLE / run-failure). |
| STEP10-003 | core's default feature changes from `["vector-qdrant"]` to `[]`; `vector-qdrant` becomes a CLI feature forwarded to the plugin | After the split, core no longer carries qdrant-client; assembly is the assembler's job (CLI/service) — consistent with "plugins depend only on core"; the qdrant path of `wiktor vector ping/build` and `wiktor eval` is guarded by `feature vector-qdrant` and fails explicitly (not silently) when missing. |

## 6. Acceptance criteria A1–A7

| # | Criterion | Executable result |
|---|---|---|
| A1 | tech-docs domain pack seeds and evals | `wiktor seed` + `wiktor eval` run green |
| A2 | Existing milk-tea path regresses clean | 391 tests all green; milk-tea 134/quota/legacy assertions unchanged |
| A3 | Golden filters generic across domains | goldens with two different fact-field sets (milk-tea and tech-docs) both eval green |
| A4 | Migration loses no content | migration script idempotent; the 134-record diff changes only the filter key shape |
| A5 | qdrant plugin depends only on core | the new crate builds standalone; core/server no longer depend on qdrant |
| A6 | Meilisearch outlet works | the export command mirrors accepted pages into the mock Meilisearch; quarantine is excluded |
| A7 | Cluster-sharding plan delivered | step10-cluster-sharding(.en).md exists; no code change |

## 7. Risks and boundaries

| Risk | Handling |
|---|---|
| golden generalization breaks eval assertions | idempotent migration script + stepwise `cargo test`; migrate then verify; stop on failure rather than pushing blindly |
| crate split breaks compilation (scattered qdrant_client deps) | migrate all qdrant symbols and tests in one pass; run `cargo check --all-features` first |
| tech-docs data is large / error-prone by hand | golden and facts are produced by deterministic generator scripts, reusing the milk-tea JSONL generation pattern |
| no real Meilisearch instance | verify the protocol with a mock HTTP server; leave real-instance work to a deployment drill |

<!-- END STEP10 SPEC v1.0 -->
