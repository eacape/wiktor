# Step 2 Spec: Hand-Compiled Seed Wiki + JSONL Fact Plane + Minimum Query Closed Loop

> Version: v1.0 (2026-09-20; produced by the planner main model — both planner-architect dispatches failed because the provider context window was exceeded; the main model produced the spec based on the experience that it can take over at any time)
> Builds on: Step 1 (commit 95c0652, workspace + wiktor-core four modules + qdrant adapter + CLI status/vector ping)
> Next: Builder implements item by item according to this spec; test engineers write tests according to “12. Acceptance Criteria”

## 1. Goals and Scope

**Goal**: use 20 hand-compiled seed Wiki pages + 100+ product JSONL facts to run the minimum query closed loop of “seed import → FTS5 retrieval + fact-plane filter pushdown → CLI display”, validating the two-plane data model and the expressiveness of the domain-pack YAML — **without connecting an LLM**; hand-written pages simulate compilation artifacts.

**In scope**:
- Add the `examples/milk-tea/` dataset (`domain.yaml` / 20 `seed-wiki` pages / `products.jsonl` / `golden-queries.jsonl`)
- Add to wiktor-core: JSONL data source adapter, seed-page parsing, migration 0002 (Chinese FTS), `seed_pages`/`search` methods on the kernel, and serde support for DomainConfig
- Add `seed` / `search` subcommands to wiktor-cli
- Golden-query prototype ≥20 entries (formal evaluation is in Step 4; the pass-rate threshold for this step is in §12)

**Out of scope**: LLM compilation, QUG, hybrid vector retrieval (qdrant participates), TUI, and cross-entity relation extraction — these belong respectively to Step 3 / Step 4 / later phases.

## 2. Key Design Decisions (Decision Record)

| # | Decision | Rationale |
|---|------|------|
| D1 | **FTS Chinese tokenization: migration 0002 rebuilds `pages_fts` with `tokenize='trigram'`** | Existing `unicode61` is nearly ineffective for space-free CJK (an entire Chinese paragraph becomes one token). trigram (SQLite 3.34+, rusqlite 0.32 bundled ≈ 3.46+) splits into 3-grams, making Chinese phrase/substring search work out of the box while retaining BM25. DML: `DROP TABLE pages_fts` (triggers are cascade-deleted at table level) → rebuild table + rebuild 3 triggers + backfill `SELECT ... FROM pages`. **CURRENT_SCHEMA_VERSION → 2** |
| D2 | **Short queries (<3 characters, such as “珍珠”) use a LIKE fallback** | The trigram index requires queries ≥3 characters; two-character Chinese words are very common. `title LIKE '%q%' OR content LIKE '%q%'`, with score fixed at 1.0. First write a unit test to verify trigram MATCH behavior during implementation |
| D3 | **Put JsonlDataSource in the `data/` module inside wiktor-core** (do not create a separate crate) | MASTER-PLAN §9 does not pre-build empty shells; split into a separate crate only when a second data source (postgres, Phase 2) appears |
| D4 | **Put seed-page parsing in the `seed/` module inside wiktor-core** | The Step 3 compilation pipeline also produces WikiPage; page-format parsing is a core responsibility, while the CLI only enumerates files and orchestrates |
| D5 | **domain.yaml v1 contains only `entities` + `compile` + `query` sections**; `types`/`relations`/`qug` sections are **reserved but not parsed in this step** | The existing `DomainConfig/EntityConfig/FieldDefinition` structures already align with this scope; QUG is Step 4 content, and the YAML structure can be extended forward (do not enable serde `deny_unknown_fields`) |
| D6 | **Do not introduce a QueryEngine struct**: query orchestration = `SqliteKernel::search` method + thin CLI wrapper | The single query path has no orchestration complexity yet; Step 4’s QueryEngine calls `kernel::search` directly |
| D7 | **Filter-pushdown semantics**: FTS hits knowledge pages (`entity_id`) → the fact plane filters SKUs that satisfy the conditions → take their `category` value set → intersect `pages.entity_id IN (set)` with FTS candidates | Conforms to the two-plane model: retrieval anchors on Wiki pages, filtering anchors on SKU facts, and association goes through category. A single SQL statement completes this; it is not post-RRF filtering |
| D8 | **Filter syntax**: `--filter "price<=20,sugar_level>=50,size=中杯,ingredient_ids in=pearl,taro"`; supports `<=`/`>=`/`=`/`in=`; **boolean filtering is not supported** (there is no BooleanEquals condition; `=true/false` returns “not supported yet”) | Minimal change set; boolean fields still enter facts to preserve ETL completeness |
| D9 | **Facts/Filters placement ruling** (reported by struct-style-guard in the previous round): **keep the current state** — `FactValue/Facts/Filters/FilterCondition/FieldDefinition/FieldType` remain in `types/mod.rs` (the common payload for fact-plane filter pushdown; aggregating them at the same level as error is reasonable); **correct the comment in step1 spec §2.1** (which claims they are in entity.rs) so it matches the implementation. Do not change code | Avoid unnecessary refactoring; the public export surface `types::*` is already consistent |
| D10 | **Seed idempotency key = `pages.page_id` (INSERT OR REPLACE) + facts CAS (`source_revision`)** | Repeating `wiktor seed` does not create duplicate pages/facts; `content_hash` is computed as `blake3(title + content)` for future incremental checks, but is not used for idempotency in this step |

