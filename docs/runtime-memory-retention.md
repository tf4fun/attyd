# 运行时内存过度保留：诊断记录与优化方案

状态：**第一阶段生产修复 `802c20b` 已拉取；第二阶段的局部更新、分配、浏览器诊断
保留和状态槽交接优化也已实现，并通过完整测试。** 最新方案与门槛见
[运行期内存优化第二阶段](runtime-memory-efficiency.md)。下文保留 2026-09-21 的诊断与原始方案。
诊断日期：2026-09-21；下文实机时间均为北京时间（UTC+8）。
记录时仓库 HEAD：`be6f4ca`。远端版本输出为 `0.2.8`；复现基线为
`v0.2.8` / `a5b07ff11ccdc8e338d222a578854ab2cb2158d6`。
版本字符串不能单独证明远端二进制的精确构建提交。

最初按用户要求仅保存本文档。随后用户授权拉取已完成的上一轮生命周期优化，
并将本方案转化为测试准入。当前测试基线为 `0857250`
（`fix: own optional history workflows through retry backoff`），原有 516 项 Rust 测试全部通过。
该轮更新测试、仅测试支持与验收记录，不修改生产内存行为或部署。
2026-09-22 已拉取其后续修复 `802c20b`；当前轮先建立第二阶段 Red 门槛，再完成实现，未部署。
本文补充 [运行时设计](active-turn-runtime.md) 与
[状态机 TDD 约束](bridge-state-machine-tdd.md)。最新状态交付已写成目标契约，
实际实现以 [可执行验收记录](runtime-memory-retention-tests.md) 的分阶段 Red/Green 结果为准。

## 1. 结论与证据边界

已取得以下证据：

1. 实机单个会话的完整前端 JSON 仅 **43.25 MiB**，attyd 进程 RSS 为 **2.95 GiB**，
   两者相差 **69.88 倍**。这份 JSON 包含已保存历史与当前 turn，未分页或截断。
2. 对应源码存在两条过度保留路径：RuntimeJournal 保留旧的完整 active overlay，
   legacy runtime cache 保留当前 turn 的全部原始事件。真实生产方法的独立测试已经证明，
   特定更新模式下两者分别产生平方级的正文保留量，且不依赖慢浏览器。
3. 用户随后观察到：**该 turn 正常执行结束后，attyd 内存回落至约 89 MiB**。
   这与本地完成流程会释放旧内容的结果一致，支持本次实机正常完成路径的主要回收已生效。
   该结束后数值来自用户观测，不是本轮 SSH 的第二次采样，未记录精确时间或当时视图大小。

本次问题的重点是 **turn 运行期间的巨量临时保留与复制放大**，目前没有本次正常完成后
持续泄漏的证据。这里部分“临时”数据实际被引用到整个 turn 结束，
属于生命周期过长的可达数据；fold、视图构造、序列化还会产生更短期的分配。
优化应提前释放过时版本并减少重复复制，不能只检查最终是否释放。
本次正常完成回落不替代失败、取消、断连、退休等路径的独立回归验证。
实机 RSS 还包含其他会话、其他缓存、暂存分配与分配器保留；尚无远端各缓存的堆占用分项。
因此，不能将全部 2.95 GiB 精确归于某一个缺陷，或将 69.88 倍直接视为该缺陷的实测倍率。

## 2. 实机观测

用户报告 `pi4.lan` 上一个 turn 运行约一小时，attyd 内存约 2.4 GB。
SSH 确认 7331 监听进程为 attyd PID 2482；进程自身已运行约 70 小时，
这与当前 turn 的运行时长不同。Agent 子进程内存另计，不包含在以下 attyd RSS 中。

| 时间 | attyd RSS |
| --- | ---: |
| 19:55:56 | 2.614 GiB |
| 20:00:53 | 2.724 GiB |
| 20:07:09 | 2.771 GiB |
| 20:09:12 | 2.848 GiB |
| 20:11:05 | 2.920 GiB |
| 20:12:01 | 2.916 GiB |
| 20:14:30 | 2.951 GiB |

