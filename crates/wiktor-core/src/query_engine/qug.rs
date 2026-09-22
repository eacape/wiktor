//! QUG 查询理解图：petgraph DiGraph + 短语索引 + 改写/遍历。
//! QUG query-understanding graph: petgraph DiGraph + phrase index + rewrite/traverse.
//!
//! 图是编译期产物、查询期只读：`from_edges` 一次性构图（节点按归一化短语
//! 复用、边去重），`rewrite` 在查询期做最长短语匹配与结构化过滤展开，
//! `traverse` 供诊断与未来扩展。节点保存归一化短语，边保留原始 `QugEdge`
//! （结构化 Filter/Query 已内嵌，查询时无需重新解析 YAML）。
//! The graph is a compile-time artifact and read-only at query time: `from_edges`
//! builds it once (nodes reused by normalized phrase, edges deduplicated),
//! `rewrite` performs longest-phrase matching plus structured filter expansion,
//! and `traverse` serves diagnostics and future extensions. Nodes hold normalized
//! phrases while edges keep the original `QugEdge` (structured Filter/Query is
//! embedded, so no YAML re-parsing at query time).

use crate::traits::{AttributeRule, DomainConfig, IntentConfig, TemplateExpansion};
use crate::types::error::{Error, Result};
use crate::types::{
    CompiledPage, EntityId, FilterCondition, Filters, QualityScore, Query, QugEdge, QugPath,
    RewrittenQuery, WikiPage,
};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 图遍历深度硬上限（超过返回配置错误）。
/// Hard cap on graph traversal depth (exceeding it is a config error).
pub const MAX_DEPTH: usize = 4;

/// 单次改写最多应用的匹配数。
/// Maximum number of matches applied by one rewrite.
pub const MAX_MATCHES: usize = 16;

/// 改写后展开词数量上限。
/// Upper bound on expanded terms after a rewrite.
pub const MAX_EXPANDED_TERMS: usize = 64;

/// 规范化短语的 Unicode scalar 上限。
/// Unicode-scalar cap for a normalized phrase.
pub const MAX_PHRASE_SCALARS: usize = 128;

/// QUG 节点：保存归一化短语（查找键）。
/// QUG node: holds a normalized phrase (the lookup key).
#[derive(Debug, Clone)]
pub struct QugNode {
    pub phrase: String,
}

/// QUG 图：petgraph DiGraph + phrase→NodeIndex 索引（HashMap 只做索引，不承担图语义）。
/// QUG graph: petgraph DiGraph + a phrase→NodeIndex index (the HashMap is only an
/// index; it carries no graph semantics).
#[derive(Debug)]
pub struct QugGraph {
    pub graph: DiGraph<QugNode, QugEdge>,
    pub by_phrase: HashMap<String, NodeIndex>,
    pub max_depth: usize,
}

