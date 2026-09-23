# Step 8 设计规范：一致性仲裁维度与编译任务状态机补全

> 版本：v1.0（2026-09-23）  
> 上承接：Step 4 `step4-compile-pipeline.md`、Step 6 `step6-feedback-loop.md`  
> 实现对象：`wiktor-builder`；独立验收对象：`test-engineer`  
> 中文为权威设计；英文逐节对应于 `step8-consistency-state-machine.en.md`。

## 1. 目标与非目标

Step 8 落地 MASTER-PLAN §5.1/§5.5 的两个缺口：把一致性从 `NULL` 扩展为可插拔、可离线验证的 source-ref 证据比较；把现有任务状态机补成可持续运行的租约回收、死信审核和领域包兼容检查闭环。

目标：

- `page_quality.consistency` 支持 `NULL` 或 `[0,1]`；默认实现只比较明确的 source-ref 证据，不进行自由语义推断。
- 一致性检查只消费有界的相关页 top-k，不做全量页面两两比对；检测到冲突的候选不会发布。
- 任务终态 `dead/quarantined`、租约过期回收、页级刹车终态、兼容失败都能进入可审计人工审核队列。
- 单 worker 运行期间周期性回收过期租约；每个模型等待段有心跳续租，失去 fencing 后不得写状态。
- 领域包 semver、Schema/Prompt 版本和 `artifact_version` 在升级前做全量兼容 preflight；不兼容时拒绝新编译并产生兼容审核告警。
- 迁移 0006 保持 0001–0005 数据可读，既有 `review_queue` 的反馈动作和 `UNIQUE(domain,action,subject_json)` 语义继续有效。

非目标：

- 不引入 gRPC/tonic；不改 Step 6 的 axum HTTP 面。
- 不实现一致性 LLM 仲裁。未来 LLM 只能实现同一 trait，不能绕过证据、top-k、预算和审核契约。
- 不重写 Step 4 已交付的 admission、epoch fence、指数退避、预算预留或发布事务。
- 不建立全量一致性矩阵、跨页自然语言推断或自动修改领域包。
- 不引入新的独立审核 UI、分布式 worker 协议或第二数据库连接栈。

## 2. 术语与现状约束

- **候选页**：当前即将发布的 `CompiledPage`。
- **相关页**：由 `ConsistencyCandidateProvider` 返回、数量不超过策略 `top_k` 的已发布 accepted 页。候选页自身不重复计入。
- **可比较证据**：两条 source-ref 在领域、指针语法和声明的比较键上可确定地对应，且值可以规范化为相同类型；没有可比较证据时结果为 `None`。
- **冲突/分歧**：同一明确比较键对应的源证据值不同，或候选页引用值与当前事实平面同一键的值不同。仅文字相似或不同标题不构成冲突。
- **死信**：已达到质量重编译上限、传输重试上限、租约重试上限或其它不可自动恢复终态的编译任务。SQL 仍使用 `dead`，业务结果区分 `failed` 与 `quarantined`。
- **兼容 preflight**：在 admission 前读取当前数据库中所有相关 artifact/dependency 快照，验证当前领域包的 semver 兼容范围、Schema/Prompt 版本和 artifact 版本。

本规范固定以下已确认事实，不得重新解释：

- **X1**：`review_queue.action` 当前只有 `supplemental_compile/query_template/ignore`；0006 必须放宽 CHECK。新死信按 `task_id` 去重，不能依赖 subject 文本偶然相同。
- **X2**：当前 `page_quality.consistency` 恒为 `NULL`；四维和 overall 已落库。
- **X3**：`compile_store.rs` 页级刹车到顶的注释承诺“进入人工队列”，实际未入队，本步必须兑现。
- **X4**：Step 4 已交付 claim/recover/heartbeat/fencing/backoff/superseded；本步只补真实缺口。当前 executor 仅在一次真实 run 开头调用 `recover_compile_leases`，没有周期后台回收；`process_lease` 也没有在模型等待期间续租。
- **X5**：MVP 排除一致性 LLM 仲裁。默认实现必须基于既有 source-ref 和源证据，禁止自由语义推断；trait 必须允许未来替换 LLM 实现而不改核心状态机。
- **X6**：兼容检查包括领域包 semver、Schema/Prompt 兼容性和 artifact_version；新 hash 已由 Step 4 全依赖 BLAKE3 自然触发重编译。
- **X7**：每个决策点都给出实现批次和离线验收判据；验收无 LLM、无网络、无外部服务。

## 3. 决策 D1–D12

