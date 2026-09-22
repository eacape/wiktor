//! Step 5 批1：QUG 五类边纯函数构建层（spec `step5-qug-build.md` §3 D1/D2、
//! §4.2/§4.3、§7 A1–A3、§9）。
//! Step 5 batch 1: pure-function build layer for the five QUG edge types
//! (spec `step5-qug-build.md` §3 D1/D2, §4.2/§4.3, §7 A1–A3, §9).
//!
//! 职责边界（spec §4.1：qug.rs 承担"纯边提取、规范化、图构造"，本模块为其
//! Step5 扩展，不触 SQLite、不触 CLI、禁止 LLM）：
//! - 从 `QugSourceSnapshot`（accepted 页 frontmatter_json + intents 原始 bytes）
//!   确定性派生五类边：Synonym=alias→title、Hyponym=title→tag（不自动全连接、
//!   不推断层级）；Attribute/Intent/Negation 逐条来自 intents.yaml；
//! - 单边 canonical JSON BLAKE3 `edge_hash`；D2 规范化输入按固定前缀 +
//!   u64 LE 长度域帧编码后 BLAKE3 小写 hex 得 `source_hash`（对齐
//!   `compile::hash` 惯例）；
//! - 重复边去重后按 edge_hash 确定性排序；所有计数有硬上限，超限报错不截断
//!   （spec §9）；非法 frontmatter、空/超长 phrase、未白名单 field、无法序列化
//!   edge 一律返回 `Validation`。
//! 存储与事务发布（`QugStore`）属于后续存储批次，不在本模块。
//! Responsibility boundary (spec §4.1: qug.rs owns "pure edge extraction,
//! normalization and graph construction"; this module is its Step5 extension —
//! no SQLite, no CLI, no LLM):
//! - deterministically derive the five edge types from a `QugSourceSnapshot`
//!   (accepted-page frontmatter_json + raw intents bytes): Synonym=alias→title,
//!   Hyponym=title→tag (no auto all-connect, no inferred hierarchy), while
//!   Attribute/Intent/Negation derive entry by entry from intents.yaml;
//! - per-edge canonical-JSON BLAKE3 `edge_hash`; the D2-normalized inputs are
//!   encoded with a fixed prefix + u64-LE length-field framing and hashed with
//!   BLAKE3 lowercase hex into `source_hash` (aligned with `compile::hash`);
//! - duplicates are deduplicated then deterministically ordered by edge_hash;
//!   every counter has a hard cap that errors out instead of truncating
//!   (spec §9); illegal frontmatter, empty/overlong phrases, un-whitelisted
//!   fields and unserializable edges all return `Validation`.
//! Storage and transactional publishing (`QugStore`) belong to the later
//! storage batch and are out of scope here.

use crate::compile::hash::canonical_json;
use crate::query_engine::qug::normalize;
use crate::traits::{DomainConfig, IntentConfig, QugConfig};
use crate::types::error::{Error, Result};
use crate::types::QugEdge;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// QUG 构建器版本（spec §4.2；参与 source_hash，任何派生行为变更必须换版本）。
/// QUG builder version (spec §4.2; participates in source_hash — bump it on any
/// derivation behavior change).
pub const QUG_BUILDER_VERSION: &str = "qug-build-v1";

/// source_hash 编码前缀（对齐 `compile::hash::HASH_PREFIX` 的固定前缀惯例）。
/// source_hash encoding prefix (aligned with the fixed-prefix convention of
/// `compile::hash::HASH_PREFIX`).
pub const QUG_HASH_PREFIX: &[u8] = b"wiktor.qug.hash.v1\0";

/// 快照页面数硬上限（超限报错不截断，spec §9）。
/// Hard cap on snapshot page count (errors instead of truncating, spec §9).
pub const MAX_QUUG_PAGES: usize = 100_000;

/// 单页 aliases/tags 数量硬上限（超限报错不截断，spec §9）。
/// Hard cap on aliases/tags per page (errors instead of truncating, spec §9).
pub const MAX_FRONTMATTER_LIST: usize = 64;

/// intents.yaml 原始 bytes 硬上限（1 MiB，超限报错不截断，spec §9）。
/// Hard cap on raw intents.yaml bytes (1 MiB; errors instead of truncating, spec §9).
pub const MAX_INTENTS_BYTES: usize = 1_048_576;

/// 意图条目数硬上限（超限报错不截断，spec §9）。
/// Hard cap on intent entry count (errors instead of truncating, spec §9).
pub const MAX_INTENT_ENTRIES: usize = 1_000;

/// 单条目短语数硬上限（超限报错不截断，spec §9）。
/// Hard cap on phrases per entry (errors instead of truncating, spec §9).
pub const MAX_INTENT_PHRASES: usize = 64;

/// 去重后总边数硬上限（超限报错不截断，spec §9）。
/// Hard cap on the deduplicated total edge count (errors instead of
/// truncating, spec §9).
pub const MAX_QUUG_EDGES: usize = 100_000;

/// 参与构建的单个 accepted 页输入（spec §4.2 类型契约；status 恒为 accepted，
/// 由快照组装方保证，不入结构）。
/// One accepted-page input participating in the build (spec §4.2 type contract;
/// status is always `accepted`, guaranteed by the snapshot assembler and thus
/// not part of the struct).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QugPageInput {
    pub page_id: String,
    pub generation: i64,
    pub content_hash: String,
    pub artifact_version: String,
    /// 页面 frontmatter JSON 文本（读取形状见 `StoredPageFrontmatter`：
    /// title/aliases/tags；refs/quality_policy 等额外键被忽略；空对象 `{}` 为
    /// legacy seed 页合法形状，产生零页面边）。
    /// The page's frontmatter JSON text (read shape in `StoredPageFrontmatter`:
    /// title/aliases/tags; extra keys such as refs/quality_policy are ignored;
    /// an empty object `{}` is the legal legacy-seed shape yielding zero edges).
    pub frontmatter_json: String,
}

