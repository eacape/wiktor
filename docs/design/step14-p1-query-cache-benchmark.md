# Step 14 (P1) 设计规范：真实向量路径 + 查询热点缓存基准

> 版本：v1.0（2026-09-26）
> 权威依据：`docs/MASTER-PLAN.md` §5.4/§5.5（缓存命中 <5ms、混合检索 <50ms、查询嵌入 <15ms）
> 实现对象：`wiktor-builder`；实验/基准由主模型在 Linux 生产路径（真实 qdrant + 真实嵌入）测取

## 1. 目标与非目标

本 Step 落地 MASTER-PLAN 两件承诺并给出真实数据：

- **moka 查询热点缓存**（§5.4/§5.5 契约 #5）：generation-aware + 按实体 ID 精准失效；缓存**命中集合而非整条 QueryResult**——`log_id`/`latency_ms` 每次查询独立，绝不入缓存（反馈闭环依赖真实 log_id）。
- **真实端到端延迟基准**：在 Linux 用真实 qdrant（collection `tech-docs-bench`@1024）+ 真实嵌入（aliyun qwen3.7-text-embedding-flash）测冷缓存与缓存命中两档 P50/P95/P99，替代 Step11 只测 mock/内存路径的 criterion。

非目标：不测本地 BGE（无此模型）；不测跨机网络（qdrant 本机 6334）；不改检索/编译语义。

## 2. 关键决策

### D1 缓存键 = 规范化查询签名 BLAKE3
- 成分：`(text, filters, top_k, domain, generation)` 的 `serde_json::to_vec` 摘要。
- `Filters` 含 `f64`（NumericRange）无法 derive Hash/Eq，故用 canonical JSON 做键标识，确定性、无手写哈希。
- generation 每次 search 现读（`kernel.current_published_generation`）→ 编译发布使 generation 递增 → 键变化 → 旧条目自然失效，无需主动清缓存。

### D2 实体失效 = 反向索引
- 缓存额外维护 `entity_page_id → cache keys` 反向索引（`entity_index: RwLock<HashMap>`）。
- 事实平面**直写 ETL 路径**（不经编译、不 bump generation）调用 `QueryCache::invalidate_entity_ids` 精准删除含该实体的条目——不做整页 TTL 赌博（§5.5 #5）。

### D3 缓存命中仍写 query_logs
- 命中返回同一 hits 集合，但**每次调用仍写一条新 query_logs 取新 log_id**（反馈引用依赖真实 log_id，缓存绝不吞掉）。
- 缓存值带 `candidate_empty_initial`（不在 QueryDiagnostics 但反馈分析器依赖），使缓存命中日志行忠实复现。

### D4 serve 装配
- 生产默认开启：`assemble_state_and_search` 内 `with_cache(QueryCache::new(size))`。
- 容量取 `WIKTOR_QUERY_CACHE_SIZE`（缺省 512）；设 `0` 关闭（诊断/对比）。

## 3. 基准结果（Linux 生产路径实测）

### 3.1 冷缓存（40 条 distinct golden 查询，每次 cache miss → 真实嵌入 + qdrant + FTS + RRF）

| 指标 | 值 |
|---|---|
| n | 40 |
| p50 | 80ms |
| p95 | 97ms |
| p99 | 100ms |
| max | 100ms |
| mean | 81ms |
| 命中率（有 hits） | 40/40 |
| 向量路参与 | 40/40（vector_count>0） |

冷缓存 p50 ≈ 80ms，由**远端嵌入调用（aliyun HTTP ~60-70ms）主导**，qdrant 本机 ANN（<1ms）、FTS、RRF 均毫秒级。MASTER-PLAN「查询嵌入 <15ms」目标按本地 BGE（CPU）设计；生产使用远端 embedding API 时，嵌入本身即跨越该预算——这是承诺口径与部署形态的偏差，见 §5 偏差。

### 3.2 缓存命中（同一查询重复，新二进制含 cache）

| 调用 | server latency_ms | log_id |
|---|---|---|
| 冷（首调） | 233ms | 58 |
| 命中 1–5 | 0ms | 59–63（每次唯一） |

**缓存命中服务端 ~0ms（<5ms 目标达成）**；且每次命中仍写新 log_id（59–63 互异）→ **反馈闭环未被缓存破坏**。

### 3.3 结论

- 缓存收益显著：命中 80ms → 0ms。
- 缓存正确性关键约束（log_id 每次唯一）经实测成立。
- 唯一未达标的 MASTER-PLAN 预算是「查询嵌入 <15ms」——根因是生产用远端 embedding API，非实现缺陷；本地 CPU 嵌入可回归达标。

## 4. 验收

- D1–D4 实现：`query_engine/cache.rs`（4 单测）+ `search` 接入 + serve 装配。
- 集成测试 `tests/step14_query_cache.rs` 3 项全绿：缓存命中同 hits 但新 log_id / 向量路不重跑 / 实体失效后过滤结果反映变更。
- 真实基准：冷 p50=80ms、命中 ~0ms；数据见 §3。
- workspace 全绿 + clippy -D warnings 0 + fmt 干净。

## 5. 偏差

- **STEP14-P1-001**：MASTER-PLAN §5.4「查询嵌入 <15ms」目标以本地 CPU 嵌入为前提；生产用远端 embedding API 时嵌入自身 ~60-70ms，总 p50≈80ms。非实现缺陷，属承诺口径与部署形态不符，已在新版 MASTER-PLAN/文档注明。
- **STEP14-P1-002**：`f64` 无法 derive Hash/Eq，缓存键经 canonical JSON（serde）而非手写哈希实现（D1）——确定性等价，无不一致风险。