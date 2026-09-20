use crate::types::entity::EntityId;
use crate::types::qug::QugEdge;
use serde::{Deserialize, Serialize};

/// 编译产物（知识平面 Wiki 页面 + 评分 + QUG 边，全依赖内容哈希已算）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledPage {
    pub wiki: WikiPage,
    pub quality: QualityScore,
    pub qug_edges: Vec<QugEdge>,
    /// BLAKE3 哈希：覆盖源数据 + 领域包版本 + Prompt + 编译器 + 模型版本。
    pub content_hash: String,
}

/// Wiki 页面（纯 Markdown，人类可读，可重建）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub page_id: String,
    pub entity_id: EntityId,
    pub title: String,
    /// Markdown 正文。
    pub content: String,
    /// 章节（章节级向量索引用）。
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
    pub domain_pack_version: String,
    /// Unix 时间戳（秒）。
    pub compiled_at: i64,
    pub model_version: String,
    pub embedding_model: String,
}

/// 质量评分：四规则维度 + 一致性（LLM 仲裁，可空）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityScore {
    pub coverage: f32,
    pub citation: f32,
    pub schema_compliance: f32,
    pub density: f32,
    pub consistency: Option<f32>,
}

impl QualityScore {
    /// 综合得分 = 四规则维度平均，一致性单独处理。
    pub fn overall(&self) -> f32 {
        (self.coverage + self.citation + self.schema_compliance + self.density) / 4.0
    }

    /// 是否通过质量阈值。
    pub fn passes_threshold(&self, threshold: f32) -> bool {
        self.overall() >= threshold
    }
}

/// 发布状态机：candidate → accepted / quarantined。
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
#[derive(Debug, Clone)]
pub struct CompileContext {
    pub domain_pack_version: String,
    pub prompt_template: String,
    pub model_version: String,
    pub embedding_model: String,
    pub quality_threshold: f32,
    pub require_source_refs: bool,
}