/// QUG 构建来源快照（spec D2 规范化输入的载体）。
/// QUG build source snapshot (carrier of the D2-normalized inputs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QugSourceSnapshot {
    pub domain: String,
    pub domain_version: String,
    /// `QugConfig` 的 JSON 文本（D2：以 canonical 形式纳入 source_hash）。
    /// The `QugConfig` JSON text (D2: hashed canonically into source_hash).
    pub qug_config_json: String,
    /// intents.yaml 原始 bytes（D2：原文入哈希，任何字节变化即失效）。
    /// Raw intents.yaml bytes (D2: hashed as-is — any byte change invalidates).
    pub intents_bytes: Vec<u8>,
    /// 指定 domain 下全部 accepted 页（seed 与 compiled 并集；相同 page_id 只取
    /// accepted head，spec D4）。
    /// All accepted pages of the domain (seed ∪ compiled; one accepted head per
    /// page_id, spec D4).
    pub pages: Vec<QugPageInput>,
}

/// 待持久化的页面边（保留 pages FK 语义，spec §4.2/D3）。
/// A page edge to persist (keeps the pages FK semantics, spec §4.2/D3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPageEdge {
    pub page_id: String,
    pub edge: QugEdge,
    pub edge_hash: String,
    pub generation: i64,
    pub content_hash: String,
}

/// 待持久化的配置边（无 page_id 归属，独立表，spec §4.2/D3）。
/// A config edge to persist (no page attribution, dedicated table, spec §4.2/D3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIntentEdge {
    pub edge: QugEdge,
    pub edge_hash: String,
}

/// 一次构建的统计与身份（spec §4.2；`build_id` 由存储批次填充）。
/// Stats and identity of one build (spec §4.2; `build_id` is filled by the
/// storage batch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QugBuildStats {
    pub build_id: i64,
    pub reused: bool,
    pub source_hash: String,
    pub accepted_page_count: usize,
    pub edge_count: usize,
    pub by_type: BTreeMap<String, usize>,
}

/// 构建结果：hash 命中复用或新代次发布（spec §4.2）。
/// Build outcome: hash-hit reuse or a newly published generation (spec §4.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QugBuildOutcome {
    Reused(QugBuildStats),
    Published(QugBuildStats),
}

/// 纯函数派生产物：两类持久化边 + source_hash + 分型计数（存储批次的发布输入）。
/// Pure-derivation artifact: both persisted-edge lists + source_hash + per-type
/// counts (the publish input of the storage batch).
#[derive(Debug, Clone)]
pub struct QugDerivedEdges {
    pub page_edges: Vec<PersistedPageEdge>,
    pub intent_edges: Vec<PersistedIntentEdge>,
    pub source_hash: String,
    pub accepted_page_count: usize,
    pub by_type: BTreeMap<String, usize>,
}

impl QugDerivedEdges {
    /// 去重后的总边数。
    /// The deduplicated total edge count.
    pub fn edge_count(&self) -> usize {
        self.page_edges.len() + self.intent_edges.len()
    }
}

/// 解析并做结构校验 intents.yaml 原始 bytes（spec D1：禁止 LLM，机械解析）。
/// Parses raw intents.yaml bytes with structural validation (spec D1: no LLM,
/// mechanical parsing only).
///
/// 空/全空白 bytes 是合法输入（领域未配置 intents.yaml），返回空 `IntentConfig`；
/// 超长 bytes、非法 YAML、条目/短语超上限、空或超长短语返回 `Validation`。
/// 白名单/reflist/exactly-one-rule 语义校验在 `derive_qug_edges` 内收敛 Step3
/// 的 `IntentConfig::validate`。
/// Empty/whitespace-only bytes are legal (a domain without intents.yaml) and
/// yield an empty `IntentConfig`; oversized bytes, illegal YAML, over-cap
/// entries/phrases and empty/overlong phrases return `Validation`. Whitelist/
/// reflist/exactly-one-rule semantic validation is converged with Step3's
/// `IntentConfig::validate` inside `derive_qug_edges`.
pub fn parse_intents(bytes: &[u8]) -> Result<IntentConfig> {
    if bytes.len() > MAX_INTENTS_BYTES {
        return Err(Error::Validation(format!(
            "qug build: intents.yaml {} bytes exceeds hard cap {MAX_INTENTS_BYTES}; refusing to truncate",
            bytes.len()
        )));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        // 空意图文件 = 无配置边（空 accepted 页面允许构建的同源语义，spec §4.2）。
        // An empty intent file = no config edges (same semantics as "empty
        // accepted pages may still build", spec §4.2).
        return Ok(IntentConfig {
            version: String::new(),
            intents: Vec::new(),
        });
    }
    let intents: IntentConfig = serde_yaml_ng::from_slice(bytes)
        .map_err(|e| Error::Validation(format!("qug build: invalid intents.yaml: {e}")))?;
    if intents.intents.len() > MAX_INTENT_ENTRIES {
        return Err(Error::Validation(format!(
            "qug build: {} intent entries exceeds hard cap {MAX_INTENT_ENTRIES}; refusing to truncate",
            intents.intents.len()
        )));
    }
    for entry in &intents.intents {
        if entry.phrases.len() > MAX_INTENT_PHRASES {
            return Err(Error::Validation(format!(
                "qug build: intent {} has {} phrases exceeding hard cap {MAX_INTENT_PHRASES}; refusing to truncate",
                entry.id,
                entry.phrases.len()
            )));
        }
        // Step5 错误面前移：空/超长 phrase 在构建期报 Validation，而非等到
        // 查询期构图失败（spec §4.2）。
        // Step5 front-shifts the error surface: empty/overlong phrases fail with
        // Validation at build time, not at query-time graph construction (spec §4.2).
        for phrase in &entry.phrases {
            normalize(phrase)
                .map_err(|e| Error::Validation(format!("qug build: intent {}: {e}", entry.id)))?;
        }
    }
    Ok(intents)
}

