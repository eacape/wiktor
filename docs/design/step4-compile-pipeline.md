# Step 4 Spec：LLM 编译管线与可靠发布

> 版本：v1.0（2026-09-22）  
> 上承接：Step 2 两平面内核、Step 3 最小查询闭环  
> 实现对象：`wiktor-builder`；独立验收：`test-engineer`  
> 中文版权威设计；英文版 `step4-compile-pipeline.en.md` 逐节对应。本文定义待实现契约，不表示代码已实现。

## 1. 背景、目标与 MASTER-PLAN 对应

Step 4 实现落地依赖 #4：`DataSource → 增量判定 → Compiler → 机械评分 → 有限重编译 → SQLite 发布`。默认查询仍零 LLM；领域语义只来自配置及源数据。当前权威总纲是 MASTER-PLAN **v3.2**：SQLite/Diesel 承担两平面、FTS、任务；qdrant 是后续派生向量同步目标。不得因旧总纲提到 rusqlite/sqlite-vec 而更换现有内核。

| 总纲位置 | 本步落实 |
|---|---|
| §5.1 四规则 + 可选一致性 | 四维评分、可解释计数、阈值版本、token 账本；consistency=None |
| §5.5 #1/#2/#3 | 全依赖 BLAKE3、持久任务、CAS、租约 fencing、有限重试 |
| §5.5 #4/#5 | 隔离版本不入索引；接受时原子更新页面、评分、FTS、generation |
| §5.5 #6/#7/#8 | 字段允许列表、输出 schema/值校验、体积预算、版本与快照 |
| §6 milk-tea compile 示例、§17 #3/#4 | 读取 quality_threshold=0.75、max_recompiles=2，复用查询闭环验证 |
| §10 技术栈 | async-openai 隔离于 LlmClient，本地模型走 ollama `/v1` |

```text
#2 schema/seed/facts ──→ #3 QueryEngine/FTS/QUG fallback
          └───────────→ #4 compilation/quality/publish
                              ├──→ accepted pages/sections/FTS → #3
                              ├──→ persisted edge payloads → #5 QUG construction
                              └──→ published generation/hash → later vector sync
#4 does not depend on #5 or a live vector/model service for offline acceptance.
```

范围外：LLM 抽边、QUG 全图重建/热替换、向量生成与 qdrant 写入、一致性仲裁、反馈 API、人工审核 UI、日预算自动调参。已有种子页及 golden 查询保留。机械规则证明引用存在及证据绑定，不证明任意自然语言的语义蕴含；本步使用受约束的抽取式断言，把可机械验收的范围写清楚。

## 2. 决策记录 D1–D8

| # | 决策 | 理由与精确边界 |
|---|---|---|
| D1 | `LlmClient` 隔离 async-openai；`LlmCompiler` 实现既有 Compiler | 供应商 DTO 不越过适配器。每次 compile 最多一次模型请求；重试只由 executor 调度。显式 MockCompiler 无 key、无网络跑相同校验器。 |
| D2 | 拉批默认 32，逐实体排队/领取/发布，单 worker 默认 | 网络不占数据库锁；短事务隔离单页失败。新增 kernel 专用事务方法，禁止持锁调用 execute_batch/seed_pages/upsert_facts。 |
| D3 | JSON envelope + 逐断言 `[[ref:rN]]` + section refs | 引用定位到实体、revision、JSON Pointer、原值与引文；正文必须由契约中的断言确定性重建。禁止“引用存在但不在正文”的假覆盖。 |
| D4 | 四维权重各 0.25；独立硬门槛 | 保持 QualityScore API 不变。overall≥领域阈值且 coverage≥0.60、density≥0.40、schema=1、citation=1 才接受。 |
| D5 | 保留 SQL 状态 pending/running/succeeded/failed/dead；业务状态映射 | 页级最多 1+2 个质量候选，任务失败最多 3 次；持久预算预留、租约 fencing 防止重启和并发绕过刹车。 |
| D6 | 原三元 UNIQUE 不变，行内增加 desired_hash 与 epoch | 换模型/Prompt 必须可重编译，但不能靠加 hash 到 UNIQUE 来绕过幂等。全依赖哈希使用递归 canonical JSON + 有序带长度域。 |
| D7 | `wiktor compile` 显式选择 source/provider，dry-run 完全只读 | 无 key 不偷偷降级 Mock。统计区分最终结果、跳过与预算待处理；退出码可供脚本判断。 |
| D8 | accepted 发布只写 SQLite；QUG 边载荷可为空 | 用 FTS 验证本步检索；向量与 QUG 构建后续消费 generation/hash。不得把旧 seed 查询图宣称为新编译图。 |

## 3. 架构与模块/类型契约

```text
DataSource.fetch(Cursor) → validate/project → hash → admit + facts CAS
                                                │
                                          compile_tasks
                                                ↓ claim + reserve
                                      Compiler.compile(raw, ctx)
                                      ├─ LlmCompiler → LlmClient
                                      └─ MockCompiler
                                                ↓
                                     validate evidence + RuleScorer
                                         ↓                 ↓
                                  accept transaction    attempt quarantine
                                  pages + sections      retry pending / dead
                                  quality + edges       prior accepted retained
                                  FTS + generation
```

仅在 core 内新增 `compile/{mod,config,hash,contract,quality,llm,mock,store}.rs`；CLI 新增 `compile.rs`。store 定义 DTO，数据库实现放 `kernel/sqlite.rs` 或其子模块，访问同一个私有连接。不建空壳 crate。复用 blake3、serde/serde_json/serde_yaml_ng、async-trait、uuid、pulldown-cmark、gray_matter、Diesel、tokio、tracing。新增可选 `async-openai = "0.28"`（关闭默认 feature、开启 rustls）；新增 `llm-openai` core feature，CLI 同名 feature 转发并默认开启。builder 必须用 Rust 1.85 验证该版本及锁定的传递依赖；不引入 rig、rusqlite 或独立 HTTP 重试库。只有适配器确需配置 HTTP client 时才直接依赖与 SDK 同版 reqwest，避免双 HTTP 栈。

下列签名中的 `Result` 默认是现有 `types::Result`；省略 derive/import 和方法体，不省略状态语义。

```rust
pub struct PipelineExecutor {
    pub kernel: Arc<SqliteKernel>,
    pub compiler: Arc<dyn Compiler>,
    pub scorer: Arc<dyn RuleScorer>,
    pub validator: Arc<dyn SourceRefValidator>,
    pub clock: Arc<dyn Clock>,
    pub policy: CompilePolicy,
}
impl PipelineExecutor {
    pub async fn run(&self, source: &dyn DataSource,
        ctx: &CompileContext, options: RunOptions) -> Result<CompileStats>;
}
pub trait Clock: Send + Sync { fn unix_seconds(&self) -> i64; }
pub struct RunOptions {
    pub limit: usize, pub batch_size: usize,
    pub force: bool, pub dry_run: bool,
}
pub struct CompileStats {
    pub run_id: String, pub scanned: u64, pub accepted: u64,
    pub quarantined: u64, pub failed: u64, pub skipped: u64,
    pub deferred: u64, pub would_compile: u64, pub attempts: u64,
    pub reserved_tokens: u64, pub reported_tokens: u64,
    pub circuit_open: bool, pub dry_run: bool,
}
pub struct CompilePolicy {
    pub compiler_version: String, pub artifact_version: String,
    pub scorer_version: String, pub knowledge_fields: Vec<String>,
    pub sensitive_fields: Vec<String>, pub required_headings: Vec<String>,
    pub min_coverage: f32, pub min_density: f32,
    pub max_recompiles: u32, pub max_retries: u32,
    pub task_token_budget: u64, pub batch_token_budget: u64,
    pub daily_token_budget: Option<u64>, pub max_output_tokens: u32,
    pub lease_seconds: u32, pub heartbeat_seconds: u32,
}
pub struct TaskLease {
    pub task_id: i64, pub epoch: i64, pub lease_token: String,
    pub desired_hash: String, pub attempt_no: u32,
    pub source: RawEntity, pub context: CompileContext,
}
pub enum Admission { Queued(i64), Skipped, Deferred, Rejected(String) }
pub enum CommitOutcome { Accepted { generation: i64 }, Stale }
pub enum FailureDisposition { RetryAt(i64), Quarantined, Failed }
```

