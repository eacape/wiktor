//! 查询引擎编排层：QUG 改写/fallback → 过滤下推候选域 → FTS + 向量双路召回 →
//! RRF 融合 → 查询日志。默认路径零 LLM。
//! Query-engine orchestration: QUG rewrite/fallback → filter-pushdown candidate
//! scope → FTS + vector dual recall → RRF fusion → query log. Zero-LLM by default.

use crate::kernel::SqliteKernel;
use crate::query_engine::hybrid::{rrf_merge, whitelist_filter};
use crate::query_engine::qug::QugGraph;
use crate::traits::VectorStore;
use crate::types::error::{Error, Result};
use crate::types::{Filters, Query, RewrittenQuery, SearchHit};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

pub mod hybrid;
pub mod qug;

/// QUG 改写状态（CLI/诊断展示）。
/// QUG rewrite status (CLI/diagnostics display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteStatus {
    /// 已启用且成功改写。
    /// Enabled and a rewrite was applied.
    Applied,
    /// 已启用但无匹配路径（显式 fallback）。
    /// Enabled but no matching path (explicit fallback).
    Fallback,
    /// QUG 被领域配置关闭（或增益不足降级）。
    /// QUG disabled by domain config (or degraded for insufficient gain).
    Disabled,
}

/// 查询诊断信息（CLI 展示 + 评测报告）。
/// Query diagnostics (CLI display + evaluation report).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryDiagnostics {
    pub rewrite_status: RewriteStatus,
    /// 最终生效的过滤条件（QUG + 用户 AND 合并）。
    /// Effective filter conditions (QUG + user ANDed together).
    pub applied_filters: Filters,
    /// 事实平面预筛候选实体数；无过滤时表示未限制。
    /// Candidate entities after fact-plane prefiltering; means "unrestricted" when no filter applies.
    pub candidate_count: usize,
    pub fts_count: usize,
    pub vector_count: usize,
    pub rrf_k: u32,
}

/// 查询结果（hits + 改写 + 诊断 + 延迟）。
/// Query result (hits + rewrite + diagnostics + latency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub hits: Vec<SearchHit>,
    pub rewritten: Option<RewrittenQuery>,
    pub rewrite_failure: bool,
    pub diagnostics: QueryDiagnostics,
    pub latency_ms: u64,
}

/// 查询嵌入器（查询文本 → 向量；Mock/CLI 用确定性实现，qdrant 路径由调用方注入）。
/// Query embedder (query text → vector; deterministic impls for Mock/CLI, the qdrant
/// path injects a real embedder from the caller).
#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
}

/// 查询引擎：编排 QUG、候选域、FTS、向量、RRF 与日志。
/// Query engine: orchestrates QUG, candidate scope, FTS, vectors, RRF and logging.
pub struct QueryEngine<V: VectorStore> {
    pub kernel: Arc<SqliteKernel>,
    pub vector_store: Arc<V>,
    pub qug: Option<Arc<QugGraph>>,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub collection: String,
    pub candidate_multiplier: usize,
    pub rrf_k: u32,
}

impl<V: VectorStore> QueryEngine<V> {
    /// 构造并校验参数（candidate_multiplier 1..=20）。
    /// Constructs the engine and validates parameters (candidate_multiplier 1..=20).
    pub fn new(
        kernel: Arc<SqliteKernel>,
        vector_store: Arc<V>,
        qug: Option<Arc<QugGraph>>,
        embedder: Arc<dyn QueryEmbedder>,
        collection: impl Into<String>,
        candidate_multiplier: usize,
        rrf_k: u32,
    ) -> Result<Self> {
        if !(1..=20).contains(&candidate_multiplier) {
            return Err(Error::Validation(format!(
                "candidate_multiplier {candidate_multiplier} out of range 1..=20"
            )));
        }
        Ok(Self {
            kernel,
            vector_store,
            qug,
            embedder,
            collection: collection.into(),
            candidate_multiplier,
            rrf_k,
        })
    }

