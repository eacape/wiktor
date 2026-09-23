# Step 6 设计规范：反馈闭环与 `POST /feedback`

> 版本：v1.0（2026-09-22）  
> 上承接：Step 4 `step4-compile-pipeline.md`、Step 5 `step5-qug-build.md`、Step 3 QueryEngine  
> 实现对象：`wiktor-builder`；独立验收对象：`test-engineer`  
> 本文件是中文权威设计；英文版 `step6-feedback-loop.en.md` 逐节对应。

## 1. 目标与非目标

Step 6 落地编译—检索双向反馈的数据面和控制面：上层应用用认证的 HTTP API 回传采纳信号；分析器把查询日志与反馈聚合成三类盲区报告；建议只进入人工审核队列，审核批准后才通过既有编译 admission 进入 `compile_tasks`。本步同时建立 `wiktor-server` 的最小 HTTP 面，并为 #7 保留扩展边界。

目标：

- 新建 `wiktor-feedback`（分析、报告、审核队列访问）与 `wiktor-server`（axum HTTP）；两者都依赖 core，不让 core 依赖 server。
- 新增 0005 migration：反馈事实、审核队列、查询日志滤空/放宽重试状态。
- 实现 `POST /feedback`、`GET /health`、`GET /metrics`；认证、domain 租户范围、限流、幂等、输入预算均为硬契约。
- `wiktor feedback analyze|list|review` 消费口子，沿用退出码 0/1/2/3/4。

非目标：gRPC/tonic、自动编译或自动修改领域包、审核 UI、LLM 分析、跨库分布式限流、QUG 规则自动生成。反馈分析不调用 `admit_compile`；只有人工 approve 命令可以调用它。

## 2. 术语与现状约束

- **query log**：0001 的 `query_logs` 行；当前字段为 `query_text/query_json/rewritten_json/rewrite_failure/hit_count/latency_ms/timestamp`。
- **滤空**：事实平面下推后的候选为空。按 §5.4，必须先放宽一次并重试；仍为空不等于知识盲区。当前 QueryEngine 在空候选处立即返回，Step 6 Batch 3 必须修正该落点。
- **反馈事件**：客户端针对一次 query log 的 `click`、`adopt` 或 `rate` 信号；`hit` 不作为 API 事件，命中数已在 query log 中。
- **审核项**：分析器生成的、尚未改变编译任务状态的建议；`review_queue` 是控制面事实。
- **租户**：MVP 以 `domain` 为租户边界。API key 明确绑定一个或多个 domain；请求 body 的 domain 必须在 key 的允许集合内。
- **隔离队列**：超输入预算的请求不写正常反馈表，写 `feedback_rejections` 计数表并返回 413；这是可靠性契约 #7 的可审计最小实现。

现状中 Diesel/SQLite kernel 是同步 API，异步只存在于 QueryEngine/CLI 编排；本规范保留该边界，数据库锁不得跨 await。SQLite 使用 WAL；所有写操作短事务、参数绑定、错误向上传递。

## 3. 决策 D1–D12

