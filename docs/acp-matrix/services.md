# ACP Agent→Client services and request-lifecycle inventory

[总矩阵与版本基线](../acp-bridge-matrix.md) · [源码闭合索引](schema-index.md)


Source audit of experimental bridge `991b053` versus stable rc5 `60f8606` (`488ccd9` plus rustls), pinned Rust SDK `754d5aa1` / schema `1.7.0` module `v1` with `unstable`. Zed reference is fixed at `9f63850f7affbd57772a4f99554d4fd333b50a6f`. This documents source behavior and acceptance requirements; no runtime tests were newly run.

## 1. Mechanical completeness

The schema has **13 AgentRequest variants: 12 concrete services + ExtMethodRequest**. All 12 have SDK registrations and bridge handlers. AgentNotification has **4 variants**: session/update is delegated to the content inventory; this file covers elicitation/complete, mcp/message and ExtNotification. ProtocolLevelNotification has **1 variant**, $/cancel_request. There is no extra Agent→Client NES/document/provider request in this pinned AgentRequest enum.

| Enum | Count | Variants | Source |
| --- | --- | --- | --- |
| AgentRequest | 13 | WriteTextFileRequest; ReadTextFileRequest; RequestPermissionRequest; CreateTerminalRequest; TerminalOutputRequest; ReleaseTerminalRequest; WaitForTerminalExitRequest; KillTerminalRequest; CreateElicitationRequest; ConnectMcpRequest; MessageMcpRequest; DisconnectMcpRequest; ExtMethodRequest | `v1/client.rs:2568` |
| ClientResponse | 13 | WriteTextFileResponse; ReadTextFileResponse; RequestPermissionResponse; CreateTerminalResponse; TerminalOutputResponse; ReleaseTerminalResponse; WaitForTerminalExitResponse; KillTerminalResponse; CreateElicitationResponse; ConnectMcpResponse; DisconnectMcpResponse; MessageMcpResponse; ExtMethodResponse | `v1/client.rs:2717` |
| AgentNotification | 4 | SessionNotification; CompleteElicitationNotification; MessageMcpNotification; ExtNotification | `v1/client.rs:2761` |
| ProtocolLevelNotification | 1 | CancelRequestNotification | `v1/protocol_level.rs:91` |
| PermissionOptionKind | 4 | AllowOnce; AllowAlways; RejectOnce; RejectAlways | `v1/client.rs:987` |
| RequestPermissionOutcome | 2 | Cancelled; Selected | `v1/client.rs:1050` |
| StringFormat | 4 | Email; Uri; Date; DateTime | `v1/elicitation.rs:42` |
| ElicitationPropertySchema | 6 | String; Number; Integer; Boolean; Array; Other | `v1/elicitation.rs:968` |
| MultiSelectItems | 3 | String; Other; Titled | `v1/elicitation.rs:815` |
| ElicitationMode | 3 | Form; Url; Other | `v1/elicitation.rs:1640` |
| ElicitationAction | 4 | Accept; Decline; Cancel; Other | `v1/elicitation.rs:1914` |
| ElicitationContentValue | 5 | String; Integer; Number; Boolean; StringArray | `v1/elicitation.rs:2072` |
| ElicitationScope | 2 | Session; Request | `v1/elicitation.rs:1489` |

SDK registration cross-check:

| Kind | Method | Request/notification → response | SDK source |
| --- | --- | --- | --- |
| request | `session/request_permission` | RequestPermissionRequest → RequestPermissionResponse | `schema/agent_to_client/requests.rs:10` |
| request | `fs/write_text_file` | WriteTextFileRequest → WriteTextFileResponse | `schema/agent_to_client/requests.rs:15` |
| request | `fs/read_text_file` | ReadTextFileRequest → ReadTextFileResponse | `schema/agent_to_client/requests.rs:20` |
| request | `terminal/create` | CreateTerminalRequest → CreateTerminalResponse | `schema/agent_to_client/requests.rs:25` |
| request | `terminal/output` | TerminalOutputRequest → TerminalOutputResponse | `schema/agent_to_client/requests.rs:30` |
| request | `terminal/release` | ReleaseTerminalRequest → ReleaseTerminalResponse | `schema/agent_to_client/requests.rs:35` |
| request | `terminal/wait_for_exit` | WaitForTerminalExitRequest → WaitForTerminalExitResponse | `schema/agent_to_client/requests.rs:40` |
| request | `terminal/kill` | KillTerminalRequest → KillTerminalResponse | `schema/agent_to_client/requests.rs:45` |
| request | `elicitation/create` | CreateElicitationRequest → CreateElicitationResponse | `schema/agent_to_client/requests.rs:46` |
| notification | `session/update` | SessionNotification → — | `schema/agent_to_client/notifications.rs:4` |
| notification | `elicitation/complete` | CompleteElicitationNotification → — | `schema/agent_to_client/notifications.rs:5` |
| request | `mcp/connect` | ConnectMcpRequest → ConnectMcpResponse | `schema/mcp.rs:8` |
| request | `mcp/message` | MessageMcpRequest → MessageMcpResponse | `schema/mcp.rs:9` |
| notification | `mcp/message` | MessageMcpNotification → — | `schema/mcp.rs:10` |
| request | `mcp/disconnect` | DisconnectMcpRequest → DisconnectMcpResponse | `schema/mcp.rs:11` |
| notification | `$/cancel_request` | CancelRequestNotification → — | `schema/protocol_level.rs:5` |

Capability declaration cross-check (same service declaration as rc5):

| ID | Field | Current bridge | Source |
| --- | --- | --- | --- |
| CAP01 | `ClientCapabilities.fs.readTextFile/writeTextFile` | stdio: read=true, write=!read_only; remote: false | `src/bridge.rs:2219–2225` |
| CAP02 | `ClientCapabilities.terminal` | stdio true; remote false; all five terminal methods implemented | `src/bridge.rs:2226` |
| CAP03 | `ClientCapabilities.auth.terminal` | stdio true; remote false; authentication execution covered in [lifecycle](lifecycle.md) | `src/bridge.rs:2236` |
| CAP04 | `ClientCapabilities.elicitation.form/url` | both {} advertised on every transport; known form and URL methods exist, limitations F03/F04/F07/F10 apply | `src/bridge.rs:2237–2241` |
| CAP05 | `ClientCapabilities.session.compaction/configOptions.boolean and plan` | {} advertised; content/control semantics inventoried elsewhere | `src/bridge.rs:2227–2235` |
| CAP06 | `ClientCapabilities.nes/positionEncodings/_meta` | not advertised (None/empty/None); no NES or document functionality claimed | `src/bridge.rs:2219–2242` |
| CAP07 | `AgentCapabilities.mcpCapabilities.acp + McpServer::Acp` | Agent support gate before supplying configured ACP server; not a ClientCapabilities.mcp flag; http/sse server configs separately gated, stdio baseline | `src/bridge.rs:5637–5650` |

## 2. Execution, memory and lifetime matrix

Status is deliberately split: having a handler is not proof of complete semantic/state retention. All schema field definitions and nested variants are exhaustively listed in §5 and the shared [source-inventory.json](source-inventory.json) `pinned_types` map. Source paths below are at the fixed bridge/schema versions.

### S01 `session/request_permission` (request)

- **Types / negotiation:** RequestPermissionRequest, PermissionOption, PermissionOptionKind, RequestPermissionResponse, RequestPermissionOutcome, SelectedPermissionOutcome, ToolCallUpdate; core; no separate client capability.
- **Entry / status:** `src/bridge/agent_dispatch.rs:342–431; src/semantic.rs:399–436`; **implemented_with_lifecycle_and_answer_gaps**.
- **Memory:** Full serialized request in SessionLiveState.permissions; responder/options/toolCallId in SessionResources.permissions (src/session_resources.rs:67–71); currently joins running operation (agent_dispatch:381–383).
- **Release:** Answer/cancel consumes original responder; scope/connection cleanup answers cancelled; currently operation end also cancels (src/bridge/publication.rs:234–292).
- **rc5 comparison:** rc5 permits individual responses; experiment requires all pending answers (src/protocol/admission.rs:183–207) and adds operation lifetime coupling.
- **Required behavior:** Preserve all option kinds/IDs/names/metadata; return only an offered option or cancelled; independent at-most-once answer; carry permission tool patch into retained timeline. Prompt cancel MUST return cancelled.
- **Direct acceptance:** Send all four permission kinds; select each ID and inspect ACP result. Two requests: answer one, assert the other remains; duplicate answer sends no second reply. Permission tool patch without earlier tool notification remains after answer and prompt end. Prompt cancel and wire cancel settle original request, not reused request ID.
- **Zed comparison:** v1.rs:9320–9381 independent request_tool_call_authorization_with_id; cancellation scoped to request. No local arbitrary ACP extension semantics.

