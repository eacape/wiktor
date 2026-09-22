# Step 4 Spec: LLM Compilation Pipeline and Reliable Publication

> Version: v1.0 (2026-09-22)  
> Builds on: Step 2 two-plane kernel and Step 3 minimal query loop  
> Implementation target: `wiktor-builder`; independent acceptance: `test-engineer`  
> The Chinese document is authoritative; this document mirrors it section by section. Domain literals such as `啵啵` and `珍珠` remain unchanged.

## 1. Background, goals, and MASTER-PLAN mapping

Step 4 implements delivery dependency #4: `DataSource → incremental decision → Compiler → mechanical scoring → bounded recompilation → SQLite publication`. The default query path remains zero LLM. The authoritative master plan is MASTER-PLAN **v3.2**: SQLite/Diesel owns the two planes, FTS, and task queue; qdrant is a later derived-vector synchronization target. Do not replace the existing kernel because older plan text mentions rusqlite/sqlite-vec.

| Master-plan location | This step |
|---|---|
| §5.1 four rules + optional consistency | Four-dimensional scoring, explainable counts, versioned thresholds, token ledger; `consistency=None` |
| §5.5 #1/#2/#3 | All-dependency BLAKE3, durable tasks, CAS, lease fencing, bounded retries |
| §5.5 #4/#5 | Isolated artifacts never enter the index; accepted publication atomically updates page, score, FTS, and generation |
| §5.5 #6/#7/#8 | Field allowlists, output schema/value validation, size budgets, versions and snapshots |
| §6 milk-tea compile example, §17 #3/#4 | Read `quality_threshold=0.75` and `max_recompiles=2`; reuse the query loop for validation |
| §10 technology stack | async-openai is isolated behind `LlmClient`; local models use an Ollama `/v1` endpoint |

```text
#2 schema/seed/facts ──→ #3 QueryEngine/FTS/QUG fallback
          └───────────→ #4 compilation/quality/publish
                              ├──→ accepted pages/sections/FTS → #3
                              ├──→ persisted edge payloads → #5 QUG construction
                              └──→ published generation/hash → later vector sync
#4 does not depend on #5 or a live vector/model service for offline acceptance.
```

Out of scope: LLM edge extraction, full QUG rebuild/hot replacement, embedding and qdrant writes, consistency arbitration, feedback API, human-review UI, and automatic daily-budget tuning. Existing seed pages and golden queries remain. Mechanical rules prove that citations exist and bind to evidence; they do not prove arbitrary natural-language entailment. This step deliberately uses constrained extractive assertions so offline acceptance has a provable boundary.

## 2. Decision record D1–D8

| # | Decision | Reason and exact boundary |
|---|---|---|
| D1 | Isolate async-openai behind `LlmClient`; implement the existing `Compiler` through `LlmCompiler` | Vendor DTOs do not cross the adapter. One model request per compile; retries are executor policy. `MockCompiler` runs the same validator/scorer without a key or network. |
| D2 | Default fetch batches are 32; queue, claim, and publish per entity; one worker by default | Network work never holds the database lock. Short transactions isolate page failures. Add kernel transaction methods; never call `execute_batch`/`seed_pages`/`upsert_facts` while already holding the connection mutex. |
| D3 | JSON envelope plus per-assertion `[[ref:rN]]` markers and section refs | A ref identifies entity, revision, JSON Pointer, source value, and quote. Body is reconstructed deterministically from the contract. No “citation exists but is absent from the body” coverage fraud. |
| D4 | Equal 0.25 rule weights plus independent hard gates | Preserve the `QualityScore` API. Accept only when overall meets the domain threshold and coverage≥0.60, density≥0.40, schema=1, and citation=1. |
| D5 | Keep SQL states pending/running/succeeded/failed/dead and map business states | A page gets at most 1+2 quality candidates by default; a task gets at most 3 failures. Durable budgets and lease fencing prevent restart/concurrency bypasses. |
| D6 | Keep the existing three-column UNIQUE and add desired hash/epoch columns | Model/prompt changes must recompile without allowing same-source concurrent artifacts. Do not add hash to UNIQUE as a shortcut. Use recursive canonical JSON and ordered length-delimited hash domains. |
| D7 | `wiktor compile` explicitly selects source/provider and dry-run is read-only | No silent Mock fallback when a key is absent. Statistics separate final outcomes, skipped work, and budget-deferred work; exit codes are scriptable. |
| D8 | Accepted publication writes SQLite only; QUG edge payloads may be empty | FTS validates this step. Vectors and QUG construction consume generation/hash later. Existing seed query graphs must not be presented as newly compiled graphs. |

## 3. Architecture and module/type contract

```text
DataSource.fetch(Cursor) → validate/project → hash → admit + facts CAS
                                                │
                                          compile_tasks
                                                ↓ claim + reserve
                                      Compiler.compile(raw, ctx)
                                      ├─ LlmCompiler → LlmClient
                                      └─ MockCompiler
                                                ↓
                                     validate evidence + RuleScorer
                                         ↓                 ↓
                                  accept transaction    attempt quarantine
                                  pages + sections      retry pending / dead
                                  quality + edges       prior accepted retained
                                  FTS + generation
```

Add only `compile/{mod,config,hash,contract,quality,llm,mock,store}.rs` inside core and `compile.rs` in CLI. Store DTOs belong in the compile module; the database implementation belongs in `kernel/sqlite.rs` or a child module using the same private connection. Do not create empty crates. Reuse blake3, serde/serde_json/serde_yaml_ng, async-trait, uuid, pulldown-cmark, gray_matter, Diesel, tokio, and tracing. Add optional `async-openai = "0.28"` with default features disabled and rustls enabled; add an `llm-openai` core feature and a same-named CLI forwarding feature, enabled by default. Verify the selected version and locked transitive dependencies with Rust 1.85. Do not add rig, rusqlite, or a separate HTTP retry library. Only add direct reqwest if the adapter requires a configured HTTP client, and keep its version aligned with the SDK.

