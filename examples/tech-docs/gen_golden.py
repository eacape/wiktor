#!/usr/bin/env python3
"""Generate examples/tech-docs/golden-queries.jsonl (deterministic, quota-compliant).

生成 examples/tech-docs/golden-queries.jsonl（确定性，配额满足）。

The dataset mirrors milk-tea's shape: 34 legacy records (no id/kind) + 100 new
records hitting the min quotas (synonym 25 / intent 20 / negation 20 /
attribute_filter 20 / negative 15). Queries and expected ids are kept consistent
with the seed pages and the docs.jsonl fact plane so the eval harness runs cleanly.

Usage / 用法: python3 gen_golden.py
"""
import json

# page_id -> (title, aliases). mirrors the seed-wiki frontmatter.
# page_id -> (标题, 别名)。与 seed-wiki frontmatter 对应。
PAGE = {
    "tech-docs:concept:api-design": ("API 设计", ["接口设计", "接口规范"]),
    "tech-docs:concept:semantic-versioning": ("语义化版本", ["SemVer", "版本号规范"]),
    "tech-docs:concept:http": ("HTTP 协议", ["超文本传输协议", "HTTPS"]),
    "tech-docs:concept:grpc": ("gRPC", ["GRPC", "远程过程调用框架"]),
    "tech-docs:concept:rpc": ("RPC", ["远程调用", "过程调用"]),
    "tech-docs:concept:database-indexing": ("数据库索引", ["索引", "聚簇索引"]),
    "tech-docs:concept:idempotency": ("幂等性", ["幂等", "重复执行"]),
    "tech-docs:concept:observability": ("可观测性", ["观测性", "监控"]),
    "tech-docs:concept:backward-compatibility": ("向后兼容", ["兼容性", "回兼容"]),
    "tech-docs:concept:rate-limiting": ("限流", ["限速", "流量控制"]),
    "tech-docs:technology:sqlite": ("SQLite", ["SQLite 数据库", "嵌入式数据库"]),
    "tech-docs:technology:sqlite-wal": ("SQLite WAL", ["写前日志", "WAL"]),
    "tech-docs:technology:sqlite-fts5": ("SQLite FTS5", ["FTS5", "全文索引"]),
    "tech-docs:technology:tokio": ("Tokio", ["异步运行时", "异步编程"]),
    "tech-docs:technology:tonic": ("tonic", ["Rust gRPC", "gRPC 服务端"]),
    "tech-docs:technology:qdrant": ("Qdrant", ["向量数据库", "向量检索"]),
    "tech-docs:practice:error-handling": ("错误处理", ["异常处理", "失败处理"]),
    "tech-docs:practice:documentation-as-code": ("文档即代码", ["文档代码化", "Docs-as-code"]),
    "tech-docs:practice:test-driven": ("测试驱动开发", ["TDD", "先测试"]),
    "tech-docs:practice:monitoring": ("监控实践", ["运维监控", "告警"]),
}
KEYS = list(PAGE.keys())
TITLE = {k: v[0] for k, v in PAGE.items()}
ALIASES = {k: v[1] for k, v in PAGE.items()}


def load_docs():
    """Return {topic: {level:set(), format:set()}} from docs.jsonl.
    从 docs.jsonl 读取 {topic: {level:set, format:set}}。"""
    out = {}
    for line in open("examples/tech-docs/docs.jsonl", encoding="utf-8"):
        r = json.loads(line)
        d = out.setdefault(r["topic"], {"level": set(), "format": set()})
        d["level"].add(r["level"])
        d["format"].add(r["format"])
    return out


