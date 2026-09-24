---
page_id: tech-docs:concept:observability
entity_id: tech-docs:concept:observability
title: 可观测性
entity_type: concept
aliases: [观测性, 监控]
tags: [概念, 运维]
---

可观测性从外部输出反推系统内部状态，由日志、指标与链路追踪三大支柱构成，让故障可被发现、定位与理解。

## 日志

面向事件的文本记录，记录发生了什么，是排障的第一现场；关注结构化与采样避免噪音。

## 指标

面向趋势的数值聚合（QPS、延迟、错误率），用于告警与容量规划，采集开销低可持续跟踪。

## 链路追踪

面向一次请求跨服务调用链路的详细记录，把分布式请求串成一条 trace，定位慢点与依赖瓶颈。