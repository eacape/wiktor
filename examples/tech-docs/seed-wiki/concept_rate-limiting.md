---
page_id: tech-docs:concept:rate-limiting
entity_id: tech-docs:concept:rate-limiting
title: 限流
entity_type: concept
aliases: [限速, 流量控制]
tags: [概念, 可靠性]
---

限流通过限制请求速率保护后端不被打垮，是 API 网关与高可用架构的必备手段。

## 固定窗口

按时间窗口统计请求数，窗口到期重置；实现简单但窗口边界瞬时突发可能放行超量请求。

## 令牌桶

以固定速率补充令牌、请求需消耗令牌，允许短时突发并限制长期均值，是业界最常用算法。

## 滑动窗口与漏桶

滑动窗口更平滑地限制平均速率；漏桶则强制恒定速率输出，适合削峰填谷。返回 429 并携带重试信息。