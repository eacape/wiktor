use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::traits::{DataSource, DistanceMetric, DomainConfig, EntityStore, VectorStore};
use wiktor_core::types::{Cursor, PublishStatus};
use wiktor_core::{seed, FactValue, Filters, QueryEngine};

mod commands;
mod compile;
mod embed;
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
    /// Hybrid search: QUG rewrite (optional) → fact-plane filter pushdown →
    /// FTS5 + vector → RRF fusion.
    /// 混合检索：QUG 改写（可选）→ 事实平面过滤下推 → FTS5 + 向量 → RRF 融合。
    Search {
        /// Search text (query)
        /// 检索文本（查询词）
        text: String,
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// domain.yaml path (required for QUG; without it QUG is disabled)
        /// domain.yaml 路径（构建 QUG 需要；不传则 QUG 关闭）
        #[arg(long)]
        domain: Option<PathBuf>,
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
        /// Emit a single JSON object instead of the human-readable table.
        /// 输出单个 JSON 对象而非人类可读表格。
        #[arg(long)]
        json: bool,
        /// Explicitly drop the vector path (FTS only); useful when no vector
        /// service/collection is configured.
        /// 显式关闭向量路径（仅 FTS）；无向量服务/集合时使用。
        #[arg(long)]
        no_vector: bool,
    },
    /// Vector service operations
    /// 向量服务相关操作
    Vector {
        #[command(subcommand)]
        command: VectorCommand,
    },
    /// Compile a data source into Wiki pages via the Step 4 pipeline (explicit
    /// source/provider; stats + exit codes; read-only dry-run).
    /// 经 Step 4 编译管线把数据源编译为 Wiki 页面（显式 source/provider；统计与
    /// 退出码；只读 dry-run）。
    Compile(compile::CompileArgs),
    /// QUG graph operations (Step 5).
    /// QUG 图操作（Step 5）。
    Qug {
        #[command(subcommand)]
        command: QugCommand,
    },
    /// Run the A/B/C golden evaluation and write the report trio (Step 5).
    /// 运行 A/B/C golden 评测并写出三件套报告（Step 5）。
    Eval(commands::eval::EvalArgs),
    /// Feedback loop operations (Step 6): analyze / list / review.
    /// 反馈闭环操作（Step 6）：analyze / list / review。
    Feedback {
        #[command(subcommand)]
        command: commands::feedback::FeedbackCommand,
    },
    /// Start the HTTP + gRPC server (requires the `server` feature).
    /// 启动 HTTP + gRPC 双监听服务（需要 `server` feature）。
    #[cfg(feature = "server")]
    Serve {
        /// SQLite database path.
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// HTTP listen address.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen_http: String,
        /// gRPC listen address.
        #[arg(long, default_value = "127.0.0.1:50051")]
        listen_grpc: String,
        /// Domain pack path used by the compile worker.
        #[arg(long)]
        domain: Option<PathBuf>,
        /// Optional jsonl source override.
        #[arg(long)]
        source: Option<String>,
    },
    /// Domain-pack operations (Step 8): read-only compatibility preflight.
    /// 领域包操作（Step 8）：只读兼容 preflight。
    Domain {
        #[command(subcommand)]
        command: commands::domain::DomainCommand,
    },
    /// Export accepted pages to an external search engine (optional outlet).
    /// 把 accepted 页导出到外部检索引擎（可选出口）。
    Export {
        #[command(subcommand)]
        command: ExportCommand,
    },
    /// Run the Web console (local HTTP; reads the DB read-only). Requires the
    /// `console` feature.
    /// 运行 Web console（本地 HTTP；只读读库）。需要 `console` feature。
    Console {
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// HTTP listen address (default 127.0.0.1:8081)
        /// HTTP 监听地址（默认 127.0.0.1:8081）
        #[arg(long, default_value = "127.0.0.1:8081")]
        listen: String,
        /// Static asset dir (default the repo's docs/console_ui)
        /// 静态资源目录（默认仓库的 docs/console_ui）
        #[arg(long)]
        static_dir: Option<PathBuf>,
    },
    /// Run the TUI console (terminal dashboard; reads the DB read-only).
    /// Requires the `tui` feature.
    /// 运行 TUI console（终端仪表盘；只读读库）。需要 `tui` feature。
    Tui {
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
    },
}

