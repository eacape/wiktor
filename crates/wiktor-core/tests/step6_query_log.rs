//! Step 6 批2 集成测试：滤空/放宽重试状态与查询日志 log_id 契约。
//! Step 6 batch-2 integration tests: filter-empty/relaxation-retry state and
//! the query-log log_id contract.
//!
//! 覆盖（spec `step6-feedback-loop.md` §3 D10、§6 FilterRelaxer 契约、§10
//! A9/A10、§11 批2）：
//! - A9：无过滤空候选不触发放宽（三列全 0）；有过滤空候选最多调用一次
//!   `FilterRelaxer`；无 relaxer 触发不了（attempted=0）。
//! - A10：滤空且放宽仍失败的日志以
//!   `candidate_empty_initial=1 AND relaxation_succeeded=0` 落库（不满足
//!   「真正零召回」判据，供批3 分析器区分）；放宽成功后最终有结果。
//! - log_id：`log_query` 返回实际 `log_id` 且可被 `insert_feedback_idempotent`
//!   引用；日志写失败作为查询 `Err` 传播。
//! - domain：来自 Query 的 domain，缺省 `__default__`（不再写 legacy 默认值）。
//!
//! Coverage (spec `step6-feedback-loop.md` §3 D10, §6 FilterRelaxer contract,
//! §10 A9/A10, §11 batch 2):
//! - A9: a filter-less empty result never relaxes (all three columns 0); with
//!   filters, `FilterRelaxer` runs at most once; without a relaxer nothing can
//!   trigger (attempted=0).
//! - A10: a filtered-empty that stays empty after relaxation persists as
//!   `candidate_empty_initial=1 AND relaxation_succeeded=0` (failing the
//!   "genuine zero recall" test, letting the batch-3 analyzer tell them apart);
//!   a successful relaxation ends with results.
//! - log_id: `log_query` returns the actual `log_id` referenceable by
//!   `insert_feedback_idempotent`; a log-write failure propagates as the
//!   query's `Err`.
//! - domain: comes from Query.domain, defaulting to `__default__` (the legacy
//!   default never backs new rows).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use wiktor_core::kernel::{
    FeedbackEventInput, FeedbackKind, MockVectorStore, QueryLogSnapshot, SqliteKernel,
};
use wiktor_core::query_engine::{DefaultFilterRelaxer, FilterRelaxer, QueryEmbedder, QueryEngine};
use wiktor_core::seed;
use wiktor_core::traits::EntityStore;
use wiktor_core::types::{
    EntityId, Error, FactValue, Facts, FilterCondition, Filters, PublishStatus, Query,
};

/// 测试 domain。
/// The test domain.
const DOMAIN: &str = "milk-tea";

/// 永不调用的嵌入器：`fts_only=true` 时向量路整段跳过（不嵌入、不检索），
/// 一旦被调用即失败——兼作「向量路确实没跑」的断言。
/// Never-called embedder: with `fts_only=true` the whole vector path is skipped
/// (no embedding, no search); any call fails — doubling as an assertion that
/// the vector path never ran.
struct UnusedEmbedder;

#[async_trait]
impl QueryEmbedder for UnusedEmbedder {
    async fn embed(&self, _text: &str) -> wiktor_core::Result<Vec<f32>> {
        Err(Error::Internal(
            "vector path must be skipped under fts_only".into(),
        ))
    }
}

/// 计数放宽器：统计 `relax_once` 调用次数（A9「至多一次」断言用），可配置
/// 为委托 [`DefaultFilterRelaxer`] 或恒返回 `None`。
/// Counting relaxer: counts `relax_once` invocations (for the A9 at-most-once
/// assertion); configurable to delegate to [`DefaultFilterRelaxer`] or always
/// return `None`.
struct CountingRelaxer {
    delegate: Option<DefaultFilterRelaxer>,
    calls: AtomicUsize,
}

impl CountingRelaxer {
    /// 委托默认放宽器。
    /// Delegates to the default relaxer.
    fn delegating() -> Self {
        Self {
            delegate: Some(DefaultFilterRelaxer),
            calls: AtomicUsize::new(0),
        }
    }