内存有阶段性回落，不是严格单调增长。已观察 RSS 高水位为 3,333,316 KiB，约 3.179 GiB。
`/proc` 显示几乎全部 RSS 为匿名内存，不能用可执行文件映射或文件缓存解释。
部分采样时没有已建立的 7331 TCP 连接；未观察到持续的 socket 发送积压，
但这不排除内部队列积压或之前留下的分配器高水位。

用户后续补充的观测为：同一 turn 正常完成后，内存回落至约 **89 MiB**。
它补齐了“运行中数 GiB → 完成后明显回落”的实机现象，表明此次主要占用具有 turn
生命周期特征。结束后没有重新读取完整会话视图，不能假定当时 JSON 大小仍为下表数值。

### 前端数据量对照

经用户明确授权，对目标会话 `plaid-hovercraft` 执行一次普通
`GET /api/v1/sessions/:id`。请求带 `Accept-Encoding: identity`，响应 HTTP 200，
无 Content-Encoding；Content-Length 与实际读取长度均为 **45,349,424 字节**。
只输出大小、计数和运行标识，没有输出会话正文。

| 项目 | 精确值 | 换算或计数 |
| --- | ---: | ---: |
| 完整响应 | 45,349,424 B | 43.25 MiB |
| `timeline` | 7,959,978 B | 7.59 MiB；202 条 |
| `activeTurn` | 37,250,071 B | 35.52 MiB |
| `activeTurn.updates` | 37,249,771 B | 347 条 |
| 当前 turn 内 `tool_call` 合计 | 37,076,482 B | 35.36 MiB；188 个 |
| attyd RSS | 3,094,696 KiB | 2.951 GiB |
| attyd 匿名内存 | 3,088,852 KiB | 2.946 GiB |

请求时间为 20:14:30–20:14:35，前后 RSS 相同。响应 `phase=running`、
`sessionIncarnation=30383`、`viewRevision=2957`、`historyNotice=null`。
最大单条当前工具数据序列化后约 1.01 MiB，主要业务数据来自工具内容。

比较口径由源码确认：

- [`business_session_view`](../src/server.rs) 完整返回 `baseline.updates`、`activeTurn`
  和 live resources；会话视图查询没有 timeline 分页参数。
- [前端状态重建](../web/src/lib/state.ts) 遍历完整 `timeline` 与 `activeTurn.updates`。
  UI 折叠不等于服务端省略数据。
- JSON 字节数、Rust 可达堆、进程 RSS、浏览器堆是不同指标。这里比较的是未压缩 REST
  响应与进程 RSS，没有测量 Chrome 堆，也没有统计全部其他会话的 baseline。

诊断期间没有重启 attyd、取消 turn、附加调试器或修改远端文件。普通 GET 在会话已退休时
可能触发重新加载，因此先尝试了带 owner 的只读查询，之后才经用户授权执行普通 GET。
先前有限范围的实例编号未匹配，不代表会话已退休；最终取得的真实编号为 30383。

## 3. 已复现的两条保留路径

### 3.1 RuntimeJournal 保留旧 overlay

相关实现：[`runtime_state.rs`](../src/runtime_state.rs) 的 `RuntimeJournal::commit`、
`deltas_after`、`commit_session`、`update_control_state`，以及
[`SessionState::fold_overlay_update`](../src/session_state.rs)。

更新链路如下：

1. 正文分片折叠进当前 active overlay。
2. `usage_update` 等控制状态发生变化，提交 `SessionUpsert`，引用当时的完整 overlay。
3. 后续正文更新通过 fold 构造新的正文副本；journal 中的旧 `Arc` 继续持有旧版本。
4. `deltas_after` 只克隆并返回已提交记录，消费后没有删除 journal 的原始引用。
5. 正常 turn 完成才通过 `retire_turn_payload` 清理相关日志前缀。

