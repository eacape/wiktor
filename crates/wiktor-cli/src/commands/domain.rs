//! `wiktor domain check`（Step 8 spec §3 D11、§5.1、§6.4、§7 CLI 契约、§9
//! A14–A16、§10 批 B6；STEP8-028 补偿）：对领域包与数据库做**只读全量兼容
//! preflight**。
//!
//! 语义（§7）：
//! - 只读：不迁移、不建库、不写 review、不写任何行——文件缺失的 `--db` 按空库
//!   只检查配置本身（内存库，绝不创建文件，A15）；已存在的库经
//!   `SqliteKernel::open_existing` 只读打开（旧 schema = migration_required →
//!   配置/迁移错误）；preflight 与真实 compile 共用同一库级入口
//!   [`wiktor_core::compile::compatibility::check_domain_compatibility`]（D11
//!   「同一 preflight」）。
//! - 矩阵缺失（domain.yaml 无 `compatibility` 段）→ 附加一条
//!   `MISSING_COMPATIBILITY_MATRIX` 稳定码告警（STEP8-028 补偿：legacy 只读容忍
//!   继续，但必须显式可见；告警不是违规，不翻转 compatible）。
//! - `--json` stdout 只有一份 `CompatibilityReport`（§6.4 形状；warnings 空时
//!   字段整体省略）；人类摘要走 stdout，日志/错误走 stderr。
//! - 退出码：0 兼容；1 数据库/IO 故障；2 参数/范围错误（clap 自身裁决）；
//!   3 不兼容/配置/迁移/数据协议错误；4 未分类内部错误。
//!
//! `wiktor domain check` (Step 8 spec §3 D11, §5.1, §6.4, §7 CLI contract, §9
//! A14–A16, §10 batch B6; the STEP8-028 compensation): a **read-only full
//! compatibility preflight** of a domain pack against a database.
//!
//! Semantics (§7):
//! - Read-only: no migration, no DB creation, no review writes, no rows at all —
//!   a missing `--db` file checks the configuration itself against an empty
//!   database (in-memory, never creating the file, A15); an existing database is
//!   opened read-only via `SqliteKernel::open_existing` (an old schema = a
//!   migration-required config/migration error); the preflight shares the same
//!   library entry [`wiktor_core::compile::compatibility::check_domain_
//!   compatibility`] as a real compile (D11's "the same preflight").
//! - A missing matrix (no `compatibility` section in domain.yaml) attaches one
//!   `MISSING_COMPATIBILITY_MATRIX` stable-code warning (the STEP8-028
//!   compensation: the legacy read-only tolerance stands but must be visible; a
//!   warning is not a violation and never flips `compatible`).
//! - `--json` puts exactly one `CompatibilityReport` on stdout (the §6.4 shape;
//!   the warnings field is omitted entirely when empty); the human summary goes
//!   to stdout, logs/errors to stderr.
//! - Exit codes: 0 compatible; 1 database/IO faults; 2 argument/range errors
//!   (clap's own verdict); 3 incompatible/config/migration/data-protocol errors;
//!   4 unclassified internal errors.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use wiktor_core::compile::compatibility::{
    check_domain_compatibility, missing_matrix_warning, CompatibilityReport,
    StandardCompatibilityChecker,
};
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::DomainConfig;
use wiktor_core::types::error::{Error, Result as CoreResult};

use super::{report_error, EXIT_CONFIG, EXIT_OK};

/// `wiktor domain` 子命令（§7 CLI 契约）。
/// The `wiktor domain` subcommands (the §7 CLI contract).
#[derive(Debug, Subcommand)]
pub enum DomainCommand {
    /// Read-only full compatibility preflight: every accepted page and every
    /// pending/running/dead task snapshot of this domain against the pack's
    /// declared matrix; incompatible → exit 3, nothing is ever written.
    /// 只读全量兼容 preflight：本 domain 的全部 accepted 页与全部
    /// pending/running/dead 任务快照对照领域包声明的矩阵；不兼容 → 退出码 3，
    /// 绝不写任何行。
    Check(CheckArgs),
    /// Discover installed domain packs: scan `examples/*/domain.yaml` plus any
    /// `WIKTOR_DOMAIN_DIR` (repeatable) for domain.yaml files, and report each
    /// pack's name/version/qug.enabled. Read-only; no registry, no side effects.
    /// 发现已安装的领域包：扫描 `examples/*/domain.yaml` 与 `WIKTOR_DOMAIN_DIR`
    ///（可重复）下的 domain.yaml，报告每个包的 name/version/qug.enabled。
    /// 只读；无注册表、无副作用。
    List(ListArgs),
}

