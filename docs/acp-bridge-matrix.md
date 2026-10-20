# ACP → bridge 功能矩阵

审查日期：2026-10-17。范围是 **ACP 客户端执行能力及 bridge 内存状态**。
这是实现和验收依据，不表示下列目标已经实现；本轮只改文档。
FE 对内存的复制、校验和修复由项目自定义的
[简易 Agent 交互协议](protocol.md) 承载，仍是独立的下一层设计与验收。
bridge 与 Agent 之间继续使用标准 ACP。

结论：实验分支的主要 RPC 路径已经存在，但尚不能称为完整、忠实的 ACP
内存表示。完成轮次、加载回放、失败结果、交互生命周期、引用资源和元数据
都有待补齐的路径。仅证明 FE 与当前投影一致，不能发现这些损失。

## 1. 来源和版本

以 Rust SDK 的实际注册、schema 定义、反序列化规则和执行代码为依据；
网站文档只作辅助。源码注释规定的语义与实际代码行为不一致时，分别记录，
不把某个客户端的行为升级为协议规则。

| 对象 | 固定版本 / 提交 | 用途 |
| --- | --- | --- |
| attyd 实验实现 | [`991b0532`](https://github.com/tf4fun/attyd/tree/991b0532f8f2cc321f2955a1f250e283beac1ddb) | 当前执行与内存证据；r2 文档不是已完成的代码 |
| 稳定 v1 | [`v0.3.0-rc.5` / `60f8606e`](https://github.com/tf4fun/attyd/tree/60f8606e8fbda1b707fb9bfcdad48275c089b74d) | 功能来自 `488ccd9`，后续仅修 rustls；不是旧 v0.2.8 的推断 |
| 已锁 Rust SDK | [2.0.0 / `754d5aa1`](https://github.com/agentclientprotocol/rust-sdk/tree/754d5aa1ce2cfa54ba2c2a6d3edc7e7b6bce28eb) | bridge 当前实际客户端、JSON-RPC 和 HTTP 传输 |
| 已锁 Rust schema | [1.7.0 / `272bf799`](https://github.com/agentclientprotocol/agent-client-protocol/tree/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src) | 当前 ACP v1 的方法、字段、变体和能力 |
| 上游 SDK 对照 | [3.4.0 / `d5e8bf4d`](https://github.com/agentclientprotocol/rust-sdk/tree/d5e8bf4d17eafed81b2185acd4bd366e88a33dd1) | 2026-10-16 源码快照，单列升级差异 |
| 上游 schema 对照 | [1.12.0 / `e2d83349`](https://github.com/agentclientprotocol/agent-client-protocol/tree/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/agent-client-protocol-schema/src) | 与上游 SDK、Zed 的锁文件一致；不能混入 1.7 行为 |
| Zed | [`9f63850f`](https://github.com/zed-industries/zed/tree/9f63850f7affbd57772a4f99554d4fd333b50a6f) | SDK 3.4.0 / schema 1.12.0；追踪实际 v1 adapter 调用链，区分共享代码与 v2 专用调用 |

当前 Cargo 实际解析的 7 个 unstable feature 是 `end_turn_token_usage`、
`llm_providers`、`mcp_over_acp`、`plan_operations`、`session_compaction`、
`session_fork`、`tool_call_name`。`unstable_nes` 和
`unstable_protocol_v2` **没有启用**。这来自
`cargo tree --locked --offline -e features -i agent-client-protocol-schema`，
不能用 schema 自身的 `unstable` 聚合项代替实际依赖解析结果。
attyd 的 HTTP `/api/v2` 与 ACP 的实验性协议 v2 是两件事。

## 2. 怎样使用矩阵

| 分册 | 逐项覆盖 |
| --- | --- |
| [连接、传输与 SDK](acp-matrix/transport.md) | 协商、JSON-RPC、批次、stdio/HTTP/WebSocket、排序、响应屏障、取消、错误与回收 |
| [客户端请求与能力](acp-matrix/lifecycle.md) | 初始化、两类认证、会话方法、prompt/cancel、controls、MCP 配置、providers、NES/document 和版本边界 |
| [内容、通知与控制状态](acp-matrix/content.md) | 每个 SessionUpdate/ContentBlock、tool/plan/compaction、所有相关字段、稀疏归约、元数据与新版差异 |
| [交互与宿主服务](acp-matrix/services.md) | permission、完整 form/URL schema、父 RPC、fs、5 个 terminal 方法、MCP 双向消息、扩展与取消 |
| [源码闭合索引](acp-matrix/schema-index.md) | 类型、字段与变体归属；方法枚举和 SDK 注册的交叉核对；明确列出的排除面 |

每项分别回答：**schema 是否有、是否编译、是否协商、是否执行、是否保留、
怎样结束、怎样验收**。不能用其中一项代替另外几项。
“已有处理器”不代表内存完整；“没有 UI”也不代表 ACP handler 缺失。

状态含义：

- **已有路径**：静态代码存在；具体字段保留和测试覆盖仍看对应行。
- **重构回退**：与固定的 rc.5 行为比较后，确认实验实现丢失。
- **共有缺口/限制**：两边都有；不能因为 v1 也缺少就算完整支持。
- **边界**：明确未启用或未提供的可选能力；不能计入“全部支持”。
- **待定/待验证**：源码尚不足以证明，或目标表示规则没有定稿；不能标为通过。
- **新版差异**：1.12.0 相对 1.7.0 的变化；不是当前锁定版本的回退。

分册里的验收项是应具备的直接测试，不是本轮已运行的测试结果。
引用既有测试只证明那个具体断言，不能推广成整行覆盖。

## 3. 约束实现的基本规则

第一层的唯一契约在 [audit §9](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)：
[基本规则](acp-fidelity-audit.md#91-basic-rules)、
[可展示内存及生命周期](acp-fidelity-audit.md#92-displayable-memory)、
[直接验收](acp-fidelity-audit.md#94-direct-acceptance)。本矩阵提供逐字段源码依据，
不再另定义一份内存模型。现有所有权和实现约束见
[state-sync §7.1](state-sync.md#71-design)。

“保留 `_meta`”限于 SDK 暴露的 opaque 值及其来源层级；typed schema 丢弃的
未来字段不构成字节级透传保证。fs/MCP 的正确执行与可选诊断展示也不是同一项能力，
具体边界见 [services](acp-matrix/services.md) 和 [transport T19](acp-matrix/transport.md)。

## 4. 当前需要补齐的工作

这里只索引缺口和已有迁移步骤；完成条件在领域分册，顺序与依赖在
[state-sync §7.2](state-sync.md#72-steps)。步骤 17 随各项补直接测试证据。

| 工作 | 缺口 / 范围 | 领域与步骤 |
| --- | --- | --- |
| timeline、消息边界、结果与原附件 | D1–D7；回退和共有缺口分别列明 | [content](acp-matrix/content.md)；11–13 |
| 历史知识、fork 切面、零会话恢复 | D9；含待定边界 | [lifecycle](acp-matrix/lifecycle.md)；14，fork 依赖 13/15 |
| 引用终端与资源回收 | D8；跨连接保留是额外补齐项 | [services S06–S10](acp-matrix/services.md)；15 |
| 独立交互、真实 parent RPC、URL 结果、完整表单/ID 值域 | D10/D11；含 rc.5 共有缺口 | [services S01–S03/F01–F11](acp-matrix/services.md)；16 |
| controls、close 及迟到响应 | R1/R2、D12 | [content CT/CN](acp-matrix/content.md)、[lifecycle](acp-matrix/lifecycle.md)；18 |
| 通知/响应容器元数据 | 部分共有；来源层和承载位置需明确 | content 的通知归 11、controls 归 18；lifecycle 的 setup/catalog/auth/logout 结果归 14 |
| 创建前未知 session 的资源归属 | 不得猜父操作；先明确拒绝/暂存边界 | [lifecycle](acp-matrix/lifecycle.md)、[services](acp-matrix/services.md)；14/16 |
| 已有传输、认证、fs、MCP 与可选能力边界 | 路径存在不等于验收完整；新版差异不是当前义务 | 各分册；17，不扩大到 providers/NES/draft v2 |

参数来源也属于明确的产品选择：宿主配置可以提供 cwd、additionalDirectories 和
MCP 配置，catalog 可以选择不暴露 cwd 筛选；不能据此声称所有 schema 字段都有
任意 FE 透传入口。逐方法记录在 [lifecycle](acp-matrix/lifecycle.md)。

## 5. Zed 参考的适用范围

可参考它对普通请求答复、内容实体、工具结果、终端输出、controls 和 URL 流程的
处理；每项都要追踪到相应版本的调用者，不能只搜索到一个同名辅助函数。

- v1 adapter 创建的 `active_request_scopes` 是 `None`；共享 elicitation handler 的
  可选父 RPC 检查，以及 v2 调用的 `finish_request`，不能证明 Zed v1 已完整实现
  我们需要的父 RPC 生命周期。
- 当前 Zed 的 v1 MCP 配置路径与本项目锁定的连接式 MCP-over-ACP 不是相同能力。
  新版改为 Agent→Client 的 mcp/message 请求与 Client→Agent 的通知；SDK 仍有 typed 注册，不能仅凭某个枚举的变化判断整个能力存在或删除。
- Zed 自己包含厂商兼容、编辑器/远程项目服务和本地历史存储；这些均不自动成为
  attyd 需求。只采用标准 ACP 行为和适用的内存生命周期。
- Zed 将 stderr 与 ACP 内容流分开记录。这说明诊断面板可以独立存在，不证明
  ACP session 历史必须包含 stderr 或每条 MCP 消息。

对应源码位置和新版差异见各分册；不以“Zed 如此”替代 schema 与用户目标。

## 6. 验收和推进

按 [第一层直接验收](acp-fidelity-audit.md#94-direct-acceptance) 和
[state-sync §7.3](state-sync.md#73-gates) 证明 ACP 输入、权威内存、实际答复及清理。
[覆盖台账](acp-coverage.md) 只记录具体测试证据和剩余缺口；源码集合闭合不代替行为测试。

完成第一层后，再按 [简易 Agent 交互协议 §12](protocol.md#12-synchronization-design-requirements)
设计并验证 FE 同步、内容校验和修复。不能用副本等于当前投影来关闭第一层缺口，
也不能用现有同步形状反向裁剪 ACP 状态。所有实现仍在既有 11–18 步推进，
无持久层、厂商分支、请求历史账本或另一套状态机。
