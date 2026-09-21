# Step 3 Spec: Minimal Query Loop (QUG + QueryEngine + Hybrid Retrieval)

> Version: v1.0 (2026-09-21)  
> Builds on: Step 2 `step2-seed-wiki-query-loop.md`  
> Implementation target: `wiktor-builder`  
> The Chinese document is authoritative; `step3-query-engine.en.md` mirrors it section by section.

## 1. Goals and Scope

Step 3 upgrades Step 2's single-plane FTS query into a measurable minimal query loop: `Query → QUG rewrite/fallback → fact pre-filter → FTS5 + Vector → RRF → QueryResult → query_logs`. The query path remains zero-LLM and has no remote model calls; QUG is built during compilation/seed and read-only during queries.

This step includes:

- A petgraph implementation of the five QUG edge types, sourced from seed-wiki frontmatter, `intents.yaml`, and domain-pack rules.
- A `qug` section in `DomainConfig` and hand-written milk-tea intent templates.
- A `QueryEngine` orchestration layer under `crates/wiktor-core/src/query_engine/`.
- Pre-retrieval merging of user filters and QUG filters, followed by `EntityStore::filter` to produce a candidate entity set.
- FTS5 BM25 and `VectorStore` retrieval, fused with RRF; `MockVectorStore` must run the complete path without qdrant.
- CLI search through QueryEngine, with rewrite status and applied filters displayed.
- Golden evaluation for pure FTS, hybrid retrieval, and QUG hybrid retrieval; QUG gain below 5% disables QUG by default without failing the build.

Out of scope: LLM edge extraction, cross-encoder reranking, query cache, remote APIs, TUI, and automatic feedback-task generation. Those remain outside the Step 3 boundary in the master plan.

## 2. Key Design Decisions

| # | Decision | Concrete rule and rationale |
|---|---|---|
| D1 | Use `petgraph::graph::DiGraph` for QUG | The graph is a compile-time artifact and read-only at query time. NodeIndex makes edge traversal direct; a phrase-to-NodeIndex HashMap is only a lookup index and does not replace graph semantics. |
| D2 | Nodes are normalized words/phrases; edges retain original `QugEdge` | Trim whitespace, fold repeated whitespace, and lowercase ASCII letters. Do not tokenize Chinese. Keep structured Filter/Query payloads on edges so YAML is not reparsed at query time. |
| D3 | Keep `rewrite` as `Result<Option<_>>` | `Some` means at least one executable edge matched. `None` means no edge matched, or the input can safely fall back. Corrupt graph data or invalid filter types return `Err` and are not disguised as business fallback. |
| D4 | Push filters before retrieval | User and QUG filters are combined with AND. The fact plane produces candidate entities first; both FTS and `VectorStore::search` receive that candidate set, avoiding recall loss caused by filtering after top-k. |
| D5 | Oversample both retrieval paths, then apply RRF | `candidate_k = max(top_k * 5, 50)`, capped at 500. Return only `top_k`. RRF uses constant `k=60`; multiple pages use the highest contribution for the same entity/page key. |
| D6 | Provide an offline Mock vector path; keep qdrant optional | When no embedder is configured, QueryEngine receives a query vector from its caller. CLI uses a deterministic token-hash vector for the local loop only; this is not claimed to provide semantic quality. qdrant remains aligned with the v3.2 default deployment direction. |
| D7 | Gate QUG at domain configuration level | If `qug.enabled=false`, or golden gain is below 5%, the engine skips rewrite and reports `disabled`; this satisfies the master-plan exit condition without blocking hybrid retrieval. |
| D8 | QueryEngine owns query-log writes | Keep the Step 2 kernel search log path for compatibility. QueryEngine calls a new non-logging dual-retrieval function and writes complete `rewritten_json`, failure state, and hits once, avoiding duplicate log rows. |

## 3. QUG Graph Structure and Construction

### 3.1 Structure