| ID | 拍板决策 | 理由 | 备选及不采用原因 |
|---|---|---|---|
| D1 | 新建 `wiktor-feedback` 与 `wiktor-server`；CLI 仅调用库函数和 server 构造器。server 提供 `POST /feedback`、`GET /health`、`GET /metrics`。 | 对齐 MASTER-PLAN §9，HTTP 生命周期与 CLI 解耦；插件只依赖 core。 | 把 server 塞进 CLI 会把网络、运行时和命令状态耦合，且不能复用服务库。 |
| D2 | server 使用 `tokio` 多线程 runtime；`axum = "0.7"`、`tower = "0.5"`（仅使用已有/axum 传递层能力），不引 tonic。 | axum 0.7 与 Rust 1.85、现有 Tokio 1.40 兼容；HTTP 是本步唯一协议。多线程避免单个分析/SQLite 请求阻塞 accept loop。 | 单线程 runtime 延迟隔离较差；axum 0.8 会扩大本步升级面。 |
| D3 | 事件 MVP 仅 `click`、`adopt`、`rate`；`rate` 为 1..=5；每事件必须带 `log_id`，可带 `page_id`，`click/adopt` 的 `page_id` 必填。 | 命中由 query log 已有 `hit_count` 表示；三种足够覆盖点击、采纳和显式质量评分，避免自造行为枚举。 | `hit` 重复记录已有事实；收藏/跳过/自由文本会使分析语义和隐私边界膨胀，延期。 |
| D4 | 0005 建 `feedback_events`、`review_queue`、`feedback_rejections`；不扩展 `compile_tasks` 审核列。 | 审核建议不是可运行任务；独立表可审计、去重、支持 ignore/approve，且不污染 Step4 状态机。 | 在 `compile_tasks` 加 `needs_review` 会破坏既有 pending/running/succeeded/failed/dead 契约。 |
| D5 | `feedback_events.idempotency_key` 在 `(domain, idempotency_key)` 上 UNIQUE；重复请求返回 200 且返回第一次的 `event_id`/时间，不更新载荷。 | 重试是正常网络行为；200 回放使客户端无需区分成功重试与原始成功。 | 409 会把可恢复网络重试变成业务失败；覆盖旧事件会破坏审计。 |
| D6 | 静态 key 从环境变量 `WIKTOR_FEEDBACK_API_KEYS` 读取，值为 JSON `{ "domain": "secret" }`；不从 domain.yaml 读取、不落库。请求用 `Authorization: Bearer <secret>`。 | secret 不属于领域知识；环境变量适合单机 MVP，启动时一次解析并拒绝空/重复/非法配置。 | YAML 会把凭据提交进领域包；独立配置文件增加权限和部署面。多 key/多 domain 的 JSON 仍保持单一配置入口。 |
| D7 | 租户校验 = key 的允许 domain 与 body `domain` 精确匹配；不接受 `*`，domain 使用领域配置的 canonical name。 | 防止 key 跨领域写入；错误统一 403，不泄漏 domain 是否存在。 | 仅 key 全局授权过宽；以 host/header 推断租户不稳定且易绕过。 |
| D8 | 限流为进程内固定窗口：每 `(domain,key_hash)` 60 秒最多 120 次成功或失败认证后的已授权请求；超限 429，响应 `Retry-After` 为剩余秒。重启清空。 | 无重型依赖、确定、适合单 SQLite 进程 MVP；key 原文不进内存标签，使用 BLAKE3 截断标识。 | SQLite 计数会增加写争用和清理事务；分布式限流留 #7/部署层。 |
| D9 | 请求 body UTF-8 字节上限 64 KiB；事件数组最多 100 条；单个 query_text 4096 Unicode scalar；超限返回 413，写 `feedback_rejections(reason,payload_bytes,created_at)` 计数审计。 | 落实输入预算 #7，防止解析前内存膨胀。拒绝不写 `feedback_events`，但计数可观测。 | 截断会制造错误反馈；静默丢弃违反可靠性契约。 |
| D10 | 查询滤空字段为 `candidate_empty_initial`、`relaxation_attempted`、`relaxation_succeeded`；若初始为空，QueryEngine 调用注入的 `FilterRelaxer` 至多一次。三者写同一 query log 事务。 | 明确区分滤空与盲区；当前代码空候选提前 return，新增显式接口后可验证。 | 只加 `zero_recall` 会把引擎判定和分析规则耦合，无法审计放宽结果。 |
| D11 | 分析窗口同时读取 `query_logs` 与 `feedback_events`；零召回 = `hit_count=0 AND NOT(candidate_empty_initial=1 AND relaxation_succeeded=0)`；改写失败 = `rewrite_failure=1`；低质量按 page 聚合，至少 5 个 `click/adopt/rate` 事件且采纳率 `< 0.20`。 | 规则可重放、无 LLM；低质量最小样本避免单个误点触发。`adopt` 计 1，`rate>=4` 计 1，`click` 只作分母。 | 仅按 click 会把点击误认为采纳；无最小样本会导致噪声审核。阈值以后按 domain 版本化，MVP 固定并写报告。 |
| D12 | 建议只插入 `review_queue`，action 为 `supplemental_compile/query_template/ignore`；approve supplemental_compile 时，必须在同一 `BEGIN IMMEDIATE` 中校验审核状态并调用既有 `admit_compile`，失败则整体回滚。 | 保持“反馈不自动执行”和 Step4 状态机；审核是唯一进入 compile_tasks 的门。 | 分析器直接 admission 会让噪声污染知识库，违反 §5.3。 |