### 3.1 配置、两平面与输入身份

保留 DomainConfig 的已公开 quality_threshold/max_recompiles 字段；扩展内部 CompileSection 并增加 `compile_policy` 配置字段。缺省 compile 段必须实际产生阈值 0.75、max_recompiles=2，修正当前 derive(Default) 导致缺段取零的行为。compile 新字段严格拒绝拼写错误，其他既有段维持兼容。

```yaml
compile:
  quality_threshold: 0.75
  max_recompiles: 2
  prompt: prompts/compile.md          # 可省略，使用内置 source-ref-v1 模板
  output_contract: require_source_refs
  knowledge_fields: [name, description]
  sensitive_fields: []
  required_headings: [概述]
  scorer_version: rules-v1
  min_coverage: 0.60
  min_density: 0.40
  max_retries: 3
  task_token_budget: 65536
  batch_token_budget: 262144
  daily_token_budget: null
  max_output_tokens: 2048
```

新增配置均有上述默认值；knowledge_fields 缺省仅选 schema 中 `field_type=text && !filterable` 的字段。显式列表必须命中 schema、无重复，不得包含 filterable、numeric、boolean、timestamp 或 sensitive 字段；可显式加入非 filterable reflist。高频字段不得因 `filterable=false` 被误纳入，例如 on_sale:boolean 排除。密度/覆盖度/总阈值必须有限且在 [0,1]；max_recompiles 在 0..=10，max_retries 在 1..=100，token 预算非零，max_output_tokens 在 1..=8192。required_headings 非空、唯一，默认 `[概述]`，这些标题是领域展示字符串。配置与模板在 run 启动时冻结并持久化；恢复任务不读取中途被改写的模板。

`PreparedSource { full: RawEntity, knowledge: RawEntity, facts: Facts, snapshot_hash: String }`：full 在本地校验；knowledge.fields 仅含允许字段；facts 使用现有 raw_to_facts 转换，再保留所有未选为知识的已声明字段（敏感字段允许本地存储但不进入任务/日志）。两平面字段需双用途时由领域显式建两份字段。source schema 的必填及类型规则沿用 raw_to_facts。revision 限制 1..=i64::MAX，不能用 `as i64` 溢出；JSONL 缺 revision 兼容默认 1，但提供了非法/负数/非整数 revision 必须报错，不能静默退为 1。

每个源实体编译一页，`wiki.entity_id=raw.id`，`page_id=raw.id.to_key()`；不由 LLM 发明 ID，不把多个 SKU 按 category 任取一条覆盖同页。现有 products.jsonl 是 SKU 源，编译后为 product 页；现有带过滤 golden 指向 drink 页，仍使用 seed 页验证，不声称 SKU 编译替代 drink 聚合。builder 增加小型 `compile-entities.jsonl` fixture（drink 实体 + name/description/aliases 等明确知识字段及 schema），用同一个 drink ID 与事实 category 锚点验证新编译页的过滤检索。一般化分组聚合属于未来领域适配器，不内置奶茶逻辑。

fetch 从 `Some(Cursor { offset:0, batch_size:32 })` 开始，每次按实际返回数递增 offset，空 Vec 结束；limit 默认 1000、上限 10000，batch_size 1..=128，最后一批使用 min(剩余 limit,batch_size)。超过请求数量的 adapter 返回视为协议错误。每个知识输入 canonical bytes≤64 KiB，单次 fetch 总输入≤8 MiB；JSONL adapter 后续实现改用有界逐行读取，行≤256 KiB，禁止当前 read_to_string 整文件无界读取。任务只持久化知识快照与依赖，不复制敏感 full 数据。完整输入审计 hash 不记录原文。

## 4. D1：模型抽象、错误与离线运行

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: LlmRequest)
        -> std::result::Result<LlmResponse, CompileFailure>;
}
pub struct LlmRequest {
    pub system: String, pub input_json: String, pub model: String,
    pub max_output_tokens: u32, pub timeout_seconds: u32,
}
pub struct TokenUsage { pub input: u64, pub output: u64 }
pub struct LlmResponse { pub json: String, pub usage: Option<TokenUsage> }
pub struct LlmCompiler { pub client: Arc<dyn LlmClient>, pub policy: CompilePolicy }
pub struct MockCompiler { pub policy: CompilePolicy }
pub enum CompileFailure {
    Retryable { code: String, retry_after_seconds: Option<u32> },
    Permanent { code: String },
    InvalidOutput { code: String, response_prefix: String },
}
// Add a typed variant to existing Error; retain Compilation(String) compatibility.
// Error::CompileFailure(CompileFailure)
```

Compiler.compile 签名不变。为避免丢失 refs/usage，**向 CompiledPage 增加** `#[serde(default)] pub evidence: Option<CompileEvidence>`；旧 seed 的 None 仍可解析，但通过 PipelineExecutor 发布时 None 视为 schema 失败。不得用全局 side channel、编码在 title 或猜测 Markdown 的方式传递证据。CompileContext 不增加 attempt 字段；重试复用同一 ctx，输出契约包含通用自检查指导。模型返回的 quality、content_hash、metadata 不可信，由 executor 重算/填写；Compiler 的旧 quality 字段只是占位，不能短路评分。

OpenAI 适配器采用 Chat Completions、非流式、temperature=0、JSON object 输出，不依赖供应商 JSON-schema 强制能力；本地 serde 校验始终执行。SDK 内部自动重试关闭；不能关闭则必须在适配器配置层实现单次请求语义后才验收。超时 60s，响应正文≤128 KiB；禁止 tools、联网检索和自动跟随输出指令。远端默认 `https://api.openai.com/v1`，key 只从 `WIKTOR_OPENAI_API_KEY`；ollama 默认 `http://127.0.0.1:11434/v1`，不需要用户 key。日志不含 key、原始字段和完整模型响应。

408/429/5xx、连接重置、超时可重试；400/401/403/404/422 等其他 4xx、TLS 证书错误永久失败。Retry-After 秒或 HTTP-date 转为延迟并限制 0..=300s；无值用 `min(2^(retry_count-1),60)` 秒。不加随机抖动，便于此单 worker MVP 的可重复验收。JSON/契约错误属于质量候选失败，可消耗重编译次数；认证/服务错误不伪装为低质量。旧 Error::Compilation(String) 默认永久错误，禁止通过解析错误字符串推断 HTTP 状态。

