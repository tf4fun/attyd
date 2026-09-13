# Session 隔离实施记录

目标是完整落实 [重构方案](session-isolation-proposal.md) 的三个阶段。以下记录区分已经落地的
切片与仍需迁移的职责；前端解耦、共享 payload 或新增类型本身都不代表 session 隔离完成。

## 实施状态

三个阶段已完成并接入生产；最终 Rust、前端、完整浏览器与传输检查均已通过。

| 要求 | 当前状态 | 代码／验证依据 |
| --- | --- | --- |
| 保留 permission／elicitation 重连修复 | 已保留 | bridge 回调有序登记，连接任务承接用户等待；两项 pending reconnect 原生回归通过 |
| 活动 prompt／overlay 只有一个权威 folding 来源 | 已移除生产 Runtime 的第二次 folding | 历史 reducer 产生不可变 `Arc<Vec<Value>>`，发布 DTO 共享相同分配；`runtime_delivery_shares_the_authoritative_turn_payload` 验证指针共享、旧快照不可变和错误归属不改变状态 |
| 单 session 权威状态和转换入口 | 已接入生产 | `SessionRegistry::sessions` 唯一拥有 `SessionState` 与非 Clone 的 `SessionResources`；operation、turn、metadata、responder、ingest 和 materialization 均已收敛 |
| 删除 BridgeState 的 session 级镜像准入集合 | 已删除集合，统一 operation owner 已接入 | `prompts` 和 fork／close／control／delete／attachment 五组 `pending_*` 已删除；Close/Delete 预留保留到资源清理完成，结清检查 incarnation 和操作 ID |
| RuntimeState 降为派生发布模型 | 已完成 | RuntimeJournal 只保存已提交记录，SessionRuntime 是一次性 DTO；生产 `active_sessions` 已删除，workspace 与 controls 从 canonical entry 借用 |
| 每 session 有界队列和外部操作回投 | 已接入生产 | command、ACP notification/request、RPC completion、terminal snapshot 和 observer 意图进入共同入口；外部等待释放 ExecutionTurn，结果重新取得同 owner 的票据 |
| 有序协议入口、发送／取消屏障、迟到结果隔离 | 已完成 | 发送前登记、response／EOF 单次认领和 completion handoff 已用于命令 RPC；取消按 RequestId + allocation 捕获，迟到结果按 epoch／incarnation 隔离 |
| mailbox／全局预算和过载可见失败 | 已完成 | ordinary／required、long／control RPC 与 session／global 队列分别计费；普通请求明确拒绝，必要入站过载明确报告并结束连接 |
| 已 materialized session 恢复不等待 list | 已实现 | `use-acp.ts` 独立恢复；有 list 时首次本地未命中等目录后重试，list 失败不会关闭仍可用的连接 |
| 全局 SSE 只接收其作用域 | 已实现 | `subscribe_global` 不构建 session snapshot/replay；认证 bootstrap 独立有界；作用域过滤、队列压力、总额度与清理回归 |
| 单 session Observe 切点与缺席期限所有权 | 已完成 | Observe 登记、revision 捕获和 ready marker 同锁提交；Hub pending delivery 收到 marker 才激活。取消 lease 与缺席计时归 session resources |
| Hub 不再参与业务生命周期推导 | 已移除 Hub 的 auto-close 决策 | SessionObservers 管理 lease 与缺席区间，定时器只回投带 incarnation／absence ID／permit 的关闭意图 |
| 兼容 REST/SSE、派生投影无业务决策权 | 已实现 | API 格式保持；aggregate 测试兼容与 legacy projection 是只读交付路径，不拥有 session 准入或缺席关闭决策 |
| 完整行为／传输／浏览器验收 | 已通过 | Rust 451 项、Vitest 281 项、Playwright 76 项，以及 REST/SSE、HTTP/SSE、WebSocket、ACP/MCP 和进程边界检查 |

## 实际生产分层

会话目录管理与已打开会话的交互状态分离：list/new 属于全局管理，删除未打开的历史会话也走
全局 RPC/completion 队列，只持有按 sessionId 限量登记的删除操作标识，不分配 SessionRuntime、
incarnation、历史或 session 队列。Agent 只收到 `session/delete`。已打开会话的删除仍需通过其
既有状态机协调互斥、关闭和资源清理。冷删除期间允许其他全局操作，拒绝相同 ID 的删除、打开
和创建结果认领；失败释放登记，重试使用新标识，迟到清理不能释放新操作。
`cold_session_deletion_*` 与 `catalog_deletion_reservations_*` 原生回归覆盖这些边界。

