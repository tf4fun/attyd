# ACP 协议与页面展示对照检查

检查日期：2026-09-26。检查对象是当前工作区，包含尚未提交的附件、工具排版及静态样式页改动。

## 结论

当前 SDK 定义的 **5 类 ContentBlock、3 类 ToolCallContent、10 类工具 kind 和 15 类 SessionUpdate 都有处理路径**。主要问题集中在已支持分支内部的信息展示，以及取消动作与页面反馈之间的时间差。

本次确认 **6 项应补齐的问题**：权限卡的 `null` 合并语义、权限正文展示、取消过程反馈、配置分组名称、旧版 mode 描述、表单选项描述。另记录两项展示取舍和静态样式页的覆盖缺口。下文将协议语义错误、协议建议和产品展示选择分别注明，避免把每一个未放在正文里的字段都算成不兼容。

后续补齐已实施 D01–D06，并将对应交互加入静态样式页。下文保留修复前的审查证据；当前实现与回归验证见第 10 节。

## 1. 对照基线

| 项目 | 本次基线 |
| --- | --- |
| 生产协议 | ACP v1，以能力协商后的范围为准 |
| Rust SDK | `754d5aa1ce2cfa54ba2c2a6d3edc7e7b6bce28eb`，schema `1.7.0`，启用 `unstable`；见 [Cargo.toml](../Cargo.toml) / [Cargo.lock](../Cargo.lock) |
| TypeScript SDK | `@agentclientprotocol/sdk` `1.4.0`，使用 v1 入口 |
| 内容与状态范围 | 11 类稳定 v1 SessionUpdate，另有已接入的 `plan_update`、`plan_removed`、`compaction_update`、`compaction_summary_chunk` 扩展 |
| 判断方式 | 官方规范 → SDK 字段 → Rust 校验/投影 → 浏览器 reducer → React 展示及交互 → 测试与样例 |
| 产品边界 | 通用 ACP client；附件由浏览器新页面处理；会话缓存留在 attyd 内存；不引入文件预览库、客户端历史数据库或厂商文本解析 |

