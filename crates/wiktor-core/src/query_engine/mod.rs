//! 查询引擎编排层：QUG 改写/fallback → 过滤下推候选域 → FTS + 向量双路召回 →
//! RRF 融合 → 查询日志。默认路径零 LLM。
//! Query-engine orchestration: QUG rewrite/fallback → filter-pushdown candidate
//! scope → FTS + vector dual recall → RRF fusion → query log. Zero-LLM by default.

use crate::kernel::qug_store::{load_active_qug, QUG_STALE_PREFIX};
use crate::kernel::{QueryLogInsert, SqliteKernel, DEFAULT_QUERY_LOG_DOMAIN};
use crate::query_engine::hybrid::{rrf_merge, whitelist_filter};
use crate::query_engine::qug::QugGraph;
use crate::traits::{DomainConfig, VectorHit, VectorStore};
use crate::types::error::{Error, Result};
use crate::types::{FilterCondition, Filters, Query, RewrittenQuery, SearchHit};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

pub mod cache;
pub mod hybrid;
pub mod qug;
pub use cache::{CachedSearch, QueryCache};

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
    /// 最终生效的过滤条件（QUG + 用户 AND 合并；放宽重试成功后为放宽后条件）。
    /// Effective filter conditions (QUG + user ANDed together; the relaxed ones
    /// after a successful relaxation retry).
    pub applied_filters: Filters,
    /// 事实平面预筛候选实体数；无过滤时表示未限制。
    /// Candidate entities after fact-plane prefiltering; means "unrestricted" when no filter applies.
    pub candidate_count: usize,
    pub fts_count: usize,
    pub vector_count: usize,
    pub rrf_k: u32,
    /// Step 6 D10：滤空放宽重试是否已尝试（初始候选为空 + 带过滤 + 已装
    /// relaxer；至多一次）。`#[serde(default)]` 保持旧诊断 JSON 可反序列化。
    /// Step 6 D10: whether a relaxation retry was attempted (initial empty
    /// scope + filters present + a relaxer installed; at most once).
    /// `#[serde(default)]` keeps old diagnostics JSON deserializable.
    #[serde(default)]
    pub relaxation_attempted: bool,
    /// Step 6 D10：放宽重试是否成功打开候选域（未尝试时恒 false）。
    /// Step 6 D10: whether the relaxation retry reopened the candidate scope
    /// (always false when no attempt was made).
    #[serde(default)]
    pub relaxation_succeeded: bool,
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
    /// Step 6 批2：本次查询写入的 `query_logs` 行 ID（反馈事件经
    /// `insert_feedback_idempotent` 引用它）。日志写失败会让查询返回
    /// `Err`（spec step6 §6 硬要求，不得静默吞掉），故成功路径恒为
    /// `Some`；`#[serde(default)]` 保持旧结果 JSON 可反序列化。
    /// Step 6 batch 2: the `query_logs` row id written for this query
    /// (referenced by feedback events via `insert_feedback_idempotent`).
    /// A log-write failure makes the query return `Err` (hard requirement of
    /// spec step6 §6, never swallowed silently), so a successful search always
    /// carries `Some`; `#[serde(default)]` keeps old result JSON deserializable.
    #[serde(default)]
    pub log_id: Option<i64>,
}

/// Step 6 D10：过滤放宽器——滤空（带过滤下推后候选为空）时由 QueryEngine
/// 调用，同一次查询**至多一次**。
/// Step 6 D10: the filter relaxer — invoked by the QueryEngine on a
/// filtered-empty (empty candidate scope after filter pushdown), **at most
/// once** per query.
///
/// 契约（spec step6 §6）：输入当前生效过滤条件；`Ok(Some(..))` 返回放宽后的
/// 条件，`Ok(None)` 表示无放宽语义（不重试）。实现必须确定性、无副作用；
/// `Err` 作为查询错误向上传播，不得转为空结果。
/// Contract (spec step6 §6): takes the currently effective filters;
/// `Ok(Some(..))` returns the relaxed filters and `Ok(None)` means "no
/// relaxation semantics" (no retry). Implementations must be deterministic and
/// side-effect free; `Err` propagates as a query error and must never be
/// converted into an empty result.
pub trait FilterRelaxer: Send + Sync {
    fn relax_once(&self, filters: &Filters) -> Result<Option<Filters>>;
}

