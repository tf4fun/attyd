# ACP 客户端→Agent：生命周期、能力及排除面逐项清单

[总矩阵与版本基线](../acp-bridge-matrix.md) · [源码闭合索引](schema-index.md)


- 实验实现：`991b053`；稳定 v1：`60f8606` / `v0.3.0-rc.5`。
- 锁定 SDK `754d5aa1ce2cfa54ba2c2a6d3edc7e7b6bce28eb`，schema `1.7.0` 的 `v1`。
- Zed 参考：`9f63850f7affbd57772a4f99554d4fd333b50a6f`，实际锁 SDK 3.4.0 / schema 1.12.0，仅借鉴兼容的 **v1 adapter**。
- 这里的“已调用”只表示真实 ACP 出站路径，不等于所有返回字段已保留，更不等于 FE 已支持。
- 参数字段、属性、嵌套类型及源码行号见 [source-inventory.json](source-inventory.json)；行为判断和验收要求见本分册。

## 机械闭合与范围

`ClientRequest` **21** 个变体、`ClientNotification` **10** 个变体全部逐项列出；与源枚举集合进行了独立 equality 检查。请求/通知同名 `mcp/message` 分别计数。

- 当前实际编译：18 个请求变体、3 个通知变体（包含通用 extension envelope）。
- 找到实际 bridge 出站：14 个请求方法（13 个初始化/认证/会话 + MCP message）、2 个通知方法（cancel、MCP message）。
- 3 个 providers 请求已编译/注册，项目没有 caller；2 个 extension envelope 有 SDK 支持但没有通用出站入口。
- 3 个 NES 请求 + 7 个 NES/document 通知受未启用 `unstable_nes` 排除，SDK 本身也没有这些 typed registrations。
- `$ /cancel_request`（实际拼写无空格）、JSON-RPC error、扩展入站及传输排序属于独立通用分册，避免计数混入这里。

## 重要结果

1. 生命周期 RPC 大多存在；主要差距是实验实现的数据归约/保留、close 入口与控制写准入。不能说 bridge 没有 close/mode/config RPC。
2. `session/load` 才明确完整回放，`session/resume` 不回放；fork 的早到几条通知不证明完整历史。目标规则见 [memory model](../bridge-memory-model.md)：fork 目标不继承源会话条目（§6.3，支持 load 则加载目标，否则只保留目标自己的通知并标 partial）；安装中的未知会话通知和资源按 §9 暂存归属。
3. 参数覆盖并非每个字段都有前端任意透传：catalog 刻意不传 cwd filter，workspace/MCP 来自宿主配置，request envelope `_meta` 由 bridge 默认省略。这些需明示为产品边界，不应偷算“逐字段全部支持”。
4. schema-defined metadata 的容器要单独追踪：setup 外层 `_meta` 在稳定版也未持久保留；`modes._meta` / 控制通知外层 `_meta` 是 D12 回退；catalog page 外层 `_meta` 在实验 wrapper 中另有损失。
5. Zed 不能当规范：它版本更新且有 vendor workaround。采用其标准请求生命周期/资源隔离参考，不引入厂商分支或整套 ACP v2。

## 逐方法矩阵

### 1. `initialize` — InitializeRequest (request)

- Schema: `v1/agent.rs:4790`; input `v1/agent.rs:55-80`; response `v1/agent.rs:132-165`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:14`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`protocolVersion: ProtocolVersion`, `clientCapabilities: ClientCapabilities`, `clientInfo: Option<Implementation>`, `_meta: Option<Meta>`.
- 返回字段：`protocolVersion: ProtocolVersion`, `agentCapabilities: AgentCapabilities`, `authMethods: Vec<AuthMethod>`, `agentInfo: Option<Implementation>`, `_meta: Option<Meta>`.
- 语义：One initial request negotiates version, capabilities, auth methods and implementation identity; response is not evidence that optional methods are called.
- 能力门槛：Bridge explicitly sends ProtocolVersion::V1 and rejects any response != V1; defaults and omitted capabilities follow the pinned schema.
- Bridge: `991b0532:src/bridge.rs:1662–1716`; `991b0532:src/bridge.rs:2219–2241`; `991b0532:src/bridge.rs:5624–5700`.
- 保留：Full typed InitializeResponse JSON is retained in global.agent.initialize; AgentCapabilities and offered auth methods also index runtime validation. Typed decode preserves schema-defined _meta, not arbitrary future fields absent from this version.
- 清理：Per-connection state is replaced on new connection; last experimental global can describe stopped/error. No durable cross-process capability history.
- 稳定版差异：rc5 also negotiates V1; same service capability profile. No requirement to adopt Zed ACP v2 from its use of internal v2 model types.
- 验收：(1) V1 succeeds; incompatible version refuses before session commands. (2) Snapshot exact advertised service capabilities for stdio/read-only/remote profiles. (3) Initialize metadata and all auth-method variant fields survive current global memory; terminal methods on remote are rejected.
- Zed v1 参考：[L585](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L585); [L445](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L445).

### 2. `authenticate` — AuthenticateRequest (request)

- Schema: `v1/agent.rs:4801`; input `v1/agent.rs:295-309`; response `v1/agent.rs:341-352`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:15`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`methodId: AuthMethodId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Only an offered agent-managed method is sent to authenticate; absent auth type means Agent. Terminal auth is a separate local process and must never be sent here. Success allows new sessions without auth_required.
- 能力门槛：Offered AuthMethod::Agent ID; no additional advertised boolean required.
- Bridge: `991b0532:src/bridge.rs:2838–2874`; `991b0532:src/bridge.rs:5656–5681`.
- 保留：Stores authenticatedMethodId and completes global op with {}. AuthenticateResponse only contains optional _meta; experiment currently discards it after validating size.
- 清理：Current global op is replaced by later work; auth marker clears on successful logout/new connection.
- 稳定版差异：rc5 bridge.rs:3691–3728 delivered complete typed response in result/event; this was current/transient response data, not a permanent request ledger. Decide current-result metadata retention without inventing history.
- 验收：(1) Offered agent method succeeds and unknown/terminal method sends no authenticate RPC. (2) Auth-required error remains structured and pending request-scoped interactions remain answerable according to their actual parent RPC. (3) Explicitly decide whether current auth op exposes response _meta; do not infer a cross-connection history requirement.
- Zed v1 参考：[L1529](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1529).

### 3. `logout` — LogoutRequest (request)

- Schema: `v1/agent.rs:4827`; input `v1/agent.rs:385-396`; response `v1/agent.rs:425-436`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:22`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Successful logout makes new sessions require authentication. ACP deliberately does not guarantee effects on sessions already running. Wire method is logout, not auth/logout.
- 能力门槛：agentCapabilities.auth.logout present; absence/null means unavailable.
- Bridge: `991b0532:src/bridge.rs:2876–2897`; `991b0532:src/bridge.rs:5684–5698`.
- 保留：Clears authenticatedMethodId; global op result {} drops optional response _meta.
- 清理：No forced deletion of existing sessions is implied by logout. Marker and operation are connection-local.
- 稳定版差异：rc5 bridge.rs:3729–3750 returned complete current response; same capability check and no guarantee about running Agent sessions.
- 验收：(1) Absent logout capability sends no RPC; failed logout keeps prior auth marker; success clears it. (2) Do not invent a session-close effect or retry an uncertain logout automatically.
- Zed v1 参考：[L1549](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1549).

### 4. `session/new` — NewSessionRequest (request)

