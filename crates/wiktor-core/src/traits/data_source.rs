use crate::types::error::Result;
use crate::types::{Cursor, RawEntity};
use async_trait::async_trait;

/// 数据源适配器（插件点 1：JSONL 起步，postgres 同接口另实现）。
#[async_trait]
pub trait DataSource: Send + Sync {
    async fn fetch(&self, cursor: Option<Cursor>) -> Result<Vec<RawEntity>>;
    fn schema(&self) -> EntitySchema;
}

/// 源数据 Schema。
#[derive(Debug, Clone)]
pub struct EntitySchema {
    pub entity_type: String,
    pub fields: Vec<crate::types::FieldDefinition>,
}