| 部件 | 责任 | 外部等待 |
| --- | --- | --- |
| `SessionRegistry` / `SessionState` / `SessionResources` | 唯一 session 状态、准入、历史、交互句柄及观察者归属 | 本地提交期间不等待 Agent、用户或文件/终端 I/O |
| `bridge/coordinator.rs` | 捕获 owner、分配正式 incarnation、新建/Fork 响应切点、暂存与路由 | 不等待单个 RPC 完成 |
| `bridge/scheduling.rs` / `session_dispatch.rs` | 共同入口、每 session FIFO、独立全局队列、轮转和有界执行票据 | 外部工作开始前归还票据 |
| `ordered_ingress.rs` / `completion_handoff.rs` | 发送前登记、response/EOF 单次认领、响应按原 FIFO 交回 | RPC 等待持独立请求预算，不占 session 执行权 |
| `bridge/inbound_requests.rs` / `agent_dispatch.rs` | 入站请求 allocation、typed reply、权限/表单等待、文件/终端/MCP effects | 数量和字节有界；需要写状态的结果通过 continuation 回投 |
| `SessionObservers` / `SessionObservation` | Observe 原子切点、lease、缺席期限、正常结束与硬取消 | 定时器只投递带归属的意图 |
| `BridgeHub` / `use-acp.ts` | 已提交视图交付、全局/会话订阅过滤、浏览器恢复 | 已 materialized session 的恢复独立于列表请求 |

这些队列共享 ACP 连接及进程。短暂的 canonical 提交仍使用 `BridgeState` Mutex；执行隔离来自
按 owner 排队和释放外部等待期间的票据，不要求每个 session 常驻一个 Tokio task。连接终止、
Agent 自身串行执行及连接总预算耗尽仍是共享边界。

## 已执行的切片验证

需要给本机命令添加 Node 路径：`/Users/xiaxilin/.nvm/versions/node/v26.8.2/bin`。

- 前端：36 项 `use-acp.test.ts` 通过，包括慢列表、列表错误、冷态本地未命中、导航与恢复迟到结果。
- `npm run check` 通过：TypeScript、281 项 Vitest 和生产客户端构建。
- Hub：`cargo test --features dev server::tests::`，72 项通过。
- Runtime：`cargo test runtime_state::tests::`，67 项通过，包含活动数据共享回归。
- 原始 pending permission／elicitation reconnect：两项通过。

第二个检查点（删除准入集合、Journal 提取、取消握手修复后）：

- `cargo test --features dev`：340 项全部通过。首次沙箱内运行的 8 项本地监听／子进程检查因
  `Operation not permitted` 失败，沙箱外完整复测通过。
- `npm run check`：281 项前端测试、三组 TypeScript 检查和客户端构建通过。
- pending reconnect 原生回归现同时证明：A 等待交互时，list、B 的冷恢复及 prompt、A 的 mode
  control 均可完成；A 的原交互保留，重新回答后原 turn 完成。
- 显式 Blocked 恢复保留已完成 overlay，缺少回答的 replay 不能替换 baseline；完整 replay
  提交后，Attachment owner 结清之前仍不能开始下一 turn。
- 冷恢复自动重试只允许同一 materialization owner 继续，普通 load 不抢占重试等待。
- Ordered ingress 的 11 项组件测试已纳入上述 Rust 计数，不代表已接入生产调度。
- 同一检查点已通过 7 项相关 Playwright（含慢 list／失败 list 的交互恢复、Agent 取消、Send now
  和 stdio 重连）、REST/SSE UI smoke、HTTP/SSE 与 WebSocket 远程 Agent smoke。

第三个检查点（session map 合并后）：

- `cargo test --features dev`：343 项全部通过。
- 原 Runtime 67 项、历史 28 项、纯状态 8 项全部通过；历史侧新增同 incarnation 保留 live／owner、
  Close 清理屏障、新 incarnation 替换时释放旧 history／candidate／overlay／retained terminal 计费。
- `SessionHistoryView` 单独表示发布的历史字段，不因构造 view 而克隆 live 资源或 CAS tombstone。
- 此时仍存在待消除的 live operation／turn execution 重复表示；一个 map 本身不是全部完成条件。

第四个检查点（operation／execution 统一及 SessionEntry 资源容器）：

- Runtime 75 项、纯状态 8 项、历史 30 项、SessionRegistry 5 项分别通过。
- Runtime DTO 不再拥有可写 operation／活动 turn；prompt 走一个 CAS 准入入口，关闭保留 cleanup
  owner，cleanup 窗口的迟到 permission／elicitation／terminal 写入被拒绝。
