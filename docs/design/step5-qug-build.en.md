# Step 5 Spec: QUG Edge Construction, Persistence, Golden Expansion, and Three-Tier Evaluation

> Version: v1.0 (2026-09-22)  
> Follows: Step 3 `step3-query-engine.md`, Step 4 `step4-compile-pipeline.md`  
> Implemented by: `wiktor-builder`; independently accepted by: `test-engineer`  
> This is the authoritative English design. `step5-qug-build.md` is the authoritative Chinese counterpart.

## 1. Goals and non-goals

Step 5 upgrades Step 3's in-memory QUG construction into an auditable, reusable, invalidatable, and rebuildable SQLite-derived index. A golden set of at least 100 queries makes the formal decision about whether QUG is enabled by default. The query path remains zero-LLM and zero-remote-call.

Deliverables are deterministic construction of all five edge types; BLAKE3 source hashing and failure protection; persistence of page-owned and page-independent intent edges; merging seed and Step 4 accepted compiled pages; expansion of `golden-queries.jsonl`; `wiktor qug build`; `wiktor eval`; and A/B/C evaluation with an explicit exit decision.

Out of scope: LLM edge extraction, query-time graph construction, qdrant implementation, reranking, automatic feedback compilation, automatic golden relabeling, and cross-process hot swapping. A QUG construction error must never silently publish a partial graph.

## 2. Terms and existing constraints

- Page edge: a Synonym or Hyponym derived from accepted-page frontmatter.
- Config edge: an AttributePropagation, IntentTemplate, or Negation edge derived from `intents.yaml`; it has no `page_id`.
- Accepted page: `pages.status='accepted'`, including generation-1 `artifact='seed-v1'` legacy seed pages and Step 4 accepted compiled pages.
- Active build: the only readable `published` QUG build for a domain/version pair.
- Source hash: a BLAKE3 hash covering the domain version, QUG configuration, intent file, and participating page identities.
- Fallback: when rewrite has no match, QUG is disabled, the active graph is absent, or the current hash differs, the query goes directly through Step 3 hybrid retrieval and records `rewrite_failure` or `disabled`.

Step 3's `QugEdge`, normalization, longest-phrase matching, conflict handling, maximum depth, candidate multiplier, and RRF semantics remain authoritative. Step 5 changes graph source, storage, and evaluation only.

## 3. Decisions D1–D7

### D1: Edge sources and trigger

`Synonym` and `Hyponym` are mechanically derived from accepted-page frontmatter: each alias creates `alias -> title` Synonym; each tag creates `title -> tag` Hyponym. Aliases are not fully connected and tag hierarchy is not inferred. `AttributePropagation`, `IntentTemplate`, and `Negation` are mechanically derived from `intents.yaml`, reusing and consolidating the Step 3 `extract_page_edges`/`intent_edges` validation. LLM edge extraction is forbidden.

The trigger is explicit `wiktor qug build`. Compile publication does not maintain a partial graph implicitly; this gives the full graph a clear audit boundary. Compile-time incremental updates and LLM long-tail extraction are rejected because they add consistency and reproducibility risk.

### D2: Source hash and rebuild

The normalized source hash input is: domain name/version, builder version `qug-build-v1`, canonical JSON for QugConfig, raw `intents.yaml` bytes, and UTF-8-sorted `(page_id,generation,content_hash,artifact_version,frontmatter_json,status)` for participating pages. A fixed prefix and length-delimited encoding are used before calculating lowercase BLAKE3 hex. Canonical valid-edge JSON and its sorted order are also included.

An unchanged hash with an active published build is reused. Any input change creates a new build. Rebuild uses one SQLite transaction: `building` -> write both edge sets -> validate five-type counts and hash -> new build `published`, old build `superseded`. Any parse, validation, SQL, or process failure rolls back and leaves the old published build readable. Orphaned `building` rows are never loaded; the next build marks or removes them.

### D3: Persist intent edges independently