/// 纯函数主入口：从快照确定性派生五类边并计算 source_hash（spec D1/D2）。
/// The pure-function main entry: deterministically derives the five edge types
/// from the snapshot and computes the source_hash (spec D1/D2).
///
/// 输出顺序确定性：页面边与配置边各自去重后按 `edge_hash` 升序排序；重复边
/// 去重键为 `edge_hash`（页面边并列时保留 UTF-8 序最小的 page_id）。
/// Output ordering is deterministic: page edges and config edges are each
/// deduplicated then sorted ascending by `edge_hash`; the dedup key is
/// `edge_hash` (ties among page edges keep the smallest UTF-8 page_id).
pub fn derive_qug_edges(
    snapshot: &QugSourceSnapshot,
    config: &DomainConfig,
    intents: &IntentConfig,
) -> Result<QugDerivedEdges> {
    validate_snapshot(snapshot)?;
    // 配置边语义校验收敛 Step3（exactly-one-rule / reflist / query.filters
    // 白名单），Step5 错误面统一为 Validation（spec §4.2）。
    // Config-edge semantic validation converges with Step3 (exactly-one-rule /
    // reflist / query.filters whitelist); the Step5 error surface is uniformly
    // Validation (spec §4.2).
    intents
        .validate("intents.yaml", config)
        .map_err(|e| Error::Validation(format!("qug build: {e}")))?;
    // 防御性重校验短语（调用方可能绕过 parse_intents 手工构造 IntentConfig）。
    // Defensive phrase re-check (callers may hand-build IntentConfig bypassing
    // parse_intents).
    for entry in &intents.intents {
        for phrase in &entry.phrases {
            normalize(phrase)
                .map_err(|e| Error::Validation(format!("qug build: intent {}: {e}", entry.id)))?;
        }
    }

    let intent_edges = derive_intent_edges(intents, config)?;
    let page_edges = derive_page_edges(&snapshot.pages)?;

    let page_edges = dedup_page_edges(page_edges);
    let intent_edges = dedup_intent_edges(intent_edges);

    let total = page_edges.len() + intent_edges.len();
    if total > MAX_QUUG_EDGES {
        return Err(Error::Validation(format!(
            "qug build: derived {total} edges exceeds hard cap {MAX_QUUG_EDGES}; refusing to truncate"
        )));
    }

    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    for edge in page_edges
        .iter()
        .map(|p| &p.edge)
        .chain(intent_edges.iter().map(|p| &p.edge))
    {
        *by_type.entry(edge_type_name(edge).to_string()).or_insert(0) += 1;
    }

    let source_hash = compute_source_hash(snapshot, &page_edges, &intent_edges)?;
    Ok(QugDerivedEdges {
        accepted_page_count: snapshot.pages.len(),
        page_edges,
        intent_edges,
        source_hash,
        by_type,
    })
}

/// 单边 canonical JSON BLAKE3 小写 hex（spec D2"有效 edge 的 canonical JSON
/// 纳入哈希"的单边形态；content-address 键，无前缀）。
/// Per-edge canonical-JSON BLAKE3 lowercase hex (the single-edge form of spec
/// D2's "valid-edge canonical JSON enters the hash"; a content-address key
/// without prefix).
pub fn edge_hash(edge: &QugEdge) -> Result<String> {
    Ok(blake3::hash(&edge_canonical_bytes(edge)?)
        .to_hex()
        .to_string())
}

/// 边的 canonical JSON 文本（存储批次写 `edge_json` TEXT 的唯一来源，与
/// `edge_hash` 基于同一 `serde_json::Value`，保证载荷可复核）。
/// The edge's canonical JSON text (the single source for the storage batch's
/// `edge_json` TEXT, derived from the same `serde_json::Value` as `edge_hash`
/// so payloads stay auditable).
pub fn edge_canonical_json(edge: &QugEdge) -> Result<String> {
    // 无法序列化的 edge 属于非法输入 → Validation（spec §4.2）。
    // An unserializable edge is illegal input → Validation (spec §4.2).
    let value = edge_value(edge)?;
    Ok(serde_json::to_string(&value)?)
}

/// 页面边/配置边无法序列化的统一错误面：Validation。
/// Unified error surface for unserializable page/config edges: Validation.
fn edge_value(edge: &QugEdge) -> Result<Value> {
    serde_json::to_value(edge)
        .map_err(|e| Error::Validation(format!("qug build: edge is not serializable: {e}")))
}

/// 单边 canonical 字节（edge_hash 与 source_hash 的共享底座）。
/// Per-edge canonical bytes (the shared base of edge_hash and source_hash).
fn edge_canonical_bytes(edge: &QugEdge) -> Result<Vec<u8>> {
    canonical_json(&edge_value(edge)?)
}

/// 页面 frontmatter 的规范写出口径（存储批次组装 `QugPageInput` 时复用；
/// 解析侧 `StoredPageFrontmatter` 是其读取镜像）。
/// The canonical writer shape for page frontmatter (reused by the storage batch
/// when assembling `QugPageInput`; the parse side `StoredPageFrontmatter` is its
/// read mirror).
pub fn page_frontmatter_json(title: &str, aliases: &[String], tags: &[String]) -> Result<String> {
    // serde_json::Map 默认 BTreeMap：键按 UTF-8 字节序输出，文本确定。
    // serde_json::Map is a BTreeMap by default: keys emit in UTF-8 byte order,
    // so the text is deterministic.
    Ok(serde_json::to_string(&serde_json::json!({
        "title": title,
        "aliases": aliases,
        "tags": tags,
    }))?)
}

