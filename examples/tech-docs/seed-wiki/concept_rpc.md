---
page_id: tech-docs:concept:rpc
entity_id: tech-docs:concept:rpc
title: RPC 远程过程调用
entity_type: concept
aliases: [远程调用, 过程调用]
tags: [概念, 网络]
---

RPC 让本地代码像调用本地函数一样调用远端过程，抽象了网络传输、序列化与错误传播的细节。

## 本地透明

调用方只关心方法签名，框架负责把参数序列化、经网络传输、在远端执行并返回结果，屏蔽分布式复杂性。

## 序列化与协议

不同 RPC 框架采用不同协议：二进制（Protobuf、Thrift）或文本（JSON-RPC），二进制更紧凑高效。

## 与消息队列的区别

RPC 是同步请求响应、强调低延迟；消息队列是异步解耦、强调削峰与重试，二者分别适配不同场景。