Keep the page foreign-key semantics of `qug_edges`; add `qug_intent_edges`, with `qug_builds` as the build parent. Add `build_id` to `qug_edges`; every new row must have it. Runtime loading reads only the active published build.

A sentinel page would pollute page lifecycle, generation, and cascade semantics. Keeping intent edges only in memory would require parsing YAML on every startup. An independent table is the smallest auditable and reliable design. Migration 0004 is required.

### D4: Seed and compiled-page integration

The graph input is the union of all accepted pages in the selected domain: legacy seeds and accepted compiled pages. For the same page_id, use the accepted head; retain different page_ids. Exclude candidate, quarantined, deleted, and orphaned old generations. QUG build does not modify pages, FTS, generation, or the existing 34 golden records.

### D5: Golden file and quotas

Continue using the single file `examples/milk-tea/golden-queries.jsonl`. Preserve the existing 34 records and their expectations; add records until the total is at least 100. Minimum quotas are `synonym` 25, `intent` 20, `negation` 20, `attribute_filter` 20, and `negative` 15. A single file is easier for the existing loader, review, and total-count validation; `kind` supports stratified reporting. Splitting the file is rejected.

Each record has at least `id`, `query`, `kind`, and `expected_entity_ids`; it may contain `must_exclude_entity_ids`, `filters`, and `notes`. Positive expected sets are manually confirmed against accepted pages and the fact plane. A negative record may have an empty expected set but must have exclusion or zero-hit semantics. Labels must not be generated from evaluation results; a normalized query may occur at most twice.

### D6: Three tiers and exit condition

A is pure FTS5 BM25. B is Step 3 FTS+Vector+RRF. C enables QUG, then performs rewrite, filter pushdown, and the same hybrid retrieval; a no-match rewrite is an explicit fallback. All tiers use the same database snapshot, accepted generation, facts, vector input, and tie-breaking.

Report fixed `recall@1`, `recall@5`, and `recall@10`. Positive recall is the macro mean of `|top_k ∩ expected| / |expected|`; negatives are reported separately as `negative_precision`, and a result must not contain `must_exclude`. Records with an empty expected set do not enter positive recall. The decision metric is C relative to B at recall@10: `gain_pp=(C-B)*100`, judged before rounding. Fewer than one active-QUG sample also means no gain. If `gain_pp < 5.0`, write `qug_decision=disabled`; otherwise write `enabled`. Disabled is a successful delivery and returns 0. Lower positive recall or negative precision in C must be highlighted in the report, but does not turn a valid disabled decision into a runtime failure.

### D7: CLI and exit codes

`wiktor qug build --domain <domain.yaml> --db <path> [--force] [--dry-run] [--json]`. The default reuses a matching hash; force ignores the hash; dry-run only parses, filters, hashes, and counts without writing. Successful output includes build_id, source_hash, page count, all five edge counts, and reuse/rebuild state.

`wiktor eval --domain <domain.yaml> --db <path> --golden <path> --out-dir <dir> [--top-k <n>] [--json] [--no-qdrant]`. It always computes @1/@5/@10; top-k defaults to 10 and is limited to 10..100. `--no-qdrant` uses the existing deterministic/mock VectorStore. It writes Chinese Markdown, English Markdown, and JSON results.

Exit codes are fixed for both commands: 0 success, including disabled; 1 runtime failure; 2 CLI usage error; 3 configuration, migration, golden, or edge-protocol validation error; 4 uncategorized internal error. No QUG gain must never be reported as 1.

## 4. Architecture and interface contract

### 4.1 Module boundaries

```text
crates/wiktor-core/src/query_engine/qug.rs  # pure edge extraction, normalization, graph construction
crates/wiktor-core/src/kernel/qug_store.rs  # hash, transactional publication, loading
crates/wiktor-core/src/eval/                # golden loader, A/B/C, metrics, reports
crates/wiktor-cli/src/commands/qug.rs       # qug build
crates/wiktor-cli/src/commands/eval.rs      # eval
```

