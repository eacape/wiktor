//! Step1 独立测试审查补充（测试工程师 review，2026-09-20）。
//! Step1 supplemental review tests (test-engineer review, 2026-09-20).
//!
//! 覆盖任务点：
//! Covered task points:
//! 1. EntityId::from_key 非法输入（少于 3 段 / 空段 / 含冒号）。
//!    Invalid inputs (fewer than 3 parts / empty parts / embedded colon).
//! 2. MockVectorStore：缺失 collection 搜索报错；upsert 后 ensure_collection 幂等不丢点；
//!    MockVectorStore: missing-collection search errors; ensure_collection after upsert is idempotent and preserves points;
//!    余弦得分数值正确性；delete 对不存在的 collection 的语义。
//!    cosine-score correctness; and delete semantics for a missing collection.
//! 3. facts CAS 并发：多线程同实体不同 source_revision，最终只保留最大 revision 生效。
//!    facts CAS concurrency: multiple threads write different source revisions for one entity; only the maximum wins.
//! 4. 回归用例（2026-09-20 修复后转正）：
//!    Regression cases (promoted after the 2026-09-20 fix):
//!    - `numeric_range_filter_finds_expected_entity`：NumericRange 参数顺序曾错误
//!      （field_name 的 ? 排在 min/max 之前，但参数按 [min, max, field] 入栈），
//!      任何 NumericRange 过滤都查不到结果；修复后必须命中。
//!    - `reflist_write_respects_cas`：upsert_facts 中 fact_refs 的删插须受
//!      source_revision CAS 保护，旧版本不得覆盖新版本的 reflist 值。

use std::collections::BTreeMap;
use std::sync::Arc;

use wiktor_core::traits::{
    ChunkType, DistanceMetric, EntityStore, VectorMetadata, VectorPoint, VectorStore,
};
use wiktor_core::types::{EntityId, Error, FactValue, Facts, FilterCondition, Filters};
use wiktor_core::{MockVectorStore, SqliteKernel};

// ---------------------------------------------------------------- EntityId
// ---------------------------------------------------------------- EntityId

fn meta(entity: &str) -> VectorMetadata {
    VectorMetadata {
        entity_id: entity.to_string(),
        page_id: format!("page/{entity}"),
        chunk_type: ChunkType::Summary,
        content_hash: "hash".into(),
        generation: 1,
    }
}

fn facts_with(
    id: &EntityId,
    revision: u64,
    scalar: Option<(String, FactValue)>,
    reflist: Option<(String, Vec<String>)>,
) -> Facts {
    let mut fields = BTreeMap::new();
    if let Some((k, v)) = scalar {
        fields.insert(k, v);
    }
    if let Some((k, refs)) = reflist {
        fields.insert(k, FactValue::RefList(refs));
    }
    Facts {
        entity_id: id.clone(),
        fields,
        source_revision: revision,
    }
}

#[test]
fn entity_id_from_key_rejects_fewer_than_three_segments() {
    // 少于 3 段："a:b" / "a" / 空串
    // Fewer than 3 parts: "a:b" / "a" / empty string
    for bad in ["a:b", "a", ""] {
        let err = EntityId::from_key(bad).expect_err(&format!("{bad:?} should be rejected"));
        assert!(
            matches!(err, Error::InvalidEntityId(_)),
            "expected InvalidEntityId, got {err:?}"
        );
    }
}

#[test]
fn entity_id_from_key_rejects_empty_segments() {
    // 空段：中间空 / id 空 / 全空段
    // Empty parts: empty middle component / empty id / all parts empty
    for bad in ["a::b", "a:b:", "::", "::c", "a::"] {
        let err = EntityId::from_key(bad).expect_err(&format!("{bad:?} should be rejected"));
        assert!(
            matches!(err, Error::InvalidEntityId(_)),
            "expected InvalidEntityId, got {err:?}"
        );
    }
}

#[test]
fn entity_id_from_key_rejects_colon_inside_component() {
    // splitn(3) 会把 "a:b:c:d" 拆成 ["a","b","c:d"]，id 含冒号必须报错
    // splitn(3) makes ["a", "b", "c:d"]; an id containing a colon must error
    for bad in ["a:b:c:d", "a:b:c:d:e", "a:b:x:y"] {
        let err = EntityId::from_key(bad).expect_err(&format!("{bad:?} should be rejected"));
        assert!(
            matches!(err, Error::InvalidEntityId(_)),
            "expected InvalidEntityId, got {err:?}"
        );
    }
}

#[test]
fn entity_id_from_key_roundtrip_with_unicode_id() {
    // 合法 id 含 unicode（不为空、不含冒号）应无损往返
    // A valid Unicode id (non-empty, no colon) must round-trip losslessly
    let id = EntityId::new("电商", "商品", "sku_珍珠_001").unwrap();
    assert_eq!(EntityId::from_key(&id.to_key()).unwrap(), id);
}

