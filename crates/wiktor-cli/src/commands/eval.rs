//! `wiktor eval`（Step 5 spec §3 D6/D7、§4.1 `commands/eval.rs`、§4.3 末段、
//! §6 报告契约、§7 A9–A14、§8 批6）。
//!
//! 流程：用法校验（top-k 10..=100，越界 → 2）→ 配置阶段（domain.yaml +
//! intents 原文，失败 → 3）→ 打开 DB → golden loader（实体存在性校验集合取自
//! DB accepted 页实体；loader 错误 → 3）→ 三档 A/B/C 评测（runner 在 C 档直调
//! `load_active_qug`：stale/Internal/数据库错误 = 运行失败；`Ok(None)`（无
//! active build）不报错，C 无图运行、判定走无增益 disabled）→ 报告三件套落盘
//! `--out-dir`（写盘失败 → 1）→ 人类摘要表 / `--json` JSON 结果。
//!
//! `wiktor eval` (Step 5 spec §3 D6/D7, §4.1 `commands/eval.rs`, §4.3 last
//! paragraph, §6 report contract, §7 A9–A14, §8 batch 6).
//!
//! Flow: usage validation (top-k 10..=100, out of range → 2) → config phase
//! (domain.yaml + raw intents, failure → 3) → open the DB → the golden loader
//! (entity-existence set taken from the DB's accepted-page entities; loader
//! errors → 3) → the three-tier A/B/C evaluation (the runner calls
//! `load_active_qug` directly in tier C: stale/Internal/database errors are
//! run failures; `Ok(None)` (no active build) is not an error — C runs without
//! a graph and the decision takes the no-gain disabled path) → the report trio
//! written under `--out-dir` (write failure → 1) → the human summary table /
//! the `--json` JSON result.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::Args;
use wiktor_core::eval::{
    load_golden_set, run_evaluation, EvalConfig, EvalOutcome, EvaluationReport, GoldenSet,
    QugDecision, EVAL_TOP_K_DEFAULT,
};
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::hybrid::RRF_K_DEFAULT;
use wiktor_core::traits::{DistanceMetric, DomainConfig, VectorStore};
use wiktor_core::types::error::Error;
use wiktor_core::QueryEmbedder;

use super::{load_domain_config_checked, load_intents_bytes_checked, report_error, EXIT_OK};
use crate::embed;

/// `wiktor eval` 参数（D7 参数表）。
/// `wiktor eval` arguments (the D7 parameter table).
#[derive(Debug, Args)]
pub struct EvalArgs {
    /// domain.yaml path (mandatory)
    /// domain.yaml 路径（必填）
    #[arg(long)]
    pub domain: PathBuf,
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// golden-queries.jsonl path (mandatory)
    /// golden-queries.jsonl 路径（必填）
    #[arg(long)]
    pub golden: PathBuf,
    /// Output directory for the report trio (mandatory)
    /// 三件套报告的输出目录（必填）
    #[arg(long)]
    pub out_dir: PathBuf,
    /// Evaluation top-k (default 10; range 10..=100)
    /// 评测 top-k（默认 10；范围 10..=100）
    #[arg(long, default_value_t = EVAL_TOP_K_DEFAULT)]
    pub top_k: usize,
    /// Emit the JSON report on stdout instead of the human summary table
    /// stdout 输出 JSON 报告而非人类摘要表
    #[arg(long)]
    pub json: bool,
    /// Use the deterministic/mock VectorStore instead of qdrant (offline)
    /// 使用确定性/Mock VectorStore 而非 qdrant（离线）
    #[arg(long)]
    pub no_qdrant: bool,
}

/// 用法校验：top-k 范围 10..=100（D7；越界 = CLI 用法错误，退出码 2）。
/// 在触碰文件/DB 之前执行，保证越界优先于一切配置/运行错误。
/// Usage validation: the top-k range 10..=100 (D7; out of range = a CLI usage
/// error, exit code 2). Runs before any file/DB touch so an out-of-range value
/// outranks every config/run error.
fn validate_top_k(top_k: usize) -> std::result::Result<usize, String> {
    if (10..=100).contains(&top_k) {
        Ok(top_k)
    } else {
        Err(format!("--top-k {top_k} out of range 10..=100 (D7)"))
    }
}

