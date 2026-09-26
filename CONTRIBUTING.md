# Contributing to Wiktor

Thanks for your interest in contributing to Wiktor, a compiling knowledge-retrieval
database. This guide covers how the repository is organized, how to build and test,
and the workflow for getting a change in. English is the primary contribution
language; project documentation is browsable in Chinese (`*.md`) and English (`*.en.md`).

## Code of Conduct

We expect every participant to follow our [Code of Conduct](CODE_OF_CONDUCT.md).
Interactions in issues, PRs, and community spaces should stay professional and
constructive.

## Repository layout

- `crates/wiktor-core` — the kernel: two-plane SQLite store, `QueryEngine`, QUG,
  the compile pipeline, consistency/compatibility arbiters, the Mock vector baseline.
- `crates/wiktor-cli` — the single `wiktor` binary and all `wiktor <command>` subcommands.
- `crates/wiktor-feedback` — the feedback-store trait and standard analyzer.
- `crates/wiktor-server` — the gRPC + HTTP service face.
- `crates/wiktor-vector-qdrant` — the qdrant vector-backend plugin.
- `crates/wiktor-adapter-meilisearch` — the Meilisearch retrieval-outlet plugin.
- `crates/wiktor-console` — the Web + TUI console.
- `docs/` — bilingual design docs: `MASTER-PLAN.md` (architecture, invariants),
  `PLAN.md` (phased execution), `design/` (per-step specs + recorded deviations).
- `examples/` — the two official domain packs (`milk-tea`, `tech-docs`).
- `deploy/` — production deployment scripts and runbooks.

## Building

```bash
cargo build --workspace
cargo build -p wiktor-cli              # the CLI (default features)
cargo build -p wiktor-cli --features server,console   # + service/console surface
```

A Rust toolchain (currently 1.85+) and `protoc` (for the gRPC generated code in
`wiktor-server`) are required.

## Testing and gates

Before opening a PR, make sure all three gates are green:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-features -- -D warnings
cargo test --workspace
```

Some suites are offline-only (Mock compiler / deterministic embeddings / in-memory
kernels). The qdrant and Meilisearch plugin crates keep their live integration tests
behind a marker so the offline gate stays hermetic.

## Workflow

1. Open an issue describing the problem or feature, or pick an existing one.
2. Create a feature branch: `git checkout -b my-change`.
3. Make your changes, keeping them focused on the issue; add tests for behaviour.
4. Run the gates above locally.
5. Open a pull request against `main` and reference the issue.
6. Keep the change small and reviewable — one issue per PR.
7. A maintainer reviews and merges. After merge the topic branch is deleted.

For design-heavy changes (new crate, schema migration, a new algorithm), the project
convention is to produce a short design spec under `docs/design/` (bilingual
`*.md` + `*.en.md`) before implementation, and to record any implemented deviation
in the spec's deviation table. Prefer discussing such designs in the issue before
writing code.

## Documentation & bilingual rule

- Docs in `docs/` are bilingual: the Chinese file is authoritative
  (`docs/design/foo.md`) and `foo.en.md` mirrors it.
- The root README is English-primary (`README.md`) with a Chinese mirror
  (`README.CN.md`).
- When your change alters behaviour, update the matching spec/code comment and, if
  user-facing, the README.

## Reporting bugs / security

Please see [SECURITY.md](SECURITY.md) for the responsible-disclosure policy, and
[file an issue](https://github.com/eacape/wiktor/issues) for anything else.