## 4. 架构与数据流

```text
client
  └─ POST /feedback + Bearer key + Idempotency-Key
       └─ axum: size/auth/tenant/rate-limit/schema
            └─ FeedbackStore (短 SQLite 事务)
                 ├─ duplicate → 200 replay
                 └─ new → feedback_events

query → QUG/fallback → filter pushdown
      ├─ candidate non-empty → retrieve
      └─ candidate empty → FilterRelaxer once → retrieve or final empty
      └─ query_logs(rewrite + filter state + hits)

wiktor feedback analyze --from/--to
  └─ FeedbackAnalyzer(query_logs + feedback_events)
       ├─ FeedbackReport (JSON + bilingual Markdown)
       └─ review_queue (建议，不调用 compile)

wiktor feedback review approve <id>
  └─ BEGIN IMMEDIATE: claim review item → admit_compile → approved / rollback
```

模块边界：

```text
crates/wiktor-feedback/src/{lib,model,store,analyzer,report}.rs
crates/wiktor-server/src/{lib,main,state,auth,rate_limit,metrics}.rs
crates/wiktor-cli/src/commands/feedback.rs
crates/wiktor-core/src/query_engine/mod.rs       # FilterRelaxer + query log fields
crates/wiktor-core/migrations/0005_feedback_loop/{up,down}.sql
```

`wiktor-feedback` 依赖 `wiktor-core`、serde/serde_json、async-trait、tracing；`wiktor-server` 依赖 feedback、core、axum、tokio、serde/serde_json、tracing、blake3。不得让 feedback 依赖 server。HTTP handler 不直接拼 SQL。

## 5. 反馈数据模型与 0005 DDL 草案

迁移文件：`crates/wiktor-core/migrations/0005_feedback_loop/up.sql`。迁移在 Diesel migration 事务内执行；down 只允许开发库且先检查无 Step6 行，生产 down 必须报错，不能静默丢审计。

```sql
ALTER TABLE query_logs ADD COLUMN domain TEXT NOT NULL DEFAULT '__legacy__';
ALTER TABLE query_logs ADD COLUMN candidate_empty_initial INTEGER NOT NULL DEFAULT 0
  CHECK (candidate_empty_initial IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_attempted INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_attempted IN (0,1));
ALTER TABLE query_logs ADD COLUMN relaxation_succeeded INTEGER NOT NULL DEFAULT 0
  CHECK (relaxation_succeeded IN (0,1));
CREATE INDEX idx_query_logs_domain_time ON query_logs(domain, timestamp);

CREATE TABLE feedback_events (
  event_id INTEGER PRIMARY KEY AUTOINCREMENT,
  idempotency_key TEXT NOT NULL,
  domain TEXT NOT NULL,
  log_id INTEGER NOT NULL REFERENCES query_logs(log_id) ON DELETE RESTRICT,
  kind TEXT NOT NULL CHECK (kind IN ('click','adopt','rate')),
  page_id TEXT,
  rating INTEGER CHECK (rating IS NULL OR rating BETWEEN 1 AND 5),
  metadata_json TEXT NOT NULL DEFAULT '{}',
  received_at INTEGER NOT NULL,
  CHECK ((kind = 'rate' AND rating IS NOT NULL) OR
         (kind IN ('click','adopt') AND rating IS NULL)),
  CHECK ((kind IN ('click','adopt') AND page_id IS NOT NULL) OR
         (kind = 'rate')),
  UNIQUE(domain, idempotency_key)
);
CREATE INDEX idx_feedback_log ON feedback_events(domain, log_id);
CREATE INDEX idx_feedback_page ON feedback_events(domain, page_id) WHERE page_id IS NOT NULL;
CREATE INDEX idx_feedback_received ON feedback_events(domain, received_at);

CREATE TABLE review_queue (
  review_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT NOT NULL,
  action TEXT NOT NULL CHECK (action IN ('supplemental_compile','query_template','ignore')),
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
CREATE INDEX idx_review_status ON review_queue(domain, status, created_at);

CREATE TABLE feedback_rejections (
  rejection_id INTEGER PRIMARY KEY AUTOINCREMENT,
  domain TEXT,
  reason TEXT NOT NULL CHECK (reason IN ('payload_too_large','event_count_too_large','field_too_large')),
  payload_bytes INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX idx_feedback_rejections_time ON feedback_rejections(created_at);
```