MockCompiler 确定性从允许字段输出同样的 envelope、refs、正文，usage=None，走相同 validator/scorer/hash/store；注入脚本式测试 Compiler 可返回低质候选/超时/成功。生产 provider 缺 key 是配置错误，必须显式 `--provider mock` 才离线。Mock token 也按同样预留账本计量，不能绕过刹车。

## 5. D3：Prompt 输出契约与来源验证

### 5.1 Envelope schema v1

所有对象 serde `deny_unknown_fields`，所有列出字段必填；只允许两个互斥分支。LLM 成功输出如下（领域字面量保持原样）。

```json
{
  "schema_version":"source-ref-v1",
  "status":"ok",
  "wiki":{
    "title":"啵啵",
    "aliases":[],
    "tags":[],
    "markdown":"## 概述\n\n- 啵啵[[ref:r1]]\n- 珍珠[[ref:r2]]\n"
  },
  "sections":[{
    "heading":"概述",
    "assertions":[
      {"text":"啵啵","ref_ids":["r1"]},
      {"text":"珍珠","ref_ids":["r2"]}
    ],
    "refs":[
      {"id":"r1","entity_id":"milk-tea:drink:boba","source_revision":1,"pointer":"/fields/name","value":"啵啵","quote":"啵啵"},
      {"id":"r2","entity_id":"milk-tea:drink:boba","source_revision":1,"pointer":"/fields/description","value":"珍珠","quote":"珍珠"}
    ]
  }]
}
```

```json
{"schema_version":"source-ref-v1","status":"error","error":{"code":"MISSING_SOURCE_REFS","missing_pointers":["/fields/description"]}}
```

错误 code 枚举 `MISSING_SOURCE_REFS|INSUFFICIENT_SOURCE|UNSUPPORTED_SOURCE`，missing_pointers 为字符串列表；错误分支没有 wiki/sections。无模型输出时 error 分支不是伪造 accepted 页。

```rust
pub struct OutputWiki {
    pub title: String, pub aliases: Vec<String>, pub tags: Vec<String>,
    pub markdown: String,
}
pub struct Assertion { pub text: String, pub ref_ids: Vec<String> }
pub struct SourceRef {
    pub id: String, pub entity_id: String, pub source_revision: u64,
    pub pointer: String, pub value: serde_json::Value, pub quote: String,
}
pub struct EvidenceSection {
    pub heading: String, pub assertions: Vec<Assertion>, pub refs: Vec<SourceRef>,
}
pub struct CompileEvidence {
    pub schema_version: String, pub wiki: OutputWiki,
    pub sections: Vec<EvidenceSection>, pub usage: Option<TokenUsage>,
}
pub struct RefReport {
    pub assertions: u32, pub supported_assertions: u32,
    pub ref_occurrences: u32, pub valid_ref_occurrences: u32,
    pub covered_units: BTreeSet<String>,
    pub information_chars: u64, pub total_chars: u64,
    pub issues: Vec<QualityIssue>,
}
pub struct QualityIssue { pub code: String, pub path: String }
pub trait SourceRefValidator: Send + Sync {
    fn validate(&self, source: &RawEntity, evidence: &CompileEvidence,
        require_refs: bool) -> RefReport;
}
pub fn decode_response(json: &str) -> std::result::Result<CompileEvidence, CompileFailure>;
```

### 5.2 机械算法与防伪边界

1. 严格解码完整 JSON，拒绝 markdown fence、前后散文、重复 JSON object key、未知字段与不支持版本。serde 默认覆盖重复 map key 的路径必须用重复键检测 visitor 封住。大小先于解析检查。标题非空≤128 Unicode scalar，aliases/tags 各≤32 项，每项非空≤128 scalar；sections 为 1..=32，每节 1..=64 断言，页合计≤256，refs 页合计≤512。heading 唯一、匹配 required_headings 集合（可额外 section，但标题必须出自领域配置扩展列表）。
2. source 指传给 Compiler 的 **knowledge 快照**。指针使用 RFC6901，从 `{id,fields,source_revision}` 根解析，只允许 `/fields/<allowed>` 下的 string leaf；reflist 必须指到索引元素，不能整数组引用。ref.id 匹配 `r[1-9][0-9]{0,5}`，全页唯一；entity_id/revision 必须精确匹配快照，不能引用其他实体或“最新 revision”。value 的类型和值须等于 pointer 解出的值；quote 非空且为该 string 原值的连续精确子串，不做繁简/大小写/空白归一化。
3. 文本抽取式约束：每条断言 text 非空、单行≤1024 scalar，不含 Markdown 控制结构、HTML、`[[`/`]]`。当有 refs 时，text 必须等于按 ref_ids 顺序以单个空格连接的 quote；所有 ref_ids 都必须引用本节 refs，且每条 refs 至少使用一次。此约束刻意不接受带来源但自由改写的推论。标题、aliases、tags 必须精确出现在至少一条有效 quote 中；其出现不额外计入 coverage。标题是纯文本，不允许换行/控制字符。
4. Canonical Markdown 每节固定 `## {heading}\n\n`，每断言固定 `- {text}{markers}\n`，markers 按 ref_ids 顺序连接 `[[ref:rN]]`，节间加一个空行。对 `wiki.markdown` 原样扫描 markers 并与断言映射比较，再与 canonical renderer 字节比对；额外散文、代码块、HTML、伪 refs、不受审计的段落均不能通过 schema。复用 pulldown-cmark 确认结构和正文可见文本，marker 解析用固定字面语法，不写泛化 Markdown parser。
5. `WikiPage.content` 保留 canonical Markdown 标记；sections 用既有 H2 切章语义（content 含 H2），由 renderer/共享 splitter 生成而非信任 Compiler 填值。evidence 是完整验证载荷，frontmatter 单独保存，引用清单也可经 frontmatter 无损导出。空源、空正文不做除零，不补造事实。
6. require_source_refs=true 时，无引用断言或任意虚构/不匹配/悬空/未使用 ref 都硬拒绝接受，记录具体 JSON path 与 code。false 只放松 refs 非空及无引用断言的抽取式要求；仍强制 envelope/schema，存在的 refs 仍全量校验；citation<1 的页仍 quarantine（总纲发布契约），false 不是发布绕过开关。

`MISSING_REF|UNKNOWN_REF|UNUSED_REF|SOURCE_ID_MISMATCH|REVISION_MISMATCH|POINTER_MISSING|VALUE_MISMATCH|QUOTE_MISMATCH|ASSERTION_UNSUPPORTED|MARKDOWN_MISMATCH` 是稳定诊断 code。validator 不访问网络或当前 facts；回放必须基于原始知识快照。不推断“啵啵=珍珠”：只有源已明确给出该知识才可编译，跨页一致性后补。

## 6. D4：四规则评分、归一化与发布门槛

```rust
pub struct ScoreReport {
    pub quality: QualityScore, pub issues: Vec<QualityIssue>,
    pub accepted: bool,
}
pub trait RuleScorer: Send + Sync {
    fn score(&self, source: &RawEntity, page: Option<&CompiledPage>,
        refs: &RefReport, schema_valid: bool,
        ctx: &CompileContext, policy: &CompilePolicy) -> ScoreReport;
}
```