#[derive(Subcommand)]
enum QugCommand {
    /// Derive the five edge types from accepted pages + intents and publish the
    /// graph transactionally (hash reuse; --force / --dry-run; exit codes D7).
    /// 从 accepted 页与意图配置派生五类边并事务发布图（hash 命中复用；
    /// --force / --dry-run；退出码见 D7）。
    Build(commands::qug::BuildArgs),
}

/// `wiktor export` 的外部检索引擎出口（STEP10 B4，D6）。
/// External search-engine outlets of `wiktor export` (STEP10 B4, D6).
#[derive(Subcommand)]
enum ExportCommand {
    /// Mirror accepted pages into Meilisearch (env: WIKTOR_MEILISEARCH_URL /
    /// WIKTOR_MEILISEARCH_API_KEY; index = --index or the domain name). Requires
    /// the `export-meilisearch` feature.
    /// 把 accepted 页镜像到 Meilisearch（env：WIKTOR_MEILISEARCH_URL /
    /// WIKTOR_MEILISEARCH_API_KEY；index = --index 或 domain 名）。需要
    /// `export-meilisearch` feature。
    Meilisearch {
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// domain.yaml path (mandatory)
        /// domain.yaml 路径（必填）
        #[arg(long)]
        domain: PathBuf,
        /// Meilisearch index uid (default: the domain name from domain.yaml)
        /// Meilisearch index uid（默认：domain.yaml 的 domain 名）
        #[arg(long)]
        index: Option<String>,
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
    /// Embed accepted pages and upsert them into the vector store (builds the
    /// vector index; real embeddings via the embedding-http feature or the
    /// deterministic local baseline).
    /// 把 accepted 页面嵌入并写入向量库（构建向量索引；真实嵌入走
    /// embedding-http feature，本地基线用确定性嵌入器）。
    Build {
        /// domain.yaml path (mandatory)
        /// domain.yaml 路径（必填）
        #[arg(long)]
        domain: PathBuf,
        /// SQLite database path (default ./wiktor.db)
        /// SQLite 数据库路径（默认 ./wiktor.db）
        #[arg(long, default_value = "wiktor.db")]
        db: PathBuf,
        /// Vector collection name (default: the domain name from domain.yaml)
        /// 向量 collection 名（默认 domain.yaml 的域名）
        #[arg(long)]
        collection: Option<String>,
        /// Max pages to embed (default: all accepted pages)
        /// 最多嵌入页数（默认全部 accepted 页）
        #[arg(long)]
        limit: Option<usize>,
        /// Use the deterministic local embedder instead of the real HTTP
        /// embedder (offline; useful for tests).
        /// 用确定性本地嵌入器代替真实 HTTP 嵌入器（离线；测试用）。
        #[arg(long)]
        deterministic: bool,
        /// Emit a single JSON summary on stdout.
        /// stdout 输出单个 JSON 摘要。
        #[arg(long)]
        json: bool,
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
            domain,
            filter,
            top_k,
            json,
            no_vector,
        } => {
            cmd_search(
                &db,
                &text,
                domain.as_deref(),
                filter.as_deref(),
                top_k,
                json,
                no_vector,
            )
            .await?
        }
        Command::Vector { command } => match command {
            VectorCommand::Ping { url, api_key } => {
                #[cfg(feature = "vector-qdrant")]
                {
                    let store = wiktor_vector_qdrant::QdrantVectorStore::from_config(
                        &url,
                        api_key.as_deref(),
                        768,
                    )?;
                    let health = store
                        .client()
                        .health_check()
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    println!("qdrant ok: {}", health.version);
                }
                #[cfg(not(feature = "vector-qdrant"))]
                {
                    anyhow::bail!("vector-qdrant feature is disabled (build without --no-default-features to use qdrant)");
                }
            }
            VectorCommand::Build {
                domain,
                db,
                collection,
                limit,
                deterministic,
                json,
            } => {
                cmd_vector_build(
                    &domain,
                    &db,
                    collection.as_deref(),
                    limit,
                    deterministic,
                    json,
                )
                .await?
            }
        },
        Command::Compile(args) => {
            // 退出码契约（§9/D7）：Ok(code) → 按 code 退出；Err → anyhow 默认
            // 退出码 1（数据库/内部运行故障）。
            // Exit-code contract (§9/D7): Ok(code) exits with code; Err takes
            // anyhow's default exit code 1 (database/internal faults).
            finish(compile::run(args).await?)?
        }
        Command::Qug { command } => match command {
            QugCommand::Build(args) => finish(commands::qug::run(args).await?)?,
        },
        Command::Eval(args) => finish(commands::eval::run(args).await?)?,
        Command::Feedback { command } => finish(commands::feedback::run(command).await?)?,
        Command::Domain { command } => finish(commands::domain::run(command).await?)?,
        Command::Export { command } => match command {
            ExportCommand::Meilisearch { db, domain, index } => {
                cmd_export_meilisearch(&db, &domain, index.as_deref()).await?;
            }
        },
        #[cfg(feature = "console")]
        Command::Console {
            db,
            listen,
            static_dir,
        } => {
            wiktor_console::serve(&db, &listen, static_dir).await?;
        }
        #[cfg(not(feature = "console"))]
        Command::Console { .. } => {
            anyhow::bail!(
                "wiktor console requires the console feature (build with --features console)"
            )
        }
        #[cfg(feature = "tui")]
        Command::Tui { db } => {
            wiktor_console::tui::run(&db).await?;
        }
        #[cfg(not(feature = "tui"))]
        Command::Tui { .. } => {
            anyhow::bail!("wiktor tui requires the tui feature (build with --features tui)")
        }
        #[cfg(feature = "server")]
        Command::Serve {
            db,
            listen_http,
            listen_grpc,
            domain,
            source,
        } => {
            wiktor_server::serve::run_server(wiktor_server::serve::ServeOptions {
                db,
                listen_http,
                listen_grpc,
                domain_pack: domain,
                source_path: source,
            })
            .await
            .map_err(|e| anyhow!(e))?;
        }
    }
    Ok(())
}

