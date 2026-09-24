---
page_id: tech-docs:technology:sqlite
entity_id: tech-docs:technology:sqlite
title: SQLite
entity_type: technology
aliases: [SQLite 数据库, 嵌入式数据库]
tags: [技术, 存储, 数据库]
---

SQLite 是嵌入式、零配置、单文件的关系型数据库，以库的形式内嵌进程，广泛用于客户端与边缘场景。

## 单文件与零服务

数据落在单个文件，无需独立服务进程，天然适合单机、嵌入式与可移植交付。

## WAL 与并发

WAL 模式提升读写并发，让读不阻塞写；busy_timeout 缓解锁竞争。支持 FTS5 全文索引扩展。

## 适用边界

SQLite 本地高性能、运维简单，但单写主库与写并发有限；超大规模横向扩展需外部存储或分片。