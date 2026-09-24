---
page_id: tech-docs:concept:http
entity_id: tech-docs:concept:http
title: HTTP 协议
entity_type: concept
aliases: [超文本传输协议, HTTPS]
tags: [概念, 网络]
---

HTTP 是基于请求响应模型的应用层协议，Web 与 REST API 的地基；HTTPS 在其上加 TLS 加密。

## 方法

GET 读取、POST 创建、PUT/PATCH 更新、DELETE 删除；安全方法不产生副作用，幂等方法可安全重试。

## 状态码

语义化状态码传达结果：2xx 成功、3xx 重定向、4xx 客户端错误、5xx 服务端错误。

## 无状态性

HTTP 请求天然无状态，会话与上下文由客户端携带或服务端独立存储，便于水平扩展与缓存。