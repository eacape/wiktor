//! 混合检索融合：FTS 与向量两路候选 → RRF 融合。
//! Hybrid-retrieval fusion: FTS and vector candidates → RRF fusion.
//!
//! 对每个来源，排名从 1 开始，贡献 `1 / (k + r)`；按 `page_id` 去重累加。
//! 若只有一路有结果，仍按该路贡献计算，不回退为原始分数。
//! For each source, ranks start at 1 and contribute `1 / (k + r)`; contributions
//! are accumulated and deduplicated by `page_id`. A single-source result still uses
//! that source's RRF contributions — never the raw scores.

use crate::traits::VectorHit;
use crate::types::error::Result;
use crate::types::{EntityId, SearchHit};
use std::collections::HashMap;

/// RRF 融合常数（Step 3 D5：k=60）。
/// RRF fusion constant (Step 3 D5: k=60).
pub const RRF_K_DEFAULT: u32 = 60;

/// RRF 融合（Step 3 §6）。
/// RRF fusion (Step 3 §6).
///
/// - FTS 命中的排名即其在 `fts` 中的顺序（score 已降序）；
/// - 向量命中的排名即其在 `vectors` 中的顺序（向量库已按相似度降序）；
/// - 最终按 `(rrf_score DESC, page_id ASC)` 排序，取前 `top_k`；
/// - 向量命中缺少 title（VectorMetadata 无标题字段），若该页无 FTS 命中，
///   title 为空串（CLI 可回退展示 entity_id）。
/// - FTS hit rank is its position in `fts` (already score-descending);
/// - vector hit rank is its position in `vectors` (already similarity-descending);
/// - the final order is `(rrf_score DESC, page_id ASC)`, truncated to `top_k`;
/// - vector hits carry no title (VectorMetadata has no title field); when a page
///   has no FTS hit, its title is an empty string (the CLI can fall back to entity_id).
pub fn rrf_merge(fts: &[SearchHit], vectors: &[VectorHit], top_k: usize, k: u32) -> Vec<SearchHit> {
    let kf = k as f32;
    // 去重键 page_id → (rrf_score, entity_id, title)
    // Dedup key page_id → (rrf_score, entity_id, title)
    let mut acc: HashMap<String, (f32, EntityId, String)> = HashMap::new();

    for (i, hit) in fts.iter().enumerate() {
        let r = (i + 1) as f32;
        let entry = acc
            .entry(hit.page_id.clone())
            .or_insert_with(|| (0.0, hit.entity_id.clone(), hit.title.clone()));
        entry.0 += 1.0 / (kf + r);
    }

    for (i, vh) in vectors.iter().enumerate() {
        // metadata.entity_id 解析失败（损坏 payload）→ 跳过该向量命中
        // Unparseable metadata.entity_id (corrupt payload) → skip this vector hit
        let Ok(entity_id) = EntityId::from_key(&vh.metadata.entity_id) else {
            continue;
        };
        // page_id 为空时回退到 entity key（保证去重键非空）
        // Fall back to the entity key when page_id is empty (keeps the dedup key non-empty)
        let page_id = if vh.metadata.page_id.is_empty() {
            entity_id.to_key()
        } else {
            vh.metadata.page_id.clone()
        };
        let r = (i + 1) as f32;
        let entry = acc
            .entry(page_id)
            .or_insert_with(|| (0.0, entity_id.clone(), String::new()));
        entry.0 += 1.0 / (kf + r);
    }

    let mut merged: Vec<SearchHit> = acc
        .into_iter()
        .map(|(page_id, (score, entity_id, title))| SearchHit {
            page_id,
            entity_id,
            score,
            title,
        })
        .collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.page_id.cmp(&b.page_id))
    });
    merged.truncate(top_k);
    merged
}

/// 候选域白名单第二道保护（Step 3 §6）：候选域非空（严格白名单）时，丢弃
/// 不属于候选域的越界命中（防事实与页面代际短暂不一致）。空候选域返回空。
/// Second-line candidate-scope whitelist (Step 3 §6): when the candidate scope is
/// non-empty (strict whitelist), drop out-of-scope hits (guards against brief
/// fact/page generation skew). An empty scope yields no hits.
pub fn whitelist_filter(
    hits: Vec<SearchHit>,
    candidates: Option<&[EntityId]>,
) -> Result<Vec<SearchHit>> {
    let Some(ids) = candidates else {
        return Ok(hits);
    };
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(hits
        .into_iter()
        .filter(|h| ids.contains(&h.entity_id))
        .collect())
}