// ---------------------------------------------------------------- MockVectorStore
// ---------------------------------------------------------------- MockVectorStore

#[tokio::test]
async fn mock_search_missing_collection_errors() {
    let store = MockVectorStore::new();
    let err = store
        .search("never_created", &[1.0, 0.0], 10, None)
        .await
        .expect_err("search on missing collection must error");
    assert!(matches!(err, Error::VectorStore(_)));
}

#[tokio::test]
async fn mock_search_created_but_empty_collection_returns_empty() {
    // ensure_collection 后（空 collection）搜索：不应报错，应返回空
    // Searching after ensure_collection (empty collection) should not error and should return empty
    let store = MockVectorStore::new();
    store
        .ensure_collection("c", 2, DistanceMetric::Cosine)
        .await
        .unwrap();
    let hits = store.search("c", &[1.0, 0.0], 10, None).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn mock_upsert_then_ensure_collection_is_idempotent_and_keeps_points() {
    // upsert（mock 自动建 collection）后再 ensure_collection 不应清空数据
    // ensure_collection after upsert (mock auto-creates the collection) must not clear data
    let store = MockVectorStore::new();
    store
        .upsert(
            "c",
            &[VectorPoint {
                id: "p1".into(),
                vector: vec![1.0, 0.0],
                metadata: meta("dom:prod:a"),
            }],
        )
        .await
        .unwrap();
    // 幂等：重复调用不报错
    // Idempotent: repeated calls do not error
    for _ in 0..3 {
        store
            .ensure_collection("c", 2, DistanceMetric::Cosine)
            .await
            .unwrap();
    }
    let hits = store.search("c", &[1.0, 0.0], 10, None).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "ensure_collection must not wipe existing points"
    );
    assert_eq!(hits[0].id, "p1");
}

#[tokio::test]
async fn mock_cosine_scores_are_numerically_correct() {
    // 正交向量得分 0，同向得分 1；验证分数本身而非仅排序
    // Orthogonal vectors score 0 and aligned vectors score 1; verify scores, not only ordering
    let store = MockVectorStore::new();
    store
        .ensure_collection("c", 3, DistanceMetric::Cosine)
        .await
        .unwrap();
    store
        .upsert(
            "c",
            &[
                VectorPoint {
                    id: "x".into(),
                    vector: vec![1.0, 0.0, 0.0],
                    metadata: meta("dom:prod:x"),
                },
                VectorPoint {
                    id: "y".into(),
                    vector: vec![0.0, 1.0, 0.0],
                    metadata: meta("dom:prod:y"),
                },
            ],
        )
        .await
        .unwrap();

    let hits = store.search("c", &[1.0, 0.0, 0.0], 10, None).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].id, "x");
    assert!(
        (hits[0].score - 1.0).abs() < 1e-5,
        "same direction => 1.0, got {}",
        hits[0].score
    );
    assert!(
        (hits[1].score - 0.0).abs() < 1e-5,
        "orthogonal => 0.0, got {}",
        hits[1].score
    );
}

#[tokio::test]
async fn mock_search_candidate_ids_filters_by_entity_key_not_point_id() {
    // 候选过滤按 payload.entity_id（to_key）匹配；point id 故意与 entity 不同
    // Candidate filtering matches payload.entity_id (to_key); point id is intentionally different from the entity
    let store = MockVectorStore::new();
    let key = "dom:prod:target";
    store
        .upsert(
            "c",
            &[
                VectorPoint {
                    id: "uuid-1".into(), // point id 与 entity key 无关
                    // Point id is unrelated to the entity key
                    vector: vec![1.0, 0.0],
                    metadata: meta(key),
                },
                VectorPoint {
                    id: "uuid-2".into(),
                    vector: vec![0.0, 1.0],
                    metadata: meta("dom:prod:other"),
                },
            ],
        )
        .await
        .unwrap();
    let target = EntityId::from_key(key).unwrap();
    let hits = store
        .search("c", &[1.0, 0.0], 10, Some(std::slice::from_ref(&target)))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].metadata.entity_id, key);
}

#[tokio::test]
async fn mock_delete_on_missing_collection_is_silent_noop() {
    // 记录当前语义（与 qdrant 的 delete 行为差异见报告）：mock 对不存在 collection 静默成功
    // Record current semantics (see the report for the qdrant delete difference): mock silently succeeds for a missing collection
    let store = MockVectorStore::new();
    store.delete("never_created", &["p1".into()]).await.unwrap();
}