```rust
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct QugNode {
    pub phrase: String,              // normalized lookup key
}

pub struct QugGraph {
    pub graph: DiGraph<QugNode, QugEdge>,
    pub by_phrase: HashMap<String, NodeIndex>,
    pub max_depth: usize,
}

impl QugGraph {
    pub fn from_edges(edges: impl IntoIterator<Item = QugEdge>, max_depth: usize)
        -> Result<Self>;
    pub fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>>;
    pub fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath>;
}
```

`from_edges` reuses one node for the same normalized `from`/`phrase`/`child`. The edge deduplication key is `(source, target, discriminant, serialized_payload)`. The default `max_depth` is 2, with a hard maximum of 4; exceeding the limit is a configuration error. Once constructed, the graph is held in `Arc<QugGraph>` and is never write-locked during queries.

Normalization contract: trim leading/trailing whitespace, fold repeated whitespace, and lowercase ASCII letters; preserve Chinese, digits, and punctuation. Reject empty phrases and phrases longer than 128 Unicode scalar values. Each item in a `to` list creates one directed edge. If a synonym relation must be bidirectional, the builder explicitly creates the reverse edge.

### 3.2 Edge Sources

1. **seed-wiki aliases → Synonym**: each frontmatter `alias` points to that page's `title` and searchable `entity_id`. At minimum create `Synonym { from: alias, to: [title] }`. Do not automatically fully connect aliases on one page, which would cause edge growth.
2. **seed-wiki tags → Hyponym**: treat each tag as a parent and the page `title` and `entity_type` as children; create `Hyponym { child: title, parent: tag }`. Do not infer hierarchy between tags.
3. **intents.yaml → IntentTemplate / Negation / AttributePropagation**: convert entries directly according to §4 and validate that each field is in the `query.filters` allowlist.
4. **Rule-matching edges**: an entry's `phrases` are explicit rules. Do not execute regular expressions or call an LLM at query time. Step 3 supports exact phrase and multi-word substring scanning only.

Build input contract:

```rust
pub struct QugBuildInput<'a> {
    pub pages: &'a [CompiledPage],
    pub config: &'a DomainConfig,
    pub intents: &'a IntentConfig,
}

pub struct BuiltQug {
    pub graph: Arc<QugGraph>,
    pub source_hash: String, // BLAKE3 of normalized edges + domain version
}

pub fn build_qug(input: QugBuildInput<'_>) -> Result<BuiltQug>;
```

`source_hash` is the basis for QUG-derived cache/rebuild decisions. It must include the domain version, the bytes of the `qug` configuration file, and every page's page_id/content_hash. Any input change must rebuild the graph. Step 3 does not persist the graph in SQLite.

### 3.3 Rewrite Semantics and Precedence

Scan every Unicode character start in `query.text`, preferring the longest phrase at each position; ties of equal length follow configuration-file order. Apply at most 16 matches and produce at most 64 expanded terms. Always retain the original query text as a retrieval term to protect exact matches.

```text
rewrite(query):
  if text empty or top_k == 0: return None
  matches = longest_phrase_matches(normalize(text), graph.by_phrase)
  if matches empty: return None
  out_terms = unique([query.text])
  out_filters = query.filters.conditions.clone()
  boosted = []
  intent_seen = false

  for match in matches sorted by (length desc, config order):
    for edge in outgoing(match.node):
      match edge:
        Negation:
          append exclusion if field/value does not conflict
        AttributePropagation:
          append range/equals if compatible
        IntentTemplate:
          merge expansion.text terms, expansion.filters; intent_seen = true
        Synonym:
          append every `to` as expanded term
        Hyponym:
          append child and parent as terms; parent is category expansion

  resolve_conflicts(out_filters):
    - user filter wins over QUG filter on same field when compatible
    - incompatible ranges => empty candidate scope, do not drop a user filter
    - RefExcludes wins over RefContains intersection; record diagnostic conflict
    - duplicate conditions are deduplicated
  if out_terms == [query.text] && out_filters == query.filters && !intent_seen:
      return None
  return Some(RewrittenQuery { expanded_terms: out_terms, filters: Filters { conditions: out_filters }, boost_entities: boosted })
```