In the following signatures, `Result` means the existing `types::Result`; omitted derives/imports and method bodies do not omit state semantics.

```rust
pub struct PipelineExecutor {
    pub kernel: Arc<SqliteKernel>, pub compiler: Arc<dyn Compiler>,
    pub scorer: Arc<dyn RuleScorer>, pub validator: Arc<dyn SourceRefValidator>,
    pub clock: Arc<dyn Clock>, pub policy: CompilePolicy,
}
impl PipelineExecutor {
    pub async fn run(&self, source: &dyn DataSource,
        ctx: &CompileContext, options: RunOptions) -> Result<CompileStats>;
}
pub trait Clock: Send + Sync { fn unix_seconds(&self) -> i64; }
pub struct RunOptions { pub limit: usize, pub batch_size: usize,
    pub force: bool, pub dry_run: bool }
pub struct CompileStats { pub run_id: String, pub scanned: u64,
    pub accepted: u64, pub quarantined: u64, pub failed: u64,
    pub skipped: u64, pub deferred: u64, pub would_compile: u64,
    pub attempts: u64, pub reserved_tokens: u64, pub reported_tokens: u64,
    pub circuit_open: bool, pub dry_run: bool }
pub struct CompilePolicy {
    pub compiler_version: String, pub artifact_version: String,
    pub scorer_version: String, pub knowledge_fields: Vec<String>,
    pub sensitive_fields: Vec<String>, pub required_headings: Vec<String>,
    pub min_coverage: f32, pub min_density: f32, pub max_recompiles: u32,
    pub max_retries: u32, pub task_token_budget: u64,
    pub batch_token_budget: u64, pub daily_token_budget: Option<u64>,
    pub max_output_tokens: u32, pub lease_seconds: u32,
    pub heartbeat_seconds: u32,
}
pub struct TaskLease { pub task_id: i64, pub epoch: i64,
    pub lease_token: String, pub desired_hash: String, pub attempt_no: u32,
    pub source: RawEntity, pub context: CompileContext }
pub enum Admission { Queued(i64), Skipped, Deferred, Rejected(String) }
pub enum CommitOutcome { Accepted { generation: i64 }, Stale }
pub enum FailureDisposition { RetryAt(i64), Quarantined, Failed }
```

### 3.1 Configuration, planes, and input identity

Keep the public `DomainConfig.quality_threshold` and `max_recompiles`; extend the internal compile section. Missing compile config must produce threshold 0.75 and max_recompiles 2; fix the current zero-value default path. New compile fields reject typos while existing sections remain compatible.

```yaml
compile:
  quality_threshold: 0.75
  max_recompiles: 2
  prompt: prompts/compile.md
  output_contract: require_source_refs
  knowledge_fields: [name, description]
  sensitive_fields: []
  required_headings: [概述]
  scorer_version: rules-v1
  min_coverage: 0.60
  min_density: 0.40
  max_retries: 3
  task_token_budget: 65536
  batch_token_budget: 262144
  daily_token_budget: null
  max_output_tokens: 2048
```

Those defaults are mandatory. When `knowledge_fields` is absent, select schema fields with `field_type=text && !filterable`. Explicit fields must exist, be unique, and must not be filterable, numeric, boolean, timestamp, or sensitive; non-filterable reflists may be explicitly selected. A field such as `on_sale:boolean` is therefore excluded. Density, coverage, and threshold are finite and in [0,1]; max_recompiles is 0..=10, max_retries 1..=100, budgets nonzero, and max_output_tokens 1..=8192. `required_headings` is nonempty and unique, default `[概述]`. Freeze config/template at run admission; resumed tasks use the stored dependency snapshot.

`PreparedSource { full: RawEntity, knowledge: RawEntity, facts: Facts, snapshot_hash: String }` is required. `knowledge.fields` contains only allowlisted fields. `facts` is produced by existing `raw_to_facts` and retains every declared field not selected for knowledge; sensitive fields may stay local but never enter task/log snapshots. Dual-use fields require two explicit domain fields. Existing schema-required/type rules apply. Revisions are 1..=i64::MAX; JSONL missing revision remains compatible as 1, but negative, fractional, or otherwise invalid values are errors, never silently 1.

Each source entity produces one page: `wiki.entity_id=raw.id`, `page_id=raw.id.to_key()`. The LLM cannot invent IDs, and multiple SKUs cannot overwrite a category page. The existing products file is SKU input, so its compiled pages are product pages; golden queries that expect drink pages continue to use seed pages and do not claim that SKU compilation replaces aggregation. Add a small `compile-entities.jsonl` fixture with an explicit drink entity, knowledge fields, and facts; use the same drink ID and category anchor to validate new compiled-page filter retrieval. Group aggregation is a future domain adapter, not milk-tea logic in core.

Start fetch at `Some(Cursor { offset:0, batch_size:32 })`; advance by actual records and stop on empty. Limit defaults to 1000 and caps at 10000; batch size is 1..=128; the final batch is bounded by remaining limit. Returning more than requested is a source-protocol error. A knowledge input canonical snapshot is ≤64 KiB and one fetch ≤8 MiB; JSONL later needs bounded line reads, line ≤256 KiB, instead of the current unbounded whole-file read. Persist only the knowledge snapshot and dependencies. Never log original sensitive fields.

