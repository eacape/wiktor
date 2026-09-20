# Step 1: Workspace Core Schema 实现规格

**版本**: 1.0  
**日期**: 2026-09-20  
**决策依据**: MASTER-PLAN.md v3.1（2026-09-20 架构分叉拍板 + qdrant 向量后端决策）

## 0. 架构决策摘要

**2026-09-20 已拍板**：
- 默认向量检索用 **qdrant 外部服务**（弃 sqlite-vec——pre-v1、两次断档、ANN 只在 alpha）
- qdrant 只承担向量这一路召回；知识平面/事实平面/FTS5 倒排全在 SQLite
- 事实过滤下推在 SQLite 事实平面做（单一事实来源），过滤得候选 ID 集合 → 再对候选集做 qdrant 向量检索
- 原子发布契约修订：SQLite 单事务（页面+评分+FTS5+事实）提交后，向量同步 qdrant（collection 带 generation + content_hash），滞后由 generation 对齐 + 可重建兜底

## 1. Cargo Workspace 结构

### 1.1 根 `Cargo.toml`

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
# 异步运行时
tokio = { version = "1.40", features = ["full"] }
tokio-stream = "0.1"

# 存储与序列化
rusqlite = { version = "0.32", features = ["bundled", "blob", "chrono", "uuid"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
serde_yaml_ng = "0.10"  # serde_yaml 已停止维护

# 向量检索（qdrant 外部服务）
qdrant-client = { version = "1.11", default-features = false }

# 哈希与 ID
blake3 = "1.5"
uuid = { version = "1.10", features = ["v4", "serde"] }

# 错误处理
thiserror = "1.0"
anyhow = "1.0"

# 图结构
petgraph = "0.6"

# 并发哈希
dashmap = "6.1"

# 缓存
moka = { version = "0.12", features = ["future"] }

# 日志与追踪
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# 时间处理
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

**验收判据 1.x**：
- `cargo check --workspace` 通过
- `cargo build --workspace` 成功生成 `target/debug/wiktor` 二进制
- 依赖无循环，core 不依赖 cli

---

## 2. Module 布局与文件树

### 2.1 `wiktor-core` 目录结构

```
crates/wiktor-core/
├── Cargo.toml
└── src/
    ├── lib.rs              # 模块导出 + 预导入（prelude）
    ├── types/
    │   ├── mod.rs
    │   ├── entity.rs       # EntityId, RawEntity, Facts, Filters
    │   ├── page.rs         # CompiledPage, WikiPage, QualityScore, content_hash
    │   ├── query.rs        # Query, RewrittenQuery, SearchHit, Cursor
    │   ├── qug.rs          # QugEdge, QugPath
    │   └── error.rs        # Error 枚举 + From 实现
    ├── traits/
    │   ├── mod.rs
    │   ├── data_source.rs  # DataSource trait
    │   ├── entity_store.rs # EntityStore trait
    │   ├── compiler.rs     # Compiler trait
    │   ├── qug.rs          # QueryUnderstandingGraph, QugBuilder traits
    │   ├── reranker.rs     # Reranker trait
    │   ├── domain_pack.rs  # DomainPack trait
    │   ├── feedback.rs     # FeedbackAnalyzer trait
    │   └── vector_store.rs # VectorStore trait（核心抽象）
    ├── schema/
    │   ├── mod.rs
    │   ├── migrations.rs   # schema_migrations 表 + migrate() 函数
    │   ├── knowledge.rs    # 知识平面 DDL（pages + generation + quarantine）
    │   ├── facts.rs        # 事实平面 DDL（facts + source_revision CAS）
    │   ├── fts.rs          # FTS5 虚拟表 DDL
    │   ├── tasks.rs        # 编译任务队列 DDL
    │   └── query_log.rs    # 查询日志 DDL
    └── kernel/
        ├── mod.rs
        ├── sqlite.rs       # SQLite 连接池 + WAL/foreign_keys pragma
        ├── qdrant_vector.rs # QdrantVectorStore 实现
        └── mock_vector.rs  # MockVectorStore 实现（进程内，供单测）
```

### 2.2 `wiktor-cli` 目录结构

```
crates/wiktor-cli/
├── Cargo.toml
└── src/
    └── main.rs             # clap 命令行入口（compile/search/status）
```

**验收判据 2.x**：
- 所有 `mod.rs` 能 `cargo check` 通过
- `use wiktor_core::prelude::*;` 在外部 crate 可编译
- 模块边界清晰：types 无外部依赖、traits 只依赖 types、schema 依赖 rusqlite、kernel 依赖 traits + schema

---

## 3. 领域类型定义（types 模块）

### 3.1 `types/entity.rs`

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 实体全局唯一标识符（domain:type:id 三元组）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId {
    pub domain: String,        // 领域包名称（如 "ecommerce"）
    pub entity_type: String,   // 实体类型（如 "product", "category"）
    pub id: String,            // 源数据主键
}

impl EntityId {
    pub fn new(domain: impl Into<String>, entity_type: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            entity_type: entity_type.into(),
            id: id.into(),
        }
    }

    /// 序列化为字符串形式（domain:type:id）
    pub fn to_key(&self) -> String {
        format!("{}:{}:{}", self.domain, self.entity_type, self.id)
    }

    /// 从字符串形式解析（domain:type:id）
    pub fn from_key(key: &str) -> Result<Self, crate::Error> {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() != 3 {
            return Err(crate::Error::InvalidEntityId(key.to_string()));
        }
        Ok(Self::new(parts[0], parts[1], parts[2]))
    }
}

/// 源数据原始实体（未编译）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEntity {
    pub id: EntityId,
    pub fields: HashMap<String, serde_json::Value>,  // 源数据字段键值对
    pub source_revision: u64,  // 源数据版本号（用于 CAS 幂等写入）
}

