---
page_id: tech-docs:concept:api-design
entity_id: tech-docs:concept:api-design
title: API 设计
entity_type: concept
aliases: [接口设计, 接口规范]
tags: [概念, 设计]
---

API 设计是在编写实现之前定义软件对外接口契约的工程实践，决定哪些能力对外暴露、以何种形态暴露。

## 契约

接口契约是调用方与提供方之间的稳定协议，包含路径、方法、参数、鉴权、错误码与版本。契约一旦发布即进入兼容性管理。

## 资源建模

面向资源的 API 以名词实体为核心，通过 HTTP 动词表达操作；面向动作的 API 以动词命令为核心，适合流程性操作。

## 准则

良好的 API 应自解释、最小化暴露面、逐版本演进并保持向后兼容，避免破坏性变更影响已有调用方。