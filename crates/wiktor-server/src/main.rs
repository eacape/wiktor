//! wiktor-server 二进制入口（spec `step6-feedback-loop.md` §3 D1/D2、§11 批5；
//! 上层拍板：自持 main.rs bin，参数 `--db <path> --listen <addr>`，key 从环境
//! 变量读，不做 CLI 子命令）。
//! The wiktor-server binary entry (spec `step6-feedback-loop.md` §3 D1/D2,
//! §11 batch 5; upstream decision: its own main.rs bin with `--db <path>
//! --listen <addr>`, keys read from the environment, no CLI subcommands).
//!
//! 启动顺序（fail-closed）：参数解析 → key 解析（空/重复/非法 → 启动失败）
//! → 打开数据库并应用迁移（`SqliteKernel::open` 含 migrate）→ 绑定监听地址
//! → tokio 多线程 runtime 上的 axum serve（D2）。
//! Startup order (fail-closed): argument parsing → key parsing (empty/
//! duplicate/illegal → startup failure) → open the database and apply
//! migrations (`SqliteKernel::open` includes migrate) → bind the listener →
//! axum serve on the tokio multi-threaded runtime (D2).

use std::path::PathBuf;
use std::process::ExitCode;

use wiktor_server::state::{ApiKeys, ServerState};

struct Args {
    db: PathBuf,
    listen: String,
}

/// 手工解析两个必需 flag（不引 clap：server bin 无子命令面，依赖最小化）。
/// Manually parses the two required flags (no clap: the server bin has no
/// subcommand surface, keeping dependencies minimal).
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut db: Option<PathBuf> = None;
    let mut listen: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => {
                db = Some(PathBuf::from(
                    args.next().ok_or("--db requires a path argument")?,
                ));
            }
            "--listen" => {
                listen = Some(args.next().ok_or("--listen requires an addr argument")?);
            }
            other => {
                return Err(format!(
                    "unknown argument {other:?}; expected --db/--listen"
                ))
            }
        }
    }
    Ok(Args {
        db: db.ok_or("--db <path> is required")?,
        listen: listen.ok_or("--listen <addr> is required")?,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // 启动失败信息只含配置语义，绝不含 secret 原文（D8）。
            // Startup-failure messages carry configuration semantics only,
            // never raw secrets (D8).
            eprintln!("wiktor-server: {message}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    let keys = ApiKeys::from_env().map_err(|e| e.to_string())?;
    // 打开即迁移（本批任务：server 启动时 migrate）。
    // Open implies migrate (this batch's task: migrate at server startup).
    let kernel = std::sync::Arc::new(
        wiktor_core::SqliteKernel::open(&args.db)
            .map_err(|e| format!("open database {}: {e}", args.db.display()))?,
    );
    let state = std::sync::Arc::new(ServerState::new(kernel, keys));
    let router = wiktor_server::build_router(state);
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .map_err(|e| format!("bind {}: {e}", args.listen))?;
    // 无 subscriber 时为 no-op；嵌入方可自行安装 tracing-subscriber。
    // A no-op without a subscriber; embedders can install tracing-subscriber.
    tracing::info!(listen = %args.listen, "wiktor-server listening");
    axum::serve(listener, router)
        .await
        .map_err(|e| format!("server error: {e}"))
}
