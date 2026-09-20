use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::{DataSource, DomainConfig, EntityStore};
use wiktor_core::types::{Cursor, PublishStatus};
use wiktor_core::{seed, FactValue, Filters};

mod filter;

/// Wiktor — knowledge compilation & retrieval middleware (a retrieval database).
/// Wiktor — 知识编译与检索中间件（检索数据库）。
///
/// CLI form: data source / domain pack drives LLM compilation into Wiki (knowledge
/// plane); high-frequency fields are ETL-written straight into metadata (fact plane).
/// Query path is zero-LLM by default.
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
    /// Open/create the database and report schema version + row counts.
    /// 打开/新建数据库并报告 schema 版本与各表行数。
    Status {
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
    },
    /// Seed knowledge pages + fact-plane data from a domain pack.
    /// 从领域包导入知识页面与事实平面数据。
    Seed {
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// domain.yaml path (mandatory)
        /// domain.yaml 路径（必填）
        #[arg(long)]
        domain: PathBuf,
        /// seed-wiki directory (default: <domain dir>/seed-wiki)
        /// seed-wiki 目录（默认 <domain 所在目录>/seed-wiki）
        #[arg(long)]
        pages: Option<PathBuf>,
    },
    /// Hybrid search: FTS5 (knowledge) + fact-plane filter pushdown (CLI display).
    /// 混合检索：FTS5（知识平面）+ 事实平面过滤下推（CLI 展示）。
    Search {
        /// Search text (query)
        /// 检索文本（查询词）
        text: String,
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// Comma-separated filters, e.g. "price<=20,sugar_level>=50,size=中杯";
        /// `in=`/`not_in=` lists use '|', e.g. "ingredient_ids in=pearl|taro".
        /// 逗号分隔过滤条件，如 "price<=20,sugar_level>=50,size=中杯"；
        /// `in=`/`not_in=` 列表用 '|' 分隔，如 "ingredient_ids in=pearl|taro"。
        #[arg(long)]
        filter: Option<String>,
        /// Max hits to return (default 5)
        /// 最多返回条数（默认 5）
        #[arg(long, default_value_t = 5)]
        top_k: usize,
    },
    /// Vector service operations
    /// 向量服务相关操作
    Vector {
        #[command(subcommand)]
        command: VectorCommand,
    },
}

#[derive(Subcommand)]
enum VectorCommand {
    /// Check qdrant vector service connectivity
    /// 检查 qdrant 向量服务连通性
    Ping {
        /// qdrant gRPC address (default $WIKTOR_QDRANT_URL or http://127.0.0.1:6334)
        /// qdrant gRPC 地址（默认 $WIKTOR_QDRANT_URL 或 http://127.0.0.1:6334）
        #[arg(
            long,
            env = "WIKTOR_QDRANT_URL",
            default_value = "http://127.0.0.1:6334"
        )]
        url: String,
        /// qdrant API key (default $WIKTOR_QDRANT_API_KEY)
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
        Command::Seed { db, domain, pages } => cmd_seed(&db, &domain, pages.as_deref()).await?,
        Command::Search {
            text,
            db,
            filter,
            top_k,
        } => cmd_search(&db, &text, filter.as_deref(), top_k)?,
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

/// Seed knowledge pages + fact-plane data.
/// 导入知识页面与事实平面数据。
async fn cmd_seed(db: &Path, domain_yaml: &Path, pages_dir: Option<&Path>) -> Result<()> {
    let domain_dir = domain_yaml
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let yaml_text = std::fs::read_to_string(domain_yaml)
        .with_context(|| format!("read {}", domain_yaml.display()))?;
    let config: DomainConfig =
        serde_yaml_ng::from_str(&yaml_text).map_err(|e| anyhow!("parse domain.yaml: {e}"))?;
    let kernel = SqliteKernel::open(db)?;
    let started = std::time::Instant::now();

    // 1) Seed knowledge pages from seed-wiki/*.md
    // 1) 从 seed-wiki/*.md 导入知识页面
    let pages_dir = pages_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| domain_dir.join("seed-wiki"));
    let mut md_files: Vec<PathBuf> = std::fs::read_dir(&pages_dir)
        .with_context(|| format!("read seed-wiki dir {}", pages_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();

    let mut page_count = 0usize;
    for path in &md_files {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut page = seed::parse_page(&content)
            .with_context(|| format!("parse seed page {}", path.display()))?;
        page.metadata.domain_pack_version = config.version.clone();
        kernel.seed_pages(&page, &config.name, PublishStatus::Accepted)?;
        page_count += 1;
    }

    // 2) Fact plane from the first jsonl:// entity source
    // 2) 从第一个 jsonl:// 实体源导入事实平面
    let mut fact_count = 0usize;
    let mut ref_count = 0usize;
    if let Some(entity) = config
        .entities
        .iter()
        .find(|e| e.source.starts_with("jsonl://"))
    {
        let source = JsonlDataSource::from_config(entity, &domain_dir)?;
        let mut cursor: Option<Cursor> = None;
        loop {
            let batch = source.fetch(cursor.clone()).await?;
            if batch.is_empty() {
                break;
            }
            for raw in &batch {
                let facts = source.raw_to_facts(raw)?;
                fact_count += facts.fields.len();
                for v in facts.fields.values() {
                    if let FactValue::RefList(refs) = v {
                        ref_count += refs.len();
                    }
                }
                kernel
                    .upsert_facts(&raw.id, &facts, raw.source_revision)
                    .await?;
            }
            let offset = cursor.map(|c| c.offset).unwrap_or(0) + batch.len();
            cursor = Some(Cursor {
                offset,
                batch_size: wiktor_core::data::DEFAULT_BATCH_SIZE,
            });
        }
    }

    println!(
        "seeded: pages={page_count} facts={fact_count} fact_refs={ref_count} elapsed={:?}",
        started.elapsed()
    );
    Ok(())
}

/// Search with optional fact-plane filter pushdown.
/// 检索并支持事实平面过滤下推。
fn cmd_search(db: &Path, text: &str, filter_spec: Option<&str>, top_k: usize) -> Result<()> {
    let kernel = SqliteKernel::open(db)?;
    let filters = match filter_spec {
        Some(spec) => filter::parse_filter(spec)?,
        None => Filters::empty(),
    };
    let hits = kernel.search(text, &filters, top_k, None)?;
    println!("{:<8} {:<42} {:<}", "score", "entity_id", "title");
    for h in &hits {
        println!("{:<8.4} {:<42} {}", h.score, h.entity_id.to_key(), h.title);
    }
    if hits.is_empty() {
        println!("no hits");
    }
    Ok(())
}
