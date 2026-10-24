# 历史文档索引

这些文档记录已被替代的设计、固定版本审计和当时的验证结果，不再定义当前行为或完成状态。原文固定到提交 `991b0532f8f2cc321f2955a1f250e283beac1ddb`；当前要求以每行的替代文档为准。

历史中的 v1 路由、CAS、intent、incarnation、发布日志、配额和过程分页不能作为重新引入这些机制的依据。仍适用的验证方法集中在[当前测试方法](bridge-state-machine-tdd.md)。

## 审计与发布记录

| 技术主题 | 历史原文 | 当前归属 |
| --- | --- | --- |
| 2026-09-08 ACP、Zed 与实现差异 | [acp-release-audit-2026-09-08.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/acp-release-audit-2026-09-08.md) | [ACP 源码矩阵](acp-bridge-matrix.md)、[当前忠实性审计](acp-fidelity-audit.md) |
| 2026-09-26 字段与页面展示检查 | [acp-display-audit-2026-09-26.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/acp-display-audit-2026-09-26.md) | [工具呈现规则](tool-card-presentation.md)、[当前忠实性审计](acp-fidelity-audit.md) |
| v0.1.0 发布边界与验证快照 | [release-readiness-2026-09-10.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/release-readiness-2026-09-10.md) | [验证说明](testing.md)、[发布功能对照](release-parity.md) |
| 早期架构边界、取舍与修复分类 | [acp-difference-decisions.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/acp-difference-decisions.md) | [ACP 源码矩阵](acp-bridge-matrix.md)、[使用说明](usage.md) |

## 旧运行时与隔离方案

| 技术主题 | 历史原文 | 当前归属 |
| --- | --- | --- |
| v1 权威历史、CAS、intent 与发布日志 | [active-turn-runtime.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/active-turn-runtime.md) | [实现计划](state-sync.md#7-fidelity-migration-revision-2) |
| v1 运行时验收计划 | [active-turn-runtime-tests.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/active-turn-runtime-tests.md) | [测试方法](bridge-state-machine-tdd.md)、[当前验收](state-sync.md#73-gates) |
| v1 状态机合同与阶段验收记录 | [bridge-state-machine-tdd.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/bridge-state-machine-tdd.md) | [当前测试方法](bridge-state-machine-tdd.md) |
| v1 历史同步、重试与空闲回收 | [history-sync-lifecycle.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/history-sync-lifecycle.md) | [ACP 内存合同](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)、[实现计划](state-sync.md#7-fidelity-migration-revision-2) |
| 按会话收拢状态与执行队列的讨论 | [session-isolation-proposal.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/session-isolation-proposal.md) | [实现计划](state-sync.md#71-design) |
| 会话所有权与调度迁移记录 | [session-isolation-implementation.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/session-isolation-implementation.md) | [实现计划](state-sync.md#71-design)、[验证说明](testing.md#backend-behavior) |

## 旧优化与客户端协议方案

| 技术主题 | 历史原文 | 当前归属 |
| --- | --- | --- |
| 实验协议修订 2（turn 日志、哈希链、挂起响应） | [protocol.md](https://github.com/tf4fun/attyd/blob/7ceca793d0733b0c3ac62d3bcfba4b31e2c119f5/docs/protocol.md)、[client-api-redesign.md](https://github.com/tf4fun/attyd/blob/7ceca793d0733b0c3ac62d3bcfba4b31e2c119f5/docs/client-api-redesign.md) | [简易 Agent 交互协议](protocol.md)（修订 4）、[bridge 内存模型](bridge-memory-model.md) |
| 实验协议修订 3（按 scope 的文档、`epoch` + `rev`、整文档 digest） | [protocol.md](https://github.com/tf4fun/attyd/blob/33c4347/docs/protocol.md) | [简易 Agent 交互协议](protocol.md)（修订 4）、[修订 4 设计](protocol-r4-design.md) |
| 实验协议修订 1 与修订 2 的原始规范及对照 | [protocol.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/protocol.md) | [简易 Agent 交互协议](protocol.md)、[忠实性缺陷](acp-fidelity-audit.md#2-defects) |
| 客户端协议原始提案、推导与验收 | [client-api-redesign.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/client-api-redesign.md) | [设计取舍](client-api-redesign.md)、[编号验收](protocol-acceptance.md) |
| 实验协议旧实现与迁移步骤 1–9 | [state-sync.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/state-sync.md) | [当前实现与迁移](state-sync.md) |
| 旧发布日志与投影的正文保留诊断 | [runtime-memory-retention.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/runtime-memory-retention.md) | [实现计划](state-sync.md#71-design)、[内存测试入口](testing.md#test-layers) |
| 旧内存保留验收记录 | [runtime-memory-retention-tests.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/runtime-memory-retention-tests.md) | [测试方法](bridge-state-machine-tdd.md)、[内存测试入口](testing.md#test-layers) |
| 局部更新、分配与消费交接优化记录 | [runtime-memory-efficiency.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/runtime-memory-efficiency.md) | [测试方法](bridge-state-machine-tdd.md)、[实现计划](state-sync.md#73-gates) |
| v1 过程分页与浏览器缓存策略 | [lazy-turn-process.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/lazy-turn-process.md) | [ACP 内存合同](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)、[同步设计要求](protocol.md#12-synchronization-design-requirements) |
| v1 HTTP 超时与未知写入结果 | [http-request-timeouts.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/http-request-timeouts.md) | [同步设计要求](protocol.md#12-synchronization-design-requirements) |
| 早期协议设计复审 | [protocol-v2-review.md](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/protocol-v2-review.md) | [简易 Agent 交互协议](protocol.md)、[实现计划](state-sync.md) |