executor 在所有 Compiler 实现之后运行 validator 和 scorer。无法解码时 page=None/schema_valid=false，仍调用 scorer 生成可观测零分；可解析但缺 refs 时记录部分分数。自定义 scorer 不能绕过最终 publisher 硬门槛；NaN/Inf、越界分数返回内部错误，禁止 clamp 隐藏插件 bug。

设 U 为知识快照中非空 string leaf 指针集合（string 字段一项，reflist 每个非空元素一项）；空字符串/null/空数组不作可覆盖信息，类型非法输入先拒绝。C 为被**有效且已使用且断言支持成功**的引用命中的 U 子集。A 为断言数，S 为全部引用有效且 text 满足抽取式约束的断言数。R 为正文 marker 引用次数加未使用 ref 定义数量，V 为有效 marker 引用次数。

| 维度 | 公式与边界 | 门槛 |
|---|---|---|
| coverage | `|C|/|U|`；U 为空取 0；去重指针，重复引用不涨分 | ≥配置 min_coverage，默认 0.60 |
| citation | `min(S/A,V/R)`；A=0 或 R=0 取 0；未使用定义进入分母且硬拒绝 | 必须 1，且无 ref issue |
| schema_compliance | 全部 wire schema、renderer、身份/元数据/sections 一致性检查通过为 1，否则 0 | 必须 1 |
| density | `I/T`；T=0 取 0。T 为全部 assertion text 的非空白 Unicode scalar 数；I 为每个唯一 `(pointer,quote)` 首次受支持使用的 quote 字符数（忽略空白），同一断言内重复对只算一次；重叠 quote 的源字符位置取并集，再跨断言去重；I 上限 T | ≥配置 min_density，默认 0.40 |

quote 在同一 source string 多次出现时选最左匹配，字符位置按 Unicode scalar 计。heading、marker、JSON/frontmatter 元数据不进入 T。该 density 是版本化的字符级近似，不叫模型 tokenizer 结果；重复抄写、长篇无引用填充降低密度，无法识别源自身的废话，需人工标注校准。

`overall=(coverage+citation+schema_compliance+density)/4`，四权重均 0.25；consistency=None，SQL 为 NULL，不参与 overall。接受须所有硬门槛 AND `quality.passes_threshold(ctx.quality_threshold)`；例如 (0.6,1,1,0.4) 的 overall=0.75 正好通过，coverage=0.59 即使 overall>0.75 仍拒绝。所有比值以 f64 中间计算后转 f32，比较使用现有 f32 API、无额外 epsilon。

空知识源直接 quarantine，不请求 LLM；空/超长输出、结构失败不截断后接受，四维全零（consistency=None），保存受限诊断。schema 合规但引用失败可保留 coverage/density 的有效部分便于解释。将规则版本、阈值与模型版本保存到任务依赖及 frontmatter；变更需新 hash。golden 是检索回归集，不能代替 50–100 页人工质量标注；相关性>0.7 与全集真实费用需后续真实模型校准，不能由 Mock 验收冒充。

## 7. D6：全依赖哈希、幂等与输入顺序

`content_hash` 为小写 64 位 BLAKE3 hex；seed 的 title/body hash 不满足本步哈希版本，首次编译不可据此 skip。

```rust
pub struct HashDependencies<'a> {
    pub source: &'a RawEntity, // projected knowledge source
    pub context: &'a CompileContext,
    pub policy: &'a CompilePolicy,
    pub source_schema: &'a EntitySchema,
}
pub fn content_hash(input: HashDependencies<'_>) -> Result<String>;
pub fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>>;
```

固定前缀 `wiktor.compile.hash.v1\0`；依次写每域 UTF-8 域名、u64 little-endian 长度、域 bytes，域名本身也以前置 u64 长度编码，禁止裸拼接歧义。顺序固定：`source`、`domain_pack_version`、`prompt_template`、`compiler_version`、`model_version`、`embedding_model`、`artifact_version`、`scorer_version`、`quality_policy`、`knowledge_schema`。source 为 canonical `{entity_id:to_key(),fields:knowledge.fields}`；source_revision 是 CAS/引用快照身份，**不纳入语义 hash**。quality_policy 包含 threshold、require_source_refs、min_coverage/min_density、知识/敏感字段列表、required_headings、生成参数（temperature/max_output_tokens）；预算/lease/时钟/usage/重试次数不入 hash。

对象 key 递归按 UTF-8 字节排序，数组严格保留顺序；字符串不 trim、不 Unicode 归一化；数值用 serde_json::Number 的稳定序列化（1 与 1.0 可不同，拒绝非有限值）。序列化实现版本固定为 hash.v1，serde_json 升级需黄金 hash 回归；无需声称 RFC8785。Prompt 按实际完整模板 bytes（含内置系统约束/页面模板）计算，路径字符串和 API key/base URL 不入 hash；模型标识必须是操作人可管理的固定版本，移动 tag 更新但名字不变无法自动检测，须更新 model_version。

完整源快照另算 `snapshot_hash=BLAKE3(canonical(full RawEntity))`，供**同 revision 不同内容**冲突检测，原始字段不进入 Prompt。知识 projection 后 hash 覆盖全部编译输入；price/stock 只改变 snapshot_hash，不改变 content_hash。这是对“源数据序列化”在两平面边界的明确解释，避免事实更新导致模型成本。

admit 在一个事务内执行：

1. 比较实体 source head：低 revision → skipped(stale_source)，不写 facts；同 revision 且 snapshot_hash 不同 → rejected(source_revision_conflict)，不自动采信。高 revision 更新 head 并以事实 CAS 写 facts；同 revision 同 snapshot 可幂等重放。
2. desired_hash 与已接受页 hash/版本一致且非 force → skipped；如果输入 revision 更新，记录本任务 succeeded/result=skipped，保留已接受页及其**原 revision 证据**、generation 不变。不能把旧引用改称新 revision 引用。
3. 三元 `(entity_id,source_revision,domain_pack_version)` INSERT ON CONFLICT 查询现有任务。相同 hash 的 pending/running 合并不重置计数；相同 hash 的 dead/failed 不自动重启，计入现有 terminal 结果；succeeded 且页确实匹配才 skip。desired hash 改变则原行 epoch+1，替换知识及依赖快照，计数归零；旧 attempts 保留。force 同 hash 也 epoch+1，明确人工重新运行，预算仍生效。
4. source_heads 保存 latest desired task_id/epoch/hash（配置更新以最后一次成功 admission 为准），提交时必须仍匹配。新 revision、新配置 admission 即 fence 旧 worker；并非等旧 worker 提交时才比较。相同 hash pending 的重复 admission 不能增加 epoch。

通过增加 hash 到 UNIQUE 会允许同源并发发布不同模型产物，故本步明确禁止。page CAS 同时检查最新 source head 与租约 tuple；不按 semver 字符串排序判断谁新。跨 revision 的知识复用只复用已接受版本；隔离版本从不当缓存命中。

## 8. D2/D5：SQLite 迁移、事务与状态机

### 8.1 必需迁移 0003_compile_pipeline

不改 0001/0002；新增 up/down migration、更新 db_schema。以下为列级 DDL 合同，TEXT JSON 均 canonical UTF-8，有界；所有计数/时间 INTEGER 非负，revision/epoch 从 1 起。FK 开启，所有新 FK 删除策略明确如下。