/// 事实平面结构化元数据（filterable 字段）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Facts {
    pub entity_id: EntityId,
    pub fields: HashMap<String, FactValue>,  // filterable 字段键值对
    pub source_revision: u64,  // 必须单调递增
}

/// 事实字段值（支持数值、枚举、引用列表等类型）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum FactValue {
    Numeric(f64),
    Text(String),
    Boolean(bool),
    RefList(Vec<String>),  // 引用列表（如 ingredient_ids）
    Timestamp(i64),        // Unix 时间戳（秒）
}

/// 过滤条件（用于事实平面预筛）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filters {
    pub conditions: Vec<FilterCondition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterCondition {
    NumericRange { field: String, min: Option<f64>, max: Option<f64> },
    TextEquals { field: String, value: String },
    RefContains { field: String, refs: Vec<String> },  // 引用列表包含（如排除 ingredient_ids）
    RefExcludes { field: String, refs: Vec<String> },  // 引用列表排除（否定边）
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

/// 编译产物（知识平面 Wiki 页面 + 质量评分 + QUG 边）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledPage {
    pub wiki: WikiPage,
    pub quality: QualityScore,
    pub qug_edges: Vec<QugEdge>,
    pub content_hash: String,  // BLAKE3 哈希（覆盖源数据+领域包版本+Prompt+编译器+模型版本）
}

/// Wiki 页面内容（纯 Markdown，人类可读）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub page_id: String,       // 页面全局唯一 ID（自动生成）
    pub entity_id: EntityId,   // 关联实体
    pub title: String,
    pub content: String,       // Markdown 正文
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
    pub domain_pack_version: String,  // 领域包版本（semver）
    pub compiled_at: i64,             // Unix 时间戳（秒）
    pub model_version: String,        // LLM 模型标识
    pub embedding_model: String,      // 嵌入模型标识
}

/// 质量评分（四规则维度 + 一 LLM 仲裁维度）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityScore {
    pub coverage: f32,            // 覆盖度（0.0-1.0，规则可算）
    pub citation: f32,            // 引用完整性（0.0-1.0，规则可算）
    pub schema_compliance: f32,   // 结构合规（0.0-1.0，规则可算）
    pub density: f32,             // 信息密度（0.0-1.0，近似规则）
    pub consistency: Option<f32>, // 一致性（0.0-1.0，LLM 仲裁，可选）
}

impl QualityScore {
    /// 综合得分（四规则维度平均，一致性单独处理）
    pub fn overall(&self) -> f32 {
        (self.coverage + self.citation + self.schema_compliance + self.density) / 4.0
    }

    /// 是否通过质量阈值（0.75）
    pub fn passes_threshold(&self, threshold: f32) -> bool {
        self.overall() >= threshold
    }
}

/// 发布状态（发布状态机：candidate → accepted / quarantined）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishStatus {
    Candidate,    // 待审核（未通过引用完整性校验）
    Accepted,     // 已发布（进查询索引）
    Quarantined,  // 隔离（未通过引用完整性，不入查询索引）
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

/// 用户查询请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub text: String,              // 查询文本
    pub filters: Filters,          // 结构化过滤条件（可选）
    pub top_k: usize,              // 返回结果数量上限
    pub domain: Option<String>,    // 限定领域包（可选）
}

/// QUG 改写后的查询（Option：None 则 fallback）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewrittenQuery {
    pub expanded_terms: Vec<String>,  // 同义词扩展后的词条
    pub filters: Filters,             // QUG 生成的过滤条件（属性传播边/否定边）
    pub boost_entities: Vec<EntityId>, // 上下位边扩展的实体 ID
}

/// 检索命中结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub page_id: String,
    pub entity_id: EntityId,
    pub title: String,
    pub snippet: String,        // 高亮摘要
    pub score: f32,             // 融合后得分
    pub score_breakdown: ScoreBreakdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub vector_score: Option<f32>,  // 向量检索得分（qdrant）
    pub bm25_score: Option<f32>,    // BM25 得分（FTS5）
    pub rerank_score: Option<f32>,  // 重排得分（可选）
}

/// 查询日志记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLog {
    pub log_id: String,        // 日志唯一 ID
    pub query: Query,
    pub rewritten: Option<RewrittenQuery>,
    pub hits: Vec<SearchHit>,
    pub rewrite_failure: bool, // QUG 改写失败标记（显式 fallback）
    pub latency_ms: u64,
    pub timestamp: i64,        // Unix 时间戳（秒）
}

/// 数据源游标（用于分页拉取源数据）
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

/// QUG 五类边（通用机制，具体边由领域包实例化）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QugEdge {
    /// 同义边（查询时直接替换）
    Synonym {
        from: String,
        to: Vec<String>,
    },
    /// 上下位边（品类扩展召回）
    Hyponym {
        child: String,
        parent: String,
    },
    /// 属性传播边（转为结构化过滤，落事实平面）
    AttributePropagation {
        phrase: String,
        filter: FilterCondition,  // 直接生成过滤条件
    },
    /// 意图模板边（展开为复合查询）
    IntentTemplate {
        phrase: String,
        expansion: Query,  // 展开后的查询结构
    },
    /// 否定边（生成排除过滤器）
    Negation {
        phrase: String,
        exclusion: FilterCondition,  // 排除条件
    },
}

