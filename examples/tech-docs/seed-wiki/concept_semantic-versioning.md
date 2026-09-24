---
page_id: tech-docs:concept:semantic-versioning
entity_id: tech-docs:concept:semantic-versioning
title: 语义化版本
entity_type: concept
aliases: [SemVer, 版本号规范]
tags: [概念, 版本]
---

语义化版本使用 主版本.次版本.修订号 三段式版本号传达兼容性信息：主版本突破性变更，次版本新增向后兼容特性，修订号向后兼容修复。

## 主版本

主版本号递增表示不向后兼容的破坏性变更，是升级代价最高的分界线。

## 次版本与修订号

次版本递增表示新增功能且保持向后兼容，修订号递增表示修复缺陷且完全不改变既有行为。

## 范围表达

通过比较运算符限定可接受版本范围，配合锁文件确保可复现构建与升级可预期。