Conflict handling is deterministic: an explicit user condition has priority; a QUG attribute on the same field is merged only when the intersection is compatible. For `RefExcludes` and `RefContains`, remove excluded refs from the allowed set; if the set becomes empty, return an empty candidate scope and do not relax the user condition. Intent-template expansion filters use the same merge rules. In Step 3, `boost_entities` accepts only explicit entity keys supplied by the builder; it remains empty when entity inference is not implemented.

`traverse(node, max_depth)` returns simple paths beginning at the matching node with length 1..depth; it does not return zero-length paths, and it never repeats a NodeIndex. Results preserve edge insertion order. An unknown node returns an empty Vec. This API supports diagnostics and future extensions; rewrite does not depend on an uncontrolled full-graph traversal.

## 4. `intents.yaml` and `domain.yaml`

Add to `DomainConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct QugConfig {
    #[serde(default = "default_qug_enabled")]
    pub enabled: bool,
    #[serde(default = "default_qug_max_depth")]
    pub max_depth: usize,
    #[serde(default = "default_qug_candidate_multiplier")]
    pub candidate_multiplier: usize,
    pub intent_templates: Option<String>,
}

// DomainConfig
pub qug: QugConfig;
```

When the `qug` section is absent, use `enabled=false`, `max_depth=2`, `candidate_multiplier=5`, and `intent_templates=None`, preserving complete compatibility with the Step 2 domain.yaml. Continue ignoring unknown fields; resolve paths relative to the directory containing domain.yaml. Restrict `candidate_multiplier` to 1..20; out-of-range values return `Error::Validation`.

Incremental `domain.yaml`:

```yaml
qug:
  enabled: true
  max_depth: 2
  candidate_multiplier: 5
  intent_templates: intents.yaml
```

Add `examples/milk-tea/intents.yaml`:

```yaml
version: "0.1.0"
intents:
  - id: cold_drink
    phrases: ["冰的", "冷饮"]
    expansion:
      text: "冰饮"
      filters: []
  - id: low_sugar
    phrases: ["不甜的", "少糖", "低糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠", "不加珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
```

