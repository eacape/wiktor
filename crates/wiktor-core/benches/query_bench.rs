//! Step 11 B1 性能基准：离线驱动 `QueryEngine::search`，测 A（纯FTS）/B（混合）/
//! C（QUG）三档延迟（P50/P95/P99）。
//! Step 11 B1 performance benchmark: drive `QueryEngine::search` offline and
//! measure tiers A (pure FTS) / B (hybrid) / C (QUG) latency quantiles.
//!
//! 数据：`examples/milk-tea`（20 知识页 + 120 事实 + 134 golden，Mock 向量 +
//! 确定性嵌入，零网络）。一个共享 kernel/store 承载数据；三档引擎各预构建一次
//! （QUG 仅 C 档加载一次，不计入计时），golden 查询（含真实过滤）作负载集。
//! 每档对全部去重查询重复 N 轮，收集每次 `QueryResult.latency_ms`，算
//! P50/P95/P99 输出到 stdout（供 step11-benchmarks 报告引用）。criterion 另给
//! 平均耗时与吞吐。
//! Data: `examples/milk-tea` (20 pages + 120 facts + 134 goldens; Mock vectors +
//! deterministic embedding, zero network). One shared kernel/store holds the
//! data; the three engines are each prebuilt once (QUG loads once for tier C,
//! outside the timed region); golden queries (with their real filters) form the
//! load set. Each tier runs every deduplicated query for N rounds, collecting each
//! `QueryResult.latency_ms` to compute P50/P95/P99 on stdout (for the
//! step11-benchmarks report). criterion also reports the mean time and throughput.
//!
//! 运行：`cargo bench -p wiktor-core --bench query_bench`
//! Run: `cargo bench -p wiktor-core --bench query_bench`

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use wiktor_core::eval::runner::golden_filters_to_filters;
use wiktor_core::eval::{load_golden_set, GoldenQuery};
use wiktor_core::kernel::qug_store::build_and_publish_qug;
use wiktor_core::kernel::{MockVectorStore, SqliteKernel};
use wiktor_core::query_engine::qug::qug_build::{parse_intents, QugBuildOutcome};
use wiktor_core::query_engine::qug::QugGraph;
use wiktor_core::query_engine::QueryEngine;
use wiktor_core::seed;
use wiktor_core::traits::{DataSource, DistanceMetric, DomainConfig, EntityStore, VectorStore};
use wiktor_core::types::{Cursor, PublishStatus, Query};
use wiktor_core::{data::DEFAULT_BATCH_SIZE, QueryEmbedder};

/// 确定性哈希嵌入器（同 step5_eval_smoke；零网络）。
/// A deterministic hash embedder (same as step5_eval_smoke; zero network).
#[derive(Clone)]
struct HashEmbedder {
    dim: usize,
}

impl QueryEmbedder for HashEmbedder {
    fn embed<'life0, 'life1, 'async_trait>(
        &'life0 self,
        text: &'life1 str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = wiktor_core::Result<Vec<f32>>>
                + std::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
    {
        Box::pin(async move {
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
        })
    }
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("milk-tea")
}

/// 预构建的基准夹具：共享 kernel / store + 三档引擎 + golden 查询负载。
/// A prebuilt fixture: shared kernel/store + the three engines + the golden load.
struct BenchEnv {
    config: DomainConfig,
    queries: Vec<GoldenQuery>,
    engines: Vec<QueryEngine<MockVectorStore>>,
}

/// 构造一档引擎（共享同一 seed 数据，仅检索配置不同）。
/// Builds one tier's engine over the same seeded data (retrieval config differs).
fn make_engine(
    kernel: &Arc<SqliteKernel>,
    store: &Arc<MockVectorStore>,
    config: &DomainConfig,
    tier: Tier,
) -> QueryEngine<MockVectorStore> {
    let embedder = Arc::new(HashEmbedder { dim: 64 });
    let qug: Option<Arc<QugGraph>> = if matches!(tier, Tier::C) {
        let intents_bytes = std::fs::read(examples_dir().join("intents.yaml")).unwrap();
        wiktor_core::kernel::qug_store::load_active_qug(kernel, config, &intents_bytes).unwrap()
    } else {
        None
    };
    let mut e = QueryEngine::new(
        kernel.clone(),
        store.clone(),
        qug,
        embedder,
        config.name.clone(),
        5,
        60,
    )
    .unwrap();
    if matches!(tier, Tier::A) {
        e.fts_only = true;
    }
    e
}

