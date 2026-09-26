//! Step 7 gRPC 服务面（spec step7 §3 D1/D12）：proto 生成代码引入 + 六 service
//! 注册装配。认证经 tonic interceptor（B2 接入 [`crate::auth`]）；metrics 计数
//! gRPC 请求（D12）。
//! Step 7 gRPC service surface (spec step7 §3 D1/D12): the proto-generated code
//! plus six-service registration assembly. Authentication comes via a tonic
//! interceptor (wired in B2 through [`crate::auth`]); metrics count gRPC
//! requests (D12).

/// proto 生成代码（`wiktor.v1` 包 → `wiktor::v1` 模块树）。
/// The proto-generated code (package `wiktor.v1` → the `wiktor::v1` module tree).
pub mod v1 {
    tonic::include_proto!("wiktor.v1");
}

/// 把六个 service 注册到 tonic Router，每个 service 绑定其方法权限名的
/// interceptor（Step7 §3.1 方法权限映射 + D3 同源语义）。
/// Registers the six services on a tonic Router, each bound to its method
/// permission's interceptor (Step7 §3.1 method-permission mapping + D3 shared
/// semantics).
pub fn router(
    keys: crate::state::ApiKeys,
    search: crate::services::search::SearchService<dyn wiktor_core::traits::VectorStore>,
    compile: crate::services::compile::CompileService,
    qug: crate::services::qug_build::QugBuildService,
    review: crate::services::review::ReviewService,
    compatibility: crate::services::compatibility::CompatibilityService,
    status: crate::services::status::StatusService,
) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .add_service(v1::search_server::SearchServer::with_interceptor(
            search,
            crate::auth::grpc_interceptor(keys.clone(), "search"),
        ))
        .add_service(v1::compile_server::CompileServer::with_interceptor(
            compile,
            crate::auth::grpc_interceptor(keys.clone(), "compile"),
        ))
        .add_service(v1::qug_build_server::QugBuildServer::with_interceptor(
            qug,
            crate::auth::grpc_interceptor(keys.clone(), "qug_build"),
        ))
        .add_service(v1::review_server::ReviewServer::with_interceptor(
            review,
            crate::auth::grpc_interceptor(keys.clone(), "review"),
        ))
        .add_service(
            v1::compatibility_server::CompatibilityServer::with_interceptor(
                compatibility,
                crate::auth::grpc_interceptor(keys.clone(), "compatibility"),
            ),
        )
        .add_service(v1::status_server::StatusServer::with_interceptor(
            status,
            crate::auth::grpc_interceptor(keys, "status"),
        ))
}

#[cfg(test)]
mod tests {
    use super::v1;
    use std::sync::Arc;
    use wiktor_core::{MockVectorStore, SqliteKernel};

    // A1：proto 生成代码可用，六 service server 类型存在（字段号稳定由 proto
    // 文件本身保证，任何字段号复用都会在 tonic-prost-build 报错）。
    // A1: the proto-generated code is usable and all six service server types
    // exist (field-number stability is guaranteed by the proto file itself —
    // any field-number reuse fails at tonic-prost-build time).
    #[test]
    fn generated_six_services_compile() {
        let _ = v1::SearchRequest {
            domain: "milk-tea".into(),
            text: "珍珠奶茶".into(),
            filters_json: String::new(),
            top_k: 5,
        };
        let _ = v1::compile_server::CompileServer::new(
            crate::services::compile::CompileService::default(),
        );
        let _ =
            v1::review_server::ReviewServer::new(crate::services::review::ReviewService::default());
    }