- Schema: `v1/agent.rs:4840`; input `v1/agent.rs:796-822`; response `v1/agent.rs:872-899`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:23`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`cwd: PathBuf`, `additionalDirectories: Vec<PathBuf>`, `mcpServers: Vec<McpServer>`, `_meta: Option<Meta>`.
- 返回字段：`sessionId: SessionId`, `modes: Option<SessionModeState>`, `configOptions: Option<Vec<SessionConfigOption>>`, `_meta: Option<Meta>`.
- 语义：RPC carries absolute cwd, configured additionalDirectories and mcpServers. Additional roots are a complete replacement list, not an append; response modes/configOptions fold early notifications → provided response fields → later updates. New returns a unique sessionId and can emit updates before the response; may fail auth_required.
- 能力门槛：Core method. Configured MCP transport and additional-directories capabilities validated at initialization.
- Bridge: `991b0532:src/bridge.rs:2899–3005`; `991b0532:src/bridge.rs:5026–5049`.
- 保留：Session registry retains cwd and reduced controls, not the whole setup response. Experimental controls retain nested mode/option objects but drop modes._meta and outer control-notification _meta (D12). Both baselines already discard setup response outer _meta from durable session state. Early conversation notifications become initial TurnLog, currently canon loses non-message content. create result is sessionId.
- 清理：Failed attempts clear their private replay/creation staging and reject/close newly returned invalid targets; installed session state ends on close/delete/release, or connection replacement under its retention policy.
- 稳定版差异：rc5 preserves reduced early history; experiment reduces it to message-only canon (D1). Both have pre-response staging but early unknown-session service/interaction attribution remains an explicit target decision.
- 验收：(1) Assert all request setup fields and capability gates on the wire, including absolute paths and full additional-root replacement. (2) Early controls, response fields, later equal-value notifications and sparse fields must produce explicit expected retained values and metadata. (3) On rejected/failed setup, private candidates must not mutate the previous held session or leak resources. (4) Two concurrent creates/forks emitting unknown-session interactions or terminal/fs calls must not guess parent association. (5) Creation returns sessionId even when later operations replace global op.
- Zed v1 参考：[L1238](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1238); [L1129](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1129).

### 5. `session/load` — LoadSessionRequest (request)

- Schema: `v1/agent.rs:4851`; input `v1/agent.rs:958-987`; response `v1/agent.rs:1036-1059`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:24`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`mcpServers: Vec<McpServer>`, `cwd: PathBuf`, `additionalDirectories: Vec<PathBuf>`, `sessionId: SessionId`, `_meta: Option<Meta>`.
- 返回字段：`modes: Option<SessionModeState>`, `configOptions: Option<Vec<SessionConfigOption>>`, `_meta: Option<Meta>`.
- 语义：RPC carries absolute cwd, configured additionalDirectories and mcpServers. Additional roots are a complete replacement list, not an append; response modes/configOptions fold early notifications → provided response fields → later updates. Load is authoritative entire conversation replay through session/update before the load response, unlike resume.
- 能力门槛：agentCapabilities.loadSession true.
- Bridge: `991b0532:src/bridge.rs:3078–3158`; `991b0532:src/bridge.rs:3275–3298`; `991b0532:src/protocol/registry.rs:834–839`; `991b0532:src/bridge/streams.rs:161–179`.
- 保留：Session registry retains cwd and reduced controls, not the whole setup response. Experimental controls retain nested mode/option objects but drop modes._meta and outer control-notification _meta (D12). Both baselines already discard setup response outer _meta from durable session state. Replay is staged privately then replaces known history atomically. Experimental canon currently retains only messages, not all replay content.
- 清理：Failed attempts clear their private replay/creation staging and reject/close newly returned invalid targets; installed session state ends on close/delete/release, or connection replacement under its retention policy.
- 稳定版差异：rc5 HistorySnapshot retains reduced replay. Experimental message-only projection is actual authoritative-memory loss, not merely a missing FE card.
- 验收：(1) Assert all request setup fields and capability gates on the wire, including absolute paths and full additional-root replacement. (2) Early controls, response fields, later equal-value notifications and sparse fields must produce explicit expected retained values and metadata. (3) On rejected/failed setup, private candidates must not mutate the previous held session or leak resources. (4) An empty successful load is confirmed complete empty; no replay from resume is not that case. (5) Reload all tool/thought/plan/compaction/message fields from Agent input and assert retained memory independently of the projection.
- Zed v1 参考：[L1418](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1418); [L859](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L859).

### 6. `session/resume` — ResumeSessionRequest (request)

