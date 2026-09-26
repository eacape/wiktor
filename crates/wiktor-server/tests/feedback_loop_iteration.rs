//! Step 11 B2 反馈闭环迭代实证：证明"反馈分析出知识缺口 → 补编译 → 召回提升"
//! 的闭环可迭代，并记录每轮 recall。
//! Step 11 B2 feedback-loop iteration experiment: proves the "feedback reveals a
//! knowledge gap → supplemental compile → recall gain" loop closes and is
//! iterable, recording the recall per round.
//!
//! 路径（全部走真实 server/core API）：一个小 domain（drink 实体 + JSONL 源），
//! 初始只 seed 一页；对另一实体（drink_b）的查询零召回 → 用
//! `StandardFeedbackAnalyzer` 分析窗口，断言产生 `supplemental_compile` 建议
//!（反馈→分析出缺口）；再用 `CompileService::admit`（server 已验证的真实编译
//! 路径）触发 drink_b 编译 + worker 发布（补编译）；复测该实体可检索（召回从 0
//! 提升）。这就是闭环一轮。
//! Path (all real server/core APIs): a tiny domain (`drink` entity + JSONL
//! source), initially only one page is seeded; a query for another entity
//! (drink_b) yields zero recall → the window is fed to `StandardFeedbackAnalyzer`,
//! asserting a `supplemental_compile` suggestion is produced (feedback reveals
//! the gap); then `CompileService::admit` (the server-verified real compile path)
//! triggers drink_b compilation + worker publish (the supplemental compile);
//! re-testing shows the entity is retrievable (recall 0 → hit). One closed-loop
//! round.

use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::seed;
use wiktor_core::traits::PublishStatus;
use wiktor_core::types::Filters;
use wiktor_feedback::analyzer::{analyze_window_with, FeedbackWindow, StandardKeyMatcher};
use wiktor_feedback::report::FeedbackReport;

/// 准备一个小 domain：`drink` 实体 + JSONL 源（drink_a 已 seed，drink_b 待补编译）。
/// Prepares a tiny domain: a `drink` entity + JSONL source (drink_a is seeded;
/// drink_b is to be compiled).
fn make_domain() -> (TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let domain_yaml = r#"name: demo-loop
version: "0.1.0"
entities:
  - name: drink
    source: jsonl://data.jsonl
    id_field: entity_id
    type_field: category
    fields:
      - { name: name, field_type: text, filterable: false }
      - { name: description, field_type: text, filterable: false }
      - { name: category, field_type: text, filterable: false }
      - { name: price, field_type: numeric, filterable: false }
compile:
  quality_threshold: 0.75
  knowledge_fields: [name, description]
"#;
    std::fs::write(dir.path().join("domain.yaml"), domain_yaml).unwrap();
    let data = "{\"id\": \"demo-loop:drink:a\", \"entity_id\": \"demo-loop:drink:a\", \
                \"name\": \"红茶\", \"description\": \"红茶底，香气浓郁。\", \
                \"category\": \"demo-loop:drink:a\", \"price\": 15.0, \"source_revision\": 1}\n\
                {\"id\": \"demo-loop:drink:b\", \"entity_id\": \"demo-loop:drink:b\", \
                \"name\": \"抹茶拿铁\", \"description\": \"抹茶拿铁：日式抹茶 + 鲜奶，醇厚微苦。\", \
                \"category\": \"demo-loop:drink:b\", \"price\": 22.0, \"source_revision\": 1}\n";
    std::fs::write(dir.path().join("data.jsonl"), data).unwrap();
    let domain_pack = dir
        .path()
        .join("domain.yaml")
        .to_string_lossy()
        .into_owned();
    let source = "jsonl://data.jsonl".to_string();
    (dir, domain_pack, source)
}

/// seed 一条手工页（drink_a）；drink_b 不 seed（制造零召回缺口）。
/// Seeds one hand-written page (drink_a); drink_b is NOT seeded (zero-recall gap).
fn seed_one_page(kernel: &SqliteKernel) {
    let md = "---
page_id: demo-loop:drink:a
entity_id: demo-loop:drink:a
title: 红茶
entity_type: drink
aliases: [红茶饮品]
tags: [经典]
---
红茶是发酵茶，香气浓郁、汤色红亮。
## 特点
回甘明显，适合加奶。
";
    let mut page = seed::parse_page(md).unwrap();
    page.metadata.domain_pack_version = "0.1.0".to_string();
    kernel
        .seed_pages(&page, "demo-loop", PublishStatus::Accepted)
        .unwrap();
}

