# Step 1: Workspace Core Schema Implementation Specification

**Version**: 1.0  
**Date**: 2026-09-20  
**Decision basis**: MASTER-PLAN.md v3.1 (2026-09-20 architecture fork finalized + qdrant vector backend decision)

## 0. Architecture Decision Summary

**Finalized on 2026-09-20**:
- Default vector retrieval uses the **qdrant external service** (sqlite-vec dropped — pre-v1, twice on long hiatus, ANN only in alpha)
- qdrant handles only the vector leg of recall; the knowledge plane / fact plane / FTS5 inverted index all live in SQLite
- Fact filter pushdown happens on the SQLite fact plane (single source of truth for facts); filtering yields the candidate ID set → qdrant vector search is then run on that candidate set
- Atomic-publish contract revised: after the SQLite single transaction (page + score + FTS5 + facts) commits, vectors are synced to qdrant (the collection carries generation + content_hash); lag is covered by generation alignment + rebuildable safety net

## 1. Cargo Workspace Structure

### 1.1 Root `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = [
    "crates/wiktor-core",
    "crates/wiktor-cli",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.75"
license = "MIT OR Apache-2.0"
repository = "https://github.com/wiktor-rs/wiktor"

[workspace.dependencies]
# Async runtime
tokio = { version = "1.40", features = ["full"] }
tokio-stream = "0.1"

