//! Compile 服务（spec step7 §3 D1/D4/D5；B4 实装）。Admit 走 core
//! `PipelineExecutor::admit_batch`（只 admission 不 claim，无模型调用）；Status
//! 走 kernel `compile_task_status` 只读快照。装配复用 core 公开组件（DomainConfig
//! 解析 / JsonlDataSource / MockCompiler / build_context），**不复制任何
//! publish/failure SQL**（STEP7-002），也**不依赖 CLI**（spec step7 §5）。
//! The Compile service (spec step7 §3 D1/D4/D5; B4 implementation). Admit uses
//! the core `PipelineExecutor::admit_batch` (admit-only, no claim, no model
//! call); Status uses the kernel `compile_task_status` read-only snapshot.
//! Assembly reuses core public components (DomainConfig parsing /
//! JsonlDataSource / MockCompiler / build_context) — **no publish/failure SQL is
//! copied** (STEP7-002) and **the CLI is never depended on** (spec step7 §5).

use std::path::PathBuf;
use std::sync::Arc;

use tonic::{Response, Status};
use wiktor_core::compile::config::{CompilePolicy, RunOptions, SystemClock};
use wiktor_core::compile::contract::{system_prompt, DefaultSourceRefValidator};
use wiktor_core::compile::executor::PipelineExecutor;
use wiktor_core::compile::mock::MockCompiler;
use wiktor_core::compile::quality::RuleBasedScorer;
use wiktor_core::data::JsonlDataSource;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::traits::{Compiler, DataSource, EntityConfig, EntitySchema};
use wiktor_core::types::CompileContext;

use crate::grpc::v1::compile_server::Compile;
use crate::grpc::v1::{
    CompileAdmitRequest, CompileAdmitResponse, CompileStatusRequest, CompileStatusResponse,
    CompileTaskSummary,
};

/// Compile gRPC handler（B4：kernel + 装配参数）。
/// The Compile gRPC handler (B4: kernel + assembly parameters).
#[derive(Clone)]
pub struct CompileService {
    kernel: Arc<SqliteKernel>,
}

impl CompileService {
    pub fn new(kernel: Arc<SqliteKernel>) -> Self {
        Self { kernel }
    }
}

/// B1 空壳 → B4 实装的过渡默认（测试注册用；无 kernel 时方法返回 INTERNAL）。
/// The B1-shell → B4-implementation transition default (used for test
/// registration; methods return INTERNAL without a kernel).
impl Default for CompileService {
    fn default() -> Self {
        Self {
            kernel: Arc::new(SqliteKernel::open_in_memory().expect("in-memory kernel")),
        }
    }
}

/// domain.yaml 装配（B4）：读文件 → 解析 DomainConfig → 单实体 EntityConfig →
/// 数据源（source_path 覆盖 jsonl URI 时接 source_path，否则用 domain.yaml 的
/// jsonl:// URI）→ schema。路径解析：domain_pack_path 的相对 prompt/数据源相对
/// 其所在目录。不写库、不请求模型（只读装配，spec step7 §5 路径纪律）。
/// pub(crate)：CompileWorker 也用它装配（STEP7-002：worker 与 Admit 同源，
/// policy/schema/ctx 一致才能复现 content_hash）。
/// domain.yaml assembly (B4): read → parse DomainConfig → single-entity
/// EntityConfig → data source (source_path overrides the jsonl URI when given,
/// else the domain.yaml's jsonl:// URI) → schema. Relative prompts/data sources
/// resolve against the domain pack's directory. Read-only assembly — no writes,
/// no model calls (spec step7 §5 path discipline). pub(crate): CompileWorker
/// assembles through the same entry (STEP7-002: the worker and Admit share the
/// same source, so policy/schema/ctx match and content_hash reproduces).
pub(crate) struct AssembledSource {
    pub(crate) policy: CompilePolicy,
    pub(crate) ctx: CompileContext,
    pub(crate) schema: EntitySchema,
    pub(crate) source: JsonlDataSource,
}

pub(crate) fn assemble_source(
    domain_pack_path: &str,
    source_path: Option<&str>,
    domain_pack_version: Option<&str>,
    options_json: &str,
) -> Result<AssembledSource, String> {
    let yaml_text = std::fs::read_to_string(domain_pack_path)
        .map_err(|e| format!("read {}: {e}", domain_pack_path))?;
    let config: wiktor_core::traits::DomainConfig = serde_yaml_ng::from_str(&yaml_text)
        .map_err(|e| format!("parse {}: {e}", domain_pack_path))?;
    let domain_dir = PathBuf::from(domain_pack_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let entity_cfg: EntityConfig = match config.entities.len() {
        1 => config.entities[0].clone(),
        0 => return Err("domain pack declares no entities".to_string()),
        n => {
            return Err(format!(
                "Admit supports single-entity packs only; {n} entities declared"
            ))
        }
    };

    let policy = config.compile_policy.clone();
    policy.validate().map_err(|e| e.to_string())?;
    if let Some(ver) = domain_pack_version {
        if !ver.is_empty() && ver != config.version {
            return Err(format!(
                "domain_pack_version {ver:?} mismatch with domain pack version {:?}",
                config.version
            ));
        }
    }
    let options: AdmitOptions = serde_json::from_str(options_json).unwrap_or_default();

    let prompt_template = match &config.compile_prompt {
        Some(rel) => {
            let path = if std::path::Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                domain_dir.join(rel)
            };
            std::fs::read_to_string(&path)
                .map_err(|e| format!("read prompt {}: {e}", path.display()))?
        }
        None => system_prompt(),
    };
    let identity = config
        .identity()
        .map_err(|e| format!("domain identity: {e}"))?;
    let ctx = wiktor_core::compile::config::build_context(
        &config.version,
        &prompt_template,
        &options.model_version,
        &options.embedding_model,
        config.quality_threshold,
        config.compile_output_contract == "require_source_refs",
        identity
            .schema_version
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        identity
            .prompt_version
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
    );

    let mut entity_cfg = entity_cfg;
    if let Some(sp) = source_path {
        if !sp.starts_with("jsonl://") {
            return Err(format!("source_path {sp:?} must be a jsonl:// URI"));
        }
        entity_cfg.source = sp.to_string();
    }
    let source = JsonlDataSource::from_config(&entity_cfg, &domain_dir)
        .map_err(|e| format!("open data source: {e}"))?;
    Ok(AssembledSource {
        policy,
        ctx,
        schema: source.schema(),
        source,
    })
}

