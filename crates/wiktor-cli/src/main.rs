use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::qug::build_qug_from_wiki;
use wiktor_core::traits::{
    DataSource, DistanceMetric, DomainConfig, EntityStore, IntentConfig, VectorStore,
};
use wiktor_core::types::{Cursor, PublishStatus};
use wiktor_core::{seed, FactValue, Filters, QueryEngine};

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
        Command::Compile(args) => {
            // 退出码契约（§9）：Ok(code) → 按 code 退出；Err → anyhow 默认
            // 退出码 1（数据库/内部运行故障）。
            // Exit-code contract (§9): Ok(code) exits with code; Err takes
            // anyhow's default exit code 1 (database/internal faults).
            let code = compile::run(args).await?;
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            if code != 0 {
                std::process::exit(code);
            }
        }
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

    // QUG 图（可选）：从 domain.yaml + intents.yaml + seed-wiki 页面构建。
    // `qug.enabled=false` 时禁用（传 None → 诊断状态 Disabled）。
    // QUG graph (optional): built from domain.yaml + intents.yaml + seed-wiki pages.
    // `qug.enabled=false` disables it (None → diagnostics report Disabled).
    let (qug, candidate_multiplier) = match domain_yaml {
        Some(path) => {
            let config = load_domain_config(path)?;
            let domain_dir = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let intents = load_intents(&config, &domain_dir)?;
            let wiki_pages = load_seed_pages(&domain_dir.join("seed-wiki"))?;
            let built = build_qug_from_wiki(&wiki_pages, &config, &intents)?;
            let mult = config.qug.candidate_multiplier;
            let qug = if config.qug.enabled {
                Some(built.graph)
            } else {
                None
            };
            (qug, mult)
        }
        None => (None, 5),
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
    let engine = QueryEngine::new(
        kernel.clone(),
        vector_store.clone(),
        qug,
        embedder,
        "milk-tea",
        candidate_multiplier,
        60,
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

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
    };
    println!("rewrite: {status}");
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

/// 加载并解析 intents.yaml（qug.intent_templates 指向的文件；相对 domain 目录解析）。
/// Loads and parses intents.yaml (the file pointed to by qug.intent_templates;
/// resolved relative to the domain directory).
fn load_intents(config: &DomainConfig, domain_dir: &Path) -> Result<IntentConfig> {
    match &config.qug.intent_templates {
        Some(rel) => {
            let path = domain_dir.join(rel);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read intents.yaml {}", path.display()))?;
            serde_yaml_ng::from_str(&text).map_err(|e| anyhow!("parse intents.yaml: {e}"))
        }
        None => Ok(IntentConfig {
            version: "0.0.0".into(),
            intents: Vec::new(),
        }),
    }
}

/// 读取 seed-wiki 目录下全部 Markdown 页面。
/// Reads all Markdown pages in the seed-wiki directory.
fn load_seed_pages(dir: &Path) -> Result<Vec<wiktor_core::types::WikiPage>> {
    let mut md_files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read seed-wiki dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();
    let mut pages = Vec::new();
    for path in &md_files {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        pages.push(
            seed::parse_page(&content)
                .with_context(|| format!("parse seed page {}", path.display()))?,
        );
    }
    Ok(pages)
}
