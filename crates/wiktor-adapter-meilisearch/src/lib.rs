//! Wiktor 可选插件：把 accepted 页镜像到 Meilisearch（外部检索引擎出口）。
//! Wiktor optional plugin: mirrors accepted pages into Meilisearch (the external
//! search-engine outlet).
//!
//! 这是 MASTER-PLAN §154 的"外部检索引擎是可选插件、不是产品身份"的一个出口：
//! 只做**导出/同步**，不进默认查询路径（STEP10 D6，B4）。它消费
//! [`wiktor_core::kernel::accepted_page_vectors`]（accepted 页的 title/content/
//! entity_id/content_hash/generation），把每个 accepted 页作为一个文档
//! add-or-update 到 `${WIKTOR_MEILISEARCH_URL}`。未过审（quarantine）页不在
//! accepted_page_vectors 中，天然不导出。
//! This is one outlet of MASTER-PLAN §154's "the external search engine is an
//! optional plugin, not the product identity": it only **exports/syncs** and
//! never joins the default query path (STEP10 D6, B4). It consumes
//! [`wiktor_core::kernel::accepted_page_vectors`] (accepted pages' title/
//! content/entity_id/content_hash/generation) and add-or-updates each accepted
//! page as a document at `${WIKTOR_MEILISEARCH_URL}`. Unreviewed (quarantine)
//! pages are not in accepted_page_vectors, so they are never exported.

mod meilisearch;

pub use meilisearch::{
    MeilisearchExporter, WIKTOR_MEILISEARCH_API_KEY_ENV, WIKTOR_MEILISEARCH_URL_ENV,
};
