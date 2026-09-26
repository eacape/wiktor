# Step 14 (P5) Design Spec: Ops / Open-Source Face

> Version: v1.0 (2026-09-27)
> Authority: `docs/MASTER-PLAN.md` §「open-source cold-start is hard」risk countermeasure + outstanding item「competitive comparison table (complete before writing into README)」
> Implemented by: README open-source first-screen overhaul + competition table + contribution infrastructure + crates.io metadata + ops consistency (implemented by the main model; this doc registers the spec)

## 1. Background & Goals

Wiktor's "ops/engineering" foundation is already solid and self-contained: `deploy/` ships install, dual systemd units, litestream backup/restore, smokes, Prometheus/alerts, and a bring-up runbook; CI (fmt/clippy/test) runs; a bilingual MASTER-PLAN + 20 step specs exist. The real P5 gap is concentrated in **four open-source-face** items plus **one ops-consistency** item, all delivered in this Step.

Five blocks:
1. **Competitive / comparative table** — clears the MASTER-PLAN §14 outstanding item ("competitive comparison table (complete before writing into README)").
2. **README open-source first screen overhaul** — fills the standard open-source README gaps (install paths, issue/discussion entry, roadmap, CI/crates.io badges, license detail), bilingual, and refresh the stale Status.
3. **Contribution infrastructure** — CONTRIBUTING / CODE_OF_CONDUCT / SECURITY / CHANGELOG + CI badge.
4. **crates.io metadata** — workspace-package description/homepage/documentation + per-crate descriptions completed and anglicized.
5. **Ops consistency** — deploy README gains rollback / env catalog / observability, and the metrics-family comment drift (6 → 8) is fixed.

## 2. Status survey (design premises)

- **README.md (English primary + README.CN.md mirror)**: top logo/tagline/3 badges + intro + Core features(7) + Quick start(5) + Crate layout + Documentation map + Status + License. No dead links. **Status is stale**: it reads "Steps 1–11 … 397 green", but is actually **Steps 1–14 all delivered, workspace 400+ green** (HEAD=62510b9).
- **Missing**: screencast/demo, `cargo install`/prebuilt-binary/Docker install section, Issues/discussion entry, competitive table, roadmap, contribution-guide entry, CI/crates.io/docs.rs badges, license detail.
- **Contribution surface**: no CONTRIBUTING/CODE_OF_CONDUCT/SECURITY/CHANGELOG at top level; `.github/` has only `workflows/ci.yml`.
- **crates.io metadata**: `[workspace.package]` has version/edition/rust-version/license/repository, **lacks description/homepage/documentation**; of 7 crates, 4 (core/cli/feedback/server) lack a description and 3 have Chinese descriptions (adapter/console/vector-qdrant) — improper for crates.io international audience.
- **Ops**: `deploy/README.md` covers Quick start/Ops/disaster-recovery/Bring-up/upgrade, but lacks a rollback flow, a full environment-variable table, an observability walk, and `/health` semantics; `metrics.rs` comment and `prometheus.yml` claim "6 fixed metrics" while the code actually emits **8 families** (+ `wiktor_query_latency_ms_bucket` + `wiktor_compile_status_total`).
- **Competition table**: only the MASTER-PLAN outstanding line, no actual table.

## 3. Comparative table (block 1, highest priority)

MASTER-PLAN explicitly requires "complete before writing into README". Dimensions: **compile observability / plugin ecosystem / open vs hosted / retrieval quality / deployment shape** (with Meilisearch as baseline). Rows: namespace Wiktor, Meilisearch, OpenSearch, Vectara, Pinecone, Weaviate, LangChain, WeKnora, rqlite, plus the "engineering-baseline" etcd/Redis (in R&D narrative, not in the table).

**Placement**: a new `<h2> "Wiktor in context"` in README using markdown table, one-line summary per cell, honest (admit "retrieval quality not peer-reviewed / 实测 recall@1 0→1"), conclusion: "Wiktor's distinctive slot = LLM knowledge compilation + quality gate + feedback-observable close-loop, not a vector store."

**Implementation**: README section + clear the MASTER-PLAN outstanding item (§遗留 items "completed P5"). Bilingual.

## 4. README first-screen overhaul (block 2)

Keep the existing logo/tagline/badge structure while filling gaps (both Chinese and English):

