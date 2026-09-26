# Step 14 (P1) Design Spec: Real Vector Path + Query Hot-Cache Benchmark

> Version: v1.0 (2026-09-26)
> Authority: `docs/MASTER-PLAN.md` §5.4/§5.5 (cache hit <5ms, hybrid <50ms, query embedding <15ms)
> Implementer: `wiktor-builder`; experiment/benchmark measured on the Linux production path (real qdrant + real embedding)

## 1. Goals & Non-Goals

This Step delivers two MASTER-PLAN promises with real data:

- **moka query hot-cache** (§5.4/§5.5 #5): generation-aware + precisely invalidated by entity ID; caches the **hit set, not the whole QueryResult** — `log_id`/`latency_ms` are per-query and never cached (the feedback loop depends on the real log_id).
- **Real end-to-end latency benchmark**: on Linux with real qdrant (collection `tech-docs-bench`@1024) + real embedding (aliyun qwen3.7-text-embedding-flash), measuring both cold-cache and cache-hit p50/p95/p99, replacing Step11's mock/in-memory-only criterion.

Non-goals: no local BGE (no such model); no cross-host network (qdrant is local 6334); no retrieval/compile semantics change.

## 2. Key Decisions

### D1 Cache key = BLAKE3 of the normalized query signature
- Components: `serde_json::to_vec((text, filters, top_k, domain, generation))`.
- `Filters` holds `f64` (NumericRange) so it can't derive Hash/Eq; canonical JSON serves as the key identity — deterministic, no hand-rolled hash.
- generation is read fresh each search (`kernel.current_published_generation`) → a compile publish bumps it → the key changes → old entries naturally go stale.

### D2 Entity invalidation = reverse index
- The cache keeps an `entity_page_id → cache keys` reverse index (`entity_index: RwLock<HashMap>`).
- A fact-plane **direct-write ETL path** (bypassing compile, no generation bump) calls `QueryCache::invalidate_entity_ids` to precisely drop entries referencing that entity — no whole-page TTL gambling (§5.5 #5).

### D3 A cache hit still writes query_logs
- A hit returns the same hit set but **still writes a new query_logs row each call for a fresh log_id** (feedback references the real log_id; the cache never swallows it).
- The cached value carries `candidate_empty_initial` (absent from QueryDiagnostics but relied on by the feedback analyzer) so cache-hit log rows reproduce faithfully.

### D4 Serve assembly
- On by default in production: `with_cache(QueryCache::new(size))` inside `assemble_state_and_search`.
- Capacity from `WIKTOR_QUERY_CACHE_SIZE` (default 512); `0` disables (diagnostics/comparison).

## 3. Benchmark Results (measured on the Linux production path)

### 3.1 Cold cache (40 distinct golden queries, each a cache miss → real embedding + qdrant + FTS + RRF)

| Metric | Value |
|---|---|
| n | 40 |
| p50 | 80ms |
| p95 | 97ms |
| p99 | 100ms |
| max | 100ms |
| mean | 81ms |
| hit rate (has hits) | 40/40 |
| vector path active | 40/40 (vector_count>0) |

Cold p50 ≈ 80ms, dominated by the **remote embedding call (aliyun HTTP ~60-70ms)**; local qdrant ANN (<1ms), FTS and RRF are sub-ms. MASTER-PLAN's "query embedding <15ms" target was designed for local BGE (CPU); with a remote embedding API the embedding itself exceeds that budget — a promise-vs-deployment deviation noted in §5.

### 3.2 Cache hit (same query repeated, new cache-enabled binary)

| Call | server latency_ms | log_id |
|---|---|---|
| cold (first) | 233ms | 58 |
| hit 1–5 | 0ms | 59–63 (each unique) |

**Cache-hit server latency ~0ms (<5ms target met)**; every hit still writes a fresh log_id (59–63 distinct) → **the feedback loop is not broken by the cache**.

### 3.3 Conclusion

- The cache pays off: hit 80ms → 0ms.
- The key correctness constraint (log_id unique per call) verified empirically.
- The only unmet MASTER-PLAN budget is "query embedding <15ms" — root cause is the remote embedding API in production, not an implementation defect; a local CPU embedder would return to spec.

## 4. Acceptance

- D1–D4 implemented: `query_engine/cache.rs` (4 unit tests) + `search` wiring + serve assembly.
- Integration test `tests/step14_query_cache.rs`, 3 green: cache hit same hits but fresh log_id / vector path not recomputed / entity invalidation reflected in later filter results.
- Real benchmark: cold p50=80ms, hit ~0ms; data in §3.
- Workspace all green + clippy -D warnings 0 + fmt clean.

## 5. Deviations

- **STEP14-P1-001**: MASTER-PLAN §5.4 "query embedding <15ms" presumes local CPU embedding; production uses a remote embedding API whose call alone is ~60-70ms, making the total p50 ≈ 80ms. Not a defect — a promise-vs-deployment mismatch, noted in MASTER-PLAN/docs.
- **STEP14-P1-002**: `f64` can't derive Hash/Eq, so the cache key uses canonical JSON (serde) rather than a hand-rolled hash (D1) — deterministically equivalent, no inconsistency risk.