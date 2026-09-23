use crate::compile::contract::CompileEvidence;
use crate::types::entity::EntityId;
use crate::types::qug::QugEdge;
use serde::{Deserialize, Serialize};

/// 编译产物（知识平面 Wiki 页面 + 评分 + QUG 边，全依赖内容哈希已算）。
/// Compilation artifact (knowledge-plane Wiki page + score + QUG edges, with all dependency content hashes computed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledPage {
    pub wiki: WikiPage,
    pub quality: QualityScore,
    pub qug_edges: Vec<QugEdge>,
    /// BLAKE3 哈希：覆盖源数据 + 领域包版本 + Prompt + 编译器 + 模型版本。
    /// BLAKE3 hash covering source data + domain-pack version + Prompt + compiler + model version.
    pub content_hash: String,
    /// 证据载荷（Step 4 §4：避免丢失 refs/usage；旧 seed 页反序列化为 None，
    /// 但经 PipelineExecutor 发布时 None 视为 schema 失败）。
    /// Evidence payload (Step 4 §4: avoids losing refs/usage; legacy seed pages
    /// deserialize as None, but publishing via PipelineExecutor treats None as a
    /// schema failure).
    #[serde(default)]
    pub evidence: Option<CompileEvidence>,
}

/// Wiki 页面（纯 Markdown，人类可读，可重建）。
/// Wiki page (plain Markdown, human-readable and rebuildable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub page_id: String,
    pub entity_id: EntityId,
    pub title: String,
    /// Markdown 正文。
    /// Markdown body.
    pub content: String,
    /// 章节（章节级向量索引用）。
    /// Sections (for section-level vector indexing).
    pub sections: Vec<Section>,
    pub metadata: PageMetadata,
    /// 同义词别名（frontmatter `aliases`；Step 3 QUG 同义边的来源）。
    /// Synonym aliases (frontmatter `aliases`; source of Step 3 QUG synonym edges).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// 分类标签（frontmatter `tags`；Step 3 QUG 上下位边的来源）。
    /// Category tags (frontmatter `tags`; source of Step 3 QUG hyponym edges).
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub heading: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageMetadata {
    pub domain_pack_version: String,
    /// Unix 时间戳（秒）。
    /// Unix timestamp (seconds).
    pub compiled_at: i64,
    pub model_version: String,
    pub embedding_model: String,
}

/// 质量评分：四规则维度 + 一致性（仲裁报告得分，可空）。
/// Quality score: four rule-based dimensions plus consistency (arbitration
/// score, optional).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityScore {
    pub coverage: f32,
    pub citation: f32,
    pub schema_compliance: f32,
    pub density: f32,
    pub consistency: Option<f32>,
}

impl QualityScore {
    /// 综合得分（Step8 §6.2/D4）：consistency=None 时为四维平均（Step4 语义
    /// 逐字节不变）；`Some(s)` 时为五维等权平均
    /// `(coverage+citation+schema+density+s)/5`。
    /// Overall score (Step8 §6.2/D4): with consistency=None it is the four-dim
    /// average (Step4 semantics byte-identical); with `Some(s)` it is the
    /// five-dim equal-weight average
    /// `(coverage+citation+schema+density+s)/5`.
    pub fn overall(&self) -> f32 {
        match self.consistency {
            None => (self.coverage + self.citation + self.schema_compliance + self.density) / 4.0,
            Some(s) => {
                (self.coverage + self.citation + self.schema_compliance + self.density + s) / 5.0
            }
        }
    }

    /// 是否通过质量阈值。
    /// Whether the quality threshold is met.
    pub fn passes_threshold(&self, threshold: f32) -> bool {
        self.overall() >= threshold
    }
}

/// 发布状态机：candidate → accepted / quarantined。
/// Publish state machine: candidate → accepted / quarantined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishStatus {
    Candidate,
    Accepted,
    Quarantined,
}

impl PublishStatus {
    pub fn is_queryable(&self) -> bool {
        matches!(self, PublishStatus::Accepted)
    }
}

/// 编译上下文（参与内容哈希的依赖集合）。
/// Compilation context (dependency set included in the content hash).
///
/// Step 4 起需序列化：任务快照 `compile_tasks.dependencies_json` 保存 context，
/// claim 时反序列化还原（Step 4 spec §7/§8.3）。
/// Serializable since Step 4: the task snapshot
/// `compile_tasks.dependencies_json` stores the context, deserialized back on
/// claim (Step 4 spec §7/§8.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileContext {
    pub domain_pack_version: String,
    pub prompt_template: String,
    pub model_version: String,
    pub embedding_model: String,
    pub quality_threshold: f32,
    pub require_source_refs: bool,
    // Step 8 §5.1/§6.2：schema/prompt 版本身份进入 content_hash（载体偏差记录
    // 为 STEP8-011），并随 dependencies_json 快照持久化供 B5 兼容 preflight
    // 读取历史版本；`#[serde(default)]` 保证 Step4/6 旧行可读（缺省 None）。
    // Step 8 §5.1/§6.2: the schema/prompt version identity enters the
    // content_hash (carrier deviation recorded as STEP8-011) and is persisted
    // with the dependencies_json snapshot so B5's compatibility preflight can
    // read historical versions; `#[serde(default)]` keeps Step4/6 rows readable
    // (defaulting to None).
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub prompt_version: Option<String>,
}