/// 默认放宽器（上层拍板语义，记入 spec §12 偏差）：
/// - `NumericRange{min,max}`：去掉较紧一侧——两侧都在时**先去 max**；仅一侧时
///   去掉该侧。去掉后区间变为无约束（两侧皆 None）时，该条件不再约束任何
///   实体（与 `schema::facts::filter_where` 的开放区间规则一致），整条条件从
///   列表移除；列表因此变空即等价无过滤（引擎按不限候选域处理）。
/// - `TextEquals`：不放宽（枚举值无"更宽"语义），返回 `None`。
/// - `RefContains`：不放宽（允许集语义敏感），返回 `None`。
/// - `RefExcludes`：不放宽（排除集语义敏感，MVP 不动），返回 `None`。
/// - 多条件：按条件顺序放宽**第一条**可放宽条件；全部不可放宽返回 `None`。
///
/// The default relaxer (upstream-decided semantics, recorded as a spec §12
/// deviation):
/// - `NumericRange{min,max}`: drop the tighter side — with both sides present
///   drop **max first**; with a single side drop that side. Once the range
///   becomes unconstrained (both None) the condition binds nothing (matching
///   `schema::facts::filter_where`'s open-range rule) and is removed from the
///   list; an emptied list is equivalent to no filter (the engine treats it as
///   an unrestricted candidate scope).
/// - `TextEquals`: not relaxed (enum values have no "wider" ordering) → `None`.
/// - `RefContains`: not relaxed (allow-set semantics are sensitive) → `None`.
/// - `RefExcludes`: not relaxed (exclude-set semantics are sensitive; untouched
///   in the MVP) → `None`.
/// - Multiple conditions: relax the **first** relaxable condition in order;
///   return `None` when none can be relaxed.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultFilterRelaxer;

impl DefaultFilterRelaxer {
    pub fn new() -> Self {
        Self
    }
}

impl FilterRelaxer for DefaultFilterRelaxer {
    fn relax_once(&self, filters: &Filters) -> Result<Option<Filters>> {
        for (idx, cond) in filters.conditions.iter().enumerate() {
            // 逐条件判定放宽结果：外层 `None` = 本条件无放宽语义（继续下一条）；
            // 内层 `Some(Some(c))` = 放宽为新条件；内层 `Some(None)` = 放宽后
            // 无约束 → 移除整条条件。
            // Per-condition relaxation outcome: the outer `None` = this condition
            // has no relaxation semantics (move on); the inner `Some(Some(c))` =
            // relaxed into a new condition; the inner `Some(None)` = unconstrained
            // after relaxation → remove the whole condition.
            let outcome: Option<Option<FilterCondition>> = match cond {
                FilterCondition::NumericRange { field, min, max } => match (min, max) {
                    // 两侧都在：先去 max（拍板；保留 min 下界约束）。
                    // Both sides present: drop max first (the upstream decision;
                    // the min bound stays).
                    (_, Some(_)) => Some(Some(FilterCondition::NumericRange {
                        field: field.clone(),
                        min: *min,
                        max: None,
                    })),
                    // 仅 min：去 min 后区间无约束 → 整条条件移除。
                    // min only: dropping min leaves an unconstrained range →
                    // remove the whole condition.
                    (Some(_), None) => Some(None),
                    // 双侧皆空：本就无约束，无可放宽。
                    // Both absent: already unconstrained, nothing to relax.
                    (None, None) => None,
                },
                // TextEquals / RefContains / RefExcludes：无放宽语义（拍板）。
                // TextEquals / RefContains / RefExcludes: no relaxation semantics
                // (the upstream decision).
                _ => None,
            };
            if let Some(new_cond) = outcome {
                let mut conditions = filters.conditions.clone();
                match new_cond {
                    Some(c) => conditions[idx] = c,
                    // 放宽为无约束 → 移除条件（可能使 Filters 变空）。
                    // Relaxed into unconstrained → remove the condition (this may
                    // empty the Filters).
                    None => {
                        conditions.remove(idx);
                    }
                }
                return Ok(Some(Filters { conditions }));
            }
        }
        Ok(None)
    }
}

/// 滤空/放宽三状态（D10；与 `query_logs` 的三列一一对应，随同一次日志写入
/// 落库，绝不拆成多次写）。
/// Filter-empty/relaxation state (D10; maps 1:1 to the three `query_logs`
/// columns and lands within the same single log write, never split).
#[derive(Debug, Clone, Copy, Default)]
struct RelaxState {
    candidate_empty_initial: bool,
    relaxation_attempted: bool,
    relaxation_succeeded: bool,
}

