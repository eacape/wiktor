use crate::types::error::Result;
use crate::types::{EntityId, Facts, Filters};
use async_trait::async_trait;

/// 事实平面存储（SQLite 实现由 kernel 提供）。
#[async_trait]
pub trait EntityStore: Send + Sync {
    /// 幂等写入事实；`source_revision` 用于 CAS——旧版本不得覆盖新版本。
    async fn upsert_facts(&self, id: &EntityId, facts: &Facts, source_revision: u64) -> Result<()>;
    /// 事实过滤下推：返回候选实体 ID 集合（供向量检索预筛）。
    async fn filter(&self, filters: &Filters) -> Result<Vec<EntityId>>;
    /// 删除事实（tombstone 传播）。
    async fn delete_facts(&self, id: &EntityId) -> Result<()>;
    /// 读取单实体事实。
    async fn get_facts(&self, id: &EntityId) -> Result<Option<Facts>>;
}