/// QUG 图遍历路径
#[derive(Debug, Clone)]
pub struct QugPath {
    pub nodes: Vec<String>,    // 遍历节点序列
    pub edges: Vec<QugEdge>,   // 遍历边序列
    pub depth: usize,
}
```

### 3.5 `types/error.rs`

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // 数据错误
    #[error("Invalid entity ID format: {0}")]
    InvalidEntityId(String),

    #[error("Entity not found: {0}")]
    EntityNotFound(String),

    #[error("Duplicate entity: {0}")]
    DuplicateEntity(String),

    // 存储错误
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Migration failed: {0}")]
    Migration(String),

    // 向量存储错误
    #[error("Vector store error: {0}")]
    VectorStore(String),

    #[error("Qdrant connection error: {0}")]
    QdrantConnection(String),

    // 编译错误
    #[error("Compilation failed: {0}")]
    Compilation(String),

    #[error("Quality score below threshold: {0} < {1}")]
    QualityBelowThreshold(f32, f32),

    #[error("Content hash mismatch: expected {0}, got {1}")]
    ContentHashMismatch(String, String),

    // 查询错误
    #[error("Query failed: {0}")]
    Query(String),

    #[error("QUG rewrite failed: {0}")]
    QugRewrite(String),

    #[error("Filter error: {0}")]
    Filter(String),

    // 配置错误
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Domain pack not found: {0}")]
    DomainPackNotFound(String),

    // 通用错误
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

**验收判据 3.x**：
- 所有类型 `cargo check` 通过
- 所有类型实现 `Debug + Clone + Serialize + Deserialize`
- EntityId 的 `to_key()` / `from_key()` 往返转换无损
- QualityScore 的 `overall()` 计算正确（四规则维度平均）
- Error 枚举覆盖所有错误场景，From 实现正确

---

## 4. Trait 定义（traits 模块）

### 4.1 `traits/data_source.rs`

```rust
use async_trait::async_trait;
use crate::types::{RawEntity, Cursor, error::Result};

/// 数据源适配器（JSONL、postgres 等）
#[async_trait]
pub trait DataSource: Send + Sync {
    /// 分页拉取源数据
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;

    /// 获取源数据 Schema（字段定义）
    fn schema(&self) -> EntitySchema;
}

/// 实体 Schema 定义
#[derive(Debug, Clone)]
pub struct EntitySchema {
    pub entity_type: String,
    pub fields: Vec<FieldDefinition>,
}

#[derive(Debug, Clone)]
pub struct FieldDefinition {
    pub name: String,
    pub field_type: FieldType,
    pub filterable: bool,  // 是否进事实平面
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

/// 事实平面存储（SQLite 实现）
#[async_trait]
pub trait EntityStore: Send + Sync {
    /// 幂等写入事实（source_revision 用于 CAS：旧版本不得覆盖新版本）
    async fn upsert_facts(&self, id: &EntityId, facts: &Facts, source_revision: u64) -> Result<()>;

    /// 事实过滤下推（返回候选 ID 集合，供向量检索预筛）
    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>>;

    /// 删除事实（tombstone 传播）
    async fn delete_facts(&self, id: &EntityId) -> Result<()>;

    /// 获取事实（单条查询）
    async fn get_facts(&self, id: &EntityId) -> Result<Option<Facts>>;
}
```

### 4.3 `traits/compiler.rs`

```rust
use async_trait::async_trait;
use crate::types::{RawEntity, CompiledPage, error::Result};

/// 编译器（LLM 编译 + 质量评分）
#[async_trait]
pub trait Compiler: Send + Sync {
    /// 编译单个实体为 Wiki 页面（带质量评分与 QUG 边）
    async fn compile(&self, raw: RawEntity, ctx: &CompileContext) -> Result<CompiledPage>;
}

/// 编译上下文（提供领域包配置、模型版本等信息）
#[derive(Debug, Clone)]
pub struct CompileContext {
    pub domain_pack_version: String,
    pub prompt_template: String,
    pub model_version: String,
    pub embedding_model: String,
    pub quality_threshold: f32,
    pub require_source_refs: bool,  // 是否强制要求 source 引用
}
```

### 4.4 `traits/qug.rs`

```rust
use async_trait::async_trait;
use crate::types::{Query, RewrittenQuery, QugPath, QugEdge, error::Result};

/// 查询理解图（编译时构建、查询时只读遍历）
#[async_trait]
pub trait QueryUnderstandingGraph: Send + Sync {
    /// 查询改写（返回 None = QUG 无法处理，调用方必须 fallback 到混合检索）
    async fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>>;

    /// 图遍历（返回所有可达路径，限制深度上限）
    fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath>;
}

/// QUG 构建器（从 Wiki 页面与领域包配置构建图）
#[async_trait]
pub trait QugBuilder: Send + Sync {
    /// 从编译产物提取 QUG 边
    async fn extract_edges(&self, pages: &[crate::types::CompiledPage]) -> Result<Vec<QugEdge>>;