/// 统一收尾：刷新 stdout 后按命令返回的退出码结束进程（0 = 正常返回）。
/// 返回 Ok(code) 的命令自带退出码（step4 compile 与 Step5 build/eval 的
/// 0/1/2/3/4 契约）；Err 走 anyhow 默认退出码 1。
/// Shared epilogue: flush stdout, then end the process with the command-provided
/// exit code (0 = return normally). Commands returning Ok(code) carry their own
/// exit-code contract (step4 compile and Step5 build/eval's 0/1/2/3/4); Err
/// takes anyhow's default exit code 1.
fn finish(code: i32) -> Result<()> {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Seed knowledge pages + fact-plane data.
/// 导入知识页面与事实平面数据。
///
/// pub(crate)：Step5 批6 的 CLI 集成测试复用同一条 seed 路径灌库，保证测试与
/// 生产 seed 语义一致。
/// pub(crate): the Step5 batch-6 CLI integration tests reuse the same seeding
/// path so tests match the production seed semantics.
pub(crate) async fn cmd_seed(
    db: &Path,
    domain_yaml: &Path,
    pages_dir: Option<&Path>,
) -> Result<()> {
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

/// Build the vector index: embed every accepted page of the domain and upsert
/// into the vector store. Real embeddings come from the HTTP embedder
/// (embedding-http feature, env-configured) unless `--deterministic` forces
/// the local token-hash baseline.
/// 构建向量索引：把域内全部 accepted 页嵌入并写入向量库。默认走真实 HTTP
/// 嵌入器（embedding-http feature，环境变量配置）；`--deterministic` 强制用
/// 本地 token-hash 基线（离线）。
#[cfg(feature = "vector-qdrant")]
async fn cmd_vector_build(
    domain_yaml: &Path,
    db: &Path,
    collection_override: Option<&str>,
    limit: Option<usize>,
    deterministic: bool,
    json: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
    let yaml_text = std::fs::read_to_string(domain_yaml)
        .with_context(|| format!("read {}", domain_yaml.display()))?;
    let config: DomainConfig =
        serde_yaml_ng::from_str(&yaml_text).map_err(|e| anyhow!("parse domain.yaml: {e}"))?;
    let collection = collection_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.name.clone());
    let kernel = SqliteKernel::open(db)?;

    // —— embedder：真实 HTTP（embedding-http feature）或确定性基线 ——
    // —— embedder: the real HTTP one (embedding-http feature) or the
    //    deterministic baseline ——
    #[cfg(feature = "embedding-http")]
    let embedder: Arc<dyn wiktor_core::QueryEmbedder> = if deterministic {
        Arc::new(embed::DeterministicEmbedder::new(embed::DIM))
    } else {
        Arc::new(wiktor_core::embedding::HttpEmbedder::from_env()?)
    };
    #[cfg(not(feature = "embedding-http"))]
    let embedder: Arc<dyn wiktor_core::QueryEmbedder> =
        Arc::new(embed::DeterministicEmbedder::new(embed::DIM));

    let pages = kernel.accepted_page_vectors(&config.name)?;
    let pages: Vec<wiktor_core::kernel::AcceptedPageVector> = match limit {
        Some(n) => pages.into_iter().take(n).collect(),
        None => pages,
    };
    if pages.is_empty() {
        println!("vector build: no accepted pages for domain {}", config.name);
        return Ok(());
    }

    // —— 首次嵌入探测维度 → 建 collection（维度与 generation 对齐
    //    validate_vector_payloads 的 stale 校验）——
    // —— Probe the dimension on the first embed → ensure the collection
    //    (the dimension and generation align with the validate_vector_payloads
    //    staleness check) ——
    let first_text = embed_text(&pages[0]);
    let dim = embedder.embed(&first_text).await?.len();

    // qdrant 地址/key 与 vector ping 同源（环境变量）。CLI 恒开
    // vector-qdrant feature（wiktor-vector-qdrant 插件依赖带上），store 恒为
    // QdrantVectorStore。
    // The qdrant address/key share the same env sources as `vector ping`. The CLI
    // always enables the vector-qdrant feature (via the wiktor-vector-qdrant
    // plugin dependency), so the store is always QdrantVectorStore.
    let url =
        std::env::var("WIKTOR_QDRANT_URL").unwrap_or_else(|_| "http://127.0.0.1:6334".to_string());
    let api_key = std::env::var("WIKTOR_QDRANT_API_KEY").ok();
    let store = Arc::new(wiktor_vector_qdrant::QdrantVectorStore::from_config(
        &url,
        api_key.as_deref(),
        dim,
    )?);

    store
        .ensure_collection(&collection, dim, DistanceMetric::Cosine)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let mut upserted = 0usize;
    let mut points = Vec::new();
    for (idx, page) in pages.iter().enumerate() {
        let text = if idx == 0 {
            first_text.clone()
        } else {
            embed_text(page)
        };
        let vector = embedder.embed(&text).await?;
        // point id 必须是 UUID（Step1 约定：BLAKE3 派生，防 page_id 非 UUID 撞
        // qdrant 校验）；与向量重建/幂等覆盖同源同式。
        // The point id must be a UUID (the Step1 convention: BLAKE3-derived, so
        // non-UUID page_ids never trip qdrant validation); the same source and
        // shape as vector rebuild/idempotent overwrite.
        let entity = wiktor_core::types::EntityId::from_key(&page.entity_id)?;
        let point_id =
            wiktor_vector_qdrant::QdrantVectorStore::point_id(&entity, "summary", page.generation);
        points.push(wiktor_core::traits::VectorPoint {
            id: point_id,
            vector,
            metadata: wiktor_core::traits::VectorMetadata {
                entity_id: page.entity_id.clone(),
                page_id: page.page_id.clone(),
                chunk_type: wiktor_core::traits::ChunkType::Summary,
                content_hash: page.content_hash.clone(),
                generation: page.generation,
            },
        });
        upserted += 1;
    }
    store
        .upsert(&collection, &points)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    if json {
        let summary = serde_json::json!({
            "collection": collection,
            "pages": upserted,
            "dimension": dim,
            "embedder": if deterministic { "deterministic" } else { "http" },
            "backend": "qdrant",
        });
        println!("{summary}");
    } else {
        println!(
            "vector build: collection={collection} pages={upserted} dimension={dim} elapsed={:?}",
            started.elapsed()
        );
    }
    Ok(())
}

/// vector build 需要 qdrant 向量后端插件；无 `vector-qdrant` feature 时明确
/// 报错（而非静默回退）。
/// `vector build` requires the qdrant vector-backend plugin; without the
/// `vector-qdrant` feature this fails explicitly (rather than silently falling
/// back).
#[cfg(not(feature = "vector-qdrant"))]
async fn cmd_vector_build(
    _domain_yaml: &Path,
    _db: &Path,
    _collection_override: Option<&str>,
    _limit: Option<usize>,
    _deterministic: bool,
    _json: bool,
) -> Result<()> {
    anyhow::bail!(
        "vector build requires the vector-qdrant feature (enable it; don't build with --no-default-features)"
    )
}

/// 把 accepted 页镜像到 Meilisearch（`wiktor export meilisearch`，STEP10 B4）。
/// Requires the `export-meilisearch` feature (off by default; the external engine
/// is an optional outlet).
#[cfg(feature = "export-meilisearch")]
async fn cmd_export_meilisearch(db: &Path, domain_yaml: &Path, index: Option<&str>) -> Result<()> {
    let yaml_text = std::fs::read_to_string(domain_yaml)
        .with_context(|| format!("read {}", domain_yaml.display()))?;
    let config: DomainConfig =
        serde_yaml_ng::from_str(&yaml_text).map_err(|e| anyhow!("parse domain.yaml: {e}"))?;
    let index = index
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.name.clone());
    let kernel = SqliteKernel::open(db)?;
    let pages = kernel.accepted_page_vectors(&config.name)?;
    let exporter = wiktor_adapter_meilisearch::MeilisearchExporter::from_env(&index)
        .map_err(|e| anyhow!("meilisearch exporter: {e}"))?;
    exporter
        .ensure_index()
        .await
        .map_err(|e| anyhow!("ensure index: {e}"))?;
    let n = exporter
        .export_pages(&pages)
        .await
        .map_err(|e| anyhow!("export pages: {e}"))?;
    println!("export meilisearch: index={index} pages={n}");
    Ok(())
}

