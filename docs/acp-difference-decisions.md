# ACP 差异分类与处置

本文件承接 [2026-09-08 逐接口审查](acp-release-audit-2026-09-08.md)。原报告是固定提交的发现记录；本文件区分不再实现的架构范围、需要讨论的取舍，以及应直接修复的错误。A/Z 编号沿用原报告，避免把不同性质的差异混成一个缺陷清单。

审查修复基线：明确放弃 8 类架构不支持项，并已完成 attyd 的 19 组明确缺陷修复。后续讨论确认了全部 8 项权衡，本轮已按决策落实 T01–T04、T06、T08，保留 T05 的固定 shell 策略并补充 T07 的维护规则。自动关闭默认设为 1800 秒（30 分钟），可通过 CLI 调整。第五节保留此前缺陷修复的验收记录；本轮实施与验证见第六节。

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

## 二、权衡项：决策进度与实施边界

| 编号/对应审查 | ACP 允许什么 | Zed 当前实现 | attyd 审查时实现 | 决策状态与实施内容 |
| --- | --- | --- | --- | --- |
| T01 / A10 | 生成中可切 mode/config；close 可取消活动工作 | 控制请求不以 prompt idle 为前置，thread release 可 close | 同 session 的 prompt 与控制/lifecycle mutation 互斥 | 已实施：允许生成中提交设置；手动关闭先确认；离开页面不立即关闭，随后采用 T02 的无观察者倒计时。 |
| T02 / A18 | 不规定客户端累计历史内存预算 | 编辑器实体和资源生命周期与 Web bridge 不同，不能直接移植预算 | 明确选择完整保留当前物化会话，不按累计字节截断；关闭/释放时回收 | 已实施：无观察者持续达到 CLI 配置时长后自动关闭；返回观察取消计时。继续使用生命周期回收，不增加累计历史裁剪或持久化。 |
| T03 / A07、A08 的无历史部分 | resume 独立于 load，不返回旧历史；fork 能力也不隐含 load | resume 可继续而不补 load；固定外部 adapter 没有 fork | 公开冷恢复需要 load；resume/fork 仍有 load 能力硬前置 | 已实施：消息展示尽力恢复，优先 Agent 历史，其次内存缓存，再明确缺失；fork 同样处理。历史完整性不作为继续会话的前提。 |
| T04 / A21 | 有 configOptions 时 SHOULD 排他使用它，legacy modes 仍在 v1 | 存在 configOptions 就不用 legacy modes | 仅发现 mode 类 config 时隐藏 legacy modes | 已实施：遵循 ACP 建议及 Zed，提供 configOptions 就排他使用；未提供才回退 modes，显式空数组不触发回退。 |
| T05 / 终端执行 | 要求 create/output/wait/kill/release 语义，不规定 shell、PTY 或 detached 子进程政策 | 本地 Unix 固定 /bin/sh，Windows 优先 Git Bash；项目环境、PTY、交互 shell 标志，命令 stdin 重定向为空 | Unix 非交互 /bin/sh、pipes、字面 argv；正常退出不补杀后台组，运行中显式终止清理 | 已确认、保留现状：Unix 固定 /bin/sh，保持 Agent 执行入口稳定；保留非交互、管道输出、环境继承及已确认的子进程策略。 |
| T06 / 卡片与媒体 | 必要字段语义和内容必须正确；不规定布局、折叠、编辑器导航或所有二进制播放器 | 多种编辑器呈现方式；本版本 audio/混合媒体仍有降级 | 统一卡片、原始细节折叠、Web media；binary resource 暂为摘要 | 已实施：保留统一卡片；内嵌图片/音频复用预览，其他二进制资源增加附件下载；仅链接按链接展示，不增加专用文件预览器或持久化。 |
| T07 / transport 与 Draft | stdio 是 v1 基线；HTTP profile 与其他草案仍演进，允许自定义 transport | 外部客户端以 stdio 为主要路径，未接通若干 attyd 草案 | 已提供 pinned SDK HTTP/SSE、WS、MCP、ID plan/compaction | 已确认：以 ACP v1 为协议主线，保留三种传输及已实现扩展；固定 SDK/传输规则，草案单独标注，不新增实验开关。 |
| T08 / 文件写入执行策略 | 规范不要求原子替换或递归创建父目录 | buffer transaction，可能 format-on-save | 真实文件原地覆盖，父目录须存在，写入前允许取消 | 已实施：保留原地覆盖、不自动创建父目录；失败返回明确原因，写入已开始时说明可能部分生效，由 Agent 决定如何处理。 |