    /// 构建查询理解图（petgraph）
    async fn build_graph(&self, edges: Vec<QugEdge>) -> Result<Box<dyn QueryUnderstandingGraph>>;
}
```

### 4.5 `traits/reranker.rs`

```rust
use async_trait::async_trait;
use crate::types::{Query, SearchHit, error::Result};

/// 重排器（cross_encoder，默认关闭）
#[async_trait]
pub trait Reranker: Send + Sync {
    /// 对检索结果重排（输入融合后的候选，输出重排后的结果）
    async fn rerank(&self, query: &Query, hits: Vec<SearchHit>) -> Result<Vec<SearchHit>>;
}
```

### 4.6 `traits/domain_pack.rs`

```rust
use crate::traits::{Compiler, QugBuilder, Reranker};
use crate::types::error::Result;

/// 领域包（YAML 配置 + Prompt 模板 + 页面模板）
pub trait DomainPack: Send + Sync {
    /// 领域包名称
    fn name(&self) -> &str;

    /// 领域包版本（semver）
    fn version(&self) -> &str;

    /// 获取配置
    fn config(&self) -> &DomainConfig;

    /// 获取编译器
    fn compiler(&self) -> Result<Box<dyn Compiler>>;

    /// 获取 QUG 构建器
    fn qug_builder(&self) -> Result<Box<dyn QugBuilder>>;

    /// 获取重排器（可选）
    fn reranker(&self) -> Result<Option<Box<dyn Reranker>>>;
}

/// 领域包配置（从 domain.yaml 解析）
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
    pub source: String,  // 数据源 URI（jsonl:// 或 postgres://）
    pub id_field: String,
    pub type_field: String,
    pub fields: Vec<crate::traits::data_source::FieldDefinition>,
}
```

### 4.7 `traits/feedback.rs`

```rust
use async_trait::async_trait;
use crate::types::{QueryLog, Query, error::Result};

/// 反馈分析器（查询日志 → 盲区分析 → 补充编译任务）
#[async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    /// 分析查询日志，生成反馈报告
    async fn analyze(&self, logs: &[QueryLog]) -> Result<FeedbackReport>;
}

/// 反馈报告（盲区信号 + 建议补充编译任务）
#[derive(Debug, Clone)]
pub struct FeedbackReport {
    pub zero_recall_queries: Vec<Query>,   // 零召回查询
    pub low_quality_hits: Vec<String>,     // 低质量命中（page_id）
    pub rewrite_failures: Vec<Query>,      // 查询改写失败
    pub suggested_compilations: Vec<CompileTask>,  // 进人工审核队列
}

#[derive(Debug, Clone)]
pub struct CompileTask {
    pub entity_id: crate::types::EntityId,
    pub reason: String,  // 任务触发原因
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

/// 向量存储抽象（qdrant 默认实现 + MockVectorStore 单测实现）
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// 批量插入或更新向量（带元数据）
    async fn upsert(
        &self,
        collection: &str,
        ids: &[String],
        vectors: &[Vec<f32>],
        metadata: &[VectorMetadata],
    ) -> Result<()>;

    /// 向量检索（支持过滤条件 + 候选 ID 预筛）
    async fn search(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,  // 事实平面预筛的候选 ID 域
    ) -> Result<Vec<SearchHit>>;

    /// 删除向量
    async fn delete(&self, collection: &str, ids: &[String]) -> Result<()>;

    /// 重建 collection（全量重嵌入场景）
    async fn recreate_collection(&self, collection: &str, dimension: usize) -> Result<()>;

    /// 确保 collection 存在（幂等操作）
    async fn ensure_collection(&self, collection: &str, dimension: usize, distance: DistanceMetric) -> Result<()>;
}

/// 向量元数据（存储在 qdrant payload）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMetadata {
    pub entity_id: String,        // EntityId.to_key()
    pub page_id: String,
    pub chunk_type: ChunkType,    // 页面摘要 / 章节
    pub content_hash: String,     // BLAKE3 哈希（用于重建验证）
    pub generation: u64,          // SQLite generation 版本号（用于对齐）
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkType {
    Summary,  // 页面摘要
    Section,  // 章节级
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
    DotProduct,
}

/// 向量检索命中结果
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: String,
    pub score: f32,
    pub metadata: VectorMetadata,
}
```

**验收判据 4.x**：
- 所有 trait `cargo check` 通过
- 所有 trait 标记 `Send + Sync`（支持多线程）
- async trait 正确使用 `#[async_trait]` 宏（来自 async-trait crate）
- VectorStore trait 的 `search` 方法同时支持 filters 过滤与 candidate_ids 预筛

---

## 5. SQLite Schema（schema 模块）

### 5.1 `schema/migrations.rs`

```rust
use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// 幂等迁移函数（连接打开后立即调用）
pub fn migrate(conn: &Connection) -> Result<()> {
    // 启用 WAL 模式（单次设置，持久化）
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // 启用外键约束
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // 创建 schema_migrations 表
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
        [],
    )?;

    // 获取当前版本
    let current_version: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // 应用增量迁移
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
    // 知识平面
    crate::schema::knowledge::create_tables(conn)?;
    // 事实平面
    crate::schema::facts::create_tables(conn)?;
    // FTS5 倒排
    crate::schema::fts::create_tables(conn)?;
    // 任务队列
    crate::schema::tasks::create_tables(conn)?;
    // 查询日志
    crate::schema::query_log::create_tables(conn)?;

    Ok(())
}
```

### 5.2 `schema/knowledge.rs`