### S02 `elicitation/create` (request)

- **Types / negotiation:** CreateElicitationRequest, ElicitationMode, ElicitationFormMode, ElicitationUrlMode, ElicitationScope, ElicitationSessionScope, ElicitationRequestScope, CreateElicitationResponse, ElicitationAction, ElicitationAcceptAction, ElicitationContentValue; clientCapabilities.elicitation.form/url present on every transport.
- **Entry / status:** `src/bridge/agent_dispatch.rs:460–558; src/elicitation_validation.rs:12–211`; **implemented_with_known_subset_and_lifecycle_gaps**.
- **Memory:** Complete serialized request and sender in session resources or BridgeState.request_elicitations; SessionRegistry request/session pending maps (agent_dispatch:519–533). Request scope uses pending RPC scope captured by scheduling.rs:616–627, then joins an operation.
- **Release:** Independent ACP responder exists, but experiment group answers and orphan-op cancellation narrow lifetime; session/connection cleanup cancels. Accepted URL becomes separate flow.
- **rc5 comparison:** Shared: regex subset, HTTP(S) URL policy, unknown-session rejection, prompt cancel also cancels forms. New: group-only answers and arbitrary op coupling. rc5 also lacks real parent-RPC lifecycle.
- **Required behavior:** Form schema retained with all known fields/metadata, unknown schema preserved without pretending known semantics; request scope parent actual outstanding RPC, not op; URL acceptance separate from completion. Unknown installation association remains unresolved.
- **Direct acceptance:** Valid form of every known property/schema variant reaches memory unchanged, valid answers return ACP shape; invalid answer leaves pending responder. Session form survives unrelated control RPC result; parent-scoped form cancels on actual parent response. Unknown/stale/null parent distinction explicit; toolCallId retained even without visible tool update. Reject unknown mode explicitly; preserve unknown property schema, permit decline/cancel without interpreting it.
- **Zed comparison:** v1.rs:9383–9398 converts with acp_thread/elicitation.rs:5–58 to shared handler acp.rs:669–772. v1 ClientContext has active_request_scopes=None (:533); shared optional active-parent check is NOT proof of v1 enforcement.

### S03 `elicitation/complete` (notification)

- **Types / negotiation:** CompleteElicitationNotification; elicitation.url.
- **Entry / status:** `src/bridge/agent_dispatch.rs:561–600; src/session_registry.rs:102–142`; **handled_shared_terminal_state_gap**.
- **Memory:** Accepted flow request/registration/elicitationId in session.url_flows or request_url_flows; route captures registration identity to distinguish reused Agent ID.
- **Release:** Current settle_url_flow removes waiting flow and remembers resolved ID (src/runtime_state.rs:1112–1162); terminal outcome itself is absent.
- **rc5 comparison:** rc5 runtime_state.rs:1597–1623 also removes map entry; not an experiment deletion of a prior finished-flow table.
- **Required behavior:** Keep waiting/completed/ended on the same resource until dismissal/scope release; parent RPC response does not end accepted flow; only waiting changes. Notification metadata is parsed; no raw RPC history required.
- **Direct acceptance:** Accept, end parent RPC, complete later: flow becomes completed; connection end does not overwrite completed. Dismiss then late completion does not recreate flow. Reuse elicitationId: old captured completion must not complete replacement registration; unknown notification never poisons connection.
- **Zed comparison:** v1.rs:9401–9413 forwards to shared completion; acp.rs:774–796 routes by elicitationId; acp_thread.rs:610–645 terminal monotonicity. v1 does not call parent finish_request; shared store finish/cancel APIs are used by v2, not evidence for v1 parent tracking.

### S04 `fs/read_text_file` (request)

- **Types / negotiation:** ReadTextFileRequest, ReadTextFileResponse; fs.readTextFile=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:603–620 → src/filesystem.rs:66–104`; **implemented_with_declared_host_policy**.
- **Memory:** Request task owns file and bytes; workspace derived from actual session cwd plus configured roots. No file-read transcript stored.
- **Release:** Return content or typed error; cancellation drops read future; connection/scope errors fail request.
- **rc5 comparison:** No ACP read implementation regression; changed context-search DTOs are a separate API.
- **Required behavior:** Absolute path, 1-based optional line, optional limit; UTF-8 content, correct line endings; regular file and root confinement. Optional envelope _meta is not interpreted or echoed by default.
- **Direct acceptance:** Read whole/range with line=1, limit=0, CRLF/empty lines/UTF-8; line=0 and escaping path reject. Cancel pending read: original ACP request ends, no leaked task; no automatic timeline entry.
- **Zed comparison:** v1.rs:9451–9484 delegates session thread read_text_file(path,line,limit); editor-backed storage differs from standalone bridge filesystem.

### S05 `fs/write_text_file` (request)

- **Types / negotiation:** WriteTextFileRequest, WriteTextFileResponse; fs.writeTextFile=true only stdio and !read_only.
- **Entry / status:** `src/bridge/agent_dispatch.rs:622–638 → src/filesystem.rs:115–186`; **implemented_with_declared_host_policy**.
- **Memory:** Transient request/path/content and file handle; no historical RPC or file snapshot unless Agent publishes session content.
- **Release:** Before mutation cancellation aborts; after in-place mutation starts, finishes or reports structured write failure to avoid cancellation leaving a truncated write.
- **rc5 comparison:** Same ACP write behavior as rc5.
- **Required behavior:** Write exact supplied content in authorized roots; retain error path/stage/possible partial-change information. Do not advertise remote/read-only writes.
- **Direct acceptance:** Write existing/new file and verify bytes; deny read-only/root escape/dangling symlink. Cancellation before mutation leaves file unchanged; write failure has original-request error including partial-change flag.
- **Zed comparison:** v1.rs:9416–9449 delegates thread write_text_file and replies after task; do not copy Zed editor/worktree architecture.

### Common terminal state (S06–S10)

The five methods below share these ownership and retention rules; their individual
entry points, replies and cancellation behavior remain separate.

- **Memory:** TerminalManager.terminals keyed by UUID; process/readers/output buffer; SessionLiveState.terminals canonical snapshots. EnvVariable has name/value/_meta; envelope metadata is not interpreted.
- **rc5 comparison:** All five execution methods retained. rc5 caches released referenced output in session_mirror.rs:430–478; experiment removes that cache. Old-allocation callback guard removed, reachability remains to verify.
- **Required behavior:** Do not conflate physical process, ACP handle and embedded display result. outputByteLimit absent means no fixed application cap. Session resources cleaned on scope/connection end. Unknown-session creation association remains a boundary, not a guessed owner.
- **Zed comparison:** v1.rs:9650–9845 handlers; acp_thread.rs:2998–3002 embeds Entity<Terminal>, :7596–7608 removes handle map entry. Embedded entity can outlive handle lookup.

### S06 `terminal/create` (request)

- **Types / negotiation:** CreateTerminalRequest, CreateTerminalResponse, EnvVariable, TerminalExitStatus; clientCapabilities.terminal=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:641; src/terminal.rs:122–167,482–562`; **implemented**.
- **Release:** Register before successful ACP reply; create snapshot may already contain output.
- **Direct acceptance:** command,args,env,cwd honored; literal args not shell interpolated; outputByteLimit exercised at UTF-8 boundaries. Created terminal is in canonical state before Agent can send tool reference; terminal creation failure leaks no handle.

### S07 `terminal/output` (request)

- **Types / negotiation:** TerminalOutputRequest, TerminalOutputResponse, TerminalExitStatus; clientCapabilities.terminal=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:695; src/terminal.rs:170–180,457–479`; **implemented**.
- **Release:** Handle remains live; current text/truncated/exitStatus returned, no mutation.
- **Direct acceptance:** Before/after exit returns correct output and status; byte limit truncates front at character boundary; multibyte partial read not duplicated.

### S08 `terminal/wait_for_exit` (request)

- **Types / negotiation:** WaitForTerminalExitRequest, WaitForTerminalExitResponse, TerminalExitStatus; clientCapabilities.terminal=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:710; src/terminal.rs:182–201`; **implemented**.
- **Release:** Cancel only this waiter; handle/process survive; reply carries flattened exitCode/signal.
- **Direct acceptance:** Cancel repeated waits: no accumulating waiters; process still output/kill/release capable. Normal/signal exit returns appropriate exitCode/signal after final reader drain.

### S09 `terminal/kill` (request)