| ID | 拍板决策 | 理由与边界 | 实现批次 | 验收 |
|---|---|---|---|---|
| D1 | 新增 `ConsistencyArbiter` trait，默认 `SourceRefConsistencyArbiter`；`RuleScorer` 在 scorer 之后接收一致性结果 | 核心只依赖抽象；默认确定性、可离线。未来 LLM 仅替换实现 | B2 | A3–A7 |
| D2 | 相关页通过有界 `ConsistencyCandidateProvider` 提供，默认 SQLite FTS/候选提供器只取 `top_k`，`top_k` 默认 8、上限 32 | 满足“嵌入召回 top-k、禁止全量比对”的成本边界；没有候选或无重叠证据返回 `NULL` | B2 | A4、A8 |
| D3 | 比较键只允许领域 Schema/配置显式声明的 source-ref pointer；默认键为 `(entity_id,pointer)`，值用 canonical JSON 精确比较 | 不把 `name`、别名或自然语言相似度当事实；同一页不同 revision 的证据可检测漂移 | B1/B2 | A5 |
| D4 | `consistency=None` 不降低四维 overall；存在可比较证据时 overall 为五维等权平均，且 consistency 必须达到 `min_consistency`（默认 1.0）才能 accepted | 保留旧 seed/无候选兼容性，同时任何明确冲突都阻止发布 | B2 | A6 |
| D5 | 一致性冲突作为质量候选失败，消耗 `recompile_count`；达到刹车时 `dead/quarantined`，同一事务插入 `compile_dead_letter` 和 `consistency_conflict` 诊断动作各一条 | 不自动接受矛盾；不另造 compile 状态；审核可查看证据 | B3 | A9 |
| D6 | 复用 `review_queue`；新增动作 `compile_dead_letter`、`consistency_conflict`、`compatibility_conflict` | Step 6 已有审核生命周期、审批人和事务模式；避免第二审核表 | B3 | A10 |
| D7 | `compile_dead_letter` 的 `subject_json` 固定为 `{"task_id":N}`，同一 domain/task 只允许一条；`compile_task_id` 必须回填 | 使死信按 task_id 去重，不依赖 reason 文本；重复回收/重启不重复告警 | B3 | A10 |
| D8 | 回收器在每次 run 开始仍执行一次，并新增 `LeaseReaper` 周期循环；默认间隔 30 秒，关闭时 drain 一次 | 单 worker 也必须可处理无人接管的 running；不依赖 HTTP/gRPC | B4 | A11 |
| D9 | executor 为每个 lease 启动心跳任务，周期默认为 `lease_seconds/2`，最小 1 秒；模型请求返回后先停止并 join，再发布 | 租约窗口 300 秒时默认 150 秒续租；不持锁跨 await；心跳失败只标记 stale，发布 CAS 最终裁决 | B4 | A12 |
| D10 | 兼容矩阵放在 `domain.yaml` 的 `compatibility` 段，随 `dependencies_json` 快照持久化；不建独立配置表 | 配置是领域包权威来源，数据库只保存编译时快照，避免矩阵双写漂移 | B1/B5 | A13–A15 |
| D11 | `wiktor domain check` 做只读全量 preflight；真实 `compile` 在首次 admission 前自动运行同一 preflight；不兼容时退出 3、拒绝 admission，并在 compile 事务中幂等插入 `compatibility_conflict` | CLI 可审计，编译 fail-closed；hash 变化负责后续重编译，不把兼容告警当自动升级 | B5/B6 | A14–A16 |
| D12 | 0006 down 先检查 Step8 新 action 数据，再删除新增索引/列；存在任一 Step8 审核行、consistency 数据或兼容审计则拒绝回退 | 不破坏 Step 8 审计；既有 Step 6 行也不得被静默删除 | B1 | A2、A17 |

## 4. 架构与数据流

```text
compile admission
  └─ compatibility preflight (read all persisted dependency/artifact snapshots)
       ├─ incompatible → BEGIN IMMEDIATE: compatibility review upsert → reject
       └─ compatible
            └─ claim lease
                 └─ heartbeat loop ──┐
                  compiler/model ────┤
                  source-ref validate ┘
                         ↓
             top-k related accepted pages
                         ↓
             ConsistencyArbiter → None | score + findings
                         ↓
                  RuleScorer / five-dim gates
                  ├─ accepted → publish transaction
                  └─ candidate failure → retry or dead
                                      └─ same transaction writes review_queue

LeaseReaper timer → recover expired running tasks → retry/backoff or dead/review
review approve(dead letter) → BEGIN IMMEDIATE → re-admit with epoch fence
```

模块边界：

```text
crates/wiktor-core/src/compile/consistency.rs
crates/wiktor-core/src/compile/compatibility.rs
crates/wiktor-core/src/kernel/compile_store.rs       # lease/review atomic helpers
crates/wiktor-core/src/kernel/feedback_store.rs      # new review actions + list/review
crates/wiktor-core/migrations/0006_step8_consistency/up.sql
crates/wiktor-core/migrations/0006_step8_consistency/down.sql
crates/wiktor-cli/src/commands/domain.rs              # domain check
crates/wiktor-cli/src/commands/compile.rs             # preflight/status output
```

不新增 crate，不让 core 依赖 feedback/server。异步 `PipelineExecutor` 只调同步 kernel public method 的 `spawn_blocking` 包装；同一连接 mutex 不跨 await。

## 5. 数据模型与 0006 DDL 草案

### 5.1 Domain YAML 扩展

```yaml
name: ecommerce
version: 1.2.0
schema_version: 2.1.0
prompt_version: 3.0.0
compile:
  artifact_version: wiki-v2
  consistency:
    enabled: true
    top_k: 8
    min_consistency: 1.0
    compare_pointers: ["/fields/name", "/fields/description"]
compatibility:
  domain_pack: ">=1.0.0,<2.0.0"
  schema: ">=2.0.0,<3.0.0"
  prompt: ">=3.0.0,<4.0.0"
  artifact: ["wiki-v1", "wiki-v2"]
```

`version`、`schema_version`、`prompt_version` 必须是 strict semver；兼容范围采用 semver 正式比较，不按字符串排序。`compatibility.artifact` 是显式允许列表。缺失 `compatibility` 时只允许 legacy 首次启动的只读检查；对已有 Step 4 编译数据的真实 compile 视为配置错误，不能猜测兼容。

