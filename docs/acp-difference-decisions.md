# ACP 差异分类与处置

本文件承接 [2026-09-08 逐接口审查](acp-release-audit-2026-09-08.md)。原报告是固定提交的发现记录；本文件区分不再实现的架构范围、需要讨论的取舍，以及应直接修复的错误。A/Z 编号沿用原报告，避免把不同性质的差异混成一个缺陷清单。

本轮结果：明确放弃 8 类架构不支持项，保留 8 项待讨论权衡，完成 attyd 的 19 组明确缺陷修复。全部前端、Rust、协议集成和 Chromium 检查通过；具体范围与验证记录见下文。

## 分类规则

- **架构不支持项**：当前无持久化 Web 会话客户端不承载的能力。明确不支持，不为模仿 Zed 增加数据库、编辑器或厂商适配。
- **权衡项**：规范允许不同实现，或涉及可选能力、并发、资源与体验选择。说明 ACP、Zed、attyd 各自行为；讨论前保留当前产品规则。
- **缺陷项**：已经接入/声明的功能违反字段语义、能力协商、作用域或必要 UI 义务，或对合法输入产生错误结果。直接修复，补行为回归。

“架构不支持”表示本项目确定不扩展的架构/产品边界，不表示技术上永远无法实现。可选接口未开放与调用未协商接口是不同问题：前者可以是取舍，后者必须修复。Zed 自身偏离 ACP 也不构成要求 attyd 复制错误的理由。

## 一、架构不支持项：明确放弃

| 项目 | ACP / Zed 背景 | attyd 的明确范围 |
| --- | --- | --- |
| 客户端持久化会话库、跨重启本地历史恢复 | ACP 不要求客户端保存历史；Zed 有编辑器自身的存储和线程状态 | 不引入数据库、文件、浏览器存储或临时落盘作为聊天恢复源。冷恢复仅依赖 Agent。 |
| 跨重启恢复客户端 terminal 输出、发送意图及历史 stop 边界 | Agent load 不保证重放客户端独有数据；Zed 的本地实体不等于协议可重建 | 不重执行历史 command，不伪造输出或跨重启 exactly-once。缺失时明确不可恢复。 |
| 编辑器 buffer、未保存文件、format-on-save、编辑器事务/文件跳转 | Zed 的 filesystem 与 Project/buffer 集成 | attyd 读写实际工作文件，不新增项目文本编辑器或本机编辑器启动器。 |
| NES 和 document 同步 | 编辑器草案，不是稳定会话客户端必需项 | 不声明或实现 `nes/*` 与 `document/*`；不重新引入旧编辑器面板。 |
| 模型 provider 配置与凭据管理 | provider 配置仍为草案；Zed 产品内也有自己的模型设置 | 不实现 `providers/list`、`providers/set`、`providers/disable`；认证仍由 Agent 提供标准流程。 |
| 远程 Agent 使用 attyd 宿主机 fs/terminal/terminal auth | 能力可选；两端不一定共享机器、文件系统或启动命令 | HTTP/WS 模式不声明这些本地能力，不把远端 cwd 映射成本机目录。Agent-handled 认证仍可用。 |
| Goose/Zed 私有 metadata 解释和厂商错误文本改写 | ACP 允许私有扩展，但不要求第三方复制；对应 Z07 | 保留可检查的 metadata，不用厂商字段猜工具 schema、历史或生命周期，不按 Gemini/Goose 文本伪造标准结果。 |
| 已退出命令的永久后台服务管理 | ACP terminal 是临时句柄，不是跨进程守护服务注册表 | 不恢复/重新接管 detached 服务；后台服务由其启动者管理。具体活跃句柄的清理策略见权衡 T05。 |

这些项目不列入后续缺陷修复待办。未声明的新草案也不因 SDK 加了类型就自动进入支持范围。

## 二、权衡项：保留现状，进一步讨论