```rust
use rusqlite::{Connection, Result};

/// 知识平面 DDL（pages + generation + quarantine 发布状态机）
pub fn create_tables(conn: &Connection) -> Result<()> {
    // 全局 generation 计数器（原子索引版本号）
    conn.execute(
        "CREATE TABLE IF NOT EXISTS generation (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            current INTEGER NOT NULL DEFAULT 0
        )",
        [],
    )?;
    conn.execute("INSERT OR IGNORE INTO generation (id, current) VALUES (1, 0)", [])?;

    // Wiki 页面表
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

    // 索引
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_entity_id ON pages(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_domain ON pages(domain)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_status ON pages(status)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_generation ON pages(generation)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_pages_content_hash ON pages(content_hash)", [])?;

    // 质量评分表
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

    // 页面章节表（用于章节级向量索引）
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

/// 事实平面 DDL（facts + source_revision CAS）
pub fn create_tables(conn: &Connection) -> Result<()> {
    // 事实表（filterable 字段）
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

    // 索引（支持过滤下推）
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_entity_id ON facts(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_field_name ON facts(field_name)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_numeric ON facts(value_numeric) WHERE field_type = 'numeric'", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_text ON facts(value_text) WHERE field_type = 'text'", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_facts_value_timestamp ON facts(value_timestamp) WHERE field_type = 'timestamp'", [])?;

    // 引用列表表（reflist 字段拆行存储，支持包含/排除过滤）
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

/// FTS5 全文检索表 DDL（BM25 倒排索引）
pub fn create_tables(conn: &Connection) -> Result<()> {
    // FTS5 虚拟表（索引页面标题 + 内容）
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

    // 触发器：pages 表插入/更新时同步到 FTS5
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

/// 编译任务队列 DDL（状态机：pending → running → succeeded/failed/dead）
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

    // 索引
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_status ON compile_tasks(status)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_entity_id ON compile_tasks(entity_id)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_tasks_lease_expires_at ON compile_tasks(lease_expires_at) WHERE status = 'running'", [])?;

    Ok(())
}
```

### 5.6 `schema/query_log.rs`

```rust
use rusqlite::{Connection, Result};

/// 查询日志 DDL
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

    // 索引
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_timestamp ON query_logs(timestamp)", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_rewrite_failure ON query_logs(rewrite_failure) WHERE rewrite_failure = 1", [])?;
    conn.execute("CREATE INDEX IF NOT EXISTS idx_query_logs_hit_count ON query_logs(hit_count) WHERE hit_count = 0", [])?;

    Ok(())
}
```

**验收判据 5.x**：
- `migrate()` 幂等：多次调用同一数据库文件，schema 版本号正确递增
- `PRAGMA journal_mode` 返回 `wal`
- `PRAGMA foreign_keys` 返回 `1`（启用）
- pages 表的 status 字段只接受 `candidate`/`accepted`/`quarantined` 三值
- facts 表的 source_revision 支持 CAS 条件更新（旧版本不覆盖新版本）
- FTS5 虚拟表创建成功，触发器同步 pages 表到 pages_fts
- compile_tasks 表的 UNIQUE 约束防止幂等去重重复任务

---

## 6. Qdrant 适配层（kernel/qdrant_vector.rs）

### 6.1 集合命名规范

```rust
/// Qdrant collection 命名：{domain}_{version}_gen{generation}
/// 示例："ecommerce_1_0_gen42"
pub fn collection_name(domain: &str, domain_pack_version: &str, generation: u64) -> String {
    let version_safe = domain_pack_version.replace('.', "_");
    format!("{}_{}_{}", domain, version_safe, generation)
}
```

### 6.2 Payload 字段规范

