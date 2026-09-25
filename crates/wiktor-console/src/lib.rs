//! Wiktor Web console（STEP11 B5）：本地 HTTP 服务，JSON API in-process 读
//! `SqliteKernel`，静态服务 `docs/console_ui/` 的 code.html（Obsidian 视觉原型）。
//! Wiktor Web console (STEP11 B5): a local HTTP service whose JSON API reads
//! `SqliteKernel` in-process, serving the `docs/console_ui/code.html` visuals
//! (the Obsidian-theme prototype).
//!
//! 设计（spec step11-console D1/D3）：console 是**只读监督界面**——面板数据
//! （仪表盘/编译任务/审阅/检索诊断/QUG）全部来自真实读 API，不写任何行；
//! 写操作留在 CLI/server。启动：`wiktor console --db <db> --port <port>`。
//! Design (spec step11-console D1/D3): the console is a **read-only supervisory
//! surface** — every panel (overview / tasks / reviews / retrieval diagnostics /
//! QUG) reads real data with zero writes; writes stay in the CLI/server. Run:
//! `wiktor console --db <db> --port <port>`.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use wiktor_core::embedding::deterministic::{DeterministicEmbedder, DIM};
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::QueryEngine;
use wiktor_core::traits::{DistanceMetric, VectorStore as _};

/// TUI console（STEP12 B1，feature `tui`）：与 Web console 同源数据面的终端形态。
/// The TUI console (STEP12 B1, feature `tui`): the terminal form sharing the
/// Web console's data plane.
#[cfg(feature = "tui")]
pub mod tui;

/// console 共享状态：一个只读 SqliteKernel + 同源混合检索引擎 + 静态资源目录。
/// The console's shared state: one read-only SqliteKernel + the same-source
/// hybrid query engine + a static dir.
#[derive(Clone)]
pub struct ConsoleState {
    pub kernel: Arc<SqliteKernel>,
    pub engine: Arc<QueryEngine<MockVectorStore>>,
    pub static_dir: std::path::PathBuf,
}

/// 启动 Web console 服务（阻塞；供 `wiktor console` CLI 与二进制复用）。
/// The static dir defaults to the repo's `docs/console_ui`; the server binds
/// `listen` and serves the JSON API + code.html at `/`.
/// Runs the Web console server (blocking; shared by the `wiktor console` CLI and
/// the binary). The static dir defaults to the repo's `docs/console_ui`; the
/// server binds `listen` and serves the JSON API + code.html at `/`.
pub async fn serve(
    db: &std::path::Path,
    listen: &str,
    static_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let kernel = Arc::new(SqliteKernel::open(db)?);
    // 检索走与 CLI `wiktor search` 同源的 QueryEngine（无 QUG → 混合 fallback，
    // Mock 向量集合恒空 → RRF 只剩 FTS），`POST /api/search` 因此返回真实
    // QueryDiagnostics（A4）。检索查询照常落 `query_logs`（D3 复用现有查询路径，
    // 不新增写路径）。
    // Retrieval goes through the same QueryEngine as CLI `wiktor search` (no QUG
    // → hybrid fallback; the Mock vector collection stays empty so RRF degrades
    // to FTS-only), so `POST /api/search` returns real QueryDiagnostics (A4).
    // Searches persist `query_logs` rows as usual (D3 reuses the existing query
    // path instead of adding a new write path).
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
    let static_dir = static_dir.unwrap_or_else(|| {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("docs")
            .join("console_ui")
    });
    let state = ConsoleState {
        kernel,
        engine,
        static_dir,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("wiktor console on http://{listen} (serves docs/console_ui/code.html)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// 构造 console 的 axum router：JSON API 全走 kernel 读；`/` 返回
/// `docs/console_ui/code.html`（由 `serve_index_html` 从磁盘读；调用方可经
/// `ConsoleState.static_dir` 覆盖）。
/// Builds the console's axum router: the JSON APIs all read the kernel; `/`
/// returns `docs/console_ui/code.html` (read from disk by `serve_index_html`;
/// overridable via `ConsoleState.static_dir`).
pub fn router(state: ConsoleState) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/overview", get(overview))
        .route("/api/tasks", get(tasks))
        .route("/api/reviews", get(reviews))
        .route("/api/qug", get(qug))
        .route("/api/search", post(search))
        .route("/api/domains", get(domains))
        .with_state(state)
}

/// `GET /`：返回 `docs/console_ui/code.html`（Web 原型视觉）。读盘失败回退
/// 一个占位 HTML，确保 console 始终有响应。
/// `GET /`: returns `docs/console_ui/code.html` (the Web prototype visuals). If
/// the file cannot be read, a placeholder HTML is returned so the console always
/// responds.
async fn index_html(State(s): State<ConsoleState>) -> impl IntoResponse {
    let dir = s.static_dir.clone();
    let file = dir.join("code.html");
    if let Ok(html) = std::fs::read_to_string(&file) {
        axum::response::Html(html).into_response()
    } else {
        axum::response::Html(
            "<h1>Wiktor Console</h1><p>code.html not found. Seed a DB, then visit \
             <code>POST /api/search</code> and <code>GET /api/overview</code>.</p>",
        )
        .into_response()
    }
}

/// `GET /api/overview`：schema + 行数 + 审阅 pending 数 + due 任务状态计数
/// （STEP12 B2：前端状态机五胶囊的真实数据源；计数遍历 due 任务快照）。
/// `GET /api/overview`: schema + row counts + review-pending + due-task status
/// counts (STEP12 B2: the real data source for the frontend's five state pills;
/// the counts walk the due-task snapshots).
async fn overview(State(s): State<ConsoleState>) -> impl IntoResponse {
    let schema = s.kernel.schema_version().unwrap_or_default();
    let rows = s.kernel.row_counts().unwrap_or_default();
    let pending = s.kernel.count_pending_reviews().unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let mut status_counts = std::collections::BTreeMap::new();
    for id in s
        .kernel
        .list_due_compile_task_ids(now, 100)
        .unwrap_or_default()
    {
        if let Ok(Some(st)) = s.kernel.compile_task_status(id) {
            *status_counts.entry(st.status).or_insert(0u64) += 1;
        }
    }
    Json(serde_json::json!({
        "schema_version": schema,
        "row_counts": rows,
        "review_pending": pending,
        "task_status_counts": status_counts,
    }))
}

/// `GET /api/tasks`：due 编译任务列表（task_id → status）。
/// `GET /api/tasks`: the due compile-task list (task_id → status).
async fn tasks(State(s): State<ConsoleState>) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let due = s
        .kernel
        .list_due_compile_task_ids(now, 100)
        .unwrap_or_default();
    let mut items = Vec::new();
    for id in due {
        if let Ok(Some(st)) = s.kernel.compile_task_status(id) {
            items.push(serde_json::json!({
                "task_id": st.task_id,
                "entity_id": st.entity_id,
                "status": st.status,
                "attempt_count": st.attempt_count,
            }));
        }
    }
    Json(serde_json::json!({ "tasks": items }))
}

/// `GET /api/reviews`：审阅队列（pending，上限 100）。
/// `GET /api/reviews`: the pending review queue (limit 100).
async fn reviews(State(s): State<ConsoleState>) -> impl IntoResponse {
    use wiktor_core::kernel::ReviewStatus;
    let items = s
        .kernel
        .list_reviews("", Some(ReviewStatus::Pending), 100)
        .unwrap_or_default();
    let items: Vec<serde_json::Value> = items
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "review_id": r.review_id,
                "domain": r.domain,
                "action": r.action,
                "status": format!("{:?}", r.status),
                "created_at": r.created_at,
            })
        })
        .collect();
    Json(serde_json::json!({ "reviews": items }))
}

