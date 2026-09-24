//! `wiktor qug build` 与 `wiktor eval`（Step 5 spec §4.1 模块边界、§3 D7、
//! §7 A13/A14、§8 批6）共享的底座：退出码契约与分类器、领域包/intents 加载
//! 助手（错误统一收敛为 [`Error::InvalidConfig`]，走配置错误退出码 3）。
//!
//! The shared foundation of `wiktor qug build` and `wiktor eval` (Step 5 spec
//! §4.1 module boundary, §3 D7, §7 A13/A14, §8 batch 6): the exit-code
//! contract and classifier, plus domain-pack/intents loading helpers (errors
//! converge into [`Error::InvalidConfig`] so they take the config-error exit
//! code 3).
//!
//! 退出码（D7，build/eval/feedback 统一；feedback 的空报告/无建议同为 0）：
//! - 0 成功（包括 `qug_decision=disabled`——QUG 无收益不是失败）；
//! - 1 运行失败：`source_changed`、stale、查询失败、数据库故障、报告写盘失败等；
//! - 2 CLI 用法错误：参数缺失/越界（clap 自身的解析错误也以 2 退出）；
//! - 3 配置、迁移、golden 或边协议校验错误（`InvalidConfig`/`Validation`/
//!   `Migration`）；
//! - 4 未分类内部错误（`Internal`/`Serialization` 及其余未列出变体）。
//!
//! 前缀约定（对齐批2/3）：`Error::Validation` 的 Display 带
//! `"validation error: "` 外衣，因此 `source_changed`/`stale` 的判别用
//! `contains` 匹配内核稳定前缀（[`SOURCE_CHANGED_PREFIX`]/
//! [`QUG_STALE_PREFIX`]），绝不解析其余正文。
//!
//! Exit codes (D7, shared by build/eval/feedback; a feedback empty report / no
//! suggestions is a 0 too):
//! - 0 success (including `qug_decision=disabled` — a no-gain QUG is not a
//!   failure);
//! - 1 run failure: `source_changed`, stale, query failure, database faults,
//!   report-write failures, etc.;
//! - 2 CLI usage errors: missing/out-of-range arguments (clap's own parse
//!   errors also exit 2);
//! - 3 config, migration, golden or edge-protocol validation errors
//!   (`InvalidConfig`/`Validation`/`Migration`);
//! - 4 unclassified internal errors (`Internal`/`Serialization` and every
//!   other unlisted variant).
//!
//! Prefix convention (aligned with batches 2/3): `Error::Validation`'s Display
//! wraps the message in `"validation error: "`, so `source_changed`/`stale`
//! are detected via `contains` on the kernel's stable prefixes
//! ([`SOURCE_CHANGED_PREFIX`]/[`QUG_STALE_PREFIX`]) and the rest of the
//! message is never parsed.

pub mod domain;
pub mod eval;
pub mod feedback;
pub mod qug;

use std::path::Path;

use wiktor_core::kernel::qug_store::SOURCE_CHANGED_PREFIX;
use wiktor_core::kernel::QUG_STALE_PREFIX;
use wiktor_core::traits::DomainConfig;
use wiktor_core::types::error::Error;

/// 退出码：成功（包括 disabled 判定）。
/// Exit code: success (a disabled verdict included).
pub(crate) const EXIT_OK: i32 = 0;
/// 退出码：运行失败（source_changed/stale/查询失败/报告写盘失败等）。
/// Exit code: run failure (source_changed/stale/query failure/report-write
/// failure, etc.).
pub(crate) const EXIT_RUN_FAILURE: i32 = 1;
/// 退出码：CLI 用法错误（参数缺失/越界）。
/// Exit code: CLI usage error (missing/out-of-range arguments).
pub(crate) const EXIT_USAGE: i32 = 2;
/// 退出码：配置、迁移、golden 或边协议校验错误。
/// Exit code: config, migration, golden or edge-protocol validation error.
pub(crate) const EXIT_CONFIG: i32 = 3;
/// 退出码：未分类内部错误。
/// Exit code: unclassified internal error.
pub(crate) const EXIT_INTERNAL: i32 = 4;

/// 错误 → 退出码分类（D7；纯函数，单测覆盖 1/3/4 各路径）。
/// Error → exit-code classification (D7; pure function, unit tests cover the
/// 1/3/4 paths).
pub(crate) fn classify_error(err: &Error) -> i32 {
    match err {
        // source_changed / stale：稳定前缀命中 → 运行失败（绝不吞成 disabled）。
        // source_changed / stale: a stable-prefix hit → run failure (never
        // swallowed into disabled).
        Error::Validation(msg)
            if msg.contains(SOURCE_CHANGED_PREFIX) || msg.contains(QUG_STALE_PREFIX) =>
        {
            EXIT_RUN_FAILURE
        }
        // 其余 Validation（golden loader、intents、边协议）+ 配置 + 迁移 → 3。
        // Remaining Validation (golden loader, intents, edge protocol) + config
        // + migration → 3.
        Error::InvalidConfig(_) | Error::Validation(_) | Error::Migration(_) => EXIT_CONFIG,
        // 运行期故障：查询、数据库、IO（报告写盘）、向量服务 → 1。
        // Runtime faults: query, database, IO (report writes), vector service → 1.
        Error::Query(_)
        | Error::Database(_)
        | Error::Io(_)
        | Error::VectorStore(_)
        | Error::QdrantConnection(_)
        | Error::External(_) => EXIT_RUN_FAILURE,
        // Internal / Serialization / 其余未列出变体 = 未分类内部错误 → 4。
        // Internal / Serialization / every other unlisted variant = unclassified
        // internal error → 4.
        _ => EXIT_INTERNAL,
    }
}