| 表 | 新增/定义 |
|---|---|
| pages | `source_revision INTEGER NOT NULL DEFAULT 0`（0=legacy）；`artifact_version TEXT NOT NULL DEFAULT 'seed-v1'`；`frontmatter_json TEXT NOT NULL DEFAULT '{}'`。保留 aliases/tags、完整 refs、质量策略和 metadata，body 不混入 frontmatter。 |
| compile_tasks | 保留三元 UNIQUE/status CHECK；新增 `desired_hash TEXT NOT NULL DEFAULT ''`、`epoch INTEGER NOT NULL DEFAULT 1`、`source_json TEXT NOT NULL DEFAULT '{}'`、`dependencies_json TEXT NOT NULL DEFAULT '{}'`、`snapshot_hash TEXT NOT NULL DEFAULT ''`、`recompile_count INTEGER NOT NULL DEFAULT 0`、`attempt_count INTEGER NOT NULL DEFAULT 0`、`lease_token TEXT`、`next_attempt_at INTEGER NOT NULL DEFAULT 0`、`result TEXT`、`reserved_tokens INTEGER NOT NULL DEFAULT 0`、`task_token_budget INTEGER NOT NULL DEFAULT 65536`。result CHECK NULL 或 accepted/quarantined/failed/skipped/superseded。 |
| compile_source_heads | `entity_id TEXT PRIMARY KEY, source_revision INTEGER NOT NULL, snapshot_hash TEXT NOT NULL, desired_hash TEXT NOT NULL, task_id INTEGER REFERENCES compile_tasks(task_id) ON DELETE RESTRICT, epoch INTEGER NOT NULL, updated_at INTEGER NOT NULL`。head 只保存身份，不保存敏感原文。 |
| compile_attempts | `task_id INTEGER REFERENCES compile_tasks ON DELETE RESTRICT, epoch INTEGER, attempt_no INTEGER, lease_token TEXT NOT NULL, status TEXT NOT NULL, publish_status TEXT, artifact_json TEXT, quality_json TEXT, issues_json TEXT NOT NULL, reserved_tokens INTEGER NOT NULL, reported_tokens INTEGER, error_code TEXT, created_at INTEGER NOT NULL, finished_at INTEGER`；PK(task_id,epoch,attempt_no)。status=reserved/completed/abandoned；publish_status=NULL/candidate/accepted/quarantined。artifact_json 是完整 CompiledPage（含 evidence），仅本地、≤256 KiB。 |
| qug_edges | `page_id TEXT REFERENCES pages ON DELETE CASCADE, edge_hash TEXT, edge_json TEXT NOT NULL, generation INTEGER NOT NULL, content_hash TEXT NOT NULL`；PK(page_id,edge_hash)。仅接受页拥有这些边。 |
| compile_runs | `run_id TEXT PRIMARY KEY, token_limit INTEGER NOT NULL, reserved_tokens INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL`。一个 CLI run 对应全局批预算，不随 fetch 清零。 |
| compile_daily_budget | `utc_day INTEGER PRIMARY KEY, token_limit INTEGER NOT NULL, reserved_tokens INTEGER NOT NULL DEFAULT 0`；utc_day=floor(unix_seconds/86400)。仅启用日限额时创建日行。 |

compile_attempts 额外存 `run_id TEXT REFERENCES compile_runs ON DELETE RESTRICT, utc_day INTEGER`，便于审计预留归属；reported_tokens 为 total，input/output 明细放 evidence.usage。task/epoch/attempt 所有更新须绑定 lease_token。新增 pending 调度索引 `(status,next_attempt_at,task_id)`；保留 running 租约索引。旧任务没有快照无法恢复，迁移将它们标记 dead/result=failed/error_message=legacy_task_missing_snapshot 并清租约；合法新 admission 可以补齐快照并 epoch+1。迁移恢复旧数据页、质量、sections，不改正文。

FTS 触发器必须迁移为**仅 accepted 入索引**：INSERT 用 `INSERT ... SELECT ... WHERE NEW.status='accepted'`；UPDATE 无条件删 OLD.page_id 的 FTS 行，再条件插 NEW；DELETE 删 OLD。迁移回填前清 FTS，再从 accepted pages 回填。trigram 不变。legacy generation=1 但 generations 空时插入 reserved sentinel generation=1/domain_pack='__legacy__'/version='seed'/status=published，避免新 generation 与存量 1 碰撞；后续每接受一页分配一个全局递增 generation，不重用 1。down 在存在 Step4 attempts 时拒绝破坏性降级，空新表时才允许回退；不静默丢审计记录。

### 8.2 Kernel 方法及锁

```rust
impl SqliteKernel {
    pub fn admit_compile(&self, prepared: &PreparedSource,
        ctx: &CompileContext, policy: &CompilePolicy, force: bool) -> Result<Admission>;
    pub fn claim_compile(&self, task_ids: &[i64], run_id: &str,
        now: i64) -> Result<Option<TaskLease>>;
    pub fn heartbeat_compile(&self, lease: &TaskLease, now: i64) -> Result<bool>;
    pub fn recover_compile_leases(&self, now: i64) -> Result<u64>;
    pub fn publish_compile(&self, lease: &TaskLease, page: &CompiledPage,
        report: &ScoreReport, now: i64) -> Result<CommitOutcome>;
    pub fn finish_compile_failure(&self, lease: &TaskLease,
        failure: &CompileFailure, candidate: Option<&CompiledPage>,
        report: &ScoreReport, now: i64) -> Result<FailureDisposition>;
    pub fn load_accepted_pages(&self, domain: &str) -> Result<Vec<CompiledPage>>;
}
```

每个方法取 conn Mutex **一次**，用 `SqliteConnection::immediate_transaction` 管理 BEGIN IMMEDIATE/COMMIT/ROLLBACK；内部辅助函数只接 `&mut SqliteConnection`。不得把 user/model 数据插值拼接给 execute_batch；DDL 可 batch_execute，DML 用 Diesel bind。不得持 guard 跨 await，也不得在事务中调用会再次取锁的任何 public kernel 方法。同步数据库操作由 executor 用 spawn_blocking 执行；不把 guard 返回异步层。锁 poison 返回 Error::Internal，不新增 unwrap。busy_timeout=5000ms；SQLite busy 可在数据库操作边界有限重试 3 次，不重发已经完成的模型请求。

admission 事务先做事实 CAS（包括 fact_refs 仅 CAS 成功才替换）再排队，允许知识滞后。publish 事务再次校验 lease 与 source head、检查 facts 无倒退，不回放陈旧 facts；写入的是“当前事实 + 最近已发布 Wiki”，与总纲同一 SQLite 原子边界一致，不能拿模型等待前的事实覆盖并发 ETL。

接受事务：验证 tuple → 插 generations building 得到 g → upsert pages accepted/hash/frontmatter/source_revision/g → 删除并替换 sections (`page_id#index`) → upsert page_quality（实际评分、overall）→ 删除替换该页 qug_edges → trigger 同步 FTS → generations published → attempt completed/accepted → task succeeded/result=accepted，清 lease → commit。任一步失败整事务 rollback；generation、hash、task 不能提前成功。幂等重放已成功 lease 返回原结果，不额外分配 generation。