### T01 已确认：控制权与关闭副作用

状态：以下产品规则已落实到运行逻辑、关闭确认 UI 和回归测试。

- 允许生成中提交 mode/config 请求，以 Agent 响应及通知确认设置。实际生效时点由 Agent 控制，不承诺本轮推理立即采用新设置，也不自动取消或重发 prompt。
- 用户主动关闭会话时，先显示确认框，再调用已协商的 `session/close`；成功后清理本地会话资源。错误或结果不明时保留可恢复状态，如实显示结果，不声称 Agent 已停止或已经回滚所有副作用。
- 返回项目、切换会话、关闭页面或观察者断开遵守 T02 配置，默认延迟关闭；显式设为 0 则立即关闭。无观察者倒计时已替换原先“回合提交且无人观察后立即 close”的路径；失败分配的回滚清理、明确删除会话及宿主进程退出等生命周期清理保留各自语义。
- 即使 Agent 当前没有生成，仍可能存在运行中的终端或开发服务器，因此关闭确认不能只在 `running` 时出现。

[ACP close](https://agentclientprotocol.com/protocol/v1/session-setup#closing-active-sessions) 要求 Agent 取消进行中的工作并释放活动会话资源；[terminal/release](https://agentclientprotocol.com/protocol/v1/terminals#releasing-terminals) 会终止仍运行的命令。子进程的可承诺范围是：

| 资源归属 | 关闭的影响与边界 |
| --- | --- |
| attyd 仍持有的 ACP terminal | 关闭成功后释放；当前 Unix 实现会终止仍运行的命令及其进程组，组内子进程也会受影响，包括开发服务器。 |
| 已退出的启动命令留下的后台服务，或已脱离进程组的进程 | attyd 不保存永久进程树，也不在原命令退出后补杀旧进程组；不能承诺这些服务会随会话关闭。 |
| Agent 自行管理的本地或远程进程 | 由 Agent 执行关闭时的资源清理；客户端不能列举或保证终止其中所有后代进程。 |
| 本地会话缓存与终端输出 | 关闭会释放本地临时数据；之后可恢复的历史由 Agent 提供，不能承诺全部恢复。 |

确认框文案基准（界面沿用项目语言）：

> **关闭会话？**
>
> 关闭将请求 Agent 停止此会话的任务并释放资源。仍受管理的终端命令及其子进程可能被终止，包括开发服务器。
>
> 本地临时消息和终端输出将被清理，历史能否恢复取决于 Agent。已脱离管理的后台服务可能继续运行。

按钮为“保留会话”和“关闭会话”，默认焦点落在保留操作。取消不发送关闭请求；用户确认才开始关闭。该确认不是关闭所有历史子进程的保证，也不替代 Agent 自主执行 `terminal/release` 等正常资源管理。

### T02 已确认：无观察者自动关闭倒计时

采用一个由 CLI 控制的后端回收策略，支持禁用、立即关闭和倒计时关闭。CLI 是用户预先选择的自动回收策略；自动关闭不等待一个无人查看的确认框。手动关闭继续遵守 T01 的确认要求。

- 按 session 统计观察者；任一浏览器仍订阅该会话的事件流时不计时。项目列表和全局状态订阅不算该会话的观察者。
- 启用自动回收时，已物化的会话在最后一个观察者离开后开始计时或立即触发关闭；首次物化后始终无人观察的会话也适用。仅存在于 Agent 列表、尚未打开的历史会话不创建计时器。负数配置不启动自动回收。
- 倒计时内重新出现观察者，取消本次计时；之后再次变成零观察者时重新计算完整时长。
- 倒计时只取决于持续无观察者的时长，不因 Agent 输出或 prompt 完成而重置。到期后触发已协商的 `session/close`，因此可能终止仍进行中的任务及受管理进程；不能将其描述为只关闭完全没有后台工作的会话。
- 关闭前再次核对观察者数量和会话代次。旧计时器不能关闭已经重开的同 ID 会话；若关闭请求已经发送，返回的观察者看到真实的关闭/恢复状态，不假装已撤销 Agent 收到的关闭请求。
- 未声明 close 能力时不发送该接口；关闭失败或结果不明时不报告成功，不提前清空可恢复状态。加载事务与到期关闭的协调应纳入 T01 的生命周期实现。
- 不新增每会话保活开关、基于内存压力的 LRU 或历史截断。计时器只存在于当前进程内；无观察者回收不构成总内存或 CPU 的硬上限。

CLI 接口草案：`--session-unobserved-timeout <SECONDS>`，参数使用有符号整数秒数，取值语义已确认：

| 取值 | 自动回收行为 |
| --- | --- |
| `< 0` | 永不自动回收；所有负数含义相同，`-1` 只是常用写法。手动关闭仍然可用。 |
| `= 0` | 无观察者时立即触发自动关闭，不增加等待时间，保留当前无宽限期模式。 |
| `> 0` | 持续无观察者达到指定秒数后自动关闭；例如 `1800` 表示 30 分钟。 |

CLI 必须接受负数参数，不能将其误识别为另一个选项。自动关闭仍遵守前述能力协商、观察者及会话代次检查；负数仅关闭自动回收，不禁用其他正常资源清理。已提供该参数，默认 1800 秒；CLI 帮助说明任务停止、终端清理与历史恢复限制。默认时长是本轮实施选择，可按部署需要覆盖。

### 对原报告 A18 的更正

原审查将累计内存无预算列为实现缺陷，但 [运行态契约](active-turn-runtime.md#memory-model)及 [TDD ledger](bridge-state-machine-tdd.md#memory-invariants)已经明确选择不截断协议有效历史。因此本轮将它改为 **T02 架构资源权衡**。继续记账且关闭时回收；不擅自增加 LRU、历史裁剪、终端快照淘汰或持久化。文档中若把“单项有界”写成“累计内存有界”，则直接纠正文案。

### T03 已确认：消息上下文尽力恢复

沿用无持久化、尽力展示完整消息上下文的设计；不新增要求用户选择的“无历史模式”。这里的服务器历史指 ACP Agent 提供的历史，attyd 的服务端与浏览器缓存都只存在于内存中。

- 恢复展示时，优先使用 Agent 按协议提供的历史；无法取得时使用仍可用的内存缓存；缓存也没有或已知不完整时，明确提示此前消息丢失或无法恢复。消息缺失不代表 Agent 的推理上下文也已丢失。
- 已物化且仍有效的会话继续使用当前内存状态，不为切换页面反复加载历史。使用缓存时保留来源与完整性边界，不把部分消息标作完整恢复；Agent 成功返回的空历史也不能被当作加载失败，再填回旧缓存。
- 会话能否继续取决于 Agent 是否支持并成功完成相应操作，历史是否完整仅影响展示。按协商能力调用 load/resume/fork，不再给 resume/fork 附加必须支持 load 的限制；有缓存也不能冒充 Agent 已恢复会话。
- fork 使用相同的来源优先级。Agent 未提供分支历史时，可使用分支已有缓存或分支创建时保留的源会话缓存快照辅助展示，标明缓存来源；不伪称 Agent 已重放这些消息，不随源会话后续变化更新分支，也不把缓存重新发送给 Agent 来构造上下文。
- resume/fork 已成功而后续历史加载失败时，保留成功的会话及其 ID，降级到缓存或缺失提示；重试历史加载不重复执行 fork。
- 不增加持久化，不推测缺失消息，不承诺跨重启恢复客户端独有的终端输出等数据。

[bridge](../src/bridge.rs) 已移除 resume/fork 的 load 硬前置。冷会话优先 load，否则 resume；成功附加后历史获取失败保留会话，使用可用上下文并通过 `historyNotice` 明确来源或缺失。前端 fork 入口不再检查 load 能力。

### T04 已确认：配置入口遵循 ACP 与 Zed

采用 [ACP 的排他建议](https://agentclientprotocol.com/protocol/v1/session-config-options#relationship-to-session-modes)，与审查版本 Zed 的 `config_state` 一致。

- Agent 提供 `configOptions` 时，只展示这套配置，修改使用 `session/set_config_option`；未提供时才回退到旧 `modes` 和 `session/set_mode`。
- 显式 `configOptions: []` 表示当前没有新配置项，不自动补回旧模式。实现需保留“未提供”和“提供空数组”的区别，不能仅用数组长度判断。
- 即使新配置仅有 model 项，也不额外展示旧 modes；不根据配置 ID 或 category 猜测是否需要混用两套入口。
- 按 Agent 提供的名称、顺序和类型展示；缺少或未知 category 不影响操作。当前值与配置列表遵循 Agent 的响应和通知，生成中修改的边界沿用 T01。

这属于配置入口的兼容策略，与无持久化无关；SHOULD 级建议不应描述成 MUST 级缺陷。已在状态层保留缺失/空数组区别，并在配置组件与测试中落实。

### T05 已确认：保留固定 shell 与现有执行策略

Unix 普通 ACP 工具命令继续固定使用 `/bin/sh`，避免执行语法随用户默认 shell 改变。保留当前非交互执行、管道输出、字面参数传递、会话工作目录及继承宿主环境后应用请求 env 的行为；Agent 显式请求其他解释器时仍按其命令和参数执行。

本项不增加用户 shell 选择、PTY 或项目环境发现机制。正在运行的受管理命令按 kill/release 及会话清理语义终止；启动命令正常退出后，不补杀其留下的后台进程组。终端认证的独立 PTY 流程保留。本项确认现有策略，无需修改运行代码；T01/T02 的会话关闭调整已按各自决策实施。

#### Zed 参考实现的具体边界

补充核对同一审查提交后，原报告的“默认 shell”应具体解释为以下行为，而非直接复用用户的 `$SHELL`：

- 本地 Unix 的 `get_default_system_shell_preferring_bash` 固定返回 `/bin/sh`；“优先 Bash”仅针对 Windows，优先寻找 Git Bash，失败后使用 Windows 系统 shell。远程项目优先使用远端报告的默认 shell。[选择函数](https://github.com/zed-industries/zed/blob/ad51f6825c362d930c78a6579eecd21a06a7055d/crates/util/src/shell.rs#L87)
- ACP 执行路径取得项目目录环境，设置 `PAGER`/`GIT_PAGER`，再合入 Agent 指定的环境变量，通过项目终端任务使用 PTY。[调用链](https://github.com/zed-industries/zed/blob/ad51f6825c362d930c78a6579eecd21a06a7055d/crates/acp_thread/src/terminal.rs#L615)
- `ShellBuilder` 默认启用交互标志，ACP 路径未调用 `non_interactive`，因此本地 Unix 使用 `/bin/sh -i -c`；同时在命令中重定向 stdin 到 `/dev/null`。关闭命令输入不等于关闭 shell 的交互标志。[构建器](https://github.com/zed-industries/zed/blob/ad51f6825c362d930c78a6579eecd21a06a7055d/crates/util/src/shell_builder.rs#L18)、[参数生成](https://github.com/zed-industries/zed/blob/ad51f6825c362d930c78a6579eecd21a06a7055d/crates/util/src/shell.rs#L359)

attyd 与 Zed 的本地 Unix shell 路径一致，交互标志、PTY 和项目环境集成继续保留各自的产品选择；不将这些差异视为 ACP 缺陷。

### T06 已确认：统一卡片与附件展示

保留 [现有工具卡片规则](tool-card-presentation.md)，按协议内容类型选择呈现方式；`kind` 仅用于图标及类别，不按工具名或厂商字段猜测输出结构。

| 内容 | 已确认的展示策略 |
| --- | --- |
| 文本、diff、终端 | 保持现有展示，保留内容顺序、状态与截断/不可恢复提示。 |
| 图片、音频 | 保留图片预览和音频播放；同类型的内嵌资源复用相同展示。浏览器无法预览但文件数据仍可用时，提供附件下载兜底。 |
| 其他二进制资源 | 使用统一附件卡片，显示名称、类型与解码后的实际字节大小，提供下载；暂不增加 PDF、Office 等专用预览器。 |
| 仅有资源链接 | 展示名称和地址，可访问的链接由用户点击打开；没有文件内容时，不承诺可下载，也不为生成预览或下载入口自动获取链接内容。 |
| 原始协议信息 | 继续放入折叠详情，普通阅读不直接展示 JSON、Base64 或内部 ID。 |

卡片沿用 Agent 标题，不增加推测性的 Description，不重复打印文件名；协议资源本身提供的 description 仍按内容展示。沿用当前折叠交互，不因增加附件入口重做整套卡片。

附件数据来源遵循 T03：Agent 提供的内容优先，其次当前内存缓存，再明确缺失。用户主动下载属于导出，不建立客户端持久化缓存；数据已丢失时显示不可恢复，不保留仍可点击的下载入口。

本项已实施：普通与内嵌 image/audio 共用预览，其他 binary resource 展示附件卡片及解码大小；有有效数据时可主动下载。URL consent、取消状态及字段合并正确性继续属于必要行为，不因展示取舍降级。

### T07 已确认：ACP v1 主线与额外传输能力

ACP v1 是当前实现及发布的协议主线；HTTP/SSE 与 WebSocket 作为额外传输能力保留，不改变 ACP 消息、能力协商及生命周期语义。Goose 的 serve 接入继续作为 [可选后端示例](usage.md#optional-example-goose)，不成为协议兼容性的判断标准或必装依赖。

| 范围 | 发布与维护承诺 |
| --- | --- |
| ACP v1 稳定接口、stdio | 按规范及能力协商维护兼容性。 |
| HTTP/SSE、WebSocket | 继续支持当前锁定 SDK 实现的传输规则；区分“功能已实现”与“传输规范已稳定”，不承诺兼容所有草案版本。 |
| 已实现的草案能力 | 保留 fork、MCP-over-ACP、计划操作、压缩等扩展，按各自协议要求协商，在兼容性文档标注实验性及版本依据；已确认的 T03 等行为调整另行实施。 |
| 新草案、ACP v2 | 逐项评估，不因 SDK 新增类型就自动纳入发布承诺；已明确排除的架构能力保持原边界。 |

- 不增加一套实验功能开关，已有能力按协商结果提供。
- SDK 升级时审查协议差异，运行三种传输的相关回归，并更新兼容性说明。当前 Rust SDK 固定到 `754d5aa1ce2cfa54ba2c2a6d3edc7e7b6bce28eb`，schema 为 `1.7.0`，测试对端 TypeScript SDK 为 `1.4.0`。
- 草案字段或传输规则存在版本差异时，说明支持范围，不通过猜字段或厂商特判掩盖差异。已声明功能的错误继续修复，实验性仅表示协议仍可能变化。
- README 保持简洁，具体版本、支持矩阵及草案边界放在兼容性文档。

本项确定发布与维护规则，不调整当前功能范围，也不将 Zed 当前已接入的能力作为 attyd 的支持上限。维护规则已同步到兼容性文档；本项未升级依赖或扩大协议范围。

### T08 已确认：原地写入与明确错误

保留现有文件执行策略：按 Agent 提供的完整文本原地覆盖，文件不存在时创建，父目录须已存在。不自动格式化、补换行、合并外部编辑或创建父目录；不引入原子替换机制。

- 目录缺失、权限不足、磁盘空间不足及其他可捕获的文件操作错误，应通过 ACP 错误响应明确告知 Agent。沿用适用的协议错误码，并提供目标路径、失败阶段及系统错误原因，不只返回笼统的 Internal error。
- 区分写入前拒绝与写入开始后的失败。文件已经打开截断、写入或刷新时发生错误，应明确内容可能已部分改变；不声称已回滚或原文件完好，也不报告写入成功。
- 后续创建目录、重试、重新读取或修复由 Agent 决定。客户端不在失败后自动重试写入、创建目录或执行补偿操作。
- 写入前接受取消；一旦进入文件变更阶段，不因取消主动中断写入，完成后按实际结果响应。进程退出或连接中断导致未收到响应时，结果属于未知，不能当作文件未变更的证明。
- 保留只读模式、会话工作路径与已授权根目录的边界检查。无持久化约束针对聊天恢复数据，不影响执行已授权的工作文件写入。

[filesystem](../src/filesystem.rs) 的写入错误已补齐路径、失败阶段、系统原因与 `contentMayHaveChanged`。已有取消和根目录边界保持不变，新增缺失父目录与写入后失败信息回归。

## 三、缺陷项：直接修复

下表的 19 组明确缺陷均已完成修复，回归入口和完整验证见第四、五节。A07/A08 的缺陷修复基线仅处理当时公开流程；本轮 T03 另行完成无历史 resume/fork 的尽力恢复方案。

| 原编号 | 明确错误 | 修复边界 |
| --- | --- | --- |
| A01 | 会话 cwd 正确但客户端 fs/terminal 仍用启动 cwd | 使用 session 主根/额外根，保持 incarnation 与越界保护；同根因的 `@` 文件搜索/读取也应使用所选 session。 |
| A02 | 已存活会话的 turn 外更新静默丢弃，工具 ID 限于本 turn | 保留 session 级实体与合法 idle 更新；继续隔离旧 incarnation、已关闭会话和失败的加载事务。 |
| A03 | 不同 messageId 的消息被历史折叠合并 | 保留消息边界；不同 annotations 也不能因文本合并而消失。 |
| A04 | rawInput/rawOutput 错误 deep merge | 只替换提供的整字段，省略保留；content/locations 按集合语义处理。 |
| A05 | 本地 prompt 与 Agent 单块/多块回显重复 | 按发送/回显边界匹配；保留用户正常的重复提问。 |
| A06 | 稀疏 info/usage 快照丢 title/cost | 按字段保留省略值、处理显式 null，确保快照与通知路径一致。 |
| A07 | fork/resume 成功后发送未协商 load | 基线修复避免发送未协商 load；本轮 T03 移除能力硬前置，保留协商检查并允许历史降级。 |
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

## 五、缺陷修复基线验收（29d90b5）

- `npm run check`：通过，239 项 Vitest、三个 TypeScript 配置检查及生产前端构建通过。
- `npm run test:coverage`：完整通过，包含 285 项 Rust 测试、REST/SSE UI、HTTP/SSE 与 WebSocket 远程传输、服务边界、ACP SDK 协议 smoke，以及 57 项 Chromium 测试。
- Rust 行覆盖率 **89.81%**，超过仓库要求的 **85%**；未降低门槛。
- 最终 fixture 的 TypeScript 检查、`cargo fmt --check`、`git diff --check` 均通过。

完整 Chromium 回归保持严格的阅读位置要求：离底 24px/500px 发送、流式上滚和历史搜索后的发送均检查 scrollTop 与原文锚点；位于底部时继续跟随新增内容。缺失会话回主页和文件修改留在原回合的断言也全部通过。

本轮验证使用仓库 fixture、官方 SDK 对端及临时本地实例。Zed 仍是固定源码版本的静态参考；这些基线结果不替代本轮权衡实施的验证，也不扩大明确排除的架构边界。

## 六、权衡实施与验收

- T01/T02：[runtime](../src/runtime_state.rs)、[bridge](../src/bridge.rs)、[server](../src/server.rs) 支持运行中设置/关闭、观察者倒计时及代次隔离；[关闭确认框](../web/src/components/acp/close-session-dialog.tsx) 默认聚焦保留会话，取消不发送请求。
- T03/T04：[mirror](../src/session_mirror.rs) 保留历史降级来源；Agent 空回放仍是权威结果。UI 保留缺失配置与显式空列表的区别。
- T06/T08：[内容组件](../web/src/components/acp/content-block.tsx) 统一附件预览/导出；[文件服务](../src/filesystem.rs) 提供可诊断的写入失败信息。
- T05/T07：保留固定 `/bin/sh`、stdio/HTTP/SSE/WebSocket 及已协商草案能力；[使用说明](usage.md) 和 [兼容性矩阵](acp-coverage.md) 明确实现与维护范围。

本轮验收全部通过：

- `npm run check`：244 项 Vitest、三个 TypeScript 配置检查、生产前端构建通过；最终 UI 调整后额外复核 TypeScript。
- `npm run test:coverage`：297 项 Rust 测试、REST/SSE、HTTP/SSE 与 WebSocket、服务边界、ACP SDK 协议 smoke，以及 58 项 Chromium 测试通过。
- Rust 行覆盖率 **90.59%**，高于 **85%** 门槛；`cargo fmt --check` 和 `git diff --check` 通过。
- 浏览器继续验证离底 24px/500px 发送保持原阅读高度、滚动后发送、底部新增内容跟随、文件修改归属，以及缺失会话回主页。新增运行中设置、关闭确认默认焦点、Escape 不发请求等行为测试。

测试使用独立临时实例，没有重启用户正在使用的实例。全部聊天恢复数据仍仅在内存中；没有增加持久化。
