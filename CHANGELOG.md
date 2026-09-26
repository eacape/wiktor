# Changelog

All notable changes to Wiktor are documented in this file per release. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/) once a public API is released.

## [Unreleased]

### Added (Stage 14 — Step 14)

- **Feedback semantic-match abstraction** (`wiktor-feedback`): the `FeedbackKeyMatcher`
  trait with a `StandardKeyMatcher` default, an injectable matcher on the standard
  analyzer (`with_key_matcher` / `analyze_window_with`), and a plug-in proof test.
- **Single-process multi-domain serve** (`wiktor-server`): `ServeOptions.domain_packs`
  accepts multiple packs; the search service dispatches by request `domain` from a
  map of engines and returns `NOT_FOUND` (distinct from the auth 403) for a
  not-served domain.
- **Presentation convergence** (`wiktor-console`): the Web/TUI console discovers
  compiled domains from the kernel instead of a hard-coded `milk-tea`, plus a
  `current_domain` selector.
- **Ops / open-source face**: competitive comparison table, README first-screen
  overhaul, contribution infrastructure (CONTRIBUTING/CODE_OF_CONDUCT/SECURITY),
  crates.io metadata for all seven crates, and ops-consistency fixes in `deploy/`.

## [0.1.0] - unreleased (workspace milestone)

Pre-release milestone carrying Stages 0–?12 (see Step 1–14 specs in `docs/design/`):
workspace/schema/kernel, seed wiki + query loop, QUG build + eval, compile pipeline,
feedback loop, gRPC/HTTP server, consistency state machine, litestream HA, plugin
ecosystem + second domain pack, performance benchmarks + Web console, production
readiness, and Step 14 additions above.

## Keepers

Changelog entries are added continuously as design specs land. Before the first
release, entries may be squashed into the `0.1.0` section.