低质量事务：attempt completed/quarantined + artifact/quality/issues，更新 task 计数与重试状态；不动 pages/page_quality/sections/qug_edges/generations。因此失败新版本不抹掉上一代 accepted；没有上一代则查询无页。人工队列为 dead/result=quarantined 的任务 join 最新隔离 attempt；candidate/quarantine 生命周期保存在 attempts，accepted head 保存在 pages。不得为审计复用 accepted page_id 写 quarantined 覆盖旧正文。

### 8.3 状态、重试与租约

| 业务状态 | SQL status/result | 转移 |
|---|---|---|
| pending | pending/NULL | 有预算且到 next_attempt_at 时领取 |
| compiling | running/NULL | 心跳保活；成功 accepted；失败按下述分类 |
| accepted | succeeded/accepted | terminal；仅新 hash 或 force 重排 |
| quarantined | dead/quarantined | terminal 人工队列；无自动重试 |
| failed | failed/failed 或 dead/failed | 永久错误或传输重试耗尽；无自动重试 |
| superseded/skipped | succeeded/superseded 或 succeeded/skipped | 不发布，不额外产生 generation |

现有 SQL_CLAIM_NEXT **不能原样使用**：它在领取时加 retry_count 且无上限/所有权。替换该常量为参数化领取 SQL（保留名称），只查当前 run 已 admission 的 task_ids、pending、due、retry_count<max_retries，顺序 `(next_attempt_at,task_id)`；expired running 必须先经 recover。claim 在同一事务内重新检查预算、状态与 source head，生成 UUID lease_token，attempt_count+1，插 reserved attempt，置 running/lease_expires_at=now+300。领取不增加 retry_count。最终 UPDATE WHERE pending AND epoch=expected，affected rows=1 才有 lease。

heartbeat 默认每 30s 延长至 now+300；WHERE task_id/epoch/token/status=running 且 lease_expires_at>now。所有完成/发布也要求同样未过期租约。失败匹配 0 行意味着失去所有权，只返回 stale，不能改现任 worker 的状态/计数。模型请求仍可能被供应商完成，因此“恰好一次付费调用”不承诺，预算按每次预留保守计算。

retry_count 是当前 epoch 的**失败次数**，每个失败 attempt（质量、传输、超时回收）只加一次，成功不加；max_retries=3 表示达到 3 次失败即停止，不是额外 3 次重试。recompile_count 是已失败的质量候选数；首次差输出后=1，可再尝试，第三个差输出后=3>max_recompiles=2，dead/quarantined；max_recompiles=0 首个差输出即 terminal。传输错误只加 retry_count，不加 recompile_count；混合失败可在得到三个候选前耗尽任务刹车。

低质且仍有两个上限空间 → pending/next_attempt_at=now+退避；达任一上限且存在低质输出 → dead/quarantined；永久传输错误 → failed/failed；纯可重试传输耗尽 → dead/failed。每次模型返回错误 envelope、JSON 失败、schema 错误都算一次质量候选；空知识/输入超限为 preflight quarantine，无 LLM，记录合成 completed attempt_no=0，不占 token。force/new hash 才可显式重置 epoch 计数，普通重跑不可。

回收扫描 `running AND lease_expires_at<=now`，事务内以旧 token CAS 将 reserved attempt abandoned、retry_count+1，保留其全部 token 预留，清 lease；未超限则 pending/退避，超限 dead/failed。新 source head 使任务过时则 succeeded/superseded，不发布。回收重复执行不重复计数。单 worker 也必须实现心跳、回收和 fencing，不能推到后续步骤。

### 8.4 预算熔断

每次请求前计算保守预留 `B=system.UTF8_bytes+input_json.UTF8_bytes+256+max_output_tokens`，将这定义为版本化 **budget units/token 上界估算**，不是供应商实际账单。task 65536、run 262144 为默认上限；日预算 optional，启用后同 SQLite 数据库全部 domain/run 共享，不跨独立数据库。日配置冲突采用当天已存 limit 与本次 limit 的最小值；省略日预算不能绕过已存在当天限额。

claim 事务要求 task/run/day 各 `reserved+B<=limit` 并同时增加，随后才允许模型请求。completed/timeout/crash 均不退还已预留 units；usage 仅报告真实 token，不用于释放额度，避免未知用量和重启超支。若报告 token>B，保留真实用量并立即熔断该 run，记录 estimator_underflow，禁止继续自动发请求，后续版本校准估算器。

run/day 不足：任务保持 pending，统计 deferred、circuit_open=true，本次 run 停止调度，不把预算不足当质量失败；下一 run/UTC 日可调度。task 额度不足且从未请求 → dead/quarantined(task_budget_too_small)；已有隔离候选时不足 → dead/quarantined(task_budget_exhausted)；只有传输失败时不足 → dead/failed。不能自动新 epoch 续费。页级刹车终态不因日预算恢复自动解锁。所有数值用 checked arithmetic，溢出为配置/内部错误。

## 9. D7：CLI 命令与输出

```text
wiktor compile --domain examples/milk-tea/domain.yaml --db wiktor.db \
  --entity product --data-source jsonl://products.jsonl --provider mock --limit 10
wiktor compile --domain examples/milk-tea/domain.yaml --provider ollama \
  --model qwen2.5:7b --embedding-model bge-small-zh-v1.5 --json
```

| 参数 | 契约 |
|---|---|
| --domain | 必填，文件路径；所有相对 source/prompt 路径基于它的目录 |
| --db | 默认 wiktor.db |
| --entity | entities[].name；多实体配置时必填，单实体自动选择 |
| --data-source | 可覆盖所选实体 source；本步仅 jsonl://，不猜测 schema |
| --provider | openai（默认）/ollama/mock；无 feature 时 openai/ollama 明确报错 |
| --model | 真实模型必填；mock 默认 mock-v1；写入 model_version |
| --embedding-model | 默认 none；仍入 hash，设置该参数不会执行 embedding |
| --base-url | 覆盖 provider 默认兼容端点；禁止日志打印凭据 |
| --limit / --batch-size | 默认 1000/32，范围见 §3.1；limit 是本次 scanned 源实体数，不是接受数 |
| --force | 同 hash 也新 epoch；不绕过质量、CAS、预算、source_revision 冲突 |
| --dry-run | 只读配置/源/现有 DB，算 hash/计划；不创建 DB、迁移、写事实/任务、占预算或请求模型 |
| --batch-token-budget / --task-token-budget / --daily-token-budget | 正整数覆盖配置；已有日限额不能提高；普通 resume 不增加原任务额度 |
| --json | stdout 单一 JSON CompileStats；人类日志 stderr |

需要新增只读 inspect 连接用于 dry-run，不能调用会迁移的 SqliteKernel::open；DB 不存在按空库计划，存在旧 schema 则报 migration_required，不自行升级。dry-run 不要求 key/远端连通，但校验模型名和配置。dry-run scanned=skipped+would_compile+quarantined+failed+deferred，accepted=attempts=0。真实 run 每个 scanned 只计一次最终分类：accepted/quarantined/failed/skipped/deferred；多次重编译计 attempts，不重复计页。重复源记录计 skipped(duplicate_in_run)。并发持有任务、尚未到退避时间的任务计 deferred；executor 可在本次 run 等待短退避后重试，最多等待 60s，剩余 deferred，不忙等。