- Schema: `v1/agent.rs:4881`; input `v1/agent.rs:1289-1319`; response `v1/agent.rs:1368-1391`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:29`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `cwd: PathBuf`, `additionalDirectories: Vec<PathBuf>`, `mcpServers: Vec<McpServer>`, `_meta: Option<Meta>`.
- 返回字段：`modes: Option<SessionModeState>`, `configOptions: Option<Vec<SessionConfigOption>>`, `_meta: Option<Meta>`.
- 语义：RPC carries absolute cwd, configured additionalDirectories and mcpServers. Additional roots are a complete replacement list, not an append; response modes/configOptions fold early notifications → provided response fields → later updates. Resume explicitly does not return previous messages; retain existing known history, and mark cold history partial/unknown-before rather than confirmed empty.
- 能力门槛：agentCapabilities.sessionCapabilities.resume present. Reattach chooses load if supported, else resume.
- Bridge: `991b0532:src/bridge.rs:3078–3158`; `991b0532:src/bridge.rs:3301–3342`; `991b0532:src/protocol/registry.rs:842–850`; `991b0532:src/bridge/streams.rs:161–179`.
- 保留：Session registry retains cwd and reduced controls, not the whole setup response. Experimental controls retain nested mode/option objects but drop modes._meta and outer control-notification _meta (D12). Both baselines already discard setup response outer _meta from durable session state. Current adopt_session_log turns unknown into Known(from received updates), including empty, without completeness flag (D9). Optional authoritative load follows when supported.
- 清理：Failed attempts clear their private replay/creation staging and reject/close newly returned invalid targets; installed session state ends on close/delete/release, or connection replacement under its retention policy.
- 稳定版差异：rc5 distinguishes missing history after resume and retains received context; it does not reconstruct history that the Agent never replayed. Target step 14 adds completeness semantics.
- 验收：(1) Cold resume with no updates must remain partial, not complete empty. (2) Known held log survives resume; actual full load may replace it, with no fabricated terminal bytes. (3) Fail/retry one materialization without cross-writing another attempt.
- Zed v1 参考：[L1457](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1457); [L1146](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1146).

### 7. `session/fork` — ForkSessionRequest (request)

- Schema: `v1/agent.rs:4874`; input `v1/agent.rs:1119-1148`; response `v1/agent.rs:1203-1228`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:28`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `cwd: PathBuf`, `additionalDirectories: Vec<PathBuf>`, `mcpServers: Vec<McpServer>`, `_meta: Option<Meta>`.
- 返回字段：`sessionId: SessionId`, `modes: Option<SessionModeState>`, `configOptions: Option<Vec<SessionConfigOption>>`, `_meta: Option<Meta>`.
- 语义：RPC carries absolute cwd, configured additionalDirectories and mcpServers. Additional roots are a complete replacement list, not an append; response modes/configOptions fold early notifications → provided response fields → later updates. Fork creates an independent session with the source conversation context; the pinned method does not promise a complete replay via its few early target notifications.
- 能力门槛：unstable_session_fork compiled; agentCapabilities.sessionCapabilities.fork present.
- Bridge: `991b0532:src/bridge.rs:3345–3408`; `991b0532:src/bridge.rs:3491–3509`; `991b0532:src/bridge.rs:3546–3594`.
- 保留：Captures source completed TurnLog and chooses nonempty early target canon over it, then optional load. It does not capture a future r2 idle tail/resource snapshots. Target rule ([memory model §6.3](../bridge-memory-model.md#63-fork)): the target never inherits source entries or resources; with load advertised the target is loaded after the response (`complete`, failure → `partial` with its own notifications); otherwise only notifications for the target, staged during the fork, then later updates (`partial`).
- 清理：Failed attempts clear their private replay/creation staging and reject/close newly returned invalid targets; installed session state ends on close/delete/release, or connection replacement under its retention policy.
- 稳定版差异：rc5 captures source HistorySnapshot before fork and labels fallback possibly incomplete. Neither version proves arbitrary target updates are complete. Experimental inherited history has already lost process content.
- 验收：(1) With load advertised: the target is loaded after the response and its entries equal the replay; a failed load leaves its staged notifications, `partial`. (2) Without load: notifications sent for the target before and after the response are kept, nothing from the source appears, `partial`; a target tool reusing a source tool ID changes neither session's other entity. (3) The source is not mutated and no source resource is copied. (4) Source/target cleanup and the optional load cannot race into a replacement.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 8. `session/list` — ListSessionsRequest (request)

- Schema: `v1/agent.rs:4857`; input `v1/agent.rs:1536-1553`; response `v1/agent.rs:1596-1617`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:25`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`cwd: Option<PathBuf>`, `cursor: Option<String>`, `_meta: Option<Meta>`.
- 返回字段：`sessions: Vec<SessionInfo>`, `nextCursor: Option<String>`, `_meta: Option<Meta>`.
- 语义：Request supports optional absolute cwd filter and opaque cursor; response contains complete SessionInfo rows plus nextCursor and response _meta. No nextCursor means end.
- 能力门槛：agentCapabilities.sessionCapabilities.list present.
- Bridge: `991b0532:src/bridge.rs:3008–3075`.
- 保留：Retains typed SessionInfo rows (sessionId,cwd,additionalDirectories,title,updatedAt,_meta) in listed_sessions, clears prior rows on first page. Current CatalogPage retains rows/cursor/revision but drops response _meta. Request intentionally omits cwd to discover all directories; no user cwd-filter entry.
- 清理：Rows removed on delete; list first page replaces directory cache; catalog revision rejects stale paging; cache resets with connection.
- 稳定版差异：rc5 bridge.rs:3916–3925 retains typed rows and sends entire page with catalogRevision (including response _meta). Current cwd-filter omission was already v1 product policy, not new regression.
- 验收：(1) Round-trip every SessionInfo field and opaque cursor; paginated catalog changes must refuse stale chain. (2) Retain or explicitly decide current page outer metadata; no requirement for permanent catalog-page history. (3) Test session discovery across cwd; if a cwd-filter API is required, add it explicitly instead of claiming existing full parameter exposure.
- Zed v1 参考：[L239](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L239).

### 9. `session/delete` — DeleteSessionRequest (request)

- Schema: `v1/agent.rs:4861`; input `v1/agent.rs:1661-1674`; response `v1/agent.rs:1706-1717`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:26`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Removes an existing session from the Agent session list. Bridge may close a held session first; cold delete need not load content.
- 能力门槛：agentCapabilities.sessionCapabilities.delete; close first only if held and close capability is present.
- Bridge: `991b0532:src/bridge.rs:3825–3940`; `991b0532:src/bridge.rs:3984–3994`.
- 保留：ID reservation prevents conflicting attach. Success removes catalog row and session documents/resources, ends waiting streams deleted, and advances catalog revision; global op retains current result/error.
- 清理：Close and delete are separate RPCs; cleanup follows their actual result; resources and reservations released on failure/success.
- 稳定版差异：Cold/held delete exists in rc5; refusing while the target runs was also stable admission policy, not a lost ACP implementation.
- 验收：(1) Cold delete emits no load; held delete emits close then delete when supported. (2) Close failure prevents delete; delete failure after close cannot claim the Agent session is deleted. (3) Concurrent attach/delete and partial-result failures cannot expose stale content or leak reservations.
- Zed v1 参考：[L280](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L280).

### 10. `session/close` — CloseSessionRequest (request)

- Schema: `v1/agent.rs:4888`; input `v1/agent.rs:1447-1460`; response `v1/agent.rs:1492-1503`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:30`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Agent must cancel ongoing work as if session/cancel and free session resources; not the same as deleting catalog history.
- 能力门槛：agentCapabilities.sessionCapabilities.close present.
- Bridge: `991b0532:src/bridge.rs:3596–3723`; `991b0532:src/protocol/admission.rs:34–74`.
- 保留：Internal ACP command remains; successful close clears ingest/runtime/terminal resources and retires held session. Experimental FE operation surface has no explicit close, target step 18 restores action.
- 清理：Waits for RPC success before full retirement; failed close keeps session and releases operation reservation. Replacement is blocked until cleanup completes.
- 稳定版差异：Stable rc5 has explicit route and permits close during prompt. Missing experiment command entry is R2; not absence of underlying RPC handler.
- 验收：(1) Close running prompt; verify permissions cancelled, late outputs do not mutate reopened session, resources drain, catalog row stays. (2) Auto-release and explicit close cannot overlap; timeout=0 does not release outstanding control action. (3) Failed close leaves still-valid session usable; successful close allows later load/resume.
- Zed v1 参考：[L53](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L53); [L806](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L806); [L859](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L859).

### 11. `session/set_mode` — SetSessionModeRequest (request)

- Schema: `v1/agent.rs:4902`; input `v1/agent.rs:1969-1984`; response `v1/agent.rs:2013-2024`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:31`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `modeId: SessionModeId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Offered current mode may be set while idle or actively generating. Response has only optional _meta; success acknowledges requested mode. Agent autonomous mode notifications also update state.
- 能力门槛：No separate boolean; modeId must be in availableModes. Prefer configOptions over legacy modes when supplied.
- Bridge: `991b0532:src/bridge.rs:4090–4139`; `991b0532:src/bridge.rs:4183–4206`; `991b0532:src/protocol/admission.rs:124–128`; `991b0532:src/protocol/controls.rs:16–29`.
- 保留：Synthesizes acknowledged current_mode_update into controls. Experiment loses modes container/notification metadata (D12), and optional response _meta is discarded. Public operation admission blocks during prompt despite underlying Control admission support.
- 清理：Current mode persists with held session until replaced/closed; action target r2 must bind late reply to the actual session/request and hold during wait.
- 稳定版差异：rc5 permits live Control and stores complete control update plus response in last operation. Target step 18 restores admission and source-scoped metadata, not a history of requests.
- 验收：(1) Prompt running: set_mode RPC executes and fold result reaches retained controls without ending prompt. (2) Early notification → response → later identical-to-early notification uses correct order. (3) Late old reply after close/reopen cannot modify replacement; failed request does not erase valid notifications already received.
- Zed v1 参考：[L9172](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L9172); [L9218](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L9218).

### 12. `session/set_config_option` — SetSessionConfigOptionRequest (request)

- Schema: `v1/agent.rs:4904`; input `v1/agent.rs:2524-2545`; response `v1/agent.rs:2583-2598`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:32`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `configId: SessionConfigId`, `value: SessionConfigOptionValue`, `_meta: Option<Meta>`.
- 返回字段：`configOptions: Vec<SessionConfigOption>`, `_meta: Option<Meta>`.
- 语义：Accepts tagged boolean or string value ID; response is the full set of configuration options/current values, not a delta. Categories include mode/model/model_config/thought_level and opaque Other. Select options may be grouped or ungrouped.
- 能力门槛：Offered option/value; boolean capability is advertised by the client. Model selection uses config category model; no separate session/set_model method exists in pinned v1.
- Bridge: `991b0532:src/bridge.rs:4208–4272`; `991b0532:src/bridge.rs:4316–4359`; `991b0532:src/protocol/controls.rs:30–35`.
- 保留：Validates bool/string against offered options, replaces complete configOptions from acknowledgement, preserving nested option/group/value metadata. Drops response and outer notification metadata; public operation surface blocks prompt-time controls.
- 清理：Current option state lasts with held session; late acknowledgements need real request identity and request-time holding.
- 稳定版差异：rc5 allows setting during prompt and keeps complete current response on runtime operation. Target step 18 addresses admission/D12.
- 验收：(1) Grouped/ungrouped select and boolean including false; unknown category is carried, invalid offered values send nothing. (2) Setup response optional configOptions distinguishes absent/null from []; set_config_option response has a required Vec with DefaultOnError (explicit null becomes empty), and its complete parsed list replaces options. (3) Agent changes dependent options in full response; preserve metadata and do not merge stale entries. (4) Concurrent acknowledgements fold in receive order, isolated from retired/replacement session.
- Zed v1 参考：[L180](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L180); [L9172](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L9172).

### 13. `session/prompt` — PromptRequest (request)

- Schema: `v1/agent.rs:4916`; input `v1/agent.rs:3034-3061`; response `v1/agent.rs:3096-3119`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:33`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `prompt: Vec<ContentBlock>`, `_meta: Option<Meta>`.
- 返回字段：`stopReason: StopReason`, `usage: Option<Usage>`, `_meta: Option<Meta>`.
- 语义：Whole prompt lifecycle; blocks Text and ResourceLink mandatory baseline, image/audio/embedded Resource gated. Returns one of end_turn/max_tokens/max_turn_requests/refusal/cancelled; refusal means this prompt and later content will not enter the next prompt, so presentation must reflect outcome. Pinned source comments disagree: PromptResponse.usage (agent.rs:3106) says this turn, Usage fields (3199–3203) say across session/all turns. Preserve received counters unchanged; do not silently sum or derive increments.
- 能力门槛：Held session; prompt capability checks image/audio/embeddedContext. Unknown block types are rejected by typed/semantic validation. Request envelope _meta is optional but no client entry supplies it.
- Bridge: `991b0532:src/bridge.rs:3996–4088`; `991b0532:src/bridge.rs:4480–4540`; `991b0532:src/bridge.rs:4985–5012`; `991b0532:src/protocol/registry.rs:755–792`.
- 保留：During prompt retains input, updates, tool/resource state. Current completed canon retains messages only; op keeps selected stopReason/usage while latest, not full response metadata/history. Failed/interrupted overlay settlement incomplete (D1–D7).
- 清理：Cancel requests do not finish the operation before actual Agent response. Success/normal failure/transport uncertainty have distinct settlement paths. Session release removes in-memory content.
- 稳定版差异：rc5 retained reduced process history and locally completed PromptResponse in turn_outcomes while held; reload clears local outcomes, no cross-process durable history. Target 11–13 restores full timeline, outcome and idle/late retention.
- 验收：(1) All five stop reasons survive as actual result; refusal is not rendered as context-retained success. (2) All Usage counters and usage._meta plus response._meta survive next prompt in target Turn.end. (3) Text/resourceLink accepted without optional flags; image/audio/resource refused without flags and round-trip with flags. (4) Successful, cancelled, failed and disconnected prompts retain received timeline with truthful outcome; no fabricated success on missing response.
- Zed v1 参考：[L1567](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1567).

### 14. `session/cancel` — CancelNotification (notification)

- Schema: `v1/agent.rs:5084`; input `v1/agent.rs:5173-5186`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:66`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`sessionId: SessionId`, `_meta: Option<Meta>`.
- 语义：Notification cancels prompt work but has no response itself. Agent should drain pending updates and answer original prompt with cancelled; pending permissions require cancelled outcome. Session-scoped forms are not implicitly prompt-scoped.
- 能力门槛：No optional capability. Business cancel targets current running prompt; repeated cancel has no additional effect.
- Bridge: `991b0532:src/bridge/operations.rs:220–254`; `991b0532:src/bridge.rs:4035–4062`; `991b0532:src/bridge.rs:5444–5500`.
- 保留：Sets cancelRequested and retains operation occupation until its actual end. Current commit_session_cancel still cancels all pending session elicitations along with permissions; accepted URL flows already survive (bridge.rs:5459–5464). r2 must narrow pending-form cancellation to its actual scope.
- 清理：Only intended pending permission responders are cancelled; request cancellation must settle its captured registration, not a successor.
- 稳定版差异：Both baselines have broad session interaction cancellation, so session-form retention target is a shared lifecycle gap rather than newly missing notification.
- 验收：(1) Prompt request precedes immediate queued cancel on wire; receives final cancelled response and preserves last updates. (2) Permission cancels; unrelated session-scoped form remains answerable until its own ACP end. (3) Repeated cancel emits no duplicate work; old cancellation callback cannot terminate new request registration.
- Zed v1 参考：[L1631](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L1631).

### 15. `providers/list` — ListProvidersRequest (request)

- Schema: `v1/agent.rs:4808`; input `v1/agent.rs:3472-3483`; response `v1/agent.rs:3518-3531`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:17`; 编译=true; 状态=`compiled_no_bridge_caller`.
- 输入字段：`_meta: Option<Meta>`.
- 返回字段：`providers: Vec<ProviderInfo>`, `_meta: Option<Meta>`.
- 语义：Returns configurable providers: providerId, supported LlmProtocol values, required flag, current apiType/baseUrl, and metadata. Current config does not expose credentials.
- 能力门槛：unstable_llm_providers compiled; Agent advertises agentCapabilities.providers={} (omitted/null means not advertised).
- Bridge: `991b0532:src/bridge.rs:4435–4438`.
- 保留：InitializeResponse can retain advertised providers capability, but no providers state or outgoing command exists.
- 清理：No bridge-created provider resources to clean; request not sent.
- 稳定版差异：Same absent caller in rc5; product excluded/unimplemented, not a claimed lifecycle regression.
- 验收：(1) Record product decision for each method; if excluded, never claim providers support merely because types compile. (2) If enabled later, test all advertised protocols, complete replacement semantics, errors and credentials not exposed in current config.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 16. `providers/set` — SetProviderRequest (request)

- Schema: `v1/agent.rs:4815`; input `v1/agent.rs:3571-3592`; response `v1/agent.rs:3645-3656`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:19`; 编译=true; 状态=`compiled_no_bridge_caller`.
- 输入字段：`providerId: ProviderId`, `apiType: LlmProtocol`, `baseUrl: String`, `headers: HashMap<String, String>`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Replaces provider configuration (providerId, apiType, baseUrl, complete headers). Empty headers replaces previous headers; do not invent partial merge. Supported protocols: anthropic/openai/azure/vertex/bedrock/Other.
- 能力门槛：unstable_llm_providers compiled; Agent advertises agentCapabilities.providers={} (omitted/null means not advertised).
- Bridge: `991b0532:src/bridge.rs:4435–4438`.
- 保留：InitializeResponse can retain advertised providers capability, but no providers state or outgoing command exists.
- 清理：No bridge-created provider resources to clean; request not sent.
- 稳定版差异：Same absent caller in rc5; product excluded/unimplemented, not a claimed lifecycle regression.
- 验收：(1) Record product decision for each method; if excluded, never claim providers support merely because types compile. (2) If enabled later, test all advertised protocols, complete replacement semantics, errors and credentials not exposed in current config.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 17. `providers/disable` — DisableProviderRequest (request)

- Schema: `v1/agent.rs:4822`; input `v1/agent.rs:3691-3704`; response `v1/agent.rs:3742-3753`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:21`; 编译=true; 状态=`compiled_no_bridge_caller`.
- 输入字段：`providerId: ProviderId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Disables the named provider (providerId); Agent controls consequences and reports error.
- 能力门槛：unstable_llm_providers compiled; Agent advertises agentCapabilities.providers={} (omitted/null means not advertised).
- Bridge: `991b0532:src/bridge.rs:4435–4438`.
- 保留：InitializeResponse can retain advertised providers capability, but no providers state or outgoing command exists.
- 清理：No bridge-created provider resources to clean; request not sent.
- 稳定版差异：Same absent caller in rc5; product excluded/unimplemented, not a claimed lifecycle regression.
- 验收：(1) Record product decision for each method; if excluded, never claim providers support merely because types compile. (2) If enabled later, test all advertised protocols, complete replacement semantics, errors and credentials not exposed in current config.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 18. `nes/start` — StartNesRequest (request)

- Schema: `v1/agent.rs:4923`; input `v1/nes.rs:1391-1415`; response `v1/nes.rs:1580-1593`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`workspaceUri: Option<String>`, `workspaceFolders: Option<Vec<WorkspaceFolder>>`, `repository: Option<NesRepository>`, `_meta: Option<Meta>`.
- 返回字段：`sessionId: SessionId`, `_meta: Option<Meta>`.
- 语义：Starts independent NES session with optional workspaceUri, workspaceFolders(uri,name) and repository(name,owner,remoteUrl); returns sessionId.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 19. `nes/suggest` — SuggestNesRequest (request)

- Schema: `v1/agent.rs:4930`; input `v1/nes.rs:1733-1760`; response `v1/nes.rs:2328-2341`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `version: i64`, `position: Position`, `selection: Option<Range>`, `triggerKind: NesTriggerKind`, `context: Option<NesSuggestContext>`, `_meta: Option<Meta>`.
- 返回字段：`suggestions: Vec<NesSuggestion>`, `_meta: Option<Meta>`.
- 语义：Uses sessionId, document uri/version/cursor, optional selection, automatic/diagnostic/manual trigger and capability-filtered context. Returns edit/jump/rename/searchAndReplace suggestions; empty suggestions is valid.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 20. `nes/close` — CloseNesRequest (request)

- Schema: `v1/agent.rs:4940`; input `v1/nes.rs:1630-1643`; response `v1/nes.rs:1675-1686`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `_meta: Option<Meta>`.
- 返回字段：`_meta: Option<Meta>`.
- 语义：Cancels pending NES work and frees NES session resources.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 21. `document/did_open` — DidOpenDocumentNotification (notification)

- Schema: `v1/agent.rs:5089`; input `v1/nes.rs:1045-1066`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `languageId: String`, `version: i64`, `text: String`, `_meta: Option<Meta>`.
- 语义：Registers full document text with uri, languageId and version under NES session.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 22. `document/did_change` — DidChangeDocumentNotification (notification)

- Schema: `v1/agent.rs:5094`; input `v1/nes.rs:1108-1129`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `version: i64`, `contentChanges: Vec<TextDocumentContentChangeEvent>`, `_meta: Option<Meta>`.
- 语义：Ordered contentChanges; range absent replaces complete document, range present replaces only that range. Document version and negotiated encoding must match. Agent advertises Full or Incremental syncKind.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 23. `document/did_close` — DidCloseDocumentNotification (notification)

- Schema: `v1/agent.rs:5099`; input `v1/nes.rs:1230-1245`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `_meta: Option<Meta>`.
- 语义：Closes the document identified by sessionId/uri.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 24. `document/did_save` — DidSaveDocumentNotification (notification)

- Schema: `v1/agent.rs:5104`; input `v1/nes.rs:1278-1293`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `_meta: Option<Meta>`.
- 语义：Reports document saved (sessionId/uri), no new text payload.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 25. `document/did_focus` — DidFocusDocumentNotification (notification)

- Schema: `v1/agent.rs:5109`; input `v1/nes.rs:1326-1347`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `uri: String`, `version: i64`, `position: Position`, `visibleRange: Range`, `_meta: Option<Meta>`.
- 语义：Reports active document uri/version, cursor position and visibleRange.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 26. `nes/accept` — AcceptNesNotification (notification)

- Schema: `v1/agent.rs:5114`; input `v1/nes.rs:2683-2698`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `id: NesSuggestionId`, `_meta: Option<Meta>`.
- 语义：Reports accepted suggestion by NES sessionId and suggestion id.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 27. `nes/reject` — RejectNesNotification (notification)

- Schema: `v1/agent.rs:5119`; input `v1/nes.rs:2731-2751`.
- SDK: 未注册（NES feature 排除）; 编译=false; 状态=`feature_excluded`.
- 输入字段：`sessionId: SessionId`, `id: NesSuggestionId`, `reason: Option<NesRejectReason>`, `_meta: Option<Meta>`.
- 语义：Reports rejected suggestion by sessionId/id and optional rejected/ignored/replaced/cancelled reason.
- 能力门槛：Schema unstable_nes; NOT enabled by SDK unstable aggregate and no typed registrations in the pinned SDK. Agent/Client NES and position-encoding capabilities also compile out.
- Bridge: 没有 caller/状态路径.
- 保留：No bridge NES session, document version store, suggestions or callers.
- 清理：Not applicable in current product; would require editor-document lifecycle if explicitly added.
- 稳定版差异：Excluded in both baselines; schema existence is not implementation coverage.
- 验收：(1) Keep per-method exclusion explicit. (2) If enabled later: negotiate UTF16/UTF32/UTF8, document sync, offered context fields and suggestion-kind capabilities before emitting any method.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 28. `mcp/message` — MessageMcpRequest (request)

- Schema: `v1/agent.rs:4947`; input `v1/mcp.rs:144-164`; response `v1/mcp.rs:289-291`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:35`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`connectionId: McpConnectionId`, `method: String`, `params: Option<serde_json::Map<String, serde_json::Value>>`, `_meta: Option<Meta>`.
- 返回字段：tuple/raw envelope; see exact type definition.
- 语义：Client side forwards a local MCP server request to Agent as connectionId/method/params; typed result is arbitrary raw JSON. Nested JSON-RPC IDs belong to the MCP connection and are mapped back.
- 能力门槛：unstable_mcp_over_acp compiled; configured ACP server and negotiated agent MCP acp capability.
- Bridge: `991b0532:src/mcp.rs:392–435`.
- 保留：Active MCP connection/process and pending responder routing; no transcript required for ACP fidelity.
- 清理：Connection teardown cancels routes and closes child processes; request result/error relayed to original MCP peer.
- 稳定版差异：Same core MCP relay in stable; missing traffic UI is not missing protocol relay.
- 验收：(1) MCP→Agent request exact method/params; response/error returns with original nested ID; connection cancellation cannot leave waiters.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 29. `mcp/message` — MessageMcpNotification (notification)

- Schema: `v1/agent.rs:5126`; input `v1/mcp.rs:217-239`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:68`; 编译=true; 状态=`implemented_with_limits`.
- 输入字段：`connectionId: McpConnectionId`, `method: String`, `params: Option<serde_json::Map<String, serde_json::Value>>`, `_meta: Option<Meta>`.
- 语义：One-way local MCP server notification forwarded to Agent with connectionId/method/params.
- 能力门槛：Same configured/negotiated MCP-over-ACP boundary.
- Bridge: `991b0532:src/mcp.rs:438–443`.
- 保留：No standalone session timeline entry or request history; service traffic is separate from ACP tool calls.
- 清理：Connection closes with its MCP owner.
- 稳定版差异：Kept in stable and experiment.
- 验收：(1) Forward notifications without response ID; isolate connection IDs and preserve arbitrary params.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 30. `_extension/*` — ExtMethodRequest (request)

- Schema: `v1/agent.rs:4954`; input `v1/ext.rs:25-34`; response `v1/ext.rs:56-58`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:36`; 编译=true; 状态=`generic_sdk_no_bridge_caller`.
- 输入字段：`method: Arc<str>`, `params: Arc<RawValue>`.
- 返回字段：tuple/raw envelope; see exact type definition.
- 语义：Extensibility permits custom method and arbitrary raw params/result; supported method names must use extension namespace. Typed generic support is not a requirement to proxy arbitrary FE commands.
- 能力门槛：Core extension envelope; product has no generic outbound extension command.
- Bridge: `991b0532:src/bridge.rs:4435–4438`.
- 保留：No outbound extension state.
- 清理：None unless a future explicit extension is implemented.
- 稳定版差异：Same product boundary in stable.
- 验收：(1) Document outbound extensions as unsupported unless individually implemented; unknown inbound requests handled separately by dispatch.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

### 31. `_extension/*` — ExtNotification (notification)

- Schema: `v1/agent.rs:5133`; input `v1/ext.rs:77-86`.
- SDK: `754d5aa1:src/agent-client-protocol/src/schema/enum_impls.rs:69`; 编译=true; 状态=`generic_sdk_no_bridge_caller`.
- 输入字段：`method: Arc<str>`, `params: Arc<RawValue>`.
- 语义：One-way extension notification with arbitrary method/params, no response.
- 能力门槛：Core extension envelope; no generic outbound command.
- Bridge: `991b0532:src/bridge.rs:4435–4438`.
- 保留：No outbound extension state.
- 清理：None.
- 稳定版差异：Same product boundary in stable.
- 验收：(1) Do not count generic SDK traits as actual outbound implementation; incoming extension-notification policy is a separate gate.
- Zed：此方法未找到对应 v1 caller / 不在本次引用的 adapter 范围；不据此推断整个 Zed 不支持。

## 能力及嵌套语义分项

以下 **40** 项与方法矩阵分开计数。共享源码索引列出本分册涉及的 **176** 个 schema 类型及逐字段行号（包含 NES context/suggestion 全部结构）。

| ID | Schema | 编译 | 声明/执行与验收 |
| --- | --- | --- | --- |
| `protocolVersion` | `agent.rs:55–164` | True | Exactly v1; no SDK v2 feature. attyd /api/v2 is independent of ACP protocol version. 验收：Reject mismatched negotiated response; keep all initialization data of pinned types. |
| `client.fs.readTextFile` | `client.rs:2415–2435` | True | Advertised only stdio; actual service elsewhere. 验收：Remote receives false; stdio reads with workspace/permission enforcement. |
| `client.fs.writeTextFile` | `client.rs:2415–2435` | True | Advertised stdio and not read-only. 验收：Read-only false; backend refuses even if Agent sends anyway. |
| `client.terminal` | `client.rs:1963–1974` | True | Advertised only stdio. 验收：Remote false and no accidental local execution. |
| `client.session.compaction` | `client.rs:2148–2230` | True | Advertised {}; callback executes but completed timeline currently loses compaction. 验收：Do not claim fidelity until input→retained content case passes. |
| `client.session.configOptions.boolean` | `client.rs:2248–2320` | True | Advertised {}; bool request and current options supported. 验收：false remains boolean; metadata and empty-list semantics covered. |
| `client.plan` | `client.rs:1984–1995` | True | Advertised PlanCapabilities {}; ID operations compile; full retained semantics are target. 验收：Plan create/update/remove direct-memory tests, not only running projection. |
| `client.auth.terminal` | `client.rs:2353–2372` | True | Advertised only stdio. 验收：Agent MUST not advertise terminal method when false; reject such initialize. |
| `client.elicitation.form` | `client.rs:2003–2011; elicitation.rs` | True | Advertised {}; validator accepts only a restricted pattern subset (shared gap). 验收：Primitive/enums/arrays/defaults plus ordinary grouped-repeat regex; declare any remaining subset. |
| `client.elicitation.url` | `client.rs:2003–2011; elicitation.rs` | True | Advertised {}; consent/complete implemented; retained outcome pending target. 验收：Consent vs complete vs cancelled scope; not a persistent finished table. |
| `client.nes` | `client.rs:2012–2024` | False | Not compiled or advertised. 验收：Explicit product exclusion; no claim all schema features supported. |
| `client.positionEncodings` | `client.rs:2025–2034` | False | Not compiled; NES excluded. 验收：If added negotiate code units, do not assume byte offsets. |
| `agent.loadSession` | `agent.rs:3790–3794` | True | Gates actual session/load; load preferred over resume for subscription. 验收：Absent false; load response with empty replay is authoritative empty. |
| `agent.sessionCapabilities.list` | `agent.rs:4023` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.sessionCapabilities.delete` | `agent.rs:4031` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.sessionCapabilities.additionalDirectories` | `agent.rs:4044` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.sessionCapabilities.fork` | `agent.rs:4057` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.sessionCapabilities.resume` | `agent.rs:4065` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.sessionCapabilities.close` | `agent.rs:4073` | True | Presence gates method/configuration; fork is compiled unstable. Additional directories configured globally and only sent for local transport. 验收：Test absence/null versus {} and resulting wire send/refusal; no method allowed solely by schema availability. |
| `agent.promptCapabilities.image` | `agent.rs:4446` | True | Optional content validated in bridge.rs:4985–5012; text/resource_link are mandatory baseline. 验收：Both no-capability refusal and capability acceptance preserve original block. |
| `agent.promptCapabilities.audio` | `agent.rs:4451` | True | Optional content validated in bridge.rs:4985–5012; text/resource_link are mandatory baseline. 验收：Both no-capability refusal and capability acceptance preserve original block. |
| `agent.promptCapabilities.embeddedContext` | `agent.rs:4459` | True | Optional content validated in bridge.rs:4985–5012; text/resource_link are mandatory baseline. 验收：Both no-capability refusal and capability acceptance preserve original block. |
| `agent.mcpCapabilities.http` | `agent.rs:4527` | True | Validate configured server transports at initialization; stdio baseline requires no boolean. 验收：Unsupported configured transport fails explicitly before setup. |
| `agent.mcpCapabilities.sse` | `agent.rs:4532` | True | Validate configured server transports at initialization; stdio baseline requires no boolean. 验收：Unsupported configured transport fails explicitly before setup. |
| `agent.mcpCapabilities.acp` | `agent.rs:4542` | True | Validate configured server transports at initialization; stdio baseline requires no boolean. 验收：Unsupported configured transport fails explicitly before setup. |
| `agent.auth.logout` | `agent.rs:464–482` | True | Presence gates logout; method name logout. 验收：New sessions need authentication after success; do not assert old session lifecycle. |
| `agent.providers` | `agent.rs:3815–3827` | True | Capability survives typed initialize but 3 provider methods have no project caller. 验收：Explicitly mark optional capability unused, not supported. |
| `agent.nes` | `agent.rs:3828–3840` | False | Not compiled. 验收：All 3 requests and 7 notifications individually excluded. |
| `agent.positionEncoding` | `agent.rs:3841–3850` | False | Not compiled. 验收：Not confused with terminal or UTF8 message text. |
| `AuthMethod.Agent` | `agent.rs:575–656` | True | id/name/description/_meta; absent type defaults to agent; offered ID sent authenticate. 验收：Opaque ID preserved; no terminal call; malformed duplicate IDs rejected. |
| `AuthMethod.Terminal` | `agent.rs:690–733` | True | id/name/description/args/env/_meta; configured executable + base args + method args; env overrides; zero exit success, other termination failure; local only. 验收：No authenticate RPC. Preserve methodId throughout input/resize/cancel/signal/end; success triggers new connection, failure does not. |
| `setup.McpServer.Stdio` | `agent.rs:2866` | True | MCP config loader supplies typed descriptor to new/load/resume/fork. Transport capability gate for Http/Sse/Acp; descriptors carry type-specific args/env/headers/serverId and optional _meta. 验收：Round-trip chosen configured descriptor without translating into tool events; credential/header lifecycle belongs to configuration/service. |
| `setup.McpServer.Http` | `agent.rs:2667` | True | MCP config loader supplies typed descriptor to new/load/resume/fork. Transport capability gate for Http/Sse/Acp; descriptors carry type-specific args/env/headers/serverId and optional _meta. 验收：Round-trip chosen configured descriptor without translating into tool events; credential/header lifecycle belongs to configuration/service. |
| `setup.McpServer.Sse` | `agent.rs:2724` | True | MCP config loader supplies typed descriptor to new/load/resume/fork. Transport capability gate for Http/Sse/Acp; descriptors carry type-specific args/env/headers/serverId and optional _meta. 验收：Round-trip chosen configured descriptor without translating into tool events; credential/header lifecycle belongs to configuration/service. |
| `setup.McpServer.Acp` | `agent.rs:2815` | True | MCP config loader supplies typed descriptor to new/load/resume/fork. Transport capability gate for Http/Sse/Acp; descriptors carry type-specific args/env/headers/serverId and optional _meta. 验收：Round-trip chosen configured descriptor without translating into tool events; credential/header lifecycle belongs to configuration/service. |
| `setup.additionalDirectories` | `agent.rs:799–807,965–974,1294–1303` | True | Configured local absolute roots shared across setup requests; options normalizes/deduplicates; remote options reject local roots. This is a product parameter-source policy, not all per-request schema options exposed. 验收：New/load/resume/fork carry full replacement roots, relative paths resolve against cwd, permission containment uses all roots. |
| `controls.configKinds` | `agent.rs:2102–2597` | True | Select grouped/ungrouped and Boolean; category mode/model/model_config/thought_level/Other; no separate pinned set_model method. 验收：Exact nested metadata; full replacement acknowledgement; prefer configOptions when present including []. |
| `prompt.StopReason` | `agent.rs:3164–3182` | True | Five variants parsed; current op keeps result temporarily; target stores whole response in Turn.end. 验收：Test each variant and refusal semantics; cancelled does not erase content. |
| `prompt.Usage` | `agent.rs:3197–3228` | True | unstable_end_turn_token_usage enabled; total/input/output + optional thought/cache read/cache write + _meta. PromptResponse.usage says this turn (3106), while Usage fields say across session/all turns (3199–3203). Preserve unchanged; counter aggregation semantics remain ambiguous. 验收：All counters/optional-presence/metadata retained exactly; distinguish this from session usage_update used/size/cost. |
| `metadata` | `agent.rs and every included type` | True | Request envelope _meta supported by types but bridge constructs its own requests without custom FE envelope metadata. Initialize fully retained; setup outer response meta discarded in both baselines; modes/notifications regression D12; catalog/current-result wrappers also narrow fields. 验收：State an exact retention policy per container; never interpret vendor keys or add an all-RPC event archive merely to preserve displayable state. |

## 终端认证：非 authenticate RPC 的必需变体链

- Schema `agent.rs:690–733`：本地启动已配置的 Agent 程序，追加 method.args、覆盖 method.env；exit 0 才成功，其余终止失败。不得将 terminal methodId 发到 `authenticate`。
- Bridge `src/bridge.rs:4371–4433`，`src/auth_terminal.rs:72–130,132–198,258–299`；运行中 methodId、输出、exitCode/signal、取消由 registry `src/protocol/registry.rs:528–589` 承载。成功认证触发新连接 `src/server.rs:371–395`；连接 teardown 的 close-on-drop 终止登录进程。
- 验收已有位置：`auth_terminal::tests::appends_agent_arguments_and_environment_to_base_invocation`、`reports_cancellation_and_rejects_stale_input`、`reports_a_signalled_login_without_an_exit_code`、`owner_unwind_closes_authentication_and_allows_runtime_shutdown`。本轮未运行它们，不能把静态定位写成执行通过。
- r2 资源规则应保证 input/resize/cancel/dismiss 生命周期与实际进程一致；不要求保存每次登录的历史记录。

## 最小验收入口与未决

- Fork complete/partial derivation and early unknown-session attribution are specified by [memory model §6.3 and §9](../bridge-memory-model.md#9-concurrency); they are not yet implemented or covered.
- D12 source-level controls metadata needs a concrete representation; no new raw event log is implied.
- Current response/page metadata retention beyond displayable session state must be explicit; losing wrapper _meta is observed, not automatically an ACP wire violation.
- No-session-only Agent reconnect needs an explicit recovery path (server.rs:222–237); do not resend unknown-outcome business commands.
- This inventory statically locates tests and acceptance needs; it does not claim every acceptance was executed in this turn.

## 版本边界

- SDK unstable enables 7 forwarded features, not unstable_nes or unstable_protocol_v2. Schema own unstable aggregate differs; actual resolved Cargo features are authoritative.
- Uncompiled ACP v2 uses auth/login and auth/logout instead of authenticate/logout; removes v1 load/mode entry points from its ClientRequest registry, retains resume/config. This is not a request to migrate attyd.
- Zed 9f63850 has SDK3.4/schema1.12. Compare only v1 adapter semantics compatible with pinned 1.7; notices and v2 receipt/state APIs are outside this inventory.
- Zed contains vendor-specific Cursor/Gemini/legacy metadata adapters. These are explicitly not requirements for generic attyd.

## 证据边界

- 全部 attyd 生产代码引用以 `991b053` 或明确写出的 `60f8606` 为准；工作区现有目标文档及未合入 PR 不作为实现通过证据。
- 代码审读及源枚举机械闭合完成；没有运行新集成测试，没有提交、推送或修改仓库文件。
- 共享 source-inventory.json 的字段、变体、serde 属性与固定源码链接用于防止遗漏 optional、`_meta`、flatten/untagged、DefaultOnError 和 feature gate；不能把宽松反序列化丢弃的未来字段宣称为项目原样保存。

## 上游 1.12.0 / SDK 3.4.0 差异（不是当前支持）

- 源码：schema VCS `e2d83349bc9e28a4059ea3acefa1c91c966f74e2`；SDK `d5e8bf4d17eafed81b2185acd4bd366e88a33dd1`。此表独立于 1.7 主矩阵。
- ClientRequest 枚举 17 项、ClientNotification 3 项。MCP typed request 仍注册，但方向已收窄；不能仅凭注册推断旧双向 relay 仍存在。

| 差异项 | 原始位置（1.12 / 新 SDK） | 旧 → 新 | 本项目边界 |
| --- | --- | --- | --- |
| `system_prompt.new` | `v1/agent.rs:815–826` | No systemPrompt field. → NewSessionRequest.systemPrompt optional; absent/null uses default instructions. | Absent from current schema/bridge; future opt-in only, never inject as user prompt. |
| `system_prompt.load` | `v1/agent.rs:1004–1017` | No systemPrompt field. → LoadSessionRequest.systemPrompt optional; absent/null may retain stored instructions/default; SHOULD send same as creation. | Requires explicit feature/capability gate and per-session policy if adopted. |
| `system_prompt.resume` | `v1/agent.rs:1390–1403` | No systemPrompt field. → ResumeSessionRequest.systemPrompt optional; same creation value SHOULD be supplied when used. | Not a current supported setup field. |
| `system_prompt.fork` | `v1/agent.rs:1193–1206` | No systemPrompt field. → ForkSessionRequest.systemPrompt optional; absent/null may inherit source/default; SHOULD send source value. | Only after optional unstable_session_system_prompt and fork rules are chosen. |
| `system_prompt.capability` | `v1/agent.rs:4247–4259`; `v1/agent.rs:4545–4562`; `v1/agent.rs:4609–4628` | No systemPrompt capability. → agent.sessionCapabilities.systemPrompt={} supports append; mode.override={} additionally permits override; omitted/null means unavailable; each container _meta. | No false claim from latest SDK alone; caller must honor capability. |
| `system_prompt.shape` | `v1/agent.rs:3148–3186`; `v1/agent.rs:3237–3261` | No SystemPrompt/SystemPromptMode. → Required wire mode append/override/Other plus content[] and _meta; constructor default append does not make wire mode optional. Text-only now; unknown/unsupported mode/content must error, never silently fall back. | Future separate product decision; no vendor keys or custom message encoding needed. |
| `subagents.capability` | `v1/client.rs:3261–3275`; `v1/client.rs:3402–3426` | No subagents capability; unrelated NES capability exists only under excluded feature. → unstable_subagents: client.subagents={} means child associations, work-state snapshots, session-directed messages, restricted child sessions. | Current schema cannot advertise or retain these typed variants; optional future upgrade, not proof current main violates pinned contract. |
| `subagents.lifecycle` | `v1/client.rs:896–948`; `v1/client.rs:1012–1038`; `v1/client.rs:158–180` | Ordinary session mutation capabilities only. → SubagentUpdate has child sessionId,title,description,capabilities,state,_meta, with absent/clear/set patches; child mutations require per-child explicit capability (currently cancel), ordinary session support grants no mutation right. SessionMessage/SessionMessageChunk are new update variants. | Do not expose prompt/close/config for a child merely because global Agent supports them; delegated detailed update inventory. |
| `session.notices` | `v1/client.rs:143–147`; `v1/client.rs:647–681`; `v1/client.rs:3469–3476` | No notice type/capability in 1.7. → Stable client.session.notices={} permits live Notice(severity,title,description,_meta); notices are explicitly NOT history, and receipt/display is not guaranteed. | If upgrading, unconditional all-non-control→timeline rule must exempt notices by their own semantics; currently no support claim. |
| `compaction.stable` | `v1/client.rs:148–157`; `v1/client.rs:3455–3460`; `Cargo.toml:59–75` | unstable_session_compaction flag is required/enabled. → Compaction types and session.compaction are no longer feature gated; capability presence still required. | Current compiled feature already supports wire path; upgrading does not by itself fix retained compaction content. |
| `tool_name.stable` | `v1/tool_call.rs:35–41`; `v1/tool_call.rs:284`; `Cargo.toml:59–75` | unstable_tool_call_name flag required/enabled. → ToolCall/ToolCallUpdate name no longer feature gated, still optional. | Current runtime already compiles name; retention checks remain needed. |
| `nes.removed` | `v1/agent.rs:5144–5293`; `v1/agent.rs:5390–5429`; `v1/mod.rs`; `Cargo.toml:59–75` | 3 requests +7 document/NES notifications and capability/position encodings exist under unstable_nes (not enabled). → NES module, feature, method variants, and Agent/Client NES/position-encoding fields removed. | Not a new migration requirement. Keep old rows excluded and do not implement removed draft simply to claim all schema features. |
| `mcp.enum_split` | `v1/agent.rs:5144–5293`; `SDK src/agent-client-protocol/src/schema/mcp.rs:3–6` | MessageMcpRequest/Response members of ClientRequest/AgentResponse. → Reverse Client→Agent request and Agent→Client notification removed; retained request is Agent→Client and notification Client→Agent, now serverId/requestId scoped. SDK typed registration remains for that changed contract. | Typed SDK registration does not imply old bidirectionality. MCP remains but reverse request is removed; see service volume for lifecycle/result carrier changes. |
| `default_on_null` | `v1/agent.rs:315,359,403,1083,1469,1595,1639,1813,2122,3704,3879,3978` | Structs require an object in ordinary typed deserialization. → default_on_null wrapper now accepts null as default for AuthenticateResponse,LogoutRequest,LogoutResponse,LoadSessionResponse,ResumeSessionResponse,CloseSessionResponse,ListSessionsRequest,DeleteSessionResponse,SetSessionModeResponse,ListProvidersRequest,SetProviderResponse,DisableProviderResponse. | Potential interoperability improvement on upgrade; not declare current behavior equal without decoder test; does not require accepting malformed required payloads. |
| `unstable.aggregate` | `SDK Cargo.toml:53–69`; `schema Cargo.toml:59–75` | 7 SDK unstable flags: usage,providers,MCP,plan,compaction,fork,tool_name; NES not in SDK aggregate. → Still 7 SDK unstable flags, but compaction/tool_name removed (stable), system_prompt/subagents added. unstable_protocol_v2 remains separately gated. | Enabling same feature name after upgrade changes exposed schema; must audit negotiated capability and reduction before advertising new support. |

## 277 个 schema 声明的归属闭合

- schema `src/v1` 独立扫描：**277** 个 `pub struct/enum`，名称无重复。
- 本分册覆盖 **176** 个；每个在共享 JSON `pinned_types` 中附 `matrix`、字段、变体、行号和固定源码链接。
- 其中 4 个组合容器：ClientRequest, AgentResponse, ClientNotification, AgentMethodNames；不是另加 RPC。
- 尚未归本分册的 **101** 个在 [源码闭合索引](schema-index.md) 中逐名列出，供其它分册合并；不将其标成不支持。
- 本分册列出的既有测试只是静态证据；验收要求仍需确认/补齐，不冒充本轮已运行。

## 未启用 ACP draft v2 与 SDK helpers 的显式边界

以下仅盘点 pinned SDK `schema/v2_impls.rs` 的注册，不逐字段审计另一版协议；全部因 `unstable_protocol_v2` 未启用而排除。同名方法也不能套用 v1 字段。

| v2 注册集合 | 数量（含扩展） | 方法 | 源码 |
| --- | --- | --- | --- |
| `ProtocolLevelNotification` | 1 | `$/cancel_request` | `schema/v2_impls.rs:296` |
| `ClientRequest` | 16 | `initialize`, `auth/login`, `auth/logout`, `providers/list`, `providers/set`, `providers/disable`, `session/new`, `session/list`, `session/delete`, `session/fork`, `session/resume`, `session/close`, `session/set_config_option`, `session/prompt`, `mcp/message`, `_<extension>` | `schema/v2_impls.rs:300` |
| `AgentResponse` | 16 | `initialize`, `auth/login`, `auth/logout`, `providers/list`, `providers/set`, `providers/disable`, `session/new`, `session/list`, `session/delete`, `session/fork`, `session/resume`, `session/close`, `session/set_config_option`, `session/prompt`, `mcp/message`, `_<extension>` | `schema/v2_impls.rs:324` |
| `ClientNotification` | 3 | `session/cancel`, `mcp/message`, `_<extension>` | `schema/v2_impls.rs:348` |
| `AgentRequest` | 6 | `session/request_permission`, `elicitation/create`, `mcp/connect`, `mcp/message`, `mcp/disconnect`, `_<extension>` | `schema/v2_impls.rs:355` |
| `ClientResponse` | 6 | `session/request_permission`, `elicitation/create`, `mcp/connect`, `mcp/message`, `mcp/disconnect`, `_<extension>` | `schema/v2_impls.rs:367` |
| `AgentNotification` | 4 | `session/update`, `elicitation/complete`, `mcp/message`, `_<extension>` | `schema/v2_impls.rs:379` |

- 请求使用 `auth/login`、`auth/logout`；不含 v1 `session/load`、`session/set_mode`。此 pinned v2 AgentRequest 没有 v1 fs/terminal 服务；不能将 draft 的缩减反推到当前 v1。
- 上表 response 集合是对应结果容器，不增加 RPC；protocol cancel 是独立于两个业务 notification 集合的共同通知。
- `_proxy/initialize` 属于 SDK 自定义代理链（`role/acp.rs:1459,1524`），不是 attyd 作为普通 ACP client 必须实现的标准方法。

| SDK 角色/辅助 API | 当前边界 | 源码 |
| --- | --- | --- |
| `Client`, `Agent` | v1 Client builder is the actual attyd role; Agent is peer, not an Agent implementation obligation. | `SDK role/acp.rs:43,291` |
| `Proxy`, `Conductor`, `UntypedRole` | SDK relay/composition roles, not additional ACP client services; attyd does not implement conductor or proxy chains. | `SDK role/acp.rs:1299,1501; role.rs:271` |
| `V2Builder`, `V2ConnectionContext`, `V2ConnectionTo` | unstable_protocol_v2 gated and absent from actual build. | `SDK lib.rs:133–134` |
| `ClientProtocolConnector`, `AgentProtocolRouter`, `ProxyProtocolRouter` | Version-routing helpers behind unstable_protocol_v2; not automatic fallback in current attyd. | `SDK role/acp.rs:129,378,1374; lib.rs:136–137` |
| `V2SessionBuilder`, `V2ForkSessionBuilder`, `V2ResumeSessionBuilder`, `OpenedV2Session`, `V2Session` | Uncompiled session/MCP attachment convenience helpers. Methods new/fork/resume,prompt/cancel/config/close reuse v2 wire methods, no new client capability. | `SDK session/v2.rs:229,394,562,726,766` |
| `SessionBuilder`, `ActiveSession`, `MetaCapability`, `MetaCapabilityExt` | v1 helper wrappers and typed metadata access, not extra wire methods. attyd uses direct requests and its own ordering/registry; helper existence is not its implementation proof. | `SDK session.rs:50–76,442,570–637; capabilities.rs:31,42` |

## 上游声明集合复核

- 1.12.0 有 **240** 个 `pub struct/enum`：224 个顶层声明加 16 个 `default_on_null!` 内声明；这些 wrapper 不算删除。
- 相对 1.7.0 的 277 个声明，新增 **25** 个、删除 **62** 个（NES 57 个、旧 MCP 连接型 5 个），总数 `277 + 25 − 62 = 240`。
- 共享 source-inventory.json 的 `upstream_types` 含全部源码位置和字段，`declaration_delta` 逐名列出新增、删除及 16 个 wrapper。其它分册负责类型仅做集合索引，不冒充在本分册完成语义审计。
