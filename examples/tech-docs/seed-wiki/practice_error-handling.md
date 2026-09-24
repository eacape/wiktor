---
page_id: tech-docs:practice:error-handling
entity_id: tech-docs:practice:error-handling
title: 错误处理
entity_type: practice
aliases: [异常处理, 失败处理]
tags: [实践, Rust]
---

错误处理规范定义如何表达、传播与呈现失败，是可靠软件对外行为的一部分。

## 类型化错误

用 Result 显式返回错误并把错误类型化，让调用方能区分可恢复与不可恢复失败，而非吞掉异常。

## 缺失时的行为

失败必须可见：记录错误、可观测地暴露并快速失败，而不是静默返回错误结果或空值。

## 面向调用方的呈现

给调用方稳定、语义化的错误码与消息，附带可诊断的上下文，同时避免泄露内部细节。