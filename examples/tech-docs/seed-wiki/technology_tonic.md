---
page_id: tech-docs:technology:tonic
entity_id: tech-docs:technology:tonic
title: tonic
entity_type: technology
aliases: [Rust gRPC, gRPC 服务端]
tags: [技术, RPC, Rust]
---

tonic 是 Rust 的 gRPC 实现，基于 tokio 与 HTTP/2 提供类型安全的远程调用服务端与客户端。

## 类型安全

通过 prost 从 .proto 生成强类型消息与服务骨架，编译期校验契约，减少序列化错误。

## 拦截器与中间件

tonic 支持 interceptor 做鉴权、限流与日志，与 Tower 中间件体系融合，覆盖横切关注点。

## 集成

与 tokio/axum 同栈，可同一运行时承载 HTTP 与 gRPC 双协议服务，降低运维复杂度。