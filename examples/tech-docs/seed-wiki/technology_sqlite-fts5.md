---
page_id: tech-docs:technology:sqlite-fts5
entity_id: tech-docs:technology:sqlite-fts5
title: SQLite FTS5 全文检索
entity_type: technology
aliases: [FTS5, 全文索引]
tags: [技术, 存储, 检索]
---

FTS5 是 SQLite 内置的全文检索扩展，通过倒排索引对文本做关键词匹配与 BM25 排序。

## 虚拟表

FTS5 虚拟表把结构化文本按列索引，支持前缀查询、短语查询与列过滤，BM25 提供相关性排序。

## 与外触发式同步

通过触发器在业务表写入时同步索引，保证数据与索引一致，无需额外任务同步。

## 适用

FTS5 适合单机中等规模的文本检索，是 SQLite 场景的默认全文方案；超大规模可叠加外部检索引擎。