impl QugGraph {
    /// 从边集合构图。节点按归一化短语复用；边去重键为
    /// `(source, target, serialized_edge)`。`max_depth` 默认 2、硬上限 4。
    /// Builds the graph from an edge collection. Nodes are reused by normalized
    /// phrase; the edge dedup key is `(source, target, serialized_edge)`.
    /// `max_depth` defaults to 2 with a hard cap of 4.
    pub fn from_edges(edges: impl IntoIterator<Item = QugEdge>, max_depth: usize) -> Result<Self> {
        if max_depth == 0 || max_depth > MAX_DEPTH {
            return Err(Error::Validation(format!(
                "QUG max_depth {max_depth} out of range 1..={MAX_DEPTH}"
            )));
        }
        let mut graph = DiGraph::<QugNode, QugEdge>::new();
        let mut by_phrase: HashMap<String, NodeIndex> = HashMap::new();
        let mut seen: HashSet<(String, String, String)> = HashSet::new();

        let add_edge = |src: NodeIndex,
                        dst: NodeIndex,
                        edge: &QugEdge,
                        graph: &mut DiGraph<QugNode, QugEdge>,
                        seen: &mut HashSet<(String, String, String)>|
         -> Result<()> {
            let key = (
                graph[src].phrase.clone(),
                graph[dst].phrase.clone(),
                serde_json::to_string(edge)?,
            );
            if seen.insert(key) {
                graph.add_edge(src, dst, edge.clone());
            }
            Ok(())
        };

        for edge in edges {
            match &edge {
                // 同义边：`to` 列表每个词各建一条有向边（同义关系若需双向，
                // 由构建器显式生成反向边）。
                // Synonym edge: each `to` entry becomes one directed edge (if
                // bidirectional synonymy is needed, the builder emits reverse edges).
                QugEdge::Synonym { from, to } => {
                    let src = node_for(from, &mut graph, &mut by_phrase)?;
                    for t in to {
                        let dst = node_for(t, &mut graph, &mut by_phrase)?;
                        let e = QugEdge::Synonym {
                            from: from.clone(),
                            to: vec![t.clone()],
                        };
                        add_edge(src, dst, &e, &mut graph, &mut seen)?;
                    }
                }
                // 上下位边：child → parent（品类扩展召回）。
                // Hyponym edge: child → parent (category-expansion recall).
                QugEdge::Hyponym { child, parent } => {
                    let src = node_for(child, &mut graph, &mut by_phrase)?;
                    let dst = node_for(parent, &mut graph, &mut by_phrase)?;
                    add_edge(src, dst, &edge, &mut graph, &mut seen)?;
                }
                // 短语驱动的结构化边（属性/意图/否定）：短语自环承载 payload，
                // 查询期遍历该节点的出边即可命中。
                // Phrase-driven structured edges (attribute/intent/negation): a
                // self-loop on the phrase node carries the payload; rewrite walks
                // that node's outgoing edges to apply it.
                QugEdge::AttributePropagation { phrase, .. }
                | QugEdge::IntentTemplate { phrase, .. }
                | QugEdge::Negation { phrase, .. } => {
                    let n = node_for(phrase, &mut graph, &mut by_phrase)?;
                    add_edge(n, n, &edge, &mut graph, &mut seen)?;
                }
            }
        }
        Ok(Self {
            graph,
            by_phrase,
            max_depth,
        })
    }

    /// 查询改写（Step 3 §3.3）。
    /// Query rewrite (Step 3 §3.3).
    ///
    /// 返回 `Some` = 至少应用一条可执行边；`None` = 无匹配边或输入非法但可安全
    /// fallback。图损坏/过滤类型非法等内部错误返回 `Err`。
    /// Returns `Some` when at least one actionable edge was applied; `None` when
    /// nothing matched or the input is invalid but safely fallback-able. Internal
    /// errors (corrupt graph, illegal filter types) return `Err`.
    pub fn rewrite(&self, query: &Query) -> Result<Option<RewrittenQuery>> {
        if query.text.trim().is_empty() || query.top_k == 0 {
            return Ok(None);
        }
        // 输入非法但可安全 fallback（如超过 128 scalar）→ None，不伪装成内部错误。
        // Invalid-but-fallable input (e.g. >128 scalars) → None, not an internal error.
        let norm_query = match normalize(&query.text) {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };

        // 扫描所有 Unicode 字符起点，每点取最长短语匹配
        // Scan every Unicode char start; take the longest phrase per start
        let chars: Vec<char> = norm_query.chars().collect();
        let mut matched_nodes: Vec<NodeIndex> = Vec::new();
        for start in 0..chars.len() {
            let mut best: Option<(usize, NodeIndex)> = None;
            for (phrase, &node) in &self.by_phrase {
                let plen = phrase.chars().count();
                if plen == 0 || start + plen > chars.len() {
                    continue;
                }
                if chars[start..start + plen].iter().collect::<String>() == *phrase {
                    match best {
                        Some((blen, _)) if blen >= plen => {}
                        _ => best = Some((plen, node)),
                    }
                }
            }
            if let Some((_, node)) = best {
                matched_nodes.push(node);
            }
        }
        if matched_nodes.is_empty() {
            return Ok(None);
        }

        // 按节点去重；按 (长度 desc, 节点插入序 asc) 稳定排序（配置文件顺序近似）；
        // 最多应用 16 个匹配。
        // Dedup by node; sort stably by (length desc, node insertion order asc)
        // (approximating config-file order); apply at most 16 matches.
        matched_nodes.sort_unstable();
        matched_nodes.dedup();
        let mut ranked: Vec<(usize, NodeIndex)> = matched_nodes
            .into_iter()
            .map(|n| (self.graph[n].phrase.chars().count(), n))
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ranked.truncate(MAX_MATCHES);

        // 原文始终保留为检索词之一，防止改写损失精确命中。
        // The original text is always kept as a search term to avoid losing exact hits.
        let mut out_terms: Vec<String> = vec![query.text.clone()];
        let mut out_conditions: Vec<FilterCondition> = query.filters.conditions.clone();
        let boosted: Vec<EntityId> = Vec::new();
        let mut intent_seen = false;

        for (_, node) in ranked {
            let outgoing: Vec<QugEdge> = self
                .graph
                .edges_directed(node, Direction::Outgoing)
                .map(|e| e.weight().clone())
                .collect();
            for edge in outgoing {
                match edge {
                    QugEdge::Synonym { to, .. } => {
                        for t in to {
                            push_unique(&mut out_terms, t);
                        }
                    }
                    QugEdge::Hyponym { child, parent } => {
                        push_unique(&mut out_terms, child);
                        push_unique(&mut out_terms, parent);
                    }
                    QugEdge::Negation { exclusion, .. } => {
                        out_conditions.push(exclusion);
                    }
                    QugEdge::AttributePropagation { filter, .. } => {
                        out_conditions.push(filter);
                    }
                    QugEdge::IntentTemplate { expansion, .. } => {
                        intent_seen = true;
                        push_unique(&mut out_terms, expansion.text);
                        out_conditions.extend(expansion.filters.conditions);
                    }
                }
            }
        }

        out_terms.truncate(MAX_EXPANDED_TERMS);
        let merged = resolve_conflicts(&Filters {
            conditions: out_conditions,
        });
        let out_filters = Filters {
            conditions: merged.conditions,
        };

        // 无任何实际变化且未命中意图模板 → None（调用方走混合检索 fallback）。
        // Nothing changed and no intent template fired → None (caller falls back to hybrid search).
        if out_terms.len() == 1
            && out_terms[0] == query.text
            && out_filters == query.filters
            && !intent_seen
        {
            return Ok(None);
        }

        Ok(Some(RewrittenQuery {
            expanded_terms: out_terms,
            filters: out_filters,
            boost_entities: boosted,
        }))
    }