- 自动 load 重试即使已有 resume 缓存 revision 也保持 Loading；显式 reload 失败恢复原准入 phase。
- Fork 新回归使用真实 source idle update 和响应屏障，证明 target fallback 使用准入时捕获的历史。
- Observe/lease 13 项、4 项真实挂起重连／缺席关闭 fixture、26 项已有订阅兼容检查通过。
- SessionObservers 6 项通过，覆盖计时、取消、替换与已发送关闭的边界。
- per-session dispatch 的 13 项组件测试独立通过；与 ordered ingress 一样，仍未接入生产调度。
- 上述检查期间已有后续资源迁移在编辑，尚无此检查点的完整 Rust／浏览器验收。加载等待者刚迁入
  entry，具体失败错误应先送达、再退休资源；该新增回归和 responder 迁移需在编译稳定后验证。

后续资源／发布切片（仍在实施）：

- Session responder 已迁入 `SessionEntry.resources`；连接只保留带 epoch／incarnation 的轻量路由，
  request scope Elicitation 独立。旧 incarnation 的 effects 不会取消同 ID 新资源。
- GET／Observe materialization waiters 已迁入同一资源条目；重试安装新 incarnation 时显式移动
  同一个 materialization，历史移除后由结果 owner 先发送具体错误，再做最终资源退休。
- Prompt 完成改为一次同步提交：精确 RPC／turn 校验、交互退休、内存历史提交和 view／outcome
  入 outbox 完成后才开放下一 turn。新增交错测试与既有边界共 4 项通过。
- 一次固定 binary 完整回归为 383 通过、8 失败。失败包括加载 Blocked 状态被过早清理的真实
  回归，以及旧测试跳过 cleanup／重复预留操作的断言。已修／正在适配，尚待新构建复验。
- 正常订阅结束正在改用同序 end marker 并排空已有交付；硬取消仍用于断开和慢队列淘汰。
  该切片新增测试尚未完成联合构建验证。
- `active_sessions` 元数据和 ingest buffers 仍需完成迁移；生产调度尚未使用 per-session FIFO。

第五个检查点（ingest、交互资源、原子完成与正常 Observe 结束）：

- 联合 `cargo test --features dev --no-run` 通过，零警告；固定 binary 完整运行 **395/395 通过**。
- 先前 8 个失败已处理：Blocked materialization 保留可查询状态；404 先送达等待者再退休；
  测试使用唯一 operation 与完整 cleanup。
- Bridge 的 session ingest maps 已删除，未知 ID 只有有界 CreationStaging，正式认领时保留
  原 validation allocation；已知 session 的缓冲、校验与回滚资源只有一个条目。
- 正常关闭的 observer_end 与已提交事件同序，HTTP 排空前缀后 EOF；hard cancel 仍立即终止。
- 固定 debug binary 上的 7 项相关 Playwright、REST/SSE UI smoke、HTTP/SSE 与 WebSocket
  远程 Agent smoke 全部通过；这些命令没有重建 Rust。
- Completion handoff 的 13 项组件回归通过（加 ingress／dispatch 共 37 项），覆盖响应先到、
  旧 request ID、预算释放后唤醒及最后 producer 退出；尚未代表生产队列接线完成。
- `active_sessions` 与生产 FIFO 接线仍未完成；正在修复 session/cancel 二次选取交互的竞争，
  并为可复用 URL ID 补充 registration 身份。

第六个检查点（唯一 metadata 与生产 FIFO）：

- 最新联合 `cargo test --features dev --no-run` 通过；固定测试产物完整运行 **438/438 通过**。
- 命令、Agent callbacks、RPC response／EOF、terminal snapshot 与 observer 事件已经走生产队列；
  初始化与请求作用域 MCP 保留明确的连接级 SDK 路径。
- `send_ordered` 在发送失败时保留当前票据供回滚，发送成功才让出票据；响应完成后由 FIFO
  交回执行权。Fork 目标认领在 raw response 切点进行，后续输入有界暂存。
- `active_sessions` 已删除；canonical cwd 使用 PathBuf，control 值含空数组更新均来自同一 owner。
- 该检查点之后的审查修正正在验证：PreparedAttachment 拒绝回滚、Fork 首次 baseline 顺序、
  入站请求取消与外部任务字节预算、creation 暂存请求关闭结果、terminal release 终局发布。
- 438 项结果不覆盖这些后续改动，也不替代新生产产物上的浏览器和远程传输验收。

最终检查点（边界修正后）：

- Rust 全套 **451/451 通过**，包括真实 SDK request → cancel → update 和 creation response →
  request → cancel → update 的同批消息测试；批次回复按 JSON-RPC 数组验证。
