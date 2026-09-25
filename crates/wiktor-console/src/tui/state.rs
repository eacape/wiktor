//! TUI 状态与数据装载（STEP12 B1/D2/D3）：纯逻辑、渲染无关——状态转换与
//! kernel 读 API 的映射都在这里，`tui::mod` 只做终端渲染与事件循环；全部
//! 读路径与 Web console 同源（只读监督，查询照常落 query_logs）。
//! TUI state and data loading (STEP12 B1/D2/D3): pure logic, rendering-agnostic
//! — state transitions and the kernel read-API mappings live here; `tui::mod`
//! only does terminal rendering and the event loop. Every read path shares the
//! Web console's source (read-only supervision; searches persist query_logs).

use std::sync::Arc;

use wiktor_core::embedding::deterministic::{DeterministicEmbedder, DIM};
use wiktor_core::kernel::{MockVectorStore, ReviewStatus, SqliteKernel};
use wiktor_core::query_engine::QueryEngine;
use wiktor_core::traits::{DistanceMetric, VectorStore as _};
use wiktor_core::types::Query;

/// TUI 四个 Tab（spec step12 §4 B1：1 仪表盘 / 2 任务 / 3 审阅 / 4 查询）。
/// The four TUI tabs (spec step12 §4 B1: 1 dashboard / 2 tasks / 3 reviews /
/// 4 query).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Tab {
    #[default]
    Overview,
    Tasks,
    Reviews,
    Query,
}

pub const TABS: [Tab; 4] = [Tab::Overview, Tab::Tasks, Tab::Reviews, Tab::Query];

impl Tab {
    /// Tab 标题（中文优先，与 spec step12 §4 B1 一致）。
    /// Tab titles (Chinese first, matching spec step12 §4 B1).
    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "仪表盘",
            Tab::Tasks => "任务",
            Tab::Reviews => "审阅",
            Tab::Query => "查询",
        }
    }

    pub fn next(self) -> Self {
        let i = TABS.iter().position(|t| *t == self).unwrap_or(0);
        TABS[(i + 1) % TABS.len()]
    }
}

/// 仪表盘数据：schema + 行数 + 审阅 pending + QUG 代次/发布页数。
/// Dashboard data: schema + row counts + review pending + QUG generation/publish.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverviewData {
    pub schema_version: i64,
    pub rows: Vec<(String, i64)>,
    pub review_pending: i64,
    pub generations: i64,
    pub published_pages: i64,
}

/// 任务表行（due 编译任务快照）。
/// A task-table row (a due compile-task snapshot).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRow {
    pub task_id: i64,
    pub entity_id: String,
    pub status: String,
    pub attempt_count: u32,
}

/// 审阅队列表行。
/// A review-queue table row.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewRow {
    pub review_id: i64,
    pub domain: String,
    pub action: String,
    pub status: String,
    pub created_at: i64,
}

/// 查询命中行。
/// A search-hit row.
#[derive(Clone, Debug, PartialEq)]
pub struct HitRow {
    pub title: String,
    pub entity_id: String,
    pub score: f32,
}

/// 查询终端结果：命中 + QueryDiagnostics 分路（与 Web console /api/search 同构）。
/// The query-terminal result: hits plus the QueryDiagnostics breakdown (same
/// shape as the Web console's /api/search).
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutcome {
    pub query: String,
    pub hits: Vec<HitRow>,
    pub rewrite_status: String,
    pub fts_count: usize,
    pub vector_count: usize,
    pub rrf_k: u32,
    pub latency_ms: u64,
    pub error: Option<String>,
}

/// TUI 全量状态（D3：渲染函数只读这里，事件只改这里）。
/// The full TUI state (D3: render functions only read this; events only write
/// this).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TuiState {
    pub tab: Tab,
    pub overview: Option<OverviewData>,
    pub tasks: Vec<TaskRow>,
    pub reviews: Vec<ReviewRow>,
    pub query_input: String,
    pub search: Option<SearchOutcome>,
    /// 最近一次数据装载是否失败（true → 标题栏 offline 徽标）。
    /// Whether the latest data load failed (true → the offline badge in the
    /// title bar).
    pub offline: bool,
}