    /// 执行查询全链路（Step 3 §5）。
    /// Runs the full query pipeline (Step 3 §5).
    pub async fn search(&self, query: &Query) -> Result<QueryResult> {
        let started = Instant::now();
        self.validate(query)?;

        // QUG 改写（disabled → 跳过；None → 显式 fallback）
        // QUG rewrite (disabled → skip; None → explicit fallback)
        let (rewritten, rewrite_failure, status) = match &self.qug {
            Some(graph) => match graph.rewrite(query)? {
                Some(r) => (Some(r), false, RewriteStatus::Applied),
                None => (None, true, RewriteStatus::Fallback),
            },
            None => (None, false, RewriteStatus::Disabled),
        };

        // 用户过滤与 QUG 过滤合并（QUG 已在 rewrite 内与用户条件做冲突合并）
        // Merge user filters with QUG filters (rewrite already merged them with
        // conflict resolution)
        let applied: Filters = rewritten
            .as_ref()
            .map(|r| r.filters.clone())
            .unwrap_or_else(|| query.filters.clone());

        // 过滤下推候选域：SKU 满足条件 → category 值集合（= 知识页 entity_id）
        // Filter-pushdown candidate scope: SKUs matching conditions → category
        // values (= knowledge-page entity_id)
        let candidates = if applied.is_empty() {
            None
        } else {
            Some(self.kernel.filter_page_candidates(&applied)?)
        };
        let candidate_count = candidates.as_ref().map_or(0, Vec::len);
        if let Some(ids) = &candidates {
            if ids.is_empty() {
                // 空候选域：零命中，仍写日志
                // Empty scope: zero hits; still write the log
                let latency_ms = started.elapsed().as_millis() as u64;
                let res = QueryResult {
                    hits: Vec::new(),
                    rewritten: rewritten.clone(),
                    rewrite_failure,
                    diagnostics: QueryDiagnostics {
                        rewrite_status: status,
                        applied_filters: applied.clone(),
                        candidate_count,
                        fts_count: 0,
                        vector_count: 0,
                        rrf_k: self.rrf_k,
                    },
                    latency_ms,
                };
                self.log_query(
                    query,
                    rewritten.as_ref(),
                    rewrite_failure,
                    &res.hits,
                    latency_ms,
                )?;
                return Ok(res);
            }
        }

        // 检索词：改写展开词（含原文），否则原文
        // Search terms: rewritten expansions (including the original), else the raw text
        let terms: Vec<String> = rewritten
            .as_ref()
            .map(|r| r.expanded_terms.clone())
            .unwrap_or_else(|| vec![query.text.clone()]);

        // 过采样候选数
        // Oversampled candidate count
        let cand_k = (query.top_k * self.candidate_multiplier).clamp(50, 500);

        // FTS 路径
        // FTS path
        let fts_hits = self.kernel.search_candidates(
            &terms,
            &applied,
            cand_k,
            query.domain.as_deref(),
            candidates.as_deref(),
        )?;

        // 向量路径（候选域白名单；Mock 供无 qdrant 环境）
        // Vector path (candidate whitelist; Mock for qdrant-less environments)
        let query_vec = self.embedder.embed(&terms.join(" ")).await?;
        let vector_hits = self
            .vector_store
            .search(&self.collection, &query_vec, cand_k, candidates.as_deref())
            .await?;

        // RRF 融合 + 候选域第二道保护
        // RRF fusion + second-line candidate whitelist
        let fused = rrf_merge(&fts_hits, &vector_hits, query.top_k, self.rrf_k);
        let hits = whitelist_filter(fused, candidates.as_deref())?;

        let latency_ms = started.elapsed().as_millis() as u64;
        let diagnostics = QueryDiagnostics {
            rewrite_status: status,
            applied_filters: applied,
            candidate_count,
            fts_count: fts_hits.len(),
            vector_count: vector_hits.len(),
            rrf_k: self.rrf_k,
        };
        let res = QueryResult {
            hits: hits.clone(),
            rewritten: rewritten.clone(),
            rewrite_failure,
            diagnostics,
            latency_ms,
        };
        self.log_query(
            query,
            rewritten.as_ref(),
            rewrite_failure,
            &hits,
            latency_ms,
        )?;
        Ok(res)
    }

    /// 输入校验（Step 3 A16：top_k 1..=100、文本 ≤4096）。
    /// Input validation (Step 3 A16: top_k 1..=100, text ≤4096).
    fn validate(&self, query: &Query) -> Result<()> {
        if query.text.chars().count() > 4096 {
            return Err(Error::Validation("query text exceeds 4096 chars".into()));
        }
        if query.top_k == 0 || query.top_k > 100 {
            return Err(Error::Validation(format!(
                "top_k {} out of range 1..=100",
                query.top_k
            )));
        }
        Ok(())
    }

    /// 写查询日志（成功与可恢复空结果路径都要写；写失败仅 tracing 告警）。
    /// Writes a query log entry (on success and recoverable empty-result paths;
    /// log failures only warn via tracing).
    fn log_query(
        &self,
        query: &Query,
        rewritten: Option<&RewrittenQuery>,
        rewrite_failure: bool,
        hits: &[SearchHit],
        latency_ms: u64,
    ) -> Result<()> {
        let query_json = serde_json::to_string(query)?;
        let rewritten_json = rewritten.map(serde_json::to_string).transpose()?;
        let sql = format!(
            "INSERT INTO query_logs (query_text, query_json, rewritten_json, rewrite_failure, hit_count, latency_ms, timestamp)
             VALUES ({},{},{},{},{},{},{})",
            sq(query.text.as_str()),
            sq(&query_json),
            rewritten_json.as_deref().map(sq).unwrap_or_else(|| "NULL".into()),
            if rewrite_failure { "1" } else { "0" },
            hits.len(),
            latency_ms,
            now_secs(),
        );
        if let Err(e) = self.kernel.execute_batch(&sql) {
            tracing::warn!("query log write failed: {e}");
        }
        Ok(())
    }
}

/// SQL 字符串字面量（防注入；本模块仅用于日志写入的参数内联）。
/// SQL string literal (injection-safe; used here only to inline log parameters).
fn sq(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// 当前 Unix 秒。
/// Current Unix seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