`query_engine` owns no mutable connection; `kernel` uses existing Diesel raw SQL with binds; CLI handles arguments, domain-pack loading, and formatting. Reuse petgraph, diesel, serde_yaml_ng, serde_json, blake3, gray_matter, and the existing VectorStore. Add no new heavy dependency.

### 4.2 Type contract

```rust
pub const QUG_BUILDER_VERSION: &str = "qug-build-v1";

pub struct QugPageInput {
    pub page_id: String,
    pub generation: i64,
    pub content_hash: String,
    pub artifact_version: String,
    pub frontmatter_json: String,
}

pub struct QugSourceSnapshot {
    pub domain: String,
    pub domain_version: String,
    pub qug_config_json: String,
    pub intents_bytes: Vec<u8>,
    pub pages: Vec<QugPageInput>,
}

pub struct PersistedPageEdge {
    pub page_id: String, pub edge: QugEdge, pub edge_hash: String,
    pub generation: i64, pub content_hash: String,
}
pub struct PersistedIntentEdge { pub edge: QugEdge, pub edge_hash: String }

pub struct QugBuildStats {
    pub build_id: i64, pub reused: bool, pub source_hash: String,
    pub accepted_page_count: usize, pub edge_count: usize,
    pub by_type: BTreeMap<String, usize>,
}

pub enum QugBuildOutcome { Reused(QugBuildStats), Published(QugBuildStats) }

pub trait QugStore: Send + Sync {
    fn active_source_hash(&self, domain: &str, version: &str) -> Result<Option<String>>;
    fn publish_build(&self, snapshot: &QugSourceSnapshot,
        page_edges: &[PersistedPageEdge], intent_edges: &[PersistedIntentEdge])
        -> Result<QugBuildStats>;
    fn load_active_edges(&self, domain: &str, version: &str) -> Result<Vec<QugEdge>>;
}

pub fn build_and_publish_qug(store: &dyn QugStore,
    input: QugBuildInput<'_>, force: bool) -> Result<QugBuildOutcome>;
pub fn load_active_qug(store: &dyn QugStore,
    domain: &DomainConfig) -> Result<Option<BuiltQug>>;
```

An empty accepted-page set may build and still publish config edges. Invalid frontmatter, empty or overlong phrases, non-whitelisted fields, and unserializable edges return Validation. Database errors return storage/internal errors. No error may return a partial graph.

### 4.3 Additional publication and loading constraints

`qug_page_snapshots` is the active loader's only page-edge source. `qug_edges` remains a replaceable Step 4 publication payload; QUG build rewrites its domain-page mirror in the same transaction. New Step 4 payloads may have NULL build_id and do not represent a valid complete graph. Before publication, BEGIN IMMEDIATE rereads the entire domain accepted-page manifest and compares it with the input snapshot. A change rolls back with `source_changed` (exit 1); old snapshots must not overwrite newer sources. Extraction runs outside the lock. Force creates a fresh build even with the same hash, so the hash is not UNIQUE. Concurrent builders recheck the active hash inside the write transaction and may reuse the other builder's result unless forced.

Startup loading validates the current page manifest, edge counts, and payload hashes in one read transaction using frozen domain/intent bytes. It compares source_hash before constructing the graph. Preserve Step 3 QugBuildInput compatibility; a new function receives QugSourceSnapshot, config, and intents rather than guessing generation from CompiledPage. Corrupt persisted JSON is an internal error (exit 4). Normal queries may explicitly fall back with a reason; eval must fail instead of treating a missing graph as valid C.

### 4.4 Runtime loading

At startup or domain reload, read both edge_json sets from the active published build, sort by edge_hash, decode, call `QugGraph::from_edges`, and wrap the result in `Arc`. If the active build is absent, the hash differs, or the graph is corrupt, set `qug=None`, record stale/disabled diagnostics, and use hybrid fallback. The query thread must not parse YAML or acquire a write lock. Reload after a successful build is explicit.

## 5. Migration 0004 DDL draft