| 编号/对应审查 | ACP 允许什么 | Zed 当前实现 | attyd 当前实现 | 需要讨论的决定 |
| --- | --- | --- | --- | --- |
| T01 / A10 | 生成中可切 mode/config；close 可取消活动工作 | 控制请求不以 prompt idle 为前置，thread release 可 close | 同 session 的 prompt 与控制/lifecycle mutation 互斥 | 是否拆分控制并发，支持生成中调整及一体关闭，还是保留“先停止再操作”的简单状态机。 |
| T02 / A18 | 不规定客户端累计历史内存预算 | 编辑器实体和资源生命周期与 Web bridge 不同，不能直接移植预算 | 明确选择完整保留当前物化会话，不按累计字节截断；关闭/释放时回收 | 是否允许按内存压力淘汰；若允许，如何告知无 load Agent 的不可恢复损失，以及如何固定活跃/被观察会话。 |
| T03 / A07、A08 的无历史部分 | resume 独立于 load，不返回旧历史；fork 能力也不隐含 load | resume 可继续而不补 load；固定外部 adapter 没有 fork | 公开冷恢复需要 load；fork 的完整历史附件路径也依赖 load | 是否新增明确“旧历史不可见但可以继续”的 resume/fork 体验。讨论前不开放该流程；A07 修复仍须在 Agent I/O 前拒绝不支持的组合。 |
| T04 / A21 | 有 configOptions 时 SHOULD 排他使用它，legacy modes 仍在 v1 | 存在 configOptions 就不用 legacy modes | 仅发现 mode 类 config 时隐藏 legacy modes | 遵循统一排他建议，还是保留只有 model config 时额外显示 modes 的兼容体验。 |
| T05 / 终端执行 | 要求 create/output/wait/kill/release 语义，不规定 shell、PTY 或 detached 子进程政策 | 项目环境、默认 shell、PTY、编辑器任务清理 | Unix 非交互 /bin/sh、pipes、字面 argv；正常退出不补杀后台组，运行中显式终止清理 | 是否需要可选 PTY/用户 shell/项目环境；此前确认的后台服务行为保持不变，不能以“Zed 就这样”为理由反转。 |
| T06 / 卡片与媒体 | 必要字段语义和内容必须正确；不规定布局、折叠、编辑器导航或所有二进制播放器 | 多种编辑器呈现方式；本版本 audio/混合媒体仍有降级 | 统一卡片、原始细节折叠、Web media；binary resource 暂为摘要 | 是否增加下载/更多预览，以及信息密度。URL consent 信息、取消状态和合并丢字段属于缺陷，不能归在折叠风格取舍中。 |
| T07 / transport 与 Draft | stdio 是 v1 基线；HTTP profile 与其他草案仍演进，允许自定义 transport | 外部客户端以 stdio 为主要路径，未接通若干 attyd 草案 | 已提供 pinned SDK HTTP/SSE、WS、MCP、ID plan/compaction | 以后如何固定/升级 profile，哪些草案进入稳定发布承诺。现有已声明功能的死锁/字段错误仍直接修复，不等待范围讨论。 |
| T08 / 文件写入执行策略 | 规范不要求原子替换或递归创建父目录 | buffer transaction，可能 format-on-save | 真实文件原地覆盖，父目录须存在，写入前允许取消 | 是否增加原子替换/目录创建策略；不要把 Zed 格式化后的写入当作通用协议语义。 |

### 对原报告 A18 的更正

