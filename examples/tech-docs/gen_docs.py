#!/usr/bin/env python3
"""Generate examples/tech-docs/docs.jsonl (deterministic, ~120 document entries).

生成 examples/tech-docs/docs.jsonl（确定性随机，约 120 条文档事实记录）。

The fact plane of the tech-docs domain models docs-as-code entries: each document
relates to a primary knowledge page (`topic`), carries an audience `level`, a
`format` (concept/howto/reference), a numeric `audience_years`, and a reflist of
related `tags`. These differ entirely from milk-tea's price/sugar/size/ingredients,
which exercises the domain-generic golden-filter design (STEP10 D2/D3).

Usage / 用法: python3 gen_docs.py [count] [seed]
"""
import json
import random
import sys

DEFAULT_COUNT = 120
DEFAULT_SEED = 20260924

# All 20 knowledge pages (concept / technology / practice).
# 全部 20 个知识页（概念 / 技术 / 实践）。
CONCEPTS = [
    "tech-docs:concept:api-design",
    "tech-docs:concept:semantic-versioning",
    "tech-docs:concept:http",
    "tech-docs:concept:grpc",
    "tech-docs:concept:rpc",
    "tech-docs:concept:database-indexing",
    "tech-docs:concept:idempotency",
    "tech-docs:concept:observability",
    "tech-docs:concept:backward-compatibility",
    "tech-docs:concept:rate-limiting",
]
TECHNOLOGIES = [
    "tech-docs:technology:sqlite",
    "tech-docs:technology:sqlite-wal",
    "tech-docs:technology:sqlite-fts5",
    "tech-docs:technology:tokio",
    "tech-docs:technology:tonic",
    "tech-docs:technology:qdrant",
]
PRACTICES = [
    "tech-docs:practice:error-handling",
    "tech-docs:practice:documentation-as-code",
    "tech-docs:practice:test-driven",
    "tech-docs:practice:monitoring",
]
TOPICS = CONCEPTS + TECHNOLOGIES + PRACTICES
ALL_KEYS = list(TOPICS)

# A short Chinese doc template per topic (title + one-line description).
# 每个 topic 的中文文档模板（标题 + 一行说明）。
TOPIC_DOC = {
    "tech-docs:concept:api-design": ("接口契约设计指南", "如何设计稳定自解释的公开接口"),
    "tech-docs:concept:semantic-versioning": ("语义化版本规范", "用版本号传达兼容性信息"),
    "tech-docs:concept:http": ("HTTP 协议速览", "请求响应模型与状态码语义"),
    "tech-docs:concept:grpc": ("gRPC 快速上手", "IDL 先行的高性能远程调用"),
    "tech-docs:concept:rpc": ("RPC 基础原理", "远程过程调用的本地透明抽象"),
    "tech-docs:concept:database-indexing": ("索引原理", "B 树与全文索引的取舍"),
    "tech-docs:concept:idempotency": ("幂等设计", "幂等键与安全重试"),
    "tech-docs:concept:observability": ("可观测性入门", "日志指标链路三支柱"),
    "tech-docs:concept:backward-compatibility": ("向后兼容策略", "平滑演进而非破坏升级"),
    "tech-docs:concept:rate-limiting": ("限流算法", "令牌桶与固定窗口"),
    "tech-docs:technology:sqlite": ("SQLite 手册", "嵌入式零配置数据库"),
    "tech-docs:technology:sqlite-wal": ("WAL 模式详解", "写前日志与读写并发"),
    "tech-docs:technology:sqlite-fts5": ("FTS5 全文检索", "倒排索引与 BM25 排序"),
    "tech-docs:technology:tokio": ("Tokio 指南", "Rust 异步运行时"),
    "tech-docs:technology:tonic": ("tonic 教程", "Rust gRPC 实现"),
    "tech-docs:technology:qdrant": ("Qdrant 文档", "开源向量数据库"),
    "tech-docs:practice:error-handling": ("错误处理实践", "类型化错误与可观测失败"),
    "tech-docs:practice:documentation-as-code": ("文档即代码", "把文档当作一等交付物"),
    "tech-docs:practice:test-driven": ("测试驱动开发", "红绿重构循环"),
    "tech-docs:practice:monitoring": ("监控实践", "告警质量与健康水位"),
}