File: `crates/wiktor-core/migrations/0004_qug_persistence/up.sql`. The down migration must verify that no Step 5 data exists before dropping the new structure, avoiding destructive data loss.

```sql
CREATE TABLE qug_builds (
  build_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain_name TEXT NOT NULL,
  domain_version TEXT NOT NULL,
  builder_version TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('building','published','superseded','failed')),
  page_count INTEGER NOT NULL DEFAULT 0,
  edge_count INTEGER NOT NULL DEFAULT 0,
  counts_json TEXT NOT NULL DEFAULT '{}',
  created_at INTEGER NOT NULL,
  published_at INTEGER
);
CREATE UNIQUE INDEX uq_qug_active
  ON qug_builds(domain_name, domain_version) WHERE status='published';

ALTER TABLE qug_edges ADD COLUMN build_id INTEGER
  REFERENCES qug_builds(build_id) ON DELETE CASCADE;
CREATE INDEX idx_qug_edges_build ON qug_edges(build_id, page_id);

-- Full build copy: the old (page_id,edge_hash) key cannot retain multiple builds.
CREATE TABLE qug_page_snapshots (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  page_id TEXT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  generation INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  PRIMARY KEY(build_id,page_id,edge_hash)
);

CREATE TABLE qug_intent_edges (
  build_id INTEGER NOT NULL REFERENCES qug_builds(build_id) ON DELETE CASCADE,
  edge_hash TEXT NOT NULL,
  edge_json TEXT NOT NULL,
  PRIMARY KEY(build_id, edge_hash)
);
CREATE INDEX idx_qug_intent_edges_build ON qug_intent_edges(build_id);
```

Existing 0003 `qug_edges` rows may have NULL build_id but must not be loaded by the active loader; the first Step 5 build fully rebuilds them. New rows must carry build_id. In one transaction, write the building row and edges, validate them, mark the old published row superseded, and mark the new row published. Readers see either the pre-transaction or post-transaction state.

## 6. Evaluation report contract

The `--out-dir` directory receives `step5-qug-evaluation.md`, `step5-qug-evaluation.en.md`, and `step5-qug-evaluation.json`. The bilingual reports correspond section by section and share numbers, query IDs, decision, and dataset hash. Domain literals such as “啵啵” and “珍珠” remain Chinese.

JSON includes at least `schema_version`, domain/version, dataset_hash, source_hash, A/B/C @1/@5/@10, negative_precision, per-kind results, fallback_count, failed sample IDs, `qug_decision`, `reason`, vector_backend, and the reproducible command. A query error is recorded and makes the run fail; a data-protocol error uses exit code 3.

## 7. Acceptance criteria A1–A14

- **A1**: A second build with identical input reuses source_hash, returns `reused=true`, and creates no new active edge build.
- **A2**: Changing any accepted-page content_hash/frontmatter/generation, intent bytes, domain version/config, or builder version produces a new hash and rebuild.
- **A3**: A fixture produces at least one edge of every type; counts, canonical JSON, and deserialization agree, with stable deduplication.
- **A4**: Only accepted seed and accepted compiled pages participate; candidate/quarantined/deleted/orphaned old-generation pages do not produce or load edges.
- **A5**: Intent edges have no page_id; page edges retain the pages FK; deleting a page does not delete intent edges.
- **A6**: Any write, count, or publish failure rolls back; the old active graph remains loadable and no partial new edge set appears.
- **A7**: A matching startup hash loads the graph; a mismatch, absent active build, or corrupt graph does not substitute stale data and uses explicit fallback.
- **A8**: Seed and accepted compiled pages coexist; empty Step 4 `CompiledPage.qug_edges` does not block construction; the existing 34 golden records and expectations remain unchanged.
- **A9**: The single golden file has at least 100 records and quotas 25/20/20/20/15; the loader rejects duplicate IDs, unknown kinds, fake entities, invalid JSON, and missing quotas.
- **A10**: A/B/C on the same snapshot produce recall@1/@5/@10, negative_precision, stratified results, and fallback counts; repeated runs are identical.
- **A11**: If C-B recall@10 is below 5.0pp, report `qug_decision=disabled` and exit 0; at or above the threshold, report enabled and exit 0.
- **A12**: If C recall@10 is below B, report the regression and disabled with exit 0; runtime or data errors must not masquerade as disabled.
- **A13**: build/eval support hash reuse, force, dry-run, JSON, out-dir, and produce bilingual Markdown plus JSON.
- **A14**: Exit codes are exactly 0/1/2/3/4; no heavy dependency is added; workspace check, existing QUG/compile tests, and offline evaluation pass.

