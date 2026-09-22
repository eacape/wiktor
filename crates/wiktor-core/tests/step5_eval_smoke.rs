//! Step 5 批5 离线冒烟：用 `examples/milk-tea` 的真实页面、事实平面与
//! 134 条 golden 文件跑通 A/B/C 三档评测（mock 向量 + 确定性嵌入器，
//! 零网络）。
//! Step 5 batch-5 offline smoke: runs the A/B/C evaluation end-to-end over the
//! real `examples/milk-tea` pages, fact plane and the 134-record golden file
//! (mock vectors + deterministic embedder, zero network).
//!
//! 断言面刻意收窄：只要求评测**运行成功**（Ok）、三档指标已定义、决策为两态
//! 之一；不断言质量阈值（质量判定属 CLI 批6 / 人工审阅）。
//! The assertion surface is deliberately narrow: the run must **succeed** (Ok)
//! with defined tier metrics and one of the two verdicts; no quality thresholds
//! are asserted here (quality judgment belongs to the batch-6 CLI / human
//! review).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use wiktor_core::eval::{load_golden_set, run_evaluation, EvalConfig, QugDecision};
use wiktor_core::kernel::qug_store::build_and_publish_qug;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::qug::qug_build::{parse_intents, QugBuildOutcome};
use wiktor_core::seed;
use wiktor_core::traits::{DataSource, DistanceMetric, DomainConfig, EntityStore, VectorStore};
use wiktor_core::types::{Cursor, PublishStatus};
use wiktor_core::{data::DEFAULT_BATCH_SIZE, QueryEmbedder};

/// 确定性嵌入器（与 CLI embed.rs 同思路：字符哈希 → 桶，零网络）。
/// Deterministic embedder (same idea as the CLI's embed.rs: char hash →
/// bucket, zero network).
struct HashEmbedder {
    dim: usize,
}

#[async_trait]
impl QueryEmbedder for HashEmbedder {
    async fn embed(&self, text: &str) -> wiktor_core::Result<Vec<f32>> {
        let mut vec = vec![0.0_f32; self.dim];
        for ch in text.chars() {
            let h = blake3::hash(ch.to_string().as_bytes());
            let bucket =
                u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) as usize % self.dim;
            vec[bucket] += 1.0;
        }
        let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        Ok(vec)
    }
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("milk-tea")
}