/// `wiktor domain list` 参数。
/// `wiktor domain list` arguments.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Emit a single JSON array on stdout; logs go to stderr
    /// stdout 输出单一 JSON 数组；日志走 stderr
    #[arg(long)]
    pub json: bool,
}

/// `wiktor domain check` 参数（§7 参数表）。
/// `wiktor domain check` arguments (the §7 parameter table).
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// domain.yaml path (mandatory); the `compatibility` section carries the matrix
    /// domain.yaml 路径（必填）；`compatibility` 段承载兼容矩阵
    #[arg(long)]
    pub domain: PathBuf,
    /// SQLite database path (default ./wiktor.db); read-only — a missing file is
    /// checked as an empty database and never created
    /// SQLite 数据库路径（默认 ./wiktor.db）；只读——文件缺失按空库检查且绝不
    /// 创建
    #[arg(long, default_value = "wiktor.db")]
    pub db: PathBuf,
    /// Emit a single CompatibilityReport JSON on stdout; logs go to stderr
    /// stdout 输出单一 CompatibilityReport JSON；日志走 stderr
    #[arg(long)]
    pub json: bool,
}

/// 命令入口：返回进程退出码（main 据此 `std::process::exit`）。
/// Command entry: returns the process exit code (main calls
/// `std::process::exit` with it).
pub async fn run(command: DomainCommand) -> Result<i32> {
    match command {
        DomainCommand::Check(args) => run_check(args).await,
        DomainCommand::List(args) => run_list(args),
    }
}

/// 领域包发现的单条元数据（仅读 YAML 顶层，不要求完整 DomainConfig）。
/// A single discovered domain pack's metadata (reads only the YAML top level; no
/// full DomainConfig required).
#[derive(Debug, serde::Serialize)]
struct DomainPackMeta {
    name: String,
    version: String,
    path: std::path::PathBuf,
    qug_enabled: bool,
}

/// 领域包顶层结构（宽松反序列化；只取显示需要的字段）。
/// The domain.yaml top-level shape (lenient deserialize; only what we display).
#[derive(Debug, serde::Deserialize)]
struct DomainTop {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    qug: Option<QugTop>,
}

fn default_version() -> String {
    "0.0.0".to_string()
}

#[derive(Debug, serde::Deserialize)]
struct QugTop {
    #[serde(default)]
    enabled: bool,
}

/// 扫描一个目录下含 domain.yaml 的领域包，返回 (yaml 路径, 元数据)。
/// Scans a directory for packs containing domain.yaml, returning
/// (yaml path, metadata).
fn scan_dir(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Option<DomainPackMeta>)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let yaml = path.join("domain.yaml");
        if yaml.is_file() {
            let meta = std::fs::read_to_string(&yaml)
                .ok()
                .and_then(|t| serde_yaml_ng::from_str::<DomainTop>(&t).ok())
                .map(|top| DomainPackMeta {
                    name: top.name,
                    version: top.version,
                    path: yaml.clone(),
                    qug_enabled: top.qug.map(|q| q.enabled).unwrap_or(false),
                });
            found.push((yaml, meta));
        }
    }
    found
}

/// 收集发现到的领域包（`examples/` 根 + env `WIKTOR_DOMAIN_DIR` 可重复）。
/// Collects discovered packs (the `examples/` root + repeatable `WIKTOR_DOMAIN_DIR`).
fn collect_packs() -> Vec<DomainPackMeta> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    // 当前目录下的 examples/（领域包官方存放处）。
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let examples = cwd.join("examples");
    dirs.push(examples);
    // env WIKTOR_DOMAIN_DIR（可重复，':'/';' 分隔，兼容跨平台）。
    for d in std::env::var("WIKTOR_DOMAIN_DIR")
        .unwrap_or_default()
        .split(':')
        .chain(
            std::env::var("WIKTOR_DOMAIN_DIR")
                .unwrap_or_default()
                .split(';'),
        )
        .filter(|s| !s.is_empty())
    {
        dirs.push(d.into());
    }

    let mut packs = Vec::new();
    for d in dirs {
        for (_, meta) in scan_dir(&d) {
            if let Some(m) = meta {
                packs.push(m);
            }
        }
    }
    packs
}