    /// 恒返回 None（无放宽语义）。
    /// Always returns None (no relaxation semantics).
    fn inert() -> Self {
        Self {
            delegate: None,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl FilterRelaxer for CountingRelaxer {
    fn relax_once(&self, filters: &Filters) -> wiktor_core::Result<Option<Filters>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.delegate {
            Some(d) => d.relax_once(filters),
            None => Ok(None),
        }
    }
}

/// 搭一台最小引擎：1 页知识平面（波霸奶茶）+ 1 个 SKU 事实（category + price
/// 18）；`fts_only` 跳过向量路。可选注入计数放宽器——测试侧保留同一 `Arc` 的
/// 克隆以便读取调用计数（注入后具体类型被擦除为 `dyn FilterRelaxer`）。
/// Builds a minimal engine: one knowledge page (boba milk tea) + one SKU fact
/// (category + price 18); `fts_only` skips the vector path. Optionally injects
/// the counting relaxer — the test keeps a clone of the same `Arc` to read the
/// call count (after injection the concrete type is erased to
/// `dyn FilterRelaxer`).
async fn fixture(
    relaxer: Option<Arc<CountingRelaxer>>,
) -> (Arc<SqliteKernel>, QueryEngine<MockVectorStore>) {
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let page = seed::parse_page(
        "---\npage_id: milk-tea:drink:boba\nentity_id: milk-tea:drink:boba\ntitle: 波霸奶茶\nentity_type: drink\n---\n\n波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。",
    )
    .unwrap();
    kernel
        .seed_pages(&page, DOMAIN, PublishStatus::Accepted)
        .unwrap();
    let mut facts = Facts {
        entity_id: EntityId::new(DOMAIN, "product", "a").unwrap(),
        fields: BTreeMap::new(),
        source_revision: 1,
    };
    facts.fields.insert(
        "category".into(),
        FactValue::Text("milk-tea:drink:boba".into()),
    );
    facts
        .fields
        .insert("price".into(), FactValue::Numeric(18.0));
    kernel
        .upsert_facts(&facts.entity_id, &facts, facts.source_revision)
        .await
        .unwrap();

    let mut engine = QueryEngine::new(
        kernel.clone(),
        Arc::new(MockVectorStore::new()),
        None,
        Arc::new(UnusedEmbedder),
        DOMAIN,
        5,
        60,
    )
    .unwrap();
    engine.fts_only = true;
    if let Some(relaxer) = relaxer {
        engine = engine.with_filter_relaxer(relaxer);
    }
    (kernel, engine)
}

/// 构造查询请求。
/// Builds a query request.
fn q(text: &str, filters: Filters, domain: Option<&str>) -> Query {
    Query {
        text: text.into(),
        filters,
        top_k: 5,
        domain: domain.map(str::to_string),
    }
}

/// 读该 domain 的唯一一条查询日志（多行/缺行都算失败，保证断言指向本测试写入）。
/// Reads the single query log of the domain (multiple/missing rows fail, so the
/// assertion always points at the row this test wrote).
fn read_single_log(kernel: &SqliteKernel, domain: &str) -> QueryLogSnapshot {
    let (logs, events) = kernel.load_feedback_window(domain, 0, i64::MAX).unwrap();
    assert!(events.is_empty(), "no feedback events expected");
    assert_eq!(logs.len(), 1, "expected exactly one query log for {domain}");
    logs.into_iter().next().unwrap()
}

/// price 过滤：min/max 组合下 price=18 的 SKU 可能全部落空。
/// A price filter: depending on min/max the price-18 SKU can be fully excluded.
fn price_range(min: Option<f64>, max: Option<f64>) -> Filters {
    Filters {
        conditions: vec![FilterCondition::NumericRange {
            field: "price".into(),
            min,
            max,
        }],
    }
}

// A9：无过滤空候选（纯 FTS 零命中）不触发放宽——装了计数放宽器也一次都不调，
// 三状态列全 0；domain 缺省写 `__default__`。
// A9: a filter-less empty result (pure FTS zero hits) never relaxes — the
// counting relaxer is installed yet never called, all three state columns stay
// 0; the domain defaults to `__default__`.
#[tokio::test]
async fn no_filter_empty_hits_never_relaxes_and_logs_zero_state() {
    let counting = Arc::new(CountingRelaxer::delegating());
    let (kernel, engine) = fixture(Some(counting.clone())).await;

    // "zzqx"：与种子页无任何 trigram 重叠，FTS 必然零命中。
    // "zzqx": no trigram overlap with the seeded page, so FTS finds nothing.
    let res = engine
        .search(&q("zzqx", Filters::empty(), None))
        .await
        .unwrap();
    assert!(res.hits.is_empty());
    assert!(!res.diagnostics.relaxation_attempted);
    assert!(!res.diagnostics.relaxation_succeeded);

    let log = read_single_log(&kernel, "__default__");
    assert_eq!(log.domain, "__default__");
    assert!(!log.candidate_empty_initial);
    assert!(!log.relaxation_attempted);
    assert!(!log.relaxation_succeeded);
    assert_eq!(log.hit_count, 0);
    // 计数放宽器虽然已装配，但从未被调用（A9 无过滤分支）。
    // The counting relaxer was installed yet never called (the A9 no-filter
    // branch).
    assert_eq!(counting.calls(), 0);
}

// A9 + A10：带过滤滤空 + 默认放宽器 → 恰好尝试一次；两侧区间先去 max 后
// 重试命中，relaxation_succeeded=1 且最终有结果；log_id 可被反馈引用。
// A9 + A10: filtered-empty + the default relaxer → exactly one attempt;
// dropping max from the two-sided range reopens the scope, so
// relaxation_succeeded=1 with final results; the log_id is feedback-referenceable.
#[tokio::test]
async fn filtered_empty_with_relaxer_retries_once_and_succeeds() {
    let counting = Arc::new(CountingRelaxer::delegating());
    let (kernel, engine) = fixture(Some(counting.clone())).await;

    // price ∈ [10,15] 与 price=18 不相交 → 初始候选空；放宽去 max → price ≥ 10
    // 命中 SKU → 候选域重开。
    // price ∈ [10,15] misses price=18 → initial candidates empty; relaxing drops
    // max → price ≥ 10 matches the SKU → the scope reopens.
    let res = engine
        .search(&q(
            "波霸奶茶",
            price_range(Some(10.0), Some(15.0)),
            Some(DOMAIN),
        ))
        .await
        .unwrap();
    assert_eq!(res.hits.len(), 1, "relaxed retry must find the boba page");
    assert!(res.diagnostics.relaxation_attempted);
    assert!(res.diagnostics.relaxation_succeeded);
    // 诊断 JSON 可见（字段名与现有 diagnostics 风格一致）。
    // Visible in the diagnostics JSON (field names match the existing style).
    let diag = serde_json::to_value(&res.diagnostics).unwrap();
    assert_eq!(diag["relaxation_attempted"], serde_json::json!(true));
    assert_eq!(diag["relaxation_succeeded"], serde_json::json!(true));

    let log = read_single_log(&kernel, DOMAIN);
    assert!(log.candidate_empty_initial);
    assert!(log.relaxation_attempted);
    assert!(log.relaxation_succeeded);
    assert_eq!(log.hit_count, 1);
    assert_eq!(counting.calls(), 1, "at most once");

    // log_id 契约：返回的实际 log_id 可被 insert_feedback_idempotent 引用
    // （该调用同时校验行存在与 domain 一致）。
    // The log_id contract: the returned actual log_id is referenceable by
    // insert_feedback_idempotent (which also validates row existence and domain
    // match).
    let log_id = res.log_id.expect("successful search carries its log_id");
    let ingested = kernel
        .insert_feedback_idempotent(
            &FeedbackEventInput {
                idempotency_key: "s6-batch2-1".into(),
                domain: DOMAIN.into(),
                log_id,
                kind: FeedbackKind::Click,
                page_id: Some("milk-tea:drink:boba".into()),
                rating: None,
                metadata: serde_json::json!({}),
            },
            1_000,
        )
        .unwrap();
    assert!(!ingested.replayed);
}

// 拍板语义补充：仅 min 一侧 → 去 min 后区间无约束 → 整条条件移除，Filters
// 变空等价无过滤 → 重试为不限候选域并命中。
// Extra upstream-decision coverage: a min-only range drops min, the range
// becomes unconstrained, the whole condition is removed, and the emptied
// Filters are equivalent to no filter → the retry is unrestricted and hits.
#[tokio::test]
async fn filtered_empty_relax_to_unrestricted_succeeds() {
    let counting = Arc::new(CountingRelaxer::delegating());
    let (kernel, engine) = fixture(Some(counting.clone())).await;

    // price ≥ 100 与 price=18 不相交；去 min 后条件移除 → 无过滤。
    // price ≥ 100 misses price=18; dropping min removes the condition → no
    // filter.
    let res = engine
        .search(&q("波霸奶茶", price_range(Some(100.0), None), Some(DOMAIN)))
        .await
        .unwrap();
    assert_eq!(res.hits.len(), 1);
    assert!(res.diagnostics.relaxation_succeeded);

    let log = read_single_log(&kernel, DOMAIN);
    assert!(log.candidate_empty_initial);
    assert!(log.relaxation_attempted);
    assert!(log.relaxation_succeeded);
    assert_eq!(counting.calls(), 1);
}

// A9：无 relaxer → 触发不了（attempted=0）；带过滤滤空仍如实落库。
// A9: without a relaxer nothing can trigger (attempted=0); the filtered-empty
// state is still persisted truthfully.
#[tokio::test]
async fn filtered_empty_without_relaxer_never_attempts() {
    let (kernel, engine) = fixture(None).await;
    assert!(engine.filter_relaxer.is_none(), "engine default is None");

    let res = engine
        .search(&q(
            "波霸奶茶",
            price_range(Some(10.0), Some(15.0)),
            Some(DOMAIN),
        ))
        .await
        .unwrap();
    assert!(res.hits.is_empty());
    assert!(!res.diagnostics.relaxation_attempted);
    assert!(!res.diagnostics.relaxation_succeeded);

    let log = read_single_log(&kernel, DOMAIN);
    assert!(log.candidate_empty_initial);
    assert!(!log.relaxation_attempted);
    assert!(!log.relaxation_succeeded);
}

// A10：滤空且放宽仍失败 → attempted=1、succeeded=0 落库（candidate_empty_initial=1
// AND relaxation_succeeded=0 组合，不满足「真正零召回」判据，供批3 分析器排除）。
// A10: filtered-empty that stays empty after relaxation → attempted=1 with
// succeeded=0 persisted (the candidate_empty_initial=1 AND
// relaxation_succeeded=0 combination that fails the "genuine zero recall" test
// and is excluded by the batch-3 analyzer).
#[tokio::test]
async fn filtered_empty_relaxer_without_semantics_logs_attempted_not_succeeded() {
    let counting = Arc::new(CountingRelaxer::inert());
    let (kernel, engine) = fixture(Some(counting.clone())).await;

    // TextEquals 无放宽语义（拍板）→ relax_once 返回 None，不重试。
    // TextEquals has no relaxation semantics (the upstream decision) →
    // relax_once returns None, no retry.
    let filters = Filters {
        conditions: vec![FilterCondition::TextEquals {
            field: "size".into(),
            value: "large".into(),
        }],
    };
    let res = engine
        .search(&q("波霸奶茶", filters, Some(DOMAIN)))
        .await
        .unwrap();
    assert!(res.hits.is_empty());
    assert!(res.diagnostics.relaxation_attempted);
    assert!(!res.diagnostics.relaxation_succeeded);

    let log = read_single_log(&kernel, DOMAIN);
    assert!(log.candidate_empty_initial);
    assert!(log.relaxation_attempted);
    assert!(!log.relaxation_succeeded);
    assert_eq!(log.hit_count, 0);
    assert_eq!(counting.calls(), 1, "at most once");

    // 「滤空且放宽仍失败」组合正确落库——批3 零召回判据
    // `hit_count=0 AND NOT(cei=1 AND rs=0)` 据此排除该日志。
    // The "filtered-empty and relaxation failed" combination lands exactly —
    // the batch-3 zero-recall rule `hit_count=0 AND NOT(cei=1 AND rs=0)`
    // excludes this log based on it.
    assert!(log.candidate_empty_initial && !log.relaxation_succeeded);
}

// log_query 写失败作为查询 Err 传播（spec step6 §6 硬要求，不得静默 warn）。
// A log_query write failure propagates as the query's Err (hard requirement of
// spec step6 §6; never a silent warn).
#[tokio::test]
async fn log_write_failure_propagates_as_query_error() {
    let (kernel, engine) = fixture(None).await;
    // 注入失败：日志表不存在 → INSERT 必败。
    // Failure injection: drop the log table so the INSERT must fail.
    kernel.execute_batch("DROP TABLE query_logs").unwrap();
    let err = engine
        .search(&q("波霸奶茶", Filters::empty(), Some(DOMAIN)))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Database(_)), "got {err:?}");
}