# Storage and serialization
rusqlite = { version = "0.32", features = ["bundled", "blob", "chrono", "uuid"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
serde_yaml_ng = "0.10"  # serde_yaml is no longer maintained

# Vector retrieval (qdrant external service)
qdrant-client = { version = "1.11", default-features = false }

# Hashing and IDs
blake3 = "1.5"
uuid = { version = "1.10", features = ["v4", "serde"] }

# Error handling
thiserror = "1.0"
anyhow = "1.0"

# Graph structure
petgraph = "0.6"

# Concurrent hashing
dashmap = "6.1"

# Cache
moka = { version = "0.12", features = ["future"] }

# Logging and tracing
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Time handling
chrono = { version = "0.4", default-features = false, features = ["clock", "serde"] }
```

### 1.2 `crates/wiktor-core/Cargo.toml`

```toml
[package]
name = "wiktor-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
tokio = { workspace = true }
tokio-stream = { workspace = true }
rusqlite = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml_ng = { workspace = true }
qdrant-client = { workspace = true }
blake3 = { workspace = true }
uuid = { workspace = true }
thiserror = { workspace = true }
anyhow = { workspace = true }
petgraph = { workspace = true }
dashmap = { workspace = true }
moka = { workspace = true }
tracing = { workspace = true }
chrono = { workspace = true }

[dev-dependencies]
tokio-test = "0.4"
tempfile = "3.12"
```

### 1.3 `crates/wiktor-cli/Cargo.toml`

```toml
[package]
name = "wiktor-cli"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "wiktor"
path = "src/main.rs"

[dependencies]
wiktor-core = { path = "../wiktor-core" }
tokio = { workspace = true }
anyhow = { workspace = true }
clap = { version = "4.5", features = ["derive", "env"] }
tracing-subscriber = { workspace = true }
```

**Acceptance criteria 1.x**:
- `cargo check --workspace` passes
- `cargo build --workspace` successfully produces the `target/debug/wiktor` binary
- No dependency cycles; core does not depend on cli

---

## 2. Module Layout and File Tree

### 2.1 `wiktor-core` directory structure

```
crates/wiktor-core/
├── Cargo.toml
└── src/
    ├── lib.rs              # module exports + prelude
    ├── types/
    │   ├── mod.rs
    │   ├── entity.rs       # EntityId, RawEntity, Facts, Filters
    │   ├── page.rs         # CompiledPage, WikiPage, QualityScore, content_hash
    │   ├── query.rs        # Query, RewrittenQuery, SearchHit, Cursor
    │   ├── qug.rs          # QugEdge, QugPath
    │   └── error.rs        # Error enum + From implementations
    ├── traits/
    │   ├── mod.rs
    │   ├── data_source.rs  # DataSource trait
    │   ├── entity_store.rs # EntityStore trait
    │   ├── compiler.rs     # Compiler trait
    │   ├── qug.rs          # QueryUnderstandingGraph, QugBuilder traits
    │   ├── reranker.rs     # Reranker trait
    │   ├── domain_pack.rs  # DomainPack trait
    │   ├── feedback.rs     # FeedbackAnalyzer trait
    │   └── vector_store.rs # VectorStore trait (core abstraction)
    ├── schema/
    │   ├── mod.rs
    │   ├── migrations.rs   # schema_migrations table + migrate() function
    │   ├── knowledge.rs    # knowledge-plane DDL (pages + generation + quarantine)
    │   ├── facts.rs        # fact-plane DDL (facts + source_revision CAS)
    │   ├── fts.rs          # FTS5 virtual-table DDL
    │   ├── tasks.rs        # compilation-task queue DDL
    │   └── query_log.rs    # query-log DDL
    └── kernel/
        ├── mod.rs
        ├── sqlite.rs       # SQLite connection pool + WAL/foreign_keys pragma
        ├── qdrant_vector.rs # QdrantVectorStore implementation
        └── mock_vector.rs  # MockVectorStore implementation (in-process, for unit tests)
```

### 2.2 `wiktor-cli` directory structure

```
crates/wiktor-cli/
├── Cargo.toml
└── src/
    └── main.rs             # clap command-line entry point (compile/search/status)
```

**Acceptance criteria 2.x**:
- All `mod.rs` pass `cargo check`
- `use wiktor_core::prelude::*;` compiles in an external crate
- Clear module boundaries: types has no external dependencies, traits depend only on types, schema depends on rusqlite, kernel depends on traits + schema

---

## 3. Domain Type Definitions (types module)

### 3.1 `types/entity.rs`

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// globally unique entity identifier (domain:type:id triple)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId {
    pub domain: String,        // domain-pack name (such as "ecommerce")
    pub entity_type: String,   // entity type (such as "product", "category")
    pub id: String,            // source-data primary key
}

impl EntityId {
    pub fn new(domain: impl Into<String>, entity_type: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            entity_type: entity_type.into(),
            id: id.into(),
        }
    }

    /// serialize to string form (domain:type:id)
    pub fn to_key(&self) -> String {
        format!("{}:{}:{}", self.domain, self.entity_type, self.id)
    }

    /// parse from string form (domain:type:id)
    pub fn from_key(key: &str) -> Result<Self, crate::Error> {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            return Err(crate::Error::InvalidEntityId(key.to_string()));
        }
        Ok(Self::new(parts[0], parts[1], parts[2]))
    }
}

/// raw source entity (not compiled)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEntity {
    pub id: EntityId,
    pub fields: HashMap<String, serde_json::Value>,  // source-data field key-value pairs
    pub source_revision: u64,  // source-data revision (for CAS idempotent writes)
}

/// structured fact-plane metadata (filterable fields)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Facts {
    pub entity_id: EntityId,
    pub fields: HashMap<String, FactValue>,  // filterable field key-value pairs
    pub source_revision: u64,  // must increase monotonically
}

/// fact-field value (supports numeric, enum, reference-list, and other types)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FactValue {
    Numeric(f64),
    Text(String),
    Boolean(bool),
    RefList(Vec<String>),  // reference list (such as ingredient_ids)
    Timestamp(i64),        // Unix timestamp (seconds)
}

/// filter conditions (for fact-plane pre-filtering)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filters {
    pub conditions: Vec<FilterCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterCondition {
    NumericRange { field: String, min: Option<f64>, max: Option<f64> },
    TextEquals { field: String, value: String },
    RefContains { field: String, refs: Vec<String> },  // reference-list inclusion (such as excluding ingredient_ids)
    RefExcludes { field: String, refs: Vec<String> },  // reference-list exclusion (negation edge)
}

impl Filters {
    pub fn empty() -> Self {
        Self { conditions: vec![] }
    }

    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }
}
```

### 3.2 `types/page.rs`

```rust
use serde::{Deserialize, Serialize};
use crate::types::{EntityId, qug::QugEdge};

/// compilation artifact (knowledge-plane Wiki page + quality score + QUG edges)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledPage {
    pub wiki: WikiPage,
    pub quality: QualityScore,
    pub qug_edges: Vec<QugEdge>,
    pub content_hash: String,  // BLAKE3 hash (covers source data + domain-pack version + Prompt + compiler + model version)
}

/// Wiki page content (pure Markdown, human-readable)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub page_id: String,       // globally unique page ID (auto-generated)
    pub entity_id: EntityId,   // associated entity
    pub title: String,
    pub content: String,       // Markdown body
    pub sections: Vec<Section>,
    pub metadata: PageMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub heading: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMetadata {
    pub domain_pack_version: String,  // domain-pack version (semver)
    pub compiled_at: i64,             // Unix timestamp (seconds)
    pub model_version: String,        // LLM model identifier
    pub embedding_model: String,      // embedding-model identifier
}

/// quality score (four rule dimensions + one LLM arbitration dimension)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityScore {
    pub coverage: f32,            // coverage (0.0-1.0, rule-computable)
    pub citation: f32,            // citation integrity (0.0-1.0, rule-computable)
    pub schema_compliance: f32,   // schema compliance (0.0-1.0, rule-computable)
    pub density: f32,             // information density (0.0-1.0, approximate rule)
    pub consistency: Option<f32>, // consistency (0.0-1.0, optional LLM arbitration)
}

impl QualityScore {
    /// overall score (average of four rule dimensions; consistency handled separately)
    pub fn overall(&self) -> f32 {
        (self.coverage + self.citation + self.schema_compliance + self.density) / 4.0
    }

    /// whether the quality threshold (0.75) is passed
    pub fn passes_threshold(&self, threshold: f32) -> bool {
        self.overall() >= threshold
    }
}

/// publish status (publish state machine: candidate → accepted / quarantined)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishStatus {
    Candidate,    // pending review (citation-integrity check failed)
    Accepted,     // published (enters the query index)
    Quarantined,  // quarantined (citation integrity failed; not in query index)
}

impl PublishStatus {
    pub fn is_queryable(&self) -> bool {
        matches!(self, PublishStatus::Accepted)
    }
}
```

### 3.3 `types/query.rs`

```rust
use serde::{Deserialize, Serialize};
use crate::types::{EntityId, Filters};

/// user query request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,              // query text
    pub filters: Filters,          // structured filter conditions (optional)
    pub top_k: usize,              // maximum number of results
    pub domain: Option<String>,    // restrict to a domain pack (optional)
}

/// QUG-rewritten query (Option: None falls back)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenQuery {
    pub expanded_terms: Vec<String>,  // terms expanded with synonyms
    pub filters: Filters,             // filters generated by QUG (attribute-propagation/negation edges)
    pub boost_entities: Vec<EntityId>, // entity IDs expanded by hyponym edges
}

/// retrieval hit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub page_id: String,
    pub entity_id: EntityId,
    pub title: String,
    pub snippet: String,        // highlighted snippet
    pub score: f32,             // fused score
    pub score_breakdown: ScoreBreakdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub vector_score: Option<f32>,  // vector-retrieval score (qdrant)
    pub bm25_score: Option<f32>,    // BM25 score (FTS5)
    pub rerank_score: Option<f32>,  // reranking score (optional)
}

/// query-log record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLog {
    pub log_id: String,        // unique log ID
    pub query: Query,
    pub rewritten: Option<RewrittenQuery>,
    pub hits: Vec<SearchHit>,
    pub rewrite_failure: bool, // QUG rewrite-failure marker (explicit fallback)
    pub latency_ms: u64,
    pub timestamp: i64,        // Unix timestamp (seconds)
}

/// data-source cursor (for paginated source-data fetching)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    pub offset: usize,
    pub batch_size: usize,
}
```

### 3.4 `types/qug.rs`

```rust
use serde::{Deserialize, Serialize};
use crate::types::{Filters, Query, FilterCondition};

/// five QUG edge types (generic mechanism, instantiated by the domain pack)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QugEdge {
    /// synonym edge (direct replacement at query time)
    Synonym {
        from: String,
        to: Vec<String>,
    },
    /// hyponym edge (category-expanded recall)
    Hyponym {
        child: String,
        parent: String,
    },
    /// attribute-propagation edge (converted to a structured filter, lands in the fact plane)
    AttributePropagation {
        phrase: String,
        filter: FilterCondition,  // directly generates a filter condition
    },
    /// intent-template edge (expanded into a composite query)
    IntentTemplate {
        phrase: String,
        expansion: Query,  // expanded query structure
    },
    /// negation edge (generates an exclusion filter)
    Negation {
        phrase: String,
        exclusion: FilterCondition,  // exclusion condition
    },
}

/// QUG graph traversal path
#[derive(Debug, Clone)]
pub struct QugPath {
    pub nodes: Vec<String>,    // traversed node sequence
    pub edges: Vec<QugEdge>,   // traversed edge sequence
    pub depth: usize,
}
```

### 3.5 `types/error.rs`

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // data errors
    #[error("Invalid entity ID format: {0}")]
    InvalidEntityId(String),

    #[error("Entity not found: {0}")]
    EntityNotFound(String),

    #[error("Duplicate entity: {0}")]
    DuplicateEntity(String),

    // storage errors
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Migration failed: {0}")]
    Migration(String),

    // vector storage errors
    #[error("Vector store error: {0}")]
    VectorStore(String),

    #[error("Qdrant connection error: {0}")]
    QdrantConnection(String),

    // compilation errors
    #[error("Compilation failed: {0}")]
    Compilation(String),

    #[error("Quality score below threshold: {0} < {1}")]
    QualityBelowThreshold(f32, f32),

    #[error("Content hash mismatch: expected {0}, got {1}")]
    ContentHashMismatch(String, String),

    // query errors
    #[error("Query failed: {0}")]
    Query(String),

    #[error("QUG rewrite failed: {0}")]
    QugRewrite(String),

    #[error("Filter error: {0}")]
    Filter(String),

    // configuration errors
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Domain pack not found: {0}")]
    DomainPackNotFound(String),

    // general errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

**Acceptance criteria 3.x**:
- All types pass `cargo check`
- All types implement `Debug + Clone + Serialize + Deserialize`
- `EntityId`'s `to_key()` / `from_key()` round-trip conversion is lossless
- `QualityScore`'s `overall()` computes correctly (average of the four rule dimensions)
- The Error enum covers all error scenarios, and the From implementations are correct

---

## 4. Trait Definitions (traits module)

### 4.1 `traits/data_source.rs`

```rust
use async_trait::async_trait;
use crate::types::{RawEntity, Cursor, error::Result};

/// data-source adapter (JSONL, postgres, etc.)
#[async_trait]
pub trait DataSource: Send + Sync {
    /// fetch source data page by page
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;

    /// get the source-data Schema (field definitions)
    fn schema(&self) -> EntitySchema;
}

/// entity Schema definition
#[derive(Debug, Clone)]
pub struct EntitySchema {
    pub entity_type: String,
    pub fields: Vec<FieldDefinition>,
}

#[derive(Debug, Clone)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: FieldType,
    pub filterable: bool,  // whether it enters the fact plane
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Numeric,
    Text,
    Boolean,
    RefList,
    Timestamp,
}
```

### 4.2 `traits/entity_store.rs`

```rust
use async_trait::async_trait;
use crate::types::{EntityId, Facts, Filters, error::Result};

/// fact-plane storage (SQLite implementation)
#[async_trait]
pub trait EntityStore: Send + Sync {
    /// idempotently write facts (source_revision for CAS: old versions must not overwrite newer ones)
    async fn upsert_facts(&self, id: &EntityId, facts: &Facts, source_revision: u64) -> Result<()>;

    /// fact filter pushdown (returns candidate ID set for vector-retrieval pre-filtering)
    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>>;

    /// delete facts (tombstone propagation)
    async fn delete_facts(&self, id: &EntityId) -> Result<()>;

    /// get facts (single-record query)
    async fn get_facts(&self, id: &EntityId) -> Result<Option<Facts>>;
}
```

### 4.3 `traits/compiler.rs`

```rust
use async_trait::async_trait;
use crate::types::{RawEntity, CompiledPage, error::Result};

/// compiler (LLM compilation + quality scoring)
#[async_trait]
pub trait Compiler: Send + Sync {
    /// compile one entity into a Wiki page (with quality score and QUG edges)
    async fn compile(&self, raw: RawEntity, ctx: &CompileContext) -> Result<CompiledPage>;
}

/// compilation context (provides domain-pack configuration, model version, and other information)
#[derive(Debug, Clone)]
pub struct CompileContext {
    pub domain_pack_version: String,
    pub prompt_template: String,
    pub model_version: String,
    pub embedding_model: String,
    pub quality_threshold: f32,
    pub require_source_refs: bool,  // whether source references are required
}
```

### 4.4 `traits/qug.rs`

```rust
use async_trait::async_trait;
use crate::types::{Query, RewrittenQuery, QugPath, QugEdge, error::Result};

/// query-understanding graph (built at compile time, read-only traversal at query time)
#[async_trait]
pub trait QueryUnderstandingGraph: Send + Sync {
    /// query rewriting (None means QUG cannot handle it; caller must fall back to hybrid search)
    async fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>>;

    /// graph traversal (returns all reachable paths, with a maximum depth)
    fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath>;
}

/// QUG builder (builds the graph from Wiki pages and domain-pack configuration)
#[async_trait]
pub trait QugBuilder: Send + Sync {
    /// extract QUG edges from compilation artifacts
    async fn extract_edges(&self, pages: &[crate::types::CompiledPage]) -> Result<Vec<QugEdge>>;

    /// build the query-understanding graph (petgraph)
    async fn build_graph(&self, edges: Vec<QugEdge>) -> Result<Box<dyn QueryUnderstandingGraph>>;
}
```

### 4.5 `traits/reranker.rs`

```rust
use async_trait::async_trait;
use crate::types::{Query, SearchHit, error::Result};

/// reranker (cross_encoder, off by default)
#[async_trait]
pub trait Reranker: Send + Sync {
    /// rerank retrieval results (takes fused candidates and outputs reranked results)
    async fn rerank(&self, query: &Query, hits: Vec<SearchHit>) -> Result<Vec<SearchHit>>;
}
```

### 4.6 `traits/domain_pack.rs`

```rust
use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::Result;

/// domain pack (YAML configuration + Prompt templates + page templates)
pub trait DomainPack: Send + Sync {
    /// domain-pack name
    fn name(&self) -> &str;

    /// domain-pack version (semver)
    fn version(&self) -> &str;

    /// get configuration
    fn config(&self) -> &DomainConfig;

    /// get compiler
    fn compiler(&self) -> Result<Box<dyn Compiler>>;

    /// get QUG builder
    fn qug_builder(&self) -> Result<Box<dyn QugBuilder>>;

    /// get reranker (optional)
    fn reranker(&self) -> Result<Option<Box<dyn Reranker>>>;
}

/// domain-pack configuration (parsed from domain.yaml)
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: String,
    pub version: String,
    pub entities: Vec<EntityConfig>,
    pub quality_threshold: f32,
    pub max_recompiles: usize,
}

#[derive(Debug, Clone)]
pub struct EntityConfig {
    pub name: String,
    pub source: String,  // data-source URI (jsonl:// or postgres://)
    pub id_field: String,
    pub type_field: String,
    pub fields: Vec<crate::traits::data_source::FieldDefinition>,
}
```

### 4.7 `traits/feedback.rs`

```rust
use async_trait::async_trait;
use crate::types::{QueryLog, Query, error::Result};

/// feedback analyzer (query logs → blind-spot analysis → supplemental compilation tasks)
#[async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    /// analyze query logs and generate a feedback report
    async fn analyze(&self, logs: &[QueryLog]) -> Result<FeedbackReport>;
}

/// feedback report (blind-spot signals + suggested supplemental compilation tasks)
#[derive(Debug, Clone)]
pub struct FeedbackReport {
    pub zero_recall_queries: Vec<Query>,   // zero-recall queries
    pub low_quality_hits: Vec<String>,     // low-quality hits (page_id)
    pub rewrite_failures: Vec<Query>,      // query rewrite failures
    pub suggested_compilations: Vec<CompileTask>,  // enters the manual review queue
}

#[derive(Debug, Clone)]
pub struct CompileTask {
    pub entity_id: crate::types::EntityId,
    pub reason: String,  // task trigger reason
    pub priority: TaskPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPriority {
    High,
    Medium,
    Low,
}
```

### 4.8 `traits/vector_store.rs`

```rust
use async_trait::async_trait;
use crate::types::{EntityId, Filters, error::Result};
use serde::{Deserialize, Serialize};

/// vector-store abstraction (qdrant default implementation + MockVectorStore unit-test implementation)
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// batch-insert or update vectors (with metadata)
    async fn upsert(
        &self,
        collection: &str,
        ids: &[String],
        vectors: &[Vec<f32>],
        metadata: &[VectorMetadata],
    ) -> Result<()>;

    /// vector search (supports filters + candidate-ID pre-filtering)
    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,  // candidate ID domain pre-filtered by the fact plane
    ) -> Result<Vec<SearchHit>>;

    /// delete vectors
    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()>;

    /// rebuild collection (full re-embedding scenario)
    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()>;

    /// ensure collection exists (idempotent operation)
    async fn ensure_collection(&self, collection: &str, dimension: usize, distance: DistanceMetric) -> Result<()>;
}

/// vector metadata (stored in qdrant payload)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMetadata {
    pub entity_id: String,        // EntityId.to_key()
    pub page_id: String,
    pub chunk_type: ChunkType,    // page summary / section
    pub content_hash: String,     // BLAKE3 hash (for rebuild verification)
    pub generation: u64,          // SQLite generation version (for alignment)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkType {
    Summary,  // page summary
    Section,  // section-level
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    DotProduct,
}

/// vector retrieval hit
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: String,
    pub score: f32,
    pub metadata: VectorMetadata,
}
```

**Acceptance criteria 4.x**:
- All traits pass `cargo check`
- All traits are marked `Send + Sync` (multi-thread support)
- async traits correctly use the `#[async_trait]` macro (from the async-trait crate)
- The `search` method of the VectorStore trait supports both filters filtering and candidate_ids pre-filtering

---

## 5. SQLite Schema (schema module)

### 5.1 `schema/migrations.rs`

```rust
use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// idempotent migration function (call immediately after opening the connection)
pub fn migrate(conn: &Connection) -> Result<()> {
    // enable WAL mode (set once, persistent)
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // enable foreign-key constraints
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // create the schema_migrations table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    // get the current version
    let current_version: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // apply incremental migration
    if current_version < 1 {
        apply_migration_v1(conn)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            [1, chrono::Utc::now().timestamp()],
        )?;
    }

    Ok(())
}

fn apply_migration_v1(conn: &Connection) -> Result<()> {
    // knowledge plane
    crate::schema::knowledge::create_tables(conn)?;
    // fact plane
    crate::schema::facts::create_tables(conn)?;
    // FTS5 inverted index
    crate::schema::fts::create_tables(conn)?;
    // task queue
    crate::schema::tasks::create_tables(conn)?;
    // query logs
    crate::schema::query_log::create_tables(conn)?;

    Ok(())
}
```

### 5.2 `schema/knowledge.rs`

```rust
use rusqlite::{Connection, Result};

/// knowledge plane DDL (pages + generation + quarantine publish state machine)
pub fn create_tables(conn: &Connection) -> Result<()> {
    // global generation counter (atomic index version)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS generation (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            current INTEGER NOT NULL DEFAULT 0
        )",
        [],
    )?;
    conn.execute("INSERT OR IGNORE INTO generation (id, current) VALUES (1, 0)", [])?;

    // Wiki page table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS pages (
            page_id TEXT PRIMARY KEY,
            entity_id TEXT NOT NULL,
            domain TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            title TEXT NOT NULL,
            content TEXT NOT NULL,
            content_hash TEXT NOT NULL UNIQUE,
            generation INTEGER NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('candidate', 'accepted', 'quarantined')),
            domain_pack_version TEXT NOT NULL,
            compiled_at INTEGER NOT NULL,
            model_version TEXT NOT NULL,
            embedding_model TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
        [],
    )?;

    // index
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_entity_id ON pages(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_domain ON pages(domain)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_status ON pages(status)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_generation ON pages(generation)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_content_hash ON pages(content_hash)", [])?;

    // quality-score table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS quality_scores (
            page_id TEXT PRIMARY KEY,
            coverage REAL NOT NULL,
            citation REAL NOT NULL,
            schema_compliance REAL NOT NULL,
            density REAL NOT NULL,
            consistency REAL,
            overall REAL NOT NULL,
            FOREIGN KEY (page_id) REFERENCES pages(page_id) ON DELETE CASCADE
        )",
        [],
    )?;

    // page sections table (for section-level vector indexing)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sections (
            section_id TEXT PRIMARY KEY,
            page_id TEXT NOT NULL,
            heading TEXT NOT NULL,
            content TEXT NOT NULL,
            section_index INTEGER NOT NULL,
            FOREIGN KEY (page_id) REFERENCES pages(page_id) ON DELETE CASCADE
        )",
        [],
    )?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_sections_page_id ON sections(page_id)", [])?;

    Ok(())
}
```

### 5.3 `schema/facts.rs`

```rust
use rusqlite::{Connection, Result};

/// fact-plane DDL (facts + source_revision CAS)
pub fn create_tables(conn: &Connection) -> Result<()> {
    // fact table (filterable fields)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS facts (
            entity_id TEXT NOT NULL,
            field_name TEXT NOT NULL,
            field_type TEXT NOT NULL CHECK (field_type IN ('numeric', 'text', 'boolean', 'reflist', 'timestamp')),
            value_numeric REAL,
            value_text TEXT,
            value_boolean INTEGER,
            value_timestamp INTEGER,
            source_revision INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (entity_id, field_name)
        )",
        [],
    )?;

    // indexes (support filter pushdown)
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_entity_id ON facts(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_field_name ON facts(field_name)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_numeric ON facts(value_numeric) WHERE field_type = 'numeric'", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_text ON facts(value_text) WHERE field_type = 'text'", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_timestamp ON facts(value_timestamp) WHERE field_type = 'timestamp'", [])?;

    // reference-list table (reflist fields stored one row per item, supports inclusion/exclusion filtering)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS fact_refs (
            entity_id TEXT NOT NULL,
            field_name TEXT NOT NULL,
            ref_value TEXT NOT NULL,
            PRIMARY KEY (entity_id, field_name, ref_value),
            FOREIGN KEY (entity_id, field_name) REFERENCES facts(entity_id, field_name) ON DELETE CASCADE
        )",
        [],
    )?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_fact_refs_entity_id ON fact_refs(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_fact_refs_field_name ON fact_refs(field_name)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_fact_refs_ref_value ON fact_refs(ref_value)", [])?;

    Ok(())
}
```

### 5.4 `schema/fts.rs`

```rust
use rusqlite::{Connection, Result};

/// FTS5 full-text search table DDL (BM25 inverted index)
pub fn create_tables(conn: &Connection) -> Result<()> {
    // FTS5 virtual table (indexes page title + content)
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
            page_id UNINDEXED,
            entity_id UNINDEXED,
            title,
            content,
            tokenize = 'unicode61'
        )",
        [],
    )?;

    // trigger: synchronize pages inserts/updates to FTS5
    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS pages_fts_insert AFTER INSERT ON pages
        BEGIN
            INSERT INTO pages_fts (page_id, entity_id, title, content)
            VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
        END",
        [],
    )?;

    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS pages_fts_update AFTER UPDATE ON pages
        BEGIN
            DELETE FROM pages_fts WHERE page_id = OLD.page_id;
            INSERT INTO pages_fts (page_id, entity_id, title, content)
            VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
        END",
        [],
    )?;

    conn.execute(
        "CREATE TRIGGER IF NOT EXISTS pages_fts_delete AFTER DELETE ON pages
        BEGIN
            DELETE FROM pages_fts WHERE page_id = OLD.page_id;
        END",
        [],
    )?;

    Ok(())
}
```

### 5.5 `schema/tasks.rs`

```rust
use rusqlite::{Connection, Result};

/// compile-task queue DDL (state machine: pending → running → succeeded/failed/dead)
pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS compile_tasks (
            task_id TEXT PRIMARY KEY,
            entity_id TEXT NOT NULL,
            source_revision INTEGER NOT NULL,
            domain_pack_version TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('pending', 'running', 'succeeded', 'failed', 'dead')),
            retry_count INTEGER NOT NULL DEFAULT 0,
            max_retries INTEGER NOT NULL DEFAULT 3,
            lease_expires_at INTEGER,
            error_message TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE (entity_id, source_revision, domain_pack_version)
        )",
        [],
    )?;

    // index
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_status ON compile_tasks(status)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_entity_id ON compile_tasks(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_lease_expires_at ON compile_tasks(lease_expires_at) WHERE status = 'running'", [])?;

    Ok(())
}
```

### 5.6 `schema/query_log.rs`

```rust
use rusqlite::{Connection, Result};

/// query-log DDL
pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS query_logs (
            log_id TEXT PRIMARY KEY,
            query_text TEXT NOT NULL,
            query_json TEXT NOT NULL,
            rewritten_json TEXT,
            rewrite_failure INTEGER NOT NULL DEFAULT 0,
            hit_count INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL,
            timestamp INTEGER NOT NULL
        )",
        [],
    )?;

    // index
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_timestamp ON query_logs(timestamp)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_rewrite_failure ON query_logs(rewrite_failure) WHERE rewrite_failure = 1", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_hit_count ON query_logs(hit_count) WHERE hit_count = 0", [])?;

    Ok(())
}
```

**Acceptance criteria 5.x**:
- `migrate()` is idempotent: calling it multiple times on the same database file increments the schema version correctly
- `PRAGMA journal_mode` returns `wal`
- `PRAGMA foreign_keys` returns `1` (enabled)
- The pages table's status field accepts only the three values `candidate`/`accepted`/`quarantined`
- The facts table's source_revision supports CAS conditional updates (older versions don't overwrite newer ones)
- The FTS5 virtual table is created successfully, and the triggers sync the pages table to pages_fts
- The UNIQUE constraint on compile_tasks prevents idempotency-dedup duplicate tasks

---

## 6. Qdrant Adapter Layer (kernel/qdrant_vector.rs)

### 6.1 Collection naming convention

```rust
/// Qdrant collection naming:{domain}_{version}_gen{generation}
/// Example:"ecommerce_1_0_gen42"
pub fn collection_name(domain: &str, domain_pack_version: &str, generation: u64) -> String {
    let version_safe = domain_pack_version.replace('.', "_");
    format!("{}_{}_{}", domain, version_safe, generation)
}
```

### 6.2 Payload field specification

```rust
use serde_json::json;

/// mapping VectorMetadata to qdrant payload
pub fn metadata_to_payload(metadata: &VectorMetadata) -> serde_json::Value {
    json!({
        "entity_id": metadata.entity_id,
        "page_id": metadata.page_id,
        "chunk_type": match metadata.chunk_type {
            ChunkType::Summary => "summary",
            ChunkType::Section => "section",
        },
        "content_hash": metadata.content_hash,
        "generation": metadata.generation,
    })
}
```

### 6.3 QdrantVectorStore implementation signature

```rust
use qdrant_client::{Qdrant, QdrantError};
use qdrant_client::qdrant::{
    CreateCollectionBuilder, Distance, VectorParamsBuilder, PointStruct,
    SearchPointsBuilder, Filter, Condition, FieldCondition, Match,
};
use crate::traits::vector_store::{VectorStore, VectorMetadata, DistanceMetric, SearchHit};
use crate::types::{EntityId, Filters, error::Result};
use async_trait::async_trait;

pub struct QdrantVectorStore {
    client: Qdrant,
    default_dimension: usize,  // default vector dimension (such as BGE-base-zh-v1.5 = 768)
}

impl QdrantVectorStore {
    /// connect to the qdrant service (read URL from environment variables or configuration)
    pub async fn connect(url: &str, dimension: usize) -> Result<Self> {
        let client = Qdrant::from_url(url)
            .build()
            .map_err(|e| crate::Error::QdrantConnection(e.to_string()))?;

        Ok(Self {
            client,
            default_dimension: dimension,
        })
    }

    /// ensure collection exists (idempotent operation)
    pub async fn ensure_collection_impl(
        &self,
        collection: &str,
        dimension: usize,
        distance: DistanceMetric,
    ) -> Result<()> {
        // check whether the collection exists
        let exists = self.client.collection_exists(collection).await
            .map_err(|e| crate::Error::VectorStore(e.to_string()))?;

        if !exists {
            let qdrant_distance = match distance {
                DistanceMetric::Cosine => Distance::Cosine,
                DistanceMetric::Euclidean => Distance::Euclid,
                DistanceMetric::DotProduct => Distance::Dot,
            };

            let create_req = CreateCollectionBuilder::new(collection)
                .vectors_config(VectorParamsBuilder::new(dimension as u64, qdrant_distance));

            self.client.create_collection(create_req).await
                .map_err(|e| crate::Error::VectorStore(e.to_string()))?;
        }

        Ok(())
    }

    /// batch upsert (convert metadata to payload)
    async fn upsert_impl(
        &self,
        collection: &str,
        ids: &[String],
        vectors: &[Vec<f32>],
        metadata: &[VectorMetadata],
    ) -> Result<()> {
        if ids.len() != vectors.len() || ids.len() != metadata.len() {
            return Err(crate::Error::VectorStore(
                "ids, vectors, metadata length mismatch".to_string(),
            ));
        }

        let points: Vec<PointStruct> = ids.iter()
            .zip(vectors.iter())
            .zip(metadata.iter())
            .map(|((id, vector), meta)| {
                PointStruct::new(
                    id.clone(),
                    vector.clone(),
                    metadata_to_payload(meta),
                )
            })
            .collect();

        self.client.upsert_points(collection, points, None).await
            .map_err(|e| crate::Error::VectorStore(e.to_string()))?;

        Ok(())
    }

    /// vector search (supports candidate-ID pre-filtering + filter conditions)
    async fn search_impl(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<SearchHit>> {
        let mut builder = SearchPointsBuilder::new(collection, query_vector, top_k as u64);

        // build the qdrant Filter (candidate-ID pre-filtering + filter conditions)
        let mut conditions = Vec::new();

        // candidate ID pre-filter (result of fact-plane pushdown)
        if let Some(ids) = candidate_ids {
            let id_strings: Vec<String> = ids.iter().map(|id| id.to_key()).collect();
            // qdrant filter: entity_id in [...]
            conditions.push(Condition::HasId(id_strings.into()));
        }

        // TODO: convert Filters to qdrant FieldCondition (mapping rules required)
        // not implemented in this step; wiktor-builder will add it according to domain-pack rules

        if !conditions.is_empty() {
            builder = builder.filter(Filter::must(conditions));
        }

        let search_result = self.client.search_points(builder).await
            .map_err(|e| crate::Error::VectorStore(e.to_string()))?;

        let hits = search_result.result.into_iter()
            .map(|point| {
                let payload = point.payload;
                let metadata = VectorMetadata {
                    entity_id: payload.get("entity_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    page_id: payload.get("page_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    chunk_type: match payload.get("chunk_type").and_then(|v| v.as_str()).unwrap_or("summary") {
                        "summary" => ChunkType::Summary,
                        "section" => ChunkType::Section,
                        _ => ChunkType::Summary,
                    },
                    content_hash: payload.get("content_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    generation: payload.get("generation").and_then(|v| v.as_u64()).unwrap_or(0),
                };

                SearchHit {
                    id: point.id.to_string(),
                    score: point.score,
                    metadata,
                }
            })
            .collect();

        Ok(hits)
    }

    /// delete vectors
    async fn delete_impl(&self, collection: &str, ids: &[String]) -> Result<()> {
        self.client.delete_points(collection, ids.iter().map(|s| s.into()).collect(), None).await
            .map_err(|e| crate::Error::VectorStore(e.to_string()))?;
        Ok(())
    }

    /// rebuild collection (delete the old collection and create a new one)
    pub async fn recreate_collection_impl(&self, collection: &str, dimension: usize) -> Result<()> {
        // delete the old collection (if it exists)
        let _ = self.client.delete_collection(collection).await;

        // create a new collection
        self.ensure_collection_impl(collection, dimension, DistanceMetric::Cosine).await?;

        Ok(())
    }
}

#[async_trait]
impl VectorStore for QdrantVectorStore {
    async fn upsert(
        &self,
        collection: &str,
        ids: &[String],
        vectors: &[Vec<f32>],
        metadata: &[VectorMetadata],
    ) -> Result<()> {
        self.upsert_impl(collection, ids, vectors, metadata).await
    }

    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<SearchHit>> {
        self.search_impl(collection, query_vector, top_k, filters, candidate_ids).await
    }

    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()> {
        self.delete_impl(collection, ids).await
    }

    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()> {
        self.recreate_collection_impl(collection, dimension).await
    }

    async fn ensure_collection(&self, collection: &str, dimension: usize, distance: DistanceMetric) -> Result<()> {
        self.ensure_collection_impl(collection, dimension, distance).await
    }
}
```

**Acceptance criteria 6.x**:
- `QdrantVectorStore::connect()` can connect to a local qdrant service (`docker run`)
- `ensure_collection()` is idempotent: calling it multiple times with the same collection name doesn't error
- `upsert()` can batch-insert vectors; the payload contains the five fields entity_id/page_id/chunk_type/content_hash/generation
- `search()` correctly filters candidate IDs (when candidate_ids is non-empty, it searches only within that ID domain)
- `delete()` can delete the vectors of the given IDs
- `recreate_collection()` can drop the old collection and create a new one (with a changeable dimension)

---

## 7. MockVectorStore (kernel/mock_vector.rs)

### 7.1 Contract semantics

MockVectorStore is an in-process, network-free vector store implementation for unit tests:
- Vectors are stored in an in-memory HashMap
- search uses brute-force scan to compute cosine similarity
- Supports candidate_ids pre-filtering (computes similarity only after filtering)
- No persistence (lost on process exit)

### 7.2 Implementation signature

```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use crate::traits::vector_store::{VectorStore, VectorMetadata, DistanceMetric, SearchHit};
use crate::types::{EntityId, Filters, error::Result};
use async_trait::async_trait;

#[derive(Clone)]
pub struct MockVectorStore {
    collections: Arc<RwLock<HashMap<String, MockCollection>>>,
}

struct MockCollection {
    dimension: usize,
    distance: DistanceMetric,
    points: HashMap<String, MockPoint>,
}

struct MockPoint {
    vector: Vec<f32>,
    metadata: VectorMetadata,
}

impl MockVectorStore {
    pub fn new() -> Self {
        Self {
            collections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return 0.0;
        }

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    }
}

#[async_trait]
impl VectorStore for MockVectorStore {
    async fn upsert(
        &self,
        collection: &str,
        ids: &[String],
        vectors: &[Vec<f32>],
        metadata: &[VectorMetadata],
    ) -> Result<()> {
        let mut collections = self.collections.write().unwrap();
        let coll = collections.entry(collection.to_string())
            .or_insert_with(|| MockCollection {
                dimension: vectors[0].len(),
                distance: DistanceMetric::Cosine,
                points: HashMap::new(),
            });

        for ((id, vector), meta) in ids.iter().zip(vectors.iter()).zip(metadata.iter()) {
            coll.points.insert(id.clone(), MockPoint {
                vector: vector.clone(),
                metadata: meta.clone(),
            });
        }

        Ok(())
    }

    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        _filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<SearchHit>> {
        let collections = self.collections.read().unwrap();
        let coll = collections.get(collection)
            .ok_or_else(|| crate::Error::VectorStore(format!("Collection not found: {}", collection)))?;

        let mut scores: Vec<(String, f32, VectorMetadata)> = coll.points.iter()
            .filter(|(id, point)| {
                // candidate ID pre-filter
                if let Some(candidates) = candidate_ids {
                    candidates.iter().any(|eid| eid.to_key() == point.metadata.entity_id)
                } else {
                    true
                }
            })
            .map(|(id, point)| {
                let score = Self::cosine_similarity(query_vector, &point.vector);
                (id.clone(), score, point.metadata.clone())
            })
            .collect();

        // sort by score descending
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let hits = scores.into_iter()
            .take(top_k)
            .map(|(id, score, metadata)| SearchHit { id, score, metadata })
            .collect();

        Ok(hits)
    }

    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()> {
        let mut collections = self.collections.write().unwrap();
        if let Some(coll) = collections.get_mut(collection) {
            for id in ids {
                coll.points.remove(id);
            }
        }
        Ok(())
    }

    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()> {
        let mut collections = self.collections.write().unwrap();
        collections.insert(collection.to_string(), MockCollection {
            dimension,
            distance: DistanceMetric::Cosine,
            points: HashMap::new(),
        });
        Ok(())
    }

    async fn ensure_collection(&self, collection: &str, dimension: usize, distance: DistanceMetric) -> Result<()> {
        let mut collections = self.collections.write().unwrap();
        collections.entry(collection.to_string())
            .or_insert_with(|| MockCollection {
                dimension,
                distance,
                points: HashMap::new(),
            });
        Ok(())
    }
}
```

**Acceptance criteria 7.x**:
- `MockVectorStore::new()` creates an empty instance
- `ensure_collection()` is idempotent: calling it multiple times with the same collection name doesn't error
- `upsert()` → `search()` → the returned scores are sorted descending by cosine similarity
- `search()` returns only results within the candidate ID domain when candidate_ids is non-empty
- After `delete()`, `search()` no longer returns the deleted IDs
- Unit test coverage: insert 3 vectors → query top-2 → verify 2 results are returned with correct scores

---

## 8. Implementation Notes for wiktor-builder

### 8.1 Dependency installation and environment preparation
- Ensure a Rust 1.75+ toolchain
- Start a local qdrant service (`docker run -p 6333:6333 qdrant/qdrant`) for integration tests
- Run `cargo check --workspace` to verify dependency resolution

### 8.2 Module implementation order
1. types module (no external dependencies, implement first)
2. traits module (depends on types, defines interfaces)
3. schema module (depends on rusqlite, implements DDL)
4. kernel/sqlite.rs (connection pool + pragma)
5. kernel/mock_vector.rs (for unit tests, before qdrant)
6. kernel/qdrant_vector.rs (depends on qdrant-client)
7. Write unit tests to validate each module

### 8.3 SQLite transaction boundaries
- Atomic publish = one transaction: pages + quality_scores + sections + FTS5 triggers + facts updates + generation increment
- The generation increment uses `UPDATE generation SET current = current + 1 WHERE id = 1; SELECT current FROM generation WHERE id = 1;`
- Fact-plane CAS write: `UPDATE facts SET value_numeric = ?1, source_revision = ?2 WHERE entity_id = ?3 AND field_name = ?4 AND source_revision < ?2`

### 8.4 Qdrant integration test isolation
- Each test uses a separate collection name (with a random suffix)
- Delete the collection at the end of the test (cleanup)
- Skip the test on connection failure rather than panicking (the CI environment may not have qdrant)

### 8.5 Error handling
- All trait methods return `Result<T, crate::Error>`
- Use `thiserror` to implement `std::error::Error` for the Error enum
- Use `From<rusqlite::Error>` / `From<std::io::Error>` to auto-convert underlying errors

---

## 9. Step-by-Step Acceptance Criteria Summary

| Step | Acceptance criteria | Acceptance method |
|------|---------|---------|
| 1.x Cargo config | `cargo check --workspace` passes | Command-line verification |
| 2.x Module layout | All mod.rs compile | `cargo check` |
| 3.x Types | EntityId round-trip conversion is lossless | Unit test `test_entity_id_roundtrip` |
| 4.x Traits | All traits are Send + Sync | Verified at compile time |
| 5.x Schema | migrate() idempotent, WAL enabled | Unit test `test_migrate_idempotent` |
| 6.x Qdrant | Connection succeeds, upsert + search correct | Integration test (requires docker) |
| 7.x MockVectorStore | Cosine similarity computed correctly | Unit test `test_mock_vector_store` |
| 8.x Overall | `cargo build --workspace` succeeds | Command-line verification |

---

## 10. Risk Points and Known Limitations

1. **Qdrant version dependency**: qdrant-client 1.11 may be incompatible with the latest qdrant server; needs testing to verify.
2. **FTS5 Chinese tokenization**: the default unicode61 tokenizer is limited for Chinese jargon; the MVP phase validates English first, and Chinese tokenization will be supplemented with domain pack vocabulary later.
3. **Mapping from Filters to qdrant filter conditions**: not fully implemented in this step; wiktor-builder must supplement it per the domain pack rules (see the 6.3 TODO comment).
4. **Generation alignment mechanism**: in this step, the alignment logic between the SQLite generation and qdrant collection naming defines only the naming convention; the full alignment algorithm needs to be added later (detect qdrant lag + trigger sync).
5. **CAS concurrency testing**: the facts table's source_revision CAS needs concurrent unit-test validation (multiple threads writing stale versions simultaneously; verify only the newer version takes effect).

---

## 11. Addendum v1.1 (main-model revision, 2026-09-20; authoritative over conflicts above)

> The v1.0 above was produced by planner-architect and is usable overall. The following revisions are the **final execution constraints**; where they conflict with the text above, this section prevails.

### A. Dependency alignment and trimming (fixes 1.1/1.2/1.3)

- **qdrant-client uses `1.19`** (matching server v1.19.1): `qdrant-client = { version = "1.19", default-features = false, features = ["gzip"] }`, as an **optional dependency + feature `vector-qdrant`** of `wiktor-core` (on by default).
- **No async-trait**: Rust 1.75+ supports native `async fn` in traits; write it directly and remove all `#[async_trait]` usage.
- **wiktor-core actual dependency trimming** (only the used ones): `rusqlite`(bundled), `serde`(derive), `serde_json`, `thiserror`, `blake3`, `tokio`(rt-multi-thread/macros/sync), `uuid`(v4/serde, for point ids), optional `qdrant-client`. **Removed**: petgraph / dashmap / moka / tracing / tracing-subscriber / tokio-stream / serde_yaml_ng / chrono / anyhow (left for later steps; not referenced this round; the workspace.dependencies declarations can stay, the crate doesn't list them).
- **wiktor-cli dependencies**: `wiktor-core`(features=["vector-qdrant"]), `clap`(derive), `anyhow`, `tokio`.
- `rust-version` uses `1.85` (1.97 available on the local machine; more relaxed than the spec's 1.75 as a safety margin).
- Timestamps use `std::time::SystemTime` unix seconds, **no chrono**.

### B. SearchHit naming conflict (fixes 3.3 and 4.8)

- The vector-layer hit in `traits/vector_store.rs` is renamed to **`VectorHit`** (fields `id`/`score`/`metadata: VectorMetadata`).
- The `SearchHit` in `types/query.rs` (page_id/entity_id/title/snippet/score/score_breakdown) is retained; it is the query-layer fusion result; `QueryLog.hits: Vec<SearchHit>` uses the query-layer version.
- The prelude exports only the query-layer `SearchHit` + the vector-layer `VectorHit`, avoiding same-name conflicts.

### C. Qdrant point id and candidate filtering fixes (fixes 6.3)

- **qdrant string point ids must be valid UUIDs**. `QdrantVectorStore` provides a deterministic generator:
  ```rust
  fn point_id(entity_id: &EntityId, chunk_type: ChunkType, generation: u64) -> String {
      let h = blake3::hash(format!("{}|{:?}|{}", entity_id.to_key(), chunk_type, generation).as_bytes());
      let b = h.as_bytes();
      uuid::Uuid::from_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15]]).to_string()
  }
  ```
  The same (entity, chunk, generation) overwrites idempotently; delete looks up by the same derivation.
- **Candidate ID filtering cannot use `Condition::HasId`** (that filters by point id; candidates are the entity_id strings in the payload). The correct approach:
  ```rust
  // candidate_ids: &[EntityId] → payload.entity_id keyword-match OR group
  let id_cond = Condition::matches_keyword("entity_id", ids.iter().map(|e| e.to_key()).collect::<Vec<_>>());
  builder = builder.filter(Filter::must([id_cond]));
  ```
  (`Condition::matches_keyword`'s set semantics = the field value matches any value in the set; valid at MVP scale.)

### D. Connection construction supports API key (fixes 6.3 `connect`)

```rust
pub fn from_config(url: &str, api_key: Option<&str>, dimension: usize) -> Result<Self>
```
- With an api_key, use `QdrantConfig::from_url(url).with_api_key(key)`.
- `connect(url, dimension)` is kept as shorthand for `from_config(url, None, dimension)`.
- Default port convention: if the url has no port, use 6334 (gRPC).

### E. Complete SqliteKernel and EntityStore (new 5.7/6.4; missing above)

`kernel/sqlite.rs`:

```rust
pub struct SqliteKernel { conn: std::sync::Mutex<rusqlite::Connection> }

impl SqliteKernel {
    pub fn open(path: &std::path::Path) -> Result<Self>;   // create if it does not exist
    pub fn open_in_memory() -> Result<Self>;
    pub fn migrate(&self) -> Result<()>;                   // call schema::migrations::migrate while holding the lock
    pub fn schema_version(&self) -> Result<i64>;
    pub fn row_counts(&self) -> Result<BTreeMap<String, i64>>; // pages/quality_scores/sections/facts/fact_refs/compile_tasks/query_logs/generation
}
```

`impl EntityStore for SqliteKernel` (inside async fns, use `tokio::task::spawn_blocking` + the lock):

- `upsert_facts`: **single-statement CAS** (the UPDATE-only form in spec 8.3 fails on first write; switch to):
  ```sql
  INSERT INTO facts(entity_id, field_name, field_type, value_numeric, value_text, value_boolean, value_timestamp, source_revision, updated_at)
  VALUES (?1,?2,?3,?4,?5,?6,?7,?8, unixepoch())
  ON CONFLICT(entity_id, field_name) DO UPDATE SET
      field_type=excluded.field_type, value_numeric=excluded.value_numeric,
      value_text=excluded.value_text, value_boolean=excluded.value_boolean,
      value_timestamp=excluded.value_timestamp, source_revision=excluded.source_revision,
      updated_at=unixepoch()
  WHERE excluded.source_revision > facts.source_revision;
  ```
  reflist fields are written to `fact_refs` row by row (delete the old rows for that (entity, field) first, then insert the new ones, same transaction).
- `filter`: recursively translate `FilterCondition` →
  - `NumericRange{field,min,max}` → `(value_numeric >= min) AND (value_numeric <= max)` (omit the corresponding clause on one-sided ranges)
  - `TextEquals{field,value}` → `value_text = ?`
  - `RefContains{field,refs}` → `EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN (...))`
  - `RefExcludes{field,refs}` → `NOT EXISTS (...same as above...)`
  - Multiple conditions joined with AND; empty conditions return everything; always `SELECT DISTINCT entity_id FROM facts WHERE ... LIMIT 10000`.
- `delete_facts`: delete facts + fact_refs (transaction).
- `get_facts`: read back `Facts`.

### F. Complete the CLI (new 9.x; above only has the module layout)

`crates/wiktor-cli/src/main.rs` (clap derive):

```
wiktor status [--db PATH]                # default ./wiktor.db; open+migrate, print schema version + row count for each table (one BTreeMap entry per line)
wiktor vector ping [--url URL] [--api-key KEY]
                                         # default URL=${WIKTOR_QDRANT_URL:-http://127.0.0.1:6334}
                                         # KEY priority: --api-key, then $WIKTOR_QDRANT_API_KEY
```

- `status`: `SqliteKernel::open` → `migrate` → print `schema version: N` + lines like `pages: 0`. If the parent directory of the DB path doesn't exist, `std::fs::create_dir_all`.
- `vector ping`: `QdrantVectorStore::from_config(url, key, 768)` → `health_check()` (`client.health_check()`), print `qdrant ok: <version>` on success, `anyhow!` and exit code 1 on failure.
- A top-level doc comment briefly describes Wiktor's positioning.

### G. Fix pages.content_hash (fixes 5.2)

- `content_hash TEXT NOT NULL UNIQUE` → **remove UNIQUE** (keep `CREATE INDEX idx_pages_content_hash`): neither re-compiled artifacts with identical content (rare but legal) nor different entities with identical content should be rejected by a unique constraint.

### H. Test discipline (covers 8.4)

- qdrant integration tests: read `WIKTOR_QDRANT_URL` (default `http://127.0.0.1:6334`) and `WIKTOR_QDRANT_API_KEY` (optional); on connection failure → `eprintln!` + **skip** (no panic); each test uses a separate collection name (`test_{name}_{random}`), with `#[tokio::test]` tail cleanup via `drop_collection`.
- Mock unit tests have no network dependency and are the CI mainstay; qdrant integration tests are marked `#[ignore]` (run locally when `cargo test -- --include-ignored`).

### I. Acceptance criteria additions (summary)

1. `cargo build --workspace` with zero errors; `cargo test --workspace` all green (Mock path).
2. `cargo run -p wiktor-cli -- status --db /tmp/wiktor-t1.db` outputs `schema version: 1` + row counts for 6~8 tables (0 at first creation).
3. `cargo run -p wiktor-cli -- vector ping` connects to `127.0.0.1:6334` and outputs `qdrant ok`.
4. Integration (when a local qdrant exists): in `cargo test -- --include-ignored`, the vector integration cases ensure→upsert→search→delete→drop all pass; candidate filtering (candidate_ids) returns only results within the domain.

## 12. Implementation Revision Record (wiktor-builder / main model, 2026-09-20)

The following are revisions to the spec made during implementation; all have landed in code and been validated:

1. **A-fix (async-trait decision reversal)**: Addendum A once required "native async fn, no async-trait" — **wrong**. MASTER-PLAN Section 7 requires traits such as `DomainPack::compiler() -> Box<dyn Compiler>` to work as trait objects, and native async fns are not object-safe. Ultimately all traits use `#[async_trait]` (`wiktor-core` adds the `async-trait = "0.1"` dependency). The related wording in Addendum A is void.
2. **qdrant-client 1.19 API alignment**: `Qdrant::from_url(url).api_key(key).build()`; `PointStruct::new(id, Vec<f32>, HashMap<String,Value>)`; `UpsertPointsBuilder::new(collection, points)`; `SearchPointsBuilder::new(collection, vector, limit)`; `Condition::matches("entity_id", Vec<String>)` implements candidate-set filtering (qdrant has no `matches_keyword` helper; `matches` accepts `Vec<String>` and auto-converts to Keywords); payloads are built with `value::Kind::StringValue/IntegerValue`.
3. **BUG-1 fix (NumericRange parameter order)**: the NumericRange branch of `translate_filters` used to push `[min, max, field_name]` onto the stack while `field_name = ?` comes first in the SQL, causing any numeric range filter to always be empty. Changed to push field_name first. Regression test: `numeric_range_filter_finds_expected_entity`.
4. **BUG-2 fix (reflist bypassing CAS)**: in `upsert_facts`, the delete-and-insert of `fact_refs` used to run unconditionally, so an old revision could overwrite a newer revision's reflist. Changed so that fact_refs is rewritten only when the facts-row CAS takes effect (`execute` returns changes()==1). Regression test: `reflist_write_respects_cas`.
5. **Table naming**: `page_quality` / `page_sections` / `generations` (vs. the spec draft's `quality_scores` / `sections` / `generation`) — the implementation adopts the v1.1 architecture with "generation table storing history"; `row_counts` outputs 8 tables, satisfying §I.2's "6~8 tables".
6. **Mock vs. qdrant delete semantics difference** (known, not a bug): Mock silently Ok's on a nonexistent collection, qdrant errors — when the query path is wrapped uniformly, handle per qdrant semantics.

---

**Document version**: v1.2 (contains Addendum v1.1 + implementation revision record)
**Next step**: wiktor-builder implements the code per v1.1, and after each item passes acceptance, proceed to Step 2 (20 hand-written seed Wiki pages + JSONL data source + query closed loop).