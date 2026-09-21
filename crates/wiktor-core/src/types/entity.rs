use crate::types::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// 实体全局唯一标识：`domain:entity_type:id`。
/// Globally unique entity identifier: `domain:entity_type:id`。
///
/// 组件不允许包含 `:`，也不允许为空——保证 `to_key` / `from_key` 严格无损往返。
/// Components must be non-empty and cannot contain `:` — this guarantees lossless `to_key` / `from_key` round trips.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId {
    pub domain: String,
    pub entity_type: String,
    pub id: String,
}

impl EntityId {
    pub fn new(
        domain: impl Into<String>,
        entity_type: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self> {
        let domain = domain.into();
        let entity_type = entity_type.into();
        let id = id.into();
        for (label, value) in [
            ("domain", &domain),
            ("entity_type", &entity_type),
            ("id", &id),
        ] {
            if value.is_empty() {
                return Err(Error::InvalidEntityId(format!("{label} is empty")));
            }
            if value.contains(':') {
                return Err(Error::InvalidEntityId(format!(
                    "{label} contains ':' (got {value:?})"
                )));
            }
        }
        Ok(Self {
            domain,
            entity_type,
            id,
        })
    }

    /// 序列化为 `domain:entity_type:id`。
    /// Serializes as `domain:entity_type:id`.
    pub fn to_key(&self) -> String {
        format!("{}:{}:{}", self.domain, self.entity_type, self.id)
    }

    /// 从 `domain:entity_type:id` 解析。
    /// Parses from `domain:entity_type:id`.
    pub fn from_key(key: &str) -> Result<Self> {
        let parts: Vec<&str> = key.splitn(3, ':').collect();
        if parts.len() != 3 {
            return Err(Error::InvalidEntityId(format!(
                "expected 'domain:type:id', got {key:?}"
            )));
        }
        Self::new(parts[0], parts[1], parts[2])
    }
}

/// 源数据抓回的原始实体（JSONL 一行）。
/// Raw entity fetched from a source (one JSONL line).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEntity {
    pub id: EntityId,
    /// 原始字段（语义由领域包 EntitySchema 描述）。
    /// Raw fields (semantics are described by the domain pack EntitySchema).
    pub fields: std::collections::BTreeMap<String, serde_json::Value>,
    pub source_revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_lossless() {
        let id = EntityId::new("ecommerce", "product", "sku_1001").unwrap();
        let key = id.to_key();
        assert_eq!(EntityId::from_key(&key).unwrap(), id);
    }

    #[test]
    fn rejects_colon_in_components() {
        assert!(EntityId::new("a:b", "product", "1").is_err());
    }

    #[test]
    fn rejects_empty_components() {
        assert!(EntityId::new("", "product", "1").is_err());
        assert!(EntityId::from_key("a:product:").is_err());
    }
}