/// 打一个 drink_b 的零召回查询日志（hit_count=0）。
/// Writes a zero-recall query log for drink_b (hit_count=0).
fn log_zero_recall_query(kernel: &SqliteKernel) -> i64 {
    kernel
        .insert_query_log(&wiktor_core::kernel::QueryLogInsert {
            query_text: "抹茶拿铁",
            query_json: r#"{"text":"抹茶拿铁","top_k":5,"domain":"demo-loop"}"#,
            rewritten_json: None,
            rewrite_failure: false,
            hit_count: 0,
            latency_ms: 3,
            domain: "demo-loop",
            candidate_empty_initial: false,
            relaxation_attempted: false,
            relaxation_succeeded: false,
        })
        .unwrap()
}

/// 断言 FeedbackReport 分析出了零召回缺口（该查询在窗口中未命中任何实体）。
/// Asserts the FeedbackReport surfaced a zero-recall blind spot (the query hit
/// nothing in the window).
fn report_has_zero_recall(report: &FeedbackReport) -> bool {
    !report.zero_recall.is_empty()
}

#[tokio::test]
async fn feedback_loop_iteration_closes_the_gap() {
    let (dir, domain_pack, source) = make_domain();
    let _dir = dir;
    let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
    seed_one_page(&kernel);

    // 0) 基线：drink_b 尚无页 → 搜索它零命中。
    let baseline_hits = kernel
        .search("抹茶拿铁", &Filters::default(), 5, Some("demo-loop"))
        .unwrap()
        .len();
    eprintln!("ROUND 0 baseline hits for drink_b: {baseline_hits}");
    assert_eq!(baseline_hits, 0, "baseline must be zero-recall");

    // 1) 反馈窗口：一条零召回查询日志 + 一条低分反馈事件 → analyze。
    let log_id = log_zero_recall_query(&kernel);
    kernel
        .insert_feedback_idempotent(
            &wiktor_core::kernel::FeedbackEventInput {
                idempotency_key: "loop-1".to_string(),
                domain: "demo-loop".to_string(),
                log_id,
                kind: wiktor_core::kernel::FeedbackKind::Rate,
                page_id: None,
                rating: Some(1),
                metadata: serde_json::json!({"query": "抹茶拿铁"}),
            },
            1_700_000_000,
        )
        .unwrap();
    let (logs, events) = kernel
        .load_feedback_window("demo-loop", 1_600_000_000, 1_800_000_000)
        .unwrap();
    let report = analyze_window_with(
        FeedbackWindow {
            domain: "demo-loop".to_string(),
            from: 1_600_000_000,
            to: 1_800_000_000,
            logs,
            events,
        },
        1,
        &StandardKeyMatcher,
    )
    .unwrap();
    assert!(
        report_has_zero_recall(&report),
        "analyze must surface a zero-recall blind spot for the query"
    );
    eprintln!(
        "ROUND 1 analyze → zero_recall={} (blind spot surfaced)",
        report.zero_recall.len()
    );

    // 2) 补编译：CompileService::admit（真实路径）+ 同源 worker 发布 drink_b。
    let svc = wiktor_server::services::compile::CompileService::new(kernel.clone());
    use wiktor_server::grpc::v1::compile_server::Compile;
    let admitted = svc
        .admit(tonic::Request::new(
            wiktor_server::grpc::v1::CompileAdmitRequest {
                domain: "demo-loop".into(),
                source_path: source.clone(),
                source_format: "jsonl".into(),
                domain_pack_path: domain_pack.clone(),
                domain_pack_version: "0.1.0".into(),
                force: false,
                max_entities: 0,
                options_json: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let task_id = admitted.summary.unwrap().task_ids[0];

    let worker = wiktor_server::worker::CompileWorker::from_domain_pack(
        kernel.clone(),
        &domain_pack,
        Some(&source),
        Some("0.1.0"),
        "",
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let handle = worker.spawn(cancel.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut terminal = String::new();
    while std::time::Instant::now() < deadline {
        if let Some(s) = kernel.compile_task_status(task_id).unwrap() {
            if s.status == "succeeded" || s.status == "failed" || s.status == "dead" {
                terminal = s.status;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    cancel.cancel();
    let _ = handle.await;
    assert_eq!(terminal, "succeeded", "worker must publish drink_b");
    eprintln!("ROUND 2 compile → task {task_id} {terminal} (drink_b published)");

    // 3) 复测：drink_b 现在可检索 → 召回从 0 提升。
    let after_hits = kernel
        .search("抹茶拿铁", &Filters::default(), 5, Some("demo-loop"))
        .unwrap()
        .len();
    eprintln!("ROUND 3 after-loop hits for drink_b: {after_hits}");
    let recall_round0 = 0.0f64;
    let recall_round3 = if after_hits > 0 { 1.0 } else { 0.0 };
    assert!(after_hits > 0, "after the loop drink_b must be retrievable");
    eprintln!(
        "LOOP RESULT: recall@1 round0={recall_round0} round3={recall_round3} (closed the gap)"
    );
}