人类表头固定 `accepted quarantined failed skipped deferred attempts`，另行打印 scanned、reserved_tokens、reported_tokens、circuit_open、dry_run/would_compile。skipped 细因与错误 code 通过 tracing 展示，不写源明文。退出码：0 全完成且无 failed/quarantined/deferred；2 参数/配置/输入文件协议错误；3 存在 failed 或 quarantined；4 仅预算/租约/退避导致 deferred；1 数据库/内部运行故障。优先级 1>2>3>4>0；有部分成功也打印已有统计。dry-run 正常计划返回 0，发现无效输入按 2/3，绝不声称 accepted。

## 10. D8：与 Step2/Step3 衔接

accepted 事务提交即可由现有 FTS/LIKE 路径查询，无向量前置依赖。保留已部署的 `filter_page_candidates` category→知识页映射；不要重犯拿 SKU ID 直接作 drink 页候选的错误。新增知识实体 fixture 的 ID 必须与事实 category 值一致；任意 product 页不承诺被现有 category 过滤召回。

本步 LlmCompiler/MockCompiler 默认 `qug_edges=[]`。持久化通道接受可信 Compiler 注入的既有 QugEdge 载荷，验证领域/过滤白名单及现有 QugGraph::from_edges 构造校验，只作有效性检查不启动图服务；按 canonical edge JSON 的 BLAKE3 去重，generation/hash 来自当前接受事务。LLM envelope 不提供 edges 字段，故不能未经来源审查自动抽边。aliases/tags 持久化为 frontmatter，为后续构图提供输入；旧 seed frontmatter 缺失无法从 DB 恢复，需重新 seed，不能伪造回填。

后续向量 worker 按 `(page_id,generation,content_hash,embedding_model)` 扫描 accepted+published 页，取正文/sections 后生成向量，再校验页面仍匹配才标记同步；generation 是逐页版本，不要求所有页等于域最大 generation。本步只提供读取接口和稳定键，不发送通知作为唯一恢复依据。QUG 后续从 DB accepted pages + persisted edges + intents 重建图，禁止从 attempts/quarantine 建图。

**必要边界修复**：Step3 代码目前没有在 RRF 前校验向量 payload 的 accepted/hash/generation。builder 本步补一层 SQLite accepted head 批量校验（库接口输入 page_id/hash/generation，返回有效集合），在截取 top_k 前丢弃不存在/隔离/旧 hash 或 generation 的向量 hit；缺版本 metadata 也丢弃。旧 Mock 测试同步补齐 metadata。无需生成向量即可用假旧向量验证隔离安全。当前 CLI 的 QUG 来自 seed 文件且向量是空 Mock 集合；本步只将 FTS 可见性作为编译衔接验收，不声称新页已获得 QUG/语义向量增强，不改领域 QUG 默认决策。

## 11. 验收判据 A1–A24

默认全部无 key、无网络、无 qdrant，数据库使用临时 SQLite；涉及时钟用注入 Clock，模型失败用脚本式 Compiler/LlmClient，数据库故障用事务中断点。真实 OpenAI/ollama 为单独显式集成 smoke，不阻塞离线验收。

| # | 判据 | 可执行断言 |
|---|---|---|
| A1 | Mock 全链路 | 编译单知识实体后 pages/sections/page_quality/frontmatter/generation 与 succeeded task 一致，FTS 可搜；四维由真实 scorer 产生。 |
| A2 | 增量跳过 | 相同数据/依赖重跑，模型调用 0 次，skipped=1，generation/sections/attempts 不增。 |
| A3 | 全依赖失效 | 逐一改知识值（同时升 revision）、域版本、Prompt bytes、compiler/model/embedding/scorer 版本，各触发一次新编译；同三元改模型不会被 UNIQUE 拦死。 |
| A4 | 稳定序列化 | 嵌套对象键置换 hash 相同；数组顺序、字符串空白改变 hash 不同；长度域消除拼接碰撞；固定黄金 hex。 |
| A5 | 正引用 | §5 fixture 两断言两合法 refs，citation=coverage=density=1；JSON Pointer 转义 ~0/~1 可解析。 |
| A6 | 反引用 | wrong entity/revision/pointer/value/quote、跨节 ID、悬空/未使用 ref 各不能 accepted，输出稳定 issue code。 |
| A7 | 缺引用 | require=true 缺 refs、evidence=None、error envelope 均 quarantine/有限重试；false 不绕过发布引用门槛。 |
| A8 | Schema 防逃逸 | unknown/重复 key、fence、额外散文、正文与 assertions 不同、非法 HTML/标题、额外 refs 均 schema=0；无查询索引写入。 |
| A9 | 评分边界 | 空输入/输出零分；所有分数有限且 [0,1]；(0.6,1,1,0.4) threshold=0.75 通过，coverage=.59 拒绝；consistency=NULL。 |
| A10 | 密度防重复 | 同 quote 重复十次 density=.1；重叠 quote 不重复计源字符；重复 refs 不增 coverage。 |
| A11 | 页级刹车 | 连续差输出 max_recompiles=2 → 恰好最多 3 个候选，dead/quarantined；max_recompiles=0 → 1 个；重启不恢复循环。 |
| A12 | 任务刹车 | 3 次可重试传输失败 → dead/failed；每失败计一次，claim/心跳不增加 retry_count；401 首次 terminal。 |
| A13 | 批预算 | 剩余额度不足一次预留则无模型调用、pending/deferred、退出 4；跨 fetch 不清零，重跑 task 不重置预算。 |
| A14 | 日与任务预算 | 两 run 并发预留不超共享日限額；省略日配置不可绕过；UTC 次日可排队恢复；任务不足 terminal、不自动重置。 |
| A15 | 租约 fencing | worker A 超时、B 回收领取后 A 不能发布/续约/改计数；反复回收只记一次失败和一次 reservation。 |
| A16 | 原子发布 | 在 pages/sections/quality/edges/generation 任一步注入失败，均无半套发布/新 FTS/hash；重试提交只增长一次 generation。 |
| A17 | 旧版本保留 | 已接受页后新低质版本隔离，旧正文仍检索可见，新正文不在 pages_fts；无旧页时零结果。 |
| A18 | CAS/幂等 | revision=2 后 rev1 不能改 facts/ref/page；同 revision 改源报 conflict；并发同三元只有一个 lease，新配置 epoch fence 旧 worker。 |
| A19 | 事实更新 | 仅 price/stock + revision 改动，facts CAS 更新而模型 0 次，页/generation 保留旧引用 revision；敏感字段不在 Prompt/日志/任务快照。 |
| A20 | CLI/dry-run | 统计等式、退出优先级、JSON 单对象成立；dry-run 对不存在 DB 不建文件，对旧 DB 不迁移，不调用模型且无写入。 |
| A21 | 检索衔接 | 新 drink fixture 的 ID 与 category 锚点一致时过滤 FTS 命中；旧向量 hash/generation 及 quarantine 向量在融合前被拒。 |
| A22 | 迁移兼容 | 0001+0002 升级成功，legacy 页保留，FTS 仅 accepted、trigram 不变；首次新 generation>legacy；三元 UNIQUE 仍有效。 |
| A23 | 输入/依赖边界 | batch/limit/行/响应超限受控失败；负 revision 不默认为1；数值溢出/NaN/坏策略报错；无 llm-openai feature 可运行 Mock。 |
| A24 | 工程与回归 | fmt/clippy/workspace tests 及 Rust1.85 编译通过；Step2/3 golden 不回退；真实 provider smoke 独立标注，不把 Mock 当人工相关性/费用验证。 |