/// 命令入口：返回进程退出码（main 据此 `std::process::exit`）。
/// Command entry: returns the process exit code (main calls
/// `std::process::exit` with it).
pub async fn run(args: EvalArgs) -> Result<i32> {
    let command = "eval";

    // —— 用法阶段：top-k 越界 → 退出码 2（先于一切 IO/DB）。——
    // —— Usage phase: an out-of-range top-k → exit 2 (before any IO/DB). ——
    let top_k = match validate_top_k(args.top_k) {
        Ok(k) => k,
        Err(msg) => {
            eprintln!("wiktor {command}: {msg}");
            return Ok(super::EXIT_USAGE);
        }
    };

    // —— 配置阶段（domain.yaml + intents 原文；失败 → 退出码 3）——
    // —— Config phase (domain.yaml + raw intents; failure → exit 3) ——
    let config: DomainConfig = match load_domain_config_checked(&args.domain) {
        Ok(c) => c,
        Err(e) => return Ok(report_error(command, e)),
    };
    let domain_dir = args
        .domain
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let intents_bytes = match load_intents_bytes_checked(&config, &domain_dir) {
        Ok(b) => b,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 打开 DB（会迁移；迁移 → 3、数据库故障 → 1，分类器裁决）——
    // —— Open the DB (with migration; migration → 3, database faults → 1, the
    //       classifier decides) ——
    let kernel = match SqliteKernel::open(&args.db) {
        Ok(k) => Arc::new(k),
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— golden loader：实体存在性校验集合从 DB accepted 页实体取（spec 批6
    //    口径）；golden 文件读失败 → 3；loader 校验失败（重复 id/未知 kind/
    //    虚假实体/配额不足）→ Validation → 3 ——
    // —— Golden loader: the entity-existence set comes from the DB's accepted
    //    page entities (the batch-6 rule); a golden-file read failure → 3; a
    //    loader validation failure (duplicate ids, unknown kinds, fake
    //    entities, missing quotas) → Validation → 3 ——
    let accepted_pages = match kernel.load_accepted_pages(&config.name) {
        Ok(p) => p,
        Err(e) => return Ok(report_error(command, e)),
    };
    let known_entities: BTreeSet<String> = accepted_pages
        .iter()
        .map(|p| p.wiki.entity_id.to_key())
        .collect();
    let golden_bytes = match std::fs::read(&args.golden) {
        Ok(b) => b,
        Err(e) => {
            return Ok(report_error(
                command,
                Error::InvalidConfig(format!("read {}: {e}", args.golden.display())),
            ));
        }
    };
    let golden: GoldenSet = match load_golden_set(&golden_bytes, &known_entities) {
        Ok(g) => g,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 评测配置：报告审计字段（vector_backend / 运行命令）由此带入 ——
    // —— Eval config: the report audit fields (vector_backend / run command)
    //       come from here ——
    let eval_config = EvalConfig {
        top_k,
        rrf_k: RRF_K_DEFAULT,
        collection: config.name.clone(),
        vector_backend: if args.no_qdrant { "mock" } else { "qdrant" }.into(),
        command: format!(
            "wiktor eval --domain {} --db {} --golden {} --out-dir {} --top-k {top_k}{}",
            args.domain.display(),
            args.db.display(),
            args.golden.display(),
            args.out_dir.display(),
            if args.no_qdrant { " --no-qdrant" } else { "" },
        ),
    };

    // —— 三档评测（公平性契约由 runner 保证；单条 query 错误汇总为运行失败，
    //    stale/Internal/数据库错误立即失败，均走分类器 → 1/4）——
    // —— The three-tier evaluation (fairness is the runner's contract; per-query
    //    errors summarize into one run failure while stale/Internal/database
    //    errors fail immediately — all through the classifier → 1/4) ——
    let (embedder, dim) = match resolve_embedder(args.no_qdrant).await {
        Ok(ed) => ed,
        Err(e) => return Ok(report_error(command, e)),
    };
    let outcome = if args.no_qdrant {
        run_with_mock(
            kernel.clone(),
            embedder,
            dim,
            &config,
            &intents_bytes,
            &golden,
            &eval_config,
        )
        .await
    } else {
        run_with_qdrant(
            kernel.clone(),
            embedder,
            dim,
            &config,
            &intents_bytes,
            &golden,
            &eval_config,
        )
        .await
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 报告三件套落盘（写盘失败 = 运行失败 → 1，§6/D7）——
    // —— Write the report trio (a write failure is a run failure → 1, §6/D7) ——
    let report = EvaluationReport::build(&outcome, &golden, &config, &eval_config);
    let paths = match wiktor_core::eval::write_report_files(&args.out_dir, &report) {
        Ok(p) => p,
        Err(e) => return Ok(report_error(command, e)),
    };

    print_summary(&report, &paths, args.json);
    Ok(EXIT_OK)
}

/// 解析评测嵌入器与维度：
/// - `--no-qdrant`：恒为确定性嵌入器（768 维，离线可复现）；
/// - 真实 qdrant 路径 + `embedding-http` feature + 环境变量给出嵌入配置 →
///   `HttpEmbedder`（维度从首次响应探测，不硬编码）；
/// - 其余（无 feature 或无 env）→ 确定性嵌入器（768 维）。
///
/// Resolves the evaluation embedder and dimension:
/// - `--no-qdrant`: always the deterministic embedder (768-dim, reproducible
///   offline);
/// - real qdrant + `embedding-http` feature + env-configured embedding → the
///   `HttpEmbedder` (dimension probed from the first response, never
///   hard-coded);
/// - everything else (no feature / no env) → the deterministic embedder
///   (768-dim).
async fn resolve_embedder(no_qdrant: bool) -> wiktor_core::Result<(Arc<dyn QueryEmbedder>, usize)> {
    #[cfg(feature = "embedding-http")]
    {
        use wiktor_core::embedding::{HttpEmbedder, EMBEDDING_API_KEY_ENV, EMBEDDING_BASE_URL_ENV};
        let env_configured = !no_qdrant
            && (std::env::var(EMBEDDING_BASE_URL_ENV).is_ok()
                || std::env::var(EMBEDDING_API_KEY_ENV)
                    .map(|k| !k.trim().is_empty())
                    .unwrap_or(false));
        if env_configured {
            let embedder = Arc::new(HttpEmbedder::from_env()?);
            // 探测维度（首次 embed 响应长度；阿里 MaaS qwen 嵌入维度不硬编码）。
            // Probe the dimension (the first response's vector length; the Aliyun
            // MaaS qwen embedding dimension is never hard-coded).
            let dim = embedder.embed("\u{0}dimension-probe").await?.len();
            return Ok((embedder, dim));
        }
    }
    Ok((
        Arc::new(embed::DeterministicEmbedder::new(embed::DIM)),
        embed::DIM,
    ))
}

/// `--no-qdrant`：Mock VectorStore + 空集合（向量路 0 命中、RRF 退 FTS，离线
/// 可复现）。
/// `--no-qdrant`: a Mock VectorStore over an empty collection (the vector path
/// yields 0 hits and RRF degrades to FTS — reproducible offline).
async fn run_with_mock(
    kernel: Arc<SqliteKernel>,
    embedder: Arc<dyn QueryEmbedder>,
    dim: usize,
    config: &DomainConfig,
    intents_bytes: &[u8],
    golden: &GoldenSet,
    eval_config: &EvalConfig,
) -> wiktor_core::Result<EvalOutcome> {
    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection(&eval_config.collection, dim, DistanceMetric::Cosine)
        .await?;
    run_evaluation(
        kernel,
        store,
        embedder,
        config,
        intents_bytes,
        golden,
        eval_config,
    )
    .await
}

/// qdrant 路径：地址/密钥来自 `$WIKTOR_QDRANT_URL` / `$WIKTOR_QDRANT_API_KEY`
/// （默认 http://127.0.0.1:6334），与 `vector ping` 同源；连接/集合错误 →
/// 运行失败（退出码 1）。维度取 embedder 实际维度（真实嵌入动态
/// 探测；确定性嵌入器恒定 768）。
/// The qdrant path: address/key come from `$WIKTOR_QDRANT_URL` /
/// `$WIKTOR_QDRANT_API_KEY` (default http://127.0.0.1:6334), same sources as
/// `vector ping`; connection/collection errors are run failures (exit 1). The
/// dimension comes from the embedder (dynamic for real embeddings; fixed at
/// 768 for the deterministic one). It assembles the `wiktor-vector-qdrant`
/// plugin (STEP10 B3).
#[cfg(feature = "vector-qdrant")]
async fn run_with_qdrant(
    kernel: Arc<SqliteKernel>,
    embedder: Arc<dyn QueryEmbedder>,
    dim: usize,
    config: &DomainConfig,
    intents_bytes: &[u8],
    golden: &GoldenSet,
    eval_config: &EvalConfig,
) -> wiktor_core::Result<EvalOutcome> {
    let url =
        std::env::var("WIKTOR_QDRANT_URL").unwrap_or_else(|_| "http://127.0.0.1:6334".to_string());
    let api_key = std::env::var("WIKTOR_QDRANT_API_KEY").ok();
    let store = Arc::new(wiktor_vector_qdrant::QdrantVectorStore::from_config(
        &url,
        api_key.as_deref(),
        dim,
    )?);
    store
        .ensure_collection(&eval_config.collection, dim, DistanceMetric::Cosine)
        .await?;
    run_evaluation(
        kernel,
        store,
        embedder,
        config,
        intents_bytes,
        golden,
        eval_config,
    )
    .await
}

/// `wiktor eval` 的 qdrant 路径需要 `vector-qdrant` feature；缺失时明确报错。
/// The qdrant path of `wiktor eval` requires the `vector-qdrant` feature; when
/// missing, fail explicitly.
#[cfg(not(feature = "vector-qdrant"))]
async fn run_with_qdrant(
    _kernel: Arc<SqliteKernel>,
    _embedder: Arc<dyn QueryEmbedder>,
    _dim: usize,
    _config: &DomainConfig,
    _intents_bytes: &[u8],
    _golden: &GoldenSet,
    _eval_config: &EvalConfig,
) -> wiktor_core::Result<EvalOutcome> {
    Err(wiktor_core::types::error::Error::Validation(
        "eval requires the vector-qdrant feature (don't build with --no-default-features)".into(),
    ))
}

/// 指标数字格式（与报告 `num()` 同口径：6 位小数，None → "-"）。
/// Metric formatting (same shape as the report's `num()`: 6 decimals,
/// None → "-").
fn num(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.6}"),
        None => "-".into(),
    }
}

/// 结果输出：`--json` → stdout 输出 JSON 报告；人类模式打印摘要表（三档
/// @1/@5/@10、negative_precision、fallback）+ 判定 + 回退标注 + 报告路径。
/// Result output: `--json` prints the JSON report on stdout; human mode prints
/// the summary table (three tiers' @1/@5/@10, negative_precision, fallback) +
/// the decision + regression flags + the report paths.
fn print_summary(report: &EvaluationReport, paths: &[String; 3], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!(
        "domain: {}@{}  golden: {} records  top_k: {}  rrf_k: {}  vector_backend: {}",
        report.domain,
        report.domain_version,
        report.golden_total,
        report.eval_top_k,
        report.rrf_k,
        report.vector_backend,
    );
    println!("dataset_hash: {}", report.dataset_hash);
    println!(
        "{:<4} | {:>9} | {:>9} | {:>10} | {:>19} | {:>8}",
        "tier", "recall@1", "recall@5", "recall@10", "negative_precision", "fallback"
    );
    for key in ["A", "B", "C"] {
        let tier = &report.tiers[key];
        println!(
            "{:<4} | {:>9} | {:>9} | {:>10} | {:>19} | {:>8}",
            key,
            num(tier.recall_at_1),
            num(tier.recall_at_5),
            num(tier.recall_at_10),
            num(tier.negative_precision),
            tier.fallback_count,
        );
    }
    let decision = match report.decision.qug_decision {
        QugDecision::Enabled => "enabled",
        QugDecision::Disabled => "disabled",
    };
    let gain = report
        .decision
        .gain_pp
        .map(|g| format!("{g:.6}"))
        .unwrap_or_else(|| "n/a".into());
    println!("decision: {decision} (gain_pp={gain})");
    // 回退突出展示（D6/A12）：C 低于 B 必须可见，但不改变退出语义（disabled
    // 是合格交付）。
    // Regression highlights (D6/A12): C below B must be visible, yet the exit
    // semantics never change (a disabled verdict is a valid delivery).
    if report.decision.recall_regression {
        println!("regression: tier-C recall@10 is below tier-B");
    }
    if report.decision.negative_precision_regression {
        println!("regression: tier-C negative_precision is below tier-B");
    }
    println!("reason: {}", report.decision.reason);
    println!("reports:");
    println!("  {}", paths[0]);
    println!("  {}", paths[1]);
    println!("  {}", paths[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::qug::{self, BuildArgs};
    use wiktor_core::eval::{EVAL_REPORT_FILE_EN, EVAL_REPORT_FILE_JSON, EVAL_REPORT_FILE_ZH};

    /// examples/milk-tea 目录。
    /// The examples/milk-tea directory.
    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("milk-tea")
    }

    fn base_args(db: &Path, out_dir: &Path) -> EvalArgs {
        EvalArgs {
            domain: examples_dir().join("domain.yaml"),
            db: db.to_path_buf(),
            golden: examples_dir().join("golden-queries.jsonl"),
            out_dir: out_dir.to_path_buf(),
            top_k: EVAL_TOP_K_DEFAULT,
            json: false,
            no_qdrant: true,
        }
    }

    async fn seed(db: &Path) {
        crate::cmd_seed(db, &examples_dir().join("domain.yaml"), None)
            .await
            .unwrap();
    }

    // D7 用法校验：top-k 越界 = 退出码 2（先于配置/DB 检查，故无需 seed 库）。
    // D7 usage validation: an out-of-range top-k = exit 2 (checked before
    // config/DB, so no seeded database is needed).
    #[tokio::test]
    async fn a14_top_k_out_of_range_exits_two() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        let out_dir = dir.path().join("reports");
        for bad in [9usize, 101] {
            let mut args = base_args(&db, &out_dir);
            args.top_k = bad;
            assert_eq!(run(args).await.unwrap(), super::super::EXIT_USAGE);
        }
        // 边界值合法（真正的运行在其它用例覆盖）。
        // Boundary values are legal (real runs are covered elsewhere).
        assert_eq!(validate_top_k(10).unwrap(), 10);
        assert_eq!(validate_top_k(100).unwrap(), 100);
    }

    // A13/A12/A11：离线（--no-qdrant）跑通 examples/milk-tea 全量 golden，
    // 三件套落盘；无 active 构建（未 qug build）→ 判定走无增益 disabled，
    // 退出码 0（D6：disabled 是合格交付，不得当运行失败）。
    // A13/A12/A11: the offline (--no-qdrant) run over the full examples/milk-tea
    // golden set writes the report trio; with no active build (no `qug build`)
    // the decision takes the no-gain disabled path and exits 0 (D6: disabled is
    // a valid delivery, never a run failure).
    #[tokio::test]
    async fn a13_eval_offline_full_golden_disabled_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        seed(&db).await;
        let out_dir = dir.path().join("reports");

        let args = base_args(&db, &out_dir);
        assert_eq!(run(args).await.unwrap(), EXIT_OK, "disabled must exit 0");

        // 三件套落盘且 JSON 契约字段齐备。
        // The trio is written and the JSON contract fields are all present.
        let zh = out_dir.join(EVAL_REPORT_FILE_ZH);
        let en = out_dir.join(EVAL_REPORT_FILE_EN);
        let json = out_dir.join(EVAL_REPORT_FILE_JSON);
        assert!(zh.is_file() && en.is_file() && json.is_file());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["domain"], "milk-tea");
        assert_eq!(value["vector_backend"], "mock");
        assert_eq!(value["decision"]["qug_decision"], "disabled");
        assert!(
            value["failed_samples"].as_array().unwrap().is_empty(),
            "a successful run lists no failed samples"
        );
        assert_eq!(value["golden_total"], 134, "34 legacy + 100 new records");
        assert!(
            value["source_hash"].is_null(),
            "no active build → no source_hash"
        );

        // 构建 active 代次后重评：C 有图（source_hash 落报告），同样 exit 0。
        // Re-evaluate after publishing an active generation: tier C has a graph
        // (source_hash lands in the report) and the exit stays 0.
        let build_args = BuildArgs {
            domain: examples_dir().join("domain.yaml"),
            db: db.clone(),
            force: false,
            dry_run: false,
            json: false,
        };
        assert_eq!(qug::run(build_args).await.unwrap(), EXIT_OK);
        let out_dir2 = dir.path().join("reports2");
        let args = base_args(&db, &out_dir2);
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        let value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(out_dir2.join(EVAL_REPORT_FILE_JSON)).unwrap(),
        )
        .unwrap();
        assert!(
            value["source_hash"].is_string(),
            "an active build records its source_hash"
        );
        // 判定透传（D6）：决策只可能是 enabled/disabled 之一，且两者都必须
        // exit 0。当前冻结数据集（134 条 golden + seed 页 QUG 图）实测为
        // enabled（gain ≈ 49.57pp）；本测试只钉"合法决策 + 成功退出"这一批6
        // 契约，数据集层面的精确指标由 core 评测测试守护。
        // Decision passthrough (D6): the verdict is exactly one of
        // enabled/disabled and both must exit 0. On the current frozen dataset
        // (134 golden + the seed-page QUG graph) the measured verdict is
        // enabled (gain ≈ 49.57pp); this test pins only the batch-6 contract —
        // "a legal verdict plus a successful exit" — while the dataset-level
        // exact metrics stay guarded by the core evaluation tests.
        let verdict = value["decision"]["qug_decision"].as_str().unwrap();
        assert!(
            verdict == "enabled" || verdict == "disabled",
            "the verdict must be a legal D6 value, got {verdict}"
        );
    }

    // A14：golden 文件缺失 = golden 校验错误 → 退出码 3。
    // A14: a missing golden file is a golden-validation error → exit 3.
    #[tokio::test]
    async fn a14_missing_golden_file_exits_three() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        seed(&db).await;
        let mut args = base_args(&db, &dir.path().join("reports"));
        args.golden = dir.path().join("missing.jsonl");
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_CONFIG);
    }

    // A14：报告写盘失败 = 运行失败 → 退出码 1（out-dir 是已存在的文件，
    // create_dir_all 必败）。评测本身成功，败在落盘。
    // A14: a report-write failure is a run failure → exit 1 (out-dir is an
    // existing file, so create_dir_all must fail). The evaluation itself
    // succeeded; only the write failed.
    #[tokio::test]
    async fn a14_report_write_failure_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        seed(&db).await;
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let args = base_args(&db, &blocker);
        assert_eq!(run(args).await.unwrap(), super::super::EXIT_RUN_FAILURE);
    }
}
