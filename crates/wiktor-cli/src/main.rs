use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use wiktor_core::kernel::SqliteKernel;

/// Wiktor — 知识编译与检索中间件（检索数据库）。
///
/// 本二进制是 CLI 形态：数据源/领域包驱动 LLM 编译出 Wiki（知识平面），
/// 高频字段 ETL 直写元数据（事实平面）；查询路径默认零 LLM。
#[derive(Parser)]
#[command(name = "wiktor", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 打开/新建数据库并报告 schema 版本与各表行数
    Status {
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
    },
    /// 向量服务相关操作
    Vector {
        #[command(subcommand)]
        command: VectorCommand,
    },
}

#[derive(Subcommand)]
enum VectorCommand {
    /// 检查 qdrant 向量服务连通性
    Ping {
        /// qdrant gRPC 地址（默认 $WIKTOR_QDRANT_URL 或 http://127.0.0.1:6334）
        #[arg(
            long,
            env = "WIKTOR_QDRANT_URL",
            default_value = "http://127.0.0.1:6334"
        )]
        url: String,
        /// qdrant API key（优先 $WIKTOR_QDRANT_API_KEY）
        #[arg(long, env = "WIKTOR_QDRANT_API_KEY")]
        api_key: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status { db } => {
            let kernel = SqliteKernel::open(&db)?;
            let version = kernel.schema_version()?;
            println!("schema version: {version}");
            for (table, count) in kernel.row_counts()? {
                println!("{table}: {count}");
            }
        }
        Command::Vector { command } => match command {
            VectorCommand::Ping { url, api_key } => {
                let store =
                    wiktor_core::QdrantVectorStore::from_config(&url, api_key.as_deref(), 768)?;
                let health = store
                    .client()
                    .health_check()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                println!("qdrant ok: {}", health.version);
            }
        },
    }
    Ok(())
}