LEVELS = ["beginner", "advanced"]
FORMATS = ["concept", "howto", "reference"]

# Tag affinity: which related pages tag a doc, per topic family.
# 每个 topic 家族的可选关联 tag。
CONCEPT_TAG_POOL = {
    "tech-docs:concept:api-design": ["tech-docs:concept:semantic-versioning",
                                     "tech-docs:concept:http", "tech-docs:concept:idempotency"],
    "tech-docs:concept:semantic-versioning": ["tech-docs:concept:backward-compatibility",
                                              "tech-docs:concept:api-design"],
    "tech-docs:concept:http": ["tech-docs:concept:api-design", "tech-docs:concept:rpc"],
    "tech-docs:concept:grpc": ["tech-docs:concept:rpc", "tech-docs:technology:tonic", "tech-docs:concept:http"],
    "tech-docs:concept:rpc": ["tech-docs:concept:grpc", "tech-docs:concept:http"],
    "tech-docs:concept:database-indexing": ["tech-docs:technology:sqlite", "tech-docs:technology:sqlite-fts5"],
    "tech-docs:concept:idempotency": ["tech-docs:concept:backward-compatibility"],
    "tech-docs:concept:observability": ["tech-docs:practice:monitoring"],
    "tech-docs:concept:backward-compatibility": ["tech-docs:concept:semantic-versioning", "tech-docs:concept:idempotency"],
    "tech-docs:concept:rate-limiting": ["tech-docs:concept:http"],
}
TECH_TAG_POOL = {
    "tech-docs:technology:sqlite": ["tech-docs:technology:sqlite-wal", "tech-docs:technology:sqlite-fts5", "tech-docs:concept:database-indexing"],
    "tech-docs:technology:sqlite-wal": ["tech-docs:technology:sqlite"],
    "tech-docs:technology:sqlite-fts5": ["tech-docs:technology:sqlite", "tech-docs:concept:database-indexing"],
    "tech-docs:technology:tokio": [],
    "tech-docs:technology:tonic": ["tech-docs:technology:tokio", "tech-docs:concept:grpc"],
    "tech-docs:technology:qdrant": ["tech-docs:concept:database-indexing"],
}
PRACTICE_TAG_POOL = {
    "tech-docs:practice:error-handling": ["tech-docs:concept:observability", "tech-docs:concept:idempotency"],
    "tech-docs:practice:documentation-as-code": ["tech-docs:concept:api-design", "tech-docs:concept:backward-compatibility"],
    "tech-docs:practice:test-driven": ["tech-docs:practice:error-handling"],
    "tech-docs:practice:monitoring": ["tech-docs:concept:observability"],
}
TAG_POOL = {**CONCEPT_TAG_POOL, **TECH_TAG_POOL, **PRACTICE_TAG_POOL}


def main() -> None:
    count = int(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_COUNT
    seed = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_SEED
    rng = random.Random(seed)

    lines = []
    for i in range(1, count + 1):
        topic = TOPICS[i % len(TOPICS)]
        title, description = TOPIC_DOC[topic]
        level = LEVELS[i % len(LEVELS)]
        fmt = FORMATS[i % len(FORMATS)]
        # audience_years: beginner doc 偏低，advanced 偏高。
        # audience_years: lower for beginner, higher for advanced.
        lo, hi = (0, 3) if level == "beginner" else (4, 10)
        yrs = rng.randint(lo, hi)
        pool = TAG_POOL.get(topic, [])
        tags = list(pool[: 1 + (i % 3)])  # 0..2 tags from the pool.
        doc_id = f"doc_{i:04d}"
        record = {
            "entity_id": f"tech-docs:document:{doc_id}",
            "title": f"{title}（{fmt}）",
            "description": description,
            "topic": topic,
            "level": level,
            "format": fmt,
            "audience_years": yrs,
            "tags": tags,
            "source_revision": 1,
        }
        lines.append(json.dumps(record, ensure_ascii=False))

    out = "examples/tech-docs/docs.jsonl"
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print(f"wrote {len(lines)} documents to {out}")


if __name__ == "__main__":
    main()