//! `wiktor compile`（Step 4 spec §9，决策 D7）：显式选择 source/provider 的编译
//! 入口；统计与退出码契约（A20），dry-run 完全只读（不建库、不迁移、不写事实/
//! 任务、不占预算、不请求模型），输入边界与退出码优先级 1>2>3>4>0（A23）。
//!
//! `wiktor compile` (Step 4 spec §9, decision D7): the compile entry point with
//! an explicit source/provider choice; statistics and exit-code contract (A20),
//! fully read-only dry-run (no DB creation/migration, no facts/tasks, no budget,
//! no model calls), input boundaries and exit-code priority 1>2>3>4>0 (A23).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Args;
use wiktor_core::compile::compatibility::COMPATIBILITY_REJECTED_PREFIX;
use wiktor_core::compile::config::{
    build_context, CompilePolicy, CompileStats, RunOptions, SystemClock,
};
use wiktor_core::compile::consistency::{
    ConsistencyArbiter, SourceRefConsistencyArbiter, SqliteFtsCandidateProvider,
};
use wiktor_core::compile::contract::{system_prompt, DefaultSourceRefValidator};
use wiktor_core::compile::executor::PipelineExecutor;
use wiktor_core::compile::mock::MockCompiler;
use wiktor_core::compile::quality::RuleBasedScorer;
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::{Compiler, DomainConfig, EntityConfig};
use wiktor_core::types::error::Error;

/// 退出码：0 全完成且无 failed/quarantined/deferred。
/// Exit code: everything finalized with no failed/quarantined/deferred.
const EXIT_OK: i32 = 0;
/// 退出码：2 参数/配置/输入文件协议错误。
/// Exit code: parameter/config/input-file protocol error.
const EXIT_INPUT: i32 = 2;
/// 退出码：3 存在 failed 或 quarantined。
/// Exit code: at least one failed or quarantined outcome.
const EXIT_FAILURES: i32 = 3;
/// 退出码：4 仅预算/租约/退避导致 deferred。
/// Exit code: only budget/lease/backoff deferrals remain.
const EXIT_DEFERRED: i32 = 4;

/// 编译 provider（§9：openai 默认；无 `llm-openai` feature 时 openai/ollama
/// 明确报错，绝不偷偷降级 Mock）。
/// Compile provider (§9: openai by default; without the `llm-openai` feature
/// openai/ollama error out explicitly — never a silent downgrade to Mock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProviderArg {
    /// OpenAI-compatible API (default endpoint https://api.openai.com/v1)
    /// OpenAI 兼容 API（默认端点 https://api.openai.com/v1）
    #[value(name = "openai")]
    OpenAi,
    /// ollama local server (default endpoint http://127.0.0.1:11434/v1; no key)
    /// ollama 本地服务（默认端点 http://127.0.0.1:11434/v1；无需 key）
    #[value(name = "ollama")]
    Ollama,
    /// Offline deterministic mock compiler (no key, no network)
    /// 离线确定性 Mock 编译器（无 key、无网络）
    #[value(name = "mock")]
    Mock,
}

/// `wiktor compile` 参数（§9 参数表全量）。
/// `wiktor compile` arguments (the full §9 parameter table).
#[derive(Debug, Args)]
pub struct CompileArgs {
    /// domain.yaml path (mandatory); relative source/prompt paths resolve against its directory
    /// domain.yaml 路径（必填）；相对 source/prompt 路径基于其目录解析
    #[arg(long)]
    pub domain: PathBuf,
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// entities[].name to compile (required when the pack declares several)
    /// 待编译的 entities[].name（多实体配置时必填）
    #[arg(long)]
    pub entity: Option<String>,
    /// Override the selected entity's source; only jsonl:// is supported in this step
    /// 覆盖所选实体的 source；本步仅支持 jsonl://
    #[arg(long)]
    pub data_source: Option<String>,
    /// Compile provider: openai (default) / ollama / mock
    /// 编译 provider：openai（默认）/ ollama / mock
    #[arg(long, value_enum, default_value_t = ProviderArg::OpenAi)]
    pub provider: ProviderArg,
    /// Model version (required for openai/ollama; mock defaults to mock-v1); written into model_version
    /// 模型版本（openai/ollama 必填；mock 默认 mock-v1）；写入 model_version
    #[arg(long)]
    pub model: Option<String>,
    /// Embedding model recorded in the content hash (default "none"; never runs embedding)
    /// 记入 content hash 的 embedding 模型（默认 none；设置不会执行 embedding）
    #[arg(long, default_value = "none")]
    pub embedding_model: String,
    /// Override the provider's default compatible endpoint (credentials are never logged)
    /// 覆盖 provider 默认兼容端点（凭据绝不打印到日志）
    #[arg(long)]
    pub base_url: Option<String>,
    /// Max source entities scanned this run (1..=10000, default 1000)
    /// 本次扫描的源实体上限（1..=10000，默认 1000）
    #[arg(long, default_value_t = 1000)]
    pub limit: usize,
    /// Fetch batch size (1..=128, default 32)
    /// fetch 批量大小（1..=128，默认 32）
    #[arg(long, default_value_t = 32)]
    pub batch_size: usize,
    /// Re-queue even when the accepted hash matches (never bypasses quality/CAS/budget/revision conflicts)
    /// 同 hash 也重新排队（不绕过质量/CAS/预算/revision 冲突）
    #[arg(long)]
    pub force: bool,
    /// Read-only plan: no DB creation/migration/writes, no budget, no model calls
    /// 只读计划：不建库/迁移/写入、不占预算、不请求模型
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the compatibility preflight; only allowed on an empty database with
    /// no old artifact — any existing compile data still exits 3 (fail-closed)
    /// 跳过兼容 preflight；仅允许空数据库/无旧 artifact 时跳过——发现任何旧
    /// 数据仍以退出码 3 结束（fail-closed）
    #[arg(long)]
    pub skip_compatibility_check: bool,
    /// Override compile.batch_token_budget (positive integer)
    /// 覆盖 compile.batch_token_budget（正整数）
    #[arg(long)]
    pub batch_token_budget: Option<u64>,
    /// Override compile.task_token_budget (positive integer)
    /// 覆盖 compile.task_token_budget（正整数）
    #[arg(long)]
    pub task_token_budget: Option<u64>,
    /// Override compile.daily_token_budget (positive integer; existing day limits cannot be raised)
    /// 覆盖 compile.daily_token_budget（正整数；已有日限额不能提高）
    #[arg(long)]
    pub daily_token_budget: Option<u64>,
    /// Emit a single JSON CompileStats object on stdout (human logs go to stderr)
    /// stdout 输出单一 JSON CompileStats（人类日志走 stderr）
    #[arg(long)]
    pub json: bool,
}

