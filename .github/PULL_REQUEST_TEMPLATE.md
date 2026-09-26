## What / why
Short description of the change and the issue it addresses (e.g. "Closes #123").

## Scope
- Crate(s) / files touched.
- Whether it changes behaviour, schema, CLI, or docs only.

## Tests & gates
- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --workspace --all-features -- -D warnings` clean
- [ ] `cargo test --workspace` green
- [ ] Added/updated tests for the behaviour (if code change)

## Design conventions
- Bilingual docs: is a `docs/design/*.md` spec + `*.en.md` needed / updated?
- Deviation table updated in the relevant spec if implementation differs?

## Notes for reviewer
Anything to verify carefully (e.g. reliability contract invariants, migration
guards, plugin boundaries).