## 12. 给 builder 的实现顺序

1. 配置/投影/身份与 hash：冻结 DTO、修正 compile 默认值、严格 revision，完成 A3/A4/A19/A23；保持旧 seed/query API 编译通过。
2. Envelope、renderer、证据载荷、validator、四规则 scorer：扩展 CompiledPage 的可选 evidence，完成 A5–A10；由 test-engineer 独立复核反例。
3. 0003 migration、FTS accepted gate、frontmatter/attempts/head/预算表：完成 A22，升级老测试中的 schema_version=2 断言。
4. kernel admission/claim/heartbeat/recover/failure/publish 专用事务接口：先以预构造 CompiledPage 完成 A15–A18，验证无非重入锁死。
5. executor + MockCompiler：完成 A1/A2/A11–A14/A19；失败任务重启重放不突破预算。
6. async-openai/ollama 单请求适配器与 typed failure：用 Mock HTTP/LlmClient 验证 HTTP 分类、超时和 SDK 重试关闭，真实 key smoke 单独执行。
7. CLI compile/dry-run/统计退出码与数据源有界读取，完成 A20/A23；加入知识实体 fixture 验证 category 关联，不重写已有 golden 期望。
8. 持久边读写、accepted reader、向量过期 payload 防护，完成 A21；明确向量同步/QUG 建图后续工作边界。
9. 执行 A24 并提交双语实现偏差记录；不得以改低质量门槛的方式修测试。每一步保持可编译、可独立验证。

## 13. 边界、风险与实现偏差记录

- 机械引用与抽取式输出限制表达力，但让无 LLM 验收可证明；自由综合推论需要未来更强验证/一致性仲裁，不扩大本步声称。
- 同 revision 不同内容是源契约冲突；要求修正 revision，force 也不能覆盖事实 CAS。知识复用时保留原证据 revision，避免伪造 provenance。
- budget units 是保守估算、crash 也消耗额度；真实费用按 provider usage 与外部单价计算。本步记录 usage/版本并报告未知值，不宣称确定金额。
- 来源 projection 的字段允许列表可能遗漏知识；须用人工标注集校准。源端自带的虚假内容、源自身废话不由引用机械规则识别。
- query 的 vector 版本校验是本步可靠发布必需的衔接修复，外部 qdrant 同步和 QUG 新图构建仍后续完成。数据库损坏和迁移失败停止 run，不能当单页质量问题吞掉。

**实现偏差记录（2026-09-22，设计前核对）**：当前使用 Diesel + Mutex<SqliteConnection>；SQL task 状态为 running/succeeded/dead，并非 compiling/accepted/quarantined。SQL_CLAIM_NEXT 在 claim 时加 retry_count；pages 缺 source_revision/frontmatter、无 qug_edges 表；FTS trigger 索引所有状态；seed generation 固定1；CompiledPage 缺 refs 载荷；DomainConfig 缺段默认值有零值路径；CLI QUG 来自 seed 文件、向量为空 Mock、QueryEngine 未校验向量 generation/hash。以上不是已完成修复，而是本规范明确交给 builder 的迁移/兼容项。后续实现差异必须在此及英文对应节同步追加日期、原因、接口影响与验收变化。

**实现偏差记录（2026-09-22，Step4 交付后追加，对应英文版同节）**：

1. **preflight quarantine 走 admit 前拦截**（§8.3）。`Admission::Queued(i64)` 只返回 task_id 不返回 epoch，executor 无法定位 `(task_id, epoch)` 去调 `quarantine_compile_preflight`。空知识源改为 admit 之前直接拦截：无任务行、无 token 预留、无 LLM 请求，行为等价于「空知识源直接 quarantine」。`quarantine_compile_preflight` 内核方法仍存在可供后续路径使用。接口影响：无；验收：A11 的空源路径按拦截语义断言。
2. **预算熔断信号复用 `claim_compile` 的 `Ok(None)`**（§8.4）。「无到期任务」与「预算不足」都返回 `Ok(None)`，executor 用 `retry_at` 映射区分：存在到期任务却领不到判 circuit_open，全部未到期按 RetryAt 退避（累计 60s 上限 + 50ms 唤醒余量）。接口影响：无新增返回变体；验收：A13/A14 按该区分断言。
3. **`validate_vector_payloads` 按页粒度判有效**（§10）。返回有效 page_id 集合的语义实现为「页有效 ⇔ 该页本批全部 payload 与 accepted head 一致」：同页混入新旧 chunk 时整页向量丢弃，防止旧代 chunk 借同页有效 payload 冒充。比按 hit 粒度放行更严格。接口影响：kernel 返回 `HashSet<String>`（page_id）而非复合键；验收：A21 含同页混合 payload 整页判无效用例。
4. **`open_existing` 对高于当前版本的 schema 也拒绝**（§9 dry-run）。不只拒绝旧版本；CLI 把 `migration_required` 归入退出码 2（输入问题），真实 run 的迁移失败仍是退出码 1。接口影响：`SqliteKernel::open_existing` 新增只读入口，`SUPPORTED_SCHEMA_VERSION=3`。
5. **executor 边校验用固定深度 2**（§10）。`QugGraph::from_edges` 构造校验使用领域默认深度 2，不读领域实际 `qug.max_depth`（`CompilePolicy` 不携带该字段）。只做有效性检查不启动图服务，非法载荷走 InvalidOutput 不发布。
6. **连接级错误统一 Retryable**（§4）。reqwest 无法区分连接失败是否源于 TLS 证书，连接级错误一律 Retryable，耗尽后 dead/failed，绝不误接受。
7. **稳定诊断 code 的映射收敛**（§5.2）。重复 ref id 映射为 `UNKNOWN_REF`、标题/aliases/tags 失配映射为 `QUOTE_MISMATCH`，均带 JSON path 区分；未新增 `DUP_REF` 等 code。required_headings 的集合校验在 decode 阶段只查形状与唯一性，集合匹配由 executor 持 policy 执行。
8. **`admit_compile` 签名增加 `schema: &EntitySchema` 参数**（§8.2）。content_hash 与 dependencies_json 的 knowledge_schema 域需要冻结的 schema，由 executor 持有并传入。
9. **文件落点与 spec 字面不同**（§3）。spec 写 `compile/store.rs` 承载 DTO，实际 DTO（Admission/TaskLease/CommitOutcome 等）落在 `compile/config.rs`，数据库实现在 `kernel/compile_store.rs`（`pub(crate)`）。与同节「数据库实现放 kernel、不建空壳」一致，仅文件名不同，避免再改模块路径。
10. **编译版本标识字面值**（§7）。`CompilePolicy::default()` 的 `compiler_version="compile-v1"`、`artifact_version="wiki-v1"` 为本步固定标识（spec 未给字面值），二者入 content_hash，改动即触发重编译。

<!-- END STEP4 SPEC v1.0 -->
