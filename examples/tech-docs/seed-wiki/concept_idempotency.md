---
page_id: tech-docs:concept:idempotency
entity_id: tech-docs:concept:idempotency
title: 幂等性
entity_type: concept
aliases: [幂等, 重复执行]
tags: [概念, 可靠性]
---

幂等指同一操作无论执行一次还是多次，结果都相同。对不可靠的网络与重试机制，幂等是可靠系统的基石。

## 幂等键

客户端为操作生成唯一幂等键，服务端据此去重：相同键的重复请求返回首次结果，不产生副作用。

## 天然幂等的方法

读操作、DELETE、按绝对值的 PUT 天然幂等；增量操作（计入金额）不幂等，需要幂等键保护。

## 与重试的关系

幂等让重试变得安全：客户端可放心重发，服务端去重，即便应答丢失也不重复生效。