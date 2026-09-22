//! `wiktor qug build`（Step 5 spec §3 D7、§4.1 `commands/qug.rs`、§4.2 编排
//! 契约、§7 A1/A13/A14、§8 批6）。
//!
//! 流程（D7）：加载领域包 + intents 原文 bytes →
//! - `--dry-run`：只解析 intents / 从 DB 筛 accepted 页 / 派生五类边 / 算
//!   source_hash / 计数，**不写库**，打印"将发布内容"（含 would reuse /
//!   would publish）；dry-run 不创建 DB 文件（存在 → 不迁移的 inspect 连接，
//!   缺失 → 内存空库计划，风格对齐 step4 compile dry-run）；
//! - 正常：[`build_and_publish_qug_from_bytes`] —— hash 命中且非 `--force`
//!   时复用 active 代次（A1），否则单事务发布新代次；`source_changed` 由
//!   发布事务内的来源复核兜底（稳定前缀错误 → 退出码 1）。
//!
//! 成功后**不做隐式 reload**（spec §4.4：reload 由调用方/下次启动显式触发，
//! 查询侧读 active published build 即可看到新代次）。
//!
//! `wiktor qug build` (Step 5 spec §3 D7, §4.1 `commands/qug.rs`, §4.2
//! orchestration contract, §7 A1/A13/A14, §8 batch 6).
//!
//! Flow (D7): load the domain pack + raw intents bytes →
//! - `--dry-run`: parse intents / filter accepted pages from the DB / derive
//!   the five edge types / compute source_hash / count — **never writing the
//!   database** — and print what would be published (including would reuse /
//!   would publish); dry-run never creates the DB file (existing → the
//!   non-migrating inspect connection, missing → an in-memory empty plan,
//!   styled after the step4 compile dry-run);
//! - normal: [`build_and_publish_qug_from_bytes`] — on a hash hit without
//!   `--force` the active generation is reused (A1), otherwise a new
//!   generation is published in one transaction; `source_changed` is fenced by
//!   the in-transaction source re-check (stable-prefix error → exit code 1).
//!
//! No implicit reload happens after success (spec §4.4: reload is triggered
//! explicitly by the caller / the next start; the query side reads the active
//! published build and sees the new generation).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use wiktor_core::kernel::qug_store::build_and_publish_qug_from_bytes;
use wiktor_core::kernel::{QugStore, SqliteKernel};
use wiktor_core::query_engine::qug::qug_build::{derive_qug_edges, parse_intents, QugBuildOutcome};
use wiktor_core::traits::DomainConfig;
use wiktor_core::types::error::Error;

use super::{load_domain_config_checked, load_intents_bytes_checked, report_error, EXIT_OK};

/// `wiktor qug build` 参数（D7 参数表）。
/// `wiktor qug build` arguments (the D7 parameter table).
#[derive(Debug, Args)]
pub struct BuildArgs {
    /// domain.yaml path (mandatory)
    /// domain.yaml 路径（必填）
    #[arg(long)]
    pub domain: PathBuf,
    /// SQLite database path (default ./wiktor.db)
    /// SQLite 数据库路径（默认 ./wiktor.db）
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Ignore the active source_hash and always publish a new generation
    /// 忽略 active source_hash，强制发布新代次
    #[arg(long)]
    pub force: bool,
    /// Plan only: parse/filter/hash/count and never write the database
    /// 只解析/筛选/算 hash/计数，绝不写库
    #[arg(long)]
    pub dry_run: bool,
    /// Emit a single JSON object instead of human-readable text
    /// 输出单个 JSON 对象而非人类可读文本
    #[arg(long)]
    pub json: bool,
}