### 5.2 迁移文件

迁移路径：`crates/wiktor-core/migrations/0006_step8_consistency/{up,down}.sql`。以下是必须保持的列和约束；已有表不删除、不重建、不改变 Step 4 三元 UNIQUE。

```sql
-- 迁移 0006：Step 8 一致性、死信审核与兼容检查
-- Migration 0006: Step 8 consistency, dead-letter review, compatibility checks

ALTER TABLE compile_tasks ADD COLUMN consistency_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (consistency_status IN ('unchecked','not_comparable','consistent','conflict'));
ALTER TABLE compile_tasks ADD COLUMN compatibility_status TEXT NOT NULL DEFAULT 'unchecked'
  CHECK (compatibility_status IN ('unchecked','compatible','incompatible'));

-- 仅保存确定性诊断摘要，不保存源明文；完整候选 artifact 仍在 compile_attempts
ALTER TABLE compile_attempts ADD COLUMN consistency_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE compile_attempts ADD COLUMN compatibility_json TEXT NOT NULL DEFAULT '{}';

CREATE INDEX idx_compile_tasks_dead_review
  ON compile_tasks(status, result, updated_at);
CREATE INDEX idx_compile_tasks_preflight
  ON compile_tasks(domain_pack_version, compatibility_status);

-- 0005 review_queue 的 action CHECK 必须通过表重建放宽；SQLite 不支持直接 ALTER CHECK。
CREATE TABLE review_queue_step8_new (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN (
    'supplemental_compile','query_template','ignore',
    'compile_dead_letter','consistency_conflict','compatibility_conflict'
  )),
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending','approved','ignored','failed')),
  source_log_ids_json TEXT NOT NULL,
  subject_json TEXT NOT NULL,
  reason_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  reviewed_at INTEGER,
  reviewed_by TEXT,
  compile_task_id INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT,
  UNIQUE(domain, action, subject_json)
);
INSERT INTO review_queue_step8_new
  SELECT review_id,domain,action,status,source_log_ids_json,subject_json,
         reason_json,created_at,reviewed_at,reviewed_by,compile_task_id
  FROM review_queue;
DROP TABLE review_queue;
ALTER TABLE review_queue_step8_new RENAME TO review_queue;
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);
CREATE INDEX idx_review_task ON review_queue(domain, action, compile_task_id)
  WHERE compile_task_id IS NOT NULL;
```

`consistency_json` 和 `compatibility_json` 为 canonical JSON，均不得超过 64 KiB；只存 code、比较键、旧/新值的 BLAKE3 摘要、版本和数量，不存敏感源字段原文。Step 8 的 `source_log_ids_json` 对编译审核固定为 `[]`。

死信入队的 canonical subject/reason 形状：

```json
{"task_id":42}
```

```json
{
  "code":"CONSISTENCY_CONFLICT",
  "task_id":42,
  "epoch":3,
  "attempt_no":3,
  "entity_id":"ecommerce:drink:boba",
  "desired_hash":"...",
  "findings":[{"key":"/fields/description","old_hash":"...","new_hash":"..."}]
}
```

兼容告警 subject 必须包含当前 domain、当前 domain/schema/prompt/artifact 版本；同一升级组合只能产生一条 `compatibility_conflict`。

### 5.3 down.sql 守卫

down 必须先用临时 CHECK 守卫检查：`review_queue` 中 action 属于三个新增动作的行数为 0，`compile_tasks` 中 `consistency_status <> 'unchecked'` 或 `compatibility_status <> 'unchecked'` 的行数为 0，`compile_attempts` 中任一 Step8 JSON 非 `{}` 的行数为 0。任一非零即约束失败并停止；通过后删除 Step8 索引/列，再重建 `review_queue` 恢复原三动作 CHECK 和既有唯一约束。不得删除 0005 审核事实、不得静默清空列。

`db_schema.rs` 的 Diesel table! 增加四列，`SUPPORTED_SCHEMA_VERSION` 从 5 改为 6，迁移版本断言同步改为 6。`row_counts` 继续暴露 `review_queue`，不新增网络或外部状态。

## 6. Rust 类型与 trait 契约