## 3. seed-wiki Page Contract

### 3.1 File format

Each page is a `.md` file with **YAML frontmatter (wrapped in `---`) + Markdown body**:

```markdown
---
page_id: "milk-tea:drink:boba-milk-tea"
entity_id: "milk-tea:drink:boba-milk-tea"
title: "波霸奶茶"
entity_type: "drink"
aliases: ["珍珠奶茶", "波霸奶茶"]
tags: ["奶茶", "经典"]
---

波霸奶茶是以红茶为基底、加入波霸珍珠的经典台湾奶茶。

## 简介

波霸奶茶源自台湾……（正文）

## 成分

- 红茶
- 波霸珍珠
- 鲜奶或奶精

## 口感与特征

珍珠软糯、茶味浓郁……
```

### 3.2 Field conventions (frontmatter)

| Field | Required | Type | Convention |
|------|------|------|------|
| `page_id` | ✅ | string | = `entity_id` (one entity, one page in the seed scenario); must be a valid EntityId key (`domain:type:id`, non-empty components with no colon) |
| `entity_id` | ✅ | string | Same as above |
| `title` | ✅ | string | Page title, written to `pages.title` and the FTS `title` column |
| `entity_type` | ✅ | string | Entity type (`drink/ingredient/brand/concept/practice`), written to `pages.entity_type` |
| `aliases` | ⬜ | string[] | Synonyms, used for Step 4 QUG synonym edges; only parsed, not indexed in this step |
| `tags` | ⬜ | string[] | Category tags, only parsed, not indexed in this step |

### 3.3 Body conventions

- The body begins with an **introductory paragraph** (not a heading), followed by several `##` sections.
- **A section = an `##` level-two heading**: remove `## ` from the heading line to obtain `heading`; subsequent content up to the next `##` is the section content. For an intro before the first `##`: **if introductory content exists before the first `##`, assign it to the first section with `heading = "Overview"`**.
- `###` headings after `##` remain in the current `##` section’s content verbatim (do not split further).
- Inter-page references (wiki links): `[[target_entity_id|display text]]` — target is an entity key and raw material for Step 4 relation extraction; in this step, preserve it verbatim in the body and do not parse it.

### 3.4 20-page coverage list