/// `frontmatter_json` 读取形状（宽松：额外键忽略；title 缺省表示 legacy seed
/// 空 frontmatter）。
/// Read shape of `frontmatter_json` (lenient: extra keys ignored; a missing
/// title denotes the legacy-seed empty frontmatter).
#[derive(Debug, Default, Deserialize)]
struct StoredPageFrontmatter {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

/// 快照基础校验：domain/版本非空、页面上限、page 身份字段、page_id 唯一
/// （D4：相同 page_id 只取 accepted head，重复即组装错误）。
/// Snapshot sanity validation: non-empty domain/version, page cap, page identity
/// fields, unique page_id (D4: one accepted head per page_id — duplicates are an
/// assembler bug).
fn validate_snapshot(snapshot: &QugSourceSnapshot) -> Result<()> {
    if snapshot.domain.trim().is_empty() {
        return Err(Error::Validation("qug build: domain name is empty".into()));
    }
    if snapshot.domain_version.trim().is_empty() {
        return Err(Error::Validation(
            "qug build: domain version is empty".into(),
        ));
    }
    if snapshot.pages.len() > MAX_QUUG_PAGES {
        return Err(Error::Validation(format!(
            "qug build: {} pages exceeds hard cap {MAX_QUUG_PAGES}; refusing to truncate",
            snapshot.pages.len()
        )));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for page in &snapshot.pages {
        if page.page_id.trim().is_empty() {
            return Err(Error::Validation("qug build: page_id is empty".into()));
        }
        if page.generation < 1 {
            return Err(Error::Validation(format!(
                "qug build: page {} generation {} out of range 1..=",
                page.page_id, page.generation
            )));
        }
        if page.content_hash.is_empty() {
            return Err(Error::Validation(format!(
                "qug build: page {} content_hash is empty",
                page.page_id
            )));
        }
        if page.artifact_version.is_empty() {
            return Err(Error::Validation(format!(
                "qug build: page {} artifact_version is empty",
                page.page_id
            )));
        }
        if !seen.insert(page.page_id.as_str()) {
            return Err(Error::Validation(format!(
                "qug build: duplicate accepted page_id {} (one accepted head per page_id)",
                page.page_id
            )));
        }
    }
    Ok(())
}

/// 页面边派生（D1 机械规则，收敛 Step3 `extract_page_edges` 的跳过语义并按
/// Step5 收紧错误面）：alias→title 的 Synonym（alias==title 跳过）、
/// title→tag 的 Hyponym；空 `{}` frontmatter（legacy seed）产生零边。
/// Page-edge derivation (D1 mechanical rules, converging Step3's
/// `extract_page_edges` skip semantics with Step5's stricter error surface):
/// Synonym alias→title (alias==title skipped) and Hyponym title→tag; an empty
/// `{}` frontmatter (legacy seed) yields zero edges.
fn derive_page_edges(pages: &[QugPageInput]) -> Result<Vec<PersistedPageEdge>> {
    let mut out = Vec::new();
    for page in pages {
        let fm: StoredPageFrontmatter =
            serde_json::from_str(&page.frontmatter_json).map_err(|e| {
                Error::Validation(format!(
                    "qug build: page {}: invalid frontmatter_json: {e}",
                    page.page_id
                ))
            })?;
        let title = match fm.title.as_deref() {
            Some(raw) => {
                let t = raw.trim();
                if t.is_empty() {
                    return Err(Error::Validation(format!(
                        "qug build: page {}: frontmatter title is empty",
                        page.page_id
                    )));
                }
                t.to_string()
            }
            // 无 title 且带 aliases/tags：无法锚定边 → 非法 frontmatter。
            // No title but aliases/tags present: edges cannot be anchored →
            // illegal frontmatter.
            None if !fm.aliases.is_empty() || !fm.tags.is_empty() => {
                return Err(Error::Validation(format!(
                    "qug build: page {}: frontmatter aliases/tags require a title",
                    page.page_id
                )));
            }
            // legacy seed 空 frontmatter：零页面边，不阻断构建（spec §4.2）。
            // Legacy-seed empty frontmatter: zero page edges, build continues (spec §4.2).
            None => continue,
        };
        if fm.aliases.len() > MAX_FRONTMATTER_LIST {
            return Err(Error::Validation(format!(
                "qug build: page {} has {} aliases exceeding hard cap {MAX_FRONTMATTER_LIST}; refusing to truncate",
                page.page_id,
                fm.aliases.len()
            )));
        }
        if fm.tags.len() > MAX_FRONTMATTER_LIST {
            return Err(Error::Validation(format!(
                "qug build: page {} has {} tags exceeding hard cap {MAX_FRONTMATTER_LIST}; refusing to truncate",
                page.page_id,
                fm.tags.len()
            )));
        }
        for alias in &fm.aliases {
            let a = alias.trim();
            if a.is_empty() {
                return Err(Error::Validation(format!(
                    "qug build: page {}: alias is empty",
                    page.page_id
                )));
            }
            normalize(a).map_err(|e| {
                Error::Validation(format!("qug build: page {}: alias {e}", page.page_id))
            })?;
            // 自指同义无意义（对齐 Step3 跳过语义；空/超长已在上一步报错）。
            // Self-referential synonymy is meaningless (Step3 skip semantics;
            // empty/overlong already errored above).
            if a == title {
                continue;
            }
            let edge = QugEdge::Synonym {
                from: a.to_string(),
                to: vec![title.clone()],
            };
            out.push(PersistedPageEdge {
                edge_hash: edge_hash(&edge)?,
                page_id: page.page_id.clone(),
                generation: page.generation,
                content_hash: page.content_hash.clone(),
                edge,
            });
        }
        for tag in &fm.tags {
            let t = tag.trim();
            if t.is_empty() {
                return Err(Error::Validation(format!(
                    "qug build: page {}: tag is empty",
                    page.page_id
                )));
            }
            normalize(t).map_err(|e| {
                Error::Validation(format!("qug build: page {}: tag {e}", page.page_id))
            })?;
            let edge = QugEdge::Hyponym {
                child: title.clone(),
                parent: t.to_string(),
            };
            out.push(PersistedPageEdge {
                edge_hash: edge_hash(&edge)?,
                page_id: page.page_id.clone(),
                generation: page.generation,
                content_hash: page.content_hash.clone(),
                edge,
            });
        }
    }
    Ok(out)
}

/// 配置边派生：复用并收敛 Step3 `intent_edges`（attribute 数值映射、negation
/// RefExcludes、expansion 复合展开），InvalidConfig 统一映射为 Validation
/// （spec §4.2 错误面）。
/// Config-edge derivation: reuses and converges Step3's `intent_edges`
/// (attribute numeric mapping, negation RefExcludes, expansion compound) with
/// InvalidConfig uniformly mapped to Validation (spec §4.2 error surface).
fn derive_intent_edges(
    intents: &IntentConfig,
    config: &DomainConfig,
) -> Result<Vec<PersistedIntentEdge>> {
    let edges = crate::query_engine::qug::intent_edges(intents, config)
        .map_err(|e| Error::Validation(format!("qug build: {e}")))?;
    edges
        .into_iter()
        .map(|edge| {
            let edge_hash = edge_hash(&edge)?;
            Ok(PersistedIntentEdge { edge, edge_hash })
        })
        .collect()
}

/// 页面边去重：按 (edge_hash, page_id, generation) 排序后以 edge_hash 去重
///（并列保留 UTF-8 序最小的 page_id）；结果保持 edge_hash 升序。
/// Page-edge dedup: sort by (edge_hash, page_id, generation), then dedup by
/// edge_hash (ties keep the smallest UTF-8 page_id); output stays in ascending
/// edge_hash order.
fn dedup_page_edges(mut edges: Vec<PersistedPageEdge>) -> Vec<PersistedPageEdge> {
    edges.sort_by(|a, b| {
        a.edge_hash
            .cmp(&b.edge_hash)
            .then_with(|| a.page_id.cmp(&b.page_id))
            .then_with(|| a.generation.cmp(&b.generation))
    });
    edges.dedup_by(|a, b| a.edge_hash == b.edge_hash);
    edges
}

/// 配置边去重：保留配置文件首次出现序，再按 edge_hash 升序。
/// Config-edge dedup: keep first-occurrence order from the config file, then
/// sort ascending by edge_hash.
fn dedup_intent_edges(mut edges: Vec<PersistedIntentEdge>) -> Vec<PersistedIntentEdge> {
    let mut seen: HashSet<String> = HashSet::new();
    edges.retain(|e| seen.insert(e.edge_hash.clone()));
    edges.sort_by(|a, b| a.edge_hash.cmp(&b.edge_hash));
    edges
}

/// 边类型的稳定命名（与 QugEdge serde snake_case tag 一致，进 counts_json/by_type；
/// `pub`：存储批次 publish 校验读回边后按同一命名复核五类计数，禁止双实现漂移）。
/// Stable edge-type names (matching the QugEdge serde snake_case tags; used in
/// counts_json/by_type; `pub` so the storage batch's publish validation can
/// re-tally the five type counts from read-back edges with the same naming —
/// never a drifting duplicate).
pub fn edge_type_name(edge: &QugEdge) -> &'static str {
    match edge {
        QugEdge::Synonym { .. } => "synonym",
        QugEdge::Hyponym { .. } => "hyponym",
        QugEdge::AttributePropagation { .. } => "attribute_propagation",
        QugEdge::IntentTemplate { .. } => "intent_template",
        QugEdge::Negation { .. } => "negation",
    }
}

/// u64 LE 长度域写入（对齐 `compile::hash` 的帧编码）。
/// Writes a u64-LE length field (aligned with `compile::hash` framing).
fn write_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_le_bytes());
}

