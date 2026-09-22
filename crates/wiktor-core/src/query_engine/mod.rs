//! 查询引擎编排层：QUG 改写/fallback → 过滤下推候选域 → FTS + 向量双路召回 →
//! RRF 融合 → 查询日志。默认路径零 LLM。
//! Query-engine orchestration: QUG rewrite/fallback → filter-pushdown candidate
//! scope → FTS + vector dual recall → RRF fusion → query log. Zero-LLM by default.

use crate::kernel::qug_store::{load_active_qug, QUG_STALE_PREFIX};
use crate::kernel::SqliteKernel;
use crate::query_engine::hybrid::{rrf_merge, whitelist_filter};
use crate::query_engine::qug::QugGraph;
use crate::traits::{DomainConfig, VectorHit, VectorStore};
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
    /// Step5 批3：active published build 存在，但其 source_hash 与当前来源
    /// （页清单/config/intents）不一致——图不可用，显式走混合 fallback
    /// （spec §2 术语、§4.4；绝不静默用旧图或内存重建图顶替）。
    /// Step5 batch 3: an active published build exists but its source_hash
    /// disagrees with the current source (page list/config/intents) — the graph
    /// is unusable and hybrid retrieval is the explicit fallback (spec §2 terms,
    /// §4.4; never silently substituted by the stale or a rebuilt in-memory
    /// graph).
    Stale,
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
    /// Step5 批3：`qug=None` 时的 stale 标记——`true` 表示 active build 存在但
    /// source_hash 不一致（查询诊断写 `stale`）；`false` 表示无 active build 或
    /// QUG 显式关闭（诊断写 `disabled`）。由 [`QueryEngine::with_persistent_qug`]
    /// / [`QueryEngine::reload_persistent_qug`] 维护；Step3 内存注入路径恒为
    /// `false`。
    /// Step5 batch 3: the stale flag for `qug=None` — `true` means an active
    /// build exists but its source_hash disagrees (query diagnostics report
    /// `stale`); `false` means no active build or QUG explicitly off
    /// (diagnostics report `disabled`). Maintained by
    /// [`QueryEngine::with_persistent_qug`] / [`QueryEngine::reload_persistent_qug`];
    /// the Step3 in-memory injection path keeps it `false`.
    pub qug_stale: bool,
    pub embedder: Arc<dyn QueryEmbedder>,
    pub collection: String,
    pub candidate_multiplier: usize,
    pub rrf_k: u32,
    /// Step5 批5 评测 A 档开关：`true` 时 `search` 跳过整条向量路（嵌入、向量
    /// 检索、payload 过期校验），RRF 退化为 FTS 单路名次——`1/(k+r)` 对名次
    /// 单调，故最终排序等价纯 FTS5 BM25（tie-break page_id，确定稳定）。默认
    /// `false`，不改变既有检索行为；QUG 语义不受本开关影响（A 档以 `qug=None`
    /// 表达）。
    /// Step5 batch-5 tier-A switch: when `true`, `search` skips the whole
    /// vector path (embedding, vector search, payload staleness validation) and
    /// RRF degenerates to the FTS-only ranking — `1/(k+r)` is monotonic in the
    /// rank, so the final order equals pure FTS5 BM25 (deterministic page_id
    /// tie-break). Defaults to `false` and never changes existing retrieval
    /// behavior; QUG semantics are untouched (tier A expresses itself via
    /// `qug=None`).
    pub fts_only: bool,
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
            qug_stale: false,
            embedder,
            collection: collection.into(),
            candidate_multiplier,
            rrf_k,
            fts_only: false,
        })
    }

    /// Step5 批3 构造路径（spec §4.4）：QUG 图优先从持久化 active published
    /// build 加载（`qug.enabled=false` 时保持关闭）；active 缺失/stale/加载失败
    /// 时 `qug=None` 并记录对应状态，查询期写 `disabled`/`stale` 诊断并显式走
    /// 混合 fallback——绝不静默用内存重建图顶替持久化图（A7）。
    /// `intents_bytes` 由调用方在启动/reload 时冻结传入，查询线程不解析 YAML。
    /// The Step5 batch-3 construction path (spec §4.4): the QUG graph is loaded
    /// from the persisted active published build first (kept off when
    /// `qug.enabled=false`); on a missing/stale/failed load, `qug=None` plus the
    /// corresponding recorded state makes queries report `disabled`/`stale` and
    /// fall back explicitly to hybrid retrieval — never silently substituting a
    /// rebuilt in-memory graph for the persisted one (A7). `intents_bytes` are
    /// frozen and passed in at startup/reload; query threads never parse YAML.
    pub fn with_persistent_qug(
        kernel: Arc<SqliteKernel>,
        vector_store: Arc<V>,
        domain: &DomainConfig,
        intents_bytes: &[u8],
        embedder: Arc<dyn QueryEmbedder>,
        collection: impl Into<String>,
        rrf_k: u32,
    ) -> Result<Self> {
        let mut engine = Self::new(
            kernel,
            vector_store,
            None,
            embedder,
            collection,
            // candidate_multiplier 与 max_depth 同源冻结 domain config。
            // candidate_multiplier comes frozen from the same domain config as
            // max_depth.
            domain.qug.candidate_multiplier,
            rrf_k,
        )?;
        engine.reload_persistent_qug(domain, intents_bytes);
        Ok(engine)
    }

    /// Step5 批3 重载路径（spec §4.4："build 成功后的 reload 由调用方显式触发"）：
    /// 重新从持久化 active build 加载图并更新 stale 状态。所有加载结果都被吸收为
    /// 引擎状态（图缺失/stale/损坏 → `qug=None` + 对应诊断态），本方法不返回错误；
    /// eval 等强一致路径必须直接调用 [`load_active_qug`] 并对错误失败（§4.3）。
    /// The Step5 batch-3 reload path (spec §4.4: "the reload after a successful
    /// build is explicitly triggered by the caller"): re-loads the graph from the
    /// persisted active build and refreshes the stale state. Every load outcome is
    /// absorbed into engine state (missing/stale/corrupt → `qug=None` plus the
    /// matching diagnosis state), so this method never fails; strongly-consistent
    /// paths such as eval must call [`load_active_qug`] directly and fail on
    /// errors (§4.3).
    pub fn reload_persistent_qug(&mut self, domain: &DomainConfig, intents_bytes: &[u8]) {
        if !domain.qug.enabled {
            // QUG 关闭：disabled 语义（Step2 兼容）。
            // QUG off: the disabled semantics (Step2 compatibility).
            self.qug = None;
            self.qug_stale = false;
            return;
        }
        match load_active_qug(&self.kernel, domain, intents_bytes) {
            Ok(Some(graph)) => {
                self.qug = Some(graph);
                self.qug_stale = false;
            }
            Ok(None) => {
                // 无 active published build → disabled 诊断（§4.4）。
                // No active published build → the disabled diagnosis (§4.4).
                self.qug = None;
                self.qug_stale = false;
            }
            Err(e) if e.to_string().contains(QUG_STALE_PREFIX) => {
                // hash 不一致 → stale 诊断；绝不加载旧图或内存重建图顶替（A7）。
                // 前缀匹配沿用批2 SOURCE_CHANGED_PREFIX 的 contains 约定（Error
                // Display 会带 "validation error: " 等变体外衣）。
                // Hash mismatch → the stale diagnosis; the stale graph is never
                // loaded nor substituted by an in-memory rebuild (A7). The prefix
                // match follows batch 2's SOURCE_CHANGED_PREFIX contains()
                // convention (the Error Display wraps the message in variants
                // like "validation error: ").
                tracing::warn!(error = %e, "QUG active build is stale; falling back to hybrid retrieval");
                self.qug = None;
                self.qug_stale = true;
            }
            Err(e) => {
                // 载荷损坏/图校验失败等内部错误：普通查询显式 fallback 并记录原因
                // （spec §4.3）；诊断归入 disabled 语义，原因走 tracing 供运维
                // 定位（exit 4 路径由直接调用 load_active_qug 的命令承担）。
                // Internal errors such as corrupt payloads / failed graph
                // validation: ordinary queries fall back explicitly with the
                // reason recorded (spec §4.3); the diagnosis falls into the
                // disabled semantics with the cause on tracing for ops (exit-4
                // paths belong to commands calling load_active_qug directly).
                tracing::error!(error = %e, "QUG persistent load failed; falling back to hybrid retrieval");
                self.qug = None;
                self.qug_stale = false;
            }
        }
    }

    /// 执行查询全链路（Step 3 §5）。
    /// Runs the full query pipeline (Step 3 §5).
    pub async fn search(&self, query: &Query) -> Result<QueryResult> {
        let started = Instant::now();
        self.validate(query)?;

        // QUG 改写（disabled/stale → 显式 fallback；批3：qug=None 且 stale 标记
        // 置位时诊断写 stale，其余 None 写 disabled）
        // QUG rewrite (disabled/stale → explicit fallback; batch 3: qug=None with
        // the stale flag set reports stale, any other None reports disabled)
        let (rewritten, rewrite_failure, status) = match &self.qug {
            Some(graph) => match graph.rewrite(query)? {
                Some(r) => (Some(r), false, RewriteStatus::Applied),
                None => (None, true, RewriteStatus::Fallback),
            },
            None if self.qug_stale => (None, false, RewriteStatus::Stale),
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

        // 向量路径（候选域白名单；Mock 供无 qdrant 环境）。`fts_only`（评测
        // A 档）整路跳过：不嵌入、不检索、不做 payload 过期校验，vector_count
        // 恒 0。
        // Vector path (candidate whitelist; Mock for qdrant-less environments).
        // `fts_only` (evaluation tier A) skips the whole path: no embedding, no
        // search, no payload staleness validation; vector_count stays 0.
        let vector_hits = if self.fts_only {
            Vec::new()
        } else {
            let query_vec = self.embedder.embed(&terms.join(" ")).await?;
            let vector_hits = self
                .vector_store
                .search(&self.collection, &query_vec, cand_k, candidates.as_deref())
                .await?;
            // Step 4 §10 必要边界修复：RRF 融合与截取 top_k 之前，对向量 payload 的
            // accepted/content_hash/generation 做批量校验，丢弃旧代/隔离/不存在页与
            // 缺版本 metadata 的 hit（A21）。
            // Step 4 §10 boundary fix: before RRF fusion and top_k truncation, bulk-
            // validate the vector payload's accepted/content_hash/generation and drop
            // stale/quarantined/missing pages and hits without version metadata (A21).
            self.filter_stale_vector_hits(vector_hits)?
        };
        let usable_vector_count = vector_hits.len();

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
            vector_count: usable_vector_count,
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

    /// 向量 payload 过期防护（Step 4 §10，A21）：缺版本 metadata（空
    /// content_hash 或 generation=0）先丢弃；其余按 `(page_id, content_hash,
    /// generation)` 经 kernel 批量校验。kernel 语义为「页有效 ⇔ 传入的该页全部
    /// payload 与 accepted head 一致」，因此同页混入旧代 chunk 时整页向量被拒，
    /// 旧代 hit 无法借同页有效 payload 冒充。
    /// Vector-payload staleness guard (Step 4 §10, A21): hits without version
    /// metadata (empty content_hash or generation=0) are dropped first; the rest
    /// are bulk-validated by the kernel over `(page_id, content_hash,
    /// generation)`. The kernel's semantics are "a page is valid iff every
    /// provided payload matches its accepted head", so a page mixing stale
    /// chunks is rejected as a whole and a stale hit can never ride on a
    /// sibling's valid payload.
    fn filter_stale_vector_hits(&self, hits: Vec<VectorHit>) -> Result<Vec<VectorHit>> {
        if hits.is_empty() {
            return Ok(hits);
        }
        let triples: Vec<(String, String, u64)> = hits
            .iter()
            .filter(|h| !h.metadata.content_hash.is_empty() && h.metadata.generation > 0)
            .map(|h| {
                (
                    h.metadata.page_id.clone(),
                    h.metadata.content_hash.clone(),
                    h.metadata.generation,
                )
            })
            .collect();
        if triples.is_empty() {
            tracing::debug!(
                dropped = hits.len(),
                "dropped vector hits without version metadata"
            );
            return Ok(Vec::new());
        }
        let valid = self.kernel.validate_vector_payloads(&triples)?;
        let before = hits.len();
        let kept: Vec<VectorHit> = hits
            .into_iter()
            .filter(|h| {
                !h.metadata.content_hash.is_empty()
                    && h.metadata.generation > 0
                    && valid.contains(&h.metadata.page_id)
            })
            .collect();
        if kept.len() != before {
            tracing::debug!(
                dropped = before - kept.len(),
                kept = kept.len(),
                "dropped stale vector hits before RRF fusion"
            );
        }
        Ok(kept)
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