| # | Filename (`<type>_<id>.md`) | entity_id | Type |
|---|--------------------------|-----------|------|
| 1 | drink_boba-milk-tea.md | milk-tea:drink:boba-milk-tea | drink |
| 2 | drink_tapioca-milk-tea.md | milk-tea:drink:tapioca-milk-tea | drink |
| 3 | drink_coconut-sago.md | milk-tea:drink:coconut-sago | drink |
| 4 | drink_mango-pomelo-sago.md | milk-tea:drink:mango-pomelo-sago | drink |
| 5 | drink_cheese-tea.md | milk-tea:drink:cheese-tea | drink |
| 6 | drink_matcha-latte.md | milk-tea:drink:matcha-latte | drink |
| 7 | drink_mango-smoothie.md | milk-tea:drink:mango-smoothie | drink |
| 8 | drink_lemon-tea.md | milk-tea:drink:lemon-tea | drink |
| 9 | ingredient_pearl.md | milk-tea:ingredient:pearl | ingredient |
| 10 | ingredient_coconut-jelly.md | milk-tea:ingredient:coconut-jelly | ingredient |
| 11 | ingredient_sago.md | milk-tea:ingredient:sago | ingredient |
| 12 | ingredient_taro-ball.md | milk-tea:ingredient:taro-ball | ingredient |
| 13 | ingredient_cheese-foam.md | milk-tea:ingredient:cheese-foam | ingredient |
| 14 | ingredient_red-bean.md | milk-tea:ingredient:red-bean | ingredient |
| 15 | brand_demo-a.md | milk-tea:brand:demo-a | brand |
| 16 | brand_demo-b.md | milk-tea:brand:demo-b | brand |
| 17 | concept_milk-tea.md | milk-tea:concept:milk-tea | concept |
| 18 | concept_fruit-tea.md | milk-tea:concept:fruit-tea | concept |
| 19 | practice_no-ice.md | milk-tea:practice:no-ice | practice |
| 20 | practice_half-sugar.md | milk-tea:practice:half-sugar | practice |

Content requirements: each page has ≥2 `##` sections, an intro + body of ≥150 characters, and keywords that can be matched by Chinese retrieval (the page-title term appears in the body/intro).

## 4. products.jsonl Fact-Plane Contract

### 4.1 Record format

JSON Lines, one SKU per line:

```jsonl
{"entity_id":"milk-tea:product:sku_1001","name":"波霸奶茶(中杯)","description":"红茶底+波霸","category":"milk-tea:drink:boba-milk-tea","price":18.0,"stock":120,"sugar_level":50,"size":"中杯","on_sale":true,"ingredient_ids":["milk-tea:ingredient:pearl","milk-tea:ingredient:cheese-foam"]}
```

### 4.2 Field → facts mapping

| JSONL field | field_name | FactValue | filterable | Description |
|-----------|-----------|-----------|-----------|------|
| `entity_id` | — | — | — | SKU entity key, `milk-tea:product:sku_XXXX` |
| `name` | `name` | Text | No | Display |
| `description` | `description` | Text | No | Display |
| `category` | `category` | Text | ✅ | **Points to a knowledge-page entity_id** (drink/concept/…), the association anchor for filter pushdown |
| `price` | `price` | Numeric | ✅ | Yuan |
| `stock` | `stock` | Numeric | ✅ | Units |
| `sugar_level` | `sugar_level` | Numeric | ✅ | 0-100 (sugar percentage) |
| `size` | `size` | Text | ✅ | 中杯/大杯/超大杯 |
| `on_sale` | `on_sale` | Boolean | No | Boolean filtering is not supported in this step (D8) |
| `ingredient_ids` | `ingredient_ids` | RefList | ✅ | Ingredient entity-key list, enters `fact_refs` |

### 4.3 Quantity and generation

- **≥100** SKUs, covering 8 drink pages + a small number of concepts (such as category SKUs under fruit-tea), with prices from 8–35 yuan, sugar levels from 0–100, and three sizes, ensuring filtering queries have enough distinction.
- Hand-written seed data (the first 10 rows precisely aligned with golden-query expected values) + script batch generation (deterministic random, fixed seed for reproducibility) or all hand-written — choose one during implementation, **quantity must be ≥100**.

### 4.4 Idempotency and revision

- `source_revision` defaults to `1` (each row may include `"source_revision": N` to override it).
- Re-import uses CAS through `upsert_facts`: the same revision overwrites; an older revision is rejected (implemented in Step 1).

