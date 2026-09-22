# 运行期内存优化第二阶段：局部更新与交付边界

日期：2026-09-22。代码基线：`802c20b`，已从远端快进拉取。
本轮先以自然 Red 建立验收门槛，随后完成第二阶段生产优化；未部署到 `pi4.lan`。
第一阶段诊断见 [内存保留方案](runtime-memory-retention.md)，历史 Red 记录见
[第一阶段验收](runtime-memory-retention-tests.md)。

## 1. 第一阶段已改变什么

`802c20b` 已实现成功发布水位释放 journal、legacy 不再归档 conversation 原始事件、
慢会话观察者使用可替换状态槽、generation 结束清理投影，以及浏览器保留迟到失败结果。
还避免了构造 live projection 时序列化随后丢弃的 activeTurn，并缓存 journal 的逻辑字节数。

这些改动缩短了旧版本的保留时间。第二阶段 Red 基线上还存在以下成本，现均由本文方案修复：

- `update_control_state → commit_session → SessionUpsert` 会发布完整 active prompt/overlay
  和 live resources。这里不包含 baseline 全历史，不能称作每次重发整个会话历史。
- `fold_active_turn_update` 先 `retained.to_vec()`；一次小更新会复制无关工具的大正文。
  `fold_overlay_update` 随后对完整 overlay 重新序列化计量，也产生临时分配。
- 浏览器的 `timeline.raw`（assistant 下为 `chunks[].raw`）积累原始更新，
  供调试面板读取；健康客户端及时消费 SSE 时也会产生第二份过程版本档案。
- 状态槽的弱引用存活不等于“尚未被消费者领取”。消费者已经捕获旧 payload、尚未 Drop
  时，生产者仍可能把最后一个 reset 写入旧槽；payload 替换与字节记账也存在独立交接点。
  本轮受控测试已分别复现最后 revision 丢失和队列字节下溢，不能由串行慢端测试通过推断安全。

Agent 继续是持久化权威；Bridge 按序应用所有有效更新并维护最新完整内存状态；
前端展示状态并提交意图。完整历史、当前工具结果、错误结果和有效资源均需保留。

## 2. Zed 的参考范围