`metadata_json` 只允许不超过 4 KiB 的对象，MVP 不参与判定；解析失败返回 422。`log_id` 必须存在且 domain 相同；page_id 若提供必须属于该 query 的可接受结果集合（实现可用 `query_json`/`rewritten_json` 中的结果快照；缺快照时按严格存在性失败，不猜测）。外键启用必须在每条连接设置 `PRAGMA foreign_keys=ON`。

## 6. Rust 类型与 trait 契约

以下 `Result` 在 core/feedback 内分别使用现有错误类型或明确的 `FeedbackError`，不得把数据库/JSON/认证错误转成空报告。

```rust
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackKind { Click, Adopt, Rate }

pub struct FeedbackEventInput {
    pub idempotency_key: String, // ASCII/visible, 1..=128 chars
    pub domain: String,
    pub log_id: i64,
    pub kind: FeedbackKind,
    pub page_id: Option<String>,
    pub rating: Option<u8>,
    pub metadata: serde_json::Value,
}
pub struct FeedbackIngested { pub event_id: i64, pub replayed: bool, pub received_at: i64 }

#[async_trait]
pub trait FeedbackStore: Send + Sync {
    fn insert_idempotent(&self, input: &FeedbackEventInput, now: i64)
        -> Result<FeedbackIngested>;
    fn load_window(&self, domain: &str, from: i64, to: i64)
        -> Result<(Vec<QueryLogSnapshot>, Vec<FeedbackEvent>)>;
    fn list_reviews(&self, domain: &str, status: Option<ReviewStatus>, limit: u32)
        -> Result<Vec<ReviewItem>>;
    fn approve_review(&self, review_id: i64, reviewer: &str, now: i64)
        -> Result<ReviewOutcome>;
    fn ignore_review(&self, review_id: i64, reviewer: &str, now: i64)
        -> Result<()>;
}

#[async_trait]
pub trait FeedbackAnalyzer: Send + Sync {
    async fn analyze(&self, input: FeedbackWindow) -> Result<FeedbackReport>;
}
pub struct FeedbackWindow {
    pub domain: String, pub from: i64, pub to: i64,
    pub logs: Vec<QueryLogSnapshot>, pub events: Vec<FeedbackEvent>,
}
pub struct FeedbackReport {
    pub schema_version: String, pub domain: String, pub from: i64, pub to: i64,
    pub zero_recall: Vec<BlindSpotQuery>,
    pub low_quality: Vec<LowQualityPage>,
    pub rewrite_failures: Vec<BlindSpotQuery>,
    pub suggested_reviews: Vec<ReviewSuggestion>,
    pub counts: FeedbackCounts,
}
```