## 4. D1: model abstraction, errors, and offline execution

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: LlmRequest)
        -> std::result::Result<LlmResponse, CompileFailure>;
}
pub struct LlmRequest { pub system: String, pub input_json: String,
    pub model: String, pub max_output_tokens: u32, pub timeout_seconds: u32 }
pub struct TokenUsage { pub input: u64, pub output: u64 }
pub struct LlmResponse { pub json: String, pub usage: Option<TokenUsage> }
pub struct LlmCompiler { pub client: Arc<dyn LlmClient>, pub policy: CompilePolicy }
pub struct MockCompiler { pub policy: CompilePolicy }
pub enum CompileFailure {
    Retryable { code: String, retry_after_seconds: Option<u32> },
    Permanent { code: String },
    InvalidOutput { code: String, response_prefix: String },
}
// Add a typed Error variant while retaining Compilation(String) compatibility.
```

The existing `Compiler::compile` signature stays unchanged. Add `#[serde(default)] pub evidence: Option<CompileEvidence>` to `CompiledPage`; old seed pages deserialize with None, but PipelineExecutor treats None as schema failure. Never pass refs or usage through a side channel, title, or guessed Markdown. Recompute quality, content hash, and metadata in the executor; Compiler-provided values cannot bypass scoring.

OpenAI uses non-streaming Chat Completions, temperature 0, and JSON-object output. Local serde validation is always required. Disable SDK auto-retries or configure the adapter so one logical compile is one request. Timeout is 60 seconds and response body ≤128 KiB. No tools, web retrieval, or automatic output-instruction following. OpenAI default is `https://api.openai.com/v1`, key only from `WIKTOR_OPENAI_API_KEY`; Ollama defaults to `http://127.0.0.1:11434/v1` and needs no key. Never log keys, raw fields, or complete model responses.

408/429/5xx, connection reset, and timeout are retryable. Other 4xx such as 400/401/403/404/422 and TLS certificate errors are permanent. Limit Retry-After to 0..=300 seconds; absent values use `min(2^(retry_count-1),60)`. No jitter in this single-worker MVP. JSON/contract errors are quality-candidate failures and may consume recompilation count; provider/auth failures are not disguised as low quality. Existing `Error::Compilation(String)` is permanent by default; never infer HTTP class by parsing its text.

`MockCompiler` deterministically emits the same envelope, refs, body, and `usage=None` from allowed fields, then uses the same validator/scorer/hash/store. Scripted test compilers may return low-quality candidates, timeout, or success. Missing production provider keys are configuration errors; offline use requires explicit `--provider mock`. Mock output still consumes the conservative budget ledger.

## 5. D3: prompt output contract and source validation

### 5.1 Envelope schema v1

All objects use `deny_unknown_fields`; listed fields are required. Only the two mutually exclusive branches are allowed. A successful response is:

```json
{
  "schema_version":"source-ref-v1",
  "status":"ok",
  "wiki":{"title":"啵啵","aliases":[],"tags":[],"markdown":"## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n"},
  "sections":[{"heading":"概述","assertions":[{"text":"啵啵","ref_ids":["r1"]},{"text":"珍珠","ref_ids":["r2"]}],"refs":[{"id":"r1","entity_id":"milk-tea:drink:boba","source_revision":1,"pointer":"/fields/name","value":"啵啵","quote":"啵啵"},{"id":"r2","entity_id":"milk-tea:drink:boba","source_revision":1,"pointer":"/fields/description","value":"珍珠","quote":"珍珠"}]}]
}
```

A failure response is:

```json
{"schema_version":"source-ref-v1","status":"error","error":{"code":"MISSING_SOURCE_REFS","missing_pointers":["/fields/description"]}}
```

Error codes are `MISSING_SOURCE_REFS|INSUFFICIENT_SOURCE|UNSUPPORTED_SOURCE`; the error branch has no wiki/sections. No model output may become an accepted page.

```rust
pub struct OutputWiki { pub title: String, pub aliases: Vec<String>,
    pub tags: Vec<String>, pub markdown: String }
pub struct Assertion { pub text: String, pub ref_ids: Vec<String> }
pub struct SourceRef { pub id: String, pub entity_id: String,
    pub source_revision: u64, pub pointer: String,
    pub value: serde_json::Value, pub quote: String }
pub struct EvidenceSection { pub heading: String,
    pub assertions: Vec<Assertion>, pub refs: Vec<SourceRef> }
pub struct CompileEvidence { pub schema_version: String, pub wiki: OutputWiki,
    pub sections: Vec<EvidenceSection>, pub usage: Option<TokenUsage> }
pub struct RefReport { pub assertions: u32, pub supported_assertions: u32,
    pub ref_occurrences: u32, pub valid_ref_occurrences: u32,
    pub covered_units: BTreeSet<String>, pub information_chars: u64,
    pub total_chars: u64, pub issues: Vec<QualityIssue> }
pub struct QualityIssue { pub code: String, pub path: String }
pub trait SourceRefValidator: Send + Sync {
    fn validate(&self, source: &RawEntity, evidence: &CompileEvidence,
        require_refs: bool) -> RefReport;
}
pub fn decode_response(json: &str) -> std::result::Result<CompileEvidence, CompileFailure>;
```

### 5.2 Mechanical algorithm and anti-fabrication boundary