/// 命令入口：返回进程退出码（main 据此 `std::process::exit`）。
/// Command entry: returns the process exit code (main calls
/// `std::process::exit` with it).
pub async fn run(args: CompileArgs) -> Result<i32> {
    init_tracing();
    let domain_dir = args
        .domain
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    // —— 配置（§9：--domain 必填；解析失败/配置非法 → 退出码 2）——
    // —— Config (§9: --domain mandatory; parse/config errors → exit code 2) ——
    let yaml_text = match std::fs::read_to_string(&args.domain) {
        Ok(t) => t,
        Err(e) => return input_error(format!("read {}: {e}", args.domain.display())),
    };
    let config: DomainConfig = match serde_yaml_ng::from_str(&yaml_text) {
        Ok(c) => c,
        Err(e) => return input_error(format!("parse {}: {e}", args.domain.display())),
    };

    // —— 实体选择（§9：--entity 多实体必填，单实体自动选）——
    // —— Entity selection (§9: --entity required for multi-entity packs; a
    //     single-entity pack auto-selects) ——
    let mut entity_cfg: EntityConfig = match &args.entity {
        Some(name) => match config.entities.iter().find(|e| e.name == *name) {
            Some(e) => e.clone(),
            None => {
                return input_error(format!(
                    "entity {name:?} not found in {}; declared: {:?}",
                    args.domain.display(),
                    config
                        .entities
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                ));
            }
        },
        None => match config.entities.len() {
            0 => {
                return input_error(format!(
                    "domain pack {} declares no entities",
                    args.domain.display()
                ));
            }
            1 => config.entities[0].clone(),
            n => {
                return input_error(format!(
                    "--entity is required: {n} entities declared ({:?})",
                    config
                        .entities
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                ));
            }
        },
    };
    // —— data-source 覆盖（§9：本步仅 jsonl://，不猜测 schema）——
    // —— data-source override (§9: jsonl:// only in this step; no schema guessing) ——
    if let Some(ds) = &args.data_source {
        if !ds.starts_with("jsonl://") {
            return input_error(format!(
                "--data-source {ds:?} is not supported in this step; only jsonl:// URIs are"
            ));
        }
        entity_cfg.source = ds.clone();
    }

    // —— provider/model（§9：真实模型必填；mock 默认 mock-v1；dry-run 不要求
    //     key/连通但校验模型名与配置）——
    // —— provider/model (§9: real providers require --model; mock defaults to
    //     mock-v1; dry-run needs no key/connectivity but validates model+config) ——
    let model = match args.provider {
        ProviderArg::Mock => args.model.clone().unwrap_or_else(|| "mock-v1".to_string()),
        ProviderArg::OpenAi | ProviderArg::Ollama => match &args.model {
            Some(m) => m.clone(),
            None => {
                return input_error(format!(
                    "--model is required for the {:?} provider",
                    args.provider
                ));
            }
        },
    };

    // —— 策略：domain.compile 段 + CLI 预算覆盖（§9：正整数；已有日限额不能
    //     提高——由 kernel claim 的 MIN 语义强制）——
    // —— Policy: the domain.compile section plus CLI budget overrides (§9:
    //     positive integers; an existing day limit can never be raised — enforced
    //     by the kernel claim's MIN semantics) ——
    let mut policy: CompilePolicy = config.compile_policy.clone();
    if let Some(v) = args.batch_token_budget {
        policy.batch_token_budget = v;
    }
    if let Some(v) = args.task_token_budget {
        policy.task_token_budget = v;
    }
    if let Some(v) = args.daily_token_budget {
        policy.daily_token_budget = Some(v);
    }
    if let Err(e) = policy.validate() {
        return input_error(e);
    }
    // —— Step8 §5.1/§6.4：领域身份五元组（strict semver 已在解析期验证；
    //     schema/prompt 版本随 ctx 进入 content_hash 并随 dependencies_json
    //     持久化，兼容 preflight 的实际消费在 B5）——
    // —— Step8 §5.1/§6.4: the domain-identity five-tuple (strict semver already
    //     validated at parse time; the schema/prompt versions enter the
    //     content_hash with ctx and ride dependencies_json; the actual
    //     preflight consumption lands in B5) ——
    let identity = match config.identity() {
        Ok(i) => i,
        Err(e) => return input_error(e),
    };

    // —— 编译上下文：Prompt = compile.prompt 文件（相对 domain 目录）或内置
    //     source-ref-v1 模板；Prompt bytes 参与 content_hash（§3.1/§7）——
    // —— Compile context: the prompt is the compile.prompt file (relative to the
    //     domain directory) or the built-in source-ref-v1 template; prompt bytes
    //     join the content_hash (§3.1/§7) ——
    let prompt_template = match &config.compile_prompt {
        Some(rel) => {
            let path = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                domain_dir.join(rel)
            };
            match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => return input_error(format!("read prompt {}: {e}", path.display())),
            }
        }
        None => system_prompt(),
    };
    let ctx = build_context(
        &config.version,
        &prompt_template,
        &model,
        &args.embedding_model,
        config.quality_threshold,
        config.compile_output_contract == "require_source_refs",
        identity
            .schema_version
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        identity
            .prompt_version
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
    );

    // —— 数据源（schema 必填/类型规则沿用 raw_to_facts；有界读取见 data/jsonl）——
    // —— Data source (required-field/type rules via raw_to_facts; bounded reads
    //     live in data/jsonl) ——
    let source = match JsonlDataSource::from_config(&entity_cfg, &domain_dir) {
        Ok(s) => s,
        Err(e) => return input_error(e),
    };

    // —— provider 实现（§4/D7：缺 key 是配置错误；无 feature 明确报错；两类
    //     错误都按 §9 记为退出码 2）——
    // —— Provider implementation (§4/D7: a missing key is a config error; a
    //     missing feature errors explicitly; both count as exit code 2 per §9) ——
    let compiler: Arc<dyn Compiler> = match args.provider {
        ProviderArg::Mock => Arc::new(MockCompiler::new(policy.clone())),
        ProviderArg::OpenAi | ProviderArg::Ollama => {
            match build_llm_compiler(&args, policy.clone()) {
                Ok(c) => c,
                Err(e) => return input_error(e),
            }
        }
    };

    // —— kernel（§9 dry-run 契约）：不存在 → 内存空库计划（不建文件）；存在 →
    //     open_existing 只读 inspect（旧 schema 报 migration_required 不升级）；
    //     真实 run 才走会迁移的 SqliteKernel::open（DB 故障 → 退出码 1）——
    // —— Kernel (§9 dry-run contract): missing file → in-memory empty plan (no
    //     file created); existing → open_existing read-only inspect (old schema
    //     reports migration_required without upgrading); only real runs use the
    //     migrating SqliteKernel::open (DB faults → exit code 1) ——
    let kernel = if args.dry_run {
        if args.db.exists() {
            match SqliteKernel::open_existing(&args.db) {
                Ok(k) => Arc::new(k),
                Err(e) => return input_error(e),
            }
        } else {
            // 空库计划：内存库，绝不创建文件（A20）。
            // Empty-DB plan: in-memory, never creating a file (A20).
            Arc::new(SqliteKernel::open_in_memory()?)
        }
    } else {
        Arc::new(SqliteKernel::open(&args.db)?)
    };

    // —— Step8 §7（批 B5）：--skip-compatibility-check 只允许空数据库/无旧
    //     artifact 时跳过重复检查；发现任何旧数据 fail-closed 以退出码 3 结束
    //     （进入 run 之前，零写入）。空库时以关闭 preflight 的策略运行（等价于
    //     跳过必然为空的检查）；该开关绝不能绕过不兼容结果（D11）。
    // —— Step8 §7 (batch B5): --skip-compatibility-check may only skip the
    //     repeated check on an empty database with no old artifact; any existing
    //     compile data fails closed with exit code 3 (before the run, zero
    //     writes). On an empty database the policy runs with the preflight
    //     toggled off (equivalent to skipping the necessarily-empty check); the
    //     flag can never bypass an incompatible result (D11).
    if args.skip_compatibility_check {
        let has_old = match kernel.has_existing_compile_data() {
            Ok(v) => v,
            Err(e) => return classify_run_error(e),
        };
        if has_old {
            return compatibility_rejected(format!(
                "--skip-compatibility-check refused: {} already holds compile data \
                 (accepted pages or pending/running/dead tasks)",
                args.db.display()
            ));
        }
        policy.compatibility_preflight = false;
    }

    // —— 执行（§3）：executor 内部把每个 scanned 实体归入唯一终态分类 ——
    // —— Execution (§3): the executor files every scanned entity into exactly
    //     one final classification ——
    // Step8 §4（上层拍板）：`consistency.enabled` 时装配默认确定性仲裁器与
    // SQLite FTS 有界候选提供器；否则保持 None 路径（不仲裁，consistency 列
    // NULL）。
    // Step8 §4 (upstream ruling): with `consistency.enabled` the default
    // deterministic arbiter and the bounded SQLite FTS candidate provider are
    // wired; otherwise the None path is kept (no arbitration, NULL consistency
    // column).
    let consistency_enabled = policy.consistency.enabled;
    let mut executor = PipelineExecutor::new(
        kernel.clone(),
        compiler,
        Arc::new(RuleBasedScorer::new()),
        Arc::new(DefaultSourceRefValidator::new()),
        Arc::new(SystemClock),
        policy,
    );
    if consistency_enabled {
        executor = executor
            .with_consistency_arbiter(build_cli_consistency_arbiter())
            .with_candidate_provider(Arc::new(SqliteFtsCandidateProvider::new(kernel)));
    }
    let options = RunOptions {
        limit: args.limit,
        batch_size: args.batch_size,
        force: args.force,
        dry_run: args.dry_run,
    };
    let stats: CompileStats = match executor.run(&source, &ctx, options).await {
        Ok(s) => s,
        Err(e) => return classify_run_error(e),
    };

    print_stats(&stats, args.json);
    Ok(exit_code(&stats))
}