    /// 从匹配节点出发返回深度 1..=max_depth 的简单路径；不返回零长度路径；
    /// 禁止重复 NodeIndex；按边插入顺序稳定返回；未知节点返回空 Vec。
    /// Returns simple paths of depth 1..=max_depth from a matched node; never
    /// returns zero-length paths; no repeated NodeIndex; stable edge-insertion
    /// order; unknown nodes yield an empty Vec.
    pub fn traverse(&self, node: &str, max_depth: usize) -> Vec<QugPath> {
        let norm = match normalize(node) {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
        let Some(&start) = self.by_phrase.get(&norm) else {
            return Vec::new();
        };
        let depth = max_depth.min(self.max_depth);
        let mut out = Vec::new();
        let mut path_nodes: Vec<NodeIndex> = vec![start];
        let mut path_edges: Vec<QugEdge> = Vec::new();
        traverse_dfs(
            &self.graph,
            start,
            depth,
            &mut path_nodes,
            &mut path_edges,
            &mut out,
        );
        out
    }
}

/// 获取或创建归一化短语节点（同一短语复用节点）。
/// Gets or creates the node for a normalized phrase (reusing nodes).
fn node_for(
    phrase: &str,
    graph: &mut DiGraph<QugNode, QugEdge>,
    by_phrase: &mut HashMap<String, NodeIndex>,
) -> Result<NodeIndex> {
    let norm = normalize(phrase)?;
    Ok(if let Some(&idx) = by_phrase.get(&norm) {
        idx
    } else {
        let idx = graph.add_node(QugNode {
            phrase: norm.clone(),
        });
        by_phrase.insert(norm, idx);
        idx
    })
}

/// 深度优先遍历收集简单路径。
/// Depth-first traversal collecting simple paths.
fn traverse_dfs(
    graph: &DiGraph<QugNode, QugEdge>,
    cur: NodeIndex,
    max_depth: usize,
    path_nodes: &mut Vec<NodeIndex>,
    path_edges: &mut Vec<QugEdge>,
    out: &mut Vec<QugPath>,
) {
    for edge in graph.edges_directed(cur, Direction::Outgoing) {
        let target = edge.target();
        if path_nodes.contains(&target) {
            continue;
        }
        path_nodes.push(target);
        path_edges.push(edge.weight().clone());
        out.push(QugPath {
            nodes: path_nodes
                .iter()
                .map(|n| graph[*n].phrase.clone())
                .collect(),
            edges: path_edges.clone(),
            depth: path_edges.len(),
        });
        if path_edges.len() < max_depth {
            traverse_dfs(graph, target, max_depth, path_nodes, path_edges, out);
        }
        path_nodes.pop();
        path_edges.pop();
    }
}

/// 短语归一化契约（Step 3 §3.1）：去首尾空白、连续空白折叠、Unicode 小写；
/// 保留中文/数字/标点。空短语或超过 128 个 Unicode scalar 的短语拒绝。
/// Phrase normalization contract (Step 3 §3.1): trim, collapse consecutive
/// whitespace, Unicode lowercase; keep CJK/digits/punctuation. Empty phrases and
/// phrases over 128 Unicode scalars are rejected.
pub fn normalize(phrase: &str) -> Result<String> {
    let mut out = String::with_capacity(phrase.len());
    for c in phrase.trim().chars() {
        if c.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.extend(c.to_lowercase());
        }
    }
    if out.is_empty() {
        return Err(Error::Validation(format!(
            "QUG phrase is empty after normalization: {phrase:?}"
        )));
    }
    if out.chars().count() > MAX_PHRASE_SCALARS {
        return Err(Error::Validation(format!(
            "QUG phrase exceeds {MAX_PHRASE_SCALARS} Unicode scalars: {phrase:?}"
        )));
    }
    Ok(out)
}