1. Decode complete JSON strictly. Reject fences, surrounding prose, duplicate keys, unknown fields, unsupported versions, and oversized input before parsing. Title is nonempty and ≤128 Unicode scalars; aliases/tags ≤32 items with each ≤128 scalars; sections 1..=32, each 1..=64 assertions, total ≤256; refs total ≤512. Headings are unique and must include required headings; extra headings must be allowed by domain config.
2. A source is the knowledge snapshot passed to Compiler. Pointers use RFC 6901 from `{id,fields,source_revision}`, and only `/fields/<allowed>` string leaves are legal. Reflists must point to an indexed element, not the whole array. Ref IDs match `r[1-9][0-9]{0,5}` and are page-unique. Entity ID and revision must exactly match the snapshot. `value` must equal the pointer value; `quote` must be a nonempty exact contiguous substring of a string value, with no whitespace/case normalization.
3. Each assertion is nonempty, one line, ≤1024 scalars, and contains no Markdown control structure, HTML, or ref delimiters. When refs exist, `text` exactly equals `quote`s joined in ref-id order with one space. Every ref-id belongs to the same section and each ref is used at least once. Title, aliases, and tags must occur exactly in a valid quote. No inference such as “啵啵=珍珠” is accepted unless the source explicitly contains it.
4. Render canonical Markdown as `## {heading}\n\n`, then `- {text}{markers}\n`; markers follow ref-id order and sections have one blank line. Scan markers and compare them to assertions, then byte-compare with the canonical renderer. Extra prose, code blocks, HTML, fake refs, and unaudited paragraphs fail schema. Use pulldown-cmark for structure; parse markers as fixed literals.
5. Keep canonical Markdown markers in `WikiPage.content`; derive sections with the existing H2 semantics rather than trusting compiler-supplied sections. Evidence is the complete verification payload; frontmatter is stored separately and must be losslessly exportable. Empty source/body never divides by zero or invents facts.
6. With `require_source_refs=true`, missing/false/unused/cross-entity refs are hard rejection with JSON paths and codes. With false, empty refs may be relaxed, but envelope/schema and any supplied refs are still fully validated; citation<1 still quarantines. False is not a publication bypass.

Stable issue codes are `MISSING_REF|UNKNOWN_REF|UNUSED_REF|SOURCE_ID_MISMATCH|REVISION_MISMATCH|POINTER_MISSING|VALUE_MISMATCH|QUOTE_MISMATCH|ASSERTION_UNSUPPORTED|MARKDOWN_MISMATCH`. Validator is offline and uses the original snapshot, not current facts.

## 6. D4: four-rule scoring, normalization, and publication gates

```rust
pub struct ScoreReport { pub quality: QualityScore,
    pub issues: Vec<QualityIssue>, pub accepted: bool }
pub trait RuleScorer: Send + Sync {
    fn score(&self, source: &RawEntity, page: Option<&CompiledPage>,
        refs: &RefReport, schema_valid: bool, ctx: &CompileContext,
        policy: &CompilePolicy) -> ScoreReport;
}
```

The executor always runs validator and scorer after every Compiler. Decode failure uses `page=None`, `schema_valid=false`, and still produces observable zero scores. A custom scorer cannot bypass publisher hard gates. NaN/Inf or out-of-range scores are internal errors, never hidden by clamping.

Let U be the set of nonempty string-leaf pointers in the knowledge snapshot (one unit per string field and per nonempty reflist item); C is the subset hit by valid, used, assertion-supporting refs; A is assertion count; S is fully supported assertion count; R is body marker occurrences plus unused ref definitions; V is valid marker occurrences.

| Dimension | Formula and boundary | Gate |
|---|---|---|
| coverage | `|C|/|U|`; U empty means 0; duplicate pointers do not increase it | ≥ configured min, default 0.60 |
| citation | `min(S/A,V/R)`; A=0 or R=0 means 0; unused definitions stay in denominator | exactly 1 and no ref issue |
| schema_compliance | 1 only when wire schema, renderer, identity, metadata, and sections all pass; otherwise 0 | exactly 1 |
| density | `I/T`; T=0 means 0. T is nonblank assertion-text scalars; I is the union of source quote character positions for first valid supported `(pointer,quote)` uses, capped at T | ≥ configured min, default 0.40 |

For repeated source substrings, use the leftmost match. Markers, headings, frontmatter, and JSON are excluded from T. This is a versioned character-level approximation, not a tokenizer claim; repetition lowers it but source-native fluff still needs human calibration.

`overall=(coverage+citation+schema_compliance+density)/4`; consistency is None/SQL NULL and excluded. Acceptance requires all hard gates and `QualityScore::passes_threshold(ctx.quality_threshold)`. For example, `(0.6,1,1,0.4)` passes threshold 0.75 exactly, while coverage 0.59 fails even when overall is high. Compute ratios in f64, then convert to f32 and use the existing API comparison without extra epsilon.

Empty input is quarantined without an LLM. Empty/oversized/bad output is not truncated into acceptance: score zero, consistency NULL, bounded diagnostics. Schema-valid but citation-invalid candidates retain explainable partial coverage/density. Store rule version, thresholds, and model versions in dependencies/frontmatter; any change creates a new hash. Golden queries are retrieval regression data, not the 50–100-page human quality set; Mock cannot prove correlation >0.7 or real cost.

## 7. D6: all-dependency hash, idempotency, and order

`content_hash` is lowercase 64-character BLAKE3 hex. Seed title/body hashes do not satisfy this hash version, so first compilation cannot skip from them.

```rust
pub struct HashDependencies<'a> { pub source: &'a RawEntity,
    pub context: &'a CompileContext, pub policy: &'a CompilePolicy,
    pub source_schema: &'a EntitySchema }
pub fn content_hash(input: HashDependencies<'_>) -> Result<String>;
pub fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>>;
```