def main() -> None:
    docs = load_docs()
    recs = []

    # ---- 34 legacy (no id/kind; exact title/alias hits). ----
    # 34 条 legacy（无 id/kind；精确标题/别名命中）。
    for k in KEYS:
        recs.append({"query": TITLE[k], "expected_hits": [k], "filters": {}})
    # 14 more via aliases (unique enough to resolve to one page).
    # 其余 14 条用别名（足够独特，解析到单页）。
    legacy_aliases = [
        ("接口设计", "tech-docs:concept:api-design"),
        ("版本号规范", "tech-docs:concept:semantic-versioning"),
        ("超文本传输协议", "tech-docs:concept:http"),
        ("嵌入式数据库", "tech-docs:technology:sqlite"),
        ("写前日志", "tech-docs:technology:sqlite-wal"),
        ("全文索引", "tech-docs:technology:sqlite-fts5"),
        ("异步运行时", "tech-docs:technology:tokio"),
        ("向量数据库", "tech-docs:technology:qdrant"),
        ("异常处理", "tech-docs:practice:error-handling"),
        ("文档代码化", "tech-docs:practice:documentation-as-code"),
        ("测试驱动开发", "tech-docs:practice:test-driven"),  # alias==title ok
        ("运维监控", "tech-docs:practice:monitoring"),
        ("流量控制", "tech-docs:concept:rate-limiting"),
        ("聚簇索引", "tech-docs:concept:database-indexing"),
    ]
    for q, page in legacy_aliases:
        recs.append({"query": q, "expected_hits": [page], "filters": {}})
    assert len(recs) == 34, len(recs)

    # ---- synonym 25: alias -> its page. ----
    synonym_i = 0
    seen_aliases = {"HTTP 协议"}  # avoid duplicates with exact titles
    for page in KEYS:
        if synonym_i >= 25:
            break
        for a in ALIASES[page]:
            if a in seen_aliases or a in TITLE.values():
                continue
            seen_aliases.add(a)
            recs.append({
                "id": f"synonym-{synonym_i + 1:02d}", "query": a, "kind": "synonym",
                "expected_entity_ids": [page], "filters": {}, "notes": f"alias of {TITLE[page]}",
            })
            synonym_i += 1
            if synonym_i >= 25:
                break
    assert synonym_i == 25, synonym_i

    # ---- intent 20: phrases that trigger tech-docs/intents.yaml rules. ----
    # Expansion "版本兼容" -> backward-compat; attribute level=beginner -> a
    # beginner-supported page; "不要数据库" is a negation (left to negation bucket).
    intent_specs = [
        ("版本兼容", "tech-docs:concept:backward-compatibility"),
        ("向后兼容", "tech-docs:concept:backward-compatibility"),
        ("远程过程调用", "tech-docs:concept:grpc"),
        ("过程调用", "tech-docs:concept:rpc"),
        ("全文搜索", "tech-docs:technology:sqlite-fts5"),
        ("数据库存储", "tech-docs:technology:sqlite"),
        ("异步编程框架", "tech-docs:technology:tokio"),
    ]
    intent_i = 0
    for q, page in intent_specs:
        if intent_i >= 20:
            break
        recs.append({"id": f"intent-{intent_i + 1:02d}", "query": q, "kind": "intent",
                     "expected_entity_ids": [page], "filters": {}, "notes": "phrase/expansion match"})
        intent_i += 1
    # The beginner attribute (level=beginner) is verified by the B2 unit test;
    # here we just fill the intent quota with phrase queries over the pages.
    # beginner 属性过滤由 B2 单测覆盖；此处仅用页内短语查询填满配额。
    intent_phrase_q = [
        ("版本号", "tech-docs:concept:semantic-versioning"),
        ("请求响应", "tech-docs:concept:http"),
        ("IDL", "tech-docs:concept:grpc"),
        ("倒排", "tech-docs:technology:sqlite-fts5"),
        ("WAL 模式", "tech-docs:technology:sqlite-wal"),
        ("异步", "tech-docs:technology:tokio"),
        ("向量", "tech-docs:technology:qdrant"),
        ("令牌桶", "tech-docs:concept:rate-limiting"),
        ("幂等键", "tech-docs:concept:idempotency"),
        ("链路", "tech-docs:concept:observability"),
        ("重试", "tech-docs:concept:idempotency"),
        ("契约", "tech-docs:concept:api-design"),
        ("破坏性变更", "tech-docs:concept:backward-compatibility"),
        ("倒排索引", "tech-docs:technology:sqlite-fts5"),
    ]
    for q, page in intent_phrase_q:
        if intent_i >= 20:
            break
        recs.append({"id": f"intent-{intent_i + 1:02d}", "query": q, "kind": "intent",
                     "expected_entity_ids": [page], "filters": {}, "notes": "phrase match"})
        intent_i += 1
    assert intent_i == 20, intent_i

    # ---- negation 20: must_exclude semantics. ----
    for i in range(20):
        page = KEYS[i % len(KEYS)]
        recs.append({"id": f"negation-{i + 1:02d}", "query": f"不要{TITLE[page]}", "kind": "negation",
                     "expected_entity_ids": [], "must_exclude_entity_ids": [page],
                     "filters": {}, "notes": f"exclude {TITLE[page]}"})

    # ---- attribute_filter 20: page title + a filter the page's facts support. ----
    af_i = 0
    for page in KEYS:
        if af_i >= 20:
            break
        d = docs.get(page)
        if not d:
            continue
        if "beginner" in d["level"]:
            f = {"type": "text_equals", "field": "level", "value": "beginner"}
        elif "advanced" in d["level"]:
            f = {"type": "text_equals", "field": "level", "value": "advanced"}
        else:
            continue
        recs.append({"id": f"attribute-filter-{af_i + 1:02d}", "query": TITLE[page],
                     "kind": "attribute_filter", "expected_entity_ids": [page],
                     "filters": {"conditions": [f]}, "notes": f"level filter on {TITLE[page]}"})
        af_i += 1
    assert af_i == 20, af_i

    # ---- negative 15: zero-hit nonsense queries. ----
    for i in range(15):
        recs.append({"id": f"negative-{i + 1:02d}", "query": f"不存在的概念术语{i + 1}",
                     "kind": "negative", "expected_entity_ids": [], "filters": {},
                     "notes": "zero-hit negative"})
    assert sum(1 for r in recs if r.get("kind") == "negative") == 15

    total_new = 25 + 20 + 20 + 20 + 15
    assert len(recs) == 34 + total_new, len(recs)

    # Dedup guard: the loader allows at most two records sharing the same
    # (normalized query, filters signature). Guard on raw query + filters JSON,
    # which is stricter than the Rust normalize and catches collisions early.
    # 去重守卫：加载器允许最多两条记录共享同一 (归一化查询, 过滤签名)。此处用
    # 原始查询 + filters JSON 守卫（比 Rust 的 normalize 更严格，提前暴露碰撞）。
    from collections import Counter
    key_counts = Counter((r["query"], json.dumps(r.get("filters", {}), sort_keys=True))
                         for r in recs)
    dups = [(k, n) for k, n in key_counts.items() if n > 2]
    assert not dups, f"dedup collision (same query+filters >2): {dups}"

    out = "examples/tech-docs/golden-queries.jsonl"
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(json.dumps(r, ensure_ascii=False) for r in recs) + "\n")
    print(f"wrote {len(recs)} records to {out} (34 legacy + {total_new} new)")


if __name__ == "__main__":
    main()