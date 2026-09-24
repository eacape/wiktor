---
page_id: tech-docs:concept:database-indexing
entity_id: tech-docs:concept:database-indexing
title: 数据库索引
entity_type: concept
aliases: [索引, 聚簇索引]
tags: [概念, 存储]
---

数据库索引是以额外存储换查询速度的数据结构，通过牺牲写入开销换取加速读取与检索。

## B 树

传统索引基于 B 树，数据按序排列、树高随行数对数增长，适合范围查询与点查，写入需维护树平衡。

## 全文索引

全文索引面向文本检索，倒排表记录词项到文档的映射，支撑关键词匹配与 BM25 排序，适合搜索场景。

## 权衡

索引不是越多越好：每个索引都增加写入与存储成本。应针对热点查询建立索引，并监控未用索引。