Use fixed prefix `wiktor.compile.hash.v1\0`; encode each named domain and bytes using u64 little-endian length delimiters. Fixed order: `source`, `domain_pack_version`, `prompt_template`, `compiler_version`, `model_version`, `embedding_model`, `artifact_version`, `scorer_version`, `quality_policy`, `knowledge_schema`. Source is canonical `{entity_id:to_key(),fields:knowledge.fields}`. `source_revision` is CAS/provenance identity and is **not** in semantic hash. Quality policy includes threshold, refs flag, min coverage/density, field lists, headings, and generation parameters; budgets, leases, clocks, usage, and retry counts do not.

Sort object keys by UTF-8 bytes; preserve array order; do not trim or Unicode-normalize strings; serialize numbers stably through `serde_json::Number` and reject non-finite values. Prompt hash uses complete actual template bytes, including built-in system constraints/page template. Paths, API keys, and base URLs do not enter hash. A mutable model tag must be manually changed through `model_version`.

Compute a separate `snapshot_hash=BLAKE3(canonical(full RawEntity))` for same-revision content conflicts. Projection hash covers every compiler input; price/stock alter snapshot hash but not content hash. This is the explicit two-plane interpretation of “source serialization.”

Admission in one transaction: stale revision is skipped; same revision with different snapshot is rejected; newer revision CAS-writes facts and queues work; same snapshot is replay-safe. If accepted page hash/version matches and force is false, skip. Existing task rows with equal hash merge without resetting counters. Dead/failed terminal rows do not auto-restart. Changed desired hash increments epoch, resets counters, preserves old attempts. Force increments epoch even for equal hash and still consumes budget.

`source_heads` stores latest desired task/epoch/hash. New revision/config fences old workers at admission, not only at publish. Same-hash duplicate admission does not increment epoch. Never add hash to the existing UNIQUE: that would allow concurrent same-source artifacts. Page CAS checks source-head and lease tuple; never infer newest by semver sorting. Old accepted evidence keeps its original revision.

## 8. D2/D5: SQLite migration, transactions, and state machine

### 8.1 Required migration `0003_compile_pipeline`

Do not modify 0001/0002. Add up/down migration and db_schema updates. TEXT JSON is canonical UTF-8 and bounded; integer counters/times are nonnegative; revisions/epochs start at 1.

| Table | Addition/definition |
|---|---|
| pages | `source_revision INTEGER NOT NULL DEFAULT 0` (legacy=0), `artifact_version TEXT NOT NULL DEFAULT 'seed-v1'`, `frontmatter_json TEXT NOT NULL DEFAULT '{}'`; preserve metadata. |
| compile_tasks | Keep existing UNIQUE/status CHECK; add `desired_hash`, `epoch`, `source_json`, `dependencies_json`, `snapshot_hash`, `recompile_count`, `attempt_count`, `lease_token`, `next_attempt_at`, `result`, `reserved_tokens`, `task_token_budget` with defaults. Result is NULL or accepted/quarantined/failed/skipped/superseded. |
| compile_source_heads | `entity_id` PK, revision/snapshot/desired hashes, task FK RESTRICT, epoch, updated_at; identity only, no sensitive source. |
| compile_attempts | task/epoch/attempt PK, run ID FK RESTRICT, lease token, status reserved/completed/abandoned, publish status, bounded artifact/quality/issues JSON, reserved/reported units, error code, timestamps. |
| qug_edges | page FK CASCADE, edge hash, edge JSON, generation, content hash; PK(page_id,edge_hash); accepted pages only. |
| compile_runs | run ID PK, token limit, reserved units, created_at. |
| compile_daily_budget | UTC day PK, token limit, reserved units; shared by all domains/runs in this database when enabled. |

Add task/run/day reservation indexes. Legacy tasks without snapshots become dead/failed with `legacy_task_missing_snapshot`; legal new admission may create epoch+1. Legacy pages, quality, and sections remain unchanged.

Migrate FTS triggers so only accepted pages are indexed: INSERT conditionally inserts; UPDATE always deletes OLD then conditionally inserts NEW; DELETE deletes OLD. Clear and repopulate FTS from accepted pages. If legacy generation=1 exists but generations is empty, insert a published sentinel generation 1; the next accepted generation must be >1. Down migration refuses destructive rollback when Step 4 attempts exist. The trigram tokenizer remains.

### 8.2 Kernel methods and locking

```rust
impl SqliteKernel {
    pub fn admit_compile(&self, prepared: &PreparedSource,
        ctx: &CompileContext, policy: &CompilePolicy, force: bool) -> Result<Admission>;
    pub fn claim_compile(&self, task_ids: &[i64], run_id: &str,
        now: i64) -> Result<Option<TaskLease>>;
    pub fn heartbeat_compile(&self, lease: &TaskLease, now: i64) -> Result<bool>;
    pub fn recover_compile_leases(&self, now: i64) -> Result<u64>;
    pub fn publish_compile(&self, lease: &TaskLease, page: &CompiledPage,
        report: &ScoreReport, now: i64) -> Result<CommitOutcome>;
    pub fn finish_compile_failure(&self, lease: &TaskLease,
        failure: &CompileFailure, candidate: Option<&CompiledPage>,
        report: &ScoreReport, now: i64) -> Result<FailureDisposition>;
    pub fn load_accepted_pages(&self, domain: &str) -> Result<Vec<CompiledPage>>;
}
```

Each method takes the connection mutex once and uses `immediate_transaction`; helpers accept `&mut SqliteConnection`. Never interpolate user/model data into `execute_batch`; use Diesel binds for DML. Never hold the guard across await or call a public kernel method from inside its transaction. Run blocking DB work through `spawn_blocking`. Poisoned locks return `Error::Internal`, not unwrap. Set busy_timeout=5000ms and retry SQLite busy at DB boundaries up to three times; never replay an already completed model request.