- **Types / negotiation:** KillTerminalRequest, KillTerminalResponse, TerminalExitStatus; clientCapabilities.terminal=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:730; src/terminal.rs:203–209`; **implemented**.
- **Release:** Kill process, retain terminal handle and output.
- **Direct acceptance:** Kill returns response, later output/wait work, release still required; already-exited background group not accidentally signalled.

### S10 `terminal/release` (request)

- **Types / negotiation:** ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalExitStatus; clientCapabilities.terminal=true only stdio.
- **Entry / status:** `src/bridge/agent_dispatch.rs:745; src/terminal.rs:211–267; src/runtime_state.rs:1188–1190`; **implemented_retention_gap**.
- **Release:** Handle removed; current experiment also deletes referenced display output (D8).
- **Direct acceptance:** After release all terminal/* operations reject old handle; embedded tool still resolves frozen output/status. Connection end freezes referenced output while session remains held; no reference permits cleanup.

### S11 `mcp/connect` (request)

- **Types / negotiation:** ConnectMcpRequest, ConnectMcpResponse; Agent advertises mcpCapabilities.acp; launch config includes McpServer::Acp, serverId known.
- **Entry / status:** `src/bridge/agent_dispatch.rs:765–781 → src/mcp.rs:59–167; src/bridge.rs:5637–5650`; **implemented_pinned_unstable**.
- **Memory:** Static server provider config plus active connections map: child/stdin/cancellation token/pending requests/next ID. No raw-message log retained.
- **Release:** Cancelled startup terminates unannounced child; disconnect/exit/connection shutdown drains pending and kills child.
- **rc5 comparison:** src/mcp.rs identical to rc5.
- **Required behavior:** Only configured server IDs spawn; response returns allocated connectionId. No separate ClientCapabilities.mcp bit in this baseline; it is Agent mcpCapabilities.acp gating server configuration.
- **Direct acceptance:** Connect known/unknown ID; startup exit and cancel return error and no orphan connection/process. Multiple connections have isolated IDs and shutdown cleanup.
- **Zed comparison:** Zed v1 production registration has no MCP relay methods; mcp_servers_for_project :9134–9169 passes stdio/http configs. No Zed relay parity claim.

### S12 `mcp/message` (request)

- **Types / negotiation:** MessageMcpRequest, MessageMcpResponse; active MCP-over-ACP connection.
- **Entry / status:** `src/bridge/agent_dispatch.rs:784–799 → src/mcp.rs:170–222,381–488`; **implemented_pinned_unstable**.
- **Memory:** Per-connection pending map of generated provider request ID to response sender/method; removed when reply/cancel arrives.
- **Release:** Result is arbitrary JSON raw response; MCP error code/message/data forwarded as ACP error; cancel removes pending and sends provider notifications/cancelled.
- **rc5 comparison:** No rc5 relay change.
- **Required behavior:** Forward method and optional object params. Reverse provider request sends ACP MessageMcpRequest, remaps result/error to original provider ID; provider reader must remain free for nested callbacks. Envelope _meta is not inserted into MCP params; params._meta remains data.
- **Direct acceptance:** Round-trip nested result and structured error in both directions. Provider callback makes nested request without deadlock. Cancel pending Agent request, verify provider notification and no leaked pending entry; malformed response reports error without corrupting others.
- **Zed comparison:** No corresponding Zed v1 relay registration; not inferred from Zed own MCP management UI.

### S13 `mcp/message` (notification)

- **Types / negotiation:** MessageMcpNotification; active MCP-over-ACP connection.
- **Entry / status:** `src/bridge/agent_dispatch.rs:224–230 → src/mcp.rs:225–243,438–443`; **implemented_pinned_unstable**.
- **Memory:** Transient method/params; no response/pending record for notification. Reverse provider notification forwarded to Agent.
- **Release:** Connection closure prevents further sends; unknown connection is an isolated diagnostic, not a reply to notification.
- **rc5 comparison:** No rc5 relay change.
- **Required behavior:** Bidirectional method/params semantic preservation without inventing tool cards or saving every message as history.
- **Direct acceptance:** Agent and provider each emit notification with nested params; opposite peer observes same content and no fabricated response.
- **Zed comparison:** Not supported by Zed v1 relay; static config forwarding is a different feature.

### S14 `mcp/disconnect` (request)

- **Types / negotiation:** DisconnectMcpRequest, DisconnectMcpResponse; active MCP-over-ACP connection.
- **Entry / status:** `src/bridge/agent_dispatch.rs:802–815 → src/mcp.rs:245–255,491–515`; **implemented_pinned_unstable**.
- **Memory:** Consumes connection entry; pending senders drained with disconnect reason.
- **Release:** Kills child and cancels reverse tasks; repeated unknown ID errors, does not affect other connections.
- **rc5 comparison:** No rc5 relay change.
- **Required behavior:** Release operational state exactly once, not a completed-connection ledger.
- **Direct acceptance:** Disconnect with pending request: waiter gets error, map/child gone; second connection unaffected. Provider exit and Agent connection close use same cleanup guarantee.
- **Zed comparison:** No Zed v1 relay equivalent.

### S15 `_<extension>` (request)

- **Types / negotiation:** ExtRequest, ExtResponse; No private extension negotiated.
- **Entry / status:** `src/bridge/agent_dispatch.rs:208–209`; **explicitly_unsupported**.
- **Memory:** No retained state.
- **Release:** Immediate method_not_found for unhandled method.
- **rc5 comparison:** Same fallback as rc5.
- **Required behavior:** Unknown request gets JSON-RPC error without poisoning connection; no arbitrary vendor dispatch or RPC proxy UI.
- **Direct acceptance:** Send unknown extension with arbitrary object params: method_not_found on its ID, later normal request works.
- **Zed comparison:** No generic unknown-extension implementation inferred from Zed.

### S16 `_<extension>` (notification)

- **Types / negotiation:** ExtNotification; No private extension negotiated.
- **Entry / status:** `src/bridge/agent_dispatch.rs:234–236`; **ignored_unknown_notification**.
- **Memory:** No retained state.
- **Release:** Ignored; notification must not receive response.
- **rc5 comparison:** Same default as rc5; protocol target still calls for an explicit regression test.
- **Required behavior:** Ignore unknown notification safely; optional diagnostics only. Schema fallback preservation does not oblige history retention.
- **Direct acceptance:** Send unknown extension notification then valid request/update; no reply to notification, subsequent input progresses.
- **Zed comparison:** SDK/default fallback; not evidence for a Zed vendor extension requirement.

### S17 `$/cancel_request` (protocol_notification)

- **Types / negotiation:** CancelRequestNotification, ProtocolLevelNotification, RequestId; protocol-level, no capability bit.
- **Entry / status:** `src/bridge/agent_dispatch.rs:291–339,434–455; src/bridge/inbound_requests.rs:48–139; SDK schema/protocol_level.rs:5–9`; **implemented_with_resource_boundaries**.
- **Memory:** Only live request allocation/captured route; numeric and string IDs distinguished, no historical RPC table. _meta parsed by SDK but not a cancellation semantic.
- **Release:** Original request MUST still return valid result/cancellation marker or -32800 (schema v1/protocol_level.rs:94–102). Permission/form return their cancel outcomes; cancellable read/wait/MCP stop external waits. Started in-place writes may finish valid response.
- **rc5 comparison:** Allocation-exact cancellation kept; session incarnation removed, but do not conflate request allocation correctness with unverified local callback replacement.
- **Required behavior:** No double response; unknown/late cancel harmless; terminal waiter cancel does not kill process; MCP canceled connect no leaked child. Not all operations must abort immediately: schema says MAY cancel activity.
- **Direct acceptance:** Race cancel/response and reused wire ID; exactly one original response and new allocation untouched. Cancel fs, terminal wait, MCP connection/message; assert external resource effects as documented.
- **Zed comparison:** v1 permission :9356–9376 and terminal wait :9804+ use responder cancellation; shared elicitation handler cancels its individual entry. Do not equate prompt cancel with protocol cancellation.

## 3. Form and interaction subfeatures

| ID / subfeature | Types | Current support | Required behavior / direct acceptance | Evidence |
| --- | --- | --- | --- | --- |
| F01 Permission kinds/outcomes | PermissionOptionKind; RequestPermissionOutcome; SelectedPermissionOutcome | **implemented**: 4 kinds allow_once/allow_always/reject_once/reject_always; selected offered ID or cancelled; no inferred vendor persistence. Duplicate/empty IDs and empty names rejected. | Keep option metadata; always semantics conveyed by selected option, Agent owns requested policy. Test: Exercise 4 choices and both outcomes, invalid option does not consume responder. | v1/client.rs:917–1083; src/semantic.rs:413–434; src/protocol/admission.rs:210–232 |
| F02 Form object | ElicitationSchema | **implemented_with_policy_bounds**: object type; properties, required, title, description, _meta retained; duplicate or unknown required names and empty property name rejected. | Schema/current response stay attached until individual resolution; no request archive. Test: Required/optional omission, default handling, metadata exact fixture. | v1/elicitation.rs:1128–1167; src/elicitation_validation.rs:134–160 |
| F03 String and single-select | StringPropertySchema; EnumOption; StringFormat | **partial**: title/description/minLength/maxLength/pattern/format/default/enum/oneOf/_meta; labels const/title/description/_meta retained. 4 formats checked; lengths count Unicode scalars. Pattern restricted; input with pattern >10000 UTF-16 units rejected. enum and oneOf mutually exclusive, nonempty unique choices. | Cover ordinary JSON Schema regex; declare remaining safe subset and bounded-input restriction. Test: ^(a\|b)+$ must not be rejected merely for alternation; Unicode bounds; all four formats; enum and titled single-select. | v1/elicitation.rs:42–50,70–94,140–205; src/elicitation_validation.rs:163–177,214–256,366–457,460–550 |
| F04 Number/integer | NumberPropertySchema; IntegerPropertySchema | **implemented_with_precision_review_needed**: title/description/minimum/maximum/default/_meta; finite bounds and integer fraction checked using as_f64; integer wire schema supports i64. | Verify exact integer comparison across full pinned domain; do not certify i64 bounds via f64 conversion. Test: Candidate: minimum 9007199254740993 with integer answer 9007199254740992 must reject; static precision concern, not yet runtime reproduced. | v1/elicitation.rs:335–378,444–487; src/elicitation_validation.rs:179–189,258–274,553–565 |
| F05 Boolean | BooleanPropertySchema | **implemented**: title/description/default/_meta; response must be bool, false not treated missing. | Keep defaults/metadata. Test: False default and false required response accepted; string false rejected. | v1/elicitation.rs:553–586; src/elicitation_validation.rs:205,277–281 |
| F06 Multi-select | MultiSelectPropertySchema; MultiSelectItems; StringMultiSelectItems; TitledMultiSelectItems; EnumOption | **implemented_known_variants**: minItems/maxItems/items/default, title/description/_meta; string enum or titled anyOf; unique string selections enforced. | Cover both known item representations; no nesting or arbitrary-object form support is advertised. Test: min/max/count/duplicates, plain/titled choices; exact answer string-array returned. | v1/elicitation.rs:637–697,815–878; src/elicitation_validation.rs:191–204,282–320 |
| F07 Unknown schema property/items | OtherElicitationPropertySchema; OtherMultiSelectItems | **preserved_not_fully_supported**: Unknown property type retained by typed schema and validate_property_schema returns Ok; attempting to answer that field rejects unsupported type. Unknown items type is preserved, but array validation does not inspect items.type: do not certify its semantics. | Unknown property MUST NOT be rendered as known control (schema); keep raw shape, explicit unsupported behavior; optional omission and decline/cancel may work. Test: Custom _field and future type preserved; required unsupported field cannot be silently fabricated; unknown items not falsely interpreted. | v1/elicitation.rs:724–745,979–1013; src/elicitation_validation.rs:193–206,303–330 |
| F08 Unknown elicitation mode/action | OtherElicitationMode; OtherElicitationAction | **explicitly_unsupported**: Schema can roundtrip unknown discriminator and fields; bridge rejects unknown requested mode or outgoing response action. | Explicit unsupported, not known-mode fallback; no arbitrary extension functionality promised. Test: _custom/future modes error without pending entry; unknown answer leaves known request unresolved. | v1/elicitation.rs:1645–1681,1921–1954; src/elicitation_validation.rs:40–45,50–59 |
| F09 Response action/content | ElicitationAction; ElicitationAcceptAction; ElicitationContentValue | **implemented_known_variants**: accept/decline/cancel; content optional object of string/i64/f64/bool/string-array. Nonaccept and URL accept forbid content; form checks required and rejects extra fields. Response _meta roundtrips through typed elicitation response. | At-most-once independent delivery; invalid answer preserves sender. Test: Each action; omitted/null/empty form content; all five content variants; invalid extra/type/content on decline. | v1/elicitation.rs:1867–1882,1914–1931,2043–2082; src/elicitation_validation.rs:50–97 |
| F10 Session/tool and request scope | ElicitationScope; ElicitationSessionScope; ElicitationRequestScope | **lifecycle_and_domain_gap**: SessionId optional toolCallId retained; requestId string/safe-integer/null accepted by validator, empty string allowed; length limit 1024 UTF-16 units. Pinned RequestId is Null/i64/String, so valid signed-i64 values outside ±9007199254740991 are rejected here (shared with rc5). Missing/nonpending request scope currently falls back global. Unknown session rejected. | Separate actual pending RPC from op; preserve the complete ACP ID domain; while the narrower validator remains, record it as a support gap. The negotiated elicitation capability has no numeric-ID-range switch. Unknown/null parent policy explicit, not confused with live arbitrary global request; do not invent association for pre-response unknown sessions. Test: session form during mode; request form during parent close vs delete; stale/null parent; session/tool association; known held load.; A request-scope requestId of 9007199254740993 is representable by schema RequestId but is rejected by bridge validate_scope; inspect exact ID without f64 conversion. | rpc.rs:31–40; v1/elicitation.rs:1484–1547; src/elicitation_validation.rs:100–131; src/bridge/scheduling.rs:616–627; src/bridge/agent_dispatch.rs:475–509 |
| F11 URL identity and outcomes | ElicitationUrlMode; CompleteElicitationNotification | **handled_outcome_gap**: HTTP(S) only; unique outstanding elicitationId, registration token protects ID reuse; accept creates flow; completion/dismissal removes it. Prompt cancel now preserves accepted flow (improvement over rc5). | Outcome retained on same flow; waiting-only transitions; no parent request archive. Test: HTTP(S), reject javascript/file; accept/decline/cancel; reuse/late completion; parent response and connection end sequences. | src/elicitation_validation.rs:26–33; src/bridge/agent_dispatch.rs:488–498; src/runtime_state.rs:1093–1162; src/bridge.rs:5459–5464 |

## 4. Fixed Zed reference: useful comparison, not the normative baseline

- **wire-v1 boundary**: 9 concrete core services registered plus session/update and completion; no MCP-over-ACP. V1 converts to shared v2-shaped internal elicitation types; this is not selecting ACP wire v2. [crates/agent_servers/src/acp/v1.rs:394–441,529–534,9383–9413](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L394)
- **capabilities**: Zed advertises local fs/terminal/form/url/auth and has extra metadata/notice capabilities; these are not a reason to add Agent-specific extensions to attyd. [crates/agent_servers/src/acp/v1.rs:445–475](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L445)
- **permission**: Independent tool permission ID/task/cancellation; direct result response. Useful model for independent responders. [crates/agent_servers/src/acp/v1.rs:9320–9381](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L9320)
- **form conversions**: Copies known schemas, _meta, patterns and unknown schema/mode/action fields between internal versions; no evidence here that every pattern is validated equivalently. [crates/acp_thread/src/elicitation.rs:5–58,90–199](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/acp_thread/src/elicitation.rs#L5)
- **URL completion**: Completes accepted flow only; completed/cancelled states monotonic; connection cleanup cancels current elicitation stores (:177–199). [crates/agent_servers/src/acp.rs:774–796; crates/acp_thread/src/acp_thread.rs:591–645](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp.rs#L774)
- **parent lifetime caveat**: V1 active_request_scopes=None; shared conditional parent check inactive. finish_request cancels only pending, cancel_request also accepted URL, but callers are v2 only. Do not cite v2 parent scope handling as v1 implementation. [crates/agent_servers/src/acp/v1.rs:533; crates/agent_servers/src/acp.rs:722–729; crates/acp_thread/src/acp_thread.rs:774–793; crates/agent_servers/src/acp/v2.rs:42–57](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L533)
- **terminal retention**: V1 terminal content resolves to retained Entity<Terminal>; release removes handle lookup and kills process while referenced entity remains. Do not copy GUI terminal architecture. [crates/agent_servers/src/acp/v1.rs:9650–9845; crates/acp_thread/src/acp_thread.rs:2960–2962,2998–3002,7596–7615](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L9650)
- **MCP boundary**: Only passes stdio/http server configuration to Agent; no mcp/connect/message/disconnect registered. Zed own MCP settings/manager is not ACP relay activity. [crates/agent_servers/src/acp/v1.rs:394–441,9134–9169](https://github.com/zed-industries/zed/blob/9f63850f7affbd57772a4f99554d4fd333b50a6f/crates/agent_servers/src/acp/v1.rs#L394)

In particular, v1-to-shared-internal-v2 conversion is a real v1 path, but methods only called from `acp/v2.rs` are not v1 behavior. Zed lacks this baseline’s MCP-over-ACP relay; its MCP UI is not a relay conformance oracle.

## 5. Exhaustive wire field / enum appendix

Mechanically extracted public declarations from pinned schema source. `?` denotes Rust Option; serde defaults/fallback attributes are in the shared JSON; field doc comments remain in the linked source. Flattened fields carry nested wire fields. Client-generated service response metadata may be absent; received request envelope metadata does not require raw-RPC history. Known fields marked DefaultOnError follow SDK deserialization, not byte-for-byte preservation of malformed wire fields.

### `RequestPermissionRequest` — `v1/client.rs:863`

Matrix: S01; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 865 |
| `toolCall` | `ToolCallUpdate` | no | 867 |
| `options` | `Vec<PermissionOption>` | no | 869 |
| `_meta` | `Option<Meta>` | no | 879 |

### `PermissionOption` — `v1/client.rs:917`

Matrix: S01; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `optionId` | `PermissionOptionId` | no | 919 |
| `name` | `String` | no | 921 |
| `kind` | `PermissionOptionKind` | no | 923 |
| `_meta` | `Option<Meta>` | no | 933 |

### `PermissionOptionId` — `v1/client.rs:970`

Matrix: F01, S01; ownership: services.

```rust
pub struct PermissionOptionId(pub Arc<str>);
```

### `PermissionOptionKind` — `v1/client.rs:987`

Matrix: F01, S01; ownership: services.

`AllowOnce` (L989); `AllowAlways` (L991); `RejectOnce` (L993); `RejectAlways` (L995).

### `RequestPermissionResponse` — `v1/client.rs:1006`

Matrix: S01; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `outcome` | `RequestPermissionOutcome` | no | 1009 |
| `_meta` | `Option<Meta>` | no | 1019 |

### `RequestPermissionOutcome` — `v1/client.rs:1050`

Matrix: F01, S01; ownership: services.

`Cancelled` (L1058); `Selected` → `SelectedPermissionOutcome` (L1061).

### `SelectedPermissionOutcome` — `v1/client.rs:1071`

Matrix: F01, S01; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `optionId` | `PermissionOptionId` | no | 1073 |
| `_meta` | `Option<Meta>` | no | 1083 |

### `WriteTextFileRequest` — `v1/client.rs:1120`

Matrix: S05; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1122 |
| `path` | `PathBuf` | no | 1124 |
| `content` | `String` | no | 1126 |
| `_meta` | `Option<Meta>` | no | 1136 |

### `WriteTextFileResponse` — `v1/client.rs:1175`

Matrix: S05; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 1185 |

### `ReadTextFileRequest` — `v1/client.rs:1219`

Matrix: S04; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1221 |
| `path` | `PathBuf` | no | 1223 |
| `line` | `Option<u32>` | no | 1228 |
| `limit` | `Option<u32>` | no | 1233 |
| `_meta` | `Option<Meta>` | no | 1243 |

### `ReadTextFileResponse` — `v1/client.rs:1293`

Matrix: S04; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `content` | `String` | no | 1295 |
| `_meta` | `Option<Meta>` | no | 1305 |

### `TerminalId` — `v1/client.rs:1338`

Matrix: S06, S07, S08, S09, S10; ownership: services.

```rust
pub struct TerminalId(pub Arc<str>);
```

### `CreateTerminalRequest` — `v1/client.rs:1356`

Matrix: S06; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1358 |
| `command` | `String` | no | 1360 |
| `args` | `Vec<String>` | no | 1365 |
| `env` | `Vec<EnvVariable>` | no | 1370 |
| `cwd` | `Option<PathBuf>` | no | 1375 |
| `outputByteLimit` | `Option<u64>` | no | 1387 |
| `_meta` | `Option<Meta>` | no | 1397 |

### `CreateTerminalResponse` — `v1/client.rs:1470`

Matrix: S06; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `terminalId` | `TerminalId` | no | 1472 |
| `_meta` | `Option<Meta>` | no | 1482 |

### `TerminalOutputRequest` — `v1/client.rs:1515`

Matrix: S07; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1517 |
| `terminalId` | `TerminalId` | no | 1519 |
| `_meta` | `Option<Meta>` | no | 1529 |

### `TerminalOutputResponse` — `v1/client.rs:1563`

Matrix: S07; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `output` | `String` | no | 1565 |
| `truncated` | `bool` | no | 1567 |
| `exitStatus` | `Option<TerminalExitStatus>` | no | 1572 |
| `_meta` | `Option<Meta>` | no | 1582 |

### `ReleaseTerminalRequest` — `v1/client.rs:1624`

Matrix: S10; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1626 |
| `terminalId` | `TerminalId` | no | 1628 |
| `_meta` | `Option<Meta>` | no | 1638 |

### `ReleaseTerminalResponse` — `v1/client.rs:1672`

Matrix: S10; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 1682 |

### `KillTerminalRequest` — `v1/client.rs:1712`

Matrix: S09; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1714 |
| `terminalId` | `TerminalId` | no | 1716 |
| `_meta` | `Option<Meta>` | no | 1726 |

### `KillTerminalResponse` — `v1/client.rs:1760`

Matrix: S09; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 1770 |

### `WaitForTerminalExitRequest` — `v1/client.rs:1800`

Matrix: S08; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1802 |
| `terminalId` | `TerminalId` | no | 1804 |
| `_meta` | `Option<Meta>` | no | 1814 |

### `WaitForTerminalExitResponse` — `v1/client.rs:1848`

Matrix: S08; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `exitStatus` | `TerminalExitStatus` | yes | 1851 |
| `_meta` | `Option<Meta>` | no | 1861 |

### `TerminalExitStatus` — `v1/client.rs:1893`

Matrix: S06, S07, S08, S09, S10; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `exitCode` | `Option<u32>` | no | 1898 |
| `signal` | `Option<String>` | no | 1903 |
| `_meta` | `Option<Meta>` | no | 1913 |

### `ClientCapabilities` — `v1/client.rs:1963`

Matrix: CAP01, CAP02, CAP03, CAP04, CAP05, CAP06; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `fs` | `FileSystemCapabilities` | no | 1969 |
| `terminal` | `bool` | no | 1974 |
| `session` | `Option<ClientSessionCapabilities>` | no | 1982 |
| `plan` | `Option<PlanCapabilities>` | no | 1995 |
| `auth` | `AuthCapabilities` | no | 2002 |
| `elicitation` | `Option<ElicitationCapabilities>` | no | 2011 |
| `nes` | `Option<ClientNesCapabilities>` | no | 2024 |
| `positionEncodings` | `Vec<PositionEncodingKind>` | no | 2034 |
| `_meta` | `Option<Meta>` | no | 2045 |

### `ClientSessionCapabilities` — `v1/client.rs:2148`

Matrix: CAP05; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `compaction` | `Option<CompactionCapabilities>` | no | 2159 |
| `configOptions` | `Option<SessionConfigOptionsCapabilities>` | no | 2167 |
| `_meta` | `Option<Meta>` | no | 2177 |

### `SessionConfigOptionsCapabilities` — `v1/client.rs:2248`

Matrix: CAP05; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `boolean` | `Option<BooleanConfigOptionCapabilities>` | no | 2258 |
| `_meta` | `Option<Meta>` | no | 2268 |

### `BooleanConfigOptionCapabilities` — `v1/client.rs:2310`

Matrix: CAP05; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 2320 |

### `AuthCapabilities` — `v1/client.rs:2353`

Matrix: CAP03; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `terminal` | `bool` | no | 2362 |
| `_meta` | `Option<Meta>` | no | 2372 |

### `FileSystemCapabilities` — `v1/client.rs:2415`

Matrix: CAP01; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `readTextFile` | `bool` | no | 2420 |
| `writeTextFile` | `bool` | no | 2425 |
| `_meta` | `Option<Meta>` | no | 2435 |

### `ClientMethodNames` — `v1/client.rs:2478`

Matrix: S01, S02, S04, S05, S06, S07, S08, S09, S10, S11, S12, S14, S15; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `session_request_permission` | `&'static str` | no | 2480 |
| `session_update` | `&'static str` | no | 2482 |
| `fs_write_text_file` | `&'static str` | no | 2484 |
| `fs_read_text_file` | `&'static str` | no | 2486 |
| `terminal_create` | `&'static str` | no | 2488 |
| `terminal_output` | `&'static str` | no | 2490 |
| `terminal_release` | `&'static str` | no | 2492 |
| `terminal_wait_for_exit` | `&'static str` | no | 2494 |
| `terminal_kill` | `&'static str` | no | 2496 |
| `mcp_connect` | `&'static str` | no | 2499 |
| `mcp_message` | `&'static str` | no | 2502 |
| `mcp_disconnect` | `&'static str` | no | 2505 |
| `elicitation_create` | `&'static str` | no | 2507 |
| `elicitation_complete` | `&'static str` | no | 2509 |