## 5. domain.yaml Domain Pack

```yaml
name: milk-tea
version: "0.1.0"

entities:
  - name: product
    source: jsonl://examples/milk-tea/products.jsonl
    id_field: entity_id
    type_field: category
    fields:
      - { name: name, field_type: text, filterable: false }
      - { name: description, field_type: text, filterable: false }
      - { name: category, field_type: text, filterable: true }
      - { name: price, field_type: numeric, filterable: true }
      - { name: stock, field_type: numeric, filterable: true }
      - { name: sugar_level, field_type: numeric, filterable: true }
      - { name: size, field_type: text, filterable: true }
      - { name: on_sale, field_type: boolean, filterable: false }
      - { name: ingredient_ids, field_type: reflist, filterable: true }

compile:
  quality_threshold: 0.75
  max_recompiles: 2

query:
  filters: [price, sugar_level, size, ingredient_ids]
```

Parsing requirements (D5):
- Add `#[derive(Deserialize)]` + `#[serde(rename_all = "snake_case")]` to `DomainConfig`/`EntityConfig` (aligned with the existing tags on `FieldType`).
- Reuse `FieldDefinition` directly for `EntityConfig.fields` (it already supports serde).
- Parse `type_field`/`id_field` without consuming them in this step (keep them in the structure).
- Parse `query.filters` as `Vec<String>` and store it in DomainConfig (new field, serde default = vec![]); the CLI validates that filter field names ∈ this list (permissive: an unconfigured field may still be filtered, with a warning only).
- After stripping the `jsonl://` prefix from `source`, resolve the relative path **relative to the directory containing domain.yaml**.

## 6. golden-queries.jsonl Prototype

```jsonl
{"query":"波霸奶茶","expected_hits":["milk-tea:drink:boba-milk-tea"],"filters":{}}
{"query":"珍珠奶茶","expected_hits":["milk-tea:drink:boba-milk-tea","milk-tea:drink:tapioca-milk-tea"],"filters":{}}
{"query":"奶茶","expected_hits":["milk-tea:concept:milk-tea","milk-tea:drink:boba-milk-tea","milk-tea:drink:tapioca-milk-tea"],"filters":{}}
{"query":"价格低于20元的奶茶","expected_hits":["milk-tea:drink:boba-milk-tea","milk-tea:drink:lemon-tea"],"filters":{"price_max":20}}
{"query":"不要珍珠","expected_hits":["milk-tea:drink:coconut-sago","milk-tea:drink:mango-pomelo-sago"],"filters":{"exclude_ingredients":["milk-tea:ingredient:pearl"]}}
```

Record format:

| Field | Type | Description |
|------|------|------|
| `query` | string | Natural-language query term |
| `expected_hits` | string[] | Expected knowledge-page entity_ids (**knowledge pages**, not SKUs) |
| `filters` | object | Flat filters: `price_max` / `price_min` / `sugar_max` / `sugar_min` / `size` / `ingredients` (=RefContains) / `exclude_ingredients` (=RefExcludes) |

- **≥20 entries**, covering: exact terms (≥4-character MATCH path), two-character terms (<3-character LIKE path, such as “珍珠”), numeric filters (price/sugar), exclusion (without pearls), and combinations (term + filter).
- **Pass determination (this step)**: the hit set returned by `search(query, filters)` has a non-empty intersection with `expected_hits` to count as a pass (Step 4 tightens this to ranking/all-hit evaluation).
- **Pass-rate threshold: ≥80%** (the hand-written seed data is controllable, so this threshold is reasonable; failure = implementation bug).

## 7. DDL Increment: Migration 0002

```sql
-- Rebuild pages_fts as trigram (unicode61 is ineffective for Chinese). DROP cascade-removes the original 3 triggers.
DROP TABLE IF EXISTS pages_fts;

CREATE VIRTUAL TABLE pages_fts USING fts5(
    page_id UNINDEXED,
    entity_id UNINDEXED,
    title,
    content,
    tokenize = 'trigram'
);

CREATE TRIGGER pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_update AFTER UPDATE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_delete AFTER DELETE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
END;

-- Backfill existing pages (no-op on an empty database; semantically safe)
INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT page_id, entity_id, title, content FROM pages;
```

