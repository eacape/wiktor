//! Step 2 golden-queries integration test (English + Chinese comments).
//! Step 2 golden-queries 集成测试。
//!
//! Runs the full loop against `examples/milk-tea`: seed pages + fact plane,
//! then executes every golden query and asserts the pass rate ≥ 80%.
//! 对 `examples/milk-tea` 跑完整闭环：导入页面与事实平面，执行全部 golden
//! 查询并断言通过率 ≥ 80%。
//!
//! Pass rule (Step 2 spec §6): hit set ∩ expected_hits must be non-empty.
//! 通过判定（Step 2 spec §6）：命中集合与 expected_hits 交集非空。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::seed;
use wiktor_core::traits::{DataSource, DomainConfig, EntityStore};
use wiktor_core::types::{Cursor, FilterCondition, Filters, PublishStatus};

/// Golden query record (from golden-queries.jsonl).
/// golden 查询记录（来自 golden-queries.jsonl）。
#[derive(Debug, Deserialize)]
struct Golden {
    query: String,
    expected_hits: Vec<String>,
    #[serde(default)]
    filters: GoldenFilters,
}

/// Flat filter representation; mapped to `FilterCondition` in the runner.
/// 扁平过滤表示；在 runner 中映射为 FilterCondition。
#[derive(Debug, Default, Deserialize)]
struct GoldenFilters {
    #[serde(default)]
    price_max: Option<f64>,
    #[serde(default)]
    price_min: Option<f64>,
    #[serde(default)]
    sugar_max: Option<f64>,
    #[serde(default)]
    sugar_min: Option<f64>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    ingredients: Vec<String>,
    #[serde(default)]
    exclude_ingredients: Vec<String>,
}

impl GoldenFilters {
    fn to_filters(&self) -> Filters {
        let mut conditions = Vec::new();
        if self.price_min.is_some() || self.price_max.is_some() {
            conditions.push(FilterCondition::NumericRange {
                field: "price".into(),
                min: self.price_min,
                max: self.price_max,
            });
        }
        if self.sugar_min.is_some() || self.sugar_max.is_some() {
            conditions.push(FilterCondition::NumericRange {
                field: "sugar_level".into(),
                min: self.sugar_min,
                max: self.sugar_max,
            });
        }
        if let Some(size) = &self.size {
            conditions.push(FilterCondition::TextEquals {
                field: "size".into(),
                value: size.clone(),
            });
        }
        if !self.ingredients.is_empty() {
            conditions.push(FilterCondition::RefContains {
                field: "ingredient_ids".into(),
                refs: self.ingredients.clone(),
            });
        }
        if !self.exclude_ingredients.is_empty() {
            conditions.push(FilterCondition::RefExcludes {
                field: "ingredient_ids".into(),
                refs: self.exclude_ingredients.clone(),
            });
        }
        Filters { conditions }
    }
}

/// Path to `examples/milk-tea` from the crate root.
/// 从 crate 根目录到 `examples/milk-tea` 的路径。
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("milk-tea")
}

/// Seed pages + fact plane into an in-memory kernel.
/// 向内存内核导入页面与事实平面。
async fn seed_all(kernel: &SqliteKernel, domain_dir: &Path) {
    // Parse domain.yaml into DomainConfig.
    // 解析 domain.yaml 为 DomainConfig。
    let yaml_text = std::fs::read_to_string(domain_dir.join("domain.yaml")).unwrap();
    let config: DomainConfig = serde_yaml_ng::from_str(&yaml_text).unwrap();

    // Seed pages from seed-wiki/*.md.
    // 从 seed-wiki/*.md 导入页面。
    let mut md_files: Vec<PathBuf> = std::fs::read_dir(domain_dir.join("seed-wiki"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "md").unwrap_or(false))
        .collect();
    md_files.sort();
    for path in &md_files {
        let content = std::fs::read_to_string(path).unwrap();
        let mut page = seed::parse_page(&content).unwrap();
        page.metadata.domain_pack_version = config.version.clone();
        kernel
            .seed_pages(&page, &config.name, PublishStatus::Accepted)
            .unwrap();
    }

    // fact plane from the first jsonl:// entity
    // 从第一个 jsonl:// 实体导入事实平面
    if let Some(entity) = config
        .entities
        .iter()
        .find(|e| e.source.starts_with("jsonl://"))
    {
        let source = JsonlDataSource::from_config(entity, domain_dir).unwrap();
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
                batch_size: wiktor_core::data::DEFAULT_BATCH_SIZE,
            });
        }
    }
}

#[tokio::test]
async fn golden_queries_pass_rate() {
    let domain_dir = examples_dir();
    let kernel = SqliteKernel::open_in_memory().unwrap();
    seed_all(&kernel, &domain_dir).await;

    // sanity: 20 pages, ≥100 SKU fact rows, ≥8 fact tables populated
    // 冒烟：20 页、≥100 SKU 事实行、事实表有数据
    let counts = kernel.row_counts().unwrap();
    assert_eq!(counts["pages"], 20, "expect 20 seed pages");
    assert!(
        counts["facts"] >= 100 * 8,
        "expect ≥100 SKUs × 8 fields, got {}",
        counts["facts"]
    );
    assert!(counts["fact_refs"] > 0);

    // load goldens
    // 加载 golden
    let goldens: Vec<Golden> = std::fs::read_to_string(domain_dir.join("golden-queries.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        goldens.len() >= 20,
        "need ≥20 goldens, got {}",
        goldens.len()
    );

    let mut passed = 0usize;
    let mut failed: Vec<(&str, Vec<String>)> = Vec::new();
    for g in &goldens {
        let filters = g.filters.to_filters();
        let hits = kernel.search(&g.query, &filters, 10, None).unwrap();
        let hit_ids: HashSet<String> = hits.iter().map(|h| h.entity_id.to_key()).collect();
        let expected: HashSet<&String> = g.expected_hits.iter().collect();
        if expected.iter().any(|e| hit_ids.contains(*e)) {
            passed += 1;
        } else {
            failed.push((&g.query, hit_ids.into_iter().collect()));
        }
    }

    let rate = passed as f64 / goldens.len() as f64;
    assert!(
        rate >= 0.8,
        "golden pass rate {rate:.2} < 0.8 ({passed}/{}) — failed: {failed:?}",
        goldens.len()
    );
    eprintln!("golden pass rate: {rate:.2} ({passed}/{})", goldens.len());
}

#[tokio::test]
async fn seed_is_idempotent() {
    let domain_dir = examples_dir();
    let kernel = SqliteKernel::open_in_memory().unwrap();
    seed_all(&kernel, &domain_dir).await;
    let counts1 = kernel.row_counts().unwrap();
    seed_all(&kernel, &domain_dir).await;
    let counts2 = kernel.row_counts().unwrap();
    assert_eq!(
        counts1["pages"], counts2["pages"],
        "pages must not double on reseed"
    );
    assert_eq!(
        counts1["facts"], counts2["facts"],
        "facts must not double on reseed"
    );
    assert_eq!(counts1["page_sections"], counts2["page_sections"]);
}
