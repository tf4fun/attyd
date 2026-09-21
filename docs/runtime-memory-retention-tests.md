# 运行期内存优化：TDD 验收记录

方案与实机证据见 [运行时内存过度保留](runtime-memory-retention.md)。
基线：`0857250`（已拉取的历史 workflow 生命周期修复）。
本轮只改变测试、`cfg(test)` 支持、测试命令与文档，保留生产缺陷以建立自然 Red 门槛。

## 执行方式

使用项目要求的 Node/Rust 环境，确保 `node` 位于 PATH。完整 Rust 套件包含本机监听端口、
PTY 和 Node Agent 子进程；应在允许这些测试资源的环境执行，不能把环境错误计为 TDD Red。

```sh
npm run test:memory:rust
npm run test:memory:web
npm run test:rust
npm run check
```

两个定向命令独立执行，Rust Red 不应掩盖浏览器用例结果。`npm run check` 在 Vitest Red
时不会继续构建客户端；需要独立使用 `npm run build:client` 验证构建。
Red 用例使用正常的正确行为断言，不使用 `ignore`、`should_panic`、`it.fails` 或故意失败占位。
生产修复完成后，这些测试必须自然变绿，不能靠更新期望值接受额外保留或丢失内容。

## 测试层与断言

| 测试入口 | 验证对象 | 关键证据 |
| --- | --- | --- |
| [Bridge publication](../src/bridge/memory_retention_tests.rs) | 真实 `flush_runtime` 与协调器 snapshot 路径 | 旧 overlay 的 Weak 释放、成功发布的连续水位、未交付后缀 |
| [Legacy projection](../src/runtime_cache/memory_retention_tests.rs) | 真实 `update_and_normalize`，生产 fold 对照 | 原始正文保留字节、完整工具内容、当前交互与终端资源 |
| [Hub/SSE](../src/server/memory_retention_tests.rs) | 真实 `BridgeHub::publish`、订阅与事件流 | 慢队列替换、共享 payload 引用、reset/owner/revision 边界 |
| [Browser](../tests/runtime-memory-retention.test.tsx) | 真实 `useAcp` hook，受控 REST/SSE 次序 | 刷新合并、完整状态重建、迟到失败、重试内容、旧 owner 隔离 |

单测统计可达引用与逻辑 payload/队列字节，不对进程 RSS 作精确断言。
完整内容、实体身份与终态必须同时正确，避免通过截断正文、停止 turn 或删除资源令内存断言通过。

## M01–M12 覆盖映射

| 门槛 | 可执行覆盖 | 验收要求 |
| --- | --- | --- |
| M01 | Bridge `delta_transfer_releases_old_overlay_before_queue_consumption` | 入队后 journal 原引用释放；队列仍持有完整交付，当前 overlay 存活 |
| M02 | Legacy `m02_growing_tool_replacements…`、`m02_message_chunks…`，以及更新后的大 burst 测试 | 工具替换只保留当前内容；消息追加逐字完整；没有第二份原始 conversation 归档 |
| M03 | Bridge `fixed_final_output…`、Legacy `m03_fixed_final_tool…` | 最终正文与实体数量相同，增加中间更新不能增加稳态保留 |
| M04 | Hub `m04_hub_without_observers…`、`m04_hub_one_healthy_observer…`、`m04_hub_multiple_observers…` | 无观察者也不保留旧版本；及时消费后的旧正文不随观察者数积累 |
| M05 | Bridge delta transfer、fallback snapshot、explicit snapshot、`later_changes_remain…` | 所有发布路径释放正确前缀，seq 单调，后续未发布 suffix 保持连续 |
| M06 | Bridge failed fallback、closed queue、mid-batch failure、serialization failure | 失败不能虚报成功水位；中途失败只认可连续成功前缀 |
| M07 | Hub `m07_slow_observer…`、更新后的 slow session 测试；Browser reset burst | 慢端过时 payload 被最新 reset 替换，lease 和健康端保持；GET 请求合并 |
| M08 | Hub snapshot/suffix/gap 与 stale SSE cursor；Browser folded tool/suffix 与旧 GET 响应 | 恢复等价、owner fencing、序号缺口触发正确恢复；迟到响应不覆盖新状态 |
| M09 | Hub `m09_reset_preserves_unreconstructible_failure…`；Browser 失败前后两种次序、跨 turn 迟到失败、完成/cancelled 快照 | 失败与 retry prompt 不丢，旧失败不结束新 turn；终态不重复、不自动重发 prompt |
| M10 | Legacy 当前交互、重复 resolution、UTF-8 终端追加/退出/释放、旧 operation 响应 | 迁移正文缓存时仍保留正确当前资源；cache 层只验证资源表示和索引，不冒充 responder 所有权测试 |
| M11 | Hub retirement + owner end；Browser 旧 epoch/旧 incarnation 失败事件；原有 owner fencing 回归 | 退休终态不被 reset 吞掉，旧 owner 事件不能改变新 owner |
| M12 | Bridge epoch teardown 与真实连接退出、Legacy 完成/失败/cancelled response/close-delete-retire、Hub 退订/结束 generation | 各层旧 payload 和资源引用在各自生命周期终点释放，不影响其他观察者 |

