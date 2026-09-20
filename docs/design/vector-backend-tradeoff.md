# 向量后端选型：sqlite-vec vs qdrant（决策存档 v1）

> 状态：**已决**（2026-09-20，用户拍板）。本文是调研记录与决策依据，供日后回顾，避免重做调研。
> 结论：**默认向量基线 = qdrant 外部服务**；sqlite-vec 降为可选插件；内存暴力扫描保留为评测基线。
> 对应 MASTER-PLAN v3.2 变更记录 #12–#15。

## 一、调研日期与方法

2026-09-20 实时抓取：GitHub API / Releases、crates.io、docs.rs、官网文档、作者博客、HN。两份调研由 Explore 子智能体完成。

## 二、sqlite-vec 现状（asg017/sqlite-vec）

| 项 | 事实 |
|---|---|
| 定位 | README 明确 **pre-v1, expect breaking changes**；作者措辞是"ready to try"，从未声称生产就绪 |
| 版本 | 稳定 v0.1.9（2026-03-31）；最新 v0.1.10-alpha.4（2026-05-18，含 ANN） |
| 索引能力 | 稳定版**纯暴力扫描**；ANN（rescore / DiskANN / IVF-experimental）只在 alpha |
| 过滤 | metadata 列仅 `=`/`!=` 等简单谓词；partition key 分片预过滤；无 Qdrant 式复合/范围动态过滤与独立过滤索引 |
| 维护 | star 8119，但**两次断档**：2024 底~2026-03 停更约 15 个月（社区问"是否弃坑"）；2026-05-18 至今（约 4 个月）无维护者提交、30 个 open PR 无人处理 |
| Rust 生态 | `sqlite_vec` crate 293 万下载，活跃使用；已知坑：必须 rusqlite `bundled`、auto-extension 注册 API 跨版本变过 |
| 规模 | 作者路线图目标 "low millions ~ tens of millions"，无官方支持上限 |

结论：**技术方向（嵌入 SQLite）讨喜，但工程可靠性不满足"内核默认依赖"标准**——单人维护、两度断档、pre-v1 API 可破坏性变更。不适合作为 core 的强制依赖。

## 三、Qdrant 现状

| 项 | 事实 |
|---|---|
| 版本 | v1.19.1（2026-09-04），34.7k stars，开发活跃（主分支最近提交 2026-09-19） |
| 索引 | 成熟 HNSW ANN；量化（Scalar int8 / Binary / Product / TurboQuant）；v1.19 起 Memory Tiers（cold 走 mmap） |
| 过滤 | payload filter 一等公民、可建独立索引、与 ANN 协同 |
| 资源 | 官方无硬性最低内存；十万级 × 768d fp32 ≈ **0.32GB**（300MB 稠密 + 15MB HNSW），int8 量化后约 80MB——**2GB 小机可跑** |
| Rust SDK | qdrant-client v1.19.0，380 万下载，活跃；纯网络客户端（无进程内模式），需独立服务 |
| 代价 | 外部服务：备份/升级/监控自理；破坏"单二进制"承诺；向量与 SQLite 事务的原子性需两段同步 |

## 四、对 Wiktor 的含义

1. **选 qdrant 的正确理由不是"性能更好"**，而是工程可靠性 + 过滤能力（QUG 属性传播/否定边的结构化过滤在 qdrant 是一等能力）。
2. **代价**：原子发布从"单事务"变为"SQLite 单事务 + 向量两段同步"。兜底机制：
   - 向量是**派生索引、可重建**（MASTER-PLAN 第四节铁律）——删 collection 从 Markdown 重嵌入即恢复，无数据迁移；
   - 查询按 **generation 对齐**，向量滞后于页面提交是允许的读一致性取舍（与"知识平面滞后于事实平面"同类）；
   - collection payload 带 `content_hash`，可靠性契约 #1 的全依赖哈希体系覆盖。
3. **何时该切嵌入式/其他后端**（VectorStore trait 隔离，代价已付）：
   - 需要单二进制零外部依赖的嵌入式部署 → sqlite-vec（跟踪其 ANN 稳定版）/ hnsw_rs / arroy；
   - 官方 SQLite **Vec1**（Hipp 团队，IVFADC+OPQ ANN，pre-1.0）若到 1.0，是"SQLite 一体化"的正统选项，值得跟踪；
   - 上百万级且要求 <10ms 单查询、复合动态过滤、水平扩展 → 维持 qdrant / lancedb / Milvus。
4. **评测基线**：内存暴力扫描 recall=1.0、无 ANN 近似噪声——golden-queries 评测（纯向量 vs 混合 vs QUG）用它量化 QUG 增量，不被 ANN 参数干扰。

## 五、运维事实（2026-09-20 实测）

- 本机（macOS arm64）：qdrant v1.19.1 官方二进制，127.0.0.1:6333(REST)/6334(gRPC)，`healthz check passed`。
- Linux（Debian 13，2 核 2GB，111.231.168.150）：同版本二进制 systemd 化，API key 认证（公网裸起 = 安全红线）。
- 十万级以内：暴力扫描与 HNSW 差距被高估；真正拉开差距在百万级。

## 参考来源

- sqlite-vec: github.com/asg017/sqlite-vec（README / releases / 2026-05-18 后无合并）、alexgarcia.xyz/sqlite-vec（pre-v1 警告）、crates.io/crates/sqlite_vec、issue #226（弃坑询问）、#206（rusqlite 注册 API）
- qdrant: github.com/qdrant/qdrant（v1.19.1）、qdrant.tech/documentation/capacity-planning / quantization / memory-tiers、crates.io/crates/qdrant-client
- 视角佐证：Timescale《Vector databases are the wrong abstraction》（2024-10）；rqlite 官方支持 sqlite-vec 扩展（远期 Raft 路径相关）；sqlite.org/vec1（官方 Vec1，pre-1.0）