- **Badge row**: add `CI status` (GitHub Actions), `crates.io`, `docs.rs`.
- **Quick start**: add `cargo install wiktor` and a "prebuilt binary" section that points to GitHub Releases, with a note "placeholder, released only when shipping CHANGELOG-scoped release in a future step".
- **New "Roadmap"** block: split out "next steps" from Status — production hardening (Raft HA, cluster sharding), real-benchmark publication, community plugins, release automation.
- **New "Getting help / Community"**: GitHub Issues vs Discussions.
- **Status fix**: Steps 1–11 → **1–14** (incl. the feedback semantic-match / multi-domain), test count 397 → **cargo test --workspace 435+** (measure), keep the recall quoted data.
- **License section**: one sentence on Apache-2.0 commercial/commercial fragment provision.

## 5. Contribution infrastructure (block 3)

New top-level files (English-only or bilingual; README links them):
- `CONTRIBUTING.md` — build/precheck/pre-test instructions + contribution flow (issue→PR→review) + bilingual doc rule.
- `CODE_OF_CONDUCT.md` — Contributor Charter 2.1 in English.
- `SECURITY.md` — report channel (GitHub Security advisory / issues) + supported scope (currently stable main).
- `CHANGELOG.md` — version list driven by git log (current 0.1.0 → later released by versioning).

`.github/`: `ISSUE_TEMPLATE/bug_report.md`, `ISSUE_TEMPLATE/feature_request.md`, and `PULL_REQUEST_TEMPLATE.md`.

## 6. crates.io metadata (block 4)

- Root `Cargo.toml` `[workspace.package]` add `description` (one-line English project summary), `homepage` (= GitHub repo), `documentation = "https://docs.rs/<registry>"`.
- Each crate `Cargo.toml`: complete all missing descriptions and anglicize existing Chinese ones (adapter/console/vector-qdrant).
- Do **not** publish a release this round (P5 merely prepares the surface).

## 7. Ops consistency (block 5)

- `deploy/README.md`: add 4 sections — **Rollback** (upgrade failure → restore old binary + fallback Prestashop snapshot), **Environment variables** (full env index table, from `wiktor.env.example` and source grep of all `WIKTOR_*`), **Observability** (the /metrics 8-family walkthrough, Prometheus scrape targets, the `alerts-...yml` rule and Alertmanager channel), **Health** (`/health` semantics = liveness, including kernel health injection).
- `crates/wiktor-server/src/metrics.rs` comment + `deploy/prometheus.yml` L4-5 fix the "6 fixed metrics" → "8 families (…)".

## 8. Acceptance criteria

1. README first screen includes the Comparison file, Roadmap, Community, cargo install, correct Status (Steps 1–14, real test count).
2. 4 new top-level docs + 3 .github templates, README Documentation map links to them.
3. workspace package metadata, 4 crates' descriptions in English; crates.io metadata prepared.
4. deploy/README contains rollback / env table / observability / health; metrics family counts update to 8.
5. MASTER-PLAN outstanding "competitive comparison table" — clear.
6. Rust changes only metadata/comments: `cargo build --workspace` green (incl. new doc config), `cargo fmt --check` clean, markdown dead-link → zero.

## 10. Engineering adoption & sync flow

- Pure docs + Cargo metadata, no business-logic risk: main model implements directly (no child agent needed).
- Follow the established sync protocol: local edits → tar over SSH (`--exclude='._*'`) → Linux squash commit → push GitHub → local reset align.

## 9. Deviation record

- **STEP-P5-001 (comparison table lands in README, not a standalone file)**: the spec only requires "a <h2> in the README"; the implementation put it in the "Wiktor in context" / "对标与定位" sections, one table per language (9 rows incl. rqlite as the kernel analog). The 'kernel analog' and 'vector backend qdrant' rows are engineering-baseline framing, not direct competitors, and are not overstated against Vectara et al. The existing "Meilisearch-grade retrieval, etcd-grade reliability" framing is kept as-is, with the engineering baseline noted in the table header.
- **STEP-P5-002** (workspace `documentation` points to docs.rs/wiktor): the root `[workspace.package].documentation = "https://docs.rs/wiktor"` is only an inheritance source; every crate uses an explicit `documentation = "https://docs.rs/wiktor-<crate>"`, so the workspace value never lands in released metadata (wiktor itself is not a standalone crate). Kept as the get-default placeholder.
- **STEP-P5-003** (all crate descriptions in English): all 7 crate descriptions anglicized; `wiktor-console` reworded to emphasize the read-only supervisory face (no functional change).
- **STEP-P5-004** (ops-consistency fix is comment-only): `metrics.rs` and `prometheus.yml` comments changed from "6 fixed metrics" to "8 families"; no metrics rendering behavior changes (8 families have existed since Step11 B4).