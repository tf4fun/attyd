# 源码闭合索引

[总矩阵](../acp-bridge-matrix.md) · [结构化字段与版本对照](source-inventory.json)

此索引检查源码集合有没有漏项；不把类型存在、已编译或已有 handler 等同于行为已验收。
固定源码、版本和实际 Cargo feature 以总矩阵为准。

## 1. 集合核对

- schema 1.7.0 的 `src/v1` 共 **277** 个公开 struct/enum，全部有功能分册归属。
- 公共 RPC、ProtocolVersion、MaybeUndefined 另有 **9** 个类型，共 **286** 个。
- 三个领域分册的类型集合有交叉；合并后没有未归属声明。另从源文件独立提取字段名和枚举分支，逐一对照分册清单，差集为空。
- 源码索引记录 **718** 个 struct 命名字段、**238** 个 enum 分支；enum 的内嵌字段、tuple/newtype payload、cfg/serde 属性另行记录。
- 1.12.0 对照含 **240** 个 v1 声明：224 个顶层 + 16 个 `default_on_null!` 宏内声明。相对 1.7 新增 25、移除 62（57 个 NES 类型、5 个旧 MCP 连接类型）。宏包装的原类型仍在，不能误算删除。
- 每个文件的 SHA-256 在 JSON 的 `source_files` 中；每个类型有固定提交的源码链接。

| v1 源文件 | 类型数 |
| --- | --- |
| `v1/agent.rs` | 85 |
| `v1/client.rs` | 51 |
| `v1/content.rs` | 11 |
| `v1/elicitation.rs` | 33 |
| `v1/error.rs` | 2 |
| `v1/ext.rs` | 3 |
| `v1/mcp.rs` | 8 |
| `v1/mod.rs` | 1 |
| `v1/nes.rs` | 57 |
| `v1/plan.rs` | 12 |
| `v1/protocol_level.rs` | 3 |
| `v1/tool_call.rs` | 11 |

## 2. 方法及内容枚举

“含扩展”指通用 `_` 扩展容器，不是所有扩展都实现。request 与同名 notification、两个方向的同名方法分别计数。response 枚举只作返回类型闭合，不增加方法。

| 源集合 | 源码分支数 | 当前范围 |
| --- | --- | --- |
| ClientRequest / AgentResponse | 21 / 21 | 18 编译，另 3 NES 不编译；实际调用 14 个具体请求方法，3 providers 无 caller，通用扩展无出站入口 |
| ClientNotification | 10 | 3 编译：session/cancel、mcp/message、扩展容器；另 7 NES/document 不编译；实际调用前两个 |
| AgentRequest / ClientResponse | 13 / 13 | 12 个具体服务均注册且有 bridge handler，另通用扩展 fallback |
| AgentNotification | 4 | session/update、elicitation/complete、mcp/message、扩展 fallback |
| ProtocolLevelNotification | 1 | $/cancel_request；独立于业务取消，双向共用 |
| SessionUpdate | 15 | 全部在当前 feature 组合中编译；内容、controls 与生命周期各看相应矩阵 |
| ContentBlock | 5 | text、image、audio、resource_link、resource；prompt 的可用性还受协商能力约束 |

按 schema 聚合枚举、SDK typed registrations/enum registrations、bridge dispatch/call sites 三层核对。
尤其 MCP 的 SDK 注册在独立模块，不能仅查某个 enum 文件。最新 1.12 的方向变化另见 services §8。

## 3. 不属于上述声明计数的面

| 源面 | 归属与处理 |
| --- | --- |
| `v1::Meta` | `v1/ext.rs:14` 的 JSON object alias；opaque 保留，不解释厂商键。 |
| `v1::Response<Result>` | `v1/mod.rs:35` 的公共 RPC Response + v1 Error alias；没有额外 wire 功能。 |
| `RawValue` / JSON Value / 标量 / Vec / Option / Arc | 外部类型或组合容器；字段定义已经引用。RawValue 不保证当前发布 JSON 字节级等同原输入。 |
| `IntoOption` / `IntoMaybeUndefined` / builder constructors | Rust 构造辅助；不增加方法。builder 默认值不一定意味着 wire 字段可省略。 |
| `DefaultOnError` / `VecSkipError` / `SkipListener` / serde 属性 | [transport T19](transport.md)、各字段和内容分册：明确 typed ingress 的容错/丢弃边界；非新增客户端服务。 |
| MethodNames structs / 常量 | 已含在 277 类型中；方法枚举与 SDK 注册复核其取值，不额外累加功能数。 |
| SDK Client/Agent/Proxy/Conductor、session/MCP helpers、动态 handler | [lifecycle 的 SDK 边界](lifecycle.md)、[transport T18](transport.md)；普通 client 的执行工具，不要求实现 Agent 或代理链。 |
| `_proxy/initialize` / `_proxy/successor` / symposium metadata | SDK 自定义扩展；明确列出但不是通用 ACP 客户端必需行为，遵循扩展 fallback。 |
| 未启用 ACP draft v2 | [lifecycle 的完整注册集合](lifecycle.md)；与当前 ACP v1 分开，不把删除 v1 方法的 draft 套到当前产品。 |
| SDK tests、examples、derive、HTTP server、wasm/平台适配 | 用于实现/验证和构建，不新增 client wire 能力。实际部署的 stdio/WS/HTTP client 行为在 T04–T09；本项目不承担 ACP server 实现。 |

## 4. 每个类型的功能归属

字段和分支的完整属性见 JSON；这里列简表。同一类型可用于多个功能，所以映射不是一对一。
L 列的名称对应 lifecycle 中同名请求/能力；S/F/CN/CB/TC/PL/CT/EN/T 是相应分册的行 ID。

