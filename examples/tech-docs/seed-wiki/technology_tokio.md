---
page_id: tech-docs:technology:tokio
entity_id: tech-docs:technology:tokio
title: Tokio
entity_type: technology
aliases: [异步运行时, 异步编程]
tags: [技术, 并发, Rust]
---

Tokio 是 Rust 生态的异步运行时，提供事件驱动的并发模型，是构建高性能网络服务的基础设施。

## 多线程与任务调度

Tokio 在线程池上调度非阻塞任务，把异步 I/O 复用极少数系统线程，支撑海量并发连接。

## async/await

配合 trait 与 await 表达异步流程，通过 spawn 并行执行独立任务，经 channel 在任务间通信。

## 生态

tokio 上层有 axum 做 Web、tonic 做 gRPC、Tower 做中间件，构成一致的网络服务技术栈。