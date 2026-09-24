---
page_id: tech-docs:concept:grpc
entity_id: tech-docs:concept:grpc
title: gRPC 远程调用
entity_type: concept
aliases: [GRPC, 远程过程调用]
tags: [概念, RPC, 网络]
---

gRPC 是基于 HTTP/2 与 Protobuf 的高性能远程过程调用框架，支持强类型接口定义与多语言互操作。

## IDL 先行

通过 .proto 定义服务与消息契约，类型化生成客户端与服务端骨架，避免手写序列化与调用胶水。

## HTTP/2 特性

多路复用、流式传输与双向流支持，比 HTTP/1.1 更高效，适合长连接与流式场景。

## 与 REST 的关系

REST 面向资源、基于文本；gRPC 面向方法、基于二进制协议。二者常并存，兼容网关做转换。