Admission performs facts CAS and task admission atomically, allowing knowledge to lag facts. Publish validates lease/head/facts again. The accepted transaction is page + generation building + sections + quality + edges + FTS trigger + generation published + attempt/task finalization. Any failure rolls everything back. Replaying a successful lease returns the old outcome and does not allocate another generation.

A low-quality transaction stores attempt artifact/score/issues and changes task retry state without touching pages, FTS, generations, or the accepted page. A new quarantine artifact never overwrites an old accepted page_id. Human review reads dead/quarantined latest attempts.

### 8.3 States, retries, and leases

| Business state | SQL status/result | Transition |
|---|---|---|
| pending | pending/NULL | claim when due and budget is available |
| compiling | running/NULL | heartbeat; success accepted; failure follows policy |
| accepted | succeeded/accepted | terminal; new hash or force creates work |
| quarantined | dead/quarantined | terminal manual queue; no automatic retry |
| failed | failed/failed or dead/failed | permanent or retry exhausted; no automatic retry |
| superseded/skipped | succeeded/superseded or succeeded/skipped | no publication/generation |

The current `SQL_CLAIM_NEXT` cannot be used unchanged: it increments retry_count during claim and lacks ownership/budget checks. Replace the constant while retaining its name. Claim only admitted task IDs for the current run, due pending rows with retry_count<max_retries, ordered by `(next_attempt_at,task_id)`. Recover expired running rows first. In one transaction recheck status/budget/head, create UUID lease token, increment attempt_count, create reserved attempt, and set running lease `now+300`. Claim does not increment retry_count.

Heartbeat every 30s extends to now+300 with task/epoch/token/status and unexpired lease predicates. All completion/publish operations use the same fence. Zero affected rows means stale ownership and cannot change the current worker. A provider may complete after lease loss, so exactly-once paid calls are not promised; reservations remain conservative.

`retry_count` counts failures for the current epoch; claim/heartbeat do not count. `recompile_count` counts failed quality candidates. With max_recompiles=2, the third bad candidate becomes dead/quarantined; max_recompiles=0 quarantines the first. Transport failures increment retry_count only. Quality, JSON, schema, and error-envelope failures are quality candidates. Empty input/oversized preflight creates a synthetic quarantine attempt without LLM or budget.

Lease recovery changes abandoned attempt to abandoned, increments retry_count once, keeps reservation, and moves to pending/backoff or dead/failed. Superseded tasks become succeeded/superseded and never publish. Repeated recovery is idempotent.

### 8.4 Budget circuit breaker

Before every request reserve `B=system UTF-8 bytes + input JSON UTF-8 bytes + 256 + max_output_tokens`; these are versioned budget units, not provider billing tokens. Defaults are task 65536 and run 262144; optional daily budget is shared across this database. Claim requires task/run/day `reserved+B<=limit` in one transaction. Completed, timeout, and crash reservations are not returned. Reported usage is observability only. If reported usage exceeds B, open the run circuit with `estimator_underflow` and stop requests.

Insufficient run/day budget leaves pending/deferred and exits with circuit_open; it is not a quality failure. A task budget too small to start becomes quarantined; previously isolated work that cannot afford another attempt is quarantined; transport-only exhaustion is failed. No automatic epoch renewal. Use checked arithmetic.

## 9. D7: CLI command and output

```text
wiktor compile --domain examples/milk-tea/domain.yaml --db wiktor.db \
  --entity product --data-source jsonl://products.jsonl --provider mock --limit 10
wiktor compile --domain examples/milk-tea/domain.yaml --provider ollama \
  --model qwen2.5:7b --embedding-model bge-small-zh-v1.5 --json
```

`--domain` is required; `--db` defaults to `wiktor.db`; `--entity` is required when multiple entities exist; `--data-source` overrides only `jsonl://` in this step; `--provider` is openai (default), ollama, or mock; real providers require `--model`, mock uses mock-v1; `--embedding-model` defaults none and is hash-only; `--base-url` overrides the compatible endpoint without logging credentials; `--limit/--batch-size` default 1000/32 with the stated bounds; `--force` creates an epoch and still obeys all gates; `--dry-run` is fully read-only; budget flags are positive overrides that cannot increase an existing daily limit; `--json` emits one JSON object to stdout and human logs to stderr.

Dry-run uses a separate read-only SQLite connection. It must not create a database, migrate, write facts/tasks, reserve budget, or call a model. A missing DB is treated as empty; an old schema returns migration_required. `scanned=skipped+would_compile+quarantined+failed+deferred`, while accepted and attempts are zero. In a real run each scanned entity receives one final classification; recompilations count only in attempts. Do not busy-wait longer than 60 seconds for backoff.

Human output has fixed headings `accepted quarantined failed skipped deferred attempts`, plus scanned, reserved_tokens, reported_tokens, circuit_open, dry_run, and would_compile. Exit codes: 0 complete with no failed/quarantined/deferred; 2 argument/config/source protocol error; 3 failed or quarantined exists; 4 only budget/lease/backoff deferred; 1 database/internal failure. Priority is 1>2>3>4>0. Dry-run success is 0; invalid input is 2/3 and never claims accepted.

## 10. D8: Step 2/3 integration

An accepted transaction is immediately searchable through existing FTS/LIKE without vector prerequisites. Keep Step 3 `filter_page_candidates` category→knowledge-page semantics; never pass SKU IDs directly as drink-page candidates. New drink fixture IDs must equal category fact values. Product-page compilation does not promise category-filter retrieval.

Mock/LlmCompiler defaults to `qug_edges=[]`. The persistence path accepts trusted injected `QugEdge` payloads only after domain/filter allowlist checks and existing graph-construction validation. Deduplicate by canonical edge JSON BLAKE3; generation/hash comes from the accepted transaction. The LLM envelope has no edge field, so it cannot perform unaudited automatic extraction. Persist aliases/tags in frontmatter for later graph construction; missing legacy frontmatter cannot be fabricated.

