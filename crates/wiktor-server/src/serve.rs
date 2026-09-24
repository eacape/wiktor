//! Step7 B5 共享服务运行时（spec step7 §3 D12/A19-A23）：同一进程同时启动 HTTP
//! 与 gRPC listener，并按 domain pack 装配编译 worker。旧 `wiktor-server` bin 与
//! CLI `wiktor serve` 都委托本入口。
//! Step7 B5 shared server runtime (spec step7 §3 D12/A19-A23): one process starts
//! both the HTTP and gRPC listeners and assembles the compile worker from the
//! domain pack. Both the legacy `wiktor-server` bin and the CLI `wiktor serve`
//! delegate to this entry.

use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use wiktor_core::embedding::deterministic::DeterministicEmbedder;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::QueryEngine;
use wiktor_core::traits::{DistanceMetric, VectorStore};

use crate::services::compatibility::CompatibilityService;
use crate::services::compile::CompileService;
use crate::services::qug_build::QugBuildService;
use crate::services::review::ReviewService;
use crate::services::search::SearchService;
use crate::services::status::StatusService;
use crate::state::{ApiKeys, ServerState};
use crate::worker::CompileWorker;

/// 服务启动参数（B5）。
/// Server startup options (B5).
pub struct ServeOptions {
    pub db: std::path::PathBuf,
    pub listen_http: String,
    pub listen_grpc: String,
    pub domain_pack: Option<std::path::PathBuf>,
    pub source_path: Option<String>,
}

/// 启动双 listener（HTTP + gRPC）并按需启动编译 worker。
/// Starts both listeners (HTTP + gRPC) and optionally the compile worker.
pub async fn run_server(options: ServeOptions) -> Result<(), String> {
    let keys = ApiKeys::from_env().map_err(|e| e.to_string())?;
    let kernel = Arc::new(
        SqliteKernel::open(&options.db)
            .map_err(|e| format!("open database {}: {e}", options.db.display()))?,
    );
    let state = Arc::new(ServerState::new(kernel.clone(), keys.clone()));
    let http = crate::build_router(state);
    let search = assemble_search(kernel.clone(), options.domain_pack.as_deref())?;
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

fn assemble_search(
    kernel: Arc<SqliteKernel>,
    domain_pack: Option<&Path>,
) -> Result<SearchService<MockVectorStore>, String> {
    let domain = domain_pack
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|yaml| serde_yaml_ng::from_str::<wiktor_core::traits::DomainConfig>(&yaml).ok())
        .map(|config| config.name)
        .unwrap_or_else(|| "default".to_string());
    let store = Arc::new(MockVectorStore::new());
    let store_for_collection = store.clone();
    let domain_for_collection = domain.clone();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            store_for_collection
                .ensure_collection(&domain_for_collection, 768, DistanceMetric::Cosine)
                .await
        })
    })
    .map_err(|e| format!("ensure vector collection: {e}"))?;
    let qug = None;
    let engine = QueryEngine::new(
        kernel,
        store,
        qug,
        Arc::new(DeterministicEmbedder::new(768)),
        &domain,
        5,
        60,
    )
    .map_err(|e| format!("assemble query engine: {e}"))?;
    Ok(SearchService::new(Arc::new(engine), 16 * 1024))
}
