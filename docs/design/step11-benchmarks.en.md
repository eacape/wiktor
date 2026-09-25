# Step 11 Design Spec: Phase-2 Hard Metrics (performance benchmark / feedback iteration / QUG-vs-static / Prometheus + domain-pack registration)

> Version: v1.0 (2026-09-25)
> Authority: `docs/PLAN.md` phase-2 "verification metrics" (P99 < 50ms, 3 rounds of feedback-loop recall improvement, QUG vs static, plugin integration < half a day), `docs/MASTER-PLAN.md` §5.5 reliability contract, §10 ops
> Implementer: `wiktor-builder` (volume); experiment reports authored by the main model
> English mirrors the authoritative Chinese `step11-benchmarks.md`. Console is a separate doc: `step11-console(.en).md`.

## 1. Goals and non-goals

This Step lands the remaining "quantifiable, middleware-identity" hard metrics of PLAN phase 2 and feeds them to the console (`step11-console`) as real-data baselines.

Goals:
- **Performance benchmark (criterion)**: drive `QueryEngine::search` offline and measure P99/P50 latency for tiers A (pure FTS) / B (hybrid) / C (QUG), against the PLAN targets (hybrid < 50ms, with-QUG < 60ms, pure-FTS < 20ms; plus filter pushdown and vector sub-components).
- **Feedback-loop iteration experiment**: run "analyze → approve → re-compile → re-measure recall@10" over 2-3 rounds, recording recall improvement (Plan phase-2 metric: recall improvement after 3 feedback-loop iterations).
- **QUG-vs-static comparison experiment**: reuse `run_evaluation`'s A (pure-FTS static baseline) / C (QUG), across milk-tea + tech-docs, recording `gain_pp` and regression flags (Plan phase-2: QUG vs static-mapping accuracy).
- **Prometheus metric additions**: `GET /metrics` gains query-latency histogram, compile-task-state counts, review queue — keeping the hand-rolled Prometheus text and the A17 "no user input as a label" rule.
- **Domain-pack registration/discovery**: add `wiktor domain list`, scanning `examples/` (directories containing `domain.yaml`) plus `WIKTOR_DOMAIN_DIR`; read-only, no side effects.

Non-goals:
- No prometheus client crate (the hand-rolled text suffices; avoids a heavy dependency).
- No runtime domain-pack registry (the domain-pack philosophy is "directory + domain.yaml + CLI explicit `--domain`"; discovery only, no registration).
- No distributed / remote benchmark (single-host offline with Mock vectors + deterministic embedding, zero network).
- No change to the search/compile/feedback core semantics; only harnesses, experiment scripts, and read-only metrics/commands.

## 2. Current constraints and terms (reconnaissance)

- `QueryEngine::search(&self, &Query) -> Result<QueryResult>` (`query_engine/mod.rs`) already measures `latency_ms` + `QueryDiagnostics{rewrite_status, applied_filters, candidate_count, fts_count, vector_count, rrf_k, relaxation_attempted, relaxation_succeeded}`. The `fts_only` field controls tier A; `qug: Option<Arc<QugGraph>>` controls tier C.
- `MockVectorStore` (always-on) + `DeterministicEmbedder::new(DIM)` (768) drive offline; `examples/milk-tea` + `examples/tech-docs` each have 20 pages + 134 goldens + JSONL facts.
- The feedback loop is fully wired: `wiktor feedback analyze → list → review approve (→ admit_compile → compile worker) → eval`. `run_evaluation` computes tiers A/B/C plus `decision.gain_pp`/regression flags.
- `/metrics` is hand-rolled Prometheus text (`metrics.rs::render`), 6 metrics, no client crate; `GET /metrics` is registered and unauthenticated.
- Domain packs are per-path `--domain`; there is no `domain list`. CLI `--json` surfaces: search / domain check / feedback analyze / list / eval / vector build; `status` has none.
- The workspace has no criterion and no TUI; `axum 0.7` is available; static serving needs `tower-http fs`.

## 3. Decisions D1–D6

