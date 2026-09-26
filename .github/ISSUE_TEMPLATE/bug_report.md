---
name: Bug report
about: Report a reproducible defect
title: "[bug] "
labels: bug
---

## Summary
A clear, one-paragraph description of the bug.

## Reproduction steps
1. Command / setup (e.g. `wiktor compile --db ... --provider mock`)
2. What happens
3. What you expected instead

## Environment
- Wiktor version / commit: (e.g. `62510b9` or release tag)
- Rust: `rustc -V`
- OS: (e.g. macOS arm64, Debian 13)
- Backends used: mock / qdrant (`WIKTOR_VECTOR_BACKEND`) / Meilisearch / none

## Diagnostics
Paste any `--json` output, search `diagnostics`, `/metrics` sample, or logs that help
narrow it down. Strip secrets/keys.

## Possible cause
Optional — if you have a hypothesis, note it here.