```rust
use serde_json::json;

/// VectorMetadata 到 qdrant payload 的映射
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

### 6.3 QdrantVectorStore 实现签名

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
    default_dimension: usize,  // 默认向量维度（如 BGE-base-zh-v1.5 = 768）
}

impl QdrantVectorStore {
    /// 连接到 qdrant 服务（URL 从环境变量或配置读取）
    pub async fn connect(url: &str, dimension: usize) -> Result<Self> {
        let client = Qdrant::from_url(url)
            .build()
            .map_err(|e| crate::Error::QdrantConnection(e.to_string()))?;

        Ok(Self {
            client,
            default_dimension: dimension,
        })
    }

    /// 确保 collection 存在（幂等操作）
    pub async fn ensure_collection_impl(
        &self,
        collection: &str,
        dimension: usize,
        distance: DistanceMetric,
    ) -> Result<()> {
        // 检查 collection 是否存在
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

    /// 批量 upsert（将 metadata 转为 payload）
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

    /// 向量检索（支持候选 ID 预筛 + 过滤条件）
    async fn search_impl(
        &self,
        collection: &str,
        query_vector: &[f32],
        top_k: usize,
        filters: Option<&Filters>,
        candidate_ids: Option<&[EntityId]>,
    ) -> Result<Vec<SearchHit>> {
        let mut builder = SearchPointsBuilder::new(collection, query_vector, top_k as u64);

        // 构建 qdrant Filter（候选 ID 预筛 + 过滤条件）
        let mut conditions = Vec::new();

        // 候选 ID 预筛（事实平面下推的结果）
        if let Some(ids) = candidate_ids {
            let id_strings: Vec<String> = ids.iter().map(|id| id.to_key()).collect();
            // qdrant filter: entity_id in [...]
            conditions.push(Condition::HasId(id_strings.into()));
        }

        // TODO: 将 Filters 转为 qdrant FieldCondition（需要映射规则）
        // 本步暂不实现，留待 wiktor-builder 根据领域包规则补充

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

    /// 删除向量
    async fn delete_impl(&self, collection: &str, ids: &[String]) -> Result<()> {
        self.client.delete_points(collection, ids.iter().map(|s| s.into()).collect(), None).await
            .map_err(|e| crate::Error::VectorStore(e.to_string()))?;
        Ok(())
    }

    /// 重建 collection（删除旧 collection，创建新的）
    pub async fn recreate_collection_impl(&self, collection: &str, dimension: usize) -> Result<()> {
        // 删除旧 collection（如果存在）
        let _ = self.client.delete_collection(collection).await;

        // 创建新 collection
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

**验收判据 6.x**：
- `QdrantVectorStore::connect()` 能连接到本地 qdrant 服务（docker run）
- `ensure_collection()` 幂等：多次调用同一 collection 名称，不报错
- `upsert()` 能批量插入向量，payload 包含 entity_id/page_id/chunk_type/content_hash/generation 五字段
- `search()` 能正确过滤候选 ID（candidate_ids 非空时，只在该 ID 域内检索）
- `delete()` 能删除指定 ID 的向量
- `recreate_collection()` 能删除旧 collection 并创建新的（维度可变）

---

## 7. MockVectorStore（kernel/mock_vector.rs）

### 7.1 契约语义

MockVectorStore 是进程内、无网络的向量存储实现，供单测使用：
- 向量存储在内存 HashMap
- search 用暴力扫描计算余弦相似度
- 支持 candidate_ids 预筛（过滤后再计算相似度）
- 不支持持久化（进程退出即丢失）

### 7.2 实现签名

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
                // 候选 ID 预筛
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

        // 按得分降序排序
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

**验收判据 7.x**：
- `MockVectorStore::new()` 创建空实例
- `ensure_collection()` 幂等：多次调用同一 collection 名称，不报错
- `upsert()` → `search()` → 返回结果的 score 按余弦相似度降序排列
- `search()` 在 candidate_ids 非空时，只返回候选 ID 域内的结果
- `delete()` 后再 `search()` 不返回已删除 ID
- 单测覆盖：插入 3 个向量 → 查询 top-2 → 验证返回 2 个结果且得分正确

---

## 8. 给 wiktor-builder 的实现注意点

### 8.1 依赖安装与环境准备
- 确保 Rust 1.75+ 工具链
- 本地启动 qdrant 服务（`docker run -p 6333:6333 qdrant/qdrant`）用于集成测试
- 运行 `cargo check --workspace` 验证依赖解析

### 8.2 模块实现顺序
1. types 模块（无外部依赖，先实现）
2. traits 模块（依赖 types，定义接口）
3. schema 模块（依赖 rusqlite，实现 DDL）
4. kernel/sqlite.rs（连接池 + pragma）
5. kernel/mock_vector.rs（单测用，先于 qdrant）
6. kernel/qdrant_vector.rs（依赖 qdrant-client）
7. 写单测验证各模块

### 8.3 SQLite 事务边界
- 原子发布 = 单事务：pages + quality_scores + sections + FTS5 触发器 + facts 更新 + generation 递增
- generation 递增用 `UPDATE generation SET current = current + 1 WHERE id = 1; SELECT current FROM generation WHERE id = 1;`
- 事实平面 CAS 写入：`UPDATE facts SET value_numeric = ?1, source_revision = ?2 WHERE entity_id = ?3 AND field_name = ?4 AND source_revision < ?2`

### 8.4 Qdrant 集成测试隔离
- 每个测试用独立 collection 名（带随机后缀）
- 测试结束后删除 collection（cleanup）
- 连接失败时 skip 测试而非 panic（CI 环境可能无 qdrant）

### 8.5 错误处理
- 所有 trait 方法返回 `Result<T, crate::Error>`
- 用 `thiserror` 为 Error 枚举实现 `std::error::Error`
- 用 `From<rusqlite::Error>` / `From<std::io::Error>` 自动转换底层错误

---

## 9. 分步验收判据汇总

| 步骤 | 验收判据 | 验收方式 |
|------|---------|---------|
| 1.x Cargo 配置 | `cargo check --workspace` 通过 | 命令行验证 |
| 2.x Module 布局 | 所有 mod.rs 编译通过 | `cargo check` |
| 3.x Types | EntityId 往返转换无损 | 单测 `test_entity_id_roundtrip` |
| 4.x Traits | 所有 trait Send + Sync | 编译时验证 |
| 5.x Schema | migrate() 幂等，WAL 启用 | 单测 `test_migrate_idempotent` |
| 6.x Qdrant | 连接成功，upsert + search 正确 | 集成测试（需 docker） |
| 7.x MockVectorStore | 余弦相似度计算正确 | 单测 `test_mock_vector_store` |
| 8.x 整体 | `cargo build --workspace` 成功 | 命令行验证 |

---

## 10. 风险点与已知限制

1. **Qdrant 版本依赖**：qdrant-client 1.11 可能与最新 qdrant 服务端不兼容，需测试验证。
2. **FTS5 中文分词**：默认 unicode61 tokenizer 对中文黑话效果有限，MVP 阶段先验证英文，中文分词后续用领域包词表补充。
3. **Filters 到 qdrant 过滤条件的映射**：本步未完整实现，需 wiktor-builder 根据领域包规则补充（见 6.3 TODO 注释）。
4. **Generation 对齐机制**：SQLite generation 与 qdrant collection 命名的对齐逻辑在本步只定义了命名规范，完整对齐算法需后续补充（检测 qdrant 滞后 + 触发同步）。
5. **CAS 并发测试**：facts 表的 source_revision CAS 需要并发单测验证（多线程同时写入旧版本，验证只有新版本生效）。

---

## 11. Addendum v1.1（主模型修订，2026-09-20，权威优先于上文冲突处）

> 上文 v1.0 由 planner-architect 产出，整体可用。以下修订为**最终执行约束**，与上文冲突处以本节为准。

### A. 依赖对齐与修剪（修正 1.1/1.2/1.3）

- **qdrant-client 用 `1.19`**（匹配服务端 v1.19.1）：`qdrant-client = { version = "1.19", default-features = false, features = ["gzip"] }`，作为 `wiktor-core` 的 **optional dependency + feature `vector-qdrant`**（默认开）。
- **不用 async-trait**：Rust 1.75+ 支持 trait 内原生 `async fn`，直接写，删除一切 `#[async_trait]` 用法。
- **wiktor-core 实际依赖裁剪**（只列被使用的）：`rusqlite`(bundled)、`serde`(derive)、`serde_json`、`thiserror`、`blake3`、`tokio`(rt-multi-thread/macros/sync)、`uuid`(v4/serde，用于 point id)、可选 `qdrant-client`。**移除**：petgraph / dashmap / moka / tracing / tracing-subscriber / tokio-stream / serde_yaml_ng / chrono / anyhow（这些留给后续步骤，本轮不引；workspace.dependencies 可留声明，crate 不列）。
- **wiktor-cli 依赖**：`wiktor-core`(features=["vector-qdrant"])、`clap`(derive)、`anyhow`、`tokio`。
- `rust-version` 用 `1.85`（本机 1.97 可用；比 spec 的 1.75 放宽保险）。
- 时间戳用 `std::time::SystemTime` 的 unix 秒，**不引 chrono**。