    // B3：Status 服务在内存 kernel 上返回 schema 版本 + 行数（A17 形状）。
    // B3: the Status service returns the schema version + row counts over an
    // in-memory kernel (the A17 shape).
    #[tokio::test]
    async fn status_service_returns_schema_and_rows() {
        use crate::grpc::v1::status_server::Status;
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let svc = crate::services::status::StatusService::new(kernel);
        let resp = svc
            .get(tonic::Request::new(v1::StatusRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.healthy);
        assert!(!resp.schema_version.is_empty());
        assert!(resp.row_counts.contains_key("pages"));
    }

    // B3：Search 服务在 Mock 向量 + 确定性嵌入器下可装配可调用（QUG 缺席时
    // 返回成功且 review.compute rewrite_failure 语义由引擎保证）。
    // B3: the Search service assembles and runs under the Mock vector store +
    // the deterministic embedder (with QUG absent, it succeeds and the
    // rewrite_failure semantics are the engine's).
    #[tokio::test]
    async fn search_service_runs_on_mock_engine() {
        use crate::grpc::v1::search_server::Search;
        use wiktor_core::embedding::deterministic::DeterministicEmbedder;
        use wiktor_core::query_engine::QueryEngine;
        use wiktor_core::traits::{DistanceMetric, VectorStore};

        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let store = Arc::new(MockVectorStore::new());
        store
            .ensure_collection("milk-tea", 768, DistanceMetric::Cosine)
            .await
            .unwrap();
        let engine = QueryEngine::new(
            kernel,
            store,
            None,
            Arc::new(DeterministicEmbedder::new(768)),
            "milk-tea",
            5,
            60,
        )
        .unwrap();
        let mut engines = std::collections::HashMap::new();
        engines.insert("milk-tea".to_string(), Arc::new(engine));
        let svc = crate::services::search::SearchService::new(engines, 16 * 1024);
        let resp = svc
            .search(tonic::Request::new(v1::SearchRequest {
                domain: "milk-tea".into(),
                text: "珍珠奶茶".into(),
                filters_json: String::new(),
                top_k: 5,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.hits.is_empty(), "empty store → no hits");
        assert!(
            resp.log_id > 0,
            "a successful search writes a query-log row (log_id={})",
            resp.log_id
        );
    }

    // B3：Step14 P4 —— 单进程多领域：一个引擎表同时服务两个 domain，请求按
    // `domain` 分发到各自引擎；未装配的域返回 NOT_FOUND（区别于认证 403）。
    // B3: Step14 P4 — single-process multi-domain: one engine map serves two
    // domains, dispatching each request to its own engine; an unwired domain →
    // NOT_FOUND (distinct from the auth 403).
    #[tokio::test]
    async fn search_dispatches_by_domain_in_multi_domain_map() {
        use crate::grpc::v1::search_server::Search;
        use wiktor_core::embedding::deterministic::DeterministicEmbedder;
        use wiktor_core::query_engine::QueryEngine;
        use wiktor_core::traits::{DistanceMetric, VectorStore};

        let store = Arc::new(MockVectorStore::new());
        let mut engines = std::collections::HashMap::new();
        for domain in ["milk-tea", "coffee"] {
            store
                .ensure_collection(domain, 768, DistanceMetric::Cosine)
                .await
                .unwrap();
            let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
            let engine = QueryEngine::new(
                kernel,
                store.clone(),
                None,
                Arc::new(DeterministicEmbedder::new(768)),
                domain,
                5,
                60,
            )
            .unwrap();
            engines.insert(domain.to_string(), Arc::new(engine));
        }
        let svc = crate::services::search::SearchService::new(engines, 16 * 1024);

        // 已装配域：各自成功返回（空 store → 无命中，但仍写 query-log，log_id>0）。
        // Wired domains: each succeeds over the empty store (no hits but a
        // query-log row is written → log_id>0).
        for domain in ["milk-tea", "coffee"] {
            let resp = svc
                .search(tonic::Request::new(v1::SearchRequest {
                    domain: domain.into(),
                    text: "招牌".into(),
                    filters_json: String::new(),
                    top_k: 5,
                }))
                .await
                .unwrap()
                .into_inner();
            assert!(resp.hits.is_empty());
            assert!(resp.log_id > 0, "domain {domain} search wrote a log row");
        }

        // 未装配域：NOT_FOUND，明确区别于认证失败（403/Unauthenticated）。
        // Unwired domain: NOT_FOUND (distinct from the auth 403/Unauthenticated).
        let err = svc
            .search(tonic::Request::new(v1::SearchRequest {
                domain: "bubble-tea".into(),
                text: "招牌".into(),
                filters_json: String::new(),
                top_k: 5,
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::NotFound,
            "unwired domain → NotFound"
        );
        assert!(
            err.message().contains("domain not served"),
            "diagnostic message: {}",
            err.message()
        );
    }

    // B4：Compile.Admit 只写任务不调模型；Compile.Status 返回 pending 快照。
    // 用临时目录里的单实体 domain.yaml + jsonl（离线，无网络）。
    // B4: Compile.Admit only writes tasks and never calls a model; Compile.Status
    // returns the pending snapshot. Uses a temp-dir single-entity domain.yaml +
    // jsonl (offline, no network).
    #[tokio::test]
    async fn compile_admit_and_status_roundtrip() {
        use crate::grpc::v1::compile_server::Compile;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("domain.yaml"),
            r#"name: milk-tea
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
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("data.jsonl"),
            "{\"entity_id\": \"milk-tea:drink:test\", \"name\": \"珍珠奶茶\", \
             \"description\": \"红茶底配 Q 弹珍珠。\", \"category\": \"milk-tea:drink:test\", \
             \"price\": 19.0, \"source_revision\": 1}\n",
        )
        .unwrap();
        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let svc = crate::services::compile::CompileService::new(kernel.clone());
        let domain_pack = dir
            .path()
            .join("domain.yaml")
            .to_string_lossy()
            .into_owned();
        let resp = svc
            .admit(tonic::Request::new(v1::CompileAdmitRequest {
                domain: "milk-tea".into(),
                source_path: "jsonl://data.jsonl".into(),
                source_format: "jsonl".into(),
                domain_pack_path: domain_pack,
                domain_pack_version: "0.1.0".into(),
                force: false,
                max_entities: 0,
                options_json: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        let summary = resp.summary.unwrap();
        assert_eq!(summary.scanned, 1);
        assert_eq!(summary.admitted, 1);
        assert_eq!(summary.task_ids.len(), 1);
        assert!(!summary.run_id.is_empty());
        let task_id = summary.task_ids[0];
        let st = svc
            .status(tonic::Request::new(v1::CompileStatusRequest {
                task_id,
                domain: "milk-tea".into(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(st.task_id, task_id);
        assert_eq!(st.entity_id, "milk-tea:drink:test");
        assert_eq!(st.status, "pending");
        assert_eq!(st.attempt_count, 0);
        // 不存在的任务 → NOT_FOUND。
        // An unknown task → NOT_FOUND.
        let not_found = svc
            .status(tonic::Request::new(v1::CompileStatusRequest {
                task_id: 99_999,
                domain: String::new(),
            }))
            .await;
        assert_eq!(not_found.unwrap_err().code(), tonic::Code::NotFound);
    }

    // B4：worker 消费 pending 任务并发布（MockCompiler 离线；轮询至终态）。
    // B4: the worker consumes pending tasks and publishes (MockCompiler offline;
    // polls until terminal).
    #[tokio::test]
    async fn compile_worker_consumes_and_publishes() {
        use crate::grpc::v1::compile_server::Compile;

        let dir = tempfile::tempdir().unwrap();
        let domain_yaml = r#"name: milk-tea
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
        std::fs::write(
            dir.path().join("data.jsonl"),
            "{\"entity_id\": \"milk-tea:drink:test\", \"name\": \"珍珠奶茶\", \
             \"description\": \"红茶底配 Q 弹珍珠。\", \"category\": \"milk-tea:drink:test\", \
             \"price\": 19.0, \"source_revision\": 1}\n",
        )
        .unwrap();

        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let svc = crate::services::compile::CompileService::new(kernel.clone());
        let domain_pack = dir
            .path()
            .join("domain.yaml")
            .to_string_lossy()
            .into_owned();
        let admitted = svc
            .admit(tonic::Request::new(v1::CompileAdmitRequest {
                domain: "milk-tea".into(),
                source_path: "jsonl://data.jsonl".into(),
                source_format: "jsonl".into(),
                domain_pack_path: domain_pack.clone(),
                domain_pack_version: "0.1.0".into(),
                force: false,
                max_entities: 0,
                options_json: String::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        let task_id = admitted.summary.unwrap().task_ids[0];

        // 与 Admit 同源装配 worker（STEP7-002：同一 domain.yaml → 同一 policy/
        // schema → content_hash 一致，publish fencing 通过）。
        // Assemble the worker from the same source as Admit (STEP7-002: same
        // domain.yaml → same policy/schema → content_hash matches, publish
        // fencing passes).
        let worker = crate::worker::CompileWorker::from_domain_pack(
            kernel.clone(),
            &domain_pack,
            Some("jsonl://data.jsonl"),
            Some("0.1.0"),
            "",
        )
        .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let handle = worker.spawn(cancel.clone());

        // 轮询 Status 直到终态（非 pending/running），最多 10s。
        // Poll Status until terminal (not pending/running), up to 10s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut terminal: Option<String> = None;
        while std::time::Instant::now() < deadline {
            let st = svc
                .status(tonic::Request::new(v1::CompileStatusRequest {
                    task_id,
                    domain: String::new(),
                }))
                .await
                .unwrap()
                .into_inner();
            if st.status == "succeeded" || st.status == "failed" || st.status == "dead" {
                terminal = Some(st.status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        cancel.cancel();
        let _ = handle.await;
        assert_eq!(
            terminal.as_deref(),
            Some("succeeded"),
            "MockCompiler must publish"
        );
        // 发布后该实体的 accepted 页存在（worker 走 publish_compile 的真实事务）。
        // After publish, the entity's accepted page exists (the worker ran the
        // real publish_compile transaction).
        let pages = kernel.list_published_pages("milk-tea").unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].0, "milk-tea:drink:test");
    }

    // B5：Review.List/Ignore 往返；reviewer 来自认证 label，不由客户端传入。
    // B5: Review.List/Ignore roundtrip; the reviewer comes from the auth label,
    // never from the client.
    #[tokio::test]
    async fn review_list_and_ignore_roundtrip() {
        use crate::auth::AuthedKey;
        use crate::grpc::v1::review_server::Review;
        use std::collections::BTreeSet;
        use wiktor_core::kernel::ReviewSuggestionInput;

        let kernel = Arc::new(SqliteKernel::open_in_memory().unwrap());
        let ids = kernel
            .insert_review_suggestions(
                "milk-tea",
                &[ReviewSuggestionInput {
                    action: "ignore".into(),
                    source_log_ids_json: "[1]".into(),
                    subject_json: r#"{"normalized_query":"boba"}"#.into(),
                    reason_json: r#"{"signal":"zero_recall"}"#.into(),
                    created_at: 1000,
                }],
            )
            .unwrap();
        let svc = crate::services::review::ReviewService::new(kernel);
        let listed = svc
            .list(tonic::Request::new(v1::ReviewListRequest {
                domain: "milk-tea".into(),
                status: "pending".into(),
                limit: 10,
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.items.len(), 1);
        let mut request = tonic::Request::new(v1::ReviewDecisionRequest {
            review_id: ids[0],
            domain: "milk-tea".into(),
        });
        request.extensions_mut().insert(AuthedKey {
            domain: "milk-tea".into(),
            label: "reviewer-label".into(),
            methods: BTreeSet::from(["review".to_string()]),
        });
        let ignored = svc.ignore(request).await.unwrap().into_inner();
        let item = ignored.item.unwrap();
        assert_eq!(item.status, "ignored");
        assert_eq!(item.reviewed_by, "reviewer-label");
    }
}
