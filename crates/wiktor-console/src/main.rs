//! `wiktor console` 启动器（STEP11 B5）：本地 HTTP 服务，挂载 JSON API +
//! `docs/console_ui/` 静态（code.html）。
//! `wiktor console` launcher (STEP11 B5): a local HTTP service mounting the JSON
//! API plus the `docs/console_ui/` static assets (code.html).

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use wiktor_console::serve;

/// `wiktor console`：把 Wiktor 数据接到 Web 可视化界面。
/// `wiktor console`: wire Wiktor data into a Web visualization surface.
#[derive(Parser, Debug)]
#[command(name = "console", about = "Run the Wiktor Web console")]
struct Cli {
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    db: PathBuf,
    /// HTTP listen address (default 127.0.0.1:8081)
    /// HTTP 监听地址（默认 127.0.0.1:8081）
    #[arg(long, default_value = "127.0.0.1:8081")]
    listen: String,
    /// Directory of static assets (default the repo's docs/console_ui)
    /// 静态资源目录（默认仓库的 docs/console_ui）
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let cli = Cli::parse();
    println!("wiktor console starting…");
    serve(&cli.db, &cli.listen, cli.static_dir.clone()).await
}