// 诊断/结果 JSON 的向后兼容：缺新字段的旧 JSON 仍可反序列化（serde default）。
// Backward compatibility of the diagnostics/result JSON: old JSON without the
// new fields still deserializes (serde defaults).
#[test]
fn diagnostics_and_result_json_accept_legacy_shapes() {
    use wiktor_core::query_engine::{QueryDiagnostics, QueryResult};

    let legacy_diag = serde_json::json!({
        "rewrite_status": "disabled",
        "applied_filters": {"conditions": []},
        "candidate_count": 0,
        "fts_count": 0,
        "vector_count": 0,
        "rrf_k": 60
    });
    let diag: QueryDiagnostics = serde_json::from_value(legacy_diag).unwrap();
    assert!(!diag.relaxation_attempted);
    assert!(!diag.relaxation_succeeded);

    let legacy_result = serde_json::json!({
        "hits": [],
        "rewritten": null,
        "rewrite_failure": false,
        "diagnostics": {
            "rewrite_status": "disabled",
            "applied_filters": {"conditions": []},
            "candidate_count": 0,
            "fts_count": 0,
            "vector_count": 0,
            "rrf_k": 60
        },
        "latency_ms": 3
    });
    let res: QueryResult = serde_json::from_value(legacy_result).unwrap();
    assert_eq!(res.log_id, None);
}