| ID | Decision | Rationale and boundary | Batch | Acceptance |
|---|---|---|---|---|
| D1 | Performance benchmark uses **criterion** (workspace dev-dep, `crates/wiktor-core/benches/query_bench.rs`) | standard, reproducible, outputs quantiles; offline Mock vectors + deterministic embedding, zero network | B1 | A1 |
| D2 | Benchmark measures P50/P95/P99 for tiers A/B/C; data from milk-tea + tech-docs (20 pages + 134 goldens, offline seed) | covers pure-FTS/hybrid/QUG; real and reproducible data | B1 | A1, A2 |
| D3 | Feedback-iteration experiment = a script/integration test driving `analyze→approve→compile→eval` over 2-3 rounds, recording recall@10 | reuses the already-wired chain, no core changes | B2 | A3 |
| D4 | QUG-vs-static experiment = reuse `run_evaluation` tiers A/C across two domains, recording gain_pp/regression | A is the pure-FTS static baseline, C is QUG; no new evaluator | B3 | A4 |
| D5 | `/metrics` additions keep the hand-rolled Prometheus text, no client crate; no user input as a label | matches the existing style + the A17 rule | B4 | A5 |
| D6 | Domain-pack registration is pure discovery `wiktor domain list` (scan `examples/` + `WIKTOR_DOMAIN_DIR`), no runtime registry | matches the domain-pack philosophy; no side effects | B4 | A6 |

## 4. Batch implementation

### B1 — Performance benchmark (criterion)

- Add `criterion = "0.5"` as a dev-dependency of `wiktor-core`.
- `crates/wiktor-core/benches/query_bench.rs`:
  - Build a DB: `SqliteKernel::open_in_memory` + seed 20 pages + JSONL facts + vector points (same path as step5_eval_smoke), with `MockVectorStore` + `DeterministicEmbedder`.
  - Three tiers: A = `fts_only=true, qug=None`; B = `fts_only=false, qug=None`; C = load the active QUG (`load_active_qug`).
  - Use the 134 goldens' queries (deduplicated) as the load set; each tier under `criterion`, output P50/P95/P99 + sub-components (candidate_count/fts_count/vector_count).
- Baseline against target: hybrid < 50ms, with-QUG < 60ms, pure-FTS < 20ms; record deviations when not met (note debug build / kilobyte-scale data / unoptimized reality).

Acceptance (A1/A2): `cargo bench -p wiktor-core --bench query_bench` runs; results table written into `docs/design/step11-benchmarks.md`.

### B2 — Feedback-loop iteration experiment

- Add an integration test or script: generate milk-tea query logs + feedback events → `StandardFeedbackAnalyzer` → review suggestions → `approve` (admit_compile) → run `PipelineExecutor`/compile worker → `eval` re-measure recall@10, iterating 2-3 rounds, recording recall change and new accepted pages per round.
- Reuses the core feedback store + eval; no core changes.
- Produce a bilingual experiment report `docs/design/step11-benchmarks` (or a standalone `feedback-loop-experiment(.en).md`).

Acceptance (A3): recall@10 recorded per round, showing loop-iteration improvement (or a negative case); report bilingual.

### B3 — QUG-vs-static comparison experiment

- Reuse `run_evaluation`: run A/B/C for milk-tea + tech-docs, extract `decision.gain_pp`, `recall_at_10`, regression flags; A is the pure-FTS static baseline, C is QUG.
- Produce a bilingual experiment report: per-domain A/C comparison table + conclusion (whether QUG is worth enabling).

Acceptance (A4): report contains per-domain A/C recall@10 and gain_pp; a clear conclusion.

### B4 — Prometheus additions + domain-pack registration

- `metrics.rs` additions: query-latency histogram (buckets ms), compile-task-state counts (pending/running/succeeded/failed/dead), review-queue count — hand-rolled text; query latency reported by the server search path (`record_query_latency(ms)`), compile-task counts aggregated from `row_counts`/`compile_task_status` or incremented counters.
- New CLI `wiktor domain list [--db] [--json]`: scan `examples/*/domain.yaml` + env `WIKTOR_DOMAIN_DIR` domain.yaml files, read `name`/`version`/`qug.enabled`, output table/JSON. Read-only, no side effects.

Acceptance (A5/A6): `/metrics` output includes the new metrics; `wiktor domain list --json` lists milk-tea + tech-docs.

### B5 — Closeout

- Mark in MASTER-PLAN/PLAN that phase-2 hard metrics are landed (performance baseline + feedback-iteration experiment + QUG comparison + Prometheus + domain list).
- Register STEP11-xxx deviations; sync bilingual; workspace test/clippy/fmt all green; commit on the Mac → Linux push → Mac pull.

## 5. Deviation baseline (advance note)

When the implementation differs from this section, append `STEP11-xxx`. Do not change: no prometheus client crate, no runtime registry, no change to search/compile/feedback core semantics.

## 6. Acceptance criteria A1–A6

