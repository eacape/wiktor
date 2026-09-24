//! Meilisearch 出口实现（STEP10 D6，B4）。
//! The Meilisearch outlet implementation (STEP10 D6, B4).

use serde::Serialize;
use wiktor_core::kernel::AcceptedPageVector;
use wiktor_core::types::error::Result;

/// env：Meilisearch 实例基础 URL（默认本地开发端口）。
/// Env: the Meilisearch instance base URL (defaults to the local dev port).
pub const WIKTOR_MEILISEARCH_URL_ENV: &str = "WIKTOR_MEILISEARCH_URL";
/// env：可选 admin/search API key（Bearer 认证；Meilisearch 默认无 key）。
/// Env: optional admin/search API key (Bearer auth; Meilisearch has no key by
/// default).
pub const WIKTOR_MEILISEARCH_API_KEY_ENV: &str = "WIKTOR_MEILISEARCH_API_KEY";

/// 一个 accepted 页 → Meilisearch 文档（`id` 是 Meilisearch 的文档主键，
/// 用 page_id 保证同页幂等覆盖；其余字段供过滤/检索）。
/// An accepted page → a Meilisearch document (`id` is Meilisearch's document
/// primary key, using page_id for idempotent overwrite; the rest serve
/// filtering/search).
#[derive(Serialize)]
pub struct MeiliDocument {
    pub id: String,
    pub page_id: String,
    pub entity_id: String,
    pub title: String,
    pub content: String,
    pub content_hash: String,
    pub generation: u64,
}

/// 把 accepted 页 add-or-update 到 Meilisearch 的导出器。
/// Exporter that add-or-updates accepted pages into Meilisearch.
///
/// 装配由 CLI/env 完成：`from_env(index)` 读
/// `WIKTOR_MEILISEARCH_URL`（默认 http://localhost:7700）与
/// `WIKTOR_MEILISEARCH_API_KEY`（可选）。
/// Assembly is done by the CLI/env: `from_env(index)` reads
/// `WIKTOR_MEILISEARCH_URL` (default http://localhost:7700) and
/// `WIKTOR_MEILISEARCH_API_KEY` (optional).
pub struct MeilisearchExporter {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    index: String,
}

impl MeilisearchExporter {
    /// 从环境变量构造（index 由调用方提供，通常为 domain 名）。
    /// Builds from env vars (`index` is supplied by the caller, usually the
    /// domain name).
    pub fn from_env(index: &str) -> Result<Self> {
        let base_url = std::env::var(WIKTOR_MEILISEARCH_URL_ENV)
            .unwrap_or_else(|_| "http://localhost:7700".to_owned());
        let api_key = std::env::var(WIKTOR_MEILISEARCH_API_KEY_ENV).ok();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| wiktor_core::Error::External(format!("build meilisearch client: {e}")))?;
        Ok(Self {
            client,
            base_url,
            api_key,
            index: index.to_owned(),
        })
    }

    /// 确保 index 存在（PUT /indexes/{uid}；Meilisearch 对已存在 index 幂等）。
    /// Ensures the index exists (PUT /indexes/{uid}; idempotent for an existing
    /// index in Meilisearch).
    pub async fn ensure_index(&self) -> Result<()> {
        let url = format!("{}/indexes/{}", self.base_url, self.index);
        self.request(reqwest::Method::PUT, &url, None::<&str>).await
    }

    /// 把 accepted 页 add-or-update 到 index（PATCH /indexes/{uid}/documents；
    /// 按文档 id 幂等合并，镜像 accepted 页快照）。返回导出的文档数。
    /// Add-or-updates accepted pages into the index (PATCH
    /// /indexes/{uid}/documents; idempotent merge by document id, mirroring the
    /// accepted-page snapshot). Returns the number of documents exported.
    pub async fn export_pages(&self, pages: &[AcceptedPageVector]) -> Result<usize> {
        if pages.is_empty() {
            return Ok(0);
        }
        let docs: Vec<MeiliDocument> = pages
            .iter()
            .map(|p| MeiliDocument {
                id: p.page_id.clone(),
                page_id: p.page_id.clone(),
                entity_id: p.entity_id.clone(),
                title: p.title.clone(),
                content: p.content.clone(),
                content_hash: p.content_hash.clone(),
                generation: p.generation,
            })
            .collect();
        let url = format!("{}/indexes/{}/documents", self.base_url, self.index);
        self.request(reqwest::Method::PATCH, &url, Some(&docs))
            .await?;
        Ok(docs.len())
    }

    async fn request<T: serde::Serialize + ?Sized>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&T>,
    ) -> Result<()> {
        let mut req = self.client.request(method, url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req = req.header("Accept", "application/json");
        let req = if let Some(b) = body { req.json(b) } else { req };
        let resp = req
            .send()
            .await
            .map_err(|e| wiktor_core::Error::External(format!("meilisearch request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(wiktor_core::Error::External(format!(
                "meilisearch {status}: {text}"
            )));
        }
        Ok(())
    }
}