// DefaultFilterRelaxer 拍板语义单元测试（无 DB）。
// DefaultFilterRelaxer upstream-decision semantics, unit level (no DB).

#[test]
fn default_relaxer_drops_max_first_on_two_sided_range() {
    let relaxed = DefaultFilterRelaxer
        .relax_once(&price_range(Some(10.0), Some(15.0)))
        .unwrap()
        .unwrap();
    assert_eq!(
        relaxed.conditions,
        vec![FilterCondition::NumericRange {
            field: "price".into(),
            min: Some(10.0),
            max: None,
        }]
    );
}

#[test]
fn default_relaxer_min_only_range_removes_the_condition() {
    let relaxed = DefaultFilterRelaxer
        .relax_once(&price_range(Some(10.0), None))
        .unwrap()
        .unwrap();
    assert!(relaxed.is_empty(), "min-only range relaxes into no filter");
}

#[test]
fn default_relaxer_never_relaxes_text_and_ref_conditions() {
    let relaxer = DefaultFilterRelaxer;
    for filters in [
        Filters {
            conditions: vec![FilterCondition::TextEquals {
                field: "size".into(),
                value: "large".into(),
            }],
        },
        Filters {
            conditions: vec![FilterCondition::RefContains {
                field: "ingredient_ids".into(),
                refs: vec!["pearl".into()],
            }],
        },
        Filters {
            conditions: vec![FilterCondition::RefExcludes {
                field: "ingredient_ids".into(),
                refs: vec!["pearl".into()],
            }],
        },
        // 双侧皆空的区间本就无约束，无可放宽。
        // A both-absent range is already unconstrained; nothing to relax.
        price_range(None, None),
    ] {
        assert!(
            relaxer.relax_once(&filters).unwrap().is_none(),
            "condition set must not relax: {filters:?}"
        );
    }
}

