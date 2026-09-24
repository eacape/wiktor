---
page_id: tech-docs:concept:backward-compatibility
entity_id: tech-docs:concept:backward-compatibility
title: 向后兼容
entity_type: concept
aliases: [回兼容, 兼容性]
tags: [概念, 版本]
---

向后兼容指新版本仍能正确服务基于旧版本契约编写的调用方，是渐进式演进与平滑升级的前提。

## 新增优于修改

优先增加新字段、新端点、新可选参数，而非改变或删除既有契约；旧调用方应无感升级。

## 引入破坏性变更的条件

当兼容不可持续时，通过主版本号预告破坏性变更，并提供迁移窗口、工具与文档降低升级成本。

## 兼容性检测

发布流水线应自动比对新旧契约，检测字段删除、类型变化等破坏性变更，在合入前失效。