- `CURRENT_SCHEMA_VERSION` 1 → **2**; append `(2, MIGRATION_0002)` to `migrations()`.
- Migration self-check unit tests: running the migration twice is idempotent; the `pages_fts` sqlite_master SQL contains `trigram`; after backfill, row count = pages row count.

## 8. JsonlDataSource (wiktor-core `data/` module)

```rust
// crates/wiktor-core/src/data/jsonl.rs
pub struct JsonlDataSource {
    path: PathBuf,          // file path after stripping the jsonl:// prefix
    schema: EntitySchema,   // constructed from EntityConfig
}

impl JsonlDataSource {
    /// Construct from a jsonl:// URI (resolved relative to base_dir).
    pub fn from_config(cfg: &EntityConfig, base_dir: &Path) -> Result<Self>;
}

#[async_trait]
impl DataSource for JsonlDataSource {
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;
    fn schema(&self) -> EntitySchema;
}
```

- **Cursor**: `Cursor { offset, batch_size }` — `fetch` reads `batch_size` rows starting at row `offset`; row numbers are file line numbers (1-based); EOF returns an empty Vec = termination.
- **Bad-row policy: fail-fast** (return `Error::DataSource` containing the line number and error); seed data is controlled, so expose problems early.
- Per-row parsing: `entity_id` (complete key → `EntityId::from_key`), `source_revision` (default 1), and all remaining fields → `BTreeMap<String, serde_json::Value>`.
- `EntitySchema.entity_type` = `EntityConfig.name`; `fields` = `EntityConfig.fields`.
- Do not introduce new dependencies (`serde_json` already exists).

## 9. Seed Import (CLI `wiktor seed` + core `seed/` module + `SqliteKernel::seed_pages`)

### 9.1 Command

```
wiktor seed --db <path> --domain <domain.yaml> [--pages <dir>]
```

- `--domain` is required; `--pages` defaults to `<domain.yaml directory>/seed-wiki`.
- Fact-file paths: iterate over `jsonl://` entries in `domain.yaml` `entities[].source`, resolving relative to the domain.yaml directory; **this step imports only the first `jsonl://` entity** (multiple sources are deferred to Step 3).

### 9.2 core `seed/` module (`crates/wiktor-core/src/seed/mod.rs`)

```rust
/// Parse one seed-wiki Markdown file → WikiPage (frontmatter YAML + body section splitting).
pub fn parse_page(content: &str) -> Result<WikiPage>;
```

- frontmatter: starts on the first `---` line and ends on the next (`---\n`); parse YAML into `SeedFrontmatter { page_id, entity_id, title, entity_type, aliases, tags }` (serde_yaml_ng).
- Body = all Markdown after frontmatter; split sections at `##` (see §3.3); assign the intro to a first section with `heading="Overview"`.
- `WikiPage { page_id, entity_id, title, content, sections, metadata }`; `metadata.domain_pack_version` is filled by the caller (CLI from domain.yaml), `compiled_at = now`, `model_version = "seed-manual"`, `embedding_model = "none"`.
- Take `page_id` from the frontmatter `page_id` field (parse and validate it as a legal EntityId key).

### 9.3 `SqliteKernel::seed_pages`

```rust
pub fn seed_pages(&self, page: &WikiPage, domain: &str, status: PublishStatus) -> Result<()>;
```

- Single transaction: `INSERT OR REPLACE INTO pages (...)` (PK=page_id, idempotent) + delete and reinsert old `page_sections` (skip if cascade already deleted) + `INSERT OR REPLACE INTO page_quality` (all scores 1.0; hand-written seed pages default to full score).
- Fill `pages` columns: `page_id`, `entity_id` (key), `domain`, `entity_type` (from frontmatter; add to WikiPage or derive from EntityId — **use `EntityId.entity_type`**), `title`, `content`, `content_hash = blake3(title + "\0" + content)`, `generation = 1`, `status`, `domain_pack_version`, `compiled_at`, `model_version`, `embedding_model`, `created_at = updated_at = now`.
- FTS is synchronized automatically by triggers (after migration 0002 rebuilds it). REPLACE semantics trigger the UPDATE trigger (DELETE+INSERT), leaving no residue.

