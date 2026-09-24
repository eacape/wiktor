---
page_id: tech-docs:technology:sqlite-wal
entity_id: tech-docs:technology:sqlite-wal
title: SQLite WAL 模式
entity_type: technology
aliases: [写前日志, WAL]
tags: [技术, 存储]
---

WAL（Write-Ahead Logging）是 SQLite 的日志模式，把数据库变更先写入独立的 -wal 文件，再随 checkpoint 合并回主库。

## 读写并发

WAL 让读操作与写操作并行，读者看到事务开始前的快照，写者在 WAL 尾部追加，大幅提升并发读。

## 与备份/复制的交互

WAL 文件是增量备份与持续复制的关键：工具可独立读取 WAL 进行增量同步，如 litestream。

## checkpoint

WAL 达到阈值或在 checkpoint 时回写主库并截断；异常退出后可重放 WAL 保证持久性。