分析器生成确定性 `subject_json`：零召回按 `(domain, normalized query_text)` 去重，保留 `log_ids`、次数、平均 latency；改写失败同样去重；低质量按 page_id，记录 `feedback_count/adopted_count/adoption_rate`。每一建议一次分析批次内去重，插入 review_queue 的唯一键保证重复分析幂等。报告 JSON 必须包含窗口、阈值、输入行数、三类样本、`report_hash`（canonical JSON 的 BLAKE3）。Markdown 报告输出 `step6-feedback-report.md` 与 `.en.md`，双语的数字、ID、hash 相同，`啵啵`、`珍珠` 等领域字面量不翻译。

`FilterRelaxer`：

```rust
pub trait FilterRelaxer: Send + Sync {
    fn relax_once(&self, filters: &Filters) -> Result<Option<Filters>>;
}
```

`QueryEngine` 增加可选 `filter_relaxer: Option<Arc<dyn FilterRelaxer>>`。无过滤或 `relax_once` 返回 None 时不重试；只允许一次。查询日志写入必须返回实际 `log_id`（当前 `log_query` 仅 fire-and-warn，Step 6 改为返回插入 ID；日志写失败是查询结果的 `Err`，不得静默吞掉，因为反馈引用依赖它）。日志的 domain 来自 Query 的 domain，缺省使用 `__default__`，不得继续用 legacy 默认值写新行。

## 7. HTTP API 契约

### 7.1 `POST /feedback`

必需 headers：`Authorization: Bearer <secret>`、`Idempotency-Key: <same as body.idempotency_key>`、`Content-Type: application/json`。两处幂等键必须字节相等；不等返回 422。body：

```json
{
  "domain":"ecommerce",
  "events":[
    {"idempotency_key":"checkout-7-1","log_id":42,"kind":"click","page_id":"drink:boba","metadata":{}},
    {"idempotency_key":"checkout-7-2","log_id":42,"kind":"adopt","page_id":"drink:boba","metadata":{}},
    {"idempotency_key":"checkout-7-3","log_id":42,"kind":"rate","rating":5,"metadata":{}}
  ]
}
```

请求限制：body ≤64 KiB；events 1..=100；每 key 1..=128 Unicode scalar 且不含控制符；domain 1..=128；`metadata` object ≤4 KiB；`log_id > 0`；click/adopt page_id 非空且 ≤512；rate 的 rating 必须存在且 1..=5。一个请求内任一事件失败，整批事务回滚；已存在的重复键可回放，但新旧混合时新事件全部回滚并返回错误，禁止部分成功。

成功 200：

```json
{"domain":"ecommerce","events":[
  {"idempotency_key":"checkout-7-1","event_id":91,"replayed":false,"received_at":1780000000}
],"accepted":3}
```

所有事件均重复时仍为 200；响应 `replayed=true` 并返回原记录时间。服务错误返回 `application/json` `{ "error": {"code":"...","message":"..."} }`，message 不含 secret、SQL、原始 metadata。

错误码：

| HTTP | code | 语义 |
|---:|---|---|
| 400 | `INVALID_JSON` | JSON 语法/顶层形状错误 |
| 401 | `UNAUTHENTICATED` | 缺失或错误 Bearer key |
| 403 | `TENANT_FORBIDDEN` | key 不允许该 domain，或 log/domain 不一致 |
| 409 | 不使用 | 幂等冲突按 D5 回放 200 |
| 413 | `PAYLOAD_TOO_LARGE` | body、事件数或字段预算超限；写 rejection 计数 |
| 422 | `INVALID_FEEDBACK` | 事件枚举、rating、header/body key、log/page 校验失败 |
| 429 | `RATE_LIMITED` | 固定窗口超限，带 Retry-After |
| 500 | `STORE_ERROR` | SQLite/事务错误 |
| 503 | `SERVER_NOT_READY` | 启动迁移或 key 配置未完成 |

### 7.2 健康与指标