### 9.4 CLI orchestration (seed)

1. Parse domain.yaml → `DomainConfig` (serde_yaml_ng).
2. Enumerate `*.md` in the `--pages` directory → call `seed::parse_page` on each → `kernel.seed_pages` (status=Accepted).
3. Iterate through entities → `JsonlDataSource::from_config` → loop `fetch(cursor)` until empty → convert each row to `Facts` (convert fields according to `FieldDefinition.field_type`) → `kernel.upsert_facts`.
4. Output statistics: page count, fact count, fact_refs count, elapsed time.

**Type conversion rules** (JSONL Value → FactValue):

| FieldType | JSON | FactValue |
|-----------|------|-----------|
| numeric | number | Numeric(f64) |
| text | string | Text |
| boolean | bool | Boolean |
| reflist | string[] | RefList(Vec<String>) |
| other/missing | — | `Error::Validation` (missing required fields error with entity_id) |

## 10. Query Orchestration (`SqliteKernel::search` + CLI `wiktor search`)

### 10.1 Command

```
wiktor search "波霸奶茶" --db <path> [--filter "price<=20,sugar_level>=50,size=中杯"] [--top-k 5]
```

### 10.2 Filter syntax (D8, parsed in wiktor-cli)

| Syntax | FilterCondition |
|------|----------------|
| `price<=20` | NumericRange{field:price, min:None, max:Some(20)} |
| `price>=15` | NumericRange{field:price, min:Some(15), max:None} |
| `size=中杯` | TextEquals{field:size, value:中杯} |
| `ingredient_ids in=a,b` | RefContains{field:ingredient_ids, refs:[a,b]} |
| `ingredient_ids not_in=a` | RefExcludes{field:ingredient_ids, refs:[a]} |
| `on_sale=true` | **Error: “boolean filtering is not supported yet”** |

Parsing rules: first find `<=`/`>=`, then `=`/`in=`/`not_in=`; parse numeric fields as f64. Invalid input → exit with an `anyhow` error.

### 10.3 `SqliteKernel::search`

```rust
pub fn search(
    &self,
    text: &str,
    filters: &Filters,
    top_k: usize,
    domain: Option<&str>,
) -> Result<Vec<SearchHit>>;
```

**Long query (`text.chars().count() >= 3`)**:

```sql
SELECT p.page_id, p.entity_id, p.title, bm25(pages_fts) AS score
FROM pages_fts f
JOIN pages p ON p.page_id = f.page_id
WHERE f.pages_fts MATCH ?1          -- ?1 = "\"<text>\"" (quotes escaped, wrapped as a phrase)
  AND p.status = 'accepted'
  [AND p.domain = ?N]
  [AND p.entity_id IN (
      SELECT DISTINCT cat.value_text
      FROM facts cat
      JOIN (SELECT DISTINCT entity_id FROM facts WHERE <filter_where>) ft
        ON ft.entity_id = cat.entity_id
      WHERE cat.field_name = 'category' AND cat.field_type = 'text'
  )]
ORDER BY score LIMIT ?M;
```

**Short query (<3 characters, such as “珍珠”)**: replace MATCH with LIKE:

```sql
WHERE (p.title LIKE ?q OR p.content LIKE ?q)   -- ?q = "%珍珠%"
  AND p.status = 'accepted' [...same filtering as above...]
ORDER BY p.page_id LIMIT ?M;                   -- score uniformly recorded as 1.0
```

**Filter pushdown implementation**: add `schema::facts::filter_where(filters) -> Result<Option<(String, Vec<Value>)>>`, returning a pure WHERE fragment (`translate_filters` currently builds a complete SELECT based on it, preserving compatibility). Without filters → omit the IN clause.