/// 组装 OpenAI/ollama 编译器（feature-gated；base_url 覆盖默认端点；key 只从
/// `WIKTOR_OPENAI_API_KEY` 读取，绝不打印）。
/// Assembles the OpenAI/ollama compiler (feature-gated; base_url overrides the
/// default endpoint; the key comes only from `WIKTOR_OPENAI_API_KEY` and is
/// never logged).
#[cfg(feature = "llm-openai")]
fn build_llm_compiler(args: &CompileArgs, policy: CompilePolicy) -> Result<Arc<dyn Compiler>> {
    use wiktor_core::compile::llm::{LlmCompiler, OpenAiLlmClient, API_KEY_ENV};

    let ollama = args.provider == ProviderArg::Ollama;
    // D7：生产 provider 缺 key 是配置错误（mock 需显式选择）；dry-run 不要求 key。
    // D7: a missing key on a production provider is a config error (Mock must be
    // chosen explicitly); dry-run never requires the key.
    if !args.dry_run && !ollama {
        let has_key = std::env::var(API_KEY_ENV).is_ok_and(|k| !k.trim().is_empty());
        if !has_key {
            return Err(anyhow!(
                "provider openai requires {API_KEY_ENV} to be set (use --provider mock for offline runs)"
            ));
        }
    }
    let client = OpenAiLlmClient::new(model_name(args), args.base_url.clone(), None, ollama)?;
    Ok(Arc::new(LlmCompiler {
        client: Arc::new(client),
        policy,
    }))
}

