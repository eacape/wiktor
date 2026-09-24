---
page_id: tech-docs:practice:test-driven
entity_id: tech-docs:practice:test-driven
title: 测试驱动开发
entity_type: practice
aliases: [TDD, 先测试]
tags: [实践, 测试]
---

测试驱动开发以测试先行驱动实现：先写下描述期望行为的失败测试，再实现使其通过，持续重构。

## 红绿重构

先写失败测试（红），再写最小实现使其通过（绿），最后重构消除重复与坏味道，循环推进。

## 行为契约

测试作为可执行的行为契约，锁定期望输出；覆盖边界、错误路径与并发场景，避免只测快乐路径。

## 与集成的平衡

单测保证逻辑，集成测试验证跨组件契约；测试驱动聚焦于快速反馈与保护重构安全。