### rpc.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [EmptyJsonRpcBatch](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L166) :166 | pub struct EmptyJsonRpcBatch; | [T03](transport.md) |
| [JsonRpcBatch](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L179) :179 | pub struct JsonRpcBatch<M>( #[cfg_attr(feature = "schemars", schemars(length(min = 1)))] Vec<JsonRpcMessage<M>>, ); | [T03](transport.md) |
| [JsonRpcMessage](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L133) :133 | jsonrpc, message (flatten) | [T02](transport.md) |
| [Notification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L111) :111 | method, params | [T02](transport.md) |
| [Request](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L50) :50 | id, method, params | [T02](transport.md) |
| [RequestId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L31) :31 | Null, Number, Str | [F08, F10, F11, S02, S17](services.md); [T02, T12](transport.md) |
| [Response](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/rpc.rs#L68) :68 | Result, Error | [T02, T14](transport.md) |

### serde_util.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [MaybeUndefined](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/serde_util.rs#L313) :313 | Undefined, Null, Value | [T19](transport.md) |

### v1/agent.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [AgentAuthCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L464) :464 | logout, _meta | [InitializeRequest](lifecycle.md) |
| [AgentCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3789) :3789 | loadSession, promptCapabilities, mcpCapabilities, sessionCapabilities, auth, providers, nes, positionEncoding, _meta | [InitializeRequest](lifecycle.md) |
| [AgentMethodNames](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4607) :4607 | initialize, authenticate, providers_list, providers_set, providers_disable, session_new, session_load, session_set_mode, session_set_config_option, session_prompt, session_cancel, mcp_message, session_list, session_delete, session_fork, session_resume, session_close, logout, nes_start, nes_suggest, nes_accept, nes_reject, nes_close, document_did_open, document_did_change, document_did_close, document_did_save, document_did_focus | [composite.AgentMethodNames](lifecycle.md) |
| [AgentResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L5007) :5007 | InitializeResponse, AuthenticateResponse, ListProvidersResponse, SetProviderResponse, DisableProviderResponse, LogoutResponse, NewSessionResponse, LoadSessionResponse, ListSessionsResponse, DeleteSessionResponse, ForkSessionResponse, ResumeSessionResponse, CloseSessionResponse, SetSessionModeResponse, SetSessionConfigOptionResponse, PromptResponse, StartNesResponse, SuggestNesResponse, CloseNesResponse, ExtMethodResponse, MessageMcpResponse | [composite.AgentResponse](lifecycle.md) |
| [AuthMethod](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L575) :575 | Terminal, Agent | [InitializeRequest](lifecycle.md) |
| [AuthMethodAgent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L637) :637 | id, name, description, _meta | [InitializeRequest](lifecycle.md) |
| [AuthMethodId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L557) :557 | pub struct AuthMethodId(pub Arc<str>); | [InitializeRequest, AuthenticateRequest](lifecycle.md) |
| [AuthMethodTerminal](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L703) :703 | id, name, description, args, env, _meta | [InitializeRequest](lifecycle.md) |
| [AuthenticateRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L295) :295 | methodId, _meta | [AuthenticateRequest](lifecycle.md) |
| [AuthenticateResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L341) :341 | _meta | [AuthenticateRequest](lifecycle.md) |
| [CancelNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L5173) :5173 | sessionId, _meta | [CancelNotification](lifecycle.md) |
| [ClientNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L5072) :5072 | CancelNotification, DidOpenDocumentNotification, DidChangeDocumentNotification, DidCloseDocumentNotification, DidSaveDocumentNotification, DidFocusDocumentNotification, AcceptNesNotification, RejectNesNotification, MessageMcpNotification, ExtNotification | [composite.ClientNotification](lifecycle.md) |
| [ClientRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4779) :4779 | InitializeRequest, AuthenticateRequest, ListProvidersRequest, SetProviderRequest, DisableProviderRequest, LogoutRequest, NewSessionRequest, LoadSessionRequest, ListSessionsRequest, DeleteSessionRequest, ForkSessionRequest, ResumeSessionRequest, CloseSessionRequest, SetSessionModeRequest, SetSessionConfigOptionRequest, PromptRequest, StartNesRequest, SuggestNesRequest, CloseNesRequest, MessageMcpRequest, ExtMethodRequest | [composite.ClientRequest](lifecycle.md) |
| [CloseSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1447) :1447 | sessionId, _meta | [CloseSessionRequest](lifecycle.md) |
| [CloseSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1492) :1492 | _meta | [CloseSessionRequest](lifecycle.md) |
| [DeleteSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1661) :1661 | sessionId, _meta | [DeleteSessionRequest](lifecycle.md) |
| [DeleteSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1706) :1706 | _meta | [DeleteSessionRequest](lifecycle.md) |
| [DisableProviderRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3691) :3691 | providerId, _meta | [DisableProviderRequest](lifecycle.md) |
| [DisableProviderResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3742) :3742 | _meta | [DisableProviderRequest](lifecycle.md) |
| [EnvVariable](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2933) :2933 | name, value, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md); [S06](services.md) |
| [ForkSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1119) :1119 | sessionId, cwd, additionalDirectories, mcpServers, _meta | [ForkSessionRequest](lifecycle.md) |
| [ForkSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1203) :1203 | sessionId, modes, configOptions, _meta | [ForkSessionRequest](lifecycle.md) |
| [HttpHeader](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2980) :2980 | name, value, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md) |
| [Implementation](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L222) :222 | name, title, version, _meta | [InitializeRequest](lifecycle.md) |
| [InitializeRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L55) :55 | protocolVersion, clientCapabilities, clientInfo, _meta | [InitializeRequest](lifecycle.md) |
| [InitializeResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L132) :132 | protocolVersion, agentCapabilities, authMethods, agentInfo, _meta | [InitializeRequest](lifecycle.md) |
| [ListProvidersRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3472) :3472 | _meta | [ListProvidersRequest](lifecycle.md) |
| [ListProvidersResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3518) :3518 | providers, _meta | [ListProvidersRequest](lifecycle.md) |
| [ListSessionsRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1536) :1536 | cwd, cursor, _meta | [ListSessionsRequest](lifecycle.md) |
| [ListSessionsResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1596) :1596 | sessions, nextCursor, _meta | [ListSessionsRequest](lifecycle.md) |
| [LlmProtocol](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3298) :3298 | Anthropic, OpenAi, Azure, Vertex, Bedrock, Other | [ListProvidersRequest, SetProviderRequest](lifecycle.md) |
| [LoadSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L958) :958 | mcpServers, cwd, additionalDirectories, sessionId, _meta | [LoadSessionRequest](lifecycle.md) |
| [LoadSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1036) :1036 | modes, configOptions, _meta | [LoadSessionRequest](lifecycle.md) |
| [LogoutCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L519) :519 | _meta | [InitializeRequest](lifecycle.md) |
| [LogoutRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L385) :385 | _meta | [LogoutRequest](lifecycle.md) |
| [LogoutResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L425) :425 | _meta | [LogoutRequest](lifecycle.md) |
| [McpCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4522) :4522 | http, sse, acp, _meta | [InitializeRequest](lifecycle.md); [CAP07](services.md) |
| [McpServer](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2634) :2634 | Http, Sse, Acp, Stdio | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md) |
| [McpServerAcp](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2815) :2815 | name, serverId, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md); [CAP07](services.md) |
| [McpServerAcpId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2789) :2789 | pub struct McpServerAcpId(pub Arc<str>); | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md); [CAP07, S11](services.md) |
| [McpServerHttp](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2667) :2667 | name, url, headers, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md) |
| [McpServerSse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2724) :2724 | name, url, headers, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md) |
| [McpServerStdio](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2866) :2866 | name, command, args, env, _meta | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md) |
| [NewSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L796) :796 | cwd, additionalDirectories, mcpServers, _meta | [NewSessionRequest](lifecycle.md) |
| [NewSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L872) :872 | sessionId, modes, configOptions, _meta | [NewSessionRequest](lifecycle.md) |
| [PromptCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4441) :4441 | image, audio, embeddedContext, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CT-03](content.md) |
| [PromptRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3034) :3034 | sessionId, prompt, _meta | [PromptRequest](lifecycle.md) |
| [PromptResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3096) :3096 | stopReason, usage, _meta | [PromptRequest](lifecycle.md); [EN-01](content.md) |
| [ProviderCurrentConfig](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3327) :3327 | apiType, baseUrl, _meta | [ListProvidersRequest](lifecycle.md) |
| [ProviderId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3379) :3379 | pub struct ProviderId(pub Arc<str>); | [ListProvidersRequest, SetProviderRequest, DisableProviderRequest](lifecycle.md) |
| [ProviderInfo](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3402) :3402 | providerId, supported, required, current, _meta | [ListProvidersRequest](lifecycle.md) |
| [ProvidersCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3967) :3967 | _meta | [InitializeRequest](lifecycle.md) |
| [ResumeSessionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1289) :1289 | sessionId, cwd, additionalDirectories, mcpServers, _meta | [ResumeSessionRequest](lifecycle.md) |
| [ResumeSessionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1368) :1368 | modes, configOptions, _meta | [ResumeSessionRequest](lifecycle.md) |
| [SessionAdditionalDirectoriesCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4265) :4265 | _meta | [InitializeRequest](lifecycle.md) |
| [SessionCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4015) :4015 | list, delete, additionalDirectories, fork, resume, close, _meta | [InitializeRequest](lifecycle.md) |
| [SessionCloseCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4391) :4391 | _meta | [InitializeRequest](lifecycle.md) |
| [SessionConfigBoolean](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2268) :2268 | currentValue | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigGroupId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2085) :2085 | pub struct SessionConfigGroupId(pub Arc<str>); | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2053) :2053 | pub struct SessionConfigId(pub Arc<str>); | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2314) :2314 | Select, Boolean | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigOption](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2328) :2328 | id, name, description, category, kind (flatten), _meta | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CN-11, CT-01](content.md) |
| [SessionConfigOptionCategory](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2294) :2294 | Mode, Model, ModelConfig, ThoughtLevel, Other | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigOptionValue](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2445) :2445 | Boolean, ValueId | [SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigSelect](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2241) :2241 | currentValue, options | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigSelectGroup](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2162) :2162 | group, name, options, _meta | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigSelectOption](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2102) :2102 | value, name, description, _meta | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigSelectOptions](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2216) :2216 | Ungrouped, Grouped | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionConfigValueId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2069) :2069 | pub struct SessionConfigValueId(pub Arc<str>); | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionConfigOptionRequest](lifecycle.md); [CT-01](content.md) |
| [SessionDeleteCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4222) :4222 | _meta | [InitializeRequest](lifecycle.md) |
| [SessionForkCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4310) :4310 | _meta | [InitializeRequest](lifecycle.md) |
| [SessionInfo](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1745) :1745 | sessionId, cwd, additionalDirectories, title, updatedAt, _meta | [InitializeRequest, ListSessionsRequest](lifecycle.md); [CN-12](content.md) |
| [SessionListCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4182) :4182 | _meta | [InitializeRequest](lifecycle.md) |
| [SessionMode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1892) :1892 | id, name, description, _meta | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md); [CN-10, CT-02](content.md) |
| [SessionModeId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1951) :1951 | pub struct SessionModeId(pub Arc<str>); | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, SetSessionModeRequest](lifecycle.md); [CN-10, CT-02](content.md) |
| [SessionModeState](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1838) :1838 | currentModeId, availableModes, _meta | [NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest](lifecycle.md); [CN-10, CT-02](content.md) |
| [SessionResumeCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L4351) :4351 | _meta | [InitializeRequest](lifecycle.md) |
| [SetProviderRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3571) :3571 | providerId, apiType, baseUrl, headers, _meta | [SetProviderRequest](lifecycle.md) |
| [SetProviderResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3645) :3645 | _meta | [SetProviderRequest](lifecycle.md) |
| [SetSessionConfigOptionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2524) :2524 | sessionId, configId, value (flatten), _meta | [SetSessionConfigOptionRequest](lifecycle.md); [CT-02](content.md) |
| [SetSessionConfigOptionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2583) :2583 | configOptions, _meta | [SetSessionConfigOptionRequest](lifecycle.md); [CT-02](content.md) |
| [SetSessionModeRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L1969) :1969 | sessionId, modeId, _meta | [SetSessionModeRequest](lifecycle.md); [CT-02](content.md) |
| [SetSessionModeResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L2013) :2013 | _meta | [SetSessionModeRequest](lifecycle.md); [CT-02](content.md) |
| [StopReason](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3164) :3164 | EndTurn, MaxTokens, MaxTurnRequests, Refusal, Cancelled | [PromptRequest](lifecycle.md); [EN-01](content.md) |
| [Usage](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/agent.rs#L3197) :3197 | totalTokens, inputTokens, outputTokens, thoughtTokens, cachedReadTokens, cachedWriteTokens, _meta | [PromptRequest](lifecycle.md); [EN-01](content.md) |

### v1/client.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [AgentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2761) :2761 | SessionNotification, CompleteElicitationNotification, MessageMcpNotification, ExtNotification | [S03, S13, S16](services.md); [CN-00](content.md) |
| [AgentRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2568) :2568 | WriteTextFileRequest, ReadTextFileRequest, RequestPermissionRequest, CreateTerminalRequest, TerminalOutputRequest, ReleaseTerminalRequest, WaitForTerminalExitRequest, KillTerminalRequest, CreateElicitationRequest, ConnectMcpRequest, MessageMcpRequest, DisconnectMcpRequest, ExtMethodRequest | [S01, S02, S04, S05, S06, 另 8 项](services.md) |
| [AuthCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2353) :2353 | terminal, _meta | [InitializeRequest](lifecycle.md); [CAP03](services.md) |
| [AvailableCommand](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L742) :742 | name, description, input, _meta | [CN-09](content.md) |
| [AvailableCommandInput](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L800) :800 | Unstructured | [CN-09](content.md) |
| [AvailableCommandsUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L696) :696 | availableCommands, _meta | [CN-09](content.md) |
| [BooleanConfigOptionCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2310) :2310 | _meta | [InitializeRequest](lifecycle.md); [CAP05](services.md); [CT-03](content.md) |
| [ClientCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1963) :1963 | fs, terminal, session, plan, auth, elicitation, nes, positionEncodings, _meta | [InitializeRequest](lifecycle.md); [CAP01, CAP02, CAP03, CAP04, CAP05, 另 1 项](services.md) |
| [ClientMethodNames](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2478) :2478 | session_request_permission, session_update, fs_write_text_file, fs_read_text_file, terminal_create, terminal_output, terminal_release, terminal_wait_for_exit, terminal_kill, mcp_connect, mcp_message, mcp_disconnect, elicitation_create, elicitation_complete | [S01, S02, S04, S05, S06, 另 8 项](services.md) |
| [ClientResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2717) :2717 | WriteTextFileResponse, ReadTextFileResponse, RequestPermissionResponse, CreateTerminalResponse, TerminalOutputResponse, ReleaseTerminalResponse, WaitForTerminalExitResponse, KillTerminalResponse, CreateElicitationResponse, ConnectMcpResponse, DisconnectMcpResponse, MessageMcpResponse, ExtMethodResponse | [S01, S02, S04, S05, S06, 另 8 项](services.md) |
| [ClientSessionCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2148) :2148 | compaction, configOptions, _meta | [InitializeRequest](lifecycle.md); [CAP05](services.md); [CT-03](content.md) |
| [CompactionCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2230) :2230 | unit | [InitializeRequest](lifecycle.md); [CT-03](content.md) |
| [CompactionId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L172) :172 | pub struct CompactionId(pub Arc<str>); | [CN-14, CN-15](content.md) |
| [CompactionStatus](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L193) :193 | InProgress, Completed, Failed, Cancelled, Other | [CN-14](content.md) |
| [CompactionSummaryChunk](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L307) :307 | compactionId, content, _meta | [CN-15](content.md) |
| [CompactionUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L230) :230 | compactionId, status, summary, error, _meta | [CN-14](content.md) |
| [ConfigOptionUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L392) :392 | configOptions, _meta | [CN-11](content.md) |
| [ContentChunk](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L611) :611 | content, messageId, _meta | [CN-01, CN-02, CN-03](content.md) |
| [Cost](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L564) :564 | amount, currency, _meta | [CN-13](content.md) |
| [CreateTerminalRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1356) :1356 | sessionId, command, args, env, cwd, outputByteLimit, _meta | [S06](services.md) |
| [CreateTerminalResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1470) :1470 | terminalId, _meta | [S06](services.md) |
| [CurrentModeUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L348) :348 | currentModeId, _meta | [CN-10](content.md) |
| [FileSystemCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2415) :2415 | readTextFile, writeTextFile, _meta | [InitializeRequest](lifecycle.md); [CAP01](services.md) |
| [KillTerminalRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1712) :1712 | sessionId, terminalId, _meta | [S09](services.md) |
| [KillTerminalResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1760) :1760 | _meta | [S09](services.md) |
| [MessageId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L673) :673 | pub struct MessageId(pub Arc<str>); | [CN-01, CN-02, CN-03](content.md) |
| [PermissionOption](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L917) :917 | optionId, name, kind, _meta | [S01](services.md) |
| [PermissionOptionId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L970) :970 | pub struct PermissionOptionId(pub Arc<str>); | [F01, S01](services.md) |
| [PermissionOptionKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L987) :987 | AllowOnce, AllowAlways, RejectOnce, RejectAlways | [F01, S01](services.md) |
| [ReadTextFileRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1219) :1219 | sessionId, path, line, limit, _meta | [S04](services.md) |
| [ReadTextFileResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1293) :1293 | content, _meta | [S04](services.md) |
| [ReleaseTerminalRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1624) :1624 | sessionId, terminalId, _meta | [S10](services.md) |
| [ReleaseTerminalResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1672) :1672 | _meta | [S10](services.md) |
| [RequestPermissionOutcome](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1050) :1050 | Cancelled, Selected | [F01, S01](services.md) |
| [RequestPermissionRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L863) :863 | sessionId, toolCall, options, _meta | [S01](services.md) |
| [RequestPermissionResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1006) :1006 | outcome, _meta | [S01](services.md) |
| [SelectedPermissionOutcome](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1071) :1071 | optionId, _meta | [F01, S01](services.md) |
| [SessionConfigOptionsCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L2248) :2248 | boolean, _meta | [InitializeRequest](lifecycle.md); [CAP05](services.md); [CT-03](content.md) |
| [SessionInfoUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L441) :441 | title, updatedAt, _meta | [CN-12](content.md) |
| [SessionNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L49) :49 | sessionId, update, _meta | [CN-00](content.md) |
| [SessionUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L99) :99 | UserMessageChunk, AgentMessageChunk, AgentThoughtChunk, ToolCall, ToolCallUpdate, Plan, PlanUpdate, PlanRemoved, AvailableCommandsUpdate, CurrentModeUpdate, ConfigOptionUpdate, SessionInfoUpdate, UsageUpdate, CompactionUpdate, CompactionSummaryChunk | [CN-00](content.md) |
| [TerminalExitStatus](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1893) :1893 | exitCode, signal, _meta | [S06, S07, S08, S09, S10](services.md) |
| [TerminalId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1338) :1338 | pub struct TerminalId(pub Arc<str>); | [InitializeRequest](lifecycle.md); [S06, S07, S08, S09, S10](services.md) |
| [TerminalOutputRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1515) :1515 | sessionId, terminalId, _meta | [S07](services.md) |
| [TerminalOutputResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1563) :1563 | output, truncated, exitStatus, _meta | [S07](services.md) |
| [UnstructuredCommandInput](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L812) :812 | hint, _meta | [CN-09](content.md) |
| [UsageUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L504) :504 | used, size, cost, _meta | [CN-13](content.md) |
| [WaitForTerminalExitRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1800) :1800 | sessionId, terminalId, _meta | [S08](services.md) |
| [WaitForTerminalExitResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1848) :1848 | exit_status (flatten), _meta | [S08](services.md) |
| [WriteTextFileRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1120) :1120 | sessionId, path, content, _meta | [S05](services.md) |
| [WriteTextFileResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/client.rs#L1175) :1175 | _meta | [S05](services.md) |

### v1/content.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [Annotations](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L566) :566 | audience, lastModified, priority, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-06](content.md) |
| [AudioContent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L206) :206 | annotations, data, mimeType, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-03](content.md) |
| [BlobResourceContents](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L394) :394 | blob, mimeType, uri, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-05](content.md) |
| [ContentBlock](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L38) :38 | Text, Image, Audio, ResourceLink, Resource | [InitializeRequest, PromptRequest](lifecycle.md); [CB-06](content.md) |
| [EmbeddedResource](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L265) :265 | annotations, resource, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-05](content.md) |
| [EmbeddedResourceResource](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L320) :320 | TextResourceContents, BlobResourceContents | [InitializeRequest, PromptRequest](lifecycle.md); [CB-05](content.md) |
| [ImageContent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L133) :133 | annotations, data, mimeType, uri, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-02](content.md) |
| [ResourceLink](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L454) :454 | annotations, description, mimeType, name, size, title, uri, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-04](content.md) |
| [Role](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L639) :639 | Assistant, User | [InitializeRequest, PromptRequest](lifecycle.md); [CB-06](content.md) |
| [TextContent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L70) :70 | annotations, text, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-01](content.md) |
| [TextResourceContents](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/content.rs#L334) :334 | mimeType, text, uri, _meta | [InitializeRequest, PromptRequest](lifecycle.md); [CB-05](content.md) |

### v1/elicitation.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [BooleanPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L553) :553 | title, description, default, _meta | [F02, F05, S02](services.md) |
| [CompleteElicitationNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L2147) :2147 | elicitationId, _meta | [F11, S03](services.md) |
| [CreateElicitationRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1584) :1584 | mode (flatten), message, _meta | [S02](services.md) |
| [CreateElicitationResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1867) :1867 | action (flatten), _meta | [S02](services.md) |
| [ElicitationAcceptAction](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L2043) :2043 | content | [F09, S02](services.md) |
| [ElicitationAction](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1914) :1914 | Accept, Decline, Cancel, Other | [F09, S02](services.md) |
| [ElicitationCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1306) :1306 | form, url, _meta | [InitializeRequest](lifecycle.md); [CAP04](services.md) |
| [ElicitationContentValue](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L2072) :2072 | String, Integer, Number, Boolean, StringArray | [F09, S02](services.md) |
| [ElicitationFormCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1403) :1403 | _meta | [InitializeRequest](lifecycle.md); [CAP04](services.md) |
| [ElicitationFormMode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1808) :1808 | scope (flatten), requestedSchema | [S02](services.md) |
| [ElicitationId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L27) :27 | pub struct ElicitationId(pub Arc<str>); | [F11, S02, S03](services.md) |
| [ElicitationMode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1640) :1640 | Form, Url, Other | [S02](services.md) |
| [ElicitationPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L968) :968 | String, Number, Integer, Boolean, Array, Other | [F02, S02](services.md) |
| [ElicitationRequestScope](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1545) :1545 | requestId | [F08, F10, F11, S02](services.md) |
| [ElicitationSchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1128) :1128 | type, title, properties, required, description, _meta | [F02, S02](services.md) |
| [ElicitationSchemaType](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L58) :58 | Object | [F02, S02](services.md) |
| [ElicitationScope](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1489) :1489 | Session, Request | [F08, F10, F11, S02](services.md) |
| [ElicitationSessionScope](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1508) :1508 | sessionId, toolCallId | [F08, F10, F11, S02](services.md) |
| [ElicitationUrlCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1448) :1448 | _meta | [InitializeRequest](lifecycle.md); [CAP04](services.md) |
| [ElicitationUrlMode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1832) :1832 | scope (flatten), elicitationId, url | [F11, S02](services.md) |
| [EnumOption](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L70) :70 | const, title, description, _meta | [F02, F03, F06, S02](services.md) |
| [IntegerPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L444) :444 | title, description, minimum, maximum, default, _meta | [F02, F04, S02](services.md) |
| [MultiSelectItems](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L815) :815 | String, Other, Titled | [F02, F06, S02](services.md) |
| [MultiSelectPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L833) :833 | title, description, minItems, maxItems, items, default, _meta | [F02, F06, S02](services.md) |
| [NumberPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L335) :335 | title, description, minimum, maximum, default, _meta | [F02, F04, S02](services.md) |
| [OtherElicitationAction](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1945) :1945 | action, fields (flatten) | [F08, F09, S02](services.md) |
| [OtherElicitationMode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1669) :1669 | mode, scope (flatten), fields (flatten) | [F08, S02](services.md) |
| [OtherElicitationPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L1003) :1003 | type, fields (flatten) | [F02, F07, S02](services.md) |
| [OtherMultiSelectItems](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L735) :735 | type, fields (flatten) | [F02, F06, F07, S02](services.md) |
| [StringFormat](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L42) :42 | Email, Uri, Date, DateTime | [F02, F03, S02](services.md) |
| [StringMultiSelectItems](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L637) :637 | enum, _meta | [F02, F06, S02](services.md) |
| [StringPropertySchema](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L140) :140 | title, description, minLength, maxLength, pattern, format, default, enum, oneOf, _meta | [F02, F03, S02](services.md) |
| [TitledMultiSelectItems](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/elicitation.rs#L682) :682 | anyOf, _meta | [F02, F06, S02](services.md) |

### v1/error.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [Error](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/error.rs#L36) :36 | code, message, data | [SuggestNesRequest](lifecycle.md); [T14](transport.md) |
| [ErrorCode](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/error.rs#L149) :149 | ParseError, InvalidRequest, MethodNotFound, InvalidParams, InternalError, RequestCancelled, AuthRequired, ResourceNotFound, Other | [SuggestNesRequest](lifecycle.md); [T14](transport.md) |

### v1/ext.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [ExtNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/ext.rs#L77) :77 | method, params | [ExtNotification](lifecycle.md); [S16](services.md); [T15](transport.md) |
| [ExtRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/ext.rs#L25) :25 | method, params | [ExtMethodRequest](lifecycle.md); [S15](services.md); [T15](transport.md) |
| [ExtResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/ext.rs#L56) :56 | pub struct ExtResponse( #[cfg_attr(feature = "schemars", schemars(with = "serde_json::Value"))] pub Arc<RawValue>, ); | [ExtMethodRequest](lifecycle.md); [S15](services.md); [T15](transport.md) |

### v1/mcp.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [ConnectMcpRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L46) :46 | serverId, _meta | [S11](services.md) |
| [ConnectMcpResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L95) :95 | connectionId, _meta | [S11](services.md) |
| [DisconnectMcpRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L313) :313 | connectionId, _meta | [S14](services.md) |
| [DisconnectMcpResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L362) :362 | _meta | [S14](services.md) |
| [McpConnectionId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L24) :24 | pub struct McpConnectionId(pub Arc<str>); | [MessageMcpRequest, MessageMcpNotification](lifecycle.md); [S11, S12, S13, S14](services.md) |
| [MessageMcpNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L217) :217 | connectionId, method, params, _meta | [MessageMcpNotification](lifecycle.md); [S13](services.md) |
| [MessageMcpRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L144) :144 | connectionId, method, params, _meta | [MessageMcpRequest](lifecycle.md); [S12](services.md) |
| [MessageMcpResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mcp.rs#L289) :289 | pub struct MessageMcpResponse( #[cfg_attr(feature = "schemars", schemars(with = "serde_json::Value"))] pub Arc<RawValue>, ); | [MessageMcpRequest](lifecycle.md); [S12](services.md) |

### v1/mod.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [SessionId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/mod.rs#L51) :51 | pub struct SessionId(pub Arc<str>); | [InitializeRequest, NewSessionRequest, LoadSessionRequest, ResumeSessionRequest, ForkSessionRequest, 另 17 项](lifecycle.md) |

### v1/nes.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [AcceptNesNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2683) :2683 | sessionId, id, _meta | [AcceptNesNotification](lifecycle.md) |
| [ClientNesCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L880) :880 | jump, rename, searchAndReplace, _meta | [InitializeRequest](lifecycle.md) |
| [CloseNesRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1630) :1630 | sessionId, _meta | [CloseNesRequest](lifecycle.md) |
| [CloseNesResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1675) :1675 | _meta | [CloseNesRequest](lifecycle.md) |
| [DidChangeDocumentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1108) :1108 | sessionId, uri, version, contentChanges, _meta | [DidChangeDocumentNotification](lifecycle.md) |
| [DidCloseDocumentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1230) :1230 | sessionId, uri, _meta | [DidCloseDocumentNotification](lifecycle.md) |
| [DidFocusDocumentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1326) :1326 | sessionId, uri, version, position, visibleRange, _meta | [DidFocusDocumentNotification](lifecycle.md) |
| [DidOpenDocumentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1045) :1045 | sessionId, uri, languageId, version, text, _meta | [DidOpenDocumentNotification](lifecycle.md) |
| [DidSaveDocumentNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1278) :1278 | sessionId, uri, _meta | [DidSaveDocumentNotification](lifecycle.md) |
| [NesCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L181) :181 | events, context, _meta | [InitializeRequest](lifecycle.md) |
| [NesContextCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L572) :572 | recentFiles, relatedSnippets, editHistory, userActions, openFiles, diagnostics, _meta | [InitializeRequest](lifecycle.md) |
| [NesDiagnostic](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2248) :2248 | uri, range, severity, message, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesDiagnosticSeverity](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2303) :2303 | Error, Warning, Information, Hint | [SuggestNesRequest](lifecycle.md) |
| [NesDiagnosticsCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L850) :850 | _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentDidChangeCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L431) :431 | syncKind, _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentDidCloseCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L488) :488 | _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentDidFocusCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L544) :544 | _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentDidOpenCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L403) :403 | _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentDidSaveCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L516) :516 | _meta | [InitializeRequest](lifecycle.md) |
| [NesDocumentEventCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L295) :295 | didOpen, didChange, didClose, didSave, didFocus, _meta | [InitializeRequest](lifecycle.md) |
| [NesEditHistoryCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L756) :756 | maxCount, _meta | [InitializeRequest](lifecycle.md) |
| [NesEditHistoryEntry](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2070) :2070 | uri, diff, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesEditSuggestion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2389) :2389 | id, uri, edits, cursorPosition, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesEventCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L244) :244 | document, _meta | [InitializeRequest](lifecycle.md) |
| [NesExcerpt](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2020) :2020 | startLine, endLine, text, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesJumpCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L958) :958 | _meta | [InitializeRequest](lifecycle.md) |
| [NesJumpSuggestion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2503) :2503 | id, uri, position, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesOpenFile](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2175) :2175 | uri, languageId, visibleRange, lastFocusedMs, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesOpenFilesCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L822) :822 | _meta | [InitializeRequest](lifecycle.md) |
| [NesRecentFile](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1919) :1919 | uri, languageId, text, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesRecentFilesCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L695) :695 | maxCount, _meta | [InitializeRequest](lifecycle.md) |
| [NesRejectReason](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2788) :2788 | Rejected, Ignored, Replaced, Cancelled | [RejectNesNotification](lifecycle.md) |
| [NesRelatedSnippet](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1973) :1973 | uri, excerpts, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesRelatedSnippetsCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L728) :728 | _meta | [InitializeRequest](lifecycle.md) |
| [NesRenameCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L986) :986 | _meta | [InitializeRequest](lifecycle.md) |
| [NesRenameSuggestion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2553) :2553 | id, uri, position, newName, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesRepository](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1525) :1525 | name, owner, remoteUrl, _meta | [StartNesRequest](lifecycle.md) |
| [NesSearchAndReplaceCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1014) :1014 | _meta | [InitializeRequest](lifecycle.md) |
| [NesSearchAndReplaceSuggestion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2611) :2611 | id, uri, search, replace, isRegex, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesSuggestContext](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1817) :1817 | recentFiles, relatedSnippets, editHistory, userActions, openFiles, diagnostics, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesSuggestion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2371) :2371 | Edit, Jump, Rename, SearchAndReplace | [SuggestNesRequest](lifecycle.md) |
| [NesSuggestionId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L46) :46 | pub struct NesSuggestionId(pub Arc<str>); | [SuggestNesRequest, AcceptNesNotification, RejectNesNotification](lifecycle.md) |
| [NesTextEdit](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2456) :2456 | range, newText, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesTriggerKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1713) :1713 | Automatic, Diagnostic, Manual | [SuggestNesRequest](lifecycle.md) |
| [NesUserAction](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2117) :2117 | action, uri, position, timestampMs, _meta | [SuggestNesRequest](lifecycle.md) |
| [NesUserActionsCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L789) :789 | maxCount, _meta | [InitializeRequest](lifecycle.md) |
| [Position](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L85) :85 | line, character, _meta | [SuggestNesRequest, DidChangeDocumentNotification, DidFocusDocumentNotification](lifecycle.md) |
| [PositionEncodingKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L64) :64 | Utf16, Utf32, Utf8 | [InitializeRequest](lifecycle.md) |
| [Range](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L132) :132 | start, end, _meta | [SuggestNesRequest, DidChangeDocumentNotification, DidFocusDocumentNotification](lifecycle.md) |
| [RejectNesNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2731) :2731 | sessionId, id, reason, _meta | [RejectNesNotification](lifecycle.md) |
| [StartNesRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1391) :1391 | workspaceUri, workspaceFolders, repository, _meta | [StartNesRequest](lifecycle.md) |
| [StartNesResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1580) :1580 | sessionId, _meta | [StartNesRequest](lifecycle.md) |
| [SuggestNesRequest](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1733) :1733 | sessionId, uri, version, position, selection, triggerKind, context, _meta | [SuggestNesRequest](lifecycle.md) |
| [SuggestNesResponse](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L2328) :2328 | suggestions, _meta | [SuggestNesRequest](lifecycle.md) |
| [TextDocumentContentChangeEvent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1171) :1171 | range, text, _meta | [DidChangeDocumentNotification](lifecycle.md) |
| [TextDocumentSyncKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L472) :472 | Full, Incremental | [InitializeRequest](lifecycle.md) |
| [WorkspaceFolder](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/nes.rs#L1478) :1478 | uri, name, _meta | [StartNesRequest](lifecycle.md) |

### v1/plan.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [Plan](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L33) :33 | entries, _meta | [CN-06, PL-01](content.md) |
| [PlanCapabilities](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L411) :411 | _meta | [InitializeRequest](lifecycle.md); [PL-01](content.md) |
| [PlanEntry](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L455) :455 | content, priority, status, _meta | [CN-06, PL-01](content.md) |
| [PlanEntryPriority](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L512) :512 | High, Medium, Low | [CN-06, PL-01](content.md) |
| [PlanEntryStatus](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L529) :529 | Pending, InProgress, Completed | [CN-06, PL-01](content.md) |
| [PlanFile](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L255) :255 | planId, uri, _meta | [CN-07, PL-01](content.md) |
| [PlanId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L86) :86 | pub struct PlanId(pub Arc<str>); | [CN-07, CN-08, PL-01](content.md) |
| [PlanItems](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L197) :197 | planId, entries, _meta | [CN-07, PL-01](content.md) |
| [PlanMarkdown](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L308) :308 | planId, content, _meta | [CN-07, PL-01](content.md) |
| [PlanRemoved](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L361) :361 | planId, _meta | [CN-08, PL-01](content.md) |
| [PlanUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L109) :109 | plan, _meta | [CN-07, PL-01](content.md) |
| [PlanUpdateContent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/plan.rs#L155) :155 | Items, File, Markdown | [CN-07, PL-01](content.md) |

### v1/protocol_level.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [CancelRequestNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/protocol_level.rs#L18) :18 | requestId, _meta | [S17](services.md); [T12](transport.md) |
| [GeneralMethodNames](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/protocol_level.rs#L62) :62 | cancel_request | [S17](services.md); [T12](transport.md) |
| [ProtocolLevelNotification](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/protocol_level.rs#L91) :91 | CancelRequestNotification | [S17](services.md); [T12](transport.md) |

### v1/tool_call.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [Content](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L604) :604 | content, _meta | [TC-03](content.md) |
| [Diff](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L700) :700 | path, oldText, newText, _meta | [TC-03](content.md) |
| [Terminal](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L652) :652 | terminalId, _meta | [InitializeRequest](lifecycle.md); [TC-03](content.md) |
| [ToolCall](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L29) :29 | toolCallId, title, name, kind, status, content, locations, rawInput, rawOutput, _meta | [CN-04, TC-01](content.md) |
| [ToolCallContent](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L572) :572 | Content, Diff, Terminal | [TC-02](content.md) |
| [ToolCallId](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L473) :473 | pub struct ToolCallId(pub Arc<str>); | [CN-04, CN-05, TC-01](content.md) |
| [ToolCallLocation](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L765) :765 | path, line, _meta | [TC-03](content.md) |
| [ToolCallStatus](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L540) :540 | Pending, InProgress, Completed, Failed | [TC-02](content.md) |
| [ToolCallUpdate](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L220) :220 | toolCallId, fields (flatten), _meta | [S01](services.md); [CN-05, TC-01](content.md) |
| [ToolCallUpdateFields](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L273) :273 | kind, status, title, name, content, locations, rawInput, rawOutput | [S01](services.md); [CN-05, TC-01](content.md) |
| [ToolKind](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/v1/tool_call.rs#L499) :499 | Read, Edit, Delete, Move, Search, Execute, Think, Fetch, SwitchMode, Other | [TC-02](content.md) |

### version.rs

| 类型与源码 | 字段或分支 | 功能归属 |
| --- | --- | --- |
| [ProtocolVersion](https://github.com/agentclientprotocol/agent-client-protocol/blob/272bf799f35a258c6a4107a0410ed361e83683d3/agent-client-protocol-schema/src/version.rs#L12) :12 | pub struct ProtocolVersion(u16); | [T01](transport.md) |

## 5. 复核边界

闭合声明集合不等于证明不存在 bug。运行期边界、第三方 Agent 的实际行为、整数/正则极值、
晚到回调与释放顺序仍需矩阵里的直接用例验证；标记待定的模型选择也仍未完成。
本轮不运行生产代码测试，不把静态测试定位写成 CI 通过，不以最新 Zed 的实现代替当前 schema。