原审查将累计内存无预算列为实现缺陷，但 [运行态契约](active-turn-runtime.md#memory-model)及 [TDD ledger](bridge-state-machine-tdd.md#memory-invariants)已经明确选择不截断协议有效历史。因此本轮将它改为 **T02 架构资源权衡**。继续记账且关闭时回收；不擅自增加 LRU、历史裁剪、终端快照淘汰或持久化。文档中若把“单项有界”写成“累计内存有界”，则直接纠正文案。

## 三、缺陷项：直接修复

下表的 19 组明确缺陷均已完成修复，回归入口和完整验证见第四、五节。A07/A08 只修已公开流程的错误；无历史 resume/fork 体验继续保留为 T03，不将扩展能力记作已完成。

| 原编号 | 明确错误 | 修复边界 |
| --- | --- | --- |
| A01 | 会话 cwd 正确但客户端 fs/terminal 仍用启动 cwd | 使用 session 主根/额外根，保持 incarnation 与越界保护；同根因的 `@` 文件搜索/读取也应使用所选 session。 |
| A02 | 已存活会话的 turn 外更新静默丢弃，工具 ID 限于本 turn | 保留 session 级实体与合法 idle 更新；继续隔离旧 incarnation、已关闭会话和失败的加载事务。 |
| A03 | 不同 messageId 的消息被历史折叠合并 | 保留消息边界；不同 annotations 也不能因文本合并而消失。 |
| A04 | rawInput/rawOutput 错误 deep merge | 只替换提供的整字段，省略保留；content/locations 按集合语义处理。 |
| A05 | 本地 prompt 与 Agent 单块/多块回显重复 | 按发送/回显边界匹配；保留用户正常的重复提问。 |
| A06 | 稀疏 info/usage 快照丢 title/cost | 按字段保留省略值、处理显式 null，确保快照与通知路径一致。 |
| A07 | fork/resume 成功后发送未协商 load | 在任何 Agent I/O 前检查完整历史路径前提；不擅自开放 T03 的无历史体验。 |
| A08a | 无 list 时页面异常、load-only 已知 ID/cwd 无法恢复 | 按能力刷新列表，允许内存会话；有 load 能力且 ID/cwd 有效时，GET/SSE 可直接尝试恢复，由 Agent 确认是否不存在。无 load、无内存会话或非法路由仍不能恢复。 |
| A09 | 两个观察者的合法分页 cursor 互相失效 | 浏览器分别检测分页循环；bridge 排队转发不透明 cursor，按去重后的保留元数据计费；GET/SSE 保留已知 cwd 后备，避免另一观察者刷新列表造成误报 404。 |
| A11 | cancelled turn 的未完成工具永久转圈 | 增加本地派生取消状态，保留原 wire status，允许后续 Agent 更新。 |
| A12 | URL consent 不显示真实 Agent、host、完整 URL | 同意前默认显示必要信息；保持不预取、不自动打开。 |
| A13 | 已终结的 URL elicitation ID 被永久禁止复用 | 只约束 outstanding 唯一性，两层 tombstone/待决状态一起修复。 |
| A14 | 半个 UTF-8 字符变为超出字节预算的替换字符 | 在 terminal/output 服务本身保持完整字符边界。 |
| A15 | MCP 回调中反向请求导致同连接死锁 | reader 与回调任务分离，保持取消、断开和 pending 资源清理。 |
| A16 | JSON Schema 字符长度误按 UTF-16 单元计算 | Rust 与浏览器一致按 Unicode 字符约束，保留表单格式/默认值验证。 |
| A17 | 未声明 elicitation mode 返回 -32600 | 返回规范的 -32602，区分请求 envelope 与参数错误。 |
| A19 | logout 被额外要求 authMethods 非空 | 接受合法能力组合，按 logout capability 门控。 |
| A20 | compaction 将同 ID 消息拆成多个实体 | 保持已声明草案要求的首次实体位置和后续块归属。 |
| A22 | 远程 Windows cwd 可以 list/load，不能 new | 远程参数按 portable Agent 路径校验，本地访问仍按本机文件系统验证。 |

本轮不修 Zed 代码，不引入客户端持久化，不通过取消已有正确能力来掩盖明确缺陷。

### 参考实现中的缺陷：归属 Zed

Z01 版本检查、Z02 截断方向、Z03 HTTP MCP 门控、Z04 条件性 terminal auth 分发、Z05 title:null 处理，归为 **Zed 的缺陷项**，不计入 attyd 的 19 组修复。attyd 保留当前更符合标准的行为，不复制这些错误。Z06 媒体显示差异归权衡 T06；Z07 厂商 metadata 兼容归架构不支持项。Zed 的结论与适用条件来自原报告的固定版本静态审查，未运行 Zed 验证。

## 四、修复实现与回归入口

| 缺陷 | 实现与可重复检查 |
| --- | --- |
| A01 | [filesystem](../src/filesystem.rs) 按会话 cwd 重建主根，保留只读和额外根策略；[bridge](../src/bridge.rs) 的文件、终端与 `@` 搜索/读取共用会话作用域。`workspace_context_follows_session_workspace` 和协议 smoke 覆盖切换项目及拒绝启动目录越界。 |
| A02 | [session mirror](../src/session_mirror.rs) 和 [semantic](../src/semantic.rs) 接受合法 idle 更新、保留 session 级实体。加载验证使用候选索引，失败恢复旧索引；本轮同时补充异步通知、自动加载与关闭重开的代次隔离回归。 |
| A03、A04、A05、A06、A20 | [history cache](../src/history_cache.rs)、[runtime](../src/runtime_state.rs)、[browser reducer](../web/src/lib/state.ts) 统一消息边界、整字段替换、prompt 回显、稀疏控制快照与 compaction 实体归属。[state tests](../tests/state.test.ts) 及对应 Rust 模块检查重建与即时状态。 |
| A07、A08a、A09、A19、A22 | [独立能力 Agent fixture](../tests/fixtures/session-capabilities-agent.ts) 与 [server tests](../src/server.rs) 覆盖缺失 list/load、I/O 前拒绝 fork、分页缓存竞争及 GET/SSE 冷恢复。[use-acp tests](../tests/use-acp.test.ts) 覆盖路由与观察者行为；App/state 测试覆盖空 authMethods 的 logout。[remote smoke](../scripts/rust-remote-smoke.ts) 在 HTTP、WS 均检查 Windows drive-letter/UNC 新建路径原样传递。 |
| A11、A12 | [tool card tests](../tests/tool-call.test.tsx) 检查取消展示且不改写 wire status；[UI interaction tests](../tests/ui-interactions.test.tsx) 检查 consent 默认显示真实 Agent、目标 host 和完整 URL。 |
| A13、A15、A17 | [ACP protocol smoke](../scripts/acp-protocol-smoke.ts) 验证 URL ID 终结后复用、outstanding 重复拒绝、同连接 MCP 嵌套回调及不支持的 mode 返回 -32602；runtime/reducer 同时验证新 URL 流程不继承旧完成状态。 |
| A14、A16 | [terminal tests](../src/terminal.rs) 覆盖运行中拆分 UTF-8 与 byte limit；[elicitation validation](../src/elicitation_validation.rs) 和 UI 测试覆盖 Unicode 长度。同步消除 HTML required/pattern 与 JSON Schema 语义冲突，并区分空字符串枚举与“未指定”。 |

针对原审查反例及新发现的竞态，先确认失败，再修复并保留自动回归。个别临时黑盒探针保留在本机 `/private/tmp`，不是仓库可重复测试的替代品。

测试 Agent 同步区分普通确定性错误与 `ResourceNotFound`，为不同回合的独立消息分配不同 ID，并对不存在的会话返回标准错误。浏览器的阅读高度、底部跟随、文件修改归属和缺失会话回主页断言保持原有要求。

代次隔离的边界：A02 必须拒绝“处理期间所属会话已关闭或替换”的通知和旧加载回滚。ACP `session/update` 只有 sessionId，没有 turn/load/incarnation 标识；若旧通知在同 ID 重开后才首次到达，客户端无法仅凭标准字段还原它的旧归属。本项目不增加私有协议字段来伪造这种保证。

## 五、验收记录

- `npm run check`：通过，239 项 Vitest、三个 TypeScript 配置检查及生产前端构建通过。
- `npm run test:coverage`：完整通过，包含 285 项 Rust 测试、REST/SSE UI、HTTP/SSE 与 WebSocket 远程传输、服务边界、ACP SDK 协议 smoke，以及 57 项 Chromium 测试。
- Rust 行覆盖率 **89.81%**，超过仓库要求的 **85%**；未降低门槛。
- 最终 fixture 的 TypeScript 检查、`cargo fmt --check`、`git diff --check` 均通过。

完整 Chromium 回归保持严格的阅读位置要求：离底 24px/500px 发送、流式上滚和历史搜索后的发送均检查 scrollTop 与原文锚点；位于底部时继续跟随新增内容。缺失会话回主页和文件修改留在原回合的断言也全部通过。

本轮验证使用仓库 fixture、官方 SDK 对端及临时本地实例。Zed 仍是固定源码版本的静态参考；这些结果不将未讨论的权衡或明确排除的架构能力转化为支持承诺。