### `AgentRequest` — `v1/client.rs:2568`

Matrix: S01, S02, S04, S05, S06, S07, S08, S09, S10, S11, S12, S14, S15; ownership: services.

`WriteTextFileRequest` → `WriteTextFileRequest` (L2575); `ReadTextFileRequest` → `ReadTextFileRequest` (L2582); `RequestPermissionRequest` → `RequestPermissionRequest` (L2593); `CreateTerminalRequest` → `CreateTerminalRequest` (L2608); `TerminalOutputRequest` → `TerminalOutputRequest` (L2615); `ReleaseTerminalRequest` → `ReleaseTerminalRequest` (L2628); `WaitForTerminalExitRequest` → `WaitForTerminalExitRequest` (L2632); `KillTerminalRequest` → `KillTerminalRequest` (L2645); `CreateElicitationRequest` → `CreateElicitationRequest` (L2649); `ConnectMcpRequest` → `ConnectMcpRequest` (L2656); `MessageMcpRequest` → `MessageMcpRequest` (L2663); `DisconnectMcpRequest` → `DisconnectMcpRequest` (L2670); `ExtMethodRequest` → `ExtRequest` (L2678).

### `ClientResponse` — `v1/client.rs:2717`

Matrix: S01, S02, S04, S05, S06, S07, S08, S09, S10, S11, S12, S14, S15; ownership: services.

