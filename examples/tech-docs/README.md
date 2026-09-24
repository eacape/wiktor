# Wiktor 第二个官方领域包：技术文档（tech-docs）

> 验证领域包插件点对非电商域可扩展。英文版对照无独立文件，本文件双语。
> Proves the domain-pack plugin point extends to a non-ecommerce domain.

## 数据 / Data

| 文件 | 作用 |
|---|---|
| `domain.yaml` | 领域配置：实体 `document`，事实字段 topic/level/format/audience_years/tags（filterable） |
| `seed-wiki/*.md` | 20 个技术文档知识页（10 concept + 6 technology + 4 practice） |
| `gen_docs.py` + `docs.jsonl` | 确定性生成 120 条文档事实记录 |
| `intents.yaml` | QUG 意图模板（expansion/attribute/negation） |
| `gen_golden.py` + `golden-queries.jsonl` | 确定性生成 134 条评测 golden（34 legacy + 25 synonym/20 intent/20 negation/20 attribute_filter/15 negative） |

## 用法 / Usage（Linux 或已编译 CLI）

```sh
# 建临时库 + seed
wiktor seed --db /tmp/tech-docs.db --domain examples/tech-docs/domain.yaml

# 评测（离线 mock 向量）
wiktor eval --db /tmp/tech-docs.db --domain examples/tech-docs/domain.yaml \
  --golden examples/tech-docs/golden-queries.jsonl --no-vector
```

## 与 milk-tea 的差异 / Difference from milk-tea

事实平面字段完全不同（topic/level/format/audience_years/tags），
从而驱动 golden 过滤器去 milk-tea 化的泛化改造（STEP10 D2/D3）。