/// 长度域 + 载荷写入。
/// Writes a length field followed by the payload.
fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

/// 命名域帧：域名长度域 + 域名 + 载荷长度域 + 载荷。
/// Named domain frame: name length + name + payload length + payload.
fn write_domain(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    write_bytes(out, name.as_bytes());
    write_bytes(out, payload);
}

/// D2 规范化 source_hash：固定前缀 `wiktor.qug.hash.v1\0`，域序固定
///（domain_name、domain_version、builder_version、qug_config canonical、
/// intents 原始 bytes、按 UTF-8 page_id 排序的页面五元组、按 edge_hash 排序的
/// 全部有效边 canonical JSON），BLAKE3 小写 hex。
/// The D2-normalized source_hash: fixed prefix `wiktor.qug.hash.v1\0` with fixed
/// domain order (domain_name, domain_version, builder_version, qug_config
/// canonical, raw intents bytes, page 5-tuples sorted by UTF-8 page_id, and all
/// valid edges' canonical JSON sorted by edge_hash), hashed with BLAKE3
/// lowercase hex.
///
/// `pub`：存储批次 `publish_build` 在事务内对 (snapshot, edges) 重算同一 hash
/// 落库（spec D2"校验五类计数与 hash"），与派生侧共用同一实现。
/// `pub`: the storage batch's `publish_build` recomputes the same hash over
/// (snapshot, edges) inside the transaction (spec D2 "validate the five type
/// counts and hash"), sharing one implementation with the derivation side.
pub fn compute_source_hash(
    snapshot: &QugSourceSnapshot,
    page_edges: &[PersistedPageEdge],
    intent_edges: &[PersistedIntentEdge],
) -> Result<String> {
    // qug_config_json 必须能还原为 QugConfig（非法配置 JSON → Validation）。
    // qug_config_json must round-trip into QugConfig (illegal config JSON →
    // Validation).
    let qug_config: QugConfig = serde_json::from_str(&snapshot.qug_config_json)
        .map_err(|e| Error::Validation(format!("qug build: invalid qug_config_json: {e}")))?;
    let qug_config_canon = canonical_json(&serde_json::to_value(&qug_config)?)?;

    // 页面五元组按 UTF-8 page_id 排序（generation 固定 8 字节 LE，其余长度域）。
    // Page 5-tuples sorted by UTF-8 page_id (generation as fixed 8-byte LE, the
    // rest length-framed).
    let mut sorted_pages: Vec<&QugPageInput> = snapshot.pages.iter().collect();
    sorted_pages.sort_by(|a, b| a.page_id.cmp(&b.page_id));
    let mut pages_payload = Vec::new();
    for page in sorted_pages {
        write_bytes(&mut pages_payload, page.page_id.as_bytes());
        pages_payload.extend_from_slice(&page.generation.to_le_bytes());
        write_bytes(&mut pages_payload, page.content_hash.as_bytes());
        write_bytes(&mut pages_payload, page.artifact_version.as_bytes());
        write_bytes(&mut pages_payload, page.frontmatter_json.as_bytes());
    }

    // 有效边 canonical JSON：两个列表已各自按 edge_hash 升序，这里稳定归并为
    // 全局 edge_hash 序。
    // Valid-edge canonical JSON: both lists are already ascending by edge_hash;
    // stably merge them into the global edge_hash order.
    let mut edge_items: Vec<(&str, Vec<u8>)> = Vec::new();
    for p in page_edges {
        edge_items.push((p.edge_hash.as_str(), edge_canonical_bytes(&p.edge)?));
    }
    for p in intent_edges {
        edge_items.push((p.edge_hash.as_str(), edge_canonical_bytes(&p.edge)?));
    }
    edge_items.sort_by(|a, b| a.0.cmp(b.0));
    let mut edges_payload = Vec::new();
    for (hash, bytes) in &edge_items {
        write_bytes(&mut edges_payload, hash.as_bytes());
        write_bytes(&mut edges_payload, bytes);
    }

    let mut framed = Vec::from(QUG_HASH_PREFIX);
    write_domain(&mut framed, "domain_name", snapshot.domain.as_bytes());
    write_domain(
        &mut framed,
        "domain_version",
        snapshot.domain_version.as_bytes(),
    );
    write_domain(
        &mut framed,
        "builder_version",
        QUG_BUILDER_VERSION.as_bytes(),
    );
    write_domain(&mut framed, "qug_config", &qug_config_canon);
    write_domain(&mut framed, "intents", &snapshot.intents_bytes);
    write_domain(&mut framed, "pages", &pages_payload);
    write_domain(&mut framed, "edges", &edges_payload);
    Ok(blake3::hash(&framed).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTENTS_YAML: &str = r#"version: "0.1.0"
intents:
  - id: cold_drink
    phrases: ["冰的", "冷饮"]
    expansion:
      text: "冰饮"
  - id: low_sugar
    phrases: ["不甜的", "少糖"]
    attribute:
      field: sugar_level
      max: 30
  - id: no_pearl
    phrases: ["不要珍珠"]
    negation:
      field: ingredient_ids
      refs: ["milk-tea:ingredient:pearl"]
"#;

    fn fixture_config() -> DomainConfig {
        let yaml = r#"name: milk-tea
version: "0.1.0"
entities:
  - name: drink
    source: jsonl://fixture
    id_field: id
    type_field: type
    fields:
      - name: price
        field_type: numeric
        filterable: true
      - name: sugar_level
        field_type: numeric
        filterable: true
      - name: ingredient_ids
        field_type: reflist
        filterable: true
query:
  filters: [price, sugar_level, ingredient_ids]
qug:
  enabled: true
  max_depth: 2
  candidate_multiplier: 5
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn fixture_pages() -> Vec<QugPageInput> {
        let fm1 = page_frontmatter_json(
            "珍珠奶茶",
            &["boba".to_string(), "波霸奶茶".to_string()],
            &["奶茶".to_string()],
        )
        .unwrap();
        let fm2 = page_frontmatter_json(
            "乌龙奶茶",
            &["乌龙".to_string()],
            &["奶茶".to_string(), "茶底".to_string()],
        )
        .unwrap();
        vec![
            QugPageInput {
                page_id: "p1".into(),
                generation: 1,
                content_hash: "h1".into(),
                artifact_version: "seed-v1".into(),
                frontmatter_json: fm1,
            },
            QugPageInput {
                page_id: "p2".into(),
                generation: 3,
                content_hash: "h2".into(),
                artifact_version: "domain-pack-0.1.0".into(),
                frontmatter_json: fm2,
            },
            // legacy seed 空 frontmatter：合法，零页面边。
            // Legacy-seed empty frontmatter: legal, zero page edges.
            QugPageInput {
                page_id: "p3".into(),
                generation: 1,
                content_hash: "h3".into(),
                artifact_version: "seed-v1".into(),
                frontmatter_json: "{}".into(),
            },
        ]
    }

    fn fixture() -> (QugSourceSnapshot, DomainConfig, IntentConfig) {
        let config = fixture_config();
        let snapshot = QugSourceSnapshot {
            domain: "milk-tea".into(),
            domain_version: "0.1.0".into(),
            qug_config_json: serde_json::to_string(&config.qug).unwrap(),
            intents_bytes: INTENTS_YAML.as_bytes().to_vec(),
            pages: fixture_pages(),
        };
        let intents = parse_intents(&snapshot.intents_bytes).unwrap();
        (snapshot, config, intents)
    }

    fn assert_lower_hex64(hash: &str) {
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    // A3：fixture 五类边各 ≥1，计数、分型统计与总量确定。
    // A3: the fixture yields ≥1 edge per type with deterministic counts, per-type
    // stats and total.
    #[test]
    fn five_edge_types_and_counts() {
        let (snapshot, config, intents) = fixture();
        let derived = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        assert_eq!(derived.accepted_page_count, 3);
        // 页面边：p1 两个 alias + 一个 tag，p2 一个 alias + 两个 tag，p3 空。
        // Page edges: p1 two aliases + one tag, p2 one alias + two tags, p3 empty.
        assert_eq!(derived.page_edges.len(), 6);
        // 配置边：expansion 2 短语 + attribute 2 短语 + negation 1 短语。
        // Intent edges: expansion 2 phrases + attribute 2 phrases + negation 1 phrase.
        assert_eq!(derived.intent_edges.len(), 5);
        assert_eq!(derived.edge_count(), 11);
        assert_eq!(derived.by_type.get("synonym"), Some(&3));
        assert_eq!(derived.by_type.get("hyponym"), Some(&3));
        assert_eq!(derived.by_type.get("intent_template"), Some(&2));
        assert_eq!(derived.by_type.get("attribute_propagation"), Some(&2));
        assert_eq!(derived.by_type.get("negation"), Some(&1));
        assert_eq!(derived.by_type.values().sum::<usize>(), 11);
        assert_lower_hex64(&derived.source_hash);
        // 方向契约：Synonym=alias→title、Hyponym=title→tag（不全连接、不推断层级）。
        // Direction contract: Synonym=alias→title, Hyponym=title→tag (no
        // all-connect, no inferred hierarchy).
        assert!(derived.page_edges.iter().any(|p| matches!(
            p.edge,
            QugEdge::Synonym { ref from, ref to }
                if from == "boba" && to.len() == 1 && to[0] == "珍珠奶茶"
        )));
        assert!(derived.page_edges.iter().any(|p| matches!(
            p.edge,
            QugEdge::Hyponym { ref child, ref parent }
                if child == "乌龙奶茶" && parent == "茶底"
        )));
        // 页面边只含 Synonym/Hyponym 两型（配置边不挂 page_id）。
        // Page edges only carry Synonym/Hyponym (config edges carry no page_id).
        assert!(derived
            .page_edges
            .iter()
            .all(|p| matches!(p.edge, QugEdge::Synonym { .. } | QugEdge::Hyponym { .. })));
    }

    // A1：同一输入两次派生 → source_hash 与边集完全一致（复用判定的纯函数基础）。
    // A1: deriving the same input twice → identical source_hash and edge set
    // (the pure-function basis of reuse detection).
    #[test]
    fn same_input_same_hash() {
        let (snapshot, config, intents) = fixture();
        let a = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        let b = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        assert_eq!(a.source_hash, b.source_hash);
        let ha: Vec<&str> = a.page_edges.iter().map(|p| p.edge_hash.as_str()).collect();
        let hb: Vec<&str> = b.page_edges.iter().map(|p| p.edge_hash.as_str()).collect();
        assert_eq!(ha, hb);
        let ia: Vec<&str> = a
            .intent_edges
            .iter()
            .map(|p| p.edge_hash.as_str())
            .collect();
        let ib: Vec<&str> = b
            .intent_edges
            .iter()
            .map(|p| p.edge_hash.as_str())
            .collect();
        assert_eq!(ia, ib);
    }

    // A2：任一规范化输入变化 → source_hash 变化（builder version 是编译期常量，
    // 变更即换版本，无法运行时变异）。
    // A2: any normalized-input change → a different source_hash (builder version
    // is a compile-time constant; a change means a version bump and cannot be
    // mutated at runtime).
    #[test]
    fn any_input_change_changes_hash() {
        let (base, config, intents) = fixture();
        let base_hash = derive_qug_edges(&base, &config, &intents)
            .unwrap()
            .source_hash;

        type SnapshotMutation = Box<dyn Fn(&mut QugSourceSnapshot)>;
        let mutations: Vec<SnapshotMutation> = vec![
            // 页面 content_hash 变化。
            // Page content_hash change.
            Box::new(|s: &mut QugSourceSnapshot| s.pages[0].content_hash = "h1x".into()),
            // 页面 frontmatter 变化（alias 换名）。
            // Page frontmatter change (alias renamed).
            Box::new(|s: &mut QugSourceSnapshot| {
                s.pages[0].frontmatter_json = page_frontmatter_json(
                    "珍珠奶茶",
                    &["boba2".to_string()],
                    &["奶茶".to_string()],
                )
                .unwrap();
            }),
            // 页面 generation 变化。
            // Page generation change.
            Box::new(|s: &mut QugSourceSnapshot| s.pages[0].generation = 9),
            // 页面集变化（去掉一页）。
            // Page-set change (one page removed).
            Box::new(|s: &mut QugSourceSnapshot| {
                s.pages.pop();
            }),
            // artifact_version 变化。
            // artifact_version change.
            Box::new(|s: &mut QugSourceSnapshot| s.pages[1].artifact_version = "other".into()),
            // domain version 变化。
            // Domain version change.
            Box::new(|s: &mut QugSourceSnapshot| s.domain_version = "0.2.0".into()),
            // domain name 变化。
            // Domain name change.
            Box::new(|s: &mut QugSourceSnapshot| s.domain = "milk-tea2".into()),
            // QugConfig 变化。
            // QugConfig change.
            Box::new(|s: &mut QugSourceSnapshot| {
                let mut cfg = fixture_config();
                cfg.qug.candidate_multiplier = 6;
                s.qug_config_json = serde_json::to_string(&cfg.qug).unwrap();
            }),
            // intents bytes 变化（即使解析结果相同，原文入哈希）。
            // intents bytes change (hash covers raw bytes even if parsing is
            // unchanged).
            Box::new(|s: &mut QugSourceSnapshot| s.intents_bytes.push(b'\n')),
        ];
        for (i, mutate) in mutations.iter().enumerate() {
            let mut snapshot = base.clone();
            mutate(&mut snapshot);
            let intents = parse_intents(&snapshot.intents_bytes).unwrap();
            let hash = derive_qug_edges(&snapshot, &config, &intents)
                .unwrap_or_else(|e| panic!("mutation {i} should still build: {e}"))
                .source_hash;
            assert_ne!(hash, base_hash, "mutation {i} must change source_hash");
        }
    }

    // A3：canonical JSON 文本可反序列化回 QugEdge 且 edge_hash 一致（持久化
    // 载荷可复核）。
    // A3: the canonical JSON text deserializes back into QugEdge with an
    // identical edge_hash (persisted payloads stay auditable).
    #[test]
    fn canonical_roundtrip_is_consistent() {
        let (snapshot, config, intents) = fixture();
        let derived = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        for p in &derived.page_edges {
            let text = edge_canonical_json(&p.edge).unwrap();
            assert_lower_hex64(&p.edge_hash);
            let back: QugEdge = serde_json::from_str(&text).unwrap();
            assert_eq!(edge_hash(&back).unwrap(), p.edge_hash);
        }
        for p in &derived.intent_edges {
            let text = edge_canonical_json(&p.edge).unwrap();
            let back: QugEdge = serde_json::from_str(&text).unwrap();
            assert_eq!(edge_hash(&back).unwrap(), p.edge_hash);
        }
    }

    // 边哈希内容判别：同内容同 hash，异内容异 hash。
    // Edge hashes discriminate content: equal content ⇒ equal hash, different
    // content ⇒ different hash.
    #[test]
    fn edge_hash_discriminates_content() {
        let a = QugEdge::Synonym {
            from: "boba".into(),
            to: vec!["珍珠奶茶".into()],
        };
        let a2 = a.clone();
        let b = QugEdge::Synonym {
            from: "boba".into(),
            to: vec!["乌龙奶茶".into()],
        };
        assert_eq!(edge_hash(&a).unwrap(), edge_hash(&a2).unwrap());
        assert_ne!(edge_hash(&a).unwrap(), edge_hash(&b).unwrap());
        assert_lower_hex64(&edge_hash(&a).unwrap());
    }

    // A3：重复边去重稳定（页内重复、跨页重复；故意让较大 page_id 先出现），
    // 两次派生逐条一致。
    // A3: duplicate edges dedup stably (in-page and cross-page repeats; the
    // larger page_id deliberately appears first); two derivations match one-to-one.
    #[test]
    fn dedup_is_stable() {
        let fm1 = page_frontmatter_json(
            "奶茶",
            &["boba".to_string(), "boba".to_string()],
            &["茶".to_string(), "茶".to_string()],
        )
        .unwrap();
        let fm2 =
            page_frontmatter_json("奶茶", &["boba".to_string()], &["茶".to_string()]).unwrap();
        let snapshot = QugSourceSnapshot {
            domain: "milk-tea".into(),
            domain_version: "0.1.0".into(),
            qug_config_json: serde_json::to_string(&fixture_config().qug).unwrap(),
            intents_bytes: Vec::new(),
            pages: vec![
                QugPageInput {
                    page_id: "p2".into(),
                    generation: 1,
                    content_hash: "hb".into(),
                    artifact_version: "seed-v1".into(),
                    frontmatter_json: fm2,
                },
                QugPageInput {
                    page_id: "p1".into(),
                    generation: 1,
                    content_hash: "ha".into(),
                    artifact_version: "seed-v1".into(),
                    frontmatter_json: fm1,
                },
            ],
        };
        let intents = parse_intents(&snapshot.intents_bytes).unwrap();
        let config = fixture_config();
        let derived = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        // 候选 6 条（p1 2+2、p2 1+1）→ Synonym/Hyponym 各去重为 1。
        // 6 candidates (p1 2+2, p2 1+1) → one Synonym and one Hyponym after dedup.
        assert_eq!(derived.page_edges.len(), 2);
        // 去重保留 UTF-8 序最小的 page_id。
        // Dedup keeps the smallest UTF-8 page_id.
        assert!(derived.page_edges.iter().all(|p| p.page_id == "p1"));
        // 去重后按 edge_hash 升序。
        // Deduplicated output is ascending by edge_hash.
        let hashes: Vec<&str> = derived
            .page_edges
            .iter()
            .map(|p| p.edge_hash.as_str())
            .collect();
        let mut sorted = hashes.clone();
        sorted.sort();
        assert_eq!(hashes, sorted);

        let again = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        assert_eq!(derived.source_hash, again.source_hash);
        let h1: Vec<&str> = derived
            .page_edges
            .iter()
            .map(|p| p.edge_hash.as_str())
            .collect();
        let h2: Vec<&str> = again
            .page_edges
            .iter()
            .map(|p| p.edge_hash.as_str())
            .collect();
        assert_eq!(h1, h2);
    }

    // §4.2：非法 frontmatter JSON → Validation。
    // §4.2: illegal frontmatter JSON → Validation.
    #[test]
    fn invalid_frontmatter_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages[0].frontmatter_json = "not json".into();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：aliases/tags 缺 title → 非法 frontmatter。
    // §4.2: aliases/tags without a title → illegal frontmatter.
    #[test]
    fn aliases_without_title_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages[0].frontmatter_json = r#"{"aliases":["boba"],"tags":["奶茶"]}"#.into();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：空 phrase（alias/tag/title 空白）→ Validation，不静默跳过。
    // §4.2: empty phrases (blank alias/tag/title) → Validation, never skipped.
    #[test]
    fn empty_phrase_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages[0].frontmatter_json =
            page_frontmatter_json("珍珠奶茶", &["  ".to_string()], &[]).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");

        snapshot.pages[0].frontmatter_json =
            page_frontmatter_json("珍珠奶茶", &[], &[" ".to_string()]).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");

        snapshot.pages[0].frontmatter_json =
            page_frontmatter_json("   ", &["boba".to_string()], &[]).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：超长 phrase（>128 Unicode scalar）→ Validation。
    // §4.2: overlong phrase (>128 Unicode scalars) → Validation.
    #[test]
    fn overlong_phrase_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages[0].frontmatter_json =
            page_frontmatter_json("珍珠奶茶", &["a".repeat(129)], &[]).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：未白名单 field → Validation（收敛 Step3 IntentConfig::validate）。
    // §4.2: un-whitelisted field → Validation (converged from Step3's
    // IntentConfig::validate).
    #[test]
    fn unwhitelisted_field_is_validation() {
        let (snapshot, config, _) = fixture();
        let yaml = r#"version: "1"
intents:
  - id: bad_field
    phrases: ["甜的"]
    attribute:
      field: topping
      max: 5
"#;
        let intents: IntentConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：非法 intents YAML → parse_intents 返回 Validation。
    // §4.2: illegal intents YAML → parse_intents returns Validation.
    #[test]
    fn invalid_intents_yaml_is_validation() {
        let err = parse_intents(b"{").unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // §4.2：空 intents bytes 合法（未配置 intents.yaml 的领域），配置边为零
    // 而页面边照常发布。
    // §4.2: empty intents bytes are legal (domain without intents.yaml); config
    // edges are zero while page edges still publish.
    #[test]
    fn empty_intents_bytes_allowed() {
        let (mut snapshot, config, _) = fixture();
        snapshot.intents_bytes = Vec::new();
        let intents = parse_intents(&snapshot.intents_bytes).unwrap();
        let derived = derive_qug_edges(&snapshot, &config, &intents).unwrap();
        assert_eq!(derived.intent_edges.len(), 0);
        assert_eq!(derived.page_edges.len(), 6);
        assert_eq!(derived.by_type.get("negation"), None);
    }

    // §9：alias 硬上限超限报错不截断（65 > 64）。
    // §9: exceeding the alias hard cap errors out instead of truncating (65 > 64).
    #[test]
    fn alias_cap_errors_not_truncates() {
        let (mut snapshot, config, intents) = fixture();
        let aliases: Vec<String> = (0..65).map(|i| format!("alias-{i}")).collect();
        snapshot.pages[0].frontmatter_json =
            page_frontmatter_json("珍珠奶茶", &aliases, &[]).unwrap();
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("hard cap"), "got {err}");
    }

    // §9：单条目短语硬上限超限报错不截断（65 > 64）。
    // §9: exceeding the per-entry phrase hard cap errors out instead of
    // truncating (65 > 64).
    #[test]
    fn phrase_cap_errors_not_truncates() {
        let phrases: Vec<String> = (0..65).map(|i| format!("短语{i}")).collect();
        let list = phrases
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let yaml = format!(
            "version: \"1\"\nintents:\n  - id: cap\n    phrases: [{list}]\n    expansion:\n      text: \"招牌\"\n"
        );
        let err = parse_intents(yaml.as_bytes()).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(err.to_string().contains("hard cap"), "got {err}");
    }

    // D4：快照 page_id 重复（未收敛到 accepted head）→ Validation。
    // D4: duplicate snapshot page_ids (accepted heads not collapsed) → Validation.
    #[test]
    fn duplicate_page_id_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages.push(snapshot.pages[0].clone());
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }

    // 页面身份字段校验：generation 必须 ≥1。
    // Page identity validation: generation must be ≥1.
    #[test]
    fn generation_bound_is_validation() {
        let (mut snapshot, config, intents) = fixture();
        snapshot.pages[0].generation = 0;
        let err = derive_qug_edges(&snapshot, &config, &intents).unwrap_err();
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}