**query_logs write**: search internally writes one log (`query_text`, `query_json`=serde(Query), `rewrite_failure=0`, `hit_count`, `latency_ms`, `timestamp`). Failure paths such as MATCH syntax errors also write a log (`hit_count=0`) before returning the error.

### 10.4 CLI display

```
$ wiktor search "珍珠奶茶" --db wiktor.db --filter "price<=20" --top-k 3
score  entity_id                           title
0.2874 milk-tea:drink:boba-milk-tea        波霸奶茶
0.2150 milk-tea:drink:tapioca-milk-tea     珍珠奶茶
```

Columns: score(4f) / entity_id / title, aligned to fixed widths; print `no hits` when there are no hits.

## 11. File and Module Layout

```
examples/milk-tea/
├── domain.yaml
├── seed-wiki/                    # 20 *.md files (list in §3.4)
├── products.jsonl                # ≥100 SKUs (§4)
└── golden-queries.jsonl          # ≥20 entries (§6)

crates/wiktor-core/src/
├── data/                         # new: data-source adapter
│   ├── mod.rs                    #   pub mod jsonl; header comment
│   └── jsonl.rs                  #   JsonlDataSource
├── seed/                         # new: seed-page parsing
│   ├── mod.rs                    #   parse_page + SeedFrontmatter
│   └── built-in tests
├── schema/migrations.rs          # changed: CURRENT_SCHEMA_VERSION=2 + MIGRATION_0002
├── schema/facts.rs               # changed: +filter_where, translate_filters reuses it
├── kernel/sqlite.rs              # changed: +seed_pages +search (with logging)
├── traits/domain_pack.rs         # changed: DomainConfig/EntityConfig serde support + query.filters
└── lib.rs                        # changed: pub mod data; pub mod seed;

crates/wiktor-cli/src/
├── main.rs                       # changed: +Seed +Search subcommands
├── filter.rs                     # new: --filter syntax parsing → Filters
└── seed.rs                       # new: seed orchestration (file enumeration + import statistics) or merged into main

Cargo.toml                        # changed: workspace.dependencies + serde_yaml_ng
crates/wiktor-core/Cargo.toml     # changed: +serde_yaml_ng
docs/design/step1-workspace-core-schema.md  # changed: §2.1 comment — Facts/Filters actually live in types/mod.rs (D9 ruling fix)
```

Dependency: add `serde_yaml_ng = "0.10"` to the workspace (core reference; fall back to 0.9 if the version conflicts).

## 12. Acceptance Criteria (test-engineer writes tests accordingly)

| # | Criterion | Assertion |
|---|------|------|
| A1 | Migration 0002 idempotent | Run `migrate` twice consecutively: schema version=2, 2 migration records, no errors |
| A2 | Migration 0002 trigram | `sqlite_master` pages_fts SQL contains `trigram`; backfill row count=pages row count |
| A3 | Seed import | After `seed_pages`: pages=20, page_sections≥40, page_quality=20 |
| A4 | Seed idempotent | Repeated `seed_pages` for the same page: page row count unchanged, content_hash unchanged |
| A5 | Fact import | After importing 100 SKUs: facts ≥ 100×8 (field count), fact_refs = total reflist count across all SKUs |
| A6 | Fact CAS | Re-importing an entity with a lower revision does not overwrite it (reuse Step 1 unit-test semantics) |
| A7 | Chinese MATCH (long query) | search “波霸奶茶” hits the boba-milk-tea page, top1 score>0 |
| A8 | Chinese LIKE (short query) | search “珍珠” hits a page containing pearls (≥1), score=1.0 |
| A9 | Filter pushdown | search “奶茶” + price_max=20: every returned page has a category SKU with price≤20; no page with no low-price SKU |
| A10 | Exclusion filter | search “奶茶” + exclude_ingredients=[pearl]: result pages have no drink SKU containing pearls |
| A11 | Combined filters | Price range + size equality take effect together (two conditions AND) |
| A12 | Query log | After search, query_logs row count +1, hit_count matches the return value |
| A13 | golden-queries | 20 entries achieve ≥80% (non-empty intersection criterion); `cargo test --workspace` has an integration test running the complete `examples/milk-tea` set |
| A14 | Engineering standards | `cargo fmt --check` clean, `cargo clippy --workspace --all-targets` has 0 warnings, all tests green |

