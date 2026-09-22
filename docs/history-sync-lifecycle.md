# 历史同步与空闲回收：生命周期修复方案

状态：已实施，生命周期验收门槛已 Green。生产修复自 `0857250` 落地。
历史 Red 基线：`a65d5d16fc0eb8674c2f9b26888eb9dbda7933c8`；当前验证提交：`5f66a6a`。
最新 [CI 35682667713](https://github.com/tf4fun/attyd/actions/runs/35682667713)
的 14 个执行 job 全部成功：Rust 588 项、Vitest 456 项、浏览器 94 项通过，
Rust 行覆盖率 93.39%（门槛 85%）。该运行不是 tag 触发，release job 按条件跳过。
本文补充 [runtime contract](active-turn-runtime.md) 与
[TDD ledger](bridge-state-machine-tdd.md)，新增准入用例位于
[`src/bridge/unobserved_tests/`](../src/bridge/unobserved_tests.rs)。

## 1. 修复前的问题与边界

成功 fork 后，Bridge 先为目标安装 Agent 返回的上下文或带来源说明的源会话快照，
再尝试可选的权威历史加载。加载的可重试失败会清除 `load_attempt`，并把目标恢复到
`Ready`。修复前的 `SessionWork::of` 只查看 turn、operation、load attempt、终端及交互；
它看不到仍在等待重试的同步任务，因此可能将目标判断为 Idle。

Red 阶段已通过原生 ACP 测试和真实 stdio + REST/SSE 时钟复现：

1. source 保持观察；目标尚未观察；配置 `--session-unobserved-timeout 1`。
2. Agent 成功返回 fork 目标；连续四次可选 load 返回可重试错误 `-32603`。
3. 进入 2 秒退避，期间正常 `session/list` 完成，触发 `flush_runtime`。
4. 约 1 秒后目标被自动 close；无 close 能力时则本地退休。
5. 原 fork 请求随后因 owner 已失效而失败，而不是返回已经创建的会话。

浏览器通常在 fork HTTP 成功后才观察目标，故这个无观察窗口符合正常使用流程。
缺陷也存在于 `a5b07ff`，不是 `a65d5d1` 引入的回归。已复现的是 fork 的 Ready 退避路径；
同条件的 resume 未复现，因为其 phase/admission 阻止了关闭。默认 300 秒大于当前最大
8 秒退避，正常调度下未触发此问题。不能把结论扩大为所有历史加载都会失败。

`a65d5d1` 在 `begin_load` 时取消旧倒计时，已解决单次 RPC 前后空闲区间的问题；
本方案补齐的是跨多次 RPC 的业务流程生命周期。

## 2. 设计决策：三个互相独立的维度

| 维度 | 当前归属 | 含义 |
| --- | --- | --- |
| 历史投影与业务准入 | `SessionState`：phase、active turn、operation、load attempt | 决定能否接受 prompt/control/attachment 等；保留当前规则 |
| 正在等待或执行的工作 | `SessionEntry.resources`：materialization、history sync | 整个流程未终止就属于工作，包含退避；不等同于独占业务操作 |
| 观察与自动回收 | `SessionObservers` + 派生的 `SessionWork` | 仅 Active、无 session observer 且无工作时，启动完整空闲区间 |

实现已在 **`SessionResources` 增加可选历史同步 owner，并从完整 `SessionEntry` 统一派生
`SessionWork`**。复用既有 cold materialization owner 保护外层重试。每个会话仍只有一个
Registry allocation，不增加全局任务表或第二份会话状态。

Agent 继续是唯一持久化权威；Bridge 保持内存投影；浏览器只提交意图、观察结果。
本修复无需改变 REST/SSE schema、`MirrorPhase`、CLI 默认值或历史持久化方式。

不采用以下方案：把退避伪装成在途 `load_attempt`；长期占用 `operation`；创建假 observer；
持有 execution ticket 等待；或者靠延长超时避开竞态。这些做法会混淆准入、观察或调度职责。

## 3. 资源与身份模型

以下为已实施的内部结构摘要，不是新增对外 API：

```rust
// SessionResources 内新增；整个 optional history 流程只有一个。
history_sync: Option<HistorySyncResources>

struct HistorySyncResources {
    flow_id: String,
    cancellation: CancellationToken,
}

// continuation 持有轻量身份与取消信号，不持有或复制历史。
struct HistorySyncOwner {
    session: SessionResourceOwner, // epoch + session_id + incarnation
    flow_id: String,
    cancellation: CancellationToken,
}
```

职责划分：

- `flow_id` 标识一次完整同步，包括重试、成功提交和最终缓存 fallback。
- `load_attempt` 仍只标识一次实际 RPC 和其 replay candidate；每次重试使用新 attempt ID。
- `operation_id` 继续标识原业务请求，不用它替代 flow 或 attempt 的身份。
- `MaterializationResources` 继续持有冷恢复的 waiters、取消 token 和跨外层重试的身份。
  可选同步可以作为其子流程；两种资源的存在通过布尔 OR 判定工作，不维护引用计数。
- token 表示“请求终止”，资源槽表示“尚未完成收尾”。仅 token 被取消不代表已经 Idle。

Registry 提供 `begin_history_sync`、`supersede_history_sync`、`finish_history_sync`、
`history_sync_status` 及工作状态查询。注册和续跑状态查询校验 epoch 与 incarnation；
完成在所属连接的 Registry 内按 session ID、incarnation 和 flow ID 精确匹配。
重复完成无副作用，旧完成不能清除新 flow；收尾事件不跨连接 generation 复用。
已有 replay allocation/attempt 校验继续保留，flow ID 不能替代它们。

正常 begin 遇到活跃 flow 会拒绝，不能无条件覆盖。通过既有 admission 的后继业务在
同一事务内调用 `supersede_history_sync` 释放旧槽，由后继工作接管空闲区间。
此路径不触发旧 token；旧 continuation 在原退避到期后检查到 `Superseded`，
不再 load 或 fallback，并按后继留下的当前投影完成。退休则通过资源取消触发 token。
旧 guard 仍只持旧 flow ID，迟到 finish 成为无副作用操作；新 flow 登记仍要求槽为空，
并立即撤销旧 absence permit。单纯 token 取消不授予任意请求覆盖权。

同 incarnation 的 `register_history` 只重建历史投影，继续保留资源 owner；不同 incarnation
替换必须先取消旧资源。既有 cold materialization 在 attachment 分配新 incarnation 时的
显式移交路径继续保留；optional history owner 不可随 session ID 自动迁移。

## 4. 唯一工作判定与回收入口

统一从 Registry 的完整 allocation 读取状态和资源：
`SessionRegistry::work(&self, session_id: &str) -> Option<SessionWork>`
内部交给 `SessionWork::of(&SessionEntry)`；调用方另行校验 incarnation 与 absence permit。
批量计时器刷新从 `iter_entries()` 读取同一 allocation，使用相同派生规则：

```text
pending permission / elicitation / URL interaction  -> AwaitingInteraction
active_turn OR operation OR load_attempt
OR running terminal OR materialization OR history_sync -> Running
otherwise                                             -> Idle
```

`SessionWork::Running` 表示工作未结束，不要求正在消耗 CPU，也不要求 `MirrorPhase::Running`。
不要复制一个可变 `is_busy` 字段到 State、Runtime snapshot 或浏览器。

所有自动回收检查使用同一查询：计时器刷新、`RetireUnobservedSession` 处理、自动 close
分支和无 close 能力的本地退休分支。只修改计时器启动处不足以防住已排队的过期事件。

注册流程 owner 时，在同一状态事务中立即 `stop_absence()`，撤销旧 permit。
工作结束时，在最后的 owner transition 内释放资源并刷新计时器：有其他工作或 observer
则不计时，否则从此刻开始完整 interval。不依赖以后碰巧有 list/通知触发刷新。
保留已实现的“自动 close 被拒绝后不自行重试，重新观察再离开才允许下一次尝试”规则。

## 5. 流程起止与调度顺序

```text
attachment 成功，建立目标资源 owner（发布可路由目标之前）
    |
    +-> begin_load(A1) -> replay/response -> commit -> 完成
    |                                  |
    |                                  +-> fail_load(A1)
    |                                      清除 attempt/candidate，保留 flow
    |                                      释放 execution ticket
    |                                      等待 backoff 或取消
    |                                      重新取得 ticket，校验 owner
    |                                      begin_load(A2) ...
    |
    +-> 无 load 能力 / 不可重试失败 -> 完成缓存 fallback
    +-> 被已接受的业务操作接管      -> 让位，保留当前投影
    +-> 退休 / epoch 结束           -> 取消，禁止恢复旧会话
    |
    `-> 发布终态，精确释放 flow 并刷新 idle timer
```

### 注册边界

- **fork**：目标 incarnation 和初始 baseline 建好后，在 `advance_catalog_revision`、
  `flush_runtime` 和 `creation_finish.committed` 之前登记 flow。不能等目标 continuation
  取得 execution ticket 或首个 `begin_load` 时才登记，否则 timeout=0 仍有暴露窗口。
- **resume**：成功完成 attachment 后，在释放原 operation、刷新 runtime 之前登记后续
  history flow。覆盖从 attachment 到可选 load/fallback 的交接。
- **cold observe**：沿用已有 `materialization`，从创建到 waiters 原子交付始终算工作。
  外层重试不按单次请求清掉 owner；resume 子流程结束也不提前释放父 materialization。

### 尝试与等待

成功加载仍原子安装 baseline 和 controls；失败仍回滚 candidate 与 replay validation。
可重试失败只结束 attempt，保留 flow。等待释放 `ExecutionTurn` 和状态锁，并监听
flow 及连接的取消信号；cold materialization 的父资源独立管理其 waiters 和外层生命周期。
唤醒后重新取得执行权、校验完整 owner，再开始下一次 RPC；取消和超时同时就绪也不能
绕过该检查。

### 统一收尾

attempt loop 已封装为内层结果，外层负责 fallback/让位及唯一 finalizer，避免各个 `?`
跳过 flow 清理。finalizer 覆盖成功、不可重试失败、无 load 能力、fallback 自身错误和取消。
错误退出时，仍需确保 candidate/validation/attempt 已回滚或对应 owner 已被退休。

内层 `HistorySyncOutcome` 区分 `Loaded`、`Failed(error)`、`Superseded` 与
`Stopped(error)`。仅 `Failed` 可进入缓存 fallback；业务接管直接让位，
退休或断连直接取消。不能把所有取消都当作普通 load 错误，否则会再次安装旧缓存。

流程 guard 由命令级收尾持有，记录真正的目标 owner（fork 时不是 source）。
`synchronize_attached_history` 返回只表示历史子步骤完成，不能立即释放 flow：caller 后面
还要取得最终 view、发布业务响应。正常路径先完成这些步骤，再精确结束 flow、刷新 timer。
这样 timeout=0 也不会在取 view 前回收目标。冷恢复的父 materialization 继续保护其后的
waiter/observer 原子交付。

带 history flow 的命令终态已统一到 `handle_command` 的 finalizer：先通过
`BusinessResponder` 发布成功或错误响应，再释放 flow、执行 `flush_runtime`。
fork 在 inner 中先发送具体业务响应；finalizer 的再次发送由 responder 的一次性 sender
保护，不会重复投递。resume 的通用响应也在 flow 释放前发送，不再依赖外层补发。
此边界位于后续终端资源释放等异步等待之前。

fallback 和最终 view 组装仍遵守执行权及 owner 校验，不能先释放 flow 再异步进行这些步骤。
`handle_command` 的 flow finalizer 位于 `execution.is_some()` 条件之外：即使重新取得
执行权失败，也会在状态锁内按精确身份收尾并刷新计时器，且不会创建 owner 或派发新 RPC。
owner 已退休时，Registry 的资源清理已取消旧资源，迟到 finalizer 不影响后继 allocation。

任务意外 drop 使用轻量 guard 投递带身份的收尾事件，不能在 `Drop` 中跨 FIFO 直接修改
canonical state。连接已关闭时由 `SessionResources::cancel`/`Drop` 兜底取消资源。
不得通过直接丢弃在途 RPC 宣称 Agent 工作已结束；不确定响应继续遵守现有连接排空、
失败及 epoch 替换契约。HTTP 调用者断开不自动取消 Bridge 已接受的业务。

## 6. 与业务准入的协作

| 到达的动作 | load RPC 在途 | optional fork 的 Ready/backoff |
| --- | --- | --- |
| observe / view / list | 保留现有快照与 single-flight 规则 | 正常读取/观察，不启动重复 load，不取消同步 |
| 自动回收 | 工作阻止回收 | flow 阻止回收 |
| prompt / control / fork | 保留现有 admission 拒绝或协调规则 | 保留现有 admission；成功接受后令旧 optional flow 让位 |
| 通过既有 admission 的显式 attachment | 保留既有 attachment 互斥规则 | 由成功接受的新 attachment 接管，旧 flow 不再重试 |
| 手动 close / delete | 保留既有 admission，不引入强制中断 RPC | 可以按既有规则接受；旧 flow 让位于生命周期操作 |
| 连接关闭 / owner 替换 | 连接和资源清理负责终止 | 取消等待，旧 continuation 不再派发 |

“成功接受后让位”与新的业务 reservation/CAS 在同一事务内发生。CAS 失败、重复意图
查询或其他被拒绝的请求不能取消同步。这里不改变 `SessionState::can_begin` 的基本许可；
只补上成功 admission 时对可选后台流程的不可逆接管。
例如当前 Active 会话允许显式 `session/load` 重载，但不接受再次 `session/resume`；
本修复不放开后者。合法接管按第 3 节的槽释放与登记规则处理资源。

不能只在重试醒来时检查 `active_turn` 或 `operation`：新 prompt/control 可能已经在退避
期间开始并完成，此时这些字段又为空。必须在接受新操作时记录不可逆的让位，防止旧 load
事后覆盖新 turn、控件结果或 Bridge 保留的 PromptResponse 边界。Red 阶段测试已独立复现：
prompt、control 和显式 load 完成后，旧 flow 仍派发 load；即使 Agent 忠实重放当前消息，
prompt 用例中已保留的 turn outcome 仍被清空。修复后对应门槛已通过；这些历史回归证据
不用于推断线上发生频率。

让位后的旧任务不能再执行 `use_cached_history` 或修改后继 flow 的 sync 状态。若原 fork
目标仍是同一 Active owner，返回当前视图，保持已成功创建的目标；若已经关闭、正在被
生命周期操作接管或 owner 已替换，则返回相应取消/冲突，不恢复它、不重复 fork。
手动 close 被拒绝后继续遵守既有可用性和自动 close 尝试标记规则，不偷偷重启旧可选同步。

## 7. 文件级实施范围

| 文件 | 已实施职责或保留契约 |
| --- | --- |
| `src/session_resources.rs` | history flow 资源与取消；扩展 `cancel`/`Drop` |
| `src/session_registry.rs` | 精确注册/完成 flow，统一工作查询，owner 退休清理 |
| `src/session_observers.rs` | 从完整 allocation 派生 work；保留 timer/permit 状态机 |
| `src/bridge.rs` | fork/resume 交接注册、attempt loop/统一收尾、取消监听、成功业务 admission 让位、所有 auto-retire guard 使用同一查询 |
| `src/session_mirror.rs` | 保持 attempt/candidate 语义；核验同 incarnation 历史重建不丢 flow，替换不继承旧 flow |
| `src/bridge/unobserved_tests/workflow*.rs` | 31 项流程级行为门槛及共享 harness；既有 `history.rs` 保留单次 load 的计时回归 |

guard 收尾使用内部调度事件 `BridgeInput::FinishHistorySync`；未新建通用工作框架，也未修改浏览器来
制造观察租约。flow 不携带历史 payload，不克隆 baseline，不加入对外快照。

## 8. TDD 合并准入

下面的测试族已加入 **31 项行为测试**，并保留既有 11 项 idle-retirement 门槛及其断言。
主测试经过真实 Bridge coordinator、内存 ACP transport 和业务入口，使用暂停的 Tokio
时钟；不靠直接改 Registry 或手动调用 `refresh` 证明修复。

| 编号 | 行为门槛 | 关键断言 |
| --- | --- | --- |
| W1 | 复现 1s idle / 2s backoff / list；close 能力有、无两种 | 不 close、不本地退休；第 5 次 load 发出；成功后原 fork 成功，目标 owner 连续 |
| W2 | retry 后成功及不可重试错误后 fallback | 从完整流程结束起计满 interval；此前不退休，此后恰好退休一次；cached context/notice 和空 replay 语义不变 |
| W3 | backoff 中显式 close/delete；连接在 RPC 等待或 backoff 中关闭 | 终态可达；超过最大 backoff 后无旧 load、无会话复活；手动操作不绕过在途 RPC 的既有 admission |
| W4 | 旧 flow 结束迟于同 ID 新 incarnation 或同 incarnation 新 flow | 旧完成不能清除新保护、覆盖新 baseline 或提前退休；后继会话可正常工作 |
| W5 | A 的 load 在途/退避时操作 B 并查询 catalog | B 和 catalog 在 A 恢复前完成，证明未以持票或锁等待实现保护 |
| W6 | Ready/backoff 中正常 prompt/control 接管；新操作在退避结束前完成 | admission 成功；旧 flow 不再 load/fallback；新历史、turn boundary 与 controls 保留；失败 CAS 不取消 flow |
| W7 | timeout=0 的目标发布交接，以及 cold materialization 外层重试 | 已接受且未完成的流程不出现假 Idle；最终结束后遵守 zero/observer 规则；多个 observer 只加入一个 materialization |
| W8 | finalizer 提前错误/取消、观察者回归、无 load 能力 | 不泄漏永久 busy，不误清其他 owner；正常 fallback 保持 resume/fork 可用；observer 加入不触发第二次 load |

W1 已先在 `a65d5d1` 上得到行为断言失败，再修实现；其余隔离/进展门槛在原代码中
已有部分通过，继续保留为兼容性保护。测试不仅断言“不 close”，还证明最终可完成和最终可回收。
暂停时钟用协议响应或事件作为屏障，逐段推进时间，避免一次大幅 advance 跳过目标交错。

### 可执行覆盖映射

| 门槛 | 测试模块与场景 | 用例数 |
| --- | --- | --- |
| W1 | [workflow_retry.rs](../src/bridge/unobserved_tests/workflow_retry.rs)：2s backoff 中禁止 Agent close / 本地退休 | 2 |
| W2 | [workflow_retry.rs](../src/bridge/unobserved_tests/workflow_retry.rs)：retry success / fallback × close / 本地退休，均验证完整新 interval 和恰好一次退休 | 4 |
| W3 | [workflow_cancellation.rs](../src/bridge/unobserved_tests/workflow_cancellation.rs)：手动 close、两种 delete、在途 admission、RPC / backoff 连接取消 | 6 |
| W4 | [workflow_cancellation.rs](../src/bridge/unobserved_tests/workflow_cancellation.rs)：同 incarnation 显式 reload、新 incarnation reload、同 ID 第二次 fork 的重试保护 | 3 |
| W5 | [workflow_admission.rs](../src/bridge/unobserved_tests/workflow_admission.rs)：load 在途 / backoff 时其他会话与 catalog 完成 | 2 |
| W6 | [workflow_admission.rs](../src/bridge/unobserved_tests/workflow_admission.rs)：短 prompt / control 接管，失败 CAS 不取消旧同步 | 3 |
| W7 | [workflow_handoff.rs](../src/bridge/unobserved_tests/workflow_handoff.rs)：zero timeout fork / resume × close / 本地退休；两个 cold observer 共享重试和同一交付 owner | 5 |
| W8 | [workflow_handoff.rs](../src/bridge/unobserved_tests/workflow_handoff.rs)：无 load 的 fork / resume、observer 回归、非法 replay、非法 fork target；[workflow_retry.rs](../src/bridge/unobserved_tests/workflow_retry.rs)：业务响应接收者断开 | 6 |

共享 [workflow.rs](../src/bridge/unobserved_tests/workflow.rs) 只通过业务/观察入口和 ACP 消息
驱动场景，不修改 Registry。harness 保留完整已读及待取事件，避免“没有 close RPC”掩盖
本地退休。源会话上下文在创建后写入，以 canonical view 发布作为屏障，保证 fallback
断言验证真实安装的 baseline。

W4 的第二次 fork 用例把新流程推进到旧退避 deadline 之前，验证可观察的后继保护，
允许正确实现更早取消旧流程。同 incarnation 用例覆盖已完成的显式重载；W6 另覆盖
prompt/control 在旧退避结束前完成，旧同步不再恢复。新 incarnation reload 与替代 fork
用例验证旧完成不清除后继保护、后继仍可完成且最终可回收。

实现中的 `finish_history_sync` 按 incarnation 和 flow ID 精确匹配，重复清理或不匹配
均不修改槽；命令 guard 的 `take` 也只交出一次 owner。这是代码层面的清理保证。
当前没有直接构造两个同 incarnation history flow 的 Registry 单元测试，也没有单独
重复调用 `finish_history_sync` 的幂等性测试；上述行为覆盖不能冒充这两项专门测试。
如后续补充，应保持现有 admission，不为构造测试放开 active session 的 resume。

只运行新增门槛：`ATTYD_SKIP_WEB_BUILD=1 cargo test bridge::unobserved_tests::workflow`。
连同原 11 项门槛运行：`npm run test:idle-retirement`。当前两者均为 Green；历史 Red
阶段保留原失败退出状态，未通过忽略用例或放宽断言获得 Green。

### 历史 Red 基线

以下为修复前 `a65d5d1` 上的结果，不代表当前状态：新增 31 项中 **25 通过、6 失败**；
合并原 11 项后为 **36 通过、6 失败**。
全量 Rust 为 **510 通过、同样 6 失败**，原有 485 项全部通过；`npm run check` 通过，
包含 318 项前端/共享测试。具体失败断言记录于
[History-workflow Red baseline](bridge-state-machine-tdd.md#history-workflow-red-baseline)。

### Green 与当前验收

`0857250` 已完成资源生命周期、统一工作派生、精确收尾与业务让位，31 项新增门槛和
原 11 项 idle-retirement 门槛均已通过。最新远端提交 `5f66a6a` 的
[CI 35682667713](https://github.com/tf4fun/attyd/actions/runs/35682667713)
再次通过全部 14 个执行 job：Rust **588/588**、Vitest **456/456**、浏览器 **94/94**，
Rust 行覆盖率 **93.39% ≥ 85%**；非 tag 运行的 release job 按条件跳过，不能据此宣称
已发布版本。

后续修改仍遵守以下验证顺序：

1. Red：新增 W1 和其他行为测试，记录失败断言，不忽略、不 `should_panic`、不放宽 exit code。
2. Green：统一工作派生、资源生命周期与让位逻辑；上述门槛全部通过。
3. `npm run test:idle-retirement`、`npm run test:rust`、`npm run check`。
4. `npm run test:remote`、`npm run test:ui`，最后浏览器回归；复核真实 stdio + REST/SSE 的
   原复现序列，确认对目标的自动退休已消失，fork HTTP 成功。

## 9. 兼容性、取舍与交付

退避仍是 250ms、500ms、1s、2s、4s、8s 上限；上限针对等待间隔，不是总重试次数。
持续可重试失败会持续保留正在工作的 owner。这符合“工作期间不回收”；若产品需要
可选同步总时限，应另行定义到期 fallback 策略，不能用 idle timeout 隐式取消工作。

日志可记录 epoch/incarnation/flow/attempt、retry delay、完成或取消原因；不记录历史正文，
不为本修复引入新的公开协议或监控平台。

本次已按「规范与 Red 测试 → 完整生命周期修复 → 集成验证」完成，生产实现包含
清理和业务让位，并通过上述 Green 准入。历史 Red 数据作为回归依据保留；后续合并和
发布仍以目标提交的 CI 及版本发布流程结果为准。