### 6.1 一致性

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyPolicy {
    pub enabled: bool,
    pub top_k: u32,                 // 1..=32
    pub min_consistency: f32,       // finite, 0..=1
    pub compare_pointers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimKey {
    pub entity_id: String,
    pub pointer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyFinding {
    pub code: String,               // stable code, e.g. VALUE_DIVERGENCE
    pub key: ClaimKey,
    pub candidate_value_hash: String,
    pub evidence_value_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsistencyReport {
    pub score: Option<f32>,
    pub compared_claims: u32,
    pub findings: Vec<ConsistencyFinding>,
    pub candidate_count: u32,
}

#[async_trait]
pub trait ConsistencyCandidateProvider: Send + Sync {
    async fn top_k_related(
        &self, candidate: &CompiledPage, limit: u32,
    ) -> Result<Vec<CompiledPage>>;
}

pub trait ConsistencyArbiter: Send + Sync {
    fn arbitrate(
        &self,
        candidate: &CompiledPage,
        related: &[CompiledPage],
        policy: &ConsistencyPolicy,
    ) -> Result<ConsistencyReport>;
}
```

默认 `SourceRefConsistencyArbiter` 的算法固定如下：收集 candidate 与 related accepted 页 evidence 中的 source-ref；只保留 pointer 在 `compare_pointers` 且满足 RFC6901 `/fields/...` 形状的引用；以 `(entity_id,pointer)` 分组；同组值先 `canonical_json` 再比较；任意不同值产生 `VALUE_DIVERGENCE`。同一组只有一个值时不算比较；`compared_claims=0` 返回 `score=None`；否则 `score = equal_groups / compared_groups`。所有值摘要使用 BLAKE3，诊断不落原文。重复 ref、无 evidence、旧 seed 页均跳过，不得猜测等价关系。

默认 provider 必须执行有界召回：从 candidate 标题和 aliases 生成经过长度上限的 FTS 查询，访问 `pages_fts`，`status='accepted'`，按 bm25、page_id 稳定排序并 LIMIT `top_k`；随后只加载这些页的 evidence。FTS 无命中返回空集合。实现不能先 `SELECT * FROM pages` 再内存截断；测试 provider 可直接注入预构造页。

### 6.2 评分和发布

扩展 `RuleScorer::score` 的结果或新增组合器，不破坏 Step 4 的四维接口：

```rust
pub trait QualityScorerV2: Send + Sync {
    fn score_with_consistency(
        &self,
        source: &RawEntity,
        page: Option<&CompiledPage>,
        refs: &RefReport,
        schema_valid: bool,
        consistency: &ConsistencyReport,
        ctx: &CompileContext,
        policy: &CompilePolicy,
    ) -> ScoreReport;
}
```

组合规则：四维仍按 Step 4 公式；`consistency=None` 时 `overall` 仍为四维平均；`Some(s)` 时 `overall=(coverage+citation+schema+density+s)/5`。`Some(s)` 且 `s < min_consistency` 增加稳定 issue `CONSISTENCY_BELOW_THRESHOLD`，不得 accepted。所有分数 finite 且在 `[0,1]`；`consistency` 写入 `page_quality.consistency`，不一致页不写 accepted page。

`CompilePolicy` 增加 `consistency: ConsistencyPolicy`、`lease_reaper_interval_seconds`、`compatibility_preflight: bool`。策略和版本继续进入 content hash；top-k/租约时钟不进入 content hash。改变一致性比较指针、阈值或实现版本必须改变 scorer/policy 依赖版本，使新 hash 触发重编译。

### 6.3 租约和回收

```rust
pub struct LeaseReaper {
    pub kernel: Arc<SqliteKernel>,
    pub clock: Arc<dyn Clock>,
    pub interval: Duration,
}
impl LeaseReaper {
    pub async fn run_until_cancelled(&self, cancel: CancellationToken) -> Result<()>
}

impl SqliteKernel {
    pub fn recover_compile_leases(&self, now: i64) -> Result<RecoveryStats>;
    pub fn heartbeat_compile(&self, lease: &TaskLease, now: i64) -> Result<bool>;
    pub fn enqueue_dead_letter_on_conn(
        tx: &mut SqliteConnection, task_id: i64, domain: &str,
        epoch: i64, reason_json: &str, now: i64,
    ) -> Result<()>;
}
```

`RecoveryStats` 至少包含 `recovered_pending`、`dead_failed`、`dead_quarantined`、`superseded`、`review_inserted`。现有 recovery SQL 的旧 token CAS、attempt abandoned、预留不退还、superseded 优先级必须保留；只有进入 `dead` 的分支在同一事务调用 `enqueue_dead_letter_on_conn`。`INSERT ... ON CONFLICT(domain,action,subject_json) DO NOTHING` 是幂等保护。

executor 启动 reaper；若 `run` 被取消，先取消心跳，再等待心跳任务结束，最后做一次同步 recover。心跳更新必须继续使用 `WHERE task_id AND epoch AND lease_token AND status='running' AND lease_expires_at > now`；false 只代表 lease stale，不能直接改任务。模型请求期间禁止持有 SQLite guard；心跳失败后仍允许模型返回，但 publish/failure 的既有 fencing 会拒绝旧 worker。

### 6.4 兼容检查

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilitySpec {
    pub domain_pack: VersionReq,
    pub schema: VersionReq,
    pub prompt: VersionReq,
    pub artifact: BTreeSet<String>,
}

pub struct CompatibilityReport {
    pub compatible: bool,
    pub checked_pages: u64,
    pub checked_tasks: u64,
    pub violations: Vec<CompatibilityViolation>,
}

pub trait CompatibilityChecker: Send + Sync {
    fn check(&self, current: &DomainIdentity,
        spec: &CompatibilitySpec, db: &SqliteKernel) -> Result<CompatibilityReport>;
}
```

检查所有 accepted pages 的 `domain_pack_version/artifact_version/frontmatter quality_policy` 和所有 pending/running/dead tasks 的 `dependencies_json` 快照；不检查已明确 superseded 的历史 attempt 作为当前产物，但仍统计损坏 JSON 为 violation。版本缺失、非法 semver、artifact 不在 allowlist 均为不兼容。`compatible=false` 时 compile 不调用 `admit_compile`，不改变 facts，不创建 compile task；若由真实 compile 触发，在同一 `BEGIN IMMEDIATE` 内写 `compatibility_conflict` review，失败则整个事务回滚。

## 7. CLI 契约与退出码

新增命令：

```text
wiktor domain check --domain <domain.yaml> --db <path> [--json]
wiktor compile --domain <domain.yaml> ... [--skip-compatibility-check]
wiktor feedback review approve --db <path> --review-id <id> --by <operator>
wiktor feedback review ignore  --db <path> --review-id <id> --by <operator>
```

`domain check` 默认只读，不迁移、不创建数据库、不写 review；不存在数据库按空库检查配置本身。`--json` stdout 只有 `CompatibilityReport`；人类日志走 stderr。检查发现不兼容返回退出码 3；数据库/IO 返回 1；参数 2；其它内部错误 4。

`compile` 默认自动 preflight；不能用 `--skip-compatibility-check` 绕过不兼容结果。该开关只允许空数据库/没有旧 artifact 时跳过重复检查，若发现任何旧数据仍返回 3。兼容 preflight 的失败不计入 `failed`/`quarantined` 单页统计；run 以配置/迁移错误结束。死信列表沿用 `feedback list`，action 原样输出；`approve compile_dead_letter` 必须解析 subject 的 task_id，检查该任务为 `dead`、review 为 `pending`，使用原任务快照显式 `force=true` 新建 epoch admission，再在同一事务将 review 置 `approved` 并回填新的 task_id。若 admission 不是 `Queued`，整体回滚。`consistency_conflict` 只能 approve 为一次新的 `supplemental_compile` 审核转换，不能直接发布；`compatibility_conflict` 只能 approve 为审计状态，不能绕过 preflight。

退出码沿用 Step 4/6：0 成功；1 数据库、IO、运行故障；2 参数/范围错误；3 配置、迁移、兼容或数据协议错误；4 未分类内部错误。`--json` 不混入表格或日志。

## 8. 并发、事务与端口语义

所有涉及 `compile_tasks`、`compile_attempts`、`compile_source_heads`、`review_queue` 的写入都必须取得连接 mutex 一次并使用 `immediate_transaction`，DML 绑定参数，内部 helper 只接 `&mut SqliteConnection`。不在事务中调用另一个会重新取 mutex 的 public kernel 方法，不持锁跨 await。

- **质量终态**：`finish_compile_failure` 在写 attempt、更新 task、插入 dead-letter/consistency review 时是一个事务。任何一步失败，候选审计、任务状态和审核行全部回滚。
- **回收终态**：旧 lease token 的 attempt abandoned、retry/dead/superseded 和死信入队是一个事务；重复回收 affected rows 为 0，不增加 retry_count、不重复插 review。
- **死信批准**：复用 Step 6 的 `approve_review` 事务模式；禁止调用带自身事务的 `admit_compile`，必须调用 `admit_compile_on_conn`；review 状态 CAS 和 epoch fence 与任务 admission 同事务。
- **兼容告警**：只在 compile admission 的写事务中插入；相同 `(domain,compatibility_conflict,subject_json)` 冲突跳过。只读 `domain check` 不写审核队列。
- **心跳**：heartbeat 是短单语句写事务；模型调用、候选召回、仲裁、JSON 编码均在事务外。lease 过期后心跳 false，publish/failure 只能得到 stale 语义。
- **端口**：Step8 不增加、不监听端口，不依赖 axum、HTTP、gRPC 或 qdrant；CLI 与后台 reaper 直接使用同一 SQLite kernel。

## 9. 验收标准 A1–A18

以下全部使用临时 SQLite、注入 Clock、预构造页和 fake Compiler；无 key、无网络、无 qdrant、无 LLM。

| # | 判据 | 可执行断言 |
|---|---|---|
| A1 | 迁移兼容 | 0001–0005 数据库升级到 6；既有三种 review action、Step4 任务、accepted 页和 FTS 可读；schema version=6 |
| A2 | 迁移回退守卫 | Step8 任一审核/一致性/兼容审计非空时 down 失败且行不变；全部为空时恢复旧 CHECK 和索引 |
| A3 | trait 可替换 | fake arbiter 可返回 None/1/0；executor 不依赖具体实现，未来 LLM 类型不进入 core 状态机 |
| A4 | top-k 边界 | provider 收到且最多请求 32；SQLite SQL 含 LIMIT；构造 100 页时只加载 8 页，绝不全量比较 |
| A5 | 精确证据 | 相同 `(entity_id,pointer)` 相同 canonical value 得分 1；不同 value 得 `VALUE_DIVERGENCE`；标题相似但无 ref 重叠返回 None |
| A6 | 评分兼容 | consistency=None 时 overall 等于旧四维平均；consistency=1 时按五维平均；0 或低于阈值不能 accepted；SQL NULL/0/1 精确落库 |
| A7 | 证据安全 | 不同 domain、非法 pointer、无 evidence、旧 seed 页不产生比较；诊断只保存哈希，不保存敏感原文 |
| A8 | 默认 provider | FTS 相关页按 bm25/page_id 稳定排序；quarantined/dead/旧 generation 页不会成为 related |
| A9 | 一致性刹车 | 连续冲突在 `max_recompiles=2` 后最多 3 个候选，任务 dead/quarantined；同一事务存在一条 dead-letter 和一条 consistency review |
| A10 | 死信幂等 | 同 task 重复 finish/recover 只存在一条 `compile_dead_letter`，subject 精确为 `{"task_id":N}`；`compile_task_id` 正确回填 |
| A11 | 周期回收 | 不再启动新 compile run，reaper 在注入时间到期后回收 running；未超限 pending/backoff，超限 dead；关闭时 drain 一次 |
| A12 | 心跳 fencing | fake model 等待超过 300 秒仍因心跳保持 lease；取消/失败后停止心跳；A 失去 token 后不能 publish、failure、修改计数 |
| A13 | semver 矩阵 | 合法范围接受；非法 semver、schema/prompt 不匹配、artifact 不在 allowlist 拒绝；不按字符串排序误判 |
| A14 | 全量兼容 | 数据库中任意旧 accepted page/task 违规都出现在 report，计数准确；损坏 dependencies JSON 为 violation，不能静默跳过 |
| A15 | preflight 触发 | `domain check` 只读；compile 在首次 admission 前调用同一 checker；不兼容时无 facts/task 写入 |
| A16 | 兼容审核幂等 | compile 不兼容在一个事务产生一条 `compatibility_conflict`；重复运行不新增；只读 check 不写 review |
| A17 | 审核转换 | approve dead letter 只能从 pending、dead 任务开始，复用 `admit_compile_on_conn` 创建新 epoch；失败 review/task 全回滚；compatibility review 不能绕过 preflight |
| A18 | 工程回归 | `cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、workspace tests 和 Rust 1.85 编译通过；Step4/6 验收无回退 |

## 10. 给 wiktor-builder 的实现批次

1. **批次 B1：配置、迁移与版本模型**。增加 `CompatibilitySpec`、strict semver 解析、`ConsistencyPolicy` 默认值、0006 up/down、Diesel schema、schema version 6；完成 A1/A2/A13 的纯解析部分。不得改旧 hash 语义，新增策略版本必须明确进入依赖哈希。
2. **批次 B2：一致性 trait 与确定性仲裁**。新增 `consistency.rs`、claim key/value canonical 比较、top-k provider trait、默认 FTS provider 和 `QualityScorerV2` 组合；用注入页完成 A3–A8。此批不改任务状态。
3. **批次 B3：质量失败与审核队列原子接入**。扩展 review action 校验/列表；在 `finish_failure_in_transaction` 的 dead 分支入队，写 consistency JSON 和 task 状态；完成 A9/A10。所有 SQL 使用同一事务，不复制 admission SQL。
4. **批次 B4：reaper 与心跳编排**。新增周期 `LeaseReaper`，executor 连接 cancel/drain；模型等待包围 heartbeat task，保留现有 recovery/fencing/backoff；完成 A11/A12。不得引入端口或 gRPC。
5. **批次 B5：兼容检查器与 compile preflight**。扫描 accepted pages/tasks，校验 semver/schema/prompt/artifact；实现只读 report；compile admission 前接入；完成 A13–A15。preflight 失败禁止 facts/task 写入。
6. **批次 B6：兼容审核与 dead-letter CLI**。扩展 `feedback review list/approve/ignore` 对新 action 的 fail-closed 语义，新增 `domain check`、JSON 输出、退出码；实现死信批准的新 epoch admission；完成 A16/A17。
7. **批次 B7：并发收口与回归**。补 down 守卫、WAL/busy timeout 路径、故障注入、Step4/6 回归、注释双语同步；执行 A18，追加实现偏差记录。每批必须保持可编译、可独立验证。

## 11. 风险、取舍与预置偏差表

一致性默认实现的表达力故意受限：它能证明 source-ref 证据值的精确分歧，不能证明“两个不同句子语义矛盾”。这符合 MVP 无 LLM 和不推断领域知识的约束；领域若需要更丰富比较，必须声明 pointer/key 规则或提供替换 arbiter。

FTS top-k 是离线可用的相关页提供器，未来嵌入召回可以实现 `ConsistencyCandidateProvider`，但必须继续执行 top-k 上限、accepted/hash 对齐和无全量比对。qdrant 不在 Step8 验收路径中。

死信与一致性审核共享 review_queue，简化审核生命周期，但动作数量增加后 CLI 必须 fail-closed；未知 action 视为数据库损坏/协议错误，不能当作 ignore。死信批准会新建 epoch，旧 attempt 永不复用，预算也重新按既有 admission 规则检查。

### 预置偏差表

| ID | 计划/现状偏差 | 原因 | 影响 | 补偿措施 | 是否需回写 MASTER-PLAN |
|---|---|---|---|---|---|
| STEP8-001 | MASTER-PLAN 将一致性写成 top-k+LLM；Step8 默认实现不调用 LLM | MVP 明确排除一致性 LLM，且 X5 要求离线确定性 | 默认一致性只能检测明确证据分歧 | `ConsistencyArbiter` trait 保留替换点；future LLM 不改核心 | 否 |
| STEP8-002 | 一致性 top-k 默认 provider 使用 SQLite FTS，而不是向量/嵌入召回 | Step8 验收必须无 qdrant/网络 | 相关页质量取决于词法召回 | provider trait；未来 qdrant 实现仍受 top-k 与 generation 校验 | 否 |
| STEP8-003 | 一致性无可比较 claim 时为 NULL，不写 0 | 无证据不能证明一致或矛盾 | 页面不因缺少比较对象被错误隔离 | `None` 保持旧 overall；有证据时才启用五维门槛 | 否 |
| STEP8-004 | 死信复用 review_queue 并扩展 action CHECK | Step6 已有审核事务和审计字段 | 0006 必须重建 SQLite CHECK 表 | up/down 迁移保留旧行、唯一键和 FK；新 action 独立 subject 规范 | 否 |
| STEP8-005 | 现有 executor 仅 run 开头 recover，无后台回收 | Step4 单次 run 交付了方法但没有常驻调度 | worker 停止后 running 可长期悬挂 | LeaseReaper 周期循环；启动/关闭各做一次同步回收 | 否 |
| STEP8-006 | 模型等待期间现有 executor 没有 heartbeat loop | Step4 只交付 kernel heartbeat | 长模型调用可能租约过期并触发重复付费 | B4 增加可取消心跳 task，最终仍由 token/epoch fence 裁决 | 否 |
| STEP8-007 | 兼容矩阵在 domain.yaml，不建独立数据库表 | 领域包是配置权威，独立表会产生双写漂移 | 数据库只保存快照，不能自行编辑矩阵 | compile preflight 读取配置，dependencies_json 保存快照并参与诊断 | 否 |
| STEP8-008 | 兼容失败会拒绝 compile，并幂等产生审核告警；不自动改 hash/重编译 | 不兼容升级需要人工决定，自动运行可能破坏旧产物 | 运维需先修版本矩阵或批准后再运行 | `domain check` 只读报告；compile fail-closed；hash 变化仍由 Step4 处理 | 否 |
| STEP8-009 | 0006 需要迁移重建 review_queue | SQLite 不能直接修改既有 CHECK | 迁移操作面扩大 | 复制全列、保留 FK/UNIQUE、down 有数据守卫；任何失败由迁移事务回滚 | 否 |
| STEP8-010 | B1 把兼容矩阵快照载体定为 `CompilePolicy.compatibility: Option<CompatibilitySpec>` | D10 要求矩阵随 dependencies_json 持久化，而快照只有 {context,policy,schema} 三个载体，policy 是唯一配置承载位；§6.2 字段清单未列此字段 | dependencies_json 形状扩展（serde default 向后兼容） | 进入 content hash；B5 直接消费同一类型 | 否 |
| STEP8-011 | schema/prompt 版本身份经 CompileContext 新增两个 Option\<String\>（serde default）进入 hash 与快照，而非给 HashDependencies/admit 加 identity 参数 | publish 与 supplemental 重 admission 只能经 lease.context 还原身份；加参数会波及 feedback 层（违反 B1 边界） | 快照形状扩展 | 旧行 serde default 可读；B5 可从快照读历史版本 | 否 |
| STEP8-012 | content_hash_golden 黄金哈希重钉（22454908… → 2ee5fecd04bfb1…） | 新增 4 个身份哈希域改变字节（编码与前 10 域顺序不变） | 黄金哈希变更 | 新测试锁定「身份域失效」语义；任务说明授权显式更新 | 否 |
| STEP8-013 | §6.2 的 QualityScorerV2 未落为独立 trait，改为 `RuleScorer::score_with_consistency` 扩展方法（默认实现 = 旧四维路径） | 选择标准 a：既有 executor 四维路径零改动、B3 接状态机改点最小；§6.2 参数形状与语义逐字保留 | 接口形态与 §6.2 字面不同 | 只覆写 `score` 的既有实现自动保持 Step4 行为；新测试锁定 legacy 默认方法忽略 consistency | 否 |
| STEP8-014 | `QualityScore::overall()` 改为按 consistency 分支（None→四维平均逐字节不变；Some→五维等权） | §6.2 只规定公式未指定载体；overall 需单源 | 载体选择 | 单源 overall() 使门槛判断与 B3 落库 overall 列自动一致；旧路径测试全过 | 否 |
| STEP8-015 | ConsistencyReport.candidate_count 语义固定为「参与仲裁的相关页数量 related.len()」 | §6.1 未定义该字段 | 字段语义确定 | 注释与测试锁定 | 否 |
| STEP8-016 | 多分歧值 finding 哈希取值规则：candidate_value_hash=候选页出现的最小 canonical 字节值（候选缺席取全组最小），evidence_value_hash=与其不同的最小 distinct 值 | 字节序保证确定性 | 诊断哈希取值确定 | 原文不落诊断；测试锁定 | 否 |
| STEP8-017 | provider FTS 词构造细则：标题优先、aliases 保序、按字符去重、<3 字符词跳过（trigram MATCH 下限）、总预算 256 字符前缀截断、词组引号转义 | 与 kernel search 惯例对齐、防超限 | 词构造细则确定 | SQL 全落 kernel；常量 MAX_FTS_QUERY_CHARS=256 | 否 |
| STEP8-018 | 候选自身排除以 `page_id != ?` 实现（同时覆盖其旧 generation 行）；空词表/limit=0 直接返回空不发 SQL | 实现细化 | 排除与空路径语义确定 | kernel.top_k_related_pages 测试锁定 | 否 |
| STEP8-019 | §5.2 死信 reason 示例把 findings[].key 写成裸指针字符串；实现为完整 ClaimKey 对象 {"entity_id","pointer"} | findings 可指向非本页实体，裸指针不足以定位 | 诊断 JSON 形状与示例不同 | 测试锁定形状；摘要仍为 BLAKE3 不落原文 | 否 |
| STEP8-020 | consistency_conflict 审核行 subject 固定 {"task_id":N}（与死信同纪律，UNIQUE+DO NOTHING 保证每 task 一条） | spec §5.2 只固定了死信 subject | 冲突审核 subject 形状确定 | 测试锁定 | 否 |
| STEP8-021 | enqueue_dead_letter_on_conn 实现为自由 pub(super) 事务内函数，而非 spec §6.3 的 impl SqliteKernel 公开关联函数 | 对齐既有 admit_compile_on_conn 先例；裸 tx 助手留在 crate 内 | 可见性形态与 §6.3 字面不同 | 语义不变；测试锁定幂等 | 否 |
| STEP8-022 | 存储 consistency_json 用 kernel canonical_text（BTreeMap 键序紧凑 JSON，与 quality_json 同纪律）；hash.rs canonical_json 是带类型标签的哈希输入编码非可解析 JSON | 「复用 canonical_json」按规范精神解读为同一 canonical 纪律 | 编码函数选择 | 可解析 JSON 落库；摘要仍用 hash 域编码 | 否 |
| STEP8-023 | publish 的一致性值以仲裁报告参数为权威源（与 report.quality.consistency 同源，行为等价） | 避免双写入漂移 | 数据源确定 | 测试锁定 SQL NULL/0/1 精确落库 | 否 |
| STEP8-024 | 仲裁/召回错误 fail-closed 传播并停止 run，不当单页质量失败吞掉 | spec §4 未画仲裁错误分支；内部错误统一处理面 | 错误语义确定 | 测试锁定传播路径 | 否 |
| STEP8-025 | enqueue_dead_letter_on_conn 字面返回 Result<()>，实现改为 Result<usize>（实际插入行数） | 供 RecoveryStats.review_inserted 精确计数（DO NOTHING 幂等跳过不计） | 返回类型变化 | 其余签名与语义不变；测试锁定 | 否 |
| STEP8-026 | D9 心跳周期按 spec 落为 lease_seconds/2（最小 1s）；Step4 遗留 CompilePolicy.heartbeat_seconds（默认 30，带 validate）未被该循环消费 | Step4 已存在 heartbeat_seconds 字段，与 D9 建议并存 | 双周期字段并存 | 主模型拍板：心跳循环统一消费既有 heartbeat_seconds（30s），删除 lease_heartbeat_interval 独立推导路径；策略字段单源 | 否 |
| STEP8-027 | RecoveryStats.dead_quarantined 当前回收路径恒为 0（回收 dead 分支只产生 failed） | 字段照 §6.3 保留以稳定观测面 | 观测面稳定 | 注释说明；字段保留 | 否 |
| STEP8-028 | §5.1「缺失 compatibility 时 legacy 只读容忍；有 Step4 数据的真实 compile 视为配置错误」——实现为 executor 对缺失矩阵一律直通，未对「缺失矩阵 + 已有数据」强制配置错误 | 338 基线中 Step4/6 既有测试正是 legacy 无矩阵 + 已有数据下重跑，强制即破坏基线；A13-A15 均以矩阵存在为前提 | 缺失矩阵不 fail-closed | 主模型拍板：保留直通以保基线；B6 domain check 对矩阵缺失显式输出告警 | 否 |
| STEP8-029 | 违规 code 命名为大写下划线稳定码（CORRUPT_SNAPSHOT 等） | 对齐 VALUE_DIVERGENCE 既有惯例 | code 命名风格 | 测试锁定稳定码 | 否 |
| STEP8-030 | preflight 执行点为「首个非空批次获取之后、该批次 admission 之前」（domain 取首实体） | 复用 dry-run 首实体定域惯例；spec 未指定 executor 侧定域方式 | 触发点确定 | 空源 run 零 admission 不触发 | 否 |
| STEP8-031 | 兼容告警 subject 缺失 schema/prompt 版本时以 "schema_version":null 键呈现 | §5.2 只要求必须包含版本未规定缺失形态 | 缺失形态确定 | B3 宽松校验保持兼容 | 否 |
| STEP8-032 | A17「回填新的 task_id」实际回填的是同一任务 id——UNIQUE(entity_id, source_revision, domain_pack_version) 保证 force 重排的就是原任务行（epoch+1 为新身份），死信批准不产生第二行 | 三元 UNIQUE 语义决定 | 回填语义明确 | 测试锁定（同 id、epoch 1→2、计数归零、result 清空） | 否 |
| STEP8-033 | 死信/一致性重放的 admission 输入沿任务行存档 snapshot_hash 重建 PreparedSource（不重投影），并新增 head 快照守卫（head 缺失/revision/snapshot 漂移 → Validation 回滚） | 任务快照只存知识投影，重投影会算出不同 snapshot_hash 被 head CAS 拒绝；守卫同时保证空 facts 不触发 facts CAS 写入 | 重放语义明确 | head 守卫 + 测试锁定 | 否 |
| STEP8-034 | consistency_conflict approve 时对 subject 施加与死信相同的 strict canonical {"task_id":N} 校验（内核生成行即 canonical；approve 侧收紧 fail-closed） | B3 入库侧仅要求 JSON 对象 | 校验收紧 | 测试锁定 | 否 |
| STEP8-035 | 一致性转换按「创建并批准」落地：转换事务内完成 supplemental 建议行插入 + force admission + 两行 CAS approved，建议行 born-pending 后同事务批准 | 无独立可批准的中间态、审计链完整 | 转换原子性 | 测试锁定 | 否 |
| STEP8-036 | CompatibilityReport 新增 warnings 字段（skip_serializing_if 空时省略），矩阵缺失告警 code=MISSING_COMPATIBILITY_MATRIX | STEP8-028 补偿落点 | 告警载体 | 无告警 JSON 与 §6.4 形状逐字节一致 | 否 |

实现者发现与本 spec 不一致时，必须在中英文对应节追加新的 `STEP8-xxx` 行，说明原因、接口影响和验收变化；不得静默改变 D1–D12、DDL、退出码或既有 epoch/fencing 语义。

<!-- END STEP8 SPEC v1.0 -->