官方 v2 当前仍标为 Draft。本次不把 v2 新增的消息修改、结构化文件变更或新版权限 subject 算作 v1 的遗漏。[ACP v2 Draft](https://agentclientprotocol.com/announcements/acp-v2-draft)

[协议分支覆盖表](../shared/protocol-coverage.ts) 能在 SDK 增加已知联合类型时要求代码重新分类，但无法证明每个可选字段都已在 UI 正确表达。本次检查补的是后一层。

## 2. 已确认的问题

### D01 · 权限卡用 `rawInput: null` 遮住已有输入

**性质：字段语义不一致，优先处理。**

- ACP v1 的工具更新中，`rawInput` 缺省或为 `null` 都表示保留此前值；权限请求携带的也是 `ToolCallUpdate`。[Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)
- reducer 已按这个规则合并工具，但 [PermissionCard](../web/src/components/acp/permission.tsx#L24) 使用 `hasOwnProperty` 再从原始请求取值。请求显式携带 `null` 时，卡片绕过了合并后的输入。
- 最小复现：已有工具输入 `{path: "/workspace/config.json", text: "Inspectable change"}`，随后权限请求仅更新同一工具的 `rawInput: null`。实际审批区显示 `Tool input null`，已有输入不可见。
- 建议：审批展示复用合并后的工具状态，或与 reducer 使用相同的 null/缺省规则；原始请求仍可留在诊断详情中。

### D02 · 权限请求的可读正文没有进入审批区

**性质：审批信息展示缺口，优先处理。**

- 权限请求可在 `toolCall.content` 中提供计划、说明、diff 等。官方 mode 切换示例就在这里提供 Markdown 计划，未提供 `rawInput` 或 `locations`。[Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes)
- [PermissionCard](../web/src/components/acp/permission.tsx#L20) 只使用标题、名称、kind、位置和原始输入，`inspectable` 也只检查输入和位置。
- 复现官方示例形状：审批区看不到计划，还会提示 Agent 未提供可检查的输入或位置。正文仍存在于原始请求，并由状态层合并进另一张工具卡，不能说数据已丢失；但用户需要另行展开才能阅读。
- 建议：在审批区复用通用工具内容展示，或提供明显的关联内容入口；将已有 `content` 纳入可检查上下文。

### D03 · 取消反馈要等到最终 stop response

**性质：尚未落实协议的 SHOULD 建议，优先处理。**

- 规范建议 Client 发出 `session/cancel` 时先将当前回合未结束的工具标为取消，并继续接受之后的工具更新。[Prompt Turn / Cancellation](https://agentclientprotocol.com/protocol/v1/prompt-turn#cancellation)
- [use-acp.ts](../web/src/lib/use-acp.ts#L910) 发出取消请求后只刷新会话，没有提交取消中的展示状态。
- Rust 会记录 `cancel_requested`，但它位于被 `serde(skip)` 的 `TurnExecution` 中；浏览器的 `BridgeTurnOverlay` 也没有这个字段。见 [session_state.rs](../src/session_state.rs#L28)、[runtime_state.rs](../src/runtime_state.rs#L1107)、[business-api.ts](../web/src/lib/business-api.ts#L28)。
- [finishTurnTools](../web/src/lib/state.ts#L2206) 要收到最终 `stopReason: "cancelled"` 才设置工具的本地取消标记。因此 Agent 响应取消较慢时，页面仍可能显示工具运行中。
- 建议：表达已请求取消的状态，并按规范更新未结束工具的本地显示；保留 Agent 原始状态，允许迟到更新纠正显示。实现时应遵循 [bridge 状态机验收流程](bridge-state-machine-tdd.md)，不能把用户意图当作 Agent 已经停止的事实。

### D04 · 配置选择器丢失分组名称

**性质：选择信息丢失。**

- ACP 的 grouped select 提供组标识、组名称和组内选项。[Session Config Options](https://agentclientprotocol.com/protocol/v1/session-config-options#grouped-select-options)
- [flattenOptions](../web/src/components/acp/session-controls.tsx#L229) 只保留组内 options，组名称没有进入展示或搜索。
- 最小复现：`Provider A / Fast`、`Provider B / Fast` 在选择器里成为两个相同的 `Fast`。提交的 value 仍正确，但用户失去区分依据。
- 建议：保留组标题；搜索同时匹配组名称；保留 Agent 给出的组和选项顺序。

### D05 · 旧版 mode 的描述没有展示

**性质：选择说明缺失。**

- [LegacyModes](../web/src/components/acp/session-controls.tsx#L10) 与后续映射只保留 `id/name`，未传递 `description`。
- 复现：Agent 提供 `Code` 及说明 `Changes files without asking`，菜单中只有名称。普通 config select 已能展示选项描述，旧版 modes 没有复用这一能力。
- 建议：沿用同一选择器传递和展示说明；继续保持有 `configOptions` 时优先使用它、显式空数组不回退 modes 的现有规则。

### D06 · 表单单选/多选项的描述没有展示

**性质：表单选择说明缺失。**

- SDK 的 `EnumOption` 包含 `const`、`title` 和可选 `description`。
- [ElicitationField](../web/src/components/acp/elicitation.tsx#L245) 在 `oneOf` 单选和 `items.anyOf` 多选中只映射 value/label。字段级 description 有展示，但每个选项自己的 description 被丢弃。
- 两种表单形状都已复现：选项标题可见，选项说明仅留在折叠的原始请求中。
- 建议：单选提供当前选项说明或可读的选项说明区域，多选在各选项下显示说明；不改变表单提交值。

## 3. 全部内容块对照

ACP 的五类内容块用于 prompt、消息和工具内容。嵌入资源分为 text/blob 两种载荷，并没有单独的 PDF、Word、Markdown 文件类型。[Content](https://agentclientprotocol.com/protocol/v1/content)

| ACP 内容 | 当前页面展示 | 元数据与边界 | 结论 |
| --- | --- | --- | --- |
| `text`：用户 prompt / 历史用户消息 | Markdown/GFM；按渲染后的实际高度判断，超过约 9 行才折叠，最后 3 行渐隐；内容、窗口宽度变化时重测，右侧展开全文，与普通正文平铺一致 | 完整解析 Markdown，保留块顺序及原始内容以便复用；不把厂商 `Resource:` 标记识别为附件 | 已实现；折叠判定与预览高度共用同一阈值，能完整放下的内容不渐隐、不显示展开按钮 |
| `text`：Agent 回答 | Markdown/GFM | 标题、列表、代码块、表格等由共用组件处理 | 已实现 |
| `text`：思考 / 工具 / 压缩摘要 | 各自容器中复用文本展示；思考和工具可折叠 | 不按工具名猜输出 schema；纯空白工具文本按无内容处理 | 已实现 |
| `image` | 紧凑附件入口；点击打开后端链接的新页面 | 保留 MIME、大小、可用 URI；没有名称时按 URI/MIME 生成标签 | 已实现；会话内不嵌入图片预览 |
| `audio` | 同一附件入口 | ACP audio 没有标准文件名字段；缺失原文件名不等于客户端漏读 | 已实现；会话内不嵌入播放器 |
| `resource` + `text` | 附件入口；正文留在会话内存，通过 HTTP 提供 | UTF-8 响应；`.md`、HTML 等由 MIME 和浏览器决定如何打开 | 已实现；不再把文件正文展开在消息中 |
| `resource` + `blob` | 同一附件入口 | 二进制字节、MIME、大小；浏览器负责预览或下载 | 已实现；不引入 PDF/Office 等渲染器 |
| `resource_link` | 名称/标题；工具输出为紧凑文件行，消息中为附件标签 | 有大小则显示；URI、description、MIME 在提示信息中；HTTP(S) 或本应用托管引用可打开 | 已实现；仅有 `file:`/自定义 URI 时不猜文件内容或读取宿主机文件 |
| `annotations` | 普通内容下显示 audience、priority、lastModified；工具内进入 Tool info | 不因 audience 提示而删除正文；保留无法解析的时间文本 | 已实现；展示层级见第 7 节 |
| `_meta` | 不解释厂商私有字段 | 后端保留原值；普通浏览器附件投影使用应用自己的引用元数据，显式会话导出仍可包含原始数据 | 已实现；不把私有 metadata 当通用协议 |

实现入口：[content-block.tsx](../web/src/components/acp/content-block.tsx)、[attachments.rs](../src/attachments.rs)、[hosted-attachment.ts](../web/src/lib/hosted-attachment.ts)。

发送侧保留有序的 `ContentBlock[]`；image、audio、embedded resource 分别受 Agent 的 `image`、`audio`、`embeddedContext` 能力控制，文件选择、粘贴、拖放及 `@` 文件引用汇入同一准备流程。再次发送托管引用时，后端先还原对应原始内容块，避免把仅供浏览器使用的附件引用交给 Agent。

规范对 text 的 Markdown 呈现给出 SHOULD 建议。用户 prompt 已按后续验收要求改为 Markdown/GFM，长文本保留折叠预览；显示转换不改变编辑重发或导出的原文。[ACP Schema / TextContent](https://agentclientprotocol.com/protocol/v1/schema)

附件的生命周期与会话内存相同。普通浏览器会话视图传引用，打开时从 attyd 已有缓存取字节；没有额外的附件数据库或浏览器持久化附件库。会话释放或进程重启后，能否再次打开依赖 Agent 历史是否重新提供对应内容。历史只有一段普通文本时，客户端不能可靠恢复原附件结构。

## 4. 工具输出对照

### 工具类别、状态和字段

| 范围 | 全部取值 / 字段 | 当前处理 |
| --- | --- | --- |
| `ToolKind` | `read`、`edit`、`delete`、`move`、`search`、`execute`、`think`、`fetch`、`switch_mode`、`other` | 各有图标/类别，使用同一工具卡及内容组件；缺省使用通用类别 |
| `ToolCallStatus` | `pending`、`in_progress`、`completed`、`failed` | 图标和状态文案；区分未提供输出、失败未给错误详情和仍在运行 |
| 本地取消显示 | `cancelled` | Client 派生状态，不是第五个 ACP ToolCallStatus；bridge 接受取消后标记当前回合未结束工具，继续接收最终结果 |
| 标识和标题 | `toolCallId`、`name`、`title` | 按 ID 合并；优先 Agent 标题，空标题才回退名称/类别；技术标识进入 Tool info |
| 输入 | `rawInput` | 通用结构化展示；不按工具名解释参数。权限卡的 null/缺省值沿用已合并输入 |
| 输出 | `content`、`rawOutput` | 有 content 时按顺序展示，额外 rawOutput 折叠；无 content 时用 rawOutput；保留 `0`、`false`、空字符串等有效标量 |
| 位置 | `locations[].path/line` | 工具信息中可检查；不重复制造结果文件卡；不新增编辑器跳转 |
| 更新 | `tool_call_update` 的可选字段 | 同 ID 更新，未提供/空值按 v1 规则保留；数组按新值替换；不重复追加整个工具卡 |
| 缺失初始事件 | 先收到 update 或权限请求 | 可恢复工具条目；缺失 start 的 update 有明确异常提示，后续数据仍可补齐 |

`find` 是 Agent 自己的工具名称，不是 ACP 的第 11 种工具类别。它可以报告 `kind: "search"`，结果也可能是普通文本或多个资源块；展示结构应跟随这些内容块。[Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)

### 三类 ToolCallContent

| 类型 | 当前展示 | 已检查的边界 |
| --- | --- | --- |
| `content` | 工具卡和权限区复用第 3 节内容块 | 顺序、附件行、空白文本、多个不同内容块 |
| `diff` | 路径、实际增删行、上下文；可进入只读变更汇总 | `oldText: null` 表示新文件；`newText: ""` 不擅自推断删除文件；大差异显示省略/近似提示 |
| `terminal` | 根据 `terminalId` 展示实时/历史输出快照 | 退出码、signal、空输出、truncated、句柄 release 后输出保留、不可恢复提示；工具状态与进程退出状态独立 |

实现入口：[tool-call.tsx](../web/src/components/acp/tool-call.tsx)、[file-diff.tsx](../web/src/components/acp/file-diff.tsx)、[state.ts](../web/src/lib/state.ts)。所有工具默认折叠，更新保留用户的展开选择。协议规定内容语义，不规定每个搜索结果必须有独立卡片边框。

## 5. 全部 SessionUpdate 与结束响应对照

| SessionUpdate | 当前页面落点 / 合并方式 | 结论 |
| --- | --- | --- |
| `user_message_chunk` | 用户消息；兼容的连续文本合并，保留多块顺序和消息边界 | 已实现 |
| `agent_message_chunk` | 回答内容，流式追加；保留 messageId 等边界信息 | 已实现 |
| `agent_thought_chunk` | 思考区域；与回答保持发生顺序；活动思考可自动展开，之后仍可手动查看 | 已实现 |
| `tool_call` | 工具卡，按 toolCallId 合并 | 已实现 |
| `tool_call_update` | 更新同一工具的标题、状态、输入和输出 | 已实现；取消时序见 D03 |
| `plan` | 活动回合未完成计划在输入区附近；完成或回合结束后归档到线程 | 已实现；完整替换，空列表清除；历史不会重新成为活动计划 |
| `available_commands_update` | slash 命令菜单的 name/description/input hint | 已实现；更新整个命令集合，以普通文本 prompt 调用 |
| `current_mode_update` | 当前 mode 选择值 | 已实现；可选 mode 的说明见 D05 |
| `config_option_update` | select / boolean 会话设置 | 已实现；完整替换，Agent 返回值权威；分组展示见 D04 |
| `session_info_update` | 会话标题、列表更新时间、排序元数据 | 已实现；区分缺省与显式清除；无法解析的时间仍可检查 |
| `usage_update` | 输入区上下文用量/容量及费用提示 | 已实现；未提供 cost 时保留此前累计费用 |
| `plan_update`（扩展） | 按 planId 更新 items / Markdown / 文件 URI 计划 | 已实现；文件型展示 URI，不新增编辑器或自动取文件 |
| `plan_removed`（扩展） | 标记对应计划已移除，保留条目位置 | 已实现；内部 ID 的显眼程度见第 7 节 |
| `compaction_update`（扩展） | 压缩过程卡，状态和错误说明 | 已实现；按 compactionId 更新，失败、取消、完成各有状态 |
| `compaction_summary_chunk`（扩展） | 同一压缩卡的流式摘要内容 | 已实现；活动时展开，结束后按规则折叠并保留用户选择 |

状态与历史入口：[state.ts](../web/src/lib/state.ts)、[session_presentation.rs](../src/session_presentation.rs)。展示入口：[conversation.tsx](../web/src/components/acp/conversation.tsx)、[plan.tsx](../web/src/components/acp/plan.tsx)、[compaction.tsx](../web/src/components/acp/compaction.tsx)。

| Prompt 结束/异常 | 当前展示 |
| --- | --- |
| `end_turn` | 回答结束文案 |
| `max_tokens` | 达到 token 上限 |
| `max_turn_requests` | 达到回合请求次数上限 |
| `refusal` | Agent 拒绝继续 |
| `cancelled` | 已取消；未结束工具使用本地取消显示 |
| response `usage`（扩展） | 总 token 数；提示信息中展示 input/output/thought/cache read/cache write |
| JSON-RPC / 连接错误 | 可读错误、结构化 code/data 详情；可恢复的失败 prompt 保留准确内容用于重试/编辑 |

结束原因已有对应文案，没有把工具失败、整个回合失败和用户取消合并成一个状态。历史是否带回结束原因及用量取决于可用数据，客户端不推造缺失的 Agent 历史响应。

## 6. 交互和非消息接口对照

| ACP 能力 | 页面落点 | 审查结果 |
| --- | --- | --- |
| `session/request_permission` | 输入区审批卡；工具条目同步更新 | 4 类 Agent 选项 `allow_once/allow_always/reject_once/reject_always`、取消、提交等待/失败均有处理；正文和已有输入已补齐 |
| Form elicitation | 表单、必填提示、字段说明、默认值、提交/拒绝/取消 | string/number/integer/boolean/单选/多选均有组件；浏览器及服务端校验；选项说明见 D06 |
| URL elicitation | Agent 名称、完整 URL、突出域名、用户点击新页面、后续完成状态 | 不预打开；接受打开与外部完成分开；会话及请求作用域保留；歧义 URL 提示见第 7 节 |
| `session/set_config_option` | 输入区设置条 | select/boolean；提交锁定与错误反馈；按返回值提交状态；group 名称见 D04 |
| `session/set_mode` | 没有 configOptions 时的旧版 mode 选择器 | 有 configOptions（包括空列表）时不重复显示 modes；菜单保留 mode 描述 |
| `initialize` / capabilities / Agent info | Agent 设置与连接信息 | 能力控制输入、设置和生命周期操作；原始初始化结果可检查；协议版本不匹配有错误反馈 |
| Agent `authenticate` / terminal auth / `logout` | Agent 认证区域、终端登录界面、登出动作 | 使用 Agent 声明的方法及说明；终端认证为本地 stdio 能力；不增加自有账号/凭据表单 |
| `session/new/list/load/resume/fork/close/delete` | 项目/会话列表、会话菜单、历史或缺失提示、关闭/删除确认 | 能力门控；历史尽力恢复；无历史不冒充空白新会话；fork 为已接入扩展 |
| `fs/read_text_file` / `fs/write_text_file` | 后端能力，Agent 可通过工具消息表达行为 | 本身不是额外消息内容块；没有为每次 RPC 新造 UI 卡或项目编辑器 |
| `terminal/create/output/wait_for_exit/kill/release` | 工具中的 terminal 内容以及生命周期状态 | 支持的本地能力；保留已释放终端的可用输出，缺失时明确说明 |
| ACP-transport MCP（扩展） | 连接/诊断详情 | 不将每条 MCP 中继包当作用户消息重复展示 |
| `$/cancel_request` | 请求取消/等待状态 | SDK/请求层处理；不额外生成聊天正文 |
| providers / NES / document | 不开放对应产品界面/能力 | 已明确的产品范围，不作为展示漏项；见 [兼容性矩阵](acp-coverage.md) |

Form/URL 能力是分别声明的。URL 流程已区分用户同意打开与外部流程完成，符合 ACP 的状态区分。[Elicitation](https://agentclientprotocol.com/protocol/v1/elicitation)

## 7. 展示取舍及协议建议

### 元数据的层级尚未完全一致

- “ACP 内容优先级”来自 `annotations.priority`，中文标签和徽标由 attyd 定义。协议没有要求把该字段作为正文旁的固定备注。
- 普通消息在内容下显示 annotations，工具把它们收进 Tool info；压缩卡仍在标题区显示 `compactionId`，已删除计划显示 `planId`，结束行同时显示翻译文案与原始 stopReason。
- 这些不构成字段丢失或协议错误。与当前简洁展示方向更一致的做法是将内部 ID/注解统一放到详情，保留正常阅读和决策需要的说明。

Annotations 是内容使用/展示提示，不是新的消息正文类型。[ACP Content](https://agentclientprotocol.com/protocol/v1/content)

### URL elicitation 尚无歧义地址提示

当前 [safeHttpUrl](../web/src/lib/safe-url.ts) 校验 HTTP(S) scheme；[ElicitationCard](../web/src/components/acp/elicitation.tsx) 展示规范化后的完整地址及域名。对 `xn--` 域名、userinfo 等可能混淆的地址，没有单独说明或标记。

这是对“突出域名并提示可疑/歧义 URL”的 SHOULD 建议尚未完整落实；已有的完整 URL、显式点击和独立页面流程仍然存在。可作为后续小项，不需要引入通用预览或额外浏览器组件。[Elicitation / URL security](https://agentclientprotocol.com/protocol/v1/elicitation#url-security)

## 8. 首次审查时的静态样式页覆盖

[style-showcase.tsx](../web/src/dev/style-showcase.tsx) 通过 fetch 读取 [style-session.json](../web/dev/style-session.json)，使用真实 reducer 和 Conversation；没有伪造 ACP 服务。这符合仅做样式验收的目的。

| 项目 | 当前样例 |
| --- | --- |
| ToolKind | **10 / 10** |
| ToolCallStatus | **2 / 4**：completed 13 个、failed 1 个；没有 pending/in_progress |
| SessionUpdate | **8 / 15**：user_message_chunk、agent_message_chunk、agent_thought_chunk、tool_call、plan、compaction_update、compaction_summary_chunk、usage_update |
| 工具更新过程 | 没有 tool_call_update 的分阶段样例 |
| `usage_update` | JSON 中存在，但页面只把 timeline/terminals 传给 Conversation，未挂载输入区用量组件，因此不可见 |
| 审批、表单、URL 流程 | 页面未挂载对应组件 |
| 配置、modes、slash commands | 页面未挂载对应控件 |
| 扩展计划 | 没有 items/file/markdown 三种 plan_update 及 plan_removed 样例 |
| stop reason / prompt usage / 连接与认证 / 历史缺失 | 这份静态样例不覆盖 |

建议继续用静态 JSON 扩展验收页，按场景挂载已有组件。尤其补齐：审批上下文、分组选择、带说明的表单选项、运行/取消/失败状态、窄屏长文本与文件行。无需为样式检查新增假 Agent、开发端口或后端流程。

本表统计的是样式页，不是整个项目的自动化测试覆盖率；上述不少行为已有其他测试。

## 9. 首次审查的验证与实施建议

### 本轮实际运行

| 检查 | 结果 |
| --- | --- |
| `npm run check` | 通过：三个 TypeScript 配置、39 个测试文件 / **484 项 Vitest**、生产前端构建 |
| `ATTYD_SKIP_WEB_BUILD=1 cargo test <filter>` | 下列 5 组共 **44 项 Rust 测试**通过 |
| `semantic::tests` | 18 项 |
| `session_presentation::tests` | 12 项 |
| `attachments::tests` | 3 项 |
| `server::attachment_tests` | 5 项 |
| `elicitation_validation::tests` | 6 项 |
| 真实 React 组件最小复现 | 分组同名选项、legacy mode 描述、content-only 权限请求、权限 rawInput:null、单选/多选 EnumOption 描述，均确认上述表现 |
| 文档校验 | `git diff --check` 通过；新报告本地文件链接及行尾空白检查通过 |

本轮没有运行新的跨 Agent 联调或完整浏览器截图验收。组件复现及通过的既有测试验证的是相应场景，不代表每个 Agent、浏览器或尚未协商的未来扩展均兼容。

### 建议实施顺序

1. 修复 D01/D02：让审批区使用完整且合并语义正确的工具上下文。
2. 修复 D03：补齐取消意图的 bridge 投影和页面状态，测试慢取消及迟到更新。
3. 修复 D04–D06：保留 Agent 给出的组名、mode 说明和表单选项说明。
4. 扩展静态样式页，覆盖上述交互与状态；再统一元数据的展示层级。

本轮同时校准了 [工具卡规则](tool-card-presentation.md)、[兼容性矩阵](acp-coverage.md) 和 [T06 附件决策](acp-difference-decisions.md)，移除将当前实现描述为“内嵌媒体预览、Blob 下载”的过时说明。

## 10. 补齐实现

| 编号 | 已实施行为 | 回归依据 |
| --- | --- | --- |
| D01 | 权限卡的 null/缺省输入回退到已合并的工具输入，保留有效标量 | `ui-interactions.test.tsx` 的权限上下文用例 |
| D02 | 审批区复用 ToolContentView，按顺序展示正文、diff、附件和终端；内容也计入可检查上下文 | content-only 权限、混合内容及终端快照用例；宽窄屏浏览器场景 |
| D03 | `session_view_value` 投影已有 `cancel_requested`；页面显示正在取消，未结束工具先显示本地取消；最终结果仍能更新工具 | 原生 bridge 先 Red 后 Green；浏览器 reducer 验证旧回合隔离、迟到结果及新回合复位；HTTP/WS 延迟结束验收 |
| D04 | 保留配置分组和组名，按原顺序展示；组名可搜索，当前选项同时显示所属组 | 同名分组选项的选择/搜索测试 |
| D05 | 旧版 mode 描述传入共用选择器 | legacy mode 描述测试 |
| D06 | 单选展示当前选项说明，多选展示每项说明；提交仍使用原始 value | 表单说明和原值提交测试 |

静态页增加“审批、设置与状态”入口，继续只 fetch JSON、复用产品组件，无 ACP 请求。新增内容包括两类权限上下文、表单和 URL 请求、分组/布尔设置、旧版 mode、上下文用量，以及四种工具状态和取消后的迟到结果。第 8 节记录的是补齐前的样例；本次未将该页面扩展为覆盖全部协议与认证生命周期的模拟器。

元数据层级与歧义 URL 提示仍为第 7 节的后续小项。

### 补齐后的验证

| 检查 | 实际结果 |
| --- | --- |
| `npm run check` | 通过：三个 TypeScript 配置、39 个测试文件 / **490 项 Vitest**、生产前端构建 |
| Rust 全量回归 | `ATTYD_SKIP_WEB_BUILD=1 cargo test --quiet -- --test-threads=1`：**598 / 598 通过** |
| `scripts/rust-remote-smoke.ts` | HTTP/SSE、WebSocket 均通过；取消被接受后、最终响应前可查询取消标记 |
| `scripts/ui-smoke.ts` | Rust 托管的 REST/SSE 集成检查通过 |
| 静态样式页浏览器检查 | 1280 / 360 像素宽度下的审批、分组选项、表单说明、取消与迟到结果通过；无横向溢出、浏览器错误或 `/api/` 请求 |
| 产品浏览器回归 | 5 项通过：审批及表单焦点恢复、取消后的计划归档、Send now 队列、Stop 队列、多文本块及附件的编辑/历史恢复 |
| 最后一次输入框修复 | 59 项组件交互测试、三个 TypeScript 配置、生产构建，以及审批/多文本块的 2 项浏览器回归再次通过 |
| 格式检查 | `cargo fmt --all --check`、`git diff --check` 通过 |

Rust 初次运行未将 Node 加入 PATH，修正环境后并发运行仅大消息传输用例超时；该用例单独重跑通过，随后串行全量 598 项通过，未放宽超时断言。浏览器回归另发现恢复草稿时可能先聚焦旧输入框，现改为在新文本块 DOM 提交后恢复焦点，并检查编辑和历史恢复后的焦点。

### 后续样式与 prompt 验收

用户与 Agent 名称增大字号；工具结果中的文件名、代码继承所在区域字号；轮次分隔使用独立颜色的加粗双线。配置菜单默认对齐触发器，并在窗口边缘限制位置。

用户 prompt 平铺渲染 Markdown，按实际内容高度与共用的九行上限判断折叠；末尾三行渐隐，右侧展开/收起。窗口宽度、正文或图片尺寸变化时重测。完整 Markdown 保留在渲染内容中，折叠区域之外的文字不计入会话搜索；键盘聚焦隐藏链接时展开。

| 检查 | 最终结果 |
| --- | --- |
| `npm run check` | 三个 TypeScript 配置、39 个测试文件 / **495 项 Vitest**、生产前端构建通过 |
| `prompt-folding.pw.ts` | 九行/十行边界、长源码短显示、标题/表格、窗口宽度变化、延迟图片及链接聚焦通过 |
| 产品浏览器回归 | **4 项通过**：prompt 展开/搜索/复用、会话搜索、桌面深浅主题、手机中文轮次与操作 |
| 开发实例 | `./bin/goose acp` 开发实例已验证热更新；短消息无折叠按钮和渐隐，长消息使用九行上限 |
| 格式检查 | `cargo fmt -- --check`、`git diff --check` 通过 |

这些结果覆盖当前固定 SDK 和测试场景，不代表已经完成所有 ACP Agent 的实机联调。