impl TuiState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cycle_tab(&mut self) {
        self.tab = self.tab.next();
    }

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
    }

    /// 查询 Tab 的文本输入（其余 Tab 忽略）。
    /// Text input for the query tab (ignored on other tabs).
    pub fn push_char(&mut self, c: char) {
        if self.tab == Tab::Query {
            self.query_input.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.tab == Tab::Query {
            self.query_input.pop();
        }
    }

    pub fn apply_overview(&mut self, data: OverviewData) {
        self.overview = Some(data);
        self.offline = false;
    }

    pub fn apply_tasks(&mut self, tasks: Vec<TaskRow>) {
        self.tasks = tasks;
    }

    pub fn apply_reviews(&mut self, reviews: Vec<ReviewRow>) {
        self.reviews = reviews;
    }

    pub fn apply_search(&mut self, outcome: SearchOutcome) {
        self.search = Some(outcome);
    }
}

/// 装配 TUI 数据面（D2：与 Web console `serve()` 同一构造——kernel 读 +
/// QueryEngine fallback 混合检索）。返回 (kernel, engine)。
/// Assembles the TUI data plane (D2: the same construction as the Web console's
/// `serve()` — kernel reads + the QueryEngine fallback hybrid retrieval).
/// Returns (kernel, engine).
pub async fn assemble(
    db: &std::path::Path,
) -> anyhow::Result<(Arc<SqliteKernel>, Arc<QueryEngine<MockVectorStore>>)> {
    let kernel = Arc::new(SqliteKernel::open(db)?);
    let vector_store = Arc::new(MockVectorStore::new());
    vector_store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let embedder = Arc::new(DeterministicEmbedder::new(DIM));
    let engine = Arc::new(
        QueryEngine::new(
            kernel.clone(),
            vector_store,
            None,
            embedder,
            "milk-tea",
            5,
            60,
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))?,
    );
    Ok((kernel, engine))
}

/// 仪表盘数据装载：任何读失败都折叠为 offline 而不是让 TUI 崩溃。
/// Dashboard data load: any read failure folds into offline instead of
/// crashing the TUI.
pub fn load_overview(kernel: &Arc<SqliteKernel>) -> anyhow::Result<OverviewData> {
    let schema = kernel.schema_version()?;
    let mut rows: Vec<(String, i64)> = kernel.row_counts()?.into_iter().collect();
    rows.sort();
    let rows_map = kernel.row_counts()?;
    let pending = kernel.count_pending_reviews()?;
    Ok(OverviewData {
        schema_version: schema,
        rows,
        review_pending: pending,
        generations: rows_map.get("generations").copied().unwrap_or(0),
        published_pages: rows_map.get("pages").copied().unwrap_or(0),
    })
}

pub fn load_tasks(kernel: &Arc<SqliteKernel>) -> Vec<TaskRow> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    kernel
        .list_due_compile_task_ids(now, 100)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| {
            kernel
                .compile_task_status(id)
                .ok()
                .flatten()
                .map(|st| TaskRow {
                    task_id: st.task_id,
                    entity_id: st.entity_id,
                    status: st.status,
                    attempt_count: st.attempt_count,
                })
        })
        .collect()
}

pub fn load_reviews(kernel: &Arc<SqliteKernel>) -> Vec<ReviewRow> {
    kernel
        .list_reviews("", Some(ReviewStatus::Pending), 100)
        .unwrap_or_default()
        .into_iter()
        .map(|r| ReviewRow {
            review_id: r.review_id,
            domain: r.domain,
            action: r.action,
            status: format!("{:?}", r.status),
            created_at: r.created_at,
        })
        .collect()
}

/// 检索执行（与 Web console 同源 QueryEngine；失败折进 error 字段而非崩溃）。
/// Runs a search (the same QueryEngine as the Web console; failures fold into
/// the `error` field instead of crashing).
pub async fn run_search(
    engine: &Arc<QueryEngine<MockVectorStore>>,
    text: &str,
    top_k: usize,
) -> SearchOutcome {
    let query = Query {
        text: text.to_string(),
        filters: wiktor_core::types::Filters::default(),
        top_k,
        domain: None,
    };
    match engine.search(&query).await {
        Ok(result) => SearchOutcome {
            query: text.to_string(),
            hits: result
                .hits
                .into_iter()
                .map(|h| HitRow {
                    title: h.title,
                    entity_id: h.entity_id.to_key(),
                    score: h.score,
                })
                .collect(),
            rewrite_status: format!("{:?}", result.diagnostics.rewrite_status),
            fts_count: result.diagnostics.fts_count,
            vector_count: result.diagnostics.vector_count,
            rrf_k: result.diagnostics.rrf_k,
            latency_ms: result.latency_ms,
            error: None,
        },
        Err(e) => SearchOutcome {
            query: text.to_string(),
            hits: vec![],
            rewrite_status: "-".into(),
            fts_count: 0,
            vector_count: 0,
            rrf_k: 0,
            latency_ms: 0,
            error: Some(e.to_string()),
        },
    }
}