## 13. Suggested Implementation Order

1. Migration 0002 (trigram) + unit tests (A1/A2) — first verify Chinese FTS feasibility
2. Add serde support to types + serde_yaml_ng dependency + refactor `filter_where` (without breaking existing translate_filters)
3. `data/jsonl.rs` (A5 dependency) + `seed/mod.rs` (A3 dependency)
4. `SqliteKernel::seed_pages` + `search` (A4/A7-A12)
5. `examples/milk-tea/` dataset (20 pages + ≥100 SKUs + domain.yaml + golden)
6. CLI `seed` / `search` + filter parsing
7. golden-queries integration test (A13) + full acceptance (A14)
8. struct-style-guard inspection → scp upload to Linux → Linux push → local pull

---

## 14. Implementation Revision Record (2026-09-20/21, main-model implementation)

The following revisions to the spec were made during implementation; all were landed and verified (44 tests all green, clippy 0 warnings):

1. **Switch the storage layer to diesel (user decision)**: remove the rusqlite dependency; `wiktor-core` uses **diesel 2.x (SQLite bundled, libsqlite3-sys with bundled feature)** + diesel_migrations; CRUD for pages/facts/fact_refs/page_sections/page_quality uses the diesel ORM DSL (the `table!` macro in `db_schema.rs`); **FTS5 MATCH/bm25, filter-pushdown IN subqueries, CAS upsert, and query logs** remain core retrieval SQL and use `diesel::sql_query` as a raw-SQL escape hatch (SQLite type affinity: numeric parameters are inlined as text, REAL columns convert automatically).
2. **Change the migration system to diesel embed_migrations**: `migrations/0001_create_core/up.sql` + `migrations/0002_fts_trigram/up.sql` (version tracking table `__diesel_schema_migrations`, `schema_version()` = number of applied migrations). **0002 correction: drop the three triggers before rebuilding** (0001 already created triggers with the same names, otherwise “trigger already exists”).
3. **Filter separator**: conditions are separated by `,`; items in `in=`/`not_in=` lists use **`|`** (the original spec used `,`, which conflicted with the condition separator; implementation changed it).
4. **Refs parameters must use complete entity keys**: values for `ingredient_ids in=`/`not_in=` must be complete keys matching fact_refs storage (for example `milk-tea:ingredient:pearl`); short names (`pearl`) match no rows.
5. **Parsing libraries (do not reinvent wheels, user decision)**: frontmatter uses `gray_matter` (YAML engine) instead of hand-written `---` boundary parsing; body-section splitting uses `pulldown-cmark`’s `into_offset_iter` to take offsets from H2 events, replacing hand-written line matching.
6. **SearchHit adds a `title` field** (required by the CLI display columns).
7. **Short-query (<3-character) LIKE-path filter pushdown is covered by `seed_pages_then_search_chinese`/golden**: LIKE + IN(category) combinations work correctly in SQLite (`%text%` text inlining).
8. **Golden determination**: non-empty intersection counts as a pass (same as spec §6); the actual pass rate of 29 golden entries is 100% (meeting the ≥80% threshold).

**Bilingual constraint (user decision, 2026-09-21)**: project documentation and code comments must have both English and Chinese versions. The files involved in this Step 2 (code comments, migrations, CLI, tests) were written with parallel Chinese/English comments; this Chinese spec is authoritative, and the English version `step2-seed-wiki-query-loop.en.md` is to be synchronized by doc-writer (the existing Step 1 file will be supplemented uniformly later).

---

**Document version**: v1.1 (contains implementation revision record v1.0→v1.1). **Next**: Step 3 (LLM compilation pipeline + quality scoring).
