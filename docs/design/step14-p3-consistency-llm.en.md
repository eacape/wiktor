# Step 14 (P3-A) Design Spec: Consistency Arbitration via LLM (pluggable + env-driven, CLI/worker source-identical)

> Version: v1.0 (2026-09-26)
> Authority: `docs/MASTER-PLAN.md` §8 core abstractions (plugin points), §5.x compile-consistency gate; Step 8 spec §6.1/§6.2 and decisions D1–D4
> Implemented object: `LlmConsistencyArbiter` + `LlmClient` abstraction + env-driven wiring shared by `wiktor-server`/`wiktor-cli` (implemented by the lead model; this document registers the spec)

## 1. Background and Goal

Step 8 delivered only one consistency-arbitration implementation: the deterministic `SourceRefConsistencyArbiter` (X5: no free semantic inference — compares only `(entity_id, pointer)` groups via canonical_json exact comparison). It was hard-wired separately in `compile.rs` and `worker.rs`.

P3-A turns consistency arbitration into **pluggable + env-driven**:

1. The core depends only on the `ConsistencyArbiter` abstraction (already true); a new `LlmConsistencyArbiter` becomes the second implementation using a real LLM;
2. `wiktor-server` and `wiktor-cli` wiring is **source-identical** — one env contract, defaulting back to the deterministic baseline so offline runs are unchanged;
3. LLM arbitration is constrained by existing contracts: **it cannot bypass evidence filtering / top-k / budget**, and one arbitration is exactly one LLM request.

> Note: "filter pushdown" from the original P3 list was already delivered in Step 3 (`docs/MASTER-PLAN.md` §17 item 3: "index → QUG/fallback → filter pushdown → CLI display ✅", 2026-09-21); it is not a P3-A todo. "Feedback semantic-matching abstraction" has no registered spec and is deferred to design (see §7).

## 2. Deliverables

| # | Deliverable | Status |
|---|---|---|
| P3-A1 | `LlmClient` abstraction reuse (`compile::llm::LlmClient`, async `complete`) | Done (from Step0) |
| P3-A2 | `LlmConsistencyArbiter` (`consistency.rs`, gated by feature `llm-openai`) | Done |
| P3-A3 | worker/CLI source-identical env wiring (`build_server_consistency_arbiter` / `build_cli_consistency_arbiter`) | Done |
| P3-A4 | Bilingual spec registration (this document) | This document |

## 3. Non-bypassable Contracts

- The core depends only on `ConsistencyArbiter`; **any future arbiter can only implement the same trait** and cannot bypass the explicit `compare_pointers`, top-k bounded recall, or evidence budget.
- **Evidence is filtered on the core side (the same D3 rule)**: `LlmConsistencyArbiter::arbitrate` first runs `comparable_refs` to keep only comparable source-refs — the LLM **only ever sees comparable evidence**, never mixed free semantic input.
- **One arbitration = exactly one LLM request**: a single `LlmRequest` is assembled (`CONSISTENCY_SYSTEM_PROMPT` + candidate/related/compare_pointers); `max_output_tokens` is supplied by the wiring (worker/CLI pass 512).
- **Diagnostics store only BLAKE3 digests, never raw values** (A7 carried forward): the LLM verdict is parsed into findings on the core side and only `candidate_value_hash`/`evidence_value_hash` are persisted.
- **A missing key is a config error, not a mock fallback** (D7 carried forward, consistent with compiler wiring); but **consistency arbitration is a new surface and opts in explicitly** — setting a URL does not enable it; `WIKTOR_CONSISTENCY_LLM=1` is required.

## 4. env Wiring Contract (worker/CLI source-identical)

| env | Semantics |
|---|---|
| `WIKTOR_CONSISTENCY_LLM` | opt-in: only `1`/`true` (case-insensitive) may enable LLM arbitration |
| `WIKTOR_LLM_BASE_URL` | non-empty → use OpenAI-compatible `OpenAiLlmClient`; empty/unset → deterministic fallback |
| `WIKTOR_LLM_MODEL` | model, defaulting to `qwen3.8-max` |
| `WIKTOR_OPENAI_API_KEY` (`API_KEY_ENV`) | key; if missing, `OpenAiLlmClient::new` returns Err → deterministic fallback with an eprintln notice |

**Decision chain**: `opt_in ∧ base_url non-empty ∧ (feature llm-openai)` → attempt to build `LlmConsistencyArbiter`; construction failure or any unmet condition → `SourceRefConsistencyArbiter` (offline baseline, always available). When `cfg(feature="llm-openai")` is off, it silently falls back to deterministic (the server side emits an eprintln notice).

## 5. Source-identical Wiring

- `crates/wiktor-server/src/worker.rs` `CompileWorker::from_domain_pack`: when `consistency.enabled`, it wires `build_server_consistency_arbiter()` + `SqliteFtsCandidateProvider` (kernel.clone()) into the executor; when disabled it keeps the None path (no arbitration). Wiring chains a bare `PipelineExecutor` with `.with_consistency_arbiter(..).with_candidate_provider(..)` and only then `Arc::new` — source-identical to the CLI, avoiding a move out of an `Arc`.
- `crates/wiktor-cli/src/compile.rs` `build_cli_consistency_arbiter()`: the same env decision chain.

## 6. Files and Tests

- `crates/wiktor-core/src/compile/consistency.rs`: `ComparableRef`, `LlmConsistencyArbiter` (`new`: client + model + max_output_tokens), `CONSISTENCY_SYSTEM_PROMPT`; the deterministic implementation under the same trait is unchanged.
- `crates/wiktor-core/src/compile/llm.rs`: `LlmClient` trait (async `complete(LlmRequest)->Result<LlmResponse,CompileFailure>`), `OpenAiLlmClient`, `API_KEY_ENV`.
- `crates/wiktor-cli/src/compile.rs`, `crates/wiktor-server/src/worker.rs`: source-identical wiring.
- Tests: the `compile::consistency` module has 18 passing tests — key ones are `fake_arbiter_is_substitutable` (proves trait substitutability = the pluggable contract), `empty_pointer_table_is_deterministic_none`, `cross_domain_keys_never_compare`; the mock uses `impl LlmClient for MockLlm`.

## 7. Outstanding: P3-C Feedback Semantic-Matching Abstraction (to be designed)

Today feedback events are matched to pages by "normalized-title exact equality". Abstracting this into a pluggable matcher requires a spec first (trait shape, injection point, cooperation with `insert_feedback_idempotent`, offline baseline). This document does not implement it unsolicited — per "spec before implementation", it is scheduled pending a user decision on scope (whether it stays in P3 or is split into a follow-up).
