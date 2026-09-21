use crate::types::error::Result;
use crate::types::{CompileTask, Query, QueryLog};
use async_trait::async_trait;

/// 反馈分析器（查询日志 → 盲区分析 → 补充编译任务，进人工审核队列）。
/// Feedback analyzer (query logs → blind-spot analysis → supplementary compile tasks, sent to human review).
#[async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    async fn analyze(&self, logs: &[QueryLog]) -> Result<FeedbackReport>;
}

/// 反馈报告（盲区信号 + 建议补充编译任务）。
/// Feedback report (blind-spot signals + suggested supplementary compile tasks).
#[derive(Debug, Clone, Default)]
pub struct FeedbackReport {
    pub zero_recall_queries: Vec<Query>,
    pub low_quality_hits: Vec<String>,
    pub rewrite_failures: Vec<Query>,
    pub suggested_compilations: Vec<CompileTask>,
}