/// 查询嵌入器（查询文本 → 向量；Mock/CLI 用确定性实现，qdrant 路径由调用方注入）。
/// Query embedder (query text → vector; deterministic impls for Mock/CLI, the qdrant
/// path injects a real embedder from the caller).
#[async_trait]
pub trait QueryEmbedder: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// 批量嵌入（P1）：默认逐条调 [`embed`]；`HttpEmbedder` 覆盖为真实批量请求
    /// （大语料回填显著减少 RTT 与请求数）。顺序与输入一致。
    /// Batch embed (P1): the default embeds each item individually via [`embed`];
    /// `HttpEmbedder` overrides it with a real batched request (far fewer RTTs and
    /// requests on large-corpus backfills). Order matches the input.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// 查询引擎：编排 QUG、候选域、FTS、向量、RRF 与日志。
/// Query engine: orchestrates QUG, candidate scope, FTS, vectors, RRF and logging.
pub struct QueryEngine<V: VectorStore + ?Sized> {
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
    /// Step 6 D10：可选过滤放宽器。默认 `None`——滤空不触发放宽重试（A9：
    /// 无 relaxer 触发不了）；是否装配由构造方决定（CLI search 注入
    /// [`DefaultFilterRelaxer`]，引擎绝不自动装配）。
    /// Step 6 D10: the optional filter relaxer. Defaults to `None` — a
    /// filtered-empty never triggers a relaxation retry (A9: without a relaxer
    /// nothing can trigger); installation is the constructor's decision (CLI
    /// search injects [`DefaultFilterRelaxer`]; the engine never auto-installs).
    pub filter_relaxer: Option<Arc<dyn FilterRelaxer>>,
    /// P1 查询热点缓存：`Some` 时命中集合（不含 log_id/latency）跨查询复用；
    /// `None` = 缓存关闭（默认，保持既有行为）。generation 现读键，编译发布
    /// 自动失效；实体失效经 [`QueryCache::invalidate_entity_ids`]。
    /// P1 query hot-cache: when `Some`, the hit set (excluding log_id/latency) is
    /// reused across queries; `None` = caching off (the default, preserving
    /// existing behavior). The key reads generation live, so compile publishes
    /// auto-invalidate; entity invalidation goes through
    /// [`QueryCache::invalidate_entity_ids`].
    pub cache: Option<QueryCache>,
}