#[tokio::test]
async fn real_golden_file_runs_through_all_tiers() {
    let domain_dir = examples_dir();
    let yaml = std::fs::read_to_string(domain_dir.join("domain.yaml")).unwrap();
    let config: DomainConfig = serde_yaml_ng::from_str(&yaml).unwrap();
    const DIM: usize = 64;
    let embedder = Arc::new(HashEmbedder { dim: DIM });

    // 1) seed 页面 + 事实平面（与 step2_golden 同一路径）；顺带收集实体 key
    //    （golden 引用校验）与向量点（metadata 与 accepted head 对齐，
    //    content_hash 用 seed_pages 同式 blake3(title\0content)）。
    // 1) Seed pages + fact plane (same path as step2_golden); also collect the
    //    entity keys (golden reference validation) and vector points (metadata
    //    aligned with the accepted head, content_hash per the seed_pages
    //    formula blake3(title\0content)).
    let kernel = SqliteKernel::open_in_memory().unwrap();
    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection("milk-tea", DIM, DistanceMetric::Cosine)
        .await
        .unwrap();

    let mut md_files: Vec<PathBuf> = std::fs::read_dir(domain_dir.join("seed-wiki"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();
    let mut known_entities: BTreeSet<String> = BTreeSet::new();
    let mut points: Vec<wiktor_core::traits::VectorPoint> = Vec::new();
    for path in &md_files {
        let content = std::fs::read_to_string(path).unwrap();
        let mut page = seed::parse_page(&content).unwrap();
        page.metadata.domain_pack_version = config.version.clone();
        let content_hash = blake3::hash(format!("{}\0{}", page.title, page.content).as_bytes())
            .to_hex()
            .to_string();
        known_entities.insert(page.entity_id.to_key());
        points.push(wiktor_core::traits::VectorPoint {
            id: page.page_id.clone(),
            vector: embedder
                .embed(&format!("{} {}", page.title, page.content))
                .await
                .unwrap(),
            metadata: wiktor_core::traits::VectorMetadata {
                entity_id: page.entity_id.to_key(),
                page_id: page.page_id.clone(),
                chunk_type: wiktor_core::traits::ChunkType::Summary,
                content_hash,
                generation: 1,
            },
        });
        kernel
            .seed_pages(&page, &config.name, PublishStatus::Accepted)
            .unwrap();
    }
    store.upsert("milk-tea", &points).await.unwrap();

    let entity = config
        .entities
        .iter()
        .find(|e| e.source.starts_with("jsonl://"))
        .expect("milk-tea has a jsonl entity source");
    let source = wiktor_core::data::JsonlDataSource::from_config(entity, &domain_dir).unwrap();
    let mut cursor: Option<Cursor> = None;
    loop {
        let batch = source.fetch(cursor.clone()).await.unwrap();
        if batch.is_empty() {
            break;
        }
        for raw in &batch {
            let facts = source.raw_to_facts(raw).unwrap();
            kernel
                .upsert_facts(&raw.id, &facts, raw.source_revision)
                .await
                .unwrap();
        }
        let offset = cursor.map(|c| c.offset).unwrap_or(0) + batch.len();
        cursor = Some(Cursor {
            offset,
            batch_size: DEFAULT_BATCH_SIZE,
        });
    }

    // 2) 发布 QUG 构建（真实 accepted 页 + 真实 intents.yaml 原文 bytes）。
    // 2) Publish the QUG build (real accepted pages + the real raw
    //    intents.yaml bytes).
    let intents_path = domain_dir.join(
        config
            .qug
            .intent_templates
            .clone()
            .unwrap_or_else(|| "intents.yaml".into()),
    );
    let intents_bytes = std::fs::read(&intents_path).unwrap();
    let snapshot = kernel
        .assemble_qug_snapshot(
            &config.name,
            &config.version,
            serde_json::to_string(&config.qug).unwrap(),
            intents_bytes.clone(),
        )
        .unwrap();
    let intents = parse_intents(&snapshot.intents_bytes).unwrap();
    let published = build_and_publish_qug(&kernel, &snapshot, &config, &intents, false).unwrap();
    assert!(
        matches!(published, QugBuildOutcome::Published(_)),
        "first build must publish"
    );

    // 3) 真实 134 条 golden + 三档评测。
    // 3) The real 134-record golden set + the three-tier evaluation.
    let golden_bytes = std::fs::read(domain_dir.join("golden-queries.jsonl")).unwrap();
    let golden = load_golden_set(&golden_bytes, &known_entities).unwrap();
    assert_eq!(golden.len(), 134);

    let out = run_evaluation(
        Arc::new(kernel),
        store,
        embedder,
        &config,
        &intents_bytes,
        &golden,
        &EvalConfig {
            top_k: 10,
            rrf_k: 60,
            collection: "milk-tea".into(),
            vector_backend: "mock".into(),
            command: "cargo test -p wiktor-core --test step5_eval_smoke".into(),
        },
    )
    .await
    .expect("the real-file evaluation must run end-to-end");

    assert!(matches!(
        out.decision.qug_decision,
        QugDecision::Enabled | QugDecision::Disabled
    ));
    for tier in [&out.tier_a, &out.tier_b, &out.tier_c] {
        assert!(
            tier.recall_at_10.is_some(),
            "positive recall must be defined"
        );
        assert!(
            tier.negative_precision.is_some(),
            "negative_precision must be defined"
        );
    }
    // A/B 档无 QUG → 无 rewrite_failure；fallback 只可能出现在 C 档。
    // Tiers A/B have no QUG → no rewrite_failure; fallback can only come from
    // tier C.
    assert_eq!(out.tier_a.fallback_count, 0);
    assert_eq!(out.tier_b.fallback_count, 0);
    // legacy 34 条 + 新格式 100 条（loader 口径）。
    // 34 legacy + 100 new-format records (the loader's account).
    assert_eq!(out.kind_counts.get("legacy"), Some(&34));
    let new_total: usize = out
        .kind_counts
        .iter()
        .filter(|(k, _)| k.as_str() != "legacy")
        .map(|(_, n)| n)
        .sum();
    assert_eq!(new_total, 100, "100 new-format records");
    assert!(out.source_hash.is_some(), "an active build exists");

    eprintln!(
        "SMOKE decision={:?} gain_pp={:?} A@10={:?} B@10={:?} C@10={:?} np(A/B/C)={:?}/{:?}/{:?} fallback_c={} applied_c={}",
        out.decision.qug_decision,
        out.decision.gain_pp,
        out.tier_a.recall_at_10,
        out.tier_b.recall_at_10,
        out.tier_c.recall_at_10,
        out.tier_a.negative_precision,
        out.tier_b.negative_precision,
        out.tier_c.negative_precision,
        out.tier_c.fallback_count,
        out.decision.active_qug_samples,
    );
}