表中省略的名称可在相应测试入口中按 `memory_retention` 搜索。以下已有保护继续作为完整准入：

- Bridge 原有观察 Running/Reconciling 不触发额外 load、冷恢复原子安装、候选失败回滚、
  close/delete 与重开 incarnation、pending permission/elicitation 回调等测试。
- 上一轮 workflow 的 31 项新增测试与既有空闲回收测试；本轮不得为了释放内存提前结束工作 owner。
- REST/SSE 与真实 stdio 的 burst/大正文完整性测试。可用 reset 恢复正文，但不能仅验证通知类型。

M06 的批中断点由真实 `flush_runtime` 的入队失败注入验证；序列化失败用例只验证
`EventSink::internal_typed` 不发布 runtime 记录，不声称已验证批内序列化失败后的水位。
当前 runtime delta 的字段已是可序列化的值；若后续发布实现引入可失败的编码器，需在其
真实边界补充第 k 条序列化失败的连续前缀测试，不能仅靠已有单次 sink 测试验收该分支。

## 本轮新确认的浏览器边界

`turnOutcomes` 不能重建 RPC 错误，因此保证 `session_turn_failed` 被送达仍不充分：
REST 最新视图可能先于 SSE 失败事件安装。当前 hook 把较旧 revision 的失败事件当作
过时状态更新，只刷新视图，结果错误与重试内容消失。

验收要求对同 owner 的可靠失败结果保留业务含义，同时维持最新视图 revision，
并且不停止已经开始的新 turn。旧 epoch/incarnation 的失败仍须拒绝。
测试通过“失败先到 / GET 先到 / 新 turn 已开始”分别约束这些行为；不允许通过取消 revision
与 owner 校验来简单修复。
夹具保持同一 SSE 流的顺序：revision 2 的 reset 触发 GET，GET 可以先返回 revision 4/5
的最新状态，随后 revision 3 的失败事件才送达；没有人为让 SSE 内部倒序。

另外，Hub 的 `finish_generation` 当前结束观察者后仍保留旧 canonical/legacy 投影，
M12 用同一 Hub 实例中的可达 payload 验证该清理缺口。它与普通 turn 结束后 RSS 回落
是不同的生命周期边界，不否定实机已观察到的 89 MiB 回落。

## 执行结果

基线 `0857250` 的原有 Rust 测试：**516 passed，0 failed**。
首次沙箱运行因本机端口权限和 Node PATH 不完整出现环境失败；修正环境后使用同一
已编译基线测试产物确认全绿。

本轮新增 **42 个测试**（Rust 32、浏览器 10），更新 **5 个既有测试**。
其中 3 个既有测试纳入 `memory_retention` 定向集合，另 2 个真实 stdio 测试继续在全量
套件中验证完整正文与恢复行为。

| 范围 | 通过 | 预期 Red | 总计 |
| --- | ---: | ---: | ---: |
| Bridge 内存门槛 | 4 | 7 | 11 |
| Legacy 内存门槛 | 8 | 5 | 13 |
| Hub/SSE 内存门槛 | 6 | 5 | 11 |
| 浏览器内存门槛 | 8 | 2 | 10 |
| Rust 全量（含上述 Rust 门槛） | 531 | 17 | 548 |
| Vitest 全量（含上述浏览器门槛） | 326 | 2 | 328 |

19 项 Red 均落在本次验收范围，失败原因如下：

- Bridge 7 项：普通 delta、fallback snapshot、显式 snapshot 以及固定最终正文场景中，
  旧 overlay 未释放（4 项）；关闭队列、fallback 入队失败、批中入队失败仍推进成功水位（3 项）。
- Legacy 5 项：工具累计替换、消息追加、固定最终正文、大 burst、多会话重放仍保留
  第二份原始 conversation 事件归档。
- Hub 5 项：无/单个/多个观察者的旧正文保留（3 项）、慢端未合并积压（1 项）、
  generation 结束后旧投影未释放（1 项）。
- 浏览器 2 项：GET 已安装较新状态后，迟到的 RPC 失败丢失；新 turn 已开始时同样丢失旧失败。

TypeScript 类型检查、非测试配置的 `cargo check` 与独立 `npm run build:client` 均通过。
`npm run check` 因上述 2 项 Vitest Red 返回非零，因此客户端构建单独执行。
没有新增的非预期回归；这些结果是修复前的验收基线，不代表生产内存优化已经完成。

## 完成标准与后续实机验证

内存修复的完成标准是本次所有 Red 转 Green，并保留原有及新增保护测试全绿。
`cfg(test)` 故障注入和 Weak 探针只用于检查交付/释放边界，不改变生产行为。
修复不要求以某种容器或函数命名实现，但必须保留相同业务结果和交付所有权语义。

单测全绿后，仍需用接近本次 43 MiB 视图的长 turn 负载比较运行期 RSS、
baseline/overlay/journal/legacy/queue 的分项趋势，以及 turn 完成后的回落。
本轮没有部署、生产负载试验或新的堆 profile，不能把 Green 单测当作实机峰值已经降低的证明。
