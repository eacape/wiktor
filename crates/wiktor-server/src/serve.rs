//! Step7 B5 共享服务运行时（spec step7 §3 D12/A19-A23）：同一进程同时启动 HTTP
//! 与 gRPC listener，并按 domain pack 装配编译 worker。旧 `wiktor-server` bin 与
//! CLI `wiktor serve` 都委托本入口。
//! Step7 B5 shared server runtime (spec step7 §3 D12/A19-A23): one process starts
//! both the HTTP and gRPC listeners and assembles the compile worker from the
//! domain pack. Both the legacy `wiktor-server` bin and the CLI `wiktor serve`
//! delegate to this entry.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use wiktor_core::embedding::deterministic::DeterministicEmbedder;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::{QueryEmbedder, QueryEngine};
use wiktor_core::traits::{DistanceMetric, VectorStore};

use crate::services::compatibility::CompatibilityService;
use crate::services::compile::CompileService;
use crate::services::qug_build::QugBuildService;
use crate::services::review::ReviewService;
use crate::services::search::SearchService;
use crate::services::status::StatusService;
use crate::state::{ApiKeys, ServerState};
use crate::worker::CompileWorker;

/// 服务启动参数（B5；Step13 D1 增注入口）。
/// Server startup options (B5; injection points added in Step13 D1).
pub struct ServeOptions {
    pub db: std::path::PathBuf,
    pub listen_http: String,
    pub listen_grpc: String,
    pub domain_pack: Option<std::path::PathBuf>,
    pub source_path: Option<String>,
    /// 注入向量后端（Step13 D1）：None → Mock（现状）；qdrant 等具体实现由
    /// CLI 装配者按 env 构建，server 只认 core trait（Step10 插件边界）。
    /// The injected vector backend (Step13 D1): None → Mock (status quo);
    /// concrete implementations such as qdrant are built by the CLI assembler
    /// from env, while the server only knows the core traits (the Step10
    /// plugin boundary).
    pub vector_store: Option<Arc<dyn VectorStore>>,
    /// 注入查询嵌入器：None → 确定性嵌入（离线基线）。
    /// The injected query embedder: None → the deterministic embedder (the
    /// offline baseline).
    pub embedder: Option<Arc<dyn QueryEmbedder>>,
    /// 注入持久化 QUG 图（CLI 经 load_active_qug 加载；None → qug 缺席，
    /// 显式混合 fallback）。
    /// The injected persistent QUG graph (loaded by the CLI via
    /// load_active_qug; None → the graph is absent with an explicit hybrid
    /// fallback).
    pub qug: Option<Arc<wiktor_core::query_engine::qug::QugGraph>>,
}

/// 启动双 listener（HTTP + gRPC）并按需启动编译 worker。
/// Step13 D1/D2：engine 先装配（可注入），与 ServerState 共享给 HTTP /search
/// 与 gRPC Search。
/// Starts both listeners (HTTP + gRPC) and optionally the compile worker.
/// Step13 D1/D2: the engine is assembled first (injectable) and shared with
/// both HTTP /search and the gRPC Search through ServerState.
pub async fn run_server(options: ServeOptions) -> Result<(), String> {
    let keys = ApiKeys::from_env().map_err(|e| e.to_string())?;
    let kernel = Arc::new(
        SqliteKernel::open(&options.db)
            .map_err(|e| format!("open database {}: {e}", options.db.display()))?,
    );
    let (state, search) = assemble_state_and_search(kernel.clone(), keys.clone(), &options).await?;
    let http = crate::build_router(state);
    let grpc = crate::grpc::router(
        keys,
        search,
        CompileService::new(kernel.clone()),
        QugBuildService::new(kernel.clone()),
        ReviewService::new(kernel.clone()),
        CompatibilityService::new(kernel.clone()),
        StatusService::new(kernel.clone()),
    );
    let cancel = CancellationToken::new();
    let worker = if let Some(domain_pack) = options.domain_pack.as_deref() {
        Some(
            CompileWorker::from_domain_pack(
                kernel,
                &domain_pack.display().to_string(),
                options.source_path.as_deref(),
                None,
                "",
            )
            .map_err(|e| format!("assemble compile worker: {e}"))?
            .spawn(cancel.clone()),
        )
    } else {
        None
    };
    let http_listener = TcpListener::bind(&options.listen_http)
        .await
        .map_err(|e| format!("bind HTTP {}: {e}", options.listen_http))?;
    tracing::info!(http = %options.listen_http, grpc = %options.listen_grpc, "wiktor server listening");
    let grpc_addr = options.listen_grpc.clone();
    let http_task = tokio::spawn(async move { axum::serve(http_listener, http).await });
    let grpc_task = tokio::spawn(async move {
        let addr = grpc_addr
            .parse()
            .map_err(|e: std::net::AddrParseError| format!("invalid gRPC addr: {e}"))?;
        grpc.serve(addr)
            .await
            .map_err(|e| format!("gRPC server error: {e}"))
    });
    tokio::select! {
        result = http_task => result.map_err(|e| format!("HTTP task join failed: {e}"))?.map_err(|e| format!("HTTP server error: {e}"))?,
        result = grpc_task => result.map_err(|e| format!("gRPC task join failed: {e}"))?.map_err(|e| format!("gRPC server error: {e}"))?,
    }
    cancel.cancel();
    if let Some(handle) = worker {
        let _ = handle.await;
    }
    Ok(())
}