/// 去重追加（保持顺序）。
/// Append-if-absent, preserving order.
fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// 确定性合并过滤条件（Step 3 §3.3 冲突处理）。
/// Deterministic filter-condition merging (Step 3 §3.3 conflict resolution).
///
/// 语义：同字段同类型条件求交（交集非空 → 用交集替换；交集为空 → 保留双方，
/// AND 不可满足 → 空候选域，不放宽用户条件）；RefExcludes 与 RefContains 的
/// 交集被从允许集合移除（移除后为空 → 保留双方，空候选域）；完全重复的条件去重。
/// Semantics: same-field same-kind conditions are intersected (non-empty
/// intersection replaces them; empty intersection keeps both, making the AND
/// unsatisfiable → empty candidate scope without relaxing user conditions); the
/// RefExcludes∩RefContains intersection is removed from the allowed set (when the
/// remainder is empty, both are kept → empty candidate scope); exact duplicates are
/// deduplicated.
fn resolve_conflicts(conditions: &Filters) -> Filters {
    let mut out: Vec<FilterCondition> = Vec::new();
    for cond in &conditions.conditions {
        push_merged(&mut out, cond.clone());
    }
    Filters { conditions: out }
}

/// 把单个条件并入结果列表（含同字段合并与去重）。
/// Merges one condition into the result list (with same-field merging and dedup).
fn push_merged(out: &mut Vec<FilterCondition>, cond: FilterCondition) {
    if out.contains(&cond) {
        return;
    }
    match cond {
        FilterCondition::NumericRange { field, min, max } => {
            // 与同字段所有 NumericRange 求交；空交集 → 原样追加（不可满足）
            // Intersect with all same-field NumericRange; empty intersection appends as-is (unsatisfiable)
            let mut merged_min = min;
            let mut merged_max = max;
            let mut same_field: Vec<usize> = Vec::new();
            for (i, c) in out.iter().enumerate() {
                if let FilterCondition::NumericRange {
                    field: f,
                    min: m,
                    max: x,
                } = c
                {
                    if *f == field {
                        same_field.push(i);
                        merged_min = max_opt(merged_min, *m);
                        merged_max = min_opt(merged_max, *x);
                    }
                }
            }
            let empty = matches!((merged_min, merged_max), (Some(a), Some(b)) if a > b);
            if empty || same_field.is_empty() {
                out.push(FilterCondition::NumericRange { field, min, max });
            } else {
                for &i in same_field.iter().rev() {
                    out.remove(i);
                }
                out.push(FilterCondition::NumericRange {
                    field,
                    min: merged_min,
                    max: merged_max,
                });
            }
        }
        FilterCondition::TextEquals { field, value } => {
            // 与同字段 TextEquals 不同值同时保留 → AND 不可满足（空候选域）；
            // 完全相同值已在入口去重。
            // Keeping same-field TextEquals with different values makes the AND
            // unsatisfiable (empty candidate scope); identical values are deduped at the entry.
            out.push(FilterCondition::TextEquals { field, value });
        }
        FilterCondition::RefContains { field, refs } => {
            // 与同字段 RefExcludes/RefContains 合并：先交后差
            // Merge with same-field RefExcludes/RefContains: intersect, then subtract excludes
            let mut contains: Option<Vec<String>> = Some(refs);
            let mut excludes: Vec<String> = Vec::new();
            for c in out.iter() {
                match c {
                    FilterCondition::RefExcludes { field: f, refs: es } if *f == field => {
                        for r in es {
                            if !excludes.contains(r) {
                                excludes.push(r.clone());
                            }
                        }
                    }
                    FilterCondition::RefContains { field: f, refs: cs } if *f == field => {
                        if let Some(cur) = &contains {
                            contains =
                                Some(cur.iter().filter(|r| cs.contains(r)).cloned().collect());
                        }
                    }
                    _ => {}
                }
            }
            if let Some(cs) = &contains {
                let remaining: Vec<String> = cs
                    .iter()
                    .filter(|r| !excludes.contains(r))
                    .cloned()
                    .collect();
                if remaining.is_empty() {
                    // 交集为空 → 空候选域标记（filter_where 对空 refs 输出 1=0）
                    // Empty intersection → empty-scope marker (filter_where emits 1=0 for empty refs)
                    out.push(FilterCondition::RefContains {
                        field,
                        refs: vec![],
                    });
                } else {
                    out.push(FilterCondition::RefContains {
                        field,
                        refs: remaining,
                    });
                }
            }
        }
        FilterCondition::RefExcludes { field, refs } => {
            // 与同字段 RefExcludes 取并集；从同字段 RefContains 移除被排除项
            //（RefExcludes 在交集语义上胜出）。移除后 Contains 为空 → 空候选域标记。
            // Union with same-field RefExcludes; subtract excluded refs from any
            // same-field RefContains (RefExcludes wins in the intersection sense).
            // An emptied Contains becomes an empty-scope marker.
            let mut contains: Option<Vec<String>> = None;
            let mut excludes: Vec<String> = refs.clone();
            let mut saw_same_field = false;
            for c in out.iter() {
                match c {
                    FilterCondition::RefContains { field: f, refs: cs } if *f == field => {
                        saw_same_field = true;
                        contains = Some(match contains {
                            None => cs.clone(),
                            Some(prev) => prev.into_iter().filter(|r| cs.contains(r)).collect(),
                        });
                    }
                    FilterCondition::RefExcludes { field: f, refs: es } if *f == field => {
                        saw_same_field = true;
                        for r in es {
                            if !excludes.contains(r) {
                                excludes.push(r.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !saw_same_field {
                out.push(FilterCondition::RefExcludes { field, refs });
                return;
            }
            let is_same_field = |c: &FilterCondition| match c {
                FilterCondition::RefContains { field: f, .. }
                | FilterCondition::RefExcludes { field: f, .. } => f == &field,
                _ => false,
            };
            let mut keep: Vec<FilterCondition> = Vec::new();
            for c in out.drain(..) {
                if !is_same_field(&c) {
                    keep.push(c);
                }
            }
            if let Some(cs) = &contains {
                let remaining: Vec<String> = cs
                    .iter()
                    .filter(|r| !excludes.contains(r))
                    .cloned()
                    .collect();
                if !remaining.is_empty() {
                    keep.push(FilterCondition::RefContains {
                        field: field.clone(),
                        refs: remaining,
                    });
                } else {
                    // 交集为空 → 空候选域标记（filter_where 对空 refs 输出 1=0）
                    // Empty intersection → empty-scope marker (filter_where emits 1=0 for empty refs)
                    keep.push(FilterCondition::RefContains {
                        field: field.clone(),
                        refs: vec![],
                    });
                }
            }
            keep.push(FilterCondition::RefExcludes {
                field,
                refs: excludes,
            });
            *out = keep;
        }
    }
}

/// 数值取交集用辅助。
/// Numeric intersection helpers.
fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// 从种子页提取 QUG 边（Step 3 §3.2.1/3.2.2）。
/// Extracts QUG edges from seed pages (Step 3 §3.2.1/3.2.2).
///
/// - aliases → Synonym：每个 alias 指向页面 title（同页 alias 间不自动全连接）；
/// - tags → Hyponym：parent=tag，child=title（标签间不推断层级）；
/// - 页面编译期产出的 `qug_edges` 一并并入。
/// - aliases → Synonym: each alias points at the page title (aliases on the same
///   page are not auto-connected to each other);
/// - tags → Hyponym: parent=tag, child=title (no hierarchy is inferred between tags);
/// - the page's compile-time `qug_edges` are merged in too.
pub fn extract_page_edges(pages: &[CompiledPage]) -> Vec<QugEdge> {
    let mut edges = Vec::new();
    for p in pages {
        for alias in &p.wiki.aliases {
            let a = alias.trim();
            if a.is_empty() || a == p.wiki.title {
                continue;
            }
            edges.push(QugEdge::Synonym {
                from: a.to_string(),
                to: vec![p.wiki.title.clone()],
            });
        }
        for tag in &p.wiki.tags {
            let t = tag.trim();
            if t.is_empty() {
                continue;
            }
            edges.push(QugEdge::Hyponym {
                child: p.wiki.title.clone(),
                parent: t.to_string(),
            });
        }
        edges.extend(p.qug_edges.iter().cloned());
    }
    edges
}

/// 从意图配置生成 QUG 边（Step 3 §3.2.3）。
/// Generates QUG edges from the intent config (Step 3 §3.2.3).
///
/// - expansion → IntentTemplate；
/// - attribute → AttributePropagation（`equals` 按 field 类型映射为
///   数值等值区间或 TextEquals；数值解析失败为配置错误）；
/// - negation → Negation（RefExcludes）。
/// - expansion → IntentTemplate;
/// - attribute → AttributePropagation (`equals` maps to a numeric equal-range or
///   TextEquals depending on the field type; unparseable numerics are config errors);
/// - negation → Negation (RefExcludes).
pub fn intent_edges(intents: &IntentConfig, config: &DomainConfig) -> Result<Vec<QugEdge>> {
    let mut edges = Vec::new();
    for entry in &intents.intents {
        if let Some(exp) = &entry.expansion {
            for phrase in &entry.phrases {
                edges.push(QugEdge::IntentTemplate {
                    phrase: phrase.clone(),
                    expansion: expansion_to_query(exp),
                });
            }
        } else if let Some(attr) = &entry.attribute {
            let filter = attribute_to_filter(attr, config)?;
            for phrase in &entry.phrases {
                edges.push(QugEdge::AttributePropagation {
                    phrase: phrase.clone(),
                    filter: filter.clone(),
                });
            }
        } else if let Some(neg) = &entry.negation {
            let exclusion = FilterCondition::RefExcludes {
                field: neg.field.clone(),
                refs: neg.refs.clone(),
            };
            for phrase in &entry.phrases {
                edges.push(QugEdge::Negation {
                    phrase: phrase.clone(),
                    exclusion: exclusion.clone(),
                });
            }
        }
    }
    Ok(edges)
}

/// TemplateExpansion → 展开用的 Query（top_k/domain 不参与展开语义）。
/// TemplateExpansion → the expansion Query (top_k/domain are irrelevant to expansion).
fn expansion_to_query(exp: &TemplateExpansion) -> Query {
    Query {
        text: exp.text.clone(),
        filters: exp.filters.clone(),
        top_k: 0,
        domain: None,
    }
}

/// AttributeRule → FilterCondition（按 field 声明类型映射 equals）。
/// AttributeRule → FilterCondition (maps `equals` by the declared field type).
fn attribute_to_filter(attr: &AttributeRule, config: &DomainConfig) -> Result<FilterCondition> {
    if let Some(eq) = &attr.equals {
        match config.field_type(&attr.field) {
            Some(crate::types::FieldType::Numeric) => {
                let v = eq.parse::<f64>().map_err(|_| {
                    Error::InvalidConfig(format!(
                        "intent attribute field {:?} equals {:?} is not a valid number",
                        attr.field, eq
                    ))
                })?;
                Ok(FilterCondition::NumericRange {
                    field: attr.field.clone(),
                    min: Some(v),
                    max: Some(v),
                })
            }
            _ => Ok(FilterCondition::TextEquals {
                field: attr.field.clone(),
                value: eq.clone(),
            }),
        }
    } else {
        Ok(FilterCondition::NumericRange {
            field: attr.field.clone(),
            min: attr.min,
            max: attr.max,
        })
    }
}

/// QUG 构建输入（Step 3 §3.2）。
/// QUG build input (Step 3 §3.2).
pub struct QugBuildInput<'a> {
    pub pages: &'a [CompiledPage],
    pub config: &'a DomainConfig,
    pub intents: &'a IntentConfig,
}

/// QUG 构建产物：图（只读）+ source_hash（重建/缓存依据）。
/// QUG build artifact: the (read-only) graph plus a source_hash (rebuild/cache key).
pub struct BuiltQug {
    pub graph: Arc<QugGraph>,
    /// BLAKE3：domain version + qug 配置 bytes + 意图配置 + 每页 (page_id, content_hash)。
    /// BLAKE3 over domain version + qug config bytes + intent config + per-page (page_id, content_hash).
    pub source_hash: String,
}

/// 构建 QUG 图（Step 3 §3.2）。输入改变必须重新建图；构建失败不得静默启用部分图。
/// Builds the QUG graph (Step 3 §3.2). Input changes require a full rebuild; a
/// failed build never silently enables a partial graph.
pub fn build_qug(input: QugBuildInput<'_>) -> Result<BuiltQug> {
    let mut edges = extract_page_edges(input.pages);
    edges.extend(intent_edges(input.intents, input.config)?);
    let graph = Arc::new(QugGraph::from_edges(edges, input.config.qug.max_depth)?);

    let mut hasher = blake3::Hasher::new();
    hasher.update(input.config.version.as_bytes());
    hasher.update(&[0]);
    let qug_json = serde_json::to_string(&input.config.qug)?;
    hasher.update(qug_json.as_bytes());
    hasher.update(&[0]);
    let intents_json = serde_json::to_string(input.intents)?;
    hasher.update(intents_json.as_bytes());
    hasher.update(&[0]);
    for p in input.pages {
        hasher.update(p.wiki.page_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(p.content_hash.as_bytes());
        hasher.update(&[0]);
    }
    let source_hash = hasher.finalize().to_hex().to_string();
    Ok(BuiltQug { graph, source_hash })
}

/// 把 seed WikiPage 包装成编译产物（评分满分；content_hash 与 kernel 一致；
/// qug_edges 由 build_qug 提取，此处留空）。
/// Wraps a seed WikiPage as a compilation artifact (perfect score; content_hash
/// matching the kernel; qug_edges are extracted by build_qug, so empty here).
pub fn compiled_page(wiki: &WikiPage) -> CompiledPage {
    let content_hash = blake3::hash(format!("{}\0{}", wiki.title, wiki.content).as_bytes())
        .to_hex()
        .to_string();
    CompiledPage {
        wiki: wiki.clone(),
        quality: QualityScore {
            coverage: 1.0,
            citation: 1.0,
            schema_compliance: 1.0,
            density: 1.0,
            consistency: None,
        },
        qug_edges: Vec::new(),
        content_hash,
        // seed 页无证据载荷（Step 4 §4：None 经 executor 发布视为 schema 失败）。
        // Seed pages carry no evidence payload (Step 4 §4: None published via the
        // executor is a schema failure).
        evidence: None,
    }
}

/// 从 seed WikiPage 列表直接构建（测试/CLI 便捷入口）。
/// Convenience builder from a list of seed WikiPages (tests / CLI).
pub fn build_qug_from_wiki(
    wiki_pages: &[WikiPage],
    config: &DomainConfig,
    intents: &IntentConfig,
) -> Result<BuiltQug> {
    let pages: Vec<CompiledPage> = wiki_pages.iter().map(compiled_page).collect();
    build_qug(QugBuildInput {
        pages: &pages,
        config,
        intents,
    })
}

/// Step 5 批1：五类边持久化前的纯函数构建层（类型契约 / hash / 硬上限 / 去重），
/// 见 `docs/design/step5-qug-build.md` §4.2。
/// Step 5 batch 1: pure-function build layer before persistence (type contract /
/// hashing / hard caps / dedup), see docs/design/step5-qug-build.md §4.2.
pub mod qug_build;