- `npm run check` 通过：三组 TypeScript 检查、**281/281** Vitest 与客户端生产构建。
- `cargo build` 通过；`cargo fmt -- --check` 与 `git diff --check` 通过。
- 同一生产 binary 的 REST/SSE UI smoke、HTTP/SSE 与 WebSocket remote smoke、ACP/MCP
  protocol smoke、HTTP/process boundary smoke 均通过；后者验证开放 SSE 时的正常关闭及
  Agent 子进程树退出。
- PreparedAttachment 重复 request ID／停止分发先精确回滚；Fork 的 baseline 在释放 claim
  前安装；失败冷加载可保留 Blocked 视图，同时回收无排队或执行工作的 session 队列。
- 入站任务在释放 ExecutionTurn 后仍持独立数量与字节预算，Finished 在回复前入队；
  wire cancel 只结清捕获的 allocation。终端 append 的消费与双通道入队在同一短锁内完成，
  release 发布后不再接收迟到的退出快照。
- 完整 Playwright **76/76 通过**，含刷新恢复 permission／elicitation、列表阻塞／失败、Agent 取消、
  stdio 重连、终端保留、认证和 Vite 代理。

这些结果对应上述切片完成时的工作区。后续状态、调度和发布修改后，应重新执行受影响的门禁。
最终检查点已核对准入矩阵、更新／响应顺序、发送前取消、旧 incarnation、加载单飞、
观察切点、队列过载以及 stdio／HTTP／WebSocket 行为。上述早期检查点保留为迁移历史。

## 审查后的职责边界补齐

- 仅存在于 Agent 目录、没有本地 session entry 的会话删除只登记全局目录占位，不创建
  SessionRuntime、历史或 session 队列；
  已有运行态的删除由目录管理发起，并协调该运行态关闭与资源清理。
- 每个列表操作在等待 Agent 列表锁之前申请独立配额，最多 32 个。释放执行票据允许其他
  全局操作继续；HTTP 接收端超时不会提前归还仍在执行的列表配额。
- New／Fork／Delete 成功推进目录版本，公开 `bridge/catalog_changed`。列表结果携带
  `catalogRevision`，续页提交 `expectedCatalogRevision`；旧在途结果和跨版本分页都拒绝
  写入目录缓存。浏览器从第一页重试一次，并合并目录失效通知。
- 关闭确认后发布含 epoch／incarnation 的 `bridge/session_retired`，再结束观察流。
  Delete 中关闭成功而删除失败时，仍发布 closed 结果，目录记录保留。
- 首次打开可以加载；已有观察的 GET 刷新和 SSE 恢复携带 expected owner。同连接内旧 owner
  已结束或被替换时返回 `409 session_retired`，不会重新加载。整条连接 epoch 改变则返回
  `409 bridge_replaced`，交给全局连接恢复保留路由和草稿、从 Agent 历史重新打开。
  `/api/v1/runtime` 公开 canonical epoch；SSE 同时核验 Last-Event-ID。
- 浏览器保留展示数据的 owner 标识；删除等待只登记 pending，保留内容和草稿并禁用提交。
  成功／退休结果按 owner 清理，HTTP 导航结果还校验导航版本，避免旧回复影响同 ID 新实例。

这些变更补充公开观察协议与目录管理约定，没有增加第二份可写生命周期或另一个 Actor 层。

本次修正后的验收：Rust **467/467**、Vitest **301/301**、完整 Chromium **81/81** 通过。
TypeScript 检查、客户端与 release 构建、REST/SSE、HTTP/SSE 与 WebSocket 远程传输、
ACP/MCP 协议、HTTP／进程清理及独立二进制检查通过。连接替换恢复保留路由和草稿；
观察流断开时先确认全局连接可用，再读取对应 owner，避免 Agent 已停止后的多余 503 查询。

## 迁移注意点

- SessionState 可以组合生命周期、历史同步、turn、独占操作、交互等子状态；不可化成单一 Busy。
- 显式 load 恢复与自动 materialization 重试分开；Blocked 不应无条件禁止用户显式恢复。
- close 的上游响应与本地资源清理不是同一时刻。释放历史状态或准入集合时要保留旧 incarnation
  的关闭占用，避免新 session 在终端清理期间抢占同一个 ID。
- RPC 发送登记与快速响应关联要有短同步屏障，响应、通知和交互都进入同一有序入口；
  Agent 业务错误须作为结果事件处理，不得从 SDK 回调传播成整连接失败。
- 原有请求作用域 Elicitation／URL／MCP 的 aggregate 兼容行为保留；全局业务 SSE 切片没有扩大
  其公开事件类型集合。