impl<V: VectorStore + ?Sized> QueryEngine<V> {
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
            filter_relaxer: None,
            cache: None,
        })
    }

    /// builder 风格：安装查询热点缓存（P1；默认关闭，需显式开启）。
    /// Builder style: installs the query hot-cache (P1; off by default, opt-in).
    pub fn with_cache(mut self, cache: QueryCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Step 6 批2：安装过滤放宽器（builder 风格，命名对齐
    /// `with_persistent_qug`；不安装则滤空不重试）。「至多一次」的次数约束由
    /// 引擎保证，relaxer 实现只需确定性、无副作用。
    /// Step 6 batch 2: installs a filter relaxer (builder style, named after
    /// `with_persistent_qug`; without one a filtered-empty never retries). The
    /// at-most-once bound is enforced by the engine; relaxer implementations
    /// only need to be deterministic and side-effect free.
    pub fn with_filter_relaxer(mut self, relaxer: Arc<dyn FilterRelaxer>) -> Self {
        self.filter_relaxer = Some(relaxer);
        self
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

        // —— P1 缓存：命中则复用命中集合（不含 log_id/latency），但仍写本次
        // query_logs 取新 log_id（反馈闭环依赖真实 log_id）。domain/generation
        // 现读使缓存键随编译发布递增而自然失效；实体失效经
        // `invalidate_entity_ids`。无 cache → 原路径。
        // —— P1 cache: on a hit reuse the hit set (minus log_id/latency) but still
        // write this query's query_logs for the new log_id (feedback depends on
        // the real id). Reading domain/generation live makes the key naturally
        // stale when a compile publish bumps generation; entity invalidation goes
        // through `invalidate_entity_ids`. No cache → the original path.
        let cache = self.cache.clone();
        let domain_key = query.domain.as_deref().unwrap_or(&self.collection);
        let cache_key = if cache.is_some() {
            let gen = self.kernel.current_published_generation(domain_key)?;
            Some(QueryCache::key(
                &query.text,
                &query.filters,
                query.top_k,
                domain_key,
                gen,
            ))
        } else {
            None
        };
        if let Some(key) = &cache_key {
            if let Some(cached) = cache.as_ref().unwrap().get(key).await {
                let latency_ms = started.elapsed().as_millis() as u64;
                let relax = RelaxState {
                    candidate_empty_initial: cached.candidate_empty_initial,
                    relaxation_attempted: cached.diagnostics.relaxation_attempted,
                    relaxation_succeeded: cached.diagnostics.relaxation_succeeded,
                };
                let log_id = self.log_query(
                    query,
                    cached.rewritten.as_ref(),
                    cached.rewrite_failure,
                    &cached.hits,
                    latency_ms,
                    &relax,
                )?;
                return Ok(QueryResult {
                    hits: cached.hits.clone(),
                    rewritten: cached.rewritten.clone(),
                    rewrite_failure: cached.rewrite_failure,
                    diagnostics: cached.diagnostics.clone(),
                    log_id: Some(log_id),
                    latency_ms,
                });
            }
        }

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
        let mut applied: Filters = rewritten
            .as_ref()
            .map(|r| r.filters.clone())
            .unwrap_or_else(|| query.filters.clone());

        // 过滤下推候选域：SKU 满足条件 → category 值集合（= 知识页 entity_id）
        // Filter-pushdown candidate scope: SKUs matching conditions → category
        // values (= knowledge-page entity_id)
        let mut candidates = if applied.is_empty() {
            None
        } else {
            Some(self.kernel.filter_page_candidates(&applied)?)
        };

        // Step 6 D10：初始候选为空（必然带过滤）→ 注入的 FilterRelaxer 至多
        // 放宽一次并重新下推重试。无过滤不触发（candidates 恒 None，A9）；
        // 未装 relaxer 触发不了（attempted 恒 0，A9）；放宽后仍空 → 三状态
        // `candidate_empty_initial=1 AND relaxation_succeeded=0` 显式落库，
        // 供批3 分析器区分「滤空」与「真正零召回」（A10/D11）。
        // Step 6 D10: when the initial candidate scope is empty (which implies
        // filters are present) → the injected FilterRelaxer relaxes at most once
        // and the pushdown retries. No filters → never triggered (candidates
        // stay None, A9); no relaxer installed → cannot trigger (attempted stays
        // 0, A9); still empty after relaxation → the state triple
        // `candidate_empty_initial=1 AND relaxation_succeeded=0` is persisted
        // explicitly so the batch-3 analyzer can tell "filtered empty" from
        // "genuine zero recall" (A10/D11).
        let mut relax = RelaxState::default();
        if candidates.as_ref().is_some_and(|ids| ids.is_empty()) {
            relax.candidate_empty_initial = true;
            if let Some(relaxer) = &self.filter_relaxer {
                relax.relaxation_attempted = true;
                if let Some(relaxed) = relaxer.relax_once(&applied)? {
                    let retry = if relaxed.is_empty() {
                        // 放宽为无过滤 → 不限候选域。
                        // Relaxed into no filter → unrestricted candidate scope.
                        None
                    } else {
                        Some(self.kernel.filter_page_candidates(&relaxed)?)
                    };
                    // 放宽成功 ⇔ 重试后候选域不再为空（无过滤 = 不受限，
                    // 也算打开；此后 FTS 仍可能零命中 → 真正零召回，由分析器
                    // 按 D11 判定）。
                    // Relaxation succeeded ⇔ the retried scope is no longer empty
                    // (no filter = unrestricted, which also counts as reopened;
                    // FTS may still miss → genuine zero recall, judged by the
                    // analyzer per D11).
                    if retry.as_ref().is_none_or(|ids| !ids.is_empty()) {
                        relax.relaxation_succeeded = true;
                        applied = relaxed;
                        candidates = retry;
                    }
                }
            }
        }
        let candidate_count = candidates.as_ref().map_or(0, Vec::len);
        if let Some(ids) = &candidates {
            if ids.is_empty() {
                // 空候选域：零命中，仍写日志（带滤空/放宽三状态；写失败 =
                // 查询 Err，不得静默）。
                // Empty scope: zero hits; still write the log (with the three
                // filter-empty/relaxation flags; a write failure = query Err,
                // never silent).
                let latency_ms = started.elapsed().as_millis() as u64;
                let log_id = self.log_query(
                    query,
                    rewritten.as_ref(),
                    rewrite_failure,
                    &[],
                    latency_ms,
                    &relax,
                )?;
                let diagnostics = QueryDiagnostics {
                    rewrite_status: status,
                    applied_filters: applied,
                    candidate_count,
                    fts_count: 0,
                    vector_count: 0,
                    rrf_k: self.rrf_k,
                    relaxation_attempted: relax.relaxation_attempted,
                    relaxation_succeeded: relax.relaxation_succeeded,
                };
                // P1：零命中也是合法结果，入缓存（供未来同代查询复用）。
                // P1: a zero-hit result is valid too — cache it for reuse by a
                // future same-generation query.
                if let (Some(c), Some(k)) = (cache.as_ref(), &cache_key) {
                    c.insert(
                        *k,
                        CachedSearch {
                            hits: Vec::new(),
                            rewritten: rewritten.clone(),
                            rewrite_failure,
                            diagnostics: diagnostics.clone(),
                            candidate_empty_initial: relax.candidate_empty_initial,
                        },
                    )
                    .await;
                }
                return Ok(QueryResult {
                    hits: Vec::new(),
                    rewritten: rewritten.clone(),
                    rewrite_failure,
                    diagnostics,
                    log_id: Some(log_id),
                    latency_ms,
                });
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
            relaxation_attempted: relax.relaxation_attempted,
            relaxation_succeeded: relax.relaxation_succeeded,
        };
        // 日志先写并取回实际 log_id（写失败 = 查询 Err，不得静默——反馈引用
        // 依赖它），再随结果返回。
        // The log is written first and the actual log_id returned (a write
        // failure = query Err, never silent — feedback references depend on it),
        // then handed back with the result.
        let log_id = self.log_query(
            query,
            rewritten.as_ref(),
            rewrite_failure,
            &hits,
            latency_ms,
            &relax,
        )?;
        // P1：入缓存供未来同代查询复用（实体失效经反向索引）。
        // P1: cache for reuse by a future same-generation query (entity
        // invalidation goes through the reverse index).
        if let (Some(c), Some(k)) = (cache.as_ref(), &cache_key) {
            c.insert(
                *k,
                CachedSearch {
                    hits: hits.clone(),
                    rewritten: rewritten.clone(),
                    rewrite_failure,
                    diagnostics: diagnostics.clone(),
                    candidate_empty_initial: relax.candidate_empty_initial,
                },
            )
            .await;
        }
        Ok(QueryResult {
            hits: hits.clone(),
            rewritten: rewritten.clone(),
            rewrite_failure,
            diagnostics,
            log_id: Some(log_id),
            latency_ms,
        })
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

    /// 写查询日志（成功与可恢复空结果路径都要写）。Step 6 批2：返回实际
    /// `log_id`；写失败作为查询 `Err` 向上传播（spec step6 §6 硬要求——反馈
    /// 引用依赖 log_id，不得静默 warn 吞掉）。
    /// Writes a query log entry (on success and recoverable empty-result
    /// paths). Step 6 batch 2: returns the actual `log_id`; write failures
    /// propagate as the query's `Err` (hard requirement of spec step6 §6 —
    /// feedback references depend on the log_id; the old warn-and-swallow is
    /// gone).
    fn log_query(
        &self,
        query: &Query,
        rewritten: Option<&RewrittenQuery>,
        rewrite_failure: bool,
        hits: &[SearchHit],
        latency_ms: u64,
        relax: &RelaxState,
    ) -> Result<i64> {
        let query_json = serde_json::to_string(query)?;
        let rewritten_json = rewritten.map(serde_json::to_string).transpose()?;
        // domain 取 Query 的 domain，缺省 `__default__`（spec step6 §6：不得
        // 再用 legacy 默认值写新行）。
        // The domain comes from Query.domain, defaulting to `__default__`
        // (spec step6 §6: the legacy default must no longer back new rows).
        let domain = query.domain.as_deref().unwrap_or(DEFAULT_QUERY_LOG_DOMAIN);
        self.kernel.insert_query_log(&QueryLogInsert {
            query_text: &query.text,
            query_json: &query_json,
            rewritten_json: rewritten_json.as_deref(),
            rewrite_failure,
            hit_count: hits.len() as i64,
            latency_ms: latency_ms as i64,
            domain,
            candidate_empty_initial: relax.candidate_empty_initial,
            relaxation_attempted: relax.relaxation_attempted,
            relaxation_succeeded: relax.relaxation_succeeded,
        })
    }
}

// SQL 字符串字面量与内联日志写入已随 Step 6 批2 迁往
// `kernel::sqlite::insert_query_log`（参数绑定 + 返回 log_id，query_engine
// 保持不直接写 SQL 的分工）。
// The SQL string literals and the inlined log write moved to
// `kernel::sqlite::insert_query_log` in Step 6 batch 2 (parameter binding plus
// the returned log_id; query_engine keeps its no-direct-SQL role).