/// 收集基准环境：seed 一次、每档引擎建一次、golden 查询去重。
/// Collects the bench env: seeds once, builds each tier engine once, dedupes the
/// golden queries.
fn build_env() -> BenchEnv {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let domain_dir = examples_dir();
        let yaml = std::fs::read_to_string(domain_dir.join("domain.yaml")).unwrap();
        let config: DomainConfig = serde_yaml_ng::from_str(&yaml).unwrap();

        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let store = Arc::new(MockVectorStore::new());
        store
            .ensure_collection(&config.name, 64, DistanceMetric::Cosine)
            .await
            .unwrap();
        let embedder = Arc::new(HashEmbedder { dim: 64 });

        // seed 页面 + 向量点 + 收集实体 key。
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

        // 事实平面。
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

        // QUG 构建并发布（供 C 档加载）。
        let intents_path = domain_dir.join("intents.yaml");
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

        // golden 查询负载（去重文本，保留过滤条件）。
        let golden_bytes = std::fs::read(domain_dir.join("golden-queries.jsonl")).unwrap();
        let golden = load_golden_set(&golden_bytes, &known_entities).unwrap();
        let mut seen = BTreeSet::new();
        let queries: Vec<GoldenQuery> = golden
            .queries()
            .iter()
            .filter(|q| seen.insert(q.query.clone()))
            .cloned()
            .collect();

        // 三档引擎（各建一份；共享数据）。
        let engines = vec![
            make_engine(&kernel, &store, &config, Tier::A),
            make_engine(&kernel, &store, &config, Tier::B),
            make_engine(&kernel, &store, &config, Tier::C),
        ];
        BenchEnv {
            config,
            queries,
            engines,
        }
    })
}

#[derive(Clone, Copy)]
enum Tier {
    A,
    B,
    C,
}

/// 对一档引擎跑全部去重查询 N 轮，返回逐查询 latency 与总数。
/// Runs all deduplicated queries for N rounds through one engine; returns the
/// per-query latencies and the total count.
async fn collect_latencies(
    engine: &QueryEngine<MockVectorStore>,
    env: &BenchEnv,
    rounds: usize,
) -> (Vec<u64>, usize) {
    let mut lat = Vec::with_capacity(rounds * env.queries.len());
    for _ in 0..rounds {
        for q in &env.queries {
            let query = Query {
                text: q.query.clone(),
                filters: golden_filters_to_filters(&q.filters),
                top_k: 10,
                domain: Some(env.config.name.clone()),
            };
            let r = engine.search(&query).await.unwrap();
            lat.push(r.latency_ms);
        }
    }
    let total = lat.len();
    (lat, total)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 输出 P50/P95/P99 一行（供报告引用）。
/// Prints a P50/P95/P99 line (for the report).
fn print_quantiles(name: &str, lat: &[u64]) {
    let mut sorted = lat.to_vec();
    sorted.sort_unstable();
    let (p50, p95, p99) = (
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
    );
    println!(
        "QUANTILE {name}: p50={p50}ms p95={p95}ms p99={p99}ms n={}",
        lat.len()
    );
}

fn bench_all(c: &mut Criterion) {
    let env = build_env();
    let rt = tokio::runtime::Runtime::new().unwrap();

    for (tier_idx, tier_name, rounds) in [
        (0usize, "tier_a_fts", 5),
        (1usize, "tier_b_hybrid", 3),
        (2usize, "tier_c_qug", 2),
    ] {
        let engine = &env.engines[tier_idx];
        // criterion：平均耗时/吞吐（含整负载集）。
        let mut group = c.benchmark_group(tier_name);
        group.bench_function("golden_queries", |b| {
            b.iter(|| {
                rt.block_on(async {
                    for q in &env.queries {
                        let query = Query {
                            text: q.query.clone(),
                            filters: golden_filters_to_filters(&q.filters),
                            top_k: 10,
                            domain: Some(env.config.name.clone()),
                        };
                        let _ = engine.search(&query).await.unwrap();
                    }
                })
            })
        });
        group.finish();

        // 自采样分位数（供报告引用 P99 达标）。
        let (lat, _total) = rt.block_on(collect_latencies(engine, &env, rounds));
        print_quantiles(tier_name, &lat);
    }
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(std::time::Duration::from_secs(1));
    targets = bench_all
);
criterion_main!(benches);