A later vector worker scans accepted/published pages by `(page_id,generation,content_hash,embedding_model)`, embeds body/sections, rechecks the page before marking synchronization, and uses the stable key. This step supplies readers and keys only. A later QUG worker rebuilds from accepted pages, persisted edges, and intents; attempts/quarantine never enter the graph.

Required Step 3 safety repair: before RRF and before top-k truncation, batch-check vector payloads against accepted page id/hash/generation; discard missing, quarantined, old, or metadata-less hits. Existing Mock metadata tests must be updated. The compile spec does not claim that new pages have semantic vectors or a new QUG graph; it only accepts FTS visibility as the immediate integration proof.

## 11. Acceptance criteria A1–A24

All criteria run offline with temporary SQLite, no key, network, or qdrant. Inject Clock and scripted Compiler/LlmClient; use transaction fault injection for database failures. Real OpenAI/Ollama is an explicit non-blocking integration smoke.

| # | Criterion | Executable assertion |
|---|---|---|
| A1 | Mock full pipeline | One knowledge entity creates consistent pages/sections/quality/frontmatter/generation and succeeded task; scores come from the real scorer. |
| A2 | Incremental skip | Identical dependencies call the model zero times; skipped=1; generation/sections/attempts do not grow. |
| A3 | All-dependency invalidation | Changing knowledge (with higher revision), domain version, prompt bytes, compiler/model/embedding/scorer version each triggers compilation; changed model is not blocked by UNIQUE. |
| A4 | Stable serialization | Object key permutation hashes equal; array order/string whitespace changes hash; length delimiters avoid collisions; fixed golden hex. |
| A5 | Valid refs | Two valid refs yield citation=coverage=density=1; JSON Pointer `~0/~1` works. |
| A6 | Invalid refs | Wrong entity/revision/pointer/value/quote, cross-section, dangling, and unused refs cannot accept and produce stable issue codes. |
| A7 | Missing refs | require=true missing refs, evidence=None, and error envelope quarantine/bounded-retry; false does not bypass citation gate. |
| A8 | Schema escape prevention | Unknown/duplicate keys, fence, prose, body/assertion mismatch, HTML, invalid headings, and extra refs produce schema=0 and no query index row. |
| A9 | Score boundaries | Empty input/output score zero; finite [0,1]; `(0.6,1,1,0.4)` passes at .75, coverage=.59 fails; consistency is NULL. |
| A10 | Density anti-repetition | Repeating one quote ten times lowers density; overlapping quotes do not double-count source characters; duplicate refs do not raise coverage. |
| A11 | Page brake | max_recompiles=2 allows at most three bad candidates then dead/quarantined; zero allows one; restart cannot resume the loop. |
| A12 | Task brake | Three retryable transport failures become dead/failed; failures count once; claim/heartbeat do not increment retry_count; 401 is terminal on first attempt. |
| A13 | Batch budget | Insufficient reservation makes no model call, leaves pending/deferred, and exits 4; budget is not reset between fetches or task reruns. |
| A14 | Daily/task budget | Concurrent runs cannot exceed shared daily units; omission cannot bypass an existing limit; next UTC day can resume; task budget shortage is terminal. |
| A15 | Lease fencing | Worker A cannot publish/heartbeat/change counters after timeout and worker B reclaim; repeated reclaim counts once and preserves reservation. |
| A16 | Atomic publication | Fault at page/section/quality/edge/generation write leaves no partial page/FTS/hash; retry increments generation only once. |
| A17 | Prior version retained | Low-quality new version quarantines while old accepted content remains searchable; quarantined text is absent from FTS; no prior page means zero results. |
| A18 | CAS/idempotency | Revision 2 blocks revision 1 facts/refs/page; same revision content conflict; concurrent same task has one lease; new config fences old worker. |
| A19 | Fact update | Price/stock-only revision updates facts with zero model calls; page/generation and old evidence revision remain; sensitive fields stay out of prompt/log/task snapshot. |
| A20 | CLI/dry-run | Statistics equation, exit priority, and JSON single-object output hold; dry-run does not create/migrate/write/call model. |
| A21 | Retrieval integration | A new drink fixture searches through FTS when its ID matches category anchor; old-vector hash/generation and quarantine hits are discarded pre-fusion. |
| A22 | Migration compatibility | 0001+0002 upgrade, legacy pages remain, only accepted pages enter trigram FTS, first new generation is > legacy, UNIQUE remains. |
| A23 | Input/dependency limits | Batch/limit/line/response overflows fail in a bounded way; negative revision is not defaulted to 1; overflow/NaN/bad policy errors; Mock runs without llm-openai. |
| A24 | Engineering/regression | fmt, clippy, workspace tests, and Rust 1.85 build pass; Step 2/3 golden queries do not regress; provider smoke is separately labeled and Mock is not used to claim human correlation/cost. |

## 12. Implementation order for the builder

1. Configuration, projection, identity, and hash: freeze DTOs, fix compile defaults, validate revisions; complete A3/A4/A19/A23 while keeping Step 2/3 APIs compiling.
2. Envelope, renderer, evidence, validator, and scorer: add optional `CompiledPage.evidence`; complete A5–A10.
3. 0003 migration, accepted-only FTS, frontmatter/attempt/head/budget tables; complete A22 and update old schema-version assertions.
4. Kernel admission/claim/heartbeat/recovery/failure/publish transactions; complete A15–A18 with prebuilt pages and verify no mutex reentrancy deadlocks.
5. Executor plus MockCompiler; complete A1/A2/A11–A14/A19.
6. async-openai/Ollama single-request adapters and typed failures; use Mock HTTP/LlmClient to verify classes, timeout, and disabled SDK retries.
7. CLI compile/dry-run/stats/exit codes and bounded source reads; complete A20/A23 and category-linked fixture validation.
8. Persistent edge reads/writes, accepted-page reader, and stale-vector payload protection; complete A21.
9. Run A24 and append synchronized implementation-deviation records; never lower quality gates to make tests pass. Keep every step buildable and independently verifiable.