// ---------------------------------------------------------------- facts CAS 并发（期望通过）
// ---------------------------------------------------------------- facts CAS concurrency (expected to pass)

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_upsert_facts_only_max_revision_wins_scalar() {
    // 8 个线程写同一实体、互不相同的 source_revision(1..=8)，随机延迟打乱顺序；
    // Eight threads write one entity with distinct source revisions (1..=8), with random delays scrambling arrival order;
    // CAS 保证最终每个字段都是最大 revision(8) 的值。
    // CAS guarantees every field ends with the value from the maximum revision (8).
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let id = EntityId::new("ecommerce", "product", "concurrent").unwrap();

    let mut handles = Vec::new();
    for i in 1..=8u64 {
        let kernel = kernel.clone();
        let id = id.clone();
        handles.push(tokio::spawn(async move {
            // 随机小延迟打乱到达顺序
            // Small random delays scramble arrival order
            let delay_ms = (i * 7919 % 23) * 3;
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            let facts = facts_with(
                &id,
                i,
                Some(("price".to_string(), FactValue::Numeric(i as f64 * 10.0))),
                None,
            );
            kernel.upsert_facts(&id, &facts, i).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let got = kernel.get_facts(&id).await.unwrap().expect("facts present");
    assert_eq!(
        got.fields.get("price"),
        Some(&FactValue::Numeric(80.0)),
        "only max revision (8) must survive"
    );
    assert_eq!(got.source_revision, 8);
}

// ---------------------------------------------------------------- 回归用例（原 bug 复现，已修复）
// ---------------------------------------------------------------- Regression cases (original bug reproduced and fixed)

#[tokio::test]
async fn numeric_range_filter_finds_expected_entity() {
    // 预期行为：price ∈ [10, 20] 应命中 price=15 的实体。
    // Expected: price ∈ [10, 20] should match the entity with price=15.
    // 现状 bug：translate_filters 的 NumericRange 把参数按 [min, max, field_name]
    // Former bug: translate_filters pushed NumericRange parameters as [min, max, field_name]
    // 入栈，而 SQL 中 ? 顺序是 (field_name = ? AND value_numeric >= ? AND ...)，
    // while SQL placeholder order is (field_name = ? AND value_numeric >= ? AND ...),
    // 导致 field_name 被绑定成 min 值（REAL），永远查不到 → 结果为空。
    // causing field_name to bind to min (REAL), so nothing could match → empty result.
    let kernel = SqliteKernel::open_in_memory().unwrap();
    let id = EntityId::new("ecommerce", "product", "a").unwrap();
    kernel
        .upsert_facts(
            &id,
            &facts_with(
                &id,
                1,
                Some(("price".to_string(), FactValue::Numeric(15.0))),
                None,
            ),
            1,
        )
        .await
        .unwrap();

    let hits = kernel
        .filter(&Filters {
            conditions: vec![FilterCondition::NumericRange {
                field: "price".into(),
                min: Some(10.0),
                max: Some(20.0),
            }],
        })
        .await
        .unwrap();

    assert_eq!(
        hits,
        vec![id.clone()],
        "NumericRange filter must find price=15 in [10,20]; \
         if empty, translate_filters param ordering is broken (REPRO)"
    );
}

#[tokio::test]
async fn reflist_write_respects_cas() {
    // 预期行为：revision=1 的 reflist 写回后不得覆盖 revision=2 已生效的 reflist。
    // Expected: writing the revision=1 reflist must not overwrite the active revision=2 reflist.
    // 现状 bug：upsert_facts 中 fact_refs 的 DELETE+INSERT 不经过 CAS，
    // Former bug: fact_refs DELETE+INSERT in upsert_facts bypassed CAS,
    // 旧版本会把新版本的引用列表覆盖，而 facts 行本身（source_revision）保持为 2，
    // so an old revision overwrote the new revision's reference list while the facts row itself (source_revision) remained 2,
    // 造成 get_facts 读到 revision=2 但 refs 却是 revision=1 的内容。
    // leaving get_facts with revision=2 but references from revision=1.
    let kernel = SqliteKernel::open_in_memory().unwrap();
    let id = EntityId::new("ecommerce", "product", "r1").unwrap();

    let new = facts_with(
        &id,
        2,
        None,
        Some(("ingredient_ids".into(), vec!["pearl".into(), "taro".into()])),
    );
    kernel.upsert_facts(&id, &new, 2).await.unwrap();

    let stale = facts_with(
        &id,
        1,
        None,
        Some(("ingredient_ids".into(), vec!["old_stale".into()])),
    );
    kernel.upsert_facts(&id, &stale, 1).await.unwrap();

    let got = kernel.get_facts(&id).await.unwrap().expect("facts present");
    assert_eq!(
        got.source_revision, 2,
        "stale revision must not regress source_revision"
    );
    assert_eq!(
        got.fields.get("ingredient_ids"),
        Some(&FactValue::RefList(vec!["pearl".into(), "taro".into()])),
        "stale revision must not overwrite newer reflist values (REPRO)"
    );
}
