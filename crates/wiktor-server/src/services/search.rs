//! Search 服务（spec step7 §3 D1/D2/D5，§6.1）：启动时一次装配共享
//! `QueryEngine`，`search` 把 proto 请求映射为 core `Query` → 引擎检索 →
//! 映射 proto 响应。QUG 失败显式标记 `rewrite_failure` 并走混合 fallback
//! （A10：不调用 LLM）。
//! The Search service (spec step7 §3 D1/D2/D5, §6.1): a `QueryEngine` is
//! assembled once at startup and shared; `search` maps the proto request into a
//! core `Query` → engine retrieval → a proto response. QUG failure is marked
//! explicitly via `rewrite_failure` and falls back to hybrid retrieval (A10:
//! no LLM calls).

use std::sync::Arc;

use wiktor_core::query_engine::QueryEngine;
use wiktor_core::traits::VectorStore;
use wiktor_core::types::error::Error;
use wiktor_core::types::{Filters, Query};

use crate::grpc::v1::search_server::Search;
use crate::grpc::v1::{RewrittenQuery, SearchHit, SearchRequest, SearchResponse};

/// Search gRPC handler：持装配好的引擎（server 启动时构造，跨请求共享）。
/// The Search gRPC handler: holds the assembled engine (built at server
/// startup, shared across requests).
pub struct SearchService<V: VectorStore + ?Sized> {
    engine: Arc<QueryEngine<V>>,
    /// 请求预算：query text 上限（§4）。
    /// Request budgets: the query-text cap (§4).
    max_query_chars: usize,
}

impl<V: VectorStore + ?Sized + 'static> SearchService<V> {
    pub fn new(engine: Arc<QueryEngine<V>>, max_query_chars: usize) -> Self {
        Self {
            engine,
            max_query_chars,
        }
    }
}

#[tonic::async_trait]
impl<V: VectorStore + ?Sized + 'static> Search for SearchService<V> {
    async fn search(
        &self,
        request: tonic::Request<SearchRequest>,
    ) -> std::result::Result<tonic::Response<SearchResponse>, tonic::Status> {
        let req = request.into_inner();
        // 预算（§4）：query text ≤ 16 KiB、top_k 1..=100、domain ≤ 128。
        // Budgets (§4): query text ≤ 16 KiB, top_k 1..=100, domain ≤ 128.
        if req.text.len() > self.max_query_chars {
            return Err(tonic::Status::invalid_argument("query text too long"));
        }
        if !(1..=100).contains(&req.top_k) {
            return Err(tonic::Status::invalid_argument(
                "top_k out of range 1..=100",
            ));
        }
        let filters: Filters = if req.filters_json.is_empty() {
            Filters::empty()
        } else {
            serde_json::from_str(&req.filters_json).map_err(|e| {
                tonic::Status::invalid_argument(format!("invalid filters_json: {e}"))
            })?
        };
        let query = Query {
            text: req.text.clone(),
            filters,
            top_k: req.top_k as usize,
            domain: Some(req.domain.clone()),
        };
        let result = self
            .engine
            .search(&query)
            .await
            .map_err(|e| crate::error::grpc_status(&e, "search failed"))?;
        let hits = result
            .hits
            .into_iter()
            .map(|h| SearchHit {
                page_id: h.page_id,
                entity_id: h.entity_id.to_key(),
                score: h.score,
                title: h.title,
            })
            .collect();
        let rewritten = result.rewritten.map(|r| RewrittenQuery {
            expanded_terms: r.expanded_terms,
            filters_json: serde_json::to_string(&r.filters).unwrap_or_else(|_| "{}".to_string()),
            boost_entity_ids: r.boost_entities.iter().map(|e| e.to_key()).collect(),
        });
        let diagnostics_json = serde_json::to_string(&result.diagnostics)
            .map_err(Error::Serialization)
            .map_err(|e| crate::error::grpc_status(&e, "diagnostics serialization failed"))?;
        Ok(tonic::Response::new(SearchResponse {
            hits,
            rewritten,
            rewrite_failure: result.rewrite_failure,
            diagnostics_json,
            latency_ms: result.latency_ms,
            log_id: result.log_id.unwrap_or(0),
        }))
    }
}
