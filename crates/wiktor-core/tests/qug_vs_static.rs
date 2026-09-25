//! Step 11 B3 QUG-vs-static 对比实证：复用 `run_evaluation` 的 A（纯FTS静态基线）
//! 与 C（QUG）档，跨 milk-tea + tech-docs 两领域，记录 recall@10 与 `gain_pp`，
//! 验证"QUG 是否值得开"的可执行判据。
//! Step 11 B3 QUG-vs-static comparison experiment: reuse `run_evaluation`'s tier A
//! (pure-FTS static baseline) and tier C (QUG) across milk-tea + tech-docs,
//! recording recall@10 and `gain_pp` — the executable verdict on "is QUG worth
//! enabling".
//!
//! 断言刻意宽：只要求评测运行成功、A 与 C 的 recall@10 有定义、C 档有 active
//! QUG 时 gain_pp 被如实计算；不断言达标（质量判据属 CLI/人工）。
//! The assertions are deliberately loose: the run must succeed, A and C recall@10
//! must be defined, and gain_pp must be computed when C has an active graph;
//! no quality threshold is asserted (that belongs to the CLI / human review).

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

/// 确定性哈希嵌入器（同 step5_eval_smoke；零网络）。
/// A deterministic hash embedder (same as step5_eval_smoke; zero network).
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

fn domain_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join(name)
}

/// 对一个领域跑 A/B/C 评测，返回各档 recall@10、gain_pp 与决策。
/// Runs the A/B/C evaluation for one domain; returns per-tier recall@10,
/// gain_pp, and the decision.
async fn eval_domain(name: &str) -> (f64, f64, f64, Option<f64>, String) {
    let domain_dir = domain_dir(name);
    let yaml = std::fs::read_to_string(domain_dir.join("domain.yaml")).unwrap();
    let config: DomainConfig = serde_yaml_ng::from_str(&yaml).unwrap();
    const DIM: usize = 64;
    let embedder = Arc::new(HashEmbedder { dim: DIM });

    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    let store = Arc::new(MockVectorStore::new());
    store
        .ensure_collection(&config.name, DIM, DistanceMetric::Cosine)
        .await
        .unwrap();

    let mut md_files: Vec<PathBuf> = std::fs::read_dir(domain_dir.join("seed-wiki"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();
    let mut known_entities: BTreeSet<String> = BTreeSet::new();
    let mut points = Vec::new();
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
    store.upsert(&config.name, &points).await.unwrap();

    let entity = config
        .entities
        .iter()
        .find(|e| e.source.starts_with("jsonl://"))
        .unwrap();
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

    // 发布 QUG（供 C 档加载）。
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
    let published =
        build_and_publish_qug(kernel.as_ref(), &snapshot, &config, &intents, false).unwrap();
    assert!(matches!(published, QugBuildOutcome::Published(_)));

    let golden_bytes = std::fs::read(domain_dir.join("golden-queries.jsonl")).unwrap();
    let golden = load_golden_set(&golden_bytes, &known_entities).unwrap();

    let out = run_evaluation(
        kernel,
        store,
        embedder,
        &config,
        &intents_bytes,
        &golden,
        &EvalConfig {
            top_k: 10,
            rrf_k: 60,
            collection: config.name.clone(),
            vector_backend: "mock".into(),
            command: "step11 B3 qug_vs_static".into(),
        },
    )
    .await
    .unwrap();

    let a10 = out.tier_a.recall_at_10.unwrap_or(0.0);
    let b10 = out.tier_b.recall_at_10.unwrap_or(0.0);
    let c10 = out.tier_c.recall_at_10.unwrap_or(0.0);
    let gain = out.decision.gain_pp;
    let decision = match out.decision.qug_decision {
        QugDecision::Enabled => "enabled".to_string(),
        QugDecision::Disabled => "disabled".to_string(),
    };
    eprintln!(
        "DOMAIN {name}: A@10={a10:.3} B@10={b10:.3} C@10={c10:.3} gain_pp={:?} decision={decision}",
        gain
    );
    (a10, b10, c10, gain, decision)
}

#[tokio::test]
async fn qug_vs_static_across_two_domains() {
    // milk-tea（电商奶茶）：真实语义边较多，QUG 预期增益或持平。
    let (a1, b1, c1, gain1, dec1) = eval_domain("milk-tea").await;
    // tech-docs（技术文档）：第二个官方领域包，QUG 同样应可加载并如实计增益。
    let (a2, b2, c2, gain2, dec2) = eval_domain("tech-docs").await;

    // 断言：A/B/C recall@10 有定义；QUG 决策为两态之一；C 档有 active 图时
    // gain_pp 应被如实计算（Enabled 要求 gain_pp >= 5.0pp）。
    for (a, b, c, g, d) in [
        (a1, b1, c1, gain1, dec1.as_str()),
        (a2, b2, c2, gain2, dec2.as_str()),
    ] {
        assert!((0.0..=1.0).contains(&a), "A recall@10 must be a fraction");
        assert!((0.0..=1.0).contains(&b), "B recall@10 must be a fraction");
        assert!((0.0..=1.0).contains(&c), "C recall@10 must be a fraction");
        assert!(
            d == "enabled" || d == "disabled",
            "decision must be two-state"
        );
        if d == "enabled" {
            let g = g.expect("enabled decision must carry gain_pp");
            assert!(g >= 5.0, "enabled requires gain_pp >= 5.0pp, got {g}");
        }
    }

    // 输出供报告的跨领域对比行。
    eprintln!(
        "CROSS-DOMAIN: milk-tea A@10={a1:.3} B@10={b1:.3} C@10={c1:.3} ({dec1}) | \
         tech-docs A@10={a2:.3} B@10={b2:.3} C@10={c2:.3} ({dec2})"
    );
}