`WriteTextFileResponse` → `#[serde(default)] WriteTextFileResponse` (L2719); `ReadTextFileResponse` → `ReadTextFileResponse` (L2721); `RequestPermissionResponse` → `RequestPermissionResponse` (L2723); `CreateTerminalResponse` → `CreateTerminalResponse` (L2725); `TerminalOutputResponse` → `TerminalOutputResponse` (L2727); `ReleaseTerminalResponse` → `#[serde(default)] ReleaseTerminalResponse` (L2729); `WaitForTerminalExitResponse` → `WaitForTerminalExitResponse` (L2731); `KillTerminalResponse` → `#[serde(default)] KillTerminalResponse` (L2733); `CreateElicitationResponse` → `CreateElicitationResponse` (L2735); `ConnectMcpResponse` → `ConnectMcpResponse` (L2738); `DisconnectMcpResponse` → `#[serde(default)] DisconnectMcpResponse` (L2741); `MessageMcpResponse` → `MessageMcpResponse` (L2744); `ExtMethodResponse` → `ExtResponse` (L2746).

### `AgentNotification` — `v1/client.rs:2761`

Matrix: [CN-00](content.md), S03, S13, S16; ownership: services/content.

`SessionNotification` → `SessionNotification` (L2773); `CompleteElicitationNotification` → `CompleteElicitationNotification` (L2777); `MessageMcpNotification` → `MessageMcpNotification` (L2784); `ExtNotification` → `ExtNotification` (L2792).