/// 打印一条带命令前缀的错误到 stderr 并返回分类后的退出码。
/// Prints one command-prefixed error line to stderr and returns the classified
/// exit code.
pub(crate) fn report_error(command: &str, err: Error) -> i32 {
    eprintln!("wiktor {command}: {err}");
    classify_error(&err)
}

/// 领域包加载（配置阶段）：读/解析失败统一包成 `InvalidConfig`（→ 退出码 3）。
/// Domain-pack loading (config phase): read/parse failures are wrapped into
/// `InvalidConfig` (→ exit code 3).
pub(crate) fn load_domain_config_checked(path: &Path) -> Result<DomainConfig, Error> {
    let yaml_text = std::fs::read_to_string(path)
        .map_err(|e| Error::InvalidConfig(format!("read {}: {e}", path.display())))?;
    serde_yaml_ng::from_str(&yaml_text)
        .map_err(|e| Error::InvalidConfig(format!("parse {}: {e}", path.display())))
}

/// 读取 intents.yaml 原始 bytes（`qug.intent_templates` 指向的文件，相对
/// domain 目录解析；未配置 → 空 bytes，等价于无 intents.yaml）。读失败 →
/// `InvalidConfig`（→ 退出码 3）。
/// Reads the raw intents.yaml bytes (the file pointed to by
/// `qug.intent_templates`, resolved against the domain directory; unconfigured
/// → empty bytes, equivalent to no intents.yaml). Read failure →
/// `InvalidConfig` (→ exit code 3).
pub(crate) fn load_intents_bytes_checked(
    config: &DomainConfig,
    domain_dir: &Path,
) -> Result<Vec<u8>, Error> {
    match &config.qug.intent_templates {
        Some(rel) => {
            let path = domain_dir.join(rel);
            std::fs::read(&path).map_err(|e| {
                Error::InvalidConfig(format!("read intents.yaml {}: {e}", path.display()))
            })
        }
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A14：退出码映射 —— 1（source_changed/stale 前缀、查询、IO、向量服务）、
    /// 3（普通 Validation/InvalidConfig/Migration）、4（Internal/Serialization）。
    /// A14: exit-code mapping — 1 (source_changed/stale prefixes, query, IO,
    /// vector service), 3 (plain Validation/InvalidConfig/Migration), 4
    /// (Internal/Serialization).
    #[test]
    fn a14_exit_code_mapping_covers_paths_1_3_4() {
        // —— 1：source_changed / stale 稳定前缀（Validation 外衣内 contains）。——
        // —— 1: source_changed / stale stable prefixes (contains inside the
        //       Validation wrapper). ——
        assert_eq!(
            classify_error(&Error::Validation(format!(
                "{SOURCE_CHANGED_PREFIX} accepted page set drifted"
            ))),
            EXIT_RUN_FAILURE,
            "source_changed is a run failure, not a config error"
        );
        assert_eq!(
            classify_error(&Error::Validation(format!(
                "{QUG_STALE_PREFIX} hash mismatch"
            ))),
            EXIT_RUN_FAILURE,
            "stale is a run failure (eval must fail, not fall back)"
        );
        // —— 1：运行期故障变体。——
        // —— 1: runtime-fault variants. ——
        assert_eq!(
            classify_error(&Error::Query("eval: 1 of 134 golden queries failed".into())),
            EXIT_RUN_FAILURE
        );
        assert_eq!(
            classify_error(&Error::Io(std::io::Error::other("report write failed"))),
            EXIT_RUN_FAILURE,
            "report-write failure is a run failure"
        );
        assert_eq!(
            classify_error(&Error::VectorStore("mock collection missing".into())),
            EXIT_RUN_FAILURE
        );
        // —— 3：配置 / golden 校验 / 迁移。——
        // —— 3: config / golden validation / migration. ——
        assert_eq!(
            classify_error(&Error::Validation(
                "golden line 2: invalid JSON record".into()
            )),
            EXIT_CONFIG
        );
        assert_eq!(
            classify_error(&Error::InvalidConfig("parse domain.yaml: bad yaml".into())),
            EXIT_CONFIG
        );
        assert_eq!(
            classify_error(&Error::Migration("old schema".into())),
            EXIT_CONFIG
        );
        // —— 4：未分类内部错误。——
        // —— 4: unclassified internal errors. ——
        assert_eq!(
            classify_error(&Error::Internal("corrupt persisted edge payload".into())),
            EXIT_INTERNAL
        );
        let ser = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert_eq!(
            classify_error(&Error::Serialization(ser)),
            EXIT_INTERNAL,
            "serialization faults are unclassified internal errors"
        );
    }
}