由此，当前状态只有一份最新正文，journal 却使许多不同版本的正文同时存活。
`Arc` 仅共享同一个版本，不能消除这些不同版本之间重复的文本。

**Journal 的生产用途已经确认：**
[`flush_runtime`](../src/bridge.rs) 中的 `published_runtime_seq` 是其唯一生产游标消费者，
用途为 Bridge → Hub 的内部交付，并非浏览器 `Last-Event-ID` 的回放库。
Hub 序号缺口通过 `RuntimeSnapshotRequest` 获取当前 snapshot；浏览器的
`Last-Event-ID` 用于恢复 owner 身份，当前生产路径没有拿其 revision 查询这个 journal。
浏览器收到 reset 或 revision 缺口后，通过 REST 获取最新视图。

### 3.2 Legacy runtime cache 保存全部原始更新

相关实现：[`ActiveRuntimeProjection`](../src/runtime_cache.rs) 的
`record_session_event`、`RuntimeSession::push`、`clear_turn_events`，以及
[`BridgeHub::publish`](../src/server.rs)。

active prompt 期间，每条原始事件都会追加进 `RuntimeSession.events`。
如果同一个工具反复发送增长后的完整 `content`，canonical fold 只需保留该工具最新内容，
legacy cache 却保留每次替换前后的原始内容，累计保留量呈平方级增长。
记录发生在生产发布路径中，与是否存在 SSE 客户端无关。

旧投影仍承担终端输出规范化、live resource 恢复等职责，不能直接删除整个模块。
应解除 conversation 原始事件的保留需求，并逐项迁移仍必需的当前资源状态。

## 4. 本地复现结果

在 `v0.2.8` 的独立临时副本中新增两个诊断测试，仅调用生产方法，不修改生产实现。
命令 `cargo test memory_diagnostic -- --nocapture` 的结果为 **2 passed，0 failed**。
这些测试刻画并确认当前缺陷；测试通过不表示已修复，也不等同于下文待新增的 TDD 准入测试。

两个独立场景都覆盖 N=32、64、128，每步新增 4 KiB：

| 更新次数 N | 最新正文 | 不同版本的正文合计 | 合计 / 最新 |
| ---: | ---: | ---: | ---: |
| 32 | 128 KiB | 2.0625 MiB | 16.5 倍 |
| 64 | 256 KiB | 8.125 MiB | 32.5 倍 |
| 128 | 512 KiB | 32.25 MiB | 64.5 倍 |

计算关系为 `当前正文 = chunk × N`，`累计版本正文 = chunk × N × (N + 1) / 2`。
表中累计值包含最新版本；两种场景独立成立，不能直接相加当作远端实测占用。

**Journal 场景：** 每步发送正文分片及变化的 usage，并读取、释放刚发布的 delta，
模拟及时消费。N=128 时，journal 中不同非空 overlay 版本有 128 个，正文合计
33,816,576 B。按不同 Arc 地址去重，避免把共享引用算成重复堆分配。
不发送 usage 的对照组正文完全相同，但 journal 不持有非空历史 overlay；原始正文分片
仍线性保留 524,288 B。正常完成并提交后，旧 overlay 和原始更新正文清零，
journal 剩一条小记录，baseline 保存最终正文。

**Legacy 场景：** 一个工具先创建，再发送 N 次累计完整内容替换，
每次经 `update_and_normalize`，同时用 canonical fold 对照。
N=128 时，fold 后只有一个工具及最新 524,288 B 正文，legacy 保存 130 条事件，
序列化事件合计 33,847,362 B，其中工具正文 33,816,576 B。
`prompt_complete` 后事件数量和正文计数均归零，仅留下少量容器槽位容量。

临时复现材料（可能随本机临时目录清理，不作为长期验收依赖）：

- `/tmp/attyd-memory-v028.oDCasS/memory-diagnostic.log`
- `/tmp/attyd-runtime-journal-v028.patch`
- `/tmp/attyd-legacy-memory-repro.patch`