`GET /health` 不需要认证，正常返回 200 `{"status":"ok","schema_version":"step6-v1"}`；SQLite 不可用返回 503。`GET /metrics` 为最小 Prometheus text 输出，不暴露 domain、query 文本或 key：`wiktor_feedback_ingested_total`、`wiktor_feedback_replayed_total`、`wiktor_feedback_rejected_total{reason=...}`、`wiktor_feedback_rate_limited_total`、`wiktor_feedback_store_errors_total`、`wiktor_feedback_review_pending`。指标读取只读事务/缓存，不能阻塞写事务；标签集合固定，禁止用户输入作 label。

## 8. CLI 契约与退出码

沿用 Step 5 风格：0 成功（包括空报告、无建议）；1 运行/存储失败；2 参数错误；3 数据、迁移、协议或报告校验错误；4 未分类内部错误。`--json` 时 stdout 只有一份 JSON，日志到 stderr。

```text
wiktor feedback analyze --db <path> --domain <name> --from <unix> --to <unix>
    [--out-dir <dir>] [--min-events <n>] [--json]
wiktor feedback list --db <path> --domain <name> [--status pending|approved|ignored|failed]
    [--limit 1..=1000] [--json]
wiktor feedback review approve --db <path> --review-id <id> --by <operator>
wiktor feedback review ignore  --db <path> --review-id <id> --by <operator>
```

窗口必须 `from <= to`，范围最多 31 天；默认 `--from now-24h --to now`。`analyze` 生成报告并在同一 store 事务写建议；报告文件写临时文件后 rename，失败返回 1。`approve` 只允许 pending；supplemental_compile 要求 subject 含 `entity_id/source_revision/domain_pack_version/source_json/dependencies_json`，并复用 `CompileStore::admit_compile`；缺字段返回 3，不猜造任务。`query_template` 目前只能批准为 `approved` 审计项，不自动写 QUG/配置；`ignore` 写 ignored。

## 9. 并发、事务与端口语义

server 与 CLI 可同时打开同一 SQLite：WAL、busy timeout 5 秒；读窗口使用普通只读事务，反馈插入、审核转换使用 `BEGIN IMMEDIATE`。任何公开 kernel/store 方法只取得连接 mutex 一次，事务结束后释放；不得持锁跨 await。CLI 审核与 server ingestion 竞争时，SQLite 忙/约束错误显式返回 1/500，不能重试到绕过幂等。

server 是唯一 HTTP listener。CLI `feedback analyze/list/review` 直接打开 DB，不连接自身 server，也不绑定端口；若未来 `wiktor serve`，它只调用 `wiktor-server::build_router`。Step6 不引入 server-side compile worker；server 可写反馈与审核建议，读 query_logs；只有 CLI approve 写 `compile_tasks`。

## 10. 验收标准 A1–A18

- **A1**：workspace 含 `wiktor-feedback`、`wiktor-server`；依赖图无 feedback→server、core→server 反向边，`cargo check --workspace` 通过。
- **A2**：0005 在空库和已有 0004 数据库上成功迁移；旧 query log 可读，新列默认值确定；down 对有数据库拒绝破坏性回滚。
- **A3**：反馈 DDL 的 CHECK、FK、UNIQUE 生效；错误 kind/rating/page 组合被拒绝。
- **A4**：相同 `(domain,idempotency_key)` 重发返回 200 replay，event_id/received_at 不变；不同 payload 不覆盖原行。
- **A5**：缺 key、错误 key、domain 越权分别返回 401/403；key 原文不出日志和指标。
- **A6**：64 KiB、100 事件、字段预算边界可接受；超限返回 413、写 rejection 计数且不写 event。
- **A7**：每批事件全成或全回滚；不存在 log、domain 不匹配、page 不在快照均为 422。
- **A8**：固定窗口第 121 个授权请求返回 429 和正确 Retry-After；窗口结束恢复；重启清空计数。
- **A9**：无过滤空候选不触发放宽；有过滤空候选最多调用一次 `FilterRelaxer`，重试后的状态三列准确落库。
- **A10**：滤空且放宽仍失败的日志不进入 zero-recall；真正 `hit_count=0` 且非滤空盲区进入报告；rewrite_failure=1 独立出现。
- **A11**：低质量 page 必须至少 5 个反馈，采纳率严格 `<0.20`；边界等于 0.20 不触发。
- **A12**：分析两次输出相同 canonical JSON/hash；review_queue 不重复插入；数据库错误不返回空报告。
- **A13**：报告同时生成中文 Markdown、英文 Markdown、JSON，ID/数字/hash 相同；超预算报告项按上限返回错误而非截断。
- **A14**：analyze/list/review 参数错误、存储错误、数据协议错误分别映射 2/1/3；`--json` stdout 单对象。
- **A15**：approve pending supplemental_compile 在一个事务内调用既有 admission，成功写 compile_tasks 并回填 task_id；失败保持 pending、无孤儿任务。
- **A16**：approve query_template 不修改领域配置、不写 compile_tasks；ignore 只写审计状态；重复审核拒绝。
- **A17**：health 在 SQLite 可用时 200，不可用时 503；metrics 含固定指标和隔离计数，不含用户可控 label。
- **A18**：server/CLI 并发 smoke 在 WAL 下无脏读、无旧值覆盖；连接 busy/约束/迁移/认证错误均可观察且不静默吞掉。