## 8. Implementation batches for wiktor-builder

1. **Pure-function batch**: extract Step 3 edge sources, normalization, canonical JSON, edge hash, and source snapshot/hash. Compile and validate A1–A3 with in-memory fixtures.
2. **Storage batch**: add migration 0004, Diesel schema/raw binds, active lookup, transactional publication, and rollback. SQLite smoke tests cover A5–A6.
3. **Runtime batch**: implement active loading, hash validation, Arc graph injection, stale/disabled/fallback diagnostics. Validate A4, A7, and A8.
4. **Golden batch**: preserve existing records and add enough records to reach 100; implement quota, schema, file-hash, and entity-existence checks. Validate A9.
5. **Evaluation batch**: implement deterministic/mock A/B/C, @1/@5/@10, negative safety, fallback, decision, and report model. Validate A10–A12.
6. **CLI batch**: connect `qug build`/`eval`, arguments, JSON, human output, out-dir, and 0/1/2/3/4. Validate A13–A14.

Each batch must compile, test, and roll back independently. Database, hash, configuration, and protocol errors must not become “QUG disabled.”

## 9. Risks, trade-offs, and deviation log

Snapshot-level rebuild is preferred over complicated page-level incremental updates because it avoids mixed generations caused by deleted edges and global intent rules. If scale increases, page-hash sharding can be added later. Mock vectors prove reproducibility, not production semantic quality. Any intent-file change invalidates the full graph because local publication would break source-hash consistency. Hard limits are required for edge count, alias/tag count, phrase length, and evaluation top-k; overflow is an error, never silent truncation.

| ID | Planned/current deviation | Cause | Impact | Compensation | MASTER-PLAN update required |
|---|---|---|---|---|---|
| STEP5-001 | Spec §4.2 `QugPageInput` does not state where `title` comes from, yet Synonym/Hyponym need it; Step4 `build_frontmatter_json` originally wrote only aliases/tags/refs/quality_policy, and legacy seed pages carry `'{}'` | Spec gap (found during batch 1) | No API change; `QugPageInput` shape unchanged | Ruling: `pages.frontmatter_json` in the DB is the single carrier of `title/aliases/tags` (batch 1 `page_frontmatter_json` is the canonical shape, title required). Both the seed path and Step4 `build_frontmatter_json` write title; snapshot assembly reads only the DB, never the md files; legacy `'{}'` pages yield zero edges without blocking (re-run seed or recompile to restore edges). The D2 five-tuple is unchanged (frontmatter_json including title is already covered by the hash) | No |
| STEP5-002 | §9 requires hard limits but gave no values; batch 1 uses defensive integers: pages 100k, frontmatter list 64, intents bytes 1MiB, intent entries 1000, phrases 64, edges 100k | Spec only required "limits must exist" | Values are an implementation detail | Overflow is always a Validation error, never truncation; adjustable when config-ized later | No |
| STEP5-003 | D5's "same normalized query at most twice", deduplicated on bare text, would wrongly reject existing records ("奶茶" appears with 4 filter contexts) | Spec did not define the normalization scope | Dedup key tightened to `(normalized_query, filter_signature)` ≤2 | Declared in the eval module docs; quotas 25/20/20/20/15 are met by the 100 new records (134 total; the 34 legacy rows don't count toward quotas, kind=Legacy) | No |

Implementers must append a record when reality differs from this spec. They must not silently change D1–D7, the DDL, exit conditions, or acceptance contract.