### `ElicitationId` — `v1/elicitation.rs:27`

Matrix: F11, S02, S03; ownership: services.

```rust
pub struct ElicitationId(pub Arc<str>);
```

### `StringFormat` — `v1/elicitation.rs:42`

Matrix: F02, F03, S02; ownership: services.

`Email` (L44); `Uri` (L46); `Date` (L48); `DateTime` (L50).

### `ElicitationSchemaType` — `v1/elicitation.rs:58`

Matrix: F02, S02; ownership: services.

`Object` (L61).

### `EnumOption` — `v1/elicitation.rs:70`

Matrix: F02, F03, F06, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `const` | `String` | no | 73 |
| `title` | `String` | no | 75 |
| `description` | `Option<String>` | no | 82 |
| `_meta` | `Option<Meta>` | no | 94 |

### `StringPropertySchema` — `v1/elicitation.rs:140`

Matrix: F02, F03, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `title` | `Option<String>` | no | 147 |
| `description` | `Option<String>` | no | 154 |
| `minLength` | `Option<u32>` | no | 159 |
| `maxLength` | `Option<u32>` | no | 164 |
| `pattern` | `Option<String>` | no | 169 |
| `format` | `Option<StringFormat>` | no | 174 |
| `default` | `Option<String>` | no | 181 |
| `enum` | `Option<Vec<String>>` | no | 187 |
| `oneOf` | `Option<Vec<EnumOption>>` | no | 193 |
| `_meta` | `Option<Meta>` | no | 205 |

### `NumberPropertySchema` — `v1/elicitation.rs:335`

Matrix: F02, F04, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `title` | `Option<String>` | no | 342 |
| `description` | `Option<String>` | no | 349 |
| `minimum` | `Option<f64>` | no | 354 |
| `maximum` | `Option<f64>` | no | 359 |
| `default` | `Option<f64>` | no | 366 |
| `_meta` | `Option<Meta>` | no | 378 |

### `IntegerPropertySchema` — `v1/elicitation.rs:444`

Matrix: F02, F04, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `title` | `Option<String>` | no | 451 |
| `description` | `Option<String>` | no | 458 |
| `minimum` | `Option<i64>` | no | 463 |
| `maximum` | `Option<i64>` | no | 468 |
| `default` | `Option<i64>` | no | 475 |
| `_meta` | `Option<Meta>` | no | 487 |

### `BooleanPropertySchema` — `v1/elicitation.rs:553`

Matrix: F02, F05, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `title` | `Option<String>` | no | 560 |
| `description` | `Option<String>` | no | 567 |
| `default` | `Option<bool>` | no | 574 |
| `_meta` | `Option<Meta>` | no | 586 |

### `StringMultiSelectItems` — `v1/elicitation.rs:637`

Matrix: F02, F06, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `enum` | `Vec<String>` | no | 640 |
| `_meta` | `Option<Meta>` | no | 652 |

### `TitledMultiSelectItems` — `v1/elicitation.rs:682`

Matrix: F02, F06, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `anyOf` | `Vec<EnumOption>` | no | 685 |
| `_meta` | `Option<Meta>` | no | 697 |

### `OtherMultiSelectItems` — `v1/elicitation.rs:735`

Matrix: F02, F06, F07, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `type` | `String` | no | 742 |
| `fields` | `BTreeMap<String, serde_json::Value>` | yes | 745 |

### `MultiSelectItems` — `v1/elicitation.rs:815`

Matrix: F02, F06, S02; ownership: services.

`String` → `StringMultiSelectItems` (L817); `Other` → `OtherMultiSelectItems` (L820); `Titled` → `TitledMultiSelectItems` (L823).

### `MultiSelectPropertySchema` — `v1/elicitation.rs:833`

Matrix: F02, F06, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `title` | `Option<String>` | no | 840 |
| `description` | `Option<String>` | no | 847 |
| `minItems` | `Option<u64>` | no | 852 |
| `maxItems` | `Option<u64>` | no | 857 |
| `items` | `MultiSelectItems` | no | 859 |
| `default` | `Option<Vec<String>>` | no | 866 |
| `_meta` | `Option<Meta>` | no | 878 |

### `ElicitationPropertySchema` — `v1/elicitation.rs:968`

Matrix: F02, S02; ownership: services.

`String` → `StringPropertySchema` (L970); `Number` → `NumberPropertySchema` (L972); `Integer` → `IntegerPropertySchema` (L974); `Boolean` → `BooleanPropertySchema` (L976); `Array` → `MultiSelectPropertySchema` (L978); `Other` → `OtherElicitationPropertySchema` (L989).

### `OtherElicitationPropertySchema` — `v1/elicitation.rs:1003`

Matrix: F02, F07, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `type` | `String` | no | 1010 |
| `fields` | `BTreeMap<String, serde_json::Value>` | yes | 1013 |

### `ElicitationSchema` — `v1/elicitation.rs:1128`

Matrix: F02, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `type_` | `ElicitationSchemaType` | no | 1133 |
| `title` | `Option<String>` | no | 1140 |
| `properties` | `BTreeMap<String, ElicitationPropertySchema>` | no | 1143 |
| `required` | `Option<Vec<String>>` | no | 1148 |
| `description` | `Option<String>` | no | 1155 |
| `_meta` | `Option<Meta>` | no | 1167 |

### `ElicitationCapabilities` — `v1/elicitation.rs:1306`

Matrix: CAP04; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `form` | `Option<ElicitationFormCapabilities>` | no | 1314 |
| `url` | `Option<ElicitationUrlCapabilities>` | no | 1322 |
| `_meta` | `Option<Meta>` | no | 1334 |