### B. SearchHit 命名冲突（修正 3.3 与 4.8）

- `traits/vector_store.rs` 里的向量层命中改名为 **`VectorHit`**（字段 `id`/`score`/`metadata: VectorMetadata`）。
- `types/query.rs` 的 `SearchHit`（page_id/entity_id/title/snippet/score/score_breakdown）保留，是查询层融合结果；`QueryLog.hits: Vec<SearchHit>` 用查询层版本。
- prelude 只导出查询层 `SearchHit` + 向量层 `VectorHit`，避免同名冲突。

### C. Qdrant point id 与候选过滤修正（修正 6.3）

- **qdrant 字符串 point id 必须是合法 UUID**。`QdrantVectorStore` 提供确定性生成器：
  ```rust
  fn point_id(entity_id: &EntityId, chunk_type: ChunkType, generation: u64) -> String {
      let h = blake3::hash(format!("{}|{:?}|{}", entity_id.to_key(), chunk_type, generation).as_bytes());
      let b = h.as_bytes();
      uuid::Uuid::from_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15]]).to_string()
  }
  ```
  同 (entity, chunk, generation) 幂等覆盖；delete 用同一派生回查。
- **候选 ID 过滤不能用 `Condition::HasId`**（那是按 point id 过滤；候选是 payload 里的 entity_id 字符串）。正确做法：
  ```rust
  // candidate_ids: &[EntityId] → payload.entity_id 关键词匹配 OR 组
  let id_cond = Condition::matches_keyword("entity_id", ids.iter().map(|e| e.to_key()).collect::<Vec<_>>());
  builder = builder.filter(Filter::must([id_cond]));
  ```
  （`Condition::matches_keyword` 的集合语义 = 该字段值命中集合任一值；MVP 规模成立。）

### D. 连接构造支持 API key（修正 6.3 `connect`）

```rust
pub fn from_config(url: &str, api_key: Option<&str>, dimension: usize) -> Result<Self>
```
- 有 api_key 时 `QdrantConfig::from_url(url).with_api_key(key)`。
- `connect(url, dimension)` 保留为 `from_config(url, None, dimension)` 的简写。
- 默认端口约定：url 不含端口用 6334（gRPC）。

### E. 补齐 SqliteKernel 与 EntityStore（新增 5.7/6.4，上文缺失）

`kernel/sqlite.rs`：

```rust
pub struct SqliteKernel { conn: std::sync::Mutex<rusqlite::Connection> }

impl SqliteKernel {
    pub fn open(path: &std::path::Path) -> Result<Self>;   // 不存在则创建
    pub fn open_in_memory() -> Result<Self>;
    pub fn migrate(&self) -> Result<()>;                   // 锁内调 schema::migrations::migrate
    pub fn schema_version(&self) -> Result<i64>;
    pub fn row_counts(&self) -> Result<BTreeMap<String, i64>>; // pages/quality_scores/sections/facts/fact_refs/compile_tasks/query_logs/generation
}
```

`impl EntityStore for SqliteKernel`（async fn 内用 `tokio::task::spawn_blocking` + 锁）：

- `upsert_facts`：**单语句 CAS**（spec 8.3 的 UPDATE-only 写法首次写入会失效，改用）：
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
  reflist 字段拆行写 `fact_refs`（先删该 (entity,field) 旧行再插新行，同一事务）。
- `filter`：递归翻译 `FilterCondition` →
  - `NumericRange{field,min,max}` → `(value_numeric >= min) AND (value_numeric <= max)`（单边省略对应条件）
  - `TextEquals{field,value}` → `value_text = ?`
  - `RefContains{field,refs}` → `EXISTS (SELECT 1 FROM fact_refs r WHERE r.entity_id = facts.entity_id AND r.field_name = ? AND r.ref_value IN (...))`
  - `RefExcludes{field,refs}` → `NOT EXISTS (...同上...)`
  - 多条件 AND 拼接；空条件返回全量；一律 `SELECT DISTINCT entity_id FROM facts WHERE ... LIMIT 10000`。
