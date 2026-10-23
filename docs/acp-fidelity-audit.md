# ACP fidelity audit

Status: reviewed, 2026-10-16. The original audit (#51, `a7eb3b4`) and its
revision 2 decisions are retained below. The current comparison is between
`experiment/protocol-v2` at `991b053` (revision 1 runtime, revision 2 documents)
and stable `main` at `60f8606` (`488ccd9`, rc5, plus the rustls repair).
Revision 2 is a target, not a claim that its behavior is implemented.

The [source-level feature matrix](acp-bridge-matrix.md) expands this audit into
every pinned Rust method, type and field, with fixed Zed references and a separate
upstream-version comparison. Its source closure does not imply runtime acceptance.

[§9](#9-acp-to-bridge-memory-contract) defines the current first-stage scope:
complete the ordinary ACP client and its authoritative, displayable memory before
validating FE replication. It distinguishes experimental regressions from gaps shared
with rc5 and from decisions still open. The older v0.2.8 comparison remains in the
[release parity matrix](release-parity.md); it does not establish rc5 behavior.
Migration work is tracked in
[state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2).

## 1. Baseline

The [source matrix](acp-bridge-matrix.md#1-来源和版本) owns the fixed implementation,
SDK and schema versions. TypeScript SDK 1.4.0 supplies test peers and browser types
(`package-lock.json`); its narrower type surface is discussed in §6.

Evidence cites the schema crate's doc comments as `v1/<file>.rs:<line>`, relative to
`agent-client-protocol-schema-1.7.0/src`. They are the pinned normative text. The ACP
website describes the current upstream protocol, which may be newer; it is not used to
decide a finding (§6).

Classes:

- **Defect**: the bridge drops, changes or misstates content that ACP gave, or breaks
  attyd's own stated contract (for example "carried verbatim").
- **Limitation**: a product boundary or support restriction shared with stable v1.
  Its protocol implications must be assessed explicitly; it is not automatically a
  migration regression or proof of complete ACP support.
- **Boundary**: a deliberate product scope that stays (§4).
- **Open**: the pinned schema does not settle the question; the audit states the rule
  revision 2 chose (§5).

## 2. Defects

D1–D11 retain the original revision 1 findings; D12 is the rc5 comparison's
additional control-state finding. Line references are evidence at the audited
commits, not promises of current line numbers.

| ID | Finding | Evidence (current code and docs) | ACP basis | Target (revision 2) |
| --- | --- | --- | --- | --- |
| D1 | A turn's log entry keeps only user and Agent message text. Thoughts, tool calls (status, input, output, content, diffs, locations), plans and compactions are dropped when the turn commits, and are filtered out of a load's replay. Tests assert the loss. | `canon.rs:134–163` (only message chunks), `:496–506` (`commit` keeps Agent messages); `registry.rs:756–770`; `project.rs:244` (`process` only while a prompt runs); protocol r1 S2, §3.2 step 2, §4.4; state-sync I8; `turn-collapse.pw.ts:190–199`, `lazy-process.pw.ts:30–50` | `session/load` streams "the entire conversation history" (`v1/agent.rs:4848`); `SessionUpdate` variants are conversation content (`v1/client.rs:99–159`); a compaction "fixes its timeline position" and retains a "user-displayable summary" (`v1/client.rs:214–221`) | items of every timeline kind, reduced (S8, §3.2) |
| D2 | Agent messages with different `messageId`s are concatenated into one `reply`; adjacent text of different messages merges. The test comment acknowledges it. | `canon.rs:183–200` (`turns` extends `reply`), `:202–256` (merge after concatenation); `turn-collapse.pw.ts:193–194` | "A change in `messageId` indicates a new message has started" (`v1/client.rs:614–617`) | items keep `messageId` and boundaries (§3.2) |
| D3 | User message chunks received during a live prompt are visible while it runs and dropped when it commits. | `canon.rs:496–506` (`filter(MessageKind::Agent)`) | user message chunks are session updates like any other (`v1/client.rs:100–101`) | kept as items; echo handling is presentation (§3.3) |
| D4 | A prompt that ends with an error, or whose outcome is lost with the connection, adds no entry, and its streamed messages and tool calls disappear with `op.process`. | `registry.rs:760–770` (only `TurnEnd::Completed` commits); protocol r1 §3.1 ("adds no entry") | the Agent's output was delivered; after `session/cancel` the client "SHOULD continue accepting tool call updates" (`v1/client.rs:2768–2770`) | every sent prompt settles with `end` (§3.3) |
| D5 | Conversation updates outside a running prompt are accepted and dropped; an update to an entity of a committed turn has no projection, although validation keeps such entities across prompts. | `bridge.rs:2418–2424` → `session_mirror.rs:241–257` (`Ready => Ok(())`); `semantic.rs:40–46` ("compactions can continue outside a prompt's lifetime") | no schema rule ties `session/update` to a running prompt | the tail continues the last turn; an update to a settled entity changes its entry (§3.3) |
| D6 | The hash chain covers `{prompt, reply}` text only, so two histories with different tool results, boundaries or metadata align as equal. | `canon.rs:463–470` (`chain` over `Turn`) | — (attyd's own alignment contract: a matched head means equal history) | the hash covers the whole published entry (§3.4) |
| D7 | `_meta` is dropped at every level, also from the blocks the attachment table keeps, so an attachment read or a prompt that re-sends an attachment does not return the block as received. | `canon.rs:202–256`, `:269–283`; protocol r1 §1 "carried verbatim" | `_meta` is reserved for implementations (every type, e.g. `v1/tool_call.rs:79–88`). ACP does not oblige a client to relay it; dropping it breaks attyd's own "verbatim" contract | `_meta` and annotations carried; attachments kept as received (S8, §3.1) |
| D8 | `terminal/release` removes the terminal and its output from `live`, also when a tool call embeds it; the Agent connection's end removes every session terminal, also of a session whose log stays held. | `runtime_state.rs:1188–1190`; protocol r1 §4.6; `convergence_tests.rs:974`; `acp-ui.pw.ts:2564` | "After release, the `TerminalId` can no longer be used with other `terminal/*` methods, but tool calls that already contain it, continue to display its output." (`v1/client.rs:2621–2622`) | handle and output separated: the frozen output stays while content embeds it, through release and the connection's end; not a holding need (S7, §4.6) |
| D9 | A resume-only session can be released; its next cold open resumes and turns the unknown log into a known empty one, so a client with an old head receives `from: 0, entries: []` and loses its history as if it were confirmed empty. Existing EOF tests cover only a held log. | `bridge/streams.rs:161–179` (resume counts as reattachable); `registry.rs:845–851` (`adopt_session_log`) | `session/resume` resumes "without returning previous messages" (`v1/agent.rs:1277`) | `history: "partial"`; the client may keep its earlier entries as unconfirmed UI state (§3.5) |
| D10 | An answer must name exactly all pending interactions of the operation; the web client waits for drafts of all of them. Two pages answering two different interactions can never deliver. | `admission.rs:183–207`; `use-acp.ts:813–820`; protocol r1 §6.4 | each Agent request has its own response (JSON-RPC) | each interaction answered on its own, at most once (§7) |
| D11 | An Agent request joins whatever operation runs in its scope, or starts an `agent_request`; the operation's end cancels it. A session-scoped elicitation that arrives during a `set_mode` is cancelled when the mode change ends. | protocol r1 §4.4, §6.2 "Agent requests"; state-sync §2.5 | an elicitation is scoped to a session (optionally a tool call) or to one JSON-RPC request (`v1/elicitation.rs:1484–1547`); only the permission rule ties requests to `session/cancel` (`v1/client.rs:2589–2590`). This is a product narrowing, not a wire violation | interactions live by their ACP scope; a request-scoped one has the outstanding JSON-RPC request it names as `parent`, and an elicitation naming none is refused; interactions of a session being created move into it when it is installed (§4.4) |
| D12 | Reduced controls discard the outer `_meta` of mode, config, commands and usage notifications, and of the response's `modes` object. Nested option/mode metadata and `session_info_update._meta` are not the missing fields. | experimental `runtime_state.rs:743–764`, `protocol/controls.rs:16–42,55–68,78–88`, `protocol/mod.rs:44–83`; rc5 `runtime_state.rs:1045–1068,2587–2605` retains complete control updates, and `:2614–2617` retains the complete `modes` object | ACP defines metadata on these containers (e.g. `v1/client.rs:701–710`); retaining it is attyd's own S8 contract, not a requirement to interpret it | preserve the metadata with the control state it qualifies, at its original level; current §4.5 DTOs need this addition (step 18). No control-event journal or claim of preserving the entire setup response is needed |

## 3. Limitations

An earlier version of this audit listed controls during a prompt (L1) and the absence of an explicit
close (L2) here. Both are also present in stable rc.5, so they are regressions R1 and R2 of
the [release parity matrix](release-parity.md#regressions), restored by actions
(protocol §7); they are no longer limitations.

| ID | Limitation | ACP basis | Decision |
| --- | --- | --- | --- |
| L3 | A prompt's stop reason and usage were visible only on `op` until the next operation replaced it. | `PromptResponse` (`v1/agent.rs:3096–3118`) | Resolved by D4's target: `Turn.end` carries the response. |
| L4 | A resume-only session is released like a loadable one, which loses the history the bridge held. | resume returns no messages (`v1/agent.rs:1277`) | Kept, as in v0.2.8, which released it too and showed a missing-history notice; `history: "partial"` makes it explicit (D9). Keeping such sessions held instead trades memory for history ([design tradeoffs](client-api-redesign.md#3-decisions-and-tradeoffs)). |
| L5 | Form elicitation accepts only a restricted pattern subset; even the ordinary JSON Schema pattern `^(a\|b)+$` is rejected by the quantified-group check. This is shared by rc5 and the experiment, not a v2 regression. | `v1/elicitation.rs:165–169,1120–1121` supplies a JSON Schema pattern, without the implementation's safe-pattern restriction; `elicitation_validation.rs:366–373,438–451` rejects it before registration (`bridge/agent_dispatch.rs:467–469`). The coverage ledger already calls these "safe patterns". | Complete the supported form behavior in step 16, including this ordinary pattern regression. Keep any remaining unsupported syntax explicit; do not describe the current subset as complete form support. |

## 4. Boundaries kept

These stay as they are; the audit does not widen the product:

- No persistence: no database, file or browser store as a history source; no
  cross-restart terminal output, intents or stop boundaries.
- NES, `document/*`, provider configuration: not advertised.
- Remote transports advertise no host filesystem, terminal or terminal authentication.
- MCP servers and additional directories are static launch configuration.
- Unix terminal and process rules follow [terminal semantics](usage.md#terminal-command-semantics);
  in-place file writes follow the [filesystem contract](acp-matrix/services.md).
- No vendor-specific interpretation of `_meta` or error text: carrying an opaque field
  is not interpreting it, and the UI need not render it.
- ACP methods need no one-to-one HTTP endpoint.

## 5. Questions the schema leaves open

| ID | Question | Rule revision 2 chose |
| --- | --- | --- |
| Q1 | A chunk with `messageId` A after a message B, or after a tool: one message A continued, or a new one? | A new item with `messageId` A: the timeline keeps the order and the shared ID keeps the message's identity (§3.2, rule 1). In a replay, a user item that continues the turn's opening user message does not start a turn (§3.3). |
| Q2 | Does `session/cancel` end pending session-scoped elicitations? The schema states it only for permissions. | No; they end when answered, cancelled by the Agent, or with their scope (§4.4). `session/close` ends the session, and with it all of them (§7). |
| Q3 | Does a legacy `plan` belong to the turn or to the session? The schema says only that each update replaces the plan. | The schema gives it no ID, so each turn keeps a snapshot of its last state, as v0.2.8 did; the current plan is derived from the latest snapshot. A snapshot is a record, not the current entity, and no update reduces into an older one (§3.2). ID-addressed plans are session entities. |
| Q4 | May a `tool_call_update` or compaction update arrive after the prompt's response? | Yes; the schema does not forbid it and the bridge's validation already keeps those entities. It changes the settled entry (§3.3). |
| Q5 | Agent extension notifications (`_`-prefixed methods). | Unknown requests get `method_not_found` (`bridge/agent_dispatch.rs:209`), as JSON-RPC requires. Notifications: to be confirmed by a test in step 17. |
| Q6 | A request-scoped elicitation whose `requestId` is `null`, or names a request that is not outstanding. The schema allows any `RequestId` (`rpc.rs:31–39`) and says only that the scope is "tied to a specific JSON-RPC request" (`v1/elicitation.rs:1539–1540`). | Refused with `invalid_params`, the two cases told apart in the error data; no interaction and no `global` fallback (§4.4). |
| Q7 | Does the response to a request-scoped elicitation's parent request end a URL flow the user already accepted? The schema ties the elicitation to its request (`v1/elicitation.rs:1539–1540`) and states no rule for an accepted external flow. | No. Not an ACP requirement; the parent's response cancels only interactions not yet answered, and an accepted flow keeps waiting for its completion, the connection's end or a dismissal (§4.6; release parity P5, after Zed's v1 client at `28d0a35`). |
| Q8 | The Agent's process exits with no session followed. Reads and a `global`/`catalog` stream start no connection (S6), so nothing can start the next one: how does the bridge recover? Zed keeps the dead connection, every new thread fails with `incoming_transport_closed`, and its reporter expects either that the next new thread starts a new Agent process or an explicit "agent exited — restart" affordance ([zed#62828](https://github.com/zed-industries/zed/issues/62828), open; idle connections are reaped after 30–40 s in [zed#62922](https://github.com/zed-industries/zed/pull/62922), open). | An explicit `restart` action on `global` (protocol §7), valid when `agent.phase` is `stopped` or `error`. It is the "explicit recovery" the audit's recovery row asks for, and the second of Zed's two expected outcomes; the first (a `create` that starts the connection) would make a write both refused and effectful, and would change the rule that a write without a connection is `unavailable`. A session subscription and a login's next connection still start one as before. |
| Q9 | Is there a public way to close a session from the page? Zed's `AgentConnection` has `close_session` ("free its resources; this may cancel remote work") and `should_close_sessions_on_release`, which defaults to whether the Agent supports close: a thread's release closes the session ([connection.rs](https://raw.githubusercontent.com/zed-industries/zed/main/crates/acp_thread/src/connection.rs)). | No separate button. Leaving a session releases it, and an unneeded session is closed at the Agent when the Agent advertises close (memory model §8.4), which is Zed's release rule. The `close` action (protocol §7) stays for an API client and for R2, a close during a prompt. The page keeps one meaning of leaving. |
| Q10 | What shows of a URL flow when the Agent connection ends while it waits? Zed keeps an accepted URL entry after a successful run so a late `elicitation/complete` can still arrive, emits no second response when an accepted flow is cancelled ([zed#64609](https://github.com/zed-industries/zed/pull/64609)), and on a disconnect wakes the waiters and retries nothing ([zed#65646](https://github.com/zed-industries/zed/pull/65646)); it has no record of a finished flow after a disconnect. | The flow is `ended` in the documents the connection's end publishes, and stays there: the bridge that applies a dismissal has ended with the connection, so the flow is dismissed by the next connection, which starts without it (memory model §8.3). Not a defect. |

## 6. Version differences

Listed so that none is mistaken for supported behavior:

- **TypeScript SDK 1.4.0 vs Rust schema 1.7.0.** The TypeScript schema has no
  `ToolCall.name` (`unstable_tool_call_name`), so TypeScript test peers cannot send it
  and browser types do not name it. Items carry it as received anyway (S8).
- **`unstable_protocol_v2`** (schema module `v2`) is not enabled and not in scope.
- **Newer upstream text.** Later ACP website revisions and drafts, including changes
  to compaction or to session notices, are not part of the 1.7.0 baseline. They are
  evaluated on an SDK upgrade, which already requires a review of this ledger
  (coverage ledger, "Compatibility contract").

## 7. Coverage by ACP surface

The [source matrix and domain chapters](acp-bridge-matrix.md#2-怎样使用矩阵) own
method/field coverage, implementation gaps and direct acceptance by ACP surface.
The [test ledger](acp-coverage.md) records existing test evidence. This audit owns
the findings and memory contract, not a second capability catalog.

## 8. Acceptance principle

Use the direct ACP-to-memory cases in [§9.4](#94-direct-acceptance), the numbered
[protocol acceptance cases](protocol-acceptance.md), and the
[implementation gates](state-sync.md#73-gates). Their layers remain separate.

## 9. ACP-to-bridge memory contract

This is the first layer: an ordinary ACP client executing the negotiated protocol
and retaining the state that a presentation needs. The Agent is the authority for
its session; bridge memory is the authority for what the bridge currently knows;
the FE displays that memory. A projection cannot repair content discarded before
it reaches the projection. Conversely, a missing FE view is not proof that the ACP
handler or the underlying memory is missing.

This stage does not redesign FE synchronization, add persistence, or require a
one-to-one HTTP wrapper around ACP. The declared boundaries in §4 remain explicit:
unadvertised NES, document/editor and provider-management features are not silently
counted as implemented. The locked schema's experimental MCP-over-ACP protocol is
the baseline, not a newer upstream draft.

### 9.1 Basic rules

The rules now live in [bridge memory model §1](bridge-memory-model.md#1-principles).
In particular, a load replay and live prompts build the same structure with one
reducer, and `session/load` is the only history recovery path.

### 9.2 Displayable memory

The retained state and its lifetimes are specified by the
[bridge memory model](bridge-memory-model.md): timeline entries (§3), prompt outcomes
(§4), history knowledge (§6), controls (§7) and resources (§8). This audit keeps the
evidence and status below.

### 9.3 Implementation status at the comparison commits

| Class | Evidence and required follow-through |
| --- | --- |
| Implemented execution paths | Both baselines dispatch permission, elicitation, two fs methods, five terminal methods and three MCP requests; `session/update`, `elicitation/complete` and MCP notifications have handlers (`bridge/agent_dispatch.rs:196–236`). Capabilities agree (`bridge.rs:2219–2241`; rc5 `:2921–2943`). MCP relay code is unchanged; fs ACP methods and cancellable I/O remain. This does not certify downstream retention. |
| Experimental regressions | Confirmed examples are the discarded completed/replayed timeline, stripped metadata, released terminal output, group-only answers, operation-bound interactions, and lost control/close actions. In particular, rc5 retains referenced released terminal output (`session_mirror.rs:430–478`); the experiment removes it (`runtime_state.rs:1188–1190`). The new operation binding actually cancels session forms (`bridge/publication.rs:234–292`), not merely their display. The defect table also contains shared or independent contract gaps; its IDs are not all claims of a rc5 regression. |
| Shared gaps or restrictions | Form patterns are a subset (L5). Both rc5 and the experiment cancel pending session forms on prompt cancel; Q2's narrower rule is still to implement. Real parent-RPC lifetime management is not complete in rc5 either. Neither baseline's settled URL map retains the outcome: rc5 `runtime_state.rs:1597–1623` removes the flow and publishes the remaining map, as does the experiment `:1133–1158`. Do not infer a rc5 finished-flow table or completion-event guarantee from the older v0.2.8 comparison. Step 16 covers these interaction/form/outcome paths. |
| History and recovery | rc5 also released resume-only history; D9 adds explicit knowledge instead of claiming empty completeness. Experimental recovery is triggered only by a followed session or login restart (`server.rs:222–236`), leaving no explicit recovery when neither exists. Step 14 must cover partial history and recovery with no session; the exact user-facing trigger is not prescribed here. |
| Additional retention work | D12 belongs with the existing controls/actions work in step 18. This is preservation of current controls and their metadata, not a control-update archive. |
| Still to decide or verify | Fork notifications arriving before the response, the provenance of inherited fork history, and resources naming an unknown session during concurrent creation need a clear supported boundary and tests; their routing must not be guessed. After close/reopen replaces a local session under the same ID, an old action's acknowledgement must settle only its original pending request and must not write into the replacement; verify that boundary, without changing the response-arrival ordering rule for actions of the current session. Late terminal callbacks after local session replacement also need targeted verification. These are not certified regressions or a reason to restore a public owner/CAS scheme. [Memory model §6.3 and §9](bridge-memory-model.md#9-concurrency) propose rules for fork history, installation attribution and late responses. |

The installation promises in §4.4 of the protocol remain subject to the unresolved
unknown-session association above. This stage does not invent a provisional-owner
scheme or require all possible early installation cases before fixing the confirmed
gaps. An already allocated session loading its history is a separate, testable case.

### 9.4 Direct acceptance

Drive a scripted Agent through the real coordinator and ACP transport. Assert the
bridge's owned state, resource tables and actual ACP replies against explicit fixture
values. Do not derive expected values by calling the projection, reducer under test,
or replica. These tests precede, and are independent of, later FE convergence tests.

| Case | Bridge-memory / ACP assertion |
| --- | --- |
| Negotiation and session operations | Check initialize/version/capabilities, supported authentication methods, and new/list/load/resume/fork/close/delete with actual ACP requests and replies. Preserve session metadata and opaque cursors; show current work and its real outcome. A refused or failed operation does not invent a session or successful result. Exercise mode/config and close while a prompt runs. |
| Complete live turn and load replay | Send every supported timeline kind, multiple message IDs, mixed text/attachments and metadata. After completion and after replay, the expected content and boundaries remain; no tool/thought/plan/compaction kind disappears. |
| Live/replay equivalence | The same content run live (with and without a prompt echo) and as a load replay gives equal entries, ignoring live-only prompt outcomes ([memory model §4.3](bridge-memory-model.md#43-equivalence-requirement)). |
| Failure, cancel and idle/late update | After partial output followed by error, cancellation or connection loss, content remains with the actual or unknown result. An idle update is retained; a late update changes its existing entity. The next prompt does not remove the earlier result. |
| Controls | Early update → response → later update produces the explicit expected values. Exercise sparse info, absent cost, null versus empty config, outer/nested metadata and successful action acknowledgements. Check stored state directly, including D12's metadata. |
| Independent interactions | Two pending requests exist in memory. Answering one sends only its ACP response; the other remains. A repeated answer sends nothing. Ending `set_mode` does not cancel a session form. Permission tool content is retained even without a preceding tool notification. |
| Parent RPC and cancellation | A request-scoped elicitation ends with its named RPC, not a longer multi-RPC operation. Invalid/stale parent IDs are refused explicitly. Prompt cancel answers permissions `cancelled` without erasing unrelated session forms; a delayed local cancellation callback settles only its captured request, not a replacement registration. |
| Accepted URL flow | Accept → parent response leaves a waiting flow; completion sets completed; a later connection end cannot overwrite it. Dismissed flows are not recreated by late completion. Validate session and request scopes without retaining finished parent requests. |
| Form validation | A valid form with ordinary pattern `^(a\|b)+$` is accepted, valid content can be returned, and invalid content is refused without consuming the responder. Cover the pinned primitive/enum/array/default constraints; document any remaining pattern subset. |
| Referenced resources | Release a terminal embedded in a tool: subsequent terminal methods refuse its ID, while the retained entity still resolves the exact frozen output/status. Keep referenced output through connection end. Attachments, including compaction-summary blocks, resolve unchanged while referenced and are freed when their owning content is released. |
| Physical service and cleanup | Exercise fs read/write, terminal create/output/wait/kill/release, and bidirectional MCP requests/notifications. Check results/errors, cancellation before file mutation, cancellation of a terminal waiter without killing its process, and MCP connect/disconnect cleanup. No transcript of these service RPCs is required. |
| History knowledge | A successful empty load is complete empty; a cold resume with no replay is partial, not complete empty. Retained history is not silently replaced with invented empty history. An already allocated session can receive interactions during load. Fork/unknown-session cases remain separately marked above. |
| Recovery without a session | Stop or fail the Agent before any session exists; an explicit recovery starts initialization again and permits create/auth/list. Preserve the real failure until recovery begins, and settle old pending calls without attributing them to the new connection. This does not require an FE recovery UI test. |

No hash, stream cursor, queue coalescing, raw diagnostic panel or browser screenshot
can substitute for these first-layer assertions. Their later validation is a separate
task after the bridge has the required information.
