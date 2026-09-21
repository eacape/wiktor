# Vector Backend Selection: sqlite-vec vs qdrant (Decision Archive v1)

> Status: **settled** (2026-09-20, user decision). This document is the research record and decision basis, retained for future review to avoid repeating the research.
> Conclusion: **default vector baseline = qdrant external service**; sqlite-vec downgraded to an optional plugin; in-memory brute-force scan retained as the evaluation baseline.
> Corresponds to MASTER-PLAN v3.2 change log #12–#15.

## 1. Research Date and Method

Real-time research on 2026-09-20: GitHub API / Releases, crates.io, docs.rs, official documentation, author blogs, and HN. Two research tasks were completed by the Explore sub-agent.

## 2. Current State of sqlite-vec (asg017/sqlite-vec)

| Item | Fact |
|---|---|
| Positioning | The README explicitly says **pre-v1, expect breaking changes**; the author's wording is "ready to try" and it has never claimed production readiness |
| Version | Stable v0.1.9 (2026-03-31); latest v0.1.10-alpha.4 (2026-05-18, includes ANN) |
| Index capability | Stable release is **pure brute-force scan**; ANN (rescore / DiskANN / IVF-experimental) exists only in alpha |
| Filtering | Metadata columns support only simple predicates such as `=`/`!=`; partition-key sharding pre-filters; no Qdrant-style composite/range dynamic filtering or independent filter indexes |
| Maintenance | 8119 stars, but **two hiatuses**: late 2024 to 2026-03, about 15 months (the community asked whether it had been abandoned); since 2026-05-18 (about 4 months), no maintainer commits and 30 open PRs unhandled |
| Rust ecosystem | `sqlite_vec` crate has 2.93 million downloads and active usage; known pitfalls: rusqlite `bundled` is required, and the auto-extension registration API has changed across versions |
| Scale | The author's roadmap targets “low millions ~ tens of millions”; no official supported limit |

Conclusion: **the technical direction (embedding in SQLite) is attractive, but engineering reliability does not meet the standard for a default kernel dependency** — single-person maintenance, two hiatuses, and pre-v1 API-breaking changes. It is not suitable as a mandatory core dependency.

## 3. Current State of Qdrant

| Item | Fact |
|---|---|
| Version | v1.19.1 (2026-09-04), 34.7k stars, active development (latest main-branch commit 2026-09-19) |
| Index | Mature HNSW ANN; quantization (Scalar int8 / Binary / Product / TurboQuant); Memory Tiers since v1.19 (cold data uses mmap) |
| Filtering | Payload filters are first-class, can have independent indexes, and cooperate with ANN |
| Resources | No hard official minimum memory; 100k vectors × 768d fp32 ≈ **0.32GB** (300MB dense + 15MB HNSW), about 80MB after int8 quantization — **runs on a 2GB small machine** |
| Rust SDK | qdrant-client v1.19.0, 3.8 million downloads, active; pure network client (no in-process mode), requires a separate service |
| Cost | External service: backups/upgrades/monitoring are self-managed; breaks the “single binary” promise; vector and SQLite transaction atomicity requires two-phase synchronization |

## 4. Meaning for Wiktor

1. **The right reason to choose qdrant is not “better performance”**, but engineering reliability + filtering capability (structured filtering for QUG attribute-propagation/negation edges is first-class in qdrant).
2. **Cost**: atomic publish changes from “one transaction” to “one SQLite transaction + two-phase vector synchronization”. Safety nets:
   - Vectors are **derived indexes and rebuildable** (the iron rule in MASTER-PLAN Section 4) — delete the collection and re-embed from Markdown to recover, with no data migration;
   - Queries align by **generation**; vector lag behind page commits is an allowed read-consistency trade-off (the same class as the knowledge plane lagging behind the fact plane);
   - The collection payload carries `content_hash`; reliability contract #1's all-dependency hash system covers it.
3. **When to switch to an embedded/other backend** (the `VectorStore` trait isolates it; the cost has already been paid):
   - Embedded deployment requiring a single binary with zero external dependencies → sqlite-vec (track its stable ANN release) / hnsw_rs / arroy;
   - Official SQLite **Vec1** (Hipp team, IVFADC+OPQ ANN, pre-1.0), if it reaches 1.0, is the canonical option for “all-in-one SQLite” and worth tracking;
   - More than one million vectors with a requirement for <10ms per query, composite dynamic filtering, and horizontal scaling → keep qdrant / lancedb / Milvus.
4. **Evaluation baseline**: in-memory brute-force scan has recall=1.0 and no ANN approximation noise — use it in golden-queries evaluation (pure vector vs. hybrid vs. QUG) to quantify QUG gains without ANN parameters interfering.

## 5. Operational Facts (Measured 2026-09-20)

- Local machine (macOS arm64): qdrant v1.19.1 official binary, 127.0.0.1:6333(REST)/6334(gRPC), `healthz check passed`.
- Linux (Debian 13, 2 cores, 2GB, 111.231.168.150): same-version binary managed by systemd, API-key authentication (running bare on a public network = security red line).
- Up to the 100k scale: the gap between brute-force scan and HNSW is overestimated; the real gap opens at the million scale.

## References

- sqlite-vec: github.com/asg017/sqlite-vec (README / releases / no merges after 2026-05-18), alexgarcia.xyz/sqlite-vec (pre-v1 warning), crates.io/crates/sqlite_vec, issue #226 (abandonment question), #206 (rusqlite registration API)
- qdrant: github.com/qdrant/qdrant (v1.19.1), qdrant.tech/documentation/capacity-planning / quantization / memory-tiers, crates.io/crates/qdrant-client
- Supporting perspectives: Timescale, “Vector databases are the wrong abstraction” (2024-10); rqlite officially supports the sqlite-vec extension (relevant to the long-term Raft path); sqlite.org/vec1 (official Vec1, pre-1.0)