#[cfg(not(feature = "export-meilisearch"))]
async fn cmd_export_meilisearch(_db: &Path, _domain: &Path, _index: Option<&str>) -> Result<()> {
    anyhow::bail!("export meilisearch requires the export-meilisearch feature (build with --features export-meilisearch)")
}

/// 页面向量嵌入文本（title + body，与查询侧 terms 拼接同一素材面）。
/// The text embedded per page (title + body; the same material surface as the
/// query-side term join).
fn embed_text(page: &wiktor_core::kernel::AcceptedPageVector) -> String {
    format!("{}\n{}", page.title, page.content)
}

/// Search through the QueryEngine: QUG rewrite (optional) → filter pushdown →
/// FTS5 + vector → RRF fusion.
/// 经 QueryEngine 检索：QUG 改写（可选）→ 过滤下推 → FTS5 + 向量 → RRF 融合。
async fn cmd_search(
    db: &Path,
    text: &str,
    domain_yaml: Option<&Path>,
    filter_spec: Option<&str>,
    top_k: usize,
    json: bool,
    _no_vector: bool,
) -> Result<()> {
    let kernel = Arc::new(SqliteKernel::open(db)?);
    let filters = match filter_spec {
        Some(spec) => filter::parse_filter(spec)?,
        None => Filters::empty(),
    };

    // 向量路径：--no-vector 也建空集合（引擎向量路返回 0 命中、RRF 只剩 FTS，
    // 诊断仍标记 vector=0），避免 Mock 对缺失集合报错。
    // Vector path: `--no-vector` still creates an empty collection (the vector path
    // yields 0 hits and RRF falls back to FTS-only, diagnostics keep vector=0),
    // so the Mock never errors on a missing collection.
    let vector_store = Arc::new(MockVectorStore::new());
    vector_store
        .ensure_collection("milk-tea", embed::DIM, DistanceMetric::Cosine)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let embedder = Arc::new(embed::DeterministicEmbedder::new(embed::DIM));

    // QUG 图（Step5 批3）：优先从持久化 active published build 加载（审计代次，
    // 含 seed 与 Step4 编译页边）；无 active / stale / 加载失败 → qug=None，查询
    // 诊断写 disabled/stale 并显式走混合 fallback。绝不静默用 seed-wiki 内存重建
    // 图顶替持久化图（A7：不得旧图临时顶替；且 seed-wiki 内存路径不含 Step4 编
    // 译页边，会与审计代次漂移）。intents.yaml 以原始 bytes 冻结传入，查询路径
    // 不解析 YAML。
    // QUG graph (Step5 batch 3): load from the persisted active published build
    // first (the audited generation, covering both seed and Step4 compiled-page
    // edges); missing/stale/failed load → qug=None, query diagnostics report
    // disabled/stale and hybrid retrieval is the explicit fallback. Never
    // silently rebuild the graph in memory from seed-wiki over the persisted one
    // (A7: the stale graph must never be substituted, and the seed-wiki memory
    // path lacks Step4 compiled-page edges so it would drift from the audited
    // generation). intents.yaml is frozen and passed in as raw bytes; the query
    // path never parses YAML.
    let (engine, qug_enabled) = match domain_yaml {
        Some(path) => {
            let config = load_domain_config(path)?;
            let domain_dir = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let intents_bytes = load_intents_bytes(&config, &domain_dir)?;
            let enabled = config.qug.enabled;
            let engine = QueryEngine::with_persistent_qug(
                kernel.clone(),
                vector_store.clone(),
                &config,
                &intents_bytes,
                embedder,
                "milk-tea",
                60,
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            (engine, enabled)
        }
        None => (
            QueryEngine::new(
                kernel.clone(),
                vector_store.clone(),
                None,
                embedder,
                "milk-tea",
                5,
                60,
            )
            .map_err(|e| anyhow::anyhow!(e.to_string()))?,
            false,
        ),
    };

    // Step 6 批2（D10）：CLI search 注入默认过滤放宽器——带过滤滤空时至多
    // 放宽一次并重新下推重试，滤空/放宽三状态随查询日志落库。引擎默认不装
    // 配（A9：无 relaxer 触发不了），装配是 CLI 的显式决定。
    // Step 6 batch 2 (D10): CLI search injects the default filter relaxer — a
    // filtered-empty with filters relaxes at most once and retries the
    // pushdown, with the filter-empty/relaxation state triple persisted in the
    // query log. The engine never auto-installs one (A9: without a relaxer
    // nothing can trigger); installing is the CLI's explicit decision.
    let engine = engine.with_filter_relaxer(Arc::new(
        wiktor_core::query_engine::DefaultFilterRelaxer::new(),
    ));

    // 运维提示（不改变退出语义）：QUG 不可用时的原因线索；`wiktor qug build`
    // 由 Step5 CLI 批次提供。
    // Ops hint (does not change exit semantics): why QUG is unavailable; the
    // `wiktor qug build` command arrives with the Step5 CLI batch.
    let qug_hint = if engine.qug_stale {
        Some("active QUG build is stale (source changed); run `wiktor qug build` to refresh")
    } else if qug_enabled && engine.qug.is_none() {
        Some("no active QUG build; run `wiktor qug build` to publish one")
    } else {
        None
    };

    let query = wiktor_core::types::Query {
        text: text.to_string(),
        filters,
        top_k,
        domain: Some("milk-tea".into()),
    };
    let result = engine
        .search(&query)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    if json {
        #[derive(serde::Serialize)]
        struct JsonOut<'a> {
            query: &'a str,
            hits: Vec<JsonHit<'a>>,
            rewritten: Option<wiktor_core::types::RewrittenQuery>,
            rewrite_failure: bool,
            diagnostics: wiktor_core::query_engine::QueryDiagnostics,
            latency_ms: u64,
        }
        #[derive(serde::Serialize)]
        struct JsonHit<'a> {
            page_id: &'a str,
            entity_id: String,
            score: f32,
            title: &'a str,
        }
        let out = JsonOut {
            query: text,
            hits: result
                .hits
                .iter()
                .map(|h| JsonHit {
                    page_id: &h.page_id,
                    entity_id: h.entity_id.to_key(),
                    score: h.score,
                    title: &h.title,
                })
                .collect(),
            rewritten: result.rewritten.clone(),
            rewrite_failure: result.rewrite_failure,
            diagnostics: result.diagnostics.clone(),
            latency_ms: result.latency_ms,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".into())
        );
        return Ok(());
    }

    // 人类可读展示（Step 3 §8）
    // Human-readable display (Step 3 §8)
    println!("query: {text}");
    let status = match result.diagnostics.rewrite_status {
        wiktor_core::query_engine::RewriteStatus::Applied => "applied",
        wiktor_core::query_engine::RewriteStatus::Fallback => "fallback (no matching QUG path)",
        wiktor_core::query_engine::RewriteStatus::Disabled => "disabled",
        wiktor_core::query_engine::RewriteStatus::Stale => "stale (active QUG build out of date)",
    };
    println!("rewrite: {status}");
    if let Some(hint) = &qug_hint {
        println!("hint: {hint}");
    }
    if let Some(r) = &result.rewritten {
        println!("expanded_terms: {}", r.expanded_terms.join(", "));
    } else {
        println!("expanded_terms: {text}");
    }
    if result.diagnostics.applied_filters.is_empty() {
        println!("filters: none");
    } else {
        let parts: Vec<String> = result
            .diagnostics
            .applied_filters
            .conditions
            .iter()
            .map(filter::format_condition)
            .collect();
        println!("filters: {}", parts.join("; "));
    }
    let candidates = if result.diagnostics.candidate_count == 0
        && result.diagnostics.applied_filters.is_empty()
    {
        "all".to_string()
    } else {
        result.diagnostics.candidate_count.to_string()
    };
    println!(
        "candidates: {candidates}  fts: {}  vector: {}  rrf_k: {}",
        result.diagnostics.fts_count, result.diagnostics.vector_count, result.diagnostics.rrf_k
    );
    println!("{:<8} {:<42} {:<}", "score", "entity_id", "title");
    for h in &result.hits {
        println!("{:<8.4} {:<42} {}", h.score, h.entity_id.to_key(), h.title);
    }
    if result.hits.is_empty() {
        println!("no hits");
    }
    Ok(())
}

/// 加载并解析 domain.yaml。
/// Loads and parses domain.yaml.
fn load_domain_config(path: &Path) -> Result<DomainConfig> {
    let yaml_text =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_yaml_ng::from_str(&yaml_text).map_err(|e| anyhow!("parse domain.yaml: {e}"))
}

/// 读取 intents.yaml 原始 bytes（qug.intent_templates 指向的文件；相对 domain
/// 目录解析）。Step5 批3 查询路径冻结原文传给持久化加载器（参与 source_hash），
/// 不再在查询侧解析 YAML（spec §4.4）。
/// Reads the raw intents.yaml bytes (the file pointed to by qug.intent_templates,
/// resolved relative to the domain directory). The Step5 batch-3 query path
/// freezes the raw bytes for the persistent loader (they participate in
/// source_hash) and no longer parses YAML on the query side (spec §4.4).
fn load_intents_bytes(config: &DomainConfig, domain_dir: &Path) -> Result<Vec<u8>> {
    match &config.qug.intent_templates {
        Some(rel) => {
            let path = domain_dir.join(rel);
            std::fs::read(&path).with_context(|| format!("read intents.yaml {}", path.display()))
        }
        None => Ok(Vec::new()),
    }
}