/// `wiktor domain list`：发现领域包并输出表/JSON。只读，无副作用。
/// `wiktor domain list`: discovers packs and prints a table/JSON. Read-only.
fn run_list(args: ListArgs) -> Result<i32> {
    let mut packs = collect_packs();
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    packs.dedup_by(|a, b| a.path == b.path);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&packs)?);
    } else {
        println!("{:<14} {:<10} {:<8} PATH", "NAME", "VERSION", "QUG");
        for p in &packs {
            println!(
                "{:<14} {:<10} {:<8} {}",
                p.name,
                p.version,
                if p.qug_enabled { "enabled" } else { "off" },
                p.path.display()
            );
        }
    }
    Ok(EXIT_OK)
}

/// 评估核心（与 CLI 输出解耦，便于单测）：身份五元组 → 同一 preflight 入口 →
/// 矩阵缺失时附加稳定码告警（STEP8-028 补偿）。纯读，不写任何行。
/// The evaluation core (decoupled from CLI output for unit tests): the identity
/// five-tuple → the shared preflight entry → a stable-code warning attached when
/// the matrix is missing (the STEP8-028 compensation). Purely read-only.
fn evaluate(config: &DomainConfig, kernel: &SqliteKernel) -> CoreResult<CompatibilityReport> {
    let identity = config.identity()?;
    let spec = config.compile_policy.compatibility.as_ref();
    let mut report = check_domain_compatibility(
        kernel,
        &StandardCompatibilityChecker::new(),
        &identity,
        spec,
    )?;
    if spec.is_none() {
        report.warnings.push(missing_matrix_warning(&identity));
    }
    Ok(report)
}

