---
page_id: tech-docs:technology:qdrant
entity_id: tech-docs:technology:qdrant
title: Qdrant
entity_type: technology
aliases: [向量数据库, 向量检索]
tags: [技术, 检索]
---

Qdrant 是用 Rust 编写的开源向量数据库，对外提供向量相似度检索与过滤能力，常作为语义检索的后端服务。

## 向量索引

为海量向量构建近似最近邻索引，支持余弦、欧氏与点积等距离度量，在高召回与低延迟间取得平衡。

## 过滤下推

向量检索时可同时携带结构化过滤条件，先按 payload 过滤候选再做向量比对，兼顾语义与精确约束。

## 部署形态

作为独立服务多机分布或本地单机运行，通过 gRPC/REST 访问；可重建性让向量索引可随时从源头重算。