/// `GET /api/qug`：发布/代次状态（generations 表行数 + 已发布页数），从
/// `row_counts` 真实读。
/// `GET /api/qug`: publish/generation status (the generations table count + the
/// published-pages count), read from the real `row_counts`.
async fn qug(State(s): State<ConsoleState>) -> impl IntoResponse {
    let rows = s.kernel.row_counts().unwrap_or_default();
    let generations = rows.get("generations").copied().unwrap_or(0);
    let pages = rows.get("pages").copied().unwrap_or(0);
    Json(serde_json::json!({
        "generations": generations,
        "published_pages": pages,
    }))
}

/// `POST /api/search`：经 QueryEngine 的真实混合检索（与 CLI `wiktor search`
/// 同源），返回命中 + `QueryDiagnostics` 分路诊断（rewrite/fts/vector/rrf_k）。
/// 请求体：`{"q": "...", "top_k": 5, "domain": "可选"}`。
/// `POST /api/search`: real hybrid retrieval via the QueryEngine (same source as
/// CLI `wiktor search`), returning hits plus the `QueryDiagnostics` per-path
/// breakdown (rewrite/fts/vector/rrf_k). Body:
/// `{"q": "...", "top_k": 5, "domain": "optional"}`.
async fn search(
    State(s): State<ConsoleState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    use wiktor_core::types::{Filters, Query};
    let text = body.get("q").and_then(|v| v.as_str()).unwrap_or_default();
    let top_k = body.get("top_k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
    let domain = body
        .get("domain")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let query = Query {
        text: text.to_string(),
        filters: Filters::default(),
        top_k,
        domain,
    };
    // 检索失败（如 schema 未就绪）返回空命中 + error 字段，不让面板 500。
    // On search failure (e.g. a not-yet-ready schema) return empty hits plus an
    // `error` field instead of a 500 for the panel.
    let payload = match s.engine.search(&query).await {
        Ok(result) => {
            let hits: Vec<serde_json::Value> = result
                .hits
                .into_iter()
                .map(|h| {
                    serde_json::json!({
                        "page_id": h.page_id,
                        "entity_id": h.entity_id.to_key(),
                        "score": h.score,
                        "title": h.title,
                    })
                })
                .collect();
            let diagnostics = serde_json::to_value(&result.diagnostics).unwrap_or_default();
            serde_json::json!({
                "query": text,
                "hits": hits,
                "diagnostics": diagnostics,
                "latency_ms": result.latency_ms,
            })
        }
        Err(e) => serde_json::json!({
            "query": text,
            "hits": [],
            "error": e.to_string(),
        }),
    };
    Json(payload)
}

/// `GET /api/domains`：领域包列表（由调用方注入静态已知，或经 core
/// domain 发现；此处返回一个空数组，供前端占位——真实领域发现由
/// `wiktor domain list` CLI 提供）。
/// `GET /api/domains`: domain-pack list (injected statically by the caller, or
/// from core discovery; returns an empty array here as a placeholder — the real
/// discovery is `wiktor domain list`).
async fn domains() -> impl IntoResponse {
    Json(serde_json::json!({ "domains": [] }))
}