- `delete_facts`：删 facts + fact_refs（事务）。
- `get_facts`：读回 `Facts`。

### F. 补齐 CLI（新增 9.x，上文只有模块布局）

`crates/wiktor-cli/src/main.rs`（clap derive）：

```
wiktor status [--db PATH]                # 默认 ./wiktor.db；open+migrate，打印 schema 版本 + 各表行数（BTreeMap 逐行）
wiktor vector ping [--url URL] [--api-key KEY]
                                         # 默认 URL=${WIKTOR_QDRANT_URL:-http://127.0.0.1:6334}
                                         # KEY 优先 --api-key，再 $WIKTOR_QDRANT_API_KEY
```

- `status`：`SqliteKernel::open` → `migrate` → 打印 `schema version: N` + `pages: 0` 等行。DB 路径父目录不存在则 `std::fs::create_dir_all`。
- `vector ping`：`QdrantVectorStore::from_config(url, key, 768)` → `health_check()`（`client.health_check()`），成功打印 `qdrant ok: <version>`，失败 `anyhow!` 并退出码 1。
- 顶层 doc comment 简述 Wiktor 定位。

### G. 修正 pages.content_hash（修正 5.2）

- `content_hash TEXT NOT NULL UNIQUE` → **去掉 UNIQUE**（保留 `CREATE INDEX idx_pages_content_hash`）：重编译产物相同内容（罕见但合法）或不同实体相同内容都不该被唯一约束拒绝。

### H. 测试纪律（覆盖 8.4）

- qdrant 集成测试：读 `WIKTOR_QDRANT_URL`（默认 `http://127.0.0.1:6334`）与 `WIKTOR_QDRANT_API_KEY`（可选）；连接失败 → `eprintln!` + **skip**（不 panic）；每个测试用独立 collection 名（`test_{name}_{random}`），`#[tokio::test]` 尾部清理 `drop_collection`。
- Mock 单测无网络依赖，是 CI 主力；qdrant 集成测试标记 `#[ignore]`（`cargo test -- --include-ignored` 时跑本地）。

### I. 验收判据补充（汇总）

1. `cargo build --workspace` 零错误；`cargo test --workspace` 全绿（Mock 路径）。
2. `cargo run -p wiktor-cli -- status --db /tmp/wiktor-t1.db` 输出 `schema version: 1` + 6~8 张表行数（初建 0）。
3. `cargo run -p wiktor-cli -- vector ping` 连 `127.0.0.1:6334` 输出 `qdrant ok`。
4. 集成（本地 qdrant 存在时）：`cargo test -- --include-ignored` 中 vector 集成用例 ensure→upsert→search→delete→drop 全通；候选过滤（candidate_ids）只返回限域内结果。

## 12. 实现修正记录（wiktor-builder / 主模型，2026-09-20）

以下为实现过程中对 spec 的修正，均已在代码落地并验证：

1. **A 修正（async-trait 决策反转）**：Addendum A 曾要求"原生 async fn，不用 async-trait"——**错误**。MASTER-PLAN 第七节要求 `DomainPack::compiler() -> Box<dyn Compiler>` 等 trait 可作 trait object，而原生 async fn 不 object-safe。最终全部 trait 使用 `#[async_trait]`（`wiktor-core` 增加 `async-trait = "0.1"` 依赖）。Addendum A 相关表述作废。
2. **qdrant-client 1.19 API 对齐**：`Qdrant::from_url(url).api_key(key).build()`；`PointStruct::new(id, Vec<f32>, HashMap<String,Value>)`；`UpsertPointsBuilder::new(collection, points)`；`SearchPointsBuilder::new(collection, vector, limit)`；`Condition::matches("entity_id", Vec<String>)` 实现候选集合过滤（qdrant 无 `matches_keyword` 辅助，`matches` 接受 Vec<String> 自动转 Keywords）；payload 用 `value::Kind::StringValue/IntegerValue` 构造。
3. **BUG-1 修复（NumericRange 参数顺序）**：`translate_filters` 的 NumericRange 分支曾按 `[min, max, field_name]` 入栈而 SQL 中 `field_name = ?` 在前，导致任何数值范围过滤恒空。已改为 field_name 先入栈。回归测试：`numeric_range_filter_finds_expected_entity`。
4. **BUG-2 修复（reflist 绕过 CAS）**：`upsert_facts` 中 `fact_refs` 的删插原无条件执行，旧 revision 可覆盖新 revision 的 reflist。已改为仅当 facts 行 CAS 生效（`execute` 返回 changes()==1）时才重写 fact_refs。回归测试：`reflist_write_respects_cas`。
5. **表命名**：`page_quality` / `page_sections` / `generations`（vs spec 草案的 `quality_scores` / `sections` / `generation`）——实现采用"generation 表存历史记录"的 v1.1 架构；`row_counts` 输出 8 张表，满足 §I.2"6~8 张表"。
6. **Mock 与 qdrant delete 语义差异**（已知，非 bug）：Mock 对不存在 collection 静默 Ok，qdrant 报错——查询路径统一封装时按 qdrant 语义处理。

---

**文档版本**: v1.2（含 Addendum v1.1 + 实现修正记录）
**下一步**: wiktor-builder 按 v1.1 实现代码，逐项验收通过后进入 Step 2（20 个手工种子 Wiki + JSONL 数据源 + 查询闭环）。