/// `domain check`：见模块文档流程。
/// `domain check`: see the module-doc flow.
async fn run_check(args: CheckArgs) -> Result<i32> {
    let command = "domain check";

    // —— 配置阶段：读/解析 domain.yaml（strict semver 在解析期强制；读/解析/
    //    身份失败 → InvalidConfig → 3）——
    // —— Config phase: read/parse domain.yaml (strict semver enforced at parse
    //    time; read/parse/identity failures → InvalidConfig → 3) ——
    let config = match super::load_domain_config_checked(&args.domain) {
        Ok(c) => c,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 只读打开 DB：文件缺失 → 内存空库（绝不创建文件，A15）；存在 →
    //    open_existing（不迁移；旧 schema/非 Wiktor 文件 → migration_required →
    //    3；数据库故障 → 1，分类器裁决）——
    // —— Read-only DB open: a missing file → an in-memory empty database (never
    //    creating the file, A15); an existing one → open_existing (no migration;
    //    an old schema / a non-Wiktor file → migration_required → 3; database
    //    faults → 1, the classifier decides) ——
    let kernel = if args.db.exists() {
        match SqliteKernel::open_existing(&args.db) {
            Ok(k) => k,
            Err(e) => return Ok(report_error(command, e)),
        }
    } else {
        match SqliteKernel::open_in_memory() {
            Ok(k) => k,
            Err(e) => return Ok(report_error(command, e)),
        }
    };

    // —— 同一 preflight（§6.4/D11；数据库故障 → 1，协议 → 3）——
    // —— The same preflight (§6.4/D11; database faults → 1, protocol → 3) ——
    let report = match evaluate(&config, &kernel) {
        Ok(r) => r,
        Err(e) => return Ok(report_error(command, e)),
    };

    // —— 输出：--json → stdout 单份 CompatibilityReport（warnings 空时省略字段，
    //    §6.4 形状不变；序列化失败 → 未分类内部错误 4）；否则人类摘要 ——
    // —— Output: --json → exactly one CompatibilityReport on stdout (the warnings
    //    field omitted when empty, keeping the §6.4 shape; a serialization fault →
    //    unclassified internal error 4); otherwise the human summary ——
    if args.json {
        let json = match serde_json::to_string_pretty(&report) {
            Ok(j) => j,
            Err(e) => return Ok(report_error(command, Error::Serialization(e))),
        };
        println!("{json}");
    } else {
        print_summary(&report);
    }
    // 不兼容 → 3（配置/迁移/兼容/数据协议错误语义）；兼容 → 0。
    // Incompatible → 3 (the config/migration/compatibility/data-protocol-error
    // semantics); compatible → 0.
    Ok(if report.compatible {
        EXIT_OK
    } else {
        EXIT_CONFIG
    })
}

/// 人类摘要（stdout；错误/日志走 stderr）：身份、矩阵状态、扫描计数、逐条
/// 违规与告警。展示面从不回显源明文（violation 仅含版本串/稳定码/主体标识）。
/// 泛型写出器便于单测捕获。
/// The human summary (stdout; errors/logs go to stderr): identity, matrix state,
/// scan counts, per-line violations and warnings. The display never echoes
/// source plaintext (violations carry version strings/stable codes/subject
/// identities only). The generic writer keeps the face unit-testable.
fn print_summary(report: &CompatibilityReport) {
    let mut stdout = std::io::stdout();
    let _ = write_summary(&mut stdout, report);
}

/// [`print_summary`] 的写出体（泛型 writer；单测以 Vec<u8> 捕获断言）。
/// The write body of [`print_summary`] (a generic writer; tests capture into a
/// Vec<u8> for assertions).
fn write_summary<W: std::io::Write>(
    writer: &mut W,
    report: &CompatibilityReport,
) -> std::io::Result<()> {
    let verdict = if report.compatible {
        "compatible"
    } else {
        "INCOMPATIBLE"
    };
    writeln!(
        writer,
        "checked: pages={} tasks={}  result: {verdict} ({} violation(s), {} warning(s))",
        report.checked_pages,
        report.checked_tasks,
        report.violations.len(),
        report.warnings.len()
    )?;
    for violation in &report.violations {
        writeln!(
            writer,
            "violation: {} {} {} observed={} expected={}",
            violation.subject,
            violation.code,
            violation.field,
            violation.observed,
            violation.expected
        )?;
    }
    for warning in &report.warnings {
        writeln!(
            writer,
            "warning: {} domain={} pack={} schema={} prompt={} artifact={}",
            warning.code,
            warning.domain,
            warning.domain_pack_version,
            warning.schema_version.as_deref().unwrap_or("-"),
            warning.prompt_version.as_deref().unwrap_or("-"),
            warning.artifact_version
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use wiktor_core::types::PublishStatus;

    const DOMAIN: &str = "checkdom";

    /// 带兼容矩阵的 domain.yaml（strict semver；矩阵要求 pack >=2.0.0）。
    /// A domain.yaml with a compatibility matrix (strict semver; the matrix
    /// demands pack >=2.0.0).
    fn matrix_yaml() -> String {
        format!(
            "name: {DOMAIN}\n\
             version: \"2.1.0\"\n\
             compatibility:\n\
             \x20 domain_pack: \">=2.0.0,<3.0.0\"\n\
             \x20 schema: \">=2.0.0,<3.0.0\"\n\
             \x20 prompt: \">=3.0.0,<4.0.0\"\n\
             \x20 artifact: [\"wiki-v1\"]\n"
        )
    }

    /// 无 compatibility 段的 domain.yaml（legacy 容忍 + 告警路径）。
    /// A domain.yaml without a compatibility section (the legacy tolerance +
    /// warning path).
    fn legacy_yaml() -> String {
        format!("name: {DOMAIN}\nversion: \"2.1.0\"\n")
    }

    /// 解析 domain.yaml 文本（测试直接喂字符串，不落盘）。
    /// Parses domain.yaml text (tests feed strings directly, no disk).
    fn parse_config(yaml: &str) -> DomainConfig {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    /// seed 一条旧版本 accepted 页（domain_pack_version=0.1.0，越出矩阵范围）。
    /// Seeds an old-version accepted page (domain_pack_version=0.1.0, outside the
    /// matrix range).
    fn seed_old_page(kernel: &SqliteKernel) {
        let mut page = wiktor_core::seed::parse_page(
            "---\npage_id: checkdom:drink:boba\nentity_id: checkdom:drink:boba\ntitle: 波霸奶茶\nentity_type: drink\n---\n\n波霸奶茶是以红茶为基底加入波霸珍珠的经典奶茶。",
        )
        .unwrap();
        page.metadata.domain_pack_version = "0.1.0".into();
        kernel
            .seed_pages(&page, DOMAIN, PublishStatus::Accepted)
            .unwrap();
    }

    fn check_args(db: &Path, domain_yaml: &Path, json: bool) -> DomainCommand {
        DomainCommand::Check(CheckArgs {
            domain: domain_yaml.to_path_buf(),
            db: db.to_path_buf(),
            json,
        })
    }

    // A15：文件缺失 = 空库按配置本身检查 —— 兼容 → exit 0，且绝不创建 DB 文件
    // （只读契约）；矩阵缺失 → 告警附加但不翻转 compatible（STEP8-028 补偿）。
    // A15: a missing file checks the configuration itself against an empty
    // database — compatible → exit 0 and the DB file is never created (the
    // read-only contract); a missing matrix attaches the warning without flipping
    // compatible (the STEP8-028 compensation).
    #[tokio::test]
    async fn missing_db_checks_config_only_and_never_creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("absent.db");
        let yaml = dir.path().join("domain.yaml");
        std::fs::write(&yaml, matrix_yaml()).unwrap();

        assert_eq!(run(check_args(&db, &yaml, false)).await.unwrap(), EXIT_OK);
        assert!(!db.exists(), "a read-only check must not create the DB");

        // 矩阵缺失：告警附加、仍 compatible（评估核心直接断言）。
        // Missing matrix: the warning attaches and the report stays compatible
        // (asserted directly on the evaluation core).
        let report = evaluate(
            &parse_config(&legacy_yaml()),
            &SqliteKernel::open_in_memory().unwrap(),
        )
        .unwrap();
        assert!(report.compatible);
        assert_eq!(report.checked_pages, 0);
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, "MISSING_COMPATIBILITY_MATRIX");
        assert_eq!(report.warnings[0].domain, DOMAIN);
        // 告警经 --json 渲染为可直读 JSON；空告警时字段整体省略（§6.4 形状）。
        // Warnings render as directly readable JSON under --json; an empty list
        // omits the field entirely (the §6.4 shape).
        let with_warning = serde_json::to_string(&report).unwrap();
        assert!(with_warning.contains(r#""code":"MISSING_COMPATIBILITY_MATRIX""#));
        let bare_report = evaluate(
            &parse_config(&matrix_yaml()),
            &SqliteKernel::open_in_memory().unwrap(),
        )
        .unwrap();
        assert!(!serde_json::to_string(&bare_report)
            .unwrap()
            .contains("warnings"));
    }

    // A14/A16：不兼容 → exit 3；只读检查零写入（行数前后不变、无 review 行）；
    // 报告逐条列出违规。
    // A14/A16: incompatible → exit 3; the read-only check writes nothing (row
    // counts unchanged, no review rows); the report lists every violation.
    #[tokio::test]
    async fn incompatible_db_exits_three_without_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        let yaml = dir.path().join("domain.yaml");
        std::fs::write(&yaml, matrix_yaml()).unwrap();

        {
            // 预建当前 schema 库并 seed 一条旧版本页（真实 open，含迁移）。
            // Pre-create a current-schema DB and seed one old-version page (a
            // real open, with migration).
            let kernel = SqliteKernel::open(&db).unwrap();
            seed_old_page(&kernel);
        }
        let before = SqliteKernel::open(&db).unwrap().row_counts().unwrap();
        assert!(before["pages"] > 0, "precondition: the old page is seeded");

        let code = run(check_args(&db, &yaml, false)).await.unwrap();
        assert_eq!(code, EXIT_CONFIG, "an incompatible preflight exits 3");

        // A16：只读 check 不写任何行——行数逐表不变（无 review、无任务、无页）。
        // A16: a read-only check writes nothing — per-table row counts unchanged
        // (no reviews, no tasks, no pages).
        let after = SqliteKernel::open(&db).unwrap().row_counts().unwrap();
        assert_eq!(before, after, "the check must never write");
        assert_eq!(
            after["review_queue"], 0,
            "no compatibility review is written"
        );

        // 评估核心：报告 incompatible、计数准确、违规稳定码为 VERSION_RANGE。
        // The evaluation core: an incompatible report, exact counts, the stable
        // VERSION_RANGE code.
        let kernel = SqliteKernel::open_existing(&db).unwrap();
        let report = evaluate(&parse_config(&matrix_yaml()), &kernel).unwrap();
        assert!(!report.compatible);
        assert_eq!(report.checked_pages, 1);
        assert_eq!(report.checked_tasks, 0);
        assert!(report.warnings.is_empty(), "the matrix is present");
        assert!(report
            .violations
            .iter()
            .any(|v| v.subject == "page:checkdom:drink:boba"
                && v.code == "VERSION_RANGE"
                && v.field == "domain_pack_version"));
    }

    // 兼容库 → exit 0：页/任务身份全部满足矩阵（seed 页写当前 pack 版本；
    // artifact 与 quality_policy 手工对齐矩阵——seed_pages 落的是 seed-v1 缺省
    // 且不带 quality_policy，正向夹具需显式补齐）。
    // A compatible DB → exit 0: every page/task identity satisfies the matrix
    // (the seeded page carries the current pack version; artifact and
    // quality_policy are aligned with the matrix by hand — seed_pages defaults to
    // seed-v1 without a quality_policy, so the positive fixture fills them in).
    #[tokio::test]
    async fn compatible_db_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        let yaml = dir.path().join("domain.yaml");
        std::fs::write(&yaml, matrix_yaml()).unwrap();

        {
            let kernel = SqliteKernel::open(&db).unwrap();
            let mut page = wiktor_core::seed::parse_page(
                "---\npage_id: checkdom:drink:latte\nentity_id: checkdom:drink:latte\ntitle: 拿铁\nentity_type: drink\n---\n\n香浓拿铁。",
            )
            .unwrap();
            page.metadata.domain_pack_version = "2.1.0".into();
            kernel
                .seed_pages(&page, DOMAIN, PublishStatus::Accepted)
                .unwrap();
            kernel
                .execute_batch(
                    "UPDATE pages SET artifact_version = 'wiki-v1',
                            frontmatter_json = '{\"quality_policy\":{\"artifact_version\":\"wiki-v1\"}}'
                     WHERE domain = 'checkdom'",
                )
                .unwrap();
        }

        assert_eq!(run(check_args(&db, &yaml, false)).await.unwrap(), EXIT_OK);
    }

    // 配置/数据协议错误 → 3：domain.yaml 非 strict semver；DB 文件不是 Wiktor
    // 库（migration_required）。
    // Config/data-protocol errors → 3: a non-strict-semver domain.yaml; a DB
    // file that is not a Wiktor database (migration_required).
    #[tokio::test]
    async fn config_and_protocol_errors_exit_three() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wiktor.db");
        let bad_yaml = dir.path().join("bad.yaml");
        std::fs::write(&bad_yaml, "name: d\nversion: \"test-v1\"\n").unwrap();
        assert_eq!(
            run(check_args(&db, &bad_yaml, false)).await.unwrap(),
            EXIT_CONFIG,
            "a non-strict-semver identity is a config error"
        );

        let yaml = dir.path().join("domain.yaml");
        std::fs::write(&yaml, matrix_yaml()).unwrap();
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, "definitely not a sqlite database").unwrap();
        assert_eq!(
            run(check_args(&garbage, &yaml, false)).await.unwrap(),
            EXIT_CONFIG,
            "a non-Wiktor file is a data-protocol (migration_required) error"
        );
    }

    // 人类摘要：不回显源明文，逐条违规/告警可见（泛型写出器直接捕获断言）。
    // The human summary: never echoes source plaintext; violations/warnings are
    // all visible (captured straight from the generic writer).
    #[test]
    fn summary_lists_violations_and_warnings() {
        let identity = wiktor_core::compile::config::DomainIdentity::parse(
            DOMAIN, "2.1.0", None, None, "wiki-v1",
        )
        .unwrap();
        let report = CompatibilityReport {
            compatible: false,
            checked_pages: 1,
            checked_tasks: 0,
            violations: vec![
                wiktor_core::compile::compatibility::CompatibilityViolation {
                    subject: format!("page:{DOMAIN}:drink:boba"),
                    code: "VERSION_RANGE".into(),
                    field: "domain_pack_version".into(),
                    observed: "0.1.0".into(),
                    expected: ">=2.0.0,<3.0.0".into(),
                },
            ],
            warnings: vec![missing_matrix_warning(&identity)],
        };
        let mut buf: Vec<u8> = Vec::new();
        write_summary(&mut buf, &report).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("result: INCOMPATIBLE (1 violation(s), 1 warning(s))"));
        assert!(text.contains("page:checkdom:drink:boba VERSION_RANGE domain_pack_version"));
        assert!(text.contains("warning: MISSING_COMPATIBILITY_MATRIX"));
        assert!(text.contains("prompt=-"), "absent versions render as -");
    }
}