正式测试已按以上步骤重建，并将“确认缺陷”的断言改为“要求正确行为”的准入断言；
具体映射和执行结果见 [验收记录](runtime-memory-retention-tests.md)。

## 5. 优化目标：保存最新完整状态

客户端需要的是**所有有效更新折叠后的最新完整会话状态**。
例如消息依次追加 `A`、`B`、`C`，最新正文为 `ABC`；工具内容依次被替换为
`A`、`AB`、`ABC`，最终仅需保留 `ABC`。已经完成的历史消息仍属于当前会话数据，
待处理交互和资源句柄也必须保留到其协议生命周期结束。

| 数据层 | 保留目标 | 释放边界 |
| --- | --- | --- |
| Agent → Bridge ingress | 尚未按序应用的合法输入 | 权威状态完成应用；不能以“只看最新”为由跳过输入 |
| 会话权威状态 | 当前 baseline、一个 active/reconciling overlay、当前 live resources | 新状态接替旧状态，且旧状态不再被合法在途读取引用 |
| 历史加载事务 | 当前有效 attempt 的独立 candidate | 原子提交、失败或取消完成；保留既有 workflow owner 约束 |
| Bridge → Hub publication | 尚未转交的变更及必要的在途交付 | 所有权成功转交后，原 journal 释放；消费后队列释放 |
| Hub 当前投影 | 一份当前派生状态及必要资源表示 | 新投影接替旧投影 |
| Bridge → browser delivery | 健康连接的增量批次，或可合并的最新 reset 通知 | 消费，或由能恢复完整最新状态的通知替代过时投影积压 |

对已处理并已交付的更新，长期保留量应随**当前有效业务数据量**增长，
不应随同一数据经历的更新次数增长。暂时的快照读取、序列化和交付分配需要单独记账，
不能把它们变成贯穿整个 turn 的旧版本档案。

恢复仍遵循当前模型：Agent 是持久化权威；已物化的 Running/Reconciling 会话
从 Bridge 内存恢复，不通过额外 `session/load` 获取“最新”。

## 6. 后续实施方案

以下 A/B/C 中的 journal 释放、legacy conversation 清理、慢端状态合并及迟到失败展示
已在 `802c20b` 实现。轻量控制发布、进一步减少复制与新增的消费交接竞态门槛
见 [第二阶段方案](runtime-memory-efficiency.md)，不要将原始待办措辞视为当前实现状态。

### A. 先修复已确认的 journal 生命周期

为 journal 增加按发布水位释放前缀的能力；成功转交内部 EventQueue 后释放
`seq <= published_runtime_seq` 的记录。普通 delta、缺口 fallback snapshot、
显式 `RuntimeSnapshotRequest` 三条发布路径都要覆盖；snapshot 释放边界为其 `through_seq`。

实施条件：

- `EventSink::internal_typed` 当前忽略发送结果且不返回成功状态。先让发布结果可判断，
  序列化或入队失败不能推进“已交付”水位。连接终止时由整体 teardown 释放资源。
- 水位只覆盖连续成功的前缀。同批 `seq=k` 发布失败后停止该批并保留未交付后缀，
  不能因后续 `k+1` 成功而释放到 `k+1`。若改用恢复 snapshot，须成功发布覆盖该缺口的
  snapshot 后，才能按其 `through_seq` 推进水位。
- 保持 seq 单调；释放记录不能重置 seq 或误删尚未发布的后缀。
- 在现有状态锁/有序执行切点内完成捕获、转交、推进水位和释放，避免跨会话插入竞态。
- 内部队列已经拥有交付数据，无需等待每个浏览器 ACK 才释放 journal 原引用。
- 保留 Hub 的缺口 → snapshot 恢复，以及 epoch/incarnation fencing。

这一阶段消除 journal 对旧 Arc 的长期持有；它本身不能解决内部队列或慢浏览器积压。

### B. 消除重复的 conversation 原始事件存储