## 13. Boundaries, risks, and implementation-deviation record

- Mechanical citations and extractive output limit expression, but make offline acceptance provable. Free-form synthesis requires stronger future validation/consistency arbitration.
- Same-revision different content is a source-contract conflict; require a corrected revision. Force cannot bypass facts CAS. Reused knowledge retains its original evidence revision.
- Budget units are conservative estimates; real cost depends on provider usage and prices. Record usage/version without claiming a deterministic bill.
- Field allowlists can omit useful knowledge and need calibration against the human annotation set. Source-native falsehood and fluff are outside mechanical citation detection.
- Vector metadata validation is the required publication/query handoff repair; qdrant sync and new QUG construction remain later. Corrupt DB or migration failure stops the run and is not swallowed as a page-quality error.

**Implementation-deviation record (2026-09-22, pre-implementation review)**: the repository currently uses Diesel plus `Mutex<SqliteConnection>`; SQL states are running/succeeded/dead rather than compiling/accepted/quarantined. `SQL_CLAIM_NEXT` increments retry_count at claim and has no limit/ownership fence. `pages` lacks source_revision/frontmatter, there is no qug_edges table, FTS triggers index every status, seed generation is fixed at 1, `CompiledPage` has no evidence payload, DomainConfig compile defaults have a zero-value path, and Step 3 does not validate vector generation/hash. These are not completed fixes; they are migration/compatibility work for the builder. Future deviations must be appended here and in the Chinese counterpart with date, reason, interface impact, and acceptance changes.

**Implementation-deviation record (2026-09-22, appended after the Step 4 delivery; mirrors the Chinese section)**:

1. **Preflight quarantine is intercepted before admit** (§8.3). `Admission::Queued(i64)` returns only the task_id, not the epoch, so the executor cannot address `(task_id, epoch)` to call `quarantine_compile_preflight`. An empty knowledge source is intercepted before admission instead: no task row, no token reservation, no LLM request — behaviorally equivalent to "empty source quarantines immediately". The kernel method remains available for later paths. Interface impact: none; acceptance: the empty-source path of A11 asserts the interception semantics.
2. **The budget-circuit signal reuses `claim_compile`'s `Ok(None)`** (§8.4). Both "no due task" and "budget exhausted" return `Ok(None)`; the executor distinguishes them via the `retry_at` mapping: due tasks that cannot be claimed mean circuit_open, all-not-yet-due tasks follow the RetryAt backoff (60s cumulative cap plus a 50ms wake margin). Interface impact: no new return variant; acceptance: A13/A14 assert this distinction.
3. **`validate_vector_payloads` judges validity per page** (§10). The "valid page_id set" is implemented as "a page is valid iff every payload of that page in the batch matches the accepted head": mixing old and new chunks of one page drops the page's vectors entirely, so an old-generation chunk cannot ride on a valid same-page payload. Stricter than per-hit admission. Interface impact: the kernel returns `HashSet<String>` of page_ids rather than composite keys; acceptance: A21 includes a mixed-payload whole-page rejection case.
4. **`open_existing` also rejects schemas newer than the current version** (§9 dry-run). Not only older ones; the CLI maps `migration_required` to exit code 2 (input problem), while a migration failure in a real run stays exit code 1. Interface impact: new read-only entry `SqliteKernel::open_existing`, `SUPPORTED_SCHEMA_VERSION=3`.
5. **Edge validation in the executor uses a fixed depth of 2** (§10). The `QugGraph::from_edges` construction check uses the domain default depth 2 and does not read the domain's actual `qug.max_depth` (`CompilePolicy` does not carry that field). It only checks validity and never starts the graph service; illegal payloads take the InvalidOutput path and are never published.
6. **Connection-level errors are uniformly Retryable** (§4). reqwest cannot tell whether a connection failure stems from a TLS certificate, so all connection-level errors are Retryable and end dead/failed when exhausted — never silently accepted.
7. **Stable diagnostic codes converge by mapping** (§5.2). Duplicate ref ids map to `UNKNOWN_REF` and title/aliases/tags mismatches map to `QUOTE_MISMATCH`, both distinguished by JSON path; no extra codes such as `DUP_REF` were added. The required_headings set check only verifies shape and uniqueness at decode time; set matching is performed by the executor, which holds the policy.
8. **`admit_compile` gained a `schema: &EntitySchema` parameter** (§8.2). The content_hash and the knowledge_schema domain of dependencies_json need the frozen schema, which the executor holds and passes in.
9. **File placement differs from the spec's literal names** (§3). The spec names `compile/store.rs` for the DTOs; the DTOs (Admission/TaskLease/CommitOutcome and friends) actually live in `compile/config.rs`, and the database implementation is `kernel/compile_store.rs` (`pub(crate)`). This matches the same section's "database implementation lives in the kernel, no empty shell" rule; only the file names differ, and renaming was avoided to keep module paths stable.
10. **Literal compiler version identifiers** (§7). `CompilePolicy::default()` uses `compiler_version="compile-v1"` and `artifact_version="wiki-v1"` as fixed identifiers for this step (the spec gives no literals). Both enter the content_hash, so changing them triggers recompilation.

<!-- END STEP4 SPEC v1.0 -->
