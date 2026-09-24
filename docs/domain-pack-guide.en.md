# Wiktor Domain-Pack Contribution Guide

> English mirrors the authoritative Chinese `docs/domain-pack-guide.md`. Based on
> `docs/design/step10-plugin-ecosystem.md`.

A Wiktor domain pack is "a directory + `domain.yaml` + data" defining the
knowledge plane (Markdown pages), the fact plane (JSONL/source data) and query
configuration. Official domain-pack examples: `examples/milk-tea` (e-commerce
milk tea) and `examples/tech-docs` (technical documentation, STEP10 B1). Third
parties put packs under `examples/` as a sibling directory, or ship them as a
separate repo and hand the CLI the `--domain` path. Note: the CLI has no
built-in domain-pack registry — passing `--domain <path>/domain.yaml` to the CLI
is the whole assembly.

## Layout

```
examples/my-domain/
├── domain.yaml          # domain config (entities + fact fields + compile + query + qug)
├── seed-wiki/*.md       # knowledge plane: Markdown pages with the frontmatter contract
├── data.jsonl           # fact plane: source data (schema is yours; a deterministic gen_*.py is better)
├── intents.yaml         # (optional) QUG intent templates
├── golden-queries.jsonl # (optional) eval dataset; quotas: synonym25/intent20/negation20/attribute_filter20/negative15
└── README.md
```

## domain.yaml contract

`name` (also the default vector-collection / index name) + `version` (strict
semver). Each entity:
- `name`, `source: jsonl://file.jsonl`, `id_field`, `type_field`
- `fields`: `{name, field_type: text|numeric|boolean|reflist, filterable}`

The `query.filters` whitelist covers the fact fields usable for filtering
(`reflist` / `numeric` etc.); intents must reference fields within that
whitelist. When `qug.enabled`, set `qug.intent_templates: intents.yaml`.

## Knowledge pages

Each `seed-wiki/*.md` must start with YAML frontmatter and split the body by
`##`:

```markdown
---
page_id: my-domain:concept:some-id
entity_id: my-domain:concept:some-id
title: Name
entity_type: concept
aliases: [alias1, alias2]
tags: [tag]
---

Lead-in prose (becomes the `概述` section).

## Section A

Content.

## Section B

Content.
```

`page_id` / `entity_id` are shaped `<domain>:<type>:<id>` (three components, no
colons — Step1 validation).

## Fact plane

Produce the JSONL with a deterministic generator script (`gen_*.py`) rather than
hand-authoring data. Records carry `entity_id`, the fact fields, and optional
`source_revision`. Field names correspond to `domain.yaml` `fields`; the link to
a knowledge page uses `type_field` (e.g. `topic: my-domain:concept:x` pointing at
that page). Field semantics determine how golden filters are expressed
(STEP10 D3).

## Golden filters (domain-generic)

The `filters` in `golden-queries.jsonl` use the domain-generic condition list
`{"conditions": [...]}`, each condition keyed by fact-field name (not
domain-specialized):
- `{"type":"numeric_range","field":"price","min":21,"max":22}`
- `{"type":"text_equals","field":"level","value":"beginner"}`
- `{"type":"ref_contains","field":"tags","refs":["tech-docs:technology:sqlite"]}`
- `{"type":"ref_excludes","field":"ingredient_ids","refs":[...]}`

Empty filters use `{}`. Condition fields must be in the `query.filters` whitelist
and present in the facts table.

## Verify locally

```sh
wiktor seed --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml
# if you ship a golden set
wiktor eval --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml \
  --golden examples/my-domain/golden-queries.jsonl --no-vector
# vectors (when qdrant is available):
wiktor vector build --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml
```

## The three plugin points

A domain pack is one of the three plugin points (the others: data-source
adapters, and the vector backend via the `VectorStore` trait). Plugins depend
only on `wiktor-core`, never on server/feedback (STEP10 D5). To add a vector
backend, implement the `VectorStore` trait and let the CLI/assembler instantiate
it; an external-search-engine outlet (e.g. Meilisearch) is an extra CLI
subcommand and never joins the default query path (STEP10 D6).

## Rebuildability

The knowledge plane rebuilds from Markdown and the fact plane from source JSONL.
A domain pack is therefore a self-contained "code + data" unit — deleting derived
indexes (vectors, QUG) always lets you recompute from the pack.