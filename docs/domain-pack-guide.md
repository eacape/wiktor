# Wiktor 领域包贡献指南 / Domain-Pack Contribution Guide

> 中文为权威；英文同文件交叉标注。依据 `docs/design/step10-plugin-ecosystem.md`。
> Chinese is authoritative; English is interleaved. Based on
> `docs/design/step10-plugin-ecosystem.md`.

Wiktor 的领域包（domain pack）是"一个目录 + `domain.yaml` + 数据"，定义了知识平面
（Markdown 页）、事实平面（JSONL/源数据）与查询配置。官方领域包示例：
`examples/milk-tea`（电商奶茶）与 `examples/tech-docs`（技术文档，STEP10 B1）。
第三方领域包应放入 `examples/` 下的独立目录，或作为独立仓库提供给 CLI 的 `--domain`。
说明：CLI 没有内置领域包注册表——给 CLI 传 `--domain <path>/domain.yaml` 即完成装配。

## 目录结构 / Layout

```
examples/my-domain/
├── domain.yaml          # 领域配置（实体 + 事实字段 + compile + query + qug）
├── seed-wiki/*.md       # 知识平面：Markdown 页，frontmatter 契约
├── data.jsonl           # 事实平面：源数据（方案自定，gen_*.py 确定性生成更佳）
├── intents.yaml         # （可选）QUG 意图模板
├── golden-queries.jsonl # （可选）评测数据集，配额：synonym25/intent20/negation20/attribute_filter20/negative15
└── README.md
```

## domain.yaml 契约 / Contract

`name`（也是向量集合名 / index 名默认值）+ `version`（严格 semver）。每个实体：
- `name`、`source: jsonl://file.jsonl`、`id_field`、`type_field`
- `fields`：`{name, field_type: text|numeric|boolean|reflist, filterable}`

`query.filters` 白名单要通信 `reflist` / `numeric` 等可用于过滤的事实字段；
intents 引用的字段必须在该白名单内。`qug.enabled` 时 `qug.intent_templates: intents.yaml`。

## 知识平面页 / Knowledge pages

每个 `seed-wiki/*.md` 必须以 YAML frontmatter 开头，body 用 `##` 分节：

```markdown
---
page_id: my-domain:concept:some-id
entity_id: my-domain:concept:some-id
title: 名称
entity_type: concept
aliases: [别名1, 别名2]
tags: [标签]
---

概览正文（成为 `概述` 节）。

## 节A

内容。

## 节B

内容。
```

`page_id` / `entity_id` 形如 `<domain>:<type>:<id>`（三组件，不能含冒号，Step1 校验）。

## 事实平面 / Fact plane

用确定性生成脚本（如 `gen_*.py`）产出 JSONL，避免手工造数据。记录含
`entity_id`、各事实字段，以及可选的 `source_revision`。字段名与 `domain.yaml`
`fields` 一一对应；与知识页的关联用 `type_field`（如 `topic: my-domain:concept:x`
指向该知识页）。字段语义决定了 golden 过滤用法（STEP10 D3）。

## golden 过滤（领域通用） / Golden filters (domain-generic)

`golden-queries.jsonl` 的 `filters` 用领域通用条件列表 `{"conditions":[...]}`，
每个条件按事实字段名表达（不是域特化）：
- `{"type":"numeric_range","field":"price","min":21,"max":22}`
- `{"type":"text_equals","field":"level","value":"beginner"}`
- `{"type":"ref_contains","field":"tags","refs":["tech-docs:technology:sqlite"]}`
- `{"type":"ref_excludes","field":"ingredient_ids","refs":[...]}`

空过滤用 `{}`。条件字段必须已在 `query.filters` 白名单内且事实表中有该字段。

## 如何在本地验证 / Verify locally

```sh
wiktor seed --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml
# 如果带了 golden
wiktor eval --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml \
  --golden examples/my-domain/golden-queries.jsonl --no-vector
# 向量（有 qdrant）：
wiktor vector build --db /tmp/my-domain.db --domain examples/my-domain/domain.yaml
```

## 三个插件点 / The three plugin points

领域包是三大插件点之一（其余：数据源适配器、向量后端 `VectorStore` trait）。
插件只依赖 `wiktor-core`，不依赖 server/feedback（STEP10 D5）。加一个向量后端
只需实现 `VectorStore` trait 并让 CLI/装配方实例化；外部检索引擎出口
（如 Meilisearch）走额外 CLI 子命令，不进默认查询路径（STEP10 D6）。

## 可重建承诺 / Rebuildability

知识平面可从 Markdown、事实平面可从源数据 JSONL 全量重建。领域包因此是
"代码 + 数据"的自包含单元——删掉派生索引（向量、QUG）后总能从领域包重算回来。