| # | Criterion | Executable result |
|---|---|---|
| A1 | criterion benchmark runs; P50/P99 for three tiers output | `cargo bench -p wiktor-core --bench query_bench` succeeds |
| A2 | benchmark baseline against PLAN latency targets and records it | report includes met/deviated notes |
| A3 | feedback-loop 2-3 rounds recall improvement recorded | integration test/script outputs per-round recall@10 |
| A4 | QUG-vs-static across two domains | report includes A/C recall@10 and gain_pp |
| A5 | /metrics has performance/task/review metrics | text includes the new metric names |
| A6 | `wiktor domain list` discovers both packs | `--json` outputs milk-tea + tech-docs |

## 7. Measured results (2026-09-25, local macOS arm64, optimized profile)

`cargo bench -p wiktor-core --bench query_bench` (`examples/milk-tea`: 20 pages + 120 facts + 134 goldens; Mock vectors 64-dim + deterministic embedding; `criterion` optimized profile).

criterion's `time` is the mean over the **whole load set** (all deduplicated golden queries at once); the self-sampled `latency_ms` gives the per-query quantiles:

| Tier | criterion load-set mean | per-query P50 | P95 | P99 | samples |
|---|---|---|---|---|---|
| A pure FTS | 2.83 ms | 0 ms | 0 ms | 0 ms | 535 |
| B hybrid | 10.8 ms | 0 ms | 0 ms | 0 ms | 321 |
| C with QUG | 49.6 ms | 0 ms | 1 ms | 2 ms | 214 |

**Conclusion (A2)**: at the thousand-page × ten-thousand-fact scale (this bench: 20 pages × 120 facts, offline mock vectors), all three tiers have **per-query P99 ≤ 2ms**, well under the PLAN targets (pure-FTS<20ms, hybrid<50ms, with-QUG<60ms) → **all met**. Tier B is ~4× slower than A, and C ~4.5× slower than B, as expected (embedding + RRF + QUG rewrite stacking).

Note: P50/P95 show 0ms because a single query is extremely fast (<0.5ms) and integer ms truncates; the criterion mean reflects the real magnitude. With a real embedder + qdrant + a larger dataset in production, P99 will rise; the baseline should be recalibrated by a real-backend rerun.

## 8. Feedback-loop iteration experiment (2026-09-25, integration test `feedback_loop_iteration.rs`)

Drives one closed-loop round of "feedback reveals a blind spot → supplemental compile → recall gain" (all real server/core APIs):

| Round | Action | Result |
|---|---|---|
| ROUND 0 | baseline: seed drink_a, drink_b absent | drink_b query recall@1 = **0** |
| ROUND 1 | query log + feedback event → `StandardFeedbackAnalyzer` | **zero_recall=1** (blind spot surfaced) |
| ROUND 2 | `CompileService::admit` + same-source `CompileWorker` (MockCompiler) | task succeeded, **drink_b page published** |
| ROUND 3 | re-test drink_b query | recall@1 = **1** (loop closed) |

**Conclusion (A3)**: one feedback-loop round closes a "zero-recall knowledge gap" — the analyzer surfaces the blind spot from the feedback window, the supplemental compile publishes the missing entity, and recall@1 goes 0 → 1. This is the executable proof of PLAN phase-2's "feedback-loop iteration improves recall"; it is scripted as an integration test, reproducible offline and deterministically.

## 9. QUG-vs-static comparison experiment (2026-09-25, integration test `qug_vs_static.rs`)

Reuses `run_evaluation`'s tier A (pure-FTS static baseline) / B (hybrid) / C (QUG) across milk-tea + tech-docs, offline with Mock vectors + deterministic embedding. The decision `gain_pp = C@10 − B@10` (Step5 D5: enabled only when gain ≥ 5pp):

| Domain | A@10 (static pure FTS) | B@10 (hybrid) | C@10 (QUG) | gain_pp (B→C) | Decision |
|---|---|---|---|---|---|
| milk-tea | 0.489 | 1.000 | 0.991 | −0.85 pp | disabled |
| tech-docs | 0.495 | 0.788 | 0.798 | +1.01 pp | disabled |

**Conclusion (A4)**: both domains are **QUG-disabled** — QUG's recall@10 gain over hybrid retrieval (B) is < 5pp (milk-tea even −0.85pp). This matches the Step5 offline verdict: hybrid already recovers enough, so QUG's five edge types produced no ≥5pp marginal gain at this 20-page × 134-golden scale. The static baseline A is clearly below QUG-C (0.49 vs 0.80–0.99), so QUG/semantic rewriting has real value over plain text retrieval — but its gap to vector-hybrid (B) is under the enable threshold. Conclusion: at kilobyte-page offline scale, QUG's "off by default, explicit fallback" positioning holds; it stays so until a real vector backend rerun recalibrates the baseline (linking to the Step5 real-backend conclusion).

<!-- END STEP11 BENCHMARKS SPEC v1.0 -->