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
    wiktor_server::serve::run_server(wiktor_server::serve::ServeOptions {
        db: args.db,
        listen_http: args.listen,
        listen_grpc: std::env::var("WIKTOR_GRPC_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".into()),
        domain_packs: std::env::var("WIKTOR_DOMAIN_PACK")
            .map(|v| {
                v.split(':')
                    .filter(|s| !s.trim().is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default(),
        source_path: std::env::var("WIKTOR_SOURCE_PATH").ok(),
        // 旧 bin 无注入能力（Step13 D1：注入面在 CLI 装配者）——恒走缺省
        // Mock + 确定性嵌入。
        // The legacy bin has no injection capability (Step13 D1: the
        // injection surface lives in the CLI assembler) — it always uses the
        // default Mock + deterministic embedder.
        vector_store: None,
        embedder: None,
        qugs: std::collections::HashMap::new(),
    })
    .await
}