## 11. 给 wiktor-builder 的实现批次

1. **模型与迁移批次**：加入 workspace crate 壳、0005 DDL、Diesel/raw bind 行映射、错误类型和迁移注册；只实现同步 `FeedbackStore`，验证 A1–A4。
2. **查询日志批次**：实现 `FilterRelaxer`、QueryResult 空候选重试一次、query log 返回 `log_id` 与三列；补默认 relaxer（无规则返回 None）；验证 A9–A10。
3. **分析器批次**：实现窗口读取、三信号聚合、阈值、review suggestion 去重、canonical JSON/hash、双语报告模型；不调用 compile；验证 A10–A13。
4. **审核批次**：实现 review list/approve/ignore；把 supplemental approve 接到现有 `admit_compile`，事务内 CAS/状态校验；验证 A15–A16。
5. **HTTP 批次**：加入 axum 0.7 server state/router、Bearer auth、domain 校验、body limit、固定窗口 limiter、幂等回放、health/metrics；验证 A5–A8、A17。
6. **CLI 批次**：加入 feedback 子命令、窗口/输出目录/JSON、退出码映射；验证 A13–A14。
7. **并发与收口批次**：WAL/busy timeout、server+CLI 同库 smoke、敏感字段日志审计、workspace check；验证 A18，并生成实现偏差记录。

每批必须独立可编译、可测试、可回滚。认证、租户、载荷、迁移、SQL、报告协议错误不得转成“空报告”“无盲区”或“自动忽略”。

## 12. 风险、取舍与偏差记录

固定窗口限流只保护单进程；多副本部署必须在 #7 明确外部 limiter 或 sticky routing，不能把 MVP 内存计数宣称为集群限流。低质量阈值是可解释基线，不代表统计显著性；报告必须同时输出样本数。page 快照可能因旧 query log 缺结果而无法验证，宁可 422 拒收也不把客户端 page 当可信事实。server 写入反馈而 CLI 审核编译的双入口会产生 SQLite 写竞争，WAL+短事务是补偿，不承诺无锁等待。