逐项核对 `ActiveRuntimeProjection` 在生产中的调用者，将重连和当前视图恢复统一到
canonical baseline + overlay + live resources。停止把已折叠的 conversation 事件
追加进另一份长期 `events` 容器。

终端字节解码/追加、最终输出保留、pending permission/elicitation、URL flow、operation
等仍需准确表达当前状态与持有必要句柄。迁移时保留其生命周期和 owner 校验，
不能把删除 `events` 等同于删除所有 legacy 职责。

同时减少控制更新的完整正文序列化：usage、mode、config 等纯控制变化采用轻量变更，
不因一次控制状态改变重新发布整份 active overlay。Hub 必须保留现有 overlay，
不能将轻量 patch 中的字段缺省解释为清空正文。真正的 snapshot/reset 仍提供完整当前状态。

### C. 将浏览器积压改为可合并的状态交付

本阶段首先限定为会话业务状态订阅；全局认证流等其他事件通道不自动套用此策略。
健康连接继续使用有序增量；客户端落后时，把可由完整视图恢复的未发送投影积压
合并成一次最新 revision 的 reset 通知。每个 owner 的待刷新通知可以覆盖更新，
避免“为每条事件各排一个 reset”或“每个 token 构造一个完整 JSON”。

实现应具备以下行为：

- 服务端记录最新待刷新 revision；已经失去连续性的旧增量不再继续作为合法 suffix 发送。
- 浏览器复用 owner-fenced GET，合并并发刷新请求；在途响应之后若仍落后，再恢复后续更新。
  通过现有观察切点与 revision 校验覆盖 GET/SSE 竞态，保证最终追上最新状态。
- 慢客户端保持观察 lease，不取消 Agent turn，也不影响正常客户端；队列里的旧 payload
  被替换时立即释放相应记账和引用。
- `session_retired`、epoch 更换、请求结果和交互 responder 等须按各自语义处理。
  不能仅因它们与状态更新使用同一队列就一并丢弃。
- 当前 `turnOutcomes` 仅保存含 `stopReason` 的响应，RPC 错误等失败结果不在其中；
  幂等记录也未保存完整错误结果。因此，**补齐失败结果的快照表达之前，
  `session_turn_failed` 等无法重建的终态事件必须可靠交付，不能用普通 reset 替换**。
  可合并的跨 turn 事件须先验证完整结果能由快照重建，同时覆盖错误展示、重试 prompt
  和浏览器排队 prompt 的准入。
- 可靠送达还须覆盖 REST 超越 SSE 的情况：较新的 GET 已安装后，同 owner 的迟到失败
  仍须展示错误和重试内容，但不能回退最新 revision 或停止已经开始的新 turn。
  本轮浏览器用例已确认当前实现会丢失这类错误。
- Agent ingress 与尚未应用的业务输入不参与此合并。内部 publication 若进一步合并，
  必须以一致的 snapshot/序号切点替代完整前缀，不能随意删除中间 delta。

本阶段的目标契约已同步至 [TDD ledger](bridge-state-machine-tdd.md)。旧契约要求
慢订阅者保存全部积压，并将所有队列的“不丢弃任何旧记录”写成统一约束；
新契约区分“合法输入完整应用”和“可重建的浏览器状态允许合并”。
保留会话正文不截断、观察者不取消工作等约束。
既有慢订阅测试已更新为 `memory_retention_slow_session_delivery_keeps_its_lease_and_preserves_other_observers`，
将“慢客户端最终收到全部 65 条记录”的断言改为最终状态等价和 lease 保持。
测试与契约最初先行更新，生产合并逻辑已由 `802c20b` 实现；并发领取/替换的边界
继续由第二阶段 E00 检查。

连接代结束还应释放 Hub 中不再有合法 owner 的 canonical/legacy 投影；本轮 M12 测试
曾确认 `finish_generation` 未完成这部分清理，现已由 `802c20b` 修复。普通 turn 完成后的内存回落不能代替
连接退出边界的独立验收。

### D. 增加分项观测，验证没有转移保留位置