Implementation-level serde schema:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct IntentConfig { pub version: String, pub intents: Vec<IntentEntry> }
#[derive(Debug, Clone, Deserialize)]
pub struct IntentEntry {
    pub id: String,
    pub phrases: Vec<String>,
    #[serde(default)] pub expansion: Option<TemplateExpansion>,
    #[serde(default)] pub attribute: Option<AttributeRule>,
    #[serde(default)] pub negation: Option<NegationRule>,
}
#[derive(Debug, Clone, Deserialize)]
pub struct TemplateExpansion { pub text: String, #[serde(default)] pub filters: Filters }
#[derive(Debug, Clone, Deserialize)]
pub struct AttributeRule { pub field: String, pub min: Option<f64>, pub max: Option<f64>, pub equals: Option<String> }
#[derive(Debug, Clone, Deserialize)]
pub struct NegationRule { pub field: String, pub refs: Vec<String> }
```

Every entry must have at least one non-empty phrase and exactly one rule (`expansion`, `attribute`, or `negation`). An attribute must specify exactly one of min/max/equals as applicable; a negation field must be a reflist; all fields must be in the allowlist. Invalid configuration fails during seed/engine construction and identifies the file, entry id, and field.

## 5. QueryEngine Orchestration and Type Contract

Module layout:

```text
crates/wiktor-core/src/query_engine/
├── mod.rs       // QueryEngine, QueryResult, diagnostics
├── qug.rs       // QugGraph, builder, normalization, rewrite
├── hybrid.rs    // FTS/vector candidate retrieval and RRF
└── tests.rs     // focused unit/integration helpers (builder adds tests)
```

Public types:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RewriteStatus { Applied, Fallback, Disabled }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryDiagnostics {
    pub rewrite_status: RewriteStatus,
    pub applied_filters: Filters,
    pub candidate_count: usize,
    pub fts_count: usize,
    pub vector_count: usize,
    pub rrf_k: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub hits: Vec<SearchHit>,
    pub rewritten: Option<RewrittenQuery>,
    pub rewrite_failure: bool,
    pub diagnostics: QueryDiagnostics,
    pub latency_ms: u64,
}

pub struct QueryEngine<V: VectorStore> {
    pub kernel: Arc<SqliteKernel>,
    pub entity_store: Arc<dyn EntityStore>,
    pub vector_store: Arc<V>,
    pub qug: Option<Arc<QugGraph>>,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub collection: String,
    pub candidate_multiplier: usize,
    pub rrf_k: u32,
}

#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

impl<V: VectorStore> QueryEngine<V> {
    pub async fn search(&self, query: &Query) -> Result<QueryResult>;
}
```

> **Implementation deviation record (2026-09-21, found by struct-style-guard review)**:
> The original design gave `QueryEngine` an `entity_store: Arc<dyn EntityStore>`
> field and derived the candidate scope from `entity_store.filter(filters)`. This
> semantics proved wrong in practice: `EntityStore::filter` returns **fact-plane
> entities (SKUs, `milk-tea:product:*`)**, whereas the knowledge-page candidate
> scope needs **category values (`milk-tea:drink:*`)** — using SKU ids as a
> `pages.entity_id IN` whitelist always yields an empty scope (Tier B measured
> 75.86% → 100% after the fix).
> The implementation drops the `entity_store` field; the candidate scope is
> computed by `SqliteKernel::filter_page_candidates(filters)` (SKU filter →
> category value set, same semantics as the inline pushdown in `search`). The
> `EntityStore` trait remains for ETL/future standalone storage implementations,
> but the QueryEngine's candidate path is fixed to the kernel method.

Internal `search` sequence:

```text
validate top_k (1..=100), text length <= 4096
if qug disabled: rewritten=None, status=Disabled, filters=query.filters
else match qug.rewrite(query):
  Some(r): rewritten=Some(r), status=Applied, filters=merge result
  None: rewritten=None, rewrite_failure=true, status=Fallback, filters=query.filters
candidate_ids = entity_store.filter(filters)
if filters empty: candidate_ids=None (unbounded)
if candidate_ids is Some(empty): hits=[], write log, return
k = min(max(top_k * multiplier, 50), 500)
fts_hits = kernel.search_candidates(terms, filters, k, domain)
vector = embedder.embed(join terms)
vector_hits = vector_store.search(collection, vector, k, candidate_ids)
hits = rrf_merge(fts_hits, vector_hits, top_k, rrf_k=60)
write query_logs with rewritten/failure/hits
return QueryResult
```

Extract a non-logging version of the Step 2 search method:

```rust
pub fn search_candidates(
    &self,
    terms: &[String],
    filters: &Filters,
    top_k: usize,
    domain: Option<&str>,
    candidate_ids: Option<&[EntityId]>,
) -> Result<Vec<SearchHit>>;
```

For each term it executes FTS/LIKE independently and keeps the highest BM25 score per page. `candidate_ids` is passed as a parameterized entity_id `IN` condition. Filters continue to use Step 2 `facts::filter_where`; because the engine already prefiltered, the kernel must not query the facts table again and only restrict entity_id. The legacy `search` remains available for compatibility and calls `search_candidates` before writing the legacy log format.

## 6. Hybrid Retrieval and RRF

FTS retrieves with `expanded_terms` including the original text. Vector retrieval embeds the de-duplicated terms joined by spaces. Both paths receive `candidate_ids`. Without a vector service, inject `MockVectorStore`; vector errors return `Error::VectorStore` by default rather than silently becoming pure FTS. CLI may expose `--no-vector` as an explicit degradation and must mark it in diagnostics.

RRF formula: for each entity and source rank `r`, add `1 / (60 + r)`, with rank starting at 1. Map `VectorHit.metadata.entity_id` to its entity. For multiple page hits, use page_id as the final deduplication key and accumulate contributions for that page. Sort by `(rrf_score DESC, page_id ASC)` and map the score to `SearchHit.score=rrf_score`. If only one path has results, still use its RRF contribution; do not revert to the raw score.

```rust
pub fn rrf_merge(
    fts: &[SearchHit],
    vectors: &[VectorHit],
    top_k: usize,
    k: u32,
) -> Vec<SearchHit>;
```

An empty `candidate_ids` means unfiltered. A non-empty list is a strict allowlist. FTS results whose entity is outside the allowlist are discarded as a second protection against short-lived fact/page generation skew. Generation/content_hash alignment remains governed by the vector write contract; Step 3 does not change the publication state machine.

## 7. Query Logs and Diagnostics

`rewritten_json` serializes the complete `RewrittenQuery`: `expanded_terms`, merged `filters`, and `boost_entities`. It is NULL when no rewrite was applied. The original `Query` remains in `query_json`. Write `rewrite_failure=1` only when QUG is enabled and `rewrite` returns `Ok(None)`; a disabled configuration is not a failure and writes 0. Input-validation and storage errors may write failure logs, but `rewrite_failure` retains the above meaning.

QueryEngine writes a log on success and on recoverable empty-result paths with original text, query_json, rewritten_json, rewrite_failure, hit_count, latency_ms, and timestamp. A log-write failure does not change an already computed query result, but is reported through tracing; the query's own database error still returns Err.

The CLI reads `QueryResult.diagnostics` and displays `rewrite_status`, `candidate_count`, and `applied_filters`. `applied_filters` is the final AND condition set, so operators can verify that QUG actually reached the fact plane.

## 8. CLI Display Contract

Commands:

```text
wiktor search "不要珍珠的低糖奶茶" --db wiktor.db --domain examples/milk-tea/domain.yaml --top-k 5
wiktor search "波霸奶茶" --db wiktor.db --top-k 5 --json
```

The default output must follow this exact field order (numeric values may vary):

```text
query: 不要珍珠的低糖奶茶
rewrite: applied
expanded_terms: 不要珍珠的低糖奶茶, 奶茶
filters: sugar_level<=30; ingredient_ids not_in=milk-tea:ingredient:pearl
candidates: 7  fts: 5  vector: 5  rrf_k: 60
score  entity_id                           title
0.0310 milk-tea:drink:low-sugar-tea         低糖奶茶
```

Fallback example:

```text
query: 火星口味
rewrite: fallback (no matching QUG path)
expanded_terms: 火星口味
filters: none
candidates: all  fts: 0  vector: 0  rrf_k: 60
no hits
```

No-hit output is exactly `no hits`. `--json` prints one JSON object with `query`, `hits`, `rewritten`, `rewrite_failure`, `diagnostics`, and `latency_ms`; it must not mix human-readable lines into the JSON stream. A successful query with no hits still exits 0.

## 9. Golden Evaluation and Acceptance Criteria

Run the same seeded database and query set in three modes:

- **A: pure FTS**: `search_candidates` with the original text only and no vector.
- **B: hybrid**: original-text FTS plus `MockVectorStore`, with QUG disabled.
- **C: QUG**: the complete QueryEngine path.

Use the Step 2 rule for each golden query: it passes when the intersection of returned entities and expected entities is non-empty. Report pass rate and gain over B as `(C-B)/max(B,1)`. If C improves over B by fewer than 5 percentage points and has no regression, report `QUG disabled` and pass the evaluation. Any regression fails the evaluation and requires a fix.

| # | Criterion | Testable assertion |
|---|---|---|
| A1 | QUG graph construction | aliases/tags/intents produce the required five edge types; normalization, edge deduplication, and stable source_hash work. |
| A2 | Synonym rewrite | Query “啵啵” returns `milk-tea:drink:boba-milk-tea`; `expanded_terms` contains “波霸奶茶” or “珍珠奶茶”. |
| A3 | Hyponym expansion | Query “奶茶” can hit drink pages associated through tags; `traverse("奶茶", 2)` never returns a path deeper than 2. |
| A4 | Attribute propagation | “不甜的奶茶” creates `sugar_level max=30`; every returned entity has a matching SKU. |
| A5 | Negation | “不要珍珠” creates `RefExcludes(ingredient_ids, milk-tea:ingredient:pearl)`; pages with only pearl-containing SKUs are absent. |
| A6 | Intent template | “冰的” loads intents.yaml and adds its expansion term/filter; a missing file makes engine construction fail with the path. |
| A7 | Explicit fallback | An unknown phrase returns `rewritten=None`, `rewrite_failure=true`, status Fallback, still executes FTS+vector, and writes a log. |
| A8 | Disabled mode | With `qug.enabled=false`, rewrite is not called, failure=false, and status is Disabled. |
| A9 | Filter merge | User `price<=20` and QUG `sugar_level<=30` are both pushed down with AND; a conflicting user condition is not overwritten. |
| A10 | Candidate propagation | EntityStore IDs constrain both FTS and Vector; Mock vector never returns an entity outside the allowlist. |
| A11 | RRF | `k=60`, ranks start at 1; equal-rank hits from both paths score above a single-path hit, with stable page_id tie-breaking. |
| A12 | No-qdrant loop | MockVectorStore and a deterministic embedder complete a QueryEngine query without network access. |
| A13 | Logging | Applied, fallback, and disabled paths each write one row; rewritten_json, failure, and hit_count match the result. |
| A14 | CLI contract | Default output includes rewrite, filters, candidate counts, and the header; `--json` is one parseable JSON object; no-hit output is `no hits`. |
| A15 | Three-mode golden | A/B/C each produce a pass rate; C passes enabled mode when gain is at least 5%, and passes disabled mode when gain is below 5% with no regression. |
| A16 | Reliability boundaries | Out-of-range top_k, text length, depth, or candidate cap returns Validation; quarantined pages appear in neither retrieval path. |
| A17 | Engineering checks | `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` all pass. |

## 10. File and Module Layout

```text
examples/milk-tea/
├── domain.yaml                 # add qug section
└── intents.yaml                # new intent/attribute/negation templates

crates/wiktor-core/src/
├── query_engine/
│   ├── mod.rs
│   ├── qug.rs
│   ├── hybrid.rs
│   └── tests.rs
├── traits/domain_pack.rs       # QugConfig + backward-compatible serde
├── kernel/sqlite.rs            # search_candidates; legacy search retained
├── types/query.rs              # diagnostic types (or re-export from query_engine)
└── lib.rs                      # pub mod query_engine

crates/wiktor-cli/src/
├── main.rs                     # route Search through QueryEngine
└── embed.rs                    # deterministic CLI embedder; explicitly a local baseline

tests/
└── step3_query_engine.rs       # builder implements A1-A17 integration acceptance
```

Dependencies: add `petgraph` to workspace dependencies; reuse existing serde, serde_json, serde_yaml_ng, diesel, and async-trait. Do not make sqlite-vec or a remote qdrant service a test prerequisite.

## 11. Recommended Implementation Order

1. Extend `QugConfig` and implement intents.yaml serde validation; run all Step 2 tests to confirm backward compatibility.
2. Add petgraph, normalization, edge deduplication, and `traverse`; use in-memory edge tests for A1/A3.
3. Implement seed-page alias/tag and intents edge extraction, then add milk-tea `intents.yaml`; verify A2/A4/A5/A6.
4. Extract `SqliteKernel::search_candidates`, preserving Step 2 `search` behavior and log compatibility; verify FTS and candidate restriction.
5. Implement `rrf_merge` and the deterministic embedder; use MockVectorStore for A11/A12 first.
6. Implement `QueryEngine::search`, logging, and diagnostics; verify A7-A10, A13, and A16.
7. Connect CLI to QueryEngine, add `--json`, and implement the display contract; verify A14.
8. Upgrade the golden runner to report A/B/C and the exit decision; verify A15.
9. Run fmt, clippy, and workspace tests to complete A17; test-engineer then fills out the independent test suite.

Every step must remain buildable. A QUG construction failure must never silently enable a partial graph, and a VectorStore error must never silently masquerade as a successful pure-FTS result.