/// Step14 P3-A：装配 CLI 的一致性仲裁器（env 驱动）。`WIKTOR_CONSISTENCY_LLM=1`
/// 且设了非空 `WIKTOR_LLM_BASE_URL` 时用 LLM 仲裁（模型 `WIKTOR_LLM_MODEL`，
/// 缺省 qwen3.8-max）；否则回退确定性 `SourceRefConsistencyArbiter`（离线绿）。
/// 与编译器不同，一致性仲裁是**新增面**，采用显式 opt-in（而非设 URL 即启用），
/// 避免意外引入网络裁决。
/// Step14 P3-A: assembles the CLI consistency arbiter (env-driven). With
/// `WIKTOR_CONSISTENCY_LLM=1` AND a non-empty `WIKTOR_LLM_BASE_URL` it uses the
/// LLM arbiter (model `WIKTOR_LLM_MODEL`, defaulting to qwen3.8-max); otherwise
/// it falls back to the deterministic `SourceRefConsistencyArbiter` (offline
/// green). Unlike the compiler, consistency arbitration is a **new surface** and
/// uses an explicit opt-in (rather than enabling on URL presence), to avoid
/// accidentally introducing network-based arbitration.
#[cfg(feature = "llm-openai")]
fn build_cli_consistency_arbiter() -> Arc<dyn ConsistencyArbiter> {
    use wiktor_core::compile::llm::{OpenAiLlmClient, API_KEY_ENV};

    let opt_in = std::env::var("WIKTOR_CONSISTENCY_LLM")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let base_url = std::env::var("WIKTOR_LLM_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    if opt_in {
        if let Some(url) = base_url {
            let model = std::env::var("WIKTOR_LLM_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "qwen3.8-max".to_string());
            let api_key = std::env::var(API_KEY_ENV)
                .ok()
                .filter(|k| !k.trim().is_empty());
            if let Ok(client) = OpenAiLlmClient::new(model.clone(), Some(url), api_key, false) {
                return Arc::new(wiktor_core::compile::LlmConsistencyArbiter::new(
                    Arc::new(client),
                    model,
                    512,
                ));
            }
            // 客户端构造失败（如坏 URL）→ 记日志后回退确定性，不 panic、不中断编译。
            // Client construction failure (e.g. a bad URL) → log then fall back to
            // the deterministic arbiter, never panicking or aborting the compile.
            eprintln!(
                "wiktor: consistency LLM arbiter construction failed; falling back to deterministic"
            );
        }
    }
    Arc::new(SourceRefConsistencyArbiter::new())
}

/// 无 `llm-openai` feature：仅确定性仲裁器（LLM 仲裁不可用）。
/// Without the `llm-openai` feature: only the deterministic arbiter (no LLM
/// arbitration).
#[cfg(not(feature = "llm-openai"))]
fn build_cli_consistency_arbiter() -> Arc<dyn ConsistencyArbiter> {
    Arc::new(SourceRefConsistencyArbiter::new())
}

/// `--model`（已在上游校验必填；此处仅取值）。
/// `--model` (requiredness was validated upstream; this only reads it).
#[cfg(feature = "llm-openai")]
fn model_name(args: &CompileArgs) -> String {
    args.model.clone().unwrap_or_default()
}

/// 无 `llm-openai` feature：openai/ollama 明确报错（§9/A23），mock 不受影响；
/// Err 经 run() 归类为退出码 2。
/// Without the `llm-openai` feature: openai/ollama error out explicitly
/// (§9/A23); mock is unaffected; the Err classifies as exit code 2 inside run().
#[cfg(not(feature = "llm-openai"))]
fn build_llm_compiler(_args: &CompileArgs, _policy: CompilePolicy) -> Result<Arc<dyn Compiler>> {
    Err(anyhow!(
        "provider openai/ollama requires the `llm-openai` feature (build with \
         --features llm-openai); use --provider mock for offline runs"
    ))
}

/// 退出码优先级（§9）：failed/quarantined → 3；仅 deferred → 4；否则 0。
/// 优先级 1>2>3>4>0 中 1/2 由 run 的 Err/输入错误路径返回。
/// Exit-code priority (§9): failed/quarantined → 3; deferred-only → 4; else 0.
/// Within 1>2>3>4>0, codes 1/2 come from the run-Err and input-error paths.
fn exit_code(stats: &CompileStats) -> i32 {
    if stats.failed > 0 || stats.quarantined > 0 {
        EXIT_FAILURES
    } else if stats.deferred > 0 {
        EXIT_DEFERRED
    } else {
        EXIT_OK
    }
}

/// run 级错误分类（§9）：数据库/内部运行故障 → Err（main 以退出码 1 结束）；
/// 参数/配置/输入文件协议错误 → 退出码 2。Step8 D11：兼容 preflight 拒绝
/// （稳定前缀 contains 匹配，commands::classify_error 同惯例）→ 退出码 3
/// （配置/迁移错误语义；不计入单页 failed/quarantined 统计）。
/// Run-level error classification (§9): database/internal faults → Err (main
/// exits with code 1); parameter/config/input-protocol errors → exit code 2.
/// Step8 D11: a compatibility-preflight rejection (matched via the stable
/// prefix with `contains`, the commands::classify_error convention) → exit
/// code 3 (config/migration-error semantics; never counted in the per-page
/// failed/quarantined statistics).
fn classify_run_error(err: Error) -> Result<i32> {
    if let Error::InvalidConfig(msg) = &err {
        if msg.contains(COMPATIBILITY_REJECTED_PREFIX) {
            return compatibility_rejected(err);
        }
    }
    match err {
        Error::Database(_) | Error::Internal(_) | Error::Migration(_) | Error::Serialization(_) => {
            Err(anyhow!(err))
        }
        _ => input_error(err),
    }
}

/// 兼容拒绝（Step8 §7/D11）：stderr 一条人类可读消息，退出码 3。Step4 的
/// 退出码 3 = failed/quarantined；Step8 起「配置/迁移/兼容错误」同为 3——一次
/// run 内二者互斥（兼容拒绝时单页统计恒为零，stats 可区分）。
/// Compatibility rejection (Step8 §7/D11): one human-readable stderr line, exit
/// code 3. Step4's exit code 3 = failed/quarantined; since Step8 a
/// "config/migration/compatibility error" is also 3 — the two are mutually
/// exclusive within one run (on a compatibility rejection the per-page stats
/// are always zero, which distinguishes them).
fn compatibility_rejected<E: std::fmt::Display>(err: E) -> Result<i32> {
    eprintln!("wiktor compile: {err}");
    Ok(EXIT_FAILURES)
}

/// 输入错误：stderr 打印一条人类可读消息并以 `Ok(EXIT_INPUT)` 返回退出码 2
/// （不打印源明文；区别于走 Err 的数据库/内部故障退出码 1）。
/// Input error: prints one human-readable line to stderr and returns
/// `Ok(EXIT_INPUT)` (never quoting source plaintext; distinct from the Err path
/// that exits 1 for database/internal faults).
fn input_error<E: std::fmt::Display>(err: E) -> Result<i32> {
    eprintln!("wiktor compile: {err}");
    Ok(EXIT_INPUT)
}

/// 统计输出（§9）：`--json` → stdout 单一 JSON CompileStats；人类模式打印固定
/// 表头 + 汇总行。skipped 细因与错误 code 由 tracing 走 stderr，不写源明文。
/// Statistics output (§9): `--json` → a single JSON CompileStats on stdout; the
/// human mode prints the fixed header plus a summary line. Skipped reasons and
/// error codes go through tracing on stderr, never quoting source plaintext.
fn print_stats(stats: &CompileStats, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(stats).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!("accepted quarantined failed skipped deferred attempts");
    println!(
        "{:>8} {:>11} {:>6} {:>7} {:>8} {:>8}",
        stats.accepted,
        stats.quarantined,
        stats.failed,
        stats.skipped,
        stats.deferred,
        stats.attempts
    );
    println!(
        "scanned={} reserved_tokens={} reported_tokens={} circuit_open={} dry_run={} would_compile={} run_id={}",
        stats.scanned,
        stats.reserved_tokens,
        stats.reported_tokens,
        stats.circuit_open,
        stats.dry_run,
        stats.would_compile,
        stats.run_id,
    );
}

/// 人类日志初始化：stderr，INFO 起步（skipped 细因/错误 code 经 tracing 展示）。
/// Human-log setup: stderr at INFO (skipped reasons / error codes via tracing).
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    // 测试需要 trait 方法在作用域内：DataSource::fetch / EntityStore::upsert_facts
    // / VectorStore::ensure_collection。
    // Tests need the traits in scope: DataSource::fetch /
    // EntityStore::upsert_facts / VectorStore::ensure_collection.
    use wiktor_core::traits::{DataSource, EntityStore, VectorStore};

    /// examples/milk-tea 目录（相对 crate 根）。
    /// The examples/milk-tea directory (relative to the crate root).
    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("milk-tea")
    }

    fn base_args(db: &Path) -> CompileArgs {
        CompileArgs {
            domain: examples_dir().join("domain-compile.yaml"),
            db: db.to_path_buf(),
            entity: Some("drink".to_string()),
            data_source: None,
            provider: ProviderArg::Mock,
            model: None,
            embedding_model: "none".to_string(),
            base_url: None,
            limit: 1000,
            batch_size: 32,
            force: false,
            dry_run: false,
            skip_compatibility_check: false,
            batch_token_budget: None,
            task_token_budget: None,
            daily_token_budget: None,
            json: true,
        }
    }

    // A20：退出码优先级 —— failed/quarantined → 3（高于 deferred 的 4）；
    // 仅 deferred → 4；全完成 → 0。1/2 走 Err 路径不经此函数。
    // A20: exit-code priority — failed/quarantined → 3 (above deferred's 4);
    // deferred-only → 4; all finalized → 0. Codes 1/2 travel the Err paths and
    // never reach this function.
    #[test]
    fn a20_exit_code_priority() {
        let mut s = CompileStats::default();
        assert_eq!(exit_code(&s), EXIT_OK);
        s.deferred = 2;
        assert_eq!(exit_code(&s), EXIT_DEFERRED);
        s.quarantined = 1;
        assert_eq!(
            exit_code(&s),
            EXIT_FAILURES,
            "quarantined outranks deferred"
        );
        s.quarantined = 0;
        s.failed = 1;
        assert_eq!(exit_code(&s), EXIT_FAILURES);
        s.deferred = 0;
        assert_eq!(exit_code(&s), EXIT_FAILURES);
    }

    // A20：run 级错误分类 —— 数据库/内部/迁移 → Err（退出码 1 路径）；输入协议 → 2。
    // A20: run-level error classification — database/internal/migration → Err
    // (the exit-code-1 path); input protocol → 2.
    #[test]
    fn a20_run_error_classification() {
        assert!(
            classify_run_error(Error::Internal("x".into())).is_err(),
            "Internal → exit 1 path"
        );
        assert!(
            classify_run_error(Error::Migration("boom".into())).is_err(),
            "Migration → exit 1 path"
        );
        let validation = classify_run_error(Error::Validation("bad input".into())).unwrap();
        assert_eq!(validation, EXIT_INPUT);
        let config = classify_run_error(Error::InvalidConfig("bad config".into())).unwrap();
        assert_eq!(config, EXIT_INPUT);
        let io = classify_run_error(Error::Io(std::io::Error::other("missing file"))).unwrap();
        assert_eq!(io, EXIT_INPUT, "input-file errors exit 2");
    }

    // A21/A20：mock 全链路 —— 统计等式、退出码 0、JSON 输出、accepted 页落库。
    // A21/A20: mock full pipeline — stats equation, exit code 0, JSON output and
    // accepted pages persisted.
    #[tokio::test]
    async fn a20_mock_compile_run_stats_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        let mut args = base_args(&db);
        args.json = true;
        let code = run(args).await.unwrap();
        assert_eq!(code, EXIT_OK);

        // DB 已创建；两个 fixture drink 均编译并发布。
        // The DB was created; both fixture drinks compile and publish.
        assert!(db.exists());
        let kernel = SqliteKernel::open(&db).unwrap();
        let pages = kernel.load_accepted_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 2);
        let ids: Vec<String> = pages.iter().map(|p| p.wiki.entity_id.to_key()).collect();
        assert!(ids.contains(&"milk-tea:drink:tapioca-milk-tea".to_string()));
        assert!(ids.contains(&"milk-tea:drink:lemon-tea".to_string()));
        // FTS 可见（§10：accepted 提交即可由 FTS/LIKE 路径查询）。
        // FTS-visible (§10: an accepted commit is immediately queryable).
        let hits = kernel
            .search(
                "珍珠奶茶",
                &wiktor_core::Filters::empty(),
                5,
                Some("milk-tea"),
            )
            .unwrap();
        assert!(
            hits.iter()
                .any(|h| h.entity_id.to_key() == "milk-tea:drink:tapioca-milk-tea"),
            "expected the compiled drink page in FTS hits, got {hits:?}"
        );
    }

    // A21：新 drink 页与事实 category 锚点一致 → 过滤 FTS 命中。
    // filter_page_candidates 由 SKU facts（price<=20 → category 值集合）推出
    // drink 锚点，FTS 在该候选域内命中编译页。
    // A21: the new drink page matches the fact category anchor → filtered FTS
    // hit. filter_page_candidates derives the drink anchors from SKU facts
    // (price<=20 → the set of category values) and FTS hits the compiled page
    // inside that candidate scope.
    #[tokio::test]
    async fn a21_compiled_drink_page_hits_filtered_fts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        run(base_args(&db)).await.unwrap();

        let kernel = Arc::new(SqliteKernel::open(&db).unwrap());
        // 事实平面：经 domain-compile.yaml 的 product 实体导入 products.jsonl。
        // Fact plane: import products.jsonl via the product entity of
        // domain-compile.yaml.
        let yaml = std::fs::read_to_string(examples_dir().join("domain-compile.yaml")).unwrap();
        let config: DomainConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        let product = config
            .entities
            .iter()
            .find(|e| e.name == "product")
            .expect("product entity declared")
            .clone();
        let facts_source = JsonlDataSource::from_config(&product, &examples_dir()).unwrap();
        let mut offset = 0usize;
        loop {
            let batch = facts_source
                .fetch(Some(wiktor_core::types::Cursor {
                    offset,
                    batch_size: 128,
                }))
                .await
                .unwrap();
            if batch.is_empty() {
                break;
            }
            offset += batch.len();
            for raw in &batch {
                let facts = facts_source.raw_to_facts(raw).unwrap();
                kernel
                    .upsert_facts(&raw.id, &facts, raw.source_revision)
                    .await
                    .unwrap();
            }
        }

        // 查询引擎：过滤下推 price<=20 → SKU 类别锚点集合（含 tapioca/lemon）。
        // Query engine: filter pushdown price<=20 → the set of SKU category
        // anchors (tapioca/lemon included).
        let store = Arc::new(wiktor_core::kernel::MockVectorStore::new());
        store
            .ensure_collection("milk-tea", 8, wiktor_core::traits::DistanceMetric::Cosine)
            .await
            .unwrap();
        let embedder = Arc::new(crate::embed::DeterministicEmbedder::new(8));
        let engine =
            wiktor_core::QueryEngine::new(kernel.clone(), store, None, embedder, "milk-tea", 5, 60)
                .unwrap();
        let query = wiktor_core::types::Query {
            text: "珍珠奶茶".to_string(),
            filters: crate::filter::parse_filter("price<=20").unwrap(),
            top_k: 10,
            domain: Some("milk-tea".into()),
        };
        let result = engine.search(&query).await.unwrap();
        let hit_ids: Vec<String> = result.hits.iter().map(|h| h.entity_id.to_key()).collect();
        assert!(
            hit_ids.contains(&"milk-tea:drink:tapioca-milk-tea".to_string()),
            "the compiled drink page must be recalled under the category anchor, got {hit_ids:?}"
        );
        // 无关 drink 页不应命中该查询。
        // Unrelated drink pages must not hit this query.
        assert!(
            !hit_ids.iter().any(|h| h.ends_with("cheese-tea")),
            "no cheese-tea page exists in this DB"
        );
    }

    // A20：dry-run 对不存在的 DB 只读计划——不创建文件、不调用模型
    //（attempts=0）、would_compile 计入、统计等式含 would_compile。
    // A20: dry-run over a missing DB plans read-only — no file created, no model
    // calls (attempts=0), would_compile accounted, equation covers would_compile.
    #[tokio::test]
    async fn a20_dry_run_missing_db_never_creates_or_compiles() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("planned.db");
        assert!(!db.exists());
        let mut args = base_args(&db);
        args.dry_run = true;
        let code = run(args).await.unwrap();
        assert_eq!(code, EXIT_OK, "a plan over valid input exits 0");
        assert!(!db.exists(), "dry-run must not create the DB file");
    }

    // A20：dry-run 对现有 DB 无写入——无任务/事实/页面落库（不迁移不排队）。
    // A20: dry-run over an existing DB writes nothing — no tasks/facts/pages.
    #[tokio::test]
    async fn a20_dry_run_existing_db_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("existing.db");
        {
            // 预创建当前 schema 的空库（真实 open，含迁移）。
            // Pre-create an empty current-schema DB (a real open, with migrations).
            let _ = SqliteKernel::open(&db).unwrap();
        }
        let before = {
            let k = SqliteKernel::open(&db).unwrap();
            k.row_counts().unwrap()
        };
        let mut args = base_args(&db);
        args.dry_run = true;
        let code = run(args).await.unwrap();
        assert_eq!(code, EXIT_OK);
        let after = {
            let k = SqliteKernel::open(&db).unwrap();
            k.row_counts().unwrap()
        };
        assert_eq!(before, after, "dry-run must not write any row");
        assert_eq!(after["compile_tasks"], 0);
        assert_eq!(after["pages"], 0);
        assert_eq!(after["facts"], 0);
    }

    // A23：--data-source 非 jsonl:// → 退出码 2；--entity 未声明 → 退出码 2。
    // A23: a non-jsonl:// --data-source → exit 2; an undeclared --entity → 2.
    #[tokio::test]
    async fn a23_input_contract_errors_exit_two() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let mut args = base_args(&db);
        args.data_source = Some("postgres://nope".to_string());
        assert_eq!(run(args).await.unwrap(), EXIT_INPUT);

        let mut args = base_args(&db);
        args.entity = Some("nope".to_string());
        assert_eq!(run(args).await.unwrap(), EXIT_INPUT);

        // 未存在的 domain 文件 → 退出码 2。
        // A missing domain file → exit 2.
        let mut args = base_args(&db);
        args.domain = dir.path().join("missing.yaml");
        assert_eq!(run(args).await.unwrap(), EXIT_INPUT);
    }

    // Step8 §7（批 B5）：--skip-compatibility-check 在空库/无旧 artifact 时放行
    // ——跳过 preflight 正常编译（退出码 0、页落库）。
    // Step8 §7 (batch B5): --skip-compatibility-check is allowed on an empty
    // database with no old artifact — the preflight is skipped and compilation
    // proceeds (exit 0, pages persisted).
    #[tokio::test]
    async fn b5_skip_check_allowed_on_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("fresh.db");
        let mut args = base_args(&db);
        args.skip_compatibility_check = true;
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        assert_eq!(kernel.load_accepted_pages("milk-tea").unwrap().len(), 2);
    }

    // Step8 §7（批 B5）：--skip-compatibility-check 遇到任何旧数据 fail-closed
    // ——退出码 3、进入 run 前零写入（无新任务/页/审核行）。
    // Step8 §7 (batch B5): --skip-compatibility-check fails closed on any old
    // data — exit 3 with zero writes before the run (no new tasks/pages/review
    // rows).
    #[tokio::test]
    async fn b5_skip_check_fails_closed_on_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("existing.db");
        // 首次正常编译制造旧数据（默认策略无矩阵 → legacy 直通）。
        // A first normal compile creates the old data (the default policy has
        // no matrix → the legacy pass-through).
        assert_eq!(run(base_args(&db)).await.unwrap(), EXIT_OK);
        let before = SqliteKernel::open(&db).unwrap().row_counts().unwrap();
        assert!(before["pages"] > 0, "precondition: the first run published");

        let mut args = base_args(&db);
        args.skip_compatibility_check = true;
        assert_eq!(run(args).await.unwrap(), EXIT_FAILURES);

        let after = SqliteKernel::open(&db).unwrap().row_counts().unwrap();
        assert_eq!(after, before, "the refused run must write nothing");
        assert_eq!(after["review_queue"], 0, "the skip path writes no review");
    }

    // Step8 D11：兼容拒绝的稳定前缀 → 退出码 3（配置/迁移错误语义）；普通
    // InvalidConfig 仍为 2。
    // Step8 D11: the compatibility-rejection stable prefix → exit 3 (the
    // config/migration-error semantics); a plain InvalidConfig stays 2.
    #[test]
    fn b5_run_error_classification_maps_compatibility_prefix() {
        let rejected = classify_run_error(Error::InvalidConfig(format!(
            "{COMPATIBILITY_REJECTED_PREFIX}: domain \"milk-tea\" has 2 violation(s)"
        )))
        .unwrap();
        assert_eq!(rejected, EXIT_FAILURES);
        let plain = classify_run_error(Error::InvalidConfig("bad policy".into())).unwrap();
        assert_eq!(plain, EXIT_INPUT);
    }
}