#[test]
fn default_relaxer_relaxes_the_first_relaxable_condition_only() {
    let filters = Filters {
        conditions: vec![
            FilterCondition::TextEquals {
                field: "size".into(),
                value: "large".into(),
            },
            FilterCondition::NumericRange {
                field: "price".into(),
                min: Some(10.0),
                max: Some(15.0),
            },
            FilterCondition::NumericRange {
                field: "sugar_level".into(),
                min: Some(1.0),
                max: Some(2.0),
            },
        ],
    };
    let relaxed = DefaultFilterRelaxer.relax_once(&filters).unwrap().unwrap();
    // 只有第一条可放宽条件被放宽（price 去 max），其余原样保留。
    // Only the first relaxable condition is relaxed (price drops max); the rest
    // stay untouched.
    assert_eq!(
        relaxed.conditions,
        vec![
            FilterCondition::TextEquals {
                field: "size".into(),
                value: "large".into(),
            },
            FilterCondition::NumericRange {
                field: "price".into(),
                min: Some(10.0),
                max: None,
            },
            FilterCondition::NumericRange {
                field: "sugar_level".into(),
                min: Some(1.0),
                max: Some(2.0),
            },
        ]
    );
    // 输入未被原地修改（实现无副作用契约）。
    // The input was not mutated in place (the side-effect-free contract).
    assert_eq!(filters.conditions.len(), 3);
}