固定参考官方提交 [`656e3ae`](https://github.com/zed-industries/zed/commit/656e3aed84e0febc2107d31f5752df157d83bb63)，
不是不断变化的 main，也不是 Zed 与 attyd 的 RSS 对照实验。

| 源码事实 | 本项目采用的原则 |
| --- | --- |
| [工具更新复用内容槽并截断旧尾部](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/acp_thread/src/acp_thread.rs#L1156-L1200) | 按实体更新当前数据，释放被替换版本 |
| [usage 只更新计数与费用并发轻量通知](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/acp_thread/src/acp_thread.rs#L2860-L2870) | 控制变更不携带无关正文 |
| [文本复用 Markdown entity](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/acp_thread/src/acp_thread.rs#L1659-L1686) | 减少无关实体复制；该实现仍有字符串复制，不能称为零拷贝 |
| [弱注册表与实例身份校验](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/agent_servers/src/acp.rs#L1158-L1205) | 索引不能额外钉住旧正文；保留 attyd 的 owner fencing |
| [仅淘汰可重新加载的空闲缓存](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/agent_ui/src/agent_panel.rs#L4228-L4284) | 继续以工作/观察者生命周期判断释放，不照搬会话数量限制 |

Zed 的 [调试缓存](https://github.com/zed-industries/zed/blob/656e3aed84e0febc2107d31f5752df157d83bb63/crates/agent_servers/src/acp.rs#L169-L221)
按条数保留消息，订阅队列无界；这不是整个进程的字节上限保证。
其单进程 UI 通知也不能替代 attyd 的多浏览器 owner、revision、reset 与可靠结果协议。

## 3. 已按优先级实施

### P0：完成状态槽的原子交接

定义一次明确的领取切点：消费者得到用于交付的 payload 后，生产者不能再将新状态
写入该消费者不会读取的槽。新更新必须被本次交付包含，或留下一项后续 delta/reset，
即使此后不再产生任何 Agent 更新也能追上最新 revision。

可在同一受锁状态内表示“可替换 / 已领取”，让消费方原子取走 payload；生产者发现
已领取时新建待发送槽。替换、领取和销毁的字节转移须形成一次完整事务，不能依赖
几个彼此独立的原子数值来推断 payload 所有权。具体容器和函数命名不是验收要求。
队列排空及所有在途交付结束后，记账必须恰好归零。

固定的失败、退休等不可重建事件仍完整交付。一个可替换状态槽不等于整个订阅队列有
固定内存上限，不能用截断可靠结果来满足内存指标。

### P1：发布独立的轻量控制变更

为已有会话的控制变化增加显式 typed patch，携带 epoch、sessionId、incarnation、
连续 seq、scope revision、变化的控制字段及同一提交切点的 view header。
`SessionUpsert` 继续表示完整替换；不能靠省略 `activeTurn` 将其偷偷改成部分更新。

- Bridge 的同值控制更新不提交新 delta；未交付/失败水位语义保持第一阶段契约。
- Hub 仅更新声明变化的字段，保留 prompt、overlay、operation、权限、终端等其他状态。
- 保留已有字段语义：usage cost 省略/null 不清除已知累计费用；允许显式清空的字段
  （如 title）继续按相应语义处理。缺字段与清空不得混同。
- snapshot 保持完整；snapshot 加连续 suffix 应与权威 registry 的最新状态等价。
  seq 缺口仍请求 snapshot，旧 epoch/incarnation 不得污染新实例。
- 新 runtime change kind 同步更新 Rust 序列化、Hub 解析和 `shared/bridge.ts` 验证。
  浏览器业务 SSE 的控制恢复契约不在本阶段擅自变更。

### P2：减少与本次更新无关的正文分配

以消息/工具实体为粒度保留当前内容，候选更新只复制被改变实体及必要索引。
可采用结构共享或等价实现；测试不要求某一种 Arc 嵌套结构或指针地址。
已有读者持有的旧 view 必须保持不可变，新的 view 则完整反映已提交更新。

验证、fold 与提交边界保持原子性：错误 owner、operation 或非法更新不能部分写入正文、
推进 revision 或破坏字节统计。当前会话的工作 owner、pending interaction 与终端不随
正文替换提前释放。

字节统计可按变化实体维护，或先使用不分配完整 JSON 的计数 writer；后者仍可能扫描
完整正文。分配门槛不能证明 CPU 为常数，也不代表长单条消息追加的复制成本已解决。
本阶段测试改变的是小工具/短消息，另外一个大工具的正文应不影响分配量。

### P3：浏览器诊断只保留当前来源

将同一逻辑实体的诊断 `raw` 从原始事件档案改为**最近一条有效来源事件**。
assistant 以语义 chunk（messageId/role）为粒度，工具/plan/compaction 以已有实体身份
为粒度；prompt echo 也保留最近来源。实体的实际业务状态仍由所有有效更新完整折叠。

`ToolCall.rawInput/rawOutput` 是当前业务数据，不是可丢弃的诊断档案；权限选项、
pending responder、错误详情、retry prompt、计划移除和 compaction 失败也必须保持。
从 REST 完整视图重建时保留其当前来源即可，不要求虚构全部历史 ACP 通知。
调试面板应明确标注“最近事件”，不再将来源数量解释为整个执行过程的事件总数。

本阶段限制诊断版本数，不限制合法当前正文大小。若未来需要完整协议追踪，必须设计
独立、显式启用的诊断通道及其字节预算，不能重新塞回会话展示状态。

## 4. 可执行验收门槛

| 编号 | 测试入口 | 要求 |
| --- | --- | --- |
| E00 | [delivery_handoff_tests](../src/server/delivery_handoff_tests.rs) | 消费捕获与新发布交错时最后 revision 不丢；replace/Drop 记账归零；失败和 retry prompt 仍可靠交付 |
| E01 | [Bridge memory_efficiency_tests](../src/bridge/memory_efficiency_tests.rs) | 4 KiB / 128 KiB 的已有正文下，同一 usage/mode/config 的真实发布字节不增长，且正文完整 |
| E02 | 同上 | 同值控制不发布；旧 epoch/incarnation、同 ID 重开后的旧 owner 更新无副作用 |
| E03 | [control_efficiency_tests](../src/server/control_efficiency_tests.rs) | 真实 emitted delta 经 Hub 后保留其他字段；snapshot/suffix/gap 恢复与 registry 全状态一致 |
| E04 | [SessionState memory_efficiency_tests](../src/session_state/memory_efficiency_tests.rs) | 无关正文从 8 KiB 增至 256 KiB / 1 MiB，8 次小更新的额外分配相对小样本不超过 64 KiB；旧 view 不变、新 view 完整 |
| E05 | 同上及 [计量设施](../src/test_allocations.rs) | 拒绝更新不改变状态/revision/统计，后续有效更新仍可提交；计量作用域和 unwind 正确复位 |
| E06 | [浏览器 memory-efficiency](../tests/runtime-memory-efficiency.test.ts) | 8/32/128 次累计工具替换、具名/匿名消息、prompt echo、两类计划与 compaction 更新后，诊断只保留最新来源，正文和结果完整 |
| E07 | 同上 | 稀疏工具更新、权限、错误重试和 REST 重建保留完整业务数据 |

E00 的屏障只暂停真实生产操作，不替它修改状态；按订阅账本身份隔离并行测试。
后续若将更新改为一个不可分割的受锁步骤，应将测试钩子移到该步骤外，不能为了测试
人为保留生产中已消除的竞态窗口。最后交付和归零断言继续保留。

E04 只在同步生产调用周围启用线程局部 `System` allocator 计量；输入构造、获取快照、
测试断言和测试侧序列化均在区间外。统计成功 alloc/alloc_zeroed/realloc 请求的字节，
不是实际驻留内存、峰值、复制次数或 RSS；不会覆盖转移到其他线程的工作。
64 KiB 是小样本对照的固定宽容区间，不是会话正文配额。

旧 journal 测试不再把“usage 必须钉住旧 overlay”当作永久前提。需要完整 upsert 的
夹具使用真实 cancel intent 固定旧正文，仍验证相同 Weak 释放和成功水位；不模拟
Agent 提前完成。旧前端测试从“必须保存 N 条 raw”改为检查最新来源，保留业务内容
与身份断言，诊断版本约束由 E06 单独覆盖。

```sh
npm run test:memory:rust
npm run test:memory:web
npm run test:memory:efficiency:rust
npm run test:memory:efficiency:web
npm run test:rust
npm run check
```

新测试使用正常正确行为断言建立自然 Red，不使用 ignore/should_panic/it.fails。

## 5. 实现与验证结果

实现保持既有权威边界和业务数据：

- `StateSlot` 在一个锁内完成 payload 的替换、领取和字节所有权转移。消费者领取后，
  生产者为后续状态建立新槽；未领取槽在销毁时只结算一次账本。
- 新增 `session_control_updated` runtime change。Registry 仅发布变化的 control value 和
  同一切点的 owner/revision/view header；Hub 在连续性与 owner 校验后局部应用。
- active turn 更新使用 `Arc<Vec<Arc<Value>>>`。折叠只 copy-on-write 被修改的实体；
  overlay 与 journal 的 JSON 字节统计改用 counting writer，不再创建等大的临时缓冲。
- 浏览器仍完整折叠所有业务字段，但每个工具、消息 chunk、plan、compaction 和 prompt
  echo 的诊断 `raw` 仅保留最近来源。调试 UI 标注为“最近的来源事件”，不显示累计条数。

Red 基线曾观测到：128 KiB turn 令同一 control 帧比 4 KiB turn 多 126,976 B；
交接可丢最后 revision 并令字节账本发生 unsigned 下溢；1 MiB 无关工具正文令 8 次小更新
分配约 33.6 MB；浏览器诊断数组随过程更新增长。这些断言在生产实现后自然转 Green，
没有放宽正文完整性、owner、revision、可靠失败或 64 KiB 分配宽容区间。

最终结果：

| 范围 | 结果 |
| --- | ---: |
| 第二阶段 Rust 定向 E00–E05 | 15 passed / 0 failed |
| 第二阶段浏览器 E06–E07 | 12 passed / 0 failed |
| Rust 串行全量 | 563 passed / 0 failed |
| Vitest 全量 | 340 passed / 0 failed |
| TypeScript 与生产客户端构建 | passed |

验证命令为 `cargo test memory_efficiency -- --test-threads=1 --nocapture`、
`cargo test -- --test-threads=1` 和 `npm run check`。既有 30 秒超时未放宽；先前基线中偶发
超时的 9 MB stdio EOF 用例也在本次全量串行回归中通过。

## 6. 明确暂不纳入的项目

- 虚拟列表属于 DOM/布局优化，不解决 Rust RSS 或 JS raw 归档；需要浏览器 profile
  决定，不在本轮假定更换前端渲染架构。
- 不引入“五个会话”总上限，不普遍改弱引用，也不改变现有观察 lease、空闲计时和
  history workflow 的工作 owner。Zed 的 close 失败仅记录日志不能替换 attyd 的拒绝关闭契约。
- 未发现可直接按 Zed 类比删除的服务端 ACP debug archive。`stderr` reducer 的累加
  不能证明当前业务 SSE 可达，故不以它构造本轮缺陷测试。
- `BridgeBootstrap.auth_events` 有完整认证回放需求及既有大输出测试。它不是普通
  调试日志；后续须先定义认证 attempt 进行中恢复、失败查看、终态释放，再设计门槛。
- 完整可靠结果可能积累；未来若要压缩，需先使其能由快照完整重建。不能直接丢弃失败。

代码门槛已通过，仍需在接近原 43 MiB 完整视图的长 turn 上比较分项指标和 RSS 趋势。
本轮不部署、不重启 pi4.lan，也不承诺未经实测的绝对内存上限。