/// 装配共享 engine + ServerState + gRPC Search 服务（Step13 D1/D2）。
/// 注入项缺省回退：Mock 向量库 + 确定性嵌入（离线基线），qug=None。
/// Assembles the shared engine + ServerState + the gRPC Search service
/// (Step13 D1/D2). Injection defaults fall back to the Mock vector store + the
/// deterministic embedder (the offline baseline) with qug=None.
async fn assemble_state_and_search(
    kernel: Arc<SqliteKernel>,
    keys: ApiKeys,
    options: &ServeOptions,
) -> Result<(Arc<ServerState>, SearchService<dyn VectorStore>), String> {
    let domain = options
        .domain_pack
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|yaml| serde_yaml_ng::from_str::<wiktor_core::traits::DomainConfig>(&yaml).ok());
    let domain_name = domain
        .as_ref()
        .map(|config| config.name.clone())
        .unwrap_or_else(|| "default".to_string());
    let store: Arc<dyn VectorStore> = match &options.vector_store {
        Some(store) => store.clone(),
        None => Arc::new(MockVectorStore::new()),
    };
    let embedder: Arc<dyn QueryEmbedder> = match &options.embedder {
        Some(embedder) => embedder.clone(),
        None => Arc::new(DeterministicEmbedder::new(768)),
    };
    // 维度探测：对探测串嵌入一次取长度（与 cmd_vector_build 同模式；确定性嵌
    // 入恒 768，HttpEmbedder 按远端模型实测维度）。
    // Dimension probe: embed the probe string once and take the length (the
    // same pattern as cmd_vector_build; the deterministic embedder is always
    // 768 while HttpEmbedder reflects the remote model's measured dimension).
    let probe = embedder
        .embed("wiktor-collection-dimension-probe")
        .await
        .map_err(|e| format!("embed dimension probe: {e}"))?;
    let dim = probe.len();
    store
        .ensure_collection(&domain_name, dim, DistanceMetric::Cosine)
        .await
        .map_err(|e| format!("ensure vector collection: {e}"))?;
    // QUG 图注入（CLI 已按 active build 加载，A7 语义由加载方承担）；域配置在
    // 场时 candidate_multiplier 与 config 同源冻结。
    // The QUG graph is injected (loaded from the active build by the CLI; the
    // A7 semantics belong to the loader). With a domain config present, the
    // candidate_multiplier is frozen from the same config.
    let candidate_multiplier = domain
        .as_ref()
        .map(|config| config.qug.candidate_multiplier)
        .filter(|_| options.qug.is_some())
        .unwrap_or(5);
    // P1 查询热点缓存（MASTER-PLAN §5.4/§5.5 契约 #5）：生产默认开启，容量取
    // `WIKTOR_QUERY_CACHE_SIZE`（缺省 512）；设 0 显式关闭（诊断/对比用）。键
    // 带 generation + 实体反向索引，编译发布/实体直写自动失效。
    // P1 query hot-cache (MASTER-PLAN §5.4/§5.5 #5): on by default in serve,
    // capacity from `WIKTOR_QUERY_CACHE_SIZE` (default 512); setting it to 0
    // disables (diagnostics/comparison). The key carries generation + an entity
    // reverse index, so compile publishes and direct fact writes invalidate
    // automatically.
    let cache_size: u64 = std::env::var("WIKTOR_QUERY_CACHE_SIZE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(512);
    let engine = Arc::new(
        QueryEngine::new(
            kernel.clone(),
            store,
            options.qug.clone(),
            embedder,
            &domain_name,
            candidate_multiplier,
            60,
        )
        .map(|e| {
            if cache_size > 0 {
                e.with_cache(wiktor_core::query_engine::QueryCache::new(cache_size))
            } else {
                e
            }
        })
        .map_err(|e| format!("assemble query engine: {e}"))?,
    );
    let state = Arc::new(ServerState::new(kernel, keys, engine.clone()));
    Ok((state, SearchService::new(engine, 16 * 1024)))
}