| ID | 计划/现状偏差 | 原因 | 影响 | 补偿措施 | 是否需回写 MASTER-PLAN |
|---|---|---|---|---|---|
| STEP6-001 | MASTER-PLAN 示意 trait 仅接收 `&[QueryLog]`，本规范改为 query logs + feedback window，并返回 `Result` | 三类信号必须同时读取反馈表，且 DB/JSON 错误不可吞 | 需要新公共类型，不兼容示意签名 | 保留 `FeedbackAnalyzer` 名称，明确失败语义与报告 schema | 否 |
| STEP6-002 | 0001 当前 query log 无 domain、滤空、放宽字段，且空候选立即返回 | Step 3 先实现了快速空结果路径，尚未落实 §5.4 | 旧日志只能按 `__legacy__`，旧空结果不能安全判盲区 | 0005 默认 legacy；Step6 新写入必须完整标记；FilterRelaxer 无规则时显式 None | 否 |
| STEP6-003 | MASTER-PLAN §10 写 `rusqlite`，实际仓库使用 Diesel SQLite | 现行 workspace 与 Step4 已拍板 Diesel | 不得引入第二连接栈 | 复用现有 kernel/连接 mutex/raw bind | 否 |
| STEP6-004 | 目标布局列出 `wiktor-server`，当前 workspace 尚无该 crate 且无 axum | 本步首次真实 HTTP 面 | 新增最小 HTTP 依赖 | axum 0.7、无 tonic、仅三路 endpoint；未来升级需另记偏差 | 否 |
| STEP6-005 | 审核批准后才生成 compile task，分析器不直接 admission | “反馈任务人工审核”与 Step4 状态机需同时满足 | 自动闭环延迟增加 | approve 事务复用 `admit_compile`，保留 source snapshot 与审计 | 否 |
| STEP6-006 | spec §4.1 把契约类型与 store 放在 wiktor-feedback，但 core→feedback 禁止依赖，类型必须被 core 的 SQL 实现引用 | STEP6-003「不引入第二连接栈」的必然结果 | 文件布局与 §4.1 字面不同 | 类型单源定义在 `core/kernel/feedback_store.rs`（impl SqliteKernel），`wiktor-feedback` 全量 re-export 为公开契约面并承载 trait/分析器/报告 | 否 |
| STEP6-007 | §7.1 要求 `Idempotency-Key` header 与 body 幂等键字节相等，但批量 body 每事件各带 idempotency_key，header 无法对应 | spec 内部矛盾（批量语义下 header 不可实现） | API 面少一个 header | 拍板：body 每事件 `idempotency_key` 为唯一幂等源，去掉该 header；幂等语义（D5）不变 | 否 |
| STEP6-008 | §5 要求 page_id 属于「该查询的结果快照」，但 query_logs 从未存结果集，按字面实现所有 click/adopt 恒 422 | spec 依赖了不存在的快照 | 校验口径变化 | 拍板：page_id 须为该 domain 下 accepted 页真实存在（`page_exists` 精确匹配）；log 不存在/log 与 domain 不一致 → 422（§7.1 错误码表 403 行收窄为 key/domain 越权） | 否 |
| STEP6-009 | §7.1「新旧混合时新事件全部回滚并返回错误」与「已存在的重复键可回放」并存时语义含混 | 批处理 + 幂等回放的组合未定义清楚 | 混合批返回语义 | 拍板：单事务内重复键回放、新键插入，全部合法则 200（逐事件 replayed 标记）；任一硬失败整批回滚（禁止部分成功不变） | 否 |
| STEP6-010 | §6 未定 FilterRelaxer 默认规则与 action 映射；`relaxation_succeeded` 口径未定义 | spec 留给实现的细节 | 判定语义细化 | 拍板：DefaultFilterRelaxer 仅放宽 NumericRange（去 max→去 min，区间无约束即移除整条），Text/Ref 条件不放宽；`relaxation_succeeded`=放宽重试后候选域非空（非最终 hits>0）；建议映射：零召回/低质量→supplemental_compile、改写失败→query_template | 否 |
| STEP6-011 | 批4/批6 的 fail-closed 加强与参数化 | 防噪声与可运维性 | 契约面小幅扩展 | approve 仅接受 `Admission::Queued`（Skipped/Rejected → Validation 回滚）；subject 增加实体 domain 与 review 行一致性交叉校验；action=ignore 不可 approve；`--min-events` 参数化 StandardFeedbackAnalyzer（默认=D11 常量 5，范围 1..=1000） | 否 |

实现者发现与本 spec 不一致时必须追加新行，不得静默改变 D1–D12、DDL、阈值、错误码或验收口径。
