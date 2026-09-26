# Step 14 (P3-C) Design Spec: Feedback Semantic-Matching Abstraction (pluggable query-key normalization)

> Version: v1.0 (2026-09-26)
> Authority: `docs/MASTER-PLAN.md` §5.3 compile↔retrieval feedback loop, §8 core abstractions; Step 6 spec `step6-feedback-loop.md` §6 (analyzer contract)
> Implemented object: `FeedbackKeyMatcher` abstraction + default impl (reusing the existing normalize) + analyzer injection (implemented by the lead model; this document registers the spec)

## 1. Background: Original Wording vs Actual Code

The P3 sub-item "feedback semantic-matching abstraction" was originally phrased: *"feedback semantic matching is currently normalized-title exact equality; make it pluggable."*

**After inspecting the code, the gap must be clarified**:

1. **The feedback storage layer has no "title exact-equality matching" at all.** `FeedbackEventInput` (`feedback_store.rs`) carries `page_id: Option<String>` directly; click/adopt require it (≤512, D3); the insert transaction `insert_feedback_idempotent_on_conn` only validates log_id existence + domain match + `page_exists` (page_id is an accepted page of the domain). **Page attribution is done upstream when the event is built, not by the storage layer.**
2. **The feedback analyzer is already a pluggable trait.** `FeedbackAnalyzer { async fn analyze(FeedbackWindow) -> Result<FeedbackReport> }`, with the standard impl `StandardFeedbackAnalyzer` (carrying only `min_events`).
3. **The real coupling**: the analyzer *hard-codes* `use wiktor_core::query_engine::qug::normalize;` (`analyzer.rs:62`) and pins `(domain, normalized query_text)` as the dedup key for signals 1/2 (zero-recall / rewrite-failure) via `normalized_log_query` (`analyzer.rs:401`). That is exactly where "normalization = matching/dedup key" is nailed down. Signal 3 (low quality) aggregates by `page_id`; `rate` events may lack a page_id (the DDL allows it) and cannot be attributed ( `analyzer.rs:272-282`).

**Conclusion**: P3-C's "pluggable" landing point is not the storage layer but the **analyzer's query-key normalization** — extract the hard-coded `normalize` call into a pluggable trait whose default impl preserves current behavior (offline, deterministic, reusing the Step 3 §3.1 contract), so a future semantic/vector key can replace it without rewriting the analyzer rules.

## 2. Decision: What Is Done and What Is Not

### Direction A (recommended, this sub-item) — abstract query-key normalization
Extract "QueryLogSnapshot → dedup key" normalization into a trait whose default impl is the existing `normalize`. Rationale:
- **Fits the code**: this is the only real hard-coded coupling; abstracting it changes behavior in no way and is low-risk.
- **Low cost**: a thin trait + a default impl wrapping `normalize`, leaving the three signal rules of the analyzer untouched.
- **Genuinely pluggable**: a future LLM/vector semantic key is one new impl + a wiring swap, with no rule changes.
- **Keeps the offline baseline**: the default impl stays deterministic, network-free, pure-function — honoring the user's "low wheel-reinvention" preference and the project iron rule "no LLM on the default path".

### Direction B (explicitly not done; registered as follow-up) — semantic matching
Introduce an abstraction that semantically matches `rate` events lacking page_id, or future free-text feedback, to pages. **Not done in this sub-item**, because:
- Existing events already carry page_id built upstream by server/CLI, and SQLite validation suffices; rateless `rate` events are a minority whose degraded path (cannot attribute → ignored for the page criterion) already exists and is sound.
- Semantic/LLM matching is costly and its **real gain to the production feedback loop is unverified**; shipping it would risk mis-attributing `rate` to the wrong page — worse than not attributing.
- Direction A's trait already reserves the extension point: a new impl is all that is needed; no re-planning required.

## 3. Abstraction Shape

```rust
/// Feedback dedup-key matcher: normalizes a log row to the stable key used for
/// signal-1/2 dedup aggregation. The default impl reuses
/// `query_engine::qug::normalize` (the Step 3 §3.1 contract).
#[async_trait::async_trait]
pub trait FeedbackKeyMatcher: Send + Sync {
    /// Normalizes one query log into a stable dedup key; on failure the row
    /// fails closed inside `analyze`.
    fn normalize(&self, query_text: &str) -> Result<String>;
}
```

- **Default impl** `StandardKeyMatcher`: wraps `qug::normalize`, byte-for-byte identical to current behavior (trim + collapse whitespace + lowercase + non-empty + ≤MAX_PHRASE_SCALARS).
- **Injection point**: `StandardFeedbackAnalyzer` gains a `Box<dyn FeedbackKeyMatcher>` (default `StandardKeyMatcher`); `analyze_window_with` uses `self.matcher.normalize(...)` instead of the direct `normalize` inside `normalized_log_query`.
- **Error semantics**: normalization failure keeps the current behavior — the row fails closed (analyzer returns Err), never silently dropped.

## 4. Cooperation with Existing Surfaces

- **`feedback_store`·`page_exists`**: untouched. Page attribution continues to validate accepted pages by page_id in the storage layer; this abstraction only concerns the analyzer's signal-1/2 dedup key.
- **Signals 1/2**: use the injected matcher to build the `(domain, normalized query_text)` key.
- **Signal 3 (low quality)**: still aggregates by `page_id`; not part of this abstraction (the rateless-rate degraded path is preserved).

## 5. Boundaries and Invariants

- **Idempotency**: the matcher is a pure function; the same query_text always yields the same key (guaranteed by the default impl); a semantic replacement must preserve this invariant.
- **Determinism**: report output order still follows the normalized key / page_id byte order; the matcher does not change the sorting contract.
- **Offline baseline**: the default impl is zero-network, zero-model, pure in-memory.

## 6. Files Involved

- `crates/wiktor-feedback/src/analyzer.rs`: add the `FeedbackKeyMatcher` trait + `StandardKeyMatcher`; `StandardFeedbackAnalyzer` carries a matcher; `normalized_log_query` / signals 1/2 use the injected one.
- `crates/wiktor-feedback/src/lib.rs`: re-export `FeedbackKeyMatcher`, `StandardKeyMatcher` (optional).
- Tests: existing analyzer unit tests must stay green (the default impl is byte-identical); add one substitutability test (a custom matcher injected → signals 1/2 aggregate by the new key, proving pluggability).

## 7. Not-Done List

- Direction B (semantic/LLM attribution) is not done; reason in §2, registered as a follow-up extension point.
- No change to the storage-layer page_id validation, no change to the signal-3 rules, no change to `insert_feedback_idempotent`.