### `ElicitationFormCapabilities` — `v1/elicitation.rs:1403`

Matrix: CAP04; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 1415 |

### `ElicitationUrlCapabilities` — `v1/elicitation.rs:1448`

Matrix: CAP04; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 1460 |

### `ElicitationScope` — `v1/elicitation.rs:1489`

Matrix: F08, F10, F11, S02; ownership: services.

`Session` → `ElicitationSessionScope` (L1491); `Request` → `ElicitationRequestScope` (L1494).

### `ElicitationSessionScope` — `v1/elicitation.rs:1508`

Matrix: F08, F10, F11, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `sessionId` | `SessionId` | no | 1510 |
| `toolCallId` | `Option<ToolCallId>` | no | 1518 |

### `ElicitationRequestScope` — `v1/elicitation.rs:1545`

Matrix: F08, F10, F11, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `requestId` | `RequestId` | no | 1547 |

### `CreateElicitationRequest` — `v1/elicitation.rs:1584`

Matrix: S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `mode` | `ElicitationMode` | yes | 1587 |
| `message` | `String` | no | 1589 |
| `_meta` | `Option<Meta>` | no | 1601 |

### `ElicitationMode` — `v1/elicitation.rs:1640`

Matrix: S02; ownership: services.

`Form` → `ElicitationFormMode` (L1642); `Url` → `ElicitationUrlMode` (L1644); `Other` → `OtherElicitationMode` (L1655).

### `OtherElicitationMode` — `v1/elicitation.rs:1669`

Matrix: F08, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `mode` | `String` | no | 1675 |
| `scope` | `ElicitationScope` | yes | 1678 |
| `fields` | `BTreeMap<String, serde_json::Value>` | yes | 1681 |

### `ElicitationFormMode` — `v1/elicitation.rs:1808`

Matrix: S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `scope` | `ElicitationScope` | yes | 1811 |
| `requestedSchema` | `ElicitationSchema` | no | 1813 |

### `ElicitationUrlMode` — `v1/elicitation.rs:1832`

Matrix: F11, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `scope` | `ElicitationScope` | yes | 1835 |
| `elicitationId` | `ElicitationId` | no | 1837 |
| `url` | `String` | no | 1840 |

### `CreateElicitationResponse` — `v1/elicitation.rs:1867`

Matrix: S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `action` | `ElicitationAction` | yes | 1870 |
| `_meta` | `Option<Meta>` | no | 1882 |

### `ElicitationAction` — `v1/elicitation.rs:1914`

Matrix: F09, S02; ownership: services.

`Accept` → `ElicitationAcceptAction` (L1916); `Decline` (L1918); `Cancel` (L1920); `Other` → `OtherElicitationAction` (L1931).

### `OtherElicitationAction` — `v1/elicitation.rs:1945`

Matrix: F08, F09, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `action` | `String` | no | 1951 |
| `fields` | `BTreeMap<String, serde_json::Value>` | yes | 1954 |

### `ElicitationAcceptAction` — `v1/elicitation.rs:2043`

Matrix: F09, S02; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `content` | `Option<BTreeMap<String, ElicitationContentValue>>` | no | 2046 |

### `ElicitationContentValue` — `v1/elicitation.rs:2072`

Matrix: F09, S02; ownership: services.

`String` → `String` (L2074); `Integer` → `i64` (L2076); `Number` → `f64` (L2078); `Boolean` → `bool` (L2080); `StringArray` → `Vec<String>` (L2082).

### `CompleteElicitationNotification` — `v1/elicitation.rs:2147`

Matrix: F11, S03; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `elicitationId` | `ElicitationId` | no | 2149 |
| `_meta` | `Option<Meta>` | no | 2161 |

### `McpConnectionId` — `v1/mcp.rs:24`

Matrix: S11, S12, S13, S14; ownership: services.

```rust
pub struct McpConnectionId(pub Arc<str>);
```

### `ConnectMcpRequest` — `v1/mcp.rs:46`

Matrix: S11; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `serverId` | `McpServerAcpId` | no | 48 |
| `_meta` | `Option<Meta>` | no | 58 |

### `ConnectMcpResponse` — `v1/mcp.rs:95`

Matrix: S11; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `connectionId` | `McpConnectionId` | no | 97 |
| `_meta` | `Option<Meta>` | no | 107 |

### `MessageMcpRequest` — `v1/mcp.rs:144`

Matrix: S12; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `connectionId` | `McpConnectionId` | no | 146 |
| `method` | `String` | no | 148 |
| `params` | `Option<serde_json::Map<String, serde_json::Value>>` | no | 153 |
| `_meta` | `Option<Meta>` | no | 163 |

### `MessageMcpNotification` — `v1/mcp.rs:217`

Matrix: S13; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `connectionId` | `McpConnectionId` | no | 219 |
| `method` | `String` | no | 221 |
| `params` | `Option<serde_json::Map<String, serde_json::Value>>` | no | 228 |
| `_meta` | `Option<Meta>` | no | 238 |

### `MessageMcpResponse` — `v1/mcp.rs:289`

Matrix: S12; ownership: services.

```rust
pub struct MessageMcpResponse(
    #[cfg_attr(feature = "schemars", schemars(with = "serde_json::Value"))] pub Arc<RawValue>,
);
```

### `DisconnectMcpRequest` — `v1/mcp.rs:313`

Matrix: S14; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `connectionId` | `McpConnectionId` | no | 315 |
| `_meta` | `Option<Meta>` | no | 325 |

### `DisconnectMcpResponse` — `v1/mcp.rs:362`

Matrix: S14; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `_meta` | `Option<Meta>` | no | 372 |

### `CancelRequestNotification` — `v1/protocol_level.rs:18`

Matrix: S17; ownership: common RPC.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `requestId` | `RequestId` | no | 20 |
| `_meta` | `Option<Meta>` | no | 30 |

### `GeneralMethodNames` — `v1/protocol_level.rs:62`

Matrix: S17; ownership: common RPC.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `cancel_request` | `&'static str` | no | 64 |

### `ProtocolLevelNotification` — `v1/protocol_level.rs:91`

Matrix: S17; ownership: common RPC.

`CancelRequestNotification` → `CancelRequestNotification` (L105).

### `ExtRequest` — `v1/ext.rs:25`

Matrix: S15; ownership: common RPC.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `method` | `Arc<str>` | no | 30 |
| `params` | `Arc<RawValue>` | no | 33 |

### `ExtResponse` — `v1/ext.rs:56`

Matrix: S15; ownership: common RPC.

```rust
pub struct ExtResponse(
    #[cfg_attr(feature = "schemars", schemars(with = "serde_json::Value"))] pub Arc<RawValue>,
);
```

### `ExtNotification` — `v1/ext.rs:77`

Matrix: S16; ownership: common RPC.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `method` | `Arc<str>` | no | 82 |
| `params` | `Arc<RawValue>` | no | 85 |

### `McpServerAcpId` — `v1/agent.rs:2789`

Matrix: CAP07, S11; ownership: services.

```rust
pub struct McpServerAcpId(pub Arc<str>);
```

### `McpServerAcp` — `v1/agent.rs:2815`

Matrix: CAP07; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `name` | `String` | no | 2817 |
| `serverId` | `McpServerAcpId` | no | 2822 |
| `_meta` | `Option<Meta>` | no | 2832 |

### `EnvVariable` — `v1/agent.rs:2933`

Matrix: S06; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `name` | `String` | no | 2935 |
| `value` | `String` | no | 2937 |
| `_meta` | `Option<Meta>` | no | 2947 |

### `McpCapabilities` — `v1/agent.rs:4522`

Matrix: CAP07; ownership: services.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `http` | `bool` | no | 4527 |
| `sse` | `bool` | no | 4532 |
| `acp` | `bool` | no | 4542 |
| `_meta` | `Option<Meta>` | no | 4552 |

### `ToolCallUpdate` — `v1/tool_call.rs:220`

Matrix: S01; ownership: content-shared.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `toolCallId` | `ToolCallId` | no | 222 |
| `fields` | `ToolCallUpdateFields` | yes | 225 |
| `_meta` | `Option<Meta>` | no | 235 |

### `ToolCallUpdateFields` — `v1/tool_call.rs:273`

Matrix: S01; ownership: content-shared.

| Wire field | Rust type | Flattened | Line |
| --- | --- | --- | --- |
| `kind` | `Option<ToolKind>` | no | 278 |
| `status` | `Option<ToolCallStatus>` | no | 283 |
| `title` | `Option<String>` | no | 288 |
| `name` | `Option<String>` | no | 301 |
| `content` | `Option<Vec<ToolCallContent>>` | no | 306 |
| `locations` | `Option<Vec<ToolCallLocation>>` | no | 311 |
| `rawInput` | `Option<serde_json::Value>` | no | 316 |
| `rawOutput` | `Option<serde_json::Value>` | no | 321 |

### `RequestId` — `rpc.rs:31`

Matrix: F08, F10, F11, S02, S17; ownership: common RPC.

`Null` (L34); `Number` → `i64` (L36); `Str` → `String` (L38).

## 6. Required memory and non-goals

- No historical RPC table or automatic fs/MCP traffic transcript. Retain only outstanding request correlation, active handles, interactions/URL outcomes and content references.
- Unadvertised NES/document/editor/provider management excluded; transport registration catalogue elsewhere.
- Remote transport host fs/terminal/auth unadvertised; read_only write unadvertised.
- Unknown extension requests method_not_found, notifications ignored; unknown form mode/action unsupported, unknown property schema preserved but not known input semantics.
- Fork/unknown-session installation association and old-allocation local callbacks need targeted decisions/verification; no invented owner/CAS design.
- Diagnostic activity/stderr display is optional and not this layer gate.

Remaining/delegated declarations for catalogue closure:

- **SessionNotification/SessionUpdate and content/tool bodies**: session/update inventory belongs to content chapter; permission ToolCallUpdate fields included here for cross-reference.
- **SessionId, PlanCapabilities, CompactionCapabilities, ClientNesCapabilities, PositionEncodingKind**: nested dependencies of service identifiers/capability declaration; behavior and complete definitions in lifecycle/content inventories.
- **RequestId, ExtRequest/Response/Notification, ProtocolLevelNotification, CancelRequestNotification, GeneralMethodNames**: included here for service matrix correlation; canonical common-RPC classification belongs to the transport chapter.
- **Meta alias and primitive/container serde helpers**: not pub struct/enum declarations owned here; malformed wire normalization/transport semantics belongs to the transport chapter.

## 7. Focused findings for the merged review

- Confirmed experiment changes needing planned repairs: per-interaction answers and actual interaction lifetime (D10/D11); retained terminal result on release/connection end (D8). No fs or MCP relay execution regression found.
- Shared gaps/boundaries: restricted form pattern engine (ordinary `^(a|b)+$` rejected), patterned input length cap, request scope validation limits, accepted URL terminal outcome removal, prompt-cancel also cancelling session forms. Distinguish these from regression claims.
- Confirmed embedded-ID domain restriction: schema `RequestId` permits any i64, while `validate_scope` rejects ± values outside JavaScript safe integer range; 9007199254740993 is a valid schema ID but rejected as elicitation parent. Same code in rc5. This is separate from outer SDK request-ID handling.
- Existing safe boundary: core methods respond using supplied/validated fields; this is not authority to retain every service RPC as conversation history or to invent vendor-specific MCP semantics.
- New field-level concern to verify: integer schema is i64 but validation compares f64; minimum 9007199254740993 versus supplied 9007199254740992 collapses under conversion. This report marks it a static precision concern, not a reproduced failing bridge test.
- Existing tests are useful path evidence, not proof of untested requirements: terminal.rs tests cover UTF-8 limit, literal args, environment/cwd, output drain, resource cleanup; filesystem.rs covers ranges/roots/write errors; agent_dispatch.rs tests cover captured cancellation; inbound_requests.rs tests allocation-exact reused IDs. New acceptance must inspect real ACP replies and owned state directly, never derive expectations with the projection.

## 8. Upstream 1.12 delta, separate from the pinned implementation contract

Compared against verified schema `1.12.0`, VCS `e2d83349bc9e28a4059ea3acefa1c91c966f74e2`. These are upgrade differences, not retroactive current-bridge duties. The shared source-inventory.json includes the latest field/variant declarations; this section records the semantic differences. Zed 9f63850 uses a newer SDK/schema family (3.4/1.12), so unstable MCP or newer capability behavior cannot be read as the 1.7 contract.

| ID / surface | Difference | Source | Consequence |
| --- | --- | --- | --- |
| U01 Permission, filesystem and terminal fields | Known request/response fields, permission kinds/outcomes and terminal status fields unchanged. Four service responses now declared via default_on_null! (write, release, kill, wait): null deserializes to default response; this is SDK acceptance behavior, not new bridge service capability. | `1.12 v1/client.rs:2447–2467,2936–2956,3026–3046,3116–3139` | No new field-level obligation against pinned 1.7. The transport chapter covers null/serde transport details. |
| U02 Elicitation/form/URL/scope and cancellation/extensions | All owned elicitation fields and enum variants unchanged, including string pattern and i64 content/request IDs. Elicitation diff only removes SkipListener from array-default deserialization and adds Eq derives. Cancel and ext wire shapes unchanged. | `1.12 v1/elicitation.rs; v1/protocol_level.rs; v1/ext.rs` | Existing limits remain local bridge limits; do not attribute a new form or parent-lifecycle mandate to schema 1.12. |
| U03 MCP lifecycle and route identifiers | Removed McpConnectionId, ConnectMcpRequest/Response, DisconnectMcpRequest/Response. Requests route using serverId plus caller-generated inner requestId distinct from outer ACP JSON-RPC ID. McpServerAcp name/serverId/_meta fields unchanged, but provider ID uniqueness now stated explicitly. | `1.12 v1/mcp.rs:138–182; v1/agent.rs:2909–2959` | Breaking unstable profile change: current mcp/connect/disconnect relay is valid only for pinned profile, not latest conformance. |
| U04 MCP result/error carrier | Opaque raw MessageMcpResponse replaced by Result { result, _meta? } or Error { error, _meta? }; inner McpError code/message/data/extra is separate from outer ACP binding/runtime errors. Explicit null inner result/data is preserved; outer unknown fields ignored; result branch wins malformed both-carrier input. | `1.12 v1/mcp.rs:23–33,60–99` | A future upgrade must not forward MCP error as outer ACP error; current 1.7 mapping must not be silently judged by this new contract. |
| U05 MCP direction and enum registrations | AgentRequest 13→11 and ClientResponse 13→11 remove connect/disconnect. AgentNotification 4→3 removes mcp/message. ClientRequest loses reverse mcp/message (and three unrelated NES methods); ClientNotification retains mcp/message (and removes seven unrelated NES/document notifications). New MCP notification carries serverId/requestId and flows Client→Agent. | `1.12 v1/client.rs:3882,4013,4051; v1/agent.rs:5144,5390; v1/mcp.rs:240–285` | Do not call latest profile bidirectional old-style RPC. Zed latest has no old MCP relay to copy. |
| U06 Client capability additions/removals | ClientCapabilities adds unstable subagents, removes unstable nes/positionEncodings; ClientSessionCapabilities adds notices. Existing fs/terminal/auth/elicitation/plan/config/compaction fields remain. Detailed notice/subagent/NES migration belongs to other inventories. | `1.12 v1/client.rs:3241–3324,3454–3501` | Not advertised or implemented by the current pinned bridge; this is future support scope, not evidence of a v2 regression. |

Mechanical diff types (unchanged fields do not imply every serde acceptance edge was runtime-tested):

- `AgentNotification`: wire_shape; v1/client.rs:2761 → v1/client.rs:4051.
- `AgentRequest`: wire_shape; v1/client.rs:2568 → v1/client.rs:3882.
- `ClientCapabilities`: wire_shape; v1/client.rs:1963 → v1/client.rs:3241.
- `ClientMethodNames`: wire_shape; v1/client.rs:2478 → v1/client.rs:3802.
- `ClientResponse`: wire_shape; v1/client.rs:2717 → v1/client.rs:4013.
- `ClientSessionCapabilities`: wire_shape; v1/client.rs:2148 → v1/client.rs:3454.
- `ConnectMcpRequest`: removed; v1/mcp.rs:46 → —.
- `ConnectMcpResponse`: removed; v1/mcp.rs:95 → —.
- `DisconnectMcpRequest`: removed; v1/mcp.rs:313 → —.
- `DisconnectMcpResponse`: removed; v1/mcp.rs:362 → —.
- `McpConnectionId`: removed; v1/mcp.rs:24 → —.
- `MessageMcpNotification`: wire_shape; v1/mcp.rs:217 → v1/mcp.rs:240.
- `MessageMcpRequest`: wire_shape; v1/mcp.rs:144 → v1/mcp.rs:160.
- `MessageMcpResponse`: wire_shape; v1/mcp.rs:289 → v1/mcp.rs:76.
- `McpError`: added; — → v1/mcp.rs:23.
- `McpRequestId`: added; — → v1/mcp.rs:138.