/// build 结果的输出模型（人类文本与 `--json` 共用同一数据）。
/// Output model of one build (shared by the human text and `--json`).
#[derive(Debug, Serialize)]
struct BuildOutput {
    command: &'static str,
    /// "published" / "reused" / "dry_run"。
    /// "published" / "reused" / "dry_run".
    outcome: &'static str,
    /// 复用或 dry-run 命中 active hash 时为 true。
    /// True when reused, or when a dry-run would reuse.
    reused: bool,
    /// dry-run 无真实代次 → None。
    /// A dry-run has no real generation → None.
    build_id: Option<i64>,
    source_hash: String,
    accepted_page_count: usize,
    edge_count: usize,
    /// 五类边计数（键 = edge_type_name；零边类型不出现）。
    /// Per-type edge counts (key = edge_type_name; zero-count types absent).
    by_type: BTreeMap<String, usize>,
}

/// 命令入口：返回进程退出码（main 据此 `std::process::exit`）。
/// Command entry: returns the process exit code (main calls
/// `std::process::exit` with it).
pub async fn run(args: BuildArgs) -> Result<i32> {
    let command = "qug build";

    // —— 配置阶段（读/解析 domain.yaml + intents 原文；失败 → 退出码 3）——
    // —— Config phase (read/parse domain.yaml + raw intents; failure → exit 3) ——
    let config = match load_domain_config_checked(&args.domain) {
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

    // —— kernel：dry-run 绝不创建/迁移 DB（存在 → 不迁移 inspect 连接；缺失 →
    //    内存空库计划）；正常 run 才走会迁移的 open（A13 dry-run 契约）。
    //    Migration 错误 → 退出码 3；数据库故障 → 退出码 1（分类器裁决）。
    // —— Kernel: dry-run never creates/migrates the DB (existing → the
    //    non-migrating inspect connection; missing → an in-memory empty plan);
    //    only a real run opens with migration (the A13 dry-run contract).
    //    Migration errors → exit 3; database faults → exit 1 (the classifier
    //    decides).
    let kernel = if args.dry_run {
        if args.db.exists() {
            match SqliteKernel::open_existing(&args.db) {
                Ok(k) => k,
                Err(e) => return Ok(report_error(command, e)),
            }
        } else {
            SqliteKernel::open_in_memory()?
        }
    } else {
        match SqliteKernel::open(&args.db) {
            Ok(k) => k,
            Err(e) => return Ok(report_error(command, e)),
        }
    };

    if args.dry_run {
        // —— dry-run：解析/筛选/派生/计数，不写库；打印将发布内容（D7）——
        // —— Dry-run: parse/filter/derive/count without writing; print what
        //       would be published (D7) ——
        let output = match dry_run_plan(&kernel, &config, &intents_bytes, args.force) {
            Ok(o) => o,
            Err(e) => return Ok(report_error(command, e)),
        };
        print_output(&output, args.json);
        return Ok(EXIT_OK);
    }

    // —— 正常发布：qug 段 canonical JSON 参与 source_hash（序列化失败 → 4）；
    //    hash 命中且非 force → 复用（A1）；发布事务内来源复核兜底
    //    source_changed（→ 1）。成功后不做隐式 reload（§4.4）。
    // —— Normal publish: the canonical qug-config JSON joins source_hash
    //    (serialization failure → 4); a hash hit without force reuses (A1);
    //    the in-transaction source re-check fences source_changed (→ 1). No
    //    implicit reload after success (§4.4).
    let qug_config_json = match serde_json::to_string(&config.qug) {
        Ok(s) => s,
        Err(e) => return Ok(report_error(command, Error::Serialization(e))),
    };
    let outcome = match build_and_publish_qug_from_bytes(
        &kernel,
        &config.name,
        &config.version,
        qug_config_json,
        &intents_bytes,
        &config,
        args.force,
    ) {
        Ok(o) => o,
        Err(e) => return Ok(report_error(command, e)),
    };

    let output = match outcome {
        QugBuildOutcome::Reused(s) => BuildOutput {
            command: "qug build",
            outcome: "reused",
            reused: true,
            build_id: Some(s.build_id),
            source_hash: s.source_hash,
            accepted_page_count: s.accepted_page_count,
            edge_count: s.edge_count,
            by_type: s.by_type,
        },
        QugBuildOutcome::Published(s) => BuildOutput {
            command: "qug build",
            outcome: "published",
            reused: false,
            build_id: Some(s.build_id),
            source_hash: s.source_hash,
            accepted_page_count: s.accepted_page_count,
            edge_count: s.edge_count,
            by_type: s.by_type,
        },
    };
    print_output(&output, args.json);
    Ok(EXIT_OK)
}

/// dry-run 计划：解析 intents → 组装 accepted 页快照 → 派生五类边与 hash →
/// 与 active source_hash 比对给出 would reuse / would publish。全程只读。
/// The dry-run plan: parse intents → assemble the accepted-page snapshot →
/// derive the five edge types and hash → compare with the active source_hash
/// for would reuse / would publish. Read-only throughout.
fn dry_run_plan(
    kernel: &SqliteKernel,
    config: &DomainConfig,
    intents_bytes: &[u8],
    force: bool,
) -> wiktor_core::Result<BuildOutput> {
    let intents = parse_intents(intents_bytes)?;
    let qug_config_json = serde_json::to_string(&config.qug)?;
    let snapshot = kernel.assemble_qug_snapshot(
        &config.name,
        &config.version,
        qug_config_json,
        intents_bytes.to_vec(),
    )?;
    let derived = derive_qug_edges(&snapshot, config, &intents)?;
    let would_reuse = !force
        && kernel
            .active_source_hash(&config.name, &config.version)?
            .as_deref()
            == Some(derived.source_hash.as_str());
    let edge_count = derived.edge_count();
    Ok(BuildOutput {
        command: "qug build",
        outcome: "dry_run",
        reused: would_reuse,
        build_id: None,
        source_hash: derived.source_hash,
        accepted_page_count: derived.accepted_page_count,
        edge_count,
        by_type: derived.by_type,
    })
}

/// 输出（对齐 compile 惯例）：`--json` → stdout 单一 JSON 对象；人类模式打印
/// 固定字段 + 分类型计数。
/// Output (aligned with the compile conventions): `--json` → a single JSON
/// object on stdout; human mode prints fixed fields plus per-type counts.
fn print_output(output: &BuildOutput, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(output).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!("command: {}", output.command);
    println!("mode: {}", output.outcome);
    println!(
        "build_id: {}",
        output
            .build_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("source_hash: {}", output.source_hash);
    println!("accepted_pages: {}", output.accepted_page_count);
    println!("edges: {} total", output.edge_count);
    for (kind, count) in &output.by_type {
        println!("  {kind}: {count}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// examples/milk-tea 目录（相对 crate 根，与 compile.rs 测试同一定位）。
    /// The examples/milk-tea directory (crate-root relative, same resolution
    /// as the compile.rs tests).
    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
            .join("milk-tea")
    }

    fn base_args(db: &Path) -> BuildArgs {
        BuildArgs {
            domain: examples_dir().join("domain.yaml"),
            db: db.to_path_buf(),
            force: false,
            dry_run: false,
            json: false,
        }
    }

    /// 用 CLI 的 seed 命令路径灌入 examples/milk-tea（pages + facts）。
    /// Seeds examples/milk-tea (pages + facts) via the CLI seed path.
    async fn seed(db: &Path) {
        crate::cmd_seed(db, &examples_dir().join("domain.yaml"), None)
            .await
            .unwrap();
    }

    // A13/A1：首次发布 → 二次复用（不新建代次）→ --force 新建代次（同 hash）。
    // A13/A1: first publish → second reuse (no new generation) → --force
    // publishes a new generation (same hash).
    #[tokio::test]
    async fn a13_build_publishes_reuses_and_forces() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        seed(&db).await;

        // 首次：发布（exit 0；active 代次可读、边非空）。
        // First run: publish (exit 0; the active generation is readable with
        // a non-empty edge set).
        assert_eq!(run(base_args(&db)).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        let (id1, hash1) = kernel
            .active_build_identity("milk-tea", "0.1.0")
            .unwrap()
            .expect("first build must publish an active generation");
        let edges1 = kernel.load_active_edges("milk-tea", "0.1.0").unwrap();
        assert!(!edges1.is_empty(), "seed pages + intents must yield edges");

        // 二次：hash 命中复用 —— build_id 与 source_hash 不变 = 不新建代次。
        // Second run: a hash hit reuses — identical build_id and source_hash
        // means no new generation.
        assert_eq!(run(base_args(&db)).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        let (id2, hash2) = kernel
            .active_build_identity("milk-tea", "0.1.0")
            .unwrap()
            .expect("active build must survive the reuse run");
        assert_eq!(id2, id1, "reuse must not allocate a new generation");
        assert_eq!(hash2, hash1);

        // --force：同 hash 也新建代次（新 build_id、边集等价）。
        // --force: a new generation even on a hash hit (a new build_id with an
        // equivalent edge set).
        let mut forced = base_args(&db);
        forced.force = true;
        assert_eq!(run(forced).await.unwrap(), EXIT_OK);
        let kernel = SqliteKernel::open(&db).unwrap();
        let (id3, hash3) = kernel
            .active_build_identity("milk-tea", "0.1.0")
            .unwrap()
            .expect("force must leave an active published generation");
        assert!(id3 > id1, "force must allocate a new build_id");
        assert_eq!(hash3, hash1, "identical source keeps the same hash");
        assert_eq!(
            kernel.load_active_edges("milk-tea", "0.1.0").unwrap().len(),
            edges1.len(),
            "a forced rebuild over the same source republishes the same edge set"
        );
    }

    // A13：dry-run 对已 seed 的库不写任何行、不发布代次。
    // A13: a dry-run over a seeded database writes no rows and publishes no
    // generation.
    #[tokio::test]
    async fn a13_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        seed(&db).await;
        let before = SqliteKernel::open(&db).unwrap();
        assert!(
            before
                .active_build_identity("milk-tea", "0.1.0")
                .unwrap()
                .is_none(),
            "seed alone must not publish a QUG build"
        );
        let counts_before = before.row_counts().unwrap();

        let mut args = base_args(&db);
        args.dry_run = true;
        assert_eq!(run(args).await.unwrap(), EXIT_OK);

        let after = SqliteKernel::open(&db).unwrap();
        assert!(
            after
                .active_build_identity("milk-tea", "0.1.0")
                .unwrap()
                .is_none(),
            "dry-run must not publish a generation"
        );
        assert!(
            after
                .load_active_edges("milk-tea", "0.1.0")
                .unwrap()
                .is_empty(),
            "dry-run must not produce loadable edges"
        );
        assert_eq!(
            after.row_counts().unwrap(),
            counts_before,
            "dry-run must not write any row"
        );
    }

    // A13：dry-run 对不存在的 DB 只做内存计划，绝不创建文件。
    // A13: a dry-run over a missing database plans in memory and never creates
    // the file.
    #[tokio::test]
    async fn a13_dry_run_missing_db_never_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("planned.db");
        assert!(!db.exists());
        let mut args = base_args(&db);
        args.dry_run = true;
        assert_eq!(run(args).await.unwrap(), EXIT_OK);
        assert!(!db.exists(), "dry-run must not create the DB file");
    }

    // A14：domain.yaml 缺失/不可解析 = 配置错误 → 退出码 3（而非 1/2/4）。
    // A14: a missing/unparseable domain.yaml is a config error → exit 3 (not
    // 1/2/4).
    #[tokio::test]
    async fn a14_missing_domain_config_exits_three() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.db");
        let mut args = base_args(&db);
        args.domain = dir.path().join("missing.yaml");
        assert_eq!(run(args).await.unwrap(), crate::commands::EXIT_CONFIG);

        // 解析失败同样 → 3。
        // A parse failure also → 3.
        let bad = dir.path().join("bad.yaml");
        std::fs::write(&bad, ":::: not yaml ::::").unwrap();
        let mut args = base_args(&db);
        args.domain = bad;
        assert_eq!(run(args).await.unwrap(), crate::commands::EXIT_CONFIG);
    }
}