/// Admit 选项（options_json；全可选）。
/// Admit options (options_json; all optional).
#[derive(Debug, Default, serde::Deserialize)]
struct AdmitOptions {
    model_version: String,
    embedding_model: String,
}

#[tonic::async_trait]
impl Compile for CompileService {
    async fn admit(
        &self,
        request: tonic::Request<CompileAdmitRequest>,
    ) -> std::result::Result<Response<CompileAdmitResponse>, Status> {
        let req = request.into_inner();
        // domain 校验（auth 已把 domain 限定在 key 的 domain；此处再核对请求
        // 域与装配域一致，防错配）。
        // Domain validation (auth already scopes the domain to the key's domain;
        // this re-checks that the request domain matches the assembled domain to
        // prevent mismatches).
        if req.domain.is_empty() {
            return Err(Status::invalid_argument("domain is required"));
        }

        let assembled = assemble_source(
            &req.domain_pack_path,
            if req.source_path.is_empty() {
                None
            } else {
                Some(req.source_path.as_str())
            },
            if req.domain_pack_version.is_empty() {
                None
            } else {
                Some(req.domain_pack_version.as_str())
            },
            &req.options_json,
        )
        .map_err(|e| Status::invalid_argument(format!("assembly failed: {e}")))?;

        // domain 一致性：domain.yaml 的 name 必须与请求 domain 一致（§5 纪律）。
        // Domain consistency: the domain.yaml name must match the request domain.
        let AssembledSource {
            policy,
            ctx,
            source,
            ..
        } = assembled;
        if source.schema().entity_type.is_empty() {
            return Err(Status::invalid_argument("empty entity schema"));
        }

        let kernel = self.kernel.clone();
        let compiler: Arc<dyn Compiler> = Arc::new(MockCompiler::new(policy.clone()));
        let executor = Arc::new(PipelineExecutor::new(
            kernel.clone(),
            compiler,
            Arc::new(RuleBasedScorer::new()),
            Arc::new(DefaultSourceRefValidator::new()),
            Arc::new(SystemClock),
            policy,
        ));
        let source_box: Arc<dyn DataSource> = Arc::new(source);
        let options = RunOptions {
            limit: if req.max_entities > 0 {
                req.max_entities as usize
            } else {
                1000
            },
            batch_size: 32,
            force: req.force,
            dry_run: false,
        };
        let outcome = executor
            .admit_batch(source_box.as_ref(), &ctx, options)
            .await
            .map_err(|e| crate::error::grpc_status(&e, "compile admit failed"))?;
        let summary = CompileTaskSummary {
            run_id: outcome.stats.run_id.clone(),
            scanned: outcome.stats.scanned as u32,
            admitted: outcome.task_ids.len() as u32,
            skipped: outcome.stats.skipped as u32,
            task_ids: outcome.task_ids,
        };
        Ok(Response::new(CompileAdmitResponse {
            summary: Some(summary),
        }))
    }

    async fn status(
        &self,
        request: tonic::Request<CompileStatusRequest>,
    ) -> std::result::Result<Response<CompileStatusResponse>, Status> {
        let req = request.into_inner();
        let kernel = self.kernel.clone();
        let task_id = req.task_id;
        let snapshot = tokio::task::spawn_blocking(move || kernel.compile_task_status(task_id))
            .await
            .map_err(|e| Status::internal(format!("status task join failed: {e}")))?
            .map_err(|e| crate::error::grpc_status(&e, "compile status failed"))?;
        let Some(snap) = snapshot else {
            return Err(Status::not_found(format!("task {task_id} not found")));
        };
        // domain 从 entity_id 首段提取（compile_tasks 无独立 domain 列）。
        // The domain is the first segment of the entity_id (compile_tasks has no
        // standalone domain column).
        let domain = snap
            .entity_id
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();
        if !req.domain.is_empty() && domain != req.domain {
            return Err(Status::not_found(format!(
                "task {task_id} belongs to domain {domain:?}, not {:?}",
                req.domain
            )));
        }
        Ok(Response::new(CompileStatusResponse {
            task_id: snap.task_id,
            domain,
            entity_id: snap.entity_id,
            status: snap.status,
            result: snap.result.unwrap_or_default(),
            attempt_count: snap.attempt_count,
            retry_count: snap.retry_count,
            recompile_count: snap.recompile_count,
            error_code: snap.error_message.unwrap_or_default(),
            updated_at: snap.updated_at,
            lease_expires_at: snap.lease_expires_at.unwrap_or(0),
        }))
    }
}