增加不包含正文的只读统计：materialized session 数、baseline/overlay/candidate 大小，
journal 条数和待发布/已发布水位，legacy 原始事件字节，内部及订阅队列字节，
以及 reset 合并次数。可先用于测试和本地诊断，不要求新增公开 HTTP 接口。

每个计数注明是序列化逻辑大小、可达 payload 估计还是队列传输字节。
共享 Arc 去重，不能将逻辑字节的简单相加称为真实堆大小。
实机继续单独记录 RSS/匿名内存、完整视图大小与更新进度；若分项都已释放而 RSS 仍高，
再调查序列化峰值和分配器保留。当前没有足够证据要求更换分配器。

## 7. TDD 修复准入门槛

以下为正式测试的覆盖要求；逐项实现与状态见 [验收记录](runtime-memory-retention-tests.md)。
先验证 Bridge，再验证交付和浏览器投影。
以可达 payload、引用释放、队列计数和状态等价为主要断言；RSS 用于实机趋势验证，
不作为易受分配器影响的精确单测断言。

| 编号 | 场景 | 必须满足的结果 |
| --- | --- | --- |
| M01 | 正文分片 + 变化的 usage，发布端及时消费 | 已发布 journal 不再钉住旧 overlay；最新正文完整 |
| M02 | 一个工具的累计完整内容替换 | canonical 仅保留最新工具内容；legacy 不累积旧正文 |
| M03 | 固定最终正文及实体数量，增加中间更新次数 | 交付追平后的保留量不随次数线性或平方级增长 |
| M04 | 无浏览器、一个浏览器、多个浏览器 | 无订阅也不累积旧版本；共享与清理行为正确 |
| M05 | 普通 delta、fallback snapshot、显式 snapshot | 三条成功交付路径都释放正确前缀；seq 单调，未发布后缀不丢失 |
| M06 | 批中间序列化/入队失败、内部队列关闭、epoch 结束 | 仅释放连续成功前缀；未交付不能推进水位；teardown 无残留 owner/payload |
| M07 | 一个慢浏览器与一个正常浏览器并存 | 慢端积压合并并释放旧内容；两端最终状态相同，lease 与 turn 保持 |
| M08 | snapshot 捕获与新更新交错、过期 cursor | 观察切点一致；连续 suffix 或 reset；无缺字、重复工具或错误版本回退 |
| M09 | turn 完成/失败/cancel 与 reset 交错 | baseline/overlay 原子转换；不可重建的失败事件不丢失；错误/重试语义完整，排队 prompt 不重复提交 |
| M10 | permission、elicitation、URL flow、终端追加/退出/释放 | 当前资源完整、responders 仅完成一次、旧 owner 响应无效 |
| M11 | close/delete、同 ID 重开、连接更换 | 旧消息不能污染新 incarnation；退休通知不能被普通 reset 掩盖 |
| M12 | 全部会话与观察者清理 | journal、legacy、队列及 candidate 的对应 payload 记账归零 |

在集成层复现接近本次负载的长 turn、较大的工具输出以及短暂慢客户端，比较修复前后
完整视图大小与 RSS 曲线。无需在生产机上制造内存耗尽，也不应以截断合法工具输出
来达成内存指标。没有取得实际修复结果前，不承诺某个绝对 RSS 上限。

## 8. 与正在执行的生命周期优化衔接

已拉取 [历史同步与空闲回收方案](history-sync-lifecycle.md) 的实现 `0857250`，
并确认原有 Rust 测试基线为 516/516 通过。它与本方案分别处理“工作何时结束”和
“运行时数据应保留多久”，两者都依赖 session owner 与有序发布切点。

后续建议按 A、B、C 分别交付，分项观测与相应测试随每阶段推进，避免把已经通过的
生命周期修复与浏览器交付契约调整混成一次难以归因的变更。
尤其不能通过提前结束 history workflow、释放仍有效的 candidate、退休忙会话，
或取消 Agent 工作来降低内存。
