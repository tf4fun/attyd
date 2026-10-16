# Release parity

Status: settled, 2026-10-16 (#51). It compares the user-visible capabilities
of the last release with the current `main` and with [protocol revision 2](protocol.md),
so that the protocol's simplification removes no released capability.

| Point | Version | Source |
| --- | --- | --- |
| release | **v0.2.8**, `a5b07ff`, 2026-09-18 | the tag; its `docs/acp-coverage.md`, `docs/active-turn-runtime.md`, web and browser tests |
| current | `main`, `a7eb3b4`: protocol revision 1 | [fidelity audit](acp-fidelity-audit.md) |
| target | protocol revision 2 (target specification; not implemented) | [protocol.md](protocol.md); migration in [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) |

**Classes.**

- **Kept**: `main` and the target have it as v0.2.8 did.
- **Replaced**: the target has the same capability by a different, simpler protocol.
  Internal models and endpoints of v0.2.8 are not restored.
- **Regression**: v0.2.8 had it and `main` lost it. The target restores it, in the
  migration step named; it is not an accepted limitation.
- **Decision**: a product tradeoff where the target does not simply restore v0.2.8.
  Each is settled below with the rule the target keeps.

**Acceptance.** A regression is closed only by a test that exercises the v0.2.8
behavior against the target. A test that was deleted, or changed to assert that the
capability is gone, does not count: such tests change back in the step that restores
the capability (state-sync §7).

## Matrix

### History, tool details and folding

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Completed turns keep thoughts, tool calls (status, content, diffs, terminals, locations, raw input and output), plans, compactions | kept in the in-memory baseline (`active-turn-runtime.md`, "Decision") | dropped at commit (D1) | items (protocol §3.1–§3.2) | Regression → step 11 |
| Cold-loaded history shows the same kinds | replay retained whole (`history_replay.rs`) | messages only (D1) | replay reduced to items | Regression → step 11 |
| A failed or interrupted turn keeps its output and shows the error | `error` timeline item, output kept (`web/src/lib/state.ts:2413`) | no entry, output dropped (D4) | entry with `end.error` | Regression → step 12 |
| Stop reason and usage per completed turn, while the session stays held | `stop` item (`state.ts:1540`); lost after a cold load (`active-turn-runtime.md`) | on `op` until the next operation (audit L3) | `Turn.end`; lost after a cold load, as before | Regression → step 11 |
| Tool cards collapsed by default, the user's disclosure retained; compaction disclosure | client state | while running only | client state over settled items (C1) | Regression → step 11 (content), Kept (UI) |
| Thread search over completed turns' content | over the timeline | over message text only | over items | Regression → step 11 |
| Change review of reported diffs | from tool contents | while running only | from settled tool items | Regression → step 11 |
| Live plan beside the composer; ID plans in place | per running turn; ID plans upserted in place | per running turn | current plans derived from items (§3.2) | Kept |
| Updates between prompts | accepted into the projection (`active-turn-runtime.md`) | dropped (D5) | the tail, then the last entry | Regression → step 13 |
| Late tool or compaction updates after a turn | applied to the session-scoped entity | dropped (D5) | rewrite the settled entry | Regression → step 13 |

### Messages and attachments

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Message boundaries by `messageId`; a tool between chunks splits the entry | coverage ledger, "message chunks" | merged into one reply (D2) | message items, one per contiguous run (§3.2 rule 1) | Regression → step 11 |
| User message chunks of a live turn | shown | dropped at commit (D3) | items | Regression → step 11 |
| Image, audio, resource prompts; picker, drop, paste, `@` files | yes | yes | yes | Kept |
| Attachment blocks shown and re-sent | inline blocks | `AttachmentRef`, `_meta` dropped (D7) | `AttachmentRef`, block kept as received | Replaced; D7 fixed in step 11 |
| Copy an Agent response as Markdown | yes | yes | yes | Kept |

### Interactions

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Answer each permission or elicitation on its own | `POST …/interactions/{id}/response` | all pending interactions of an operation at once (D10) | `answer` action per interaction, at most once (§7) | Regression → step 16 |
| Session- and request-scoped elicitations stay distinct | coverage ledger, "Form elicitation" | joined to the running operation (D11) | ACP scope; parent is the JSON-RPC request (§4.4) | Regression → step 16 |
| URL elicitation with consent and completion | yes | yes | yes, with the ACP scope | Kept |
| A finished URL flow shows whether it completed or was given up, also on an asynchronous completion | `acp/elicitation_complete` / `acp/elicitation_aborted` shown as a transient result (`web/src/lib/state.ts:2967–3003`, `ExternalFlowCard`; `tests/state.test.ts:2502–2538`, `:2570–2622`); the bridge removed the flow | only `waiting`; removal tells neither outcome | the flow stays with `status` `completed` or `ended` until dismissed; only `waiting` changes; a parent request's response does not end an accepted flow (protocol §4.6, P5) | **Regression R4** → step 16 |
| Permission card merges the request's tool call | yes | yes, while running | yes, also settled | Kept |

### Session operations and settings

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Change mode or configuration while a prompt runs | yes (`tests/browser/acp-ui.pw.ts:2694`, "Verbose" switch during a running prompt) | refused until the prompt ends (r1 §6.2) | `set_mode`/`set_config` actions (§7) | **Regression R1** → step 18 |
| Close a session, also while it runs, after a confirmation | yes (same test: Escape sends nothing, confirming sends one close and returns to the project) | no close (r1 §10, "release when not needed") | `close` action (§7); `end: closed` | **Regression R2** → step 18 |
| Unobserved sessions released after `--session-unobserved-timeout` (code default 300 s) | closed, also while running, if close is supported | released if loadable or resumable, never while needed | as `main` | Kept (`main` narrows it safely) |
| Reopen a resume-only session | missing-history notice | known empty history (D9) | `history: "partial"` | Regression → step 14 |
| Create, fork with fallback, delete with confirmation, list and project grouping | yes | yes | yes | Kept |
| Cancel; prompt queue with "Send now" | yes | yes | yes | Kept |
| Commands, session info, context usage | yes | yes | yes | Kept |

### Terminals and authentication

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Live terminal output in tool cards | yes | yes | yes | Kept |
| Output after `terminal/release` stays in the tool card | yes (coverage ledger, "Terminal lifecycle") | removed on release (D8) | frozen output while embedded (§4.6) | Regression → step 15 |
| Output after the Agent connection ends, for a session that stays held | kept with the projection | removed | kept (§4.6) | Regression → step 15 |
| Authenticate, logout, terminal login with input, resize, cancel | yes | yes | yes | Kept |

### Markdown export

| Section | v0.2.8 (`web/src/lib/thread-markdown.ts`) | main | Target source |
| --- | --- | --- | --- |
| user message, with `messageId` | `:48–52` | settled: text only | message items, role `user` |
| Agent message and thought, with `messageId` | `:53–60` | settled: one merged reply, no thoughts | message items, roles `agent`, `thought` |
| tool: title, name or kind, status, locations | `:61–72` | running turn only | tool items |
| tool content: blocks, diffs, terminals | `:73–75`, `:148–170` | running turn only; terminals gone on release | tool items; `live.terminals` snapshot |
| tool raw input and output | `:76–77` | running turn only | tool items |
| plans: legacy, items, file, markdown, removal | `:80–81`, plan helpers | running turn only | plan items |
| compaction: status, ID, summary, error | `:82–90` | running turn only | compaction items |
| content blocks: text, image, audio, resource link, resource, annotations | `:107–142` | yes; attachments as references | items; attachment read |
| stop reason and usage | `:93–99` | not in the log | `Turn.end.response` |
| error | `:100–101` | not in the log | `Turn.end.error` |

**Export acceptance** (step 20, after the content steps 11, 12, 15; redesign §11 case 33): a scripted session with
every row above (two messages of one role, a thought, a tool with locations, a diff, an
embedded terminal released before export, raw input and output, a legacy and an ID
plan with a removal, a compaction with a summary and one that failed, an image and an
embedded resource with annotations, a completed turn with usage and a failed turn) is
exported from a settled log, from the tail, and after a reconnect; the Markdown
contains each section, and terminal output is not "output unavailable". The test is
written from the ACP input, not from the projection.

### Diagnostics

| Capability | v0.2.8 | main | Target | Class |
| --- | --- | --- | --- | --- |
| Agent stderr panel (also MCP servers' stderr) | `bridge/stderr` events, `web/src/app.tsx:689–693` | bridge log only (`src/bridge.rs:1401–1403`) | `global.live.diagnostics.agentStderr`, bounded (§4.7) | **Regression R3** → step 19 |
| MCP-over-ACP activity: open connections and relayed messages, collapsible | `acp/mcp_connection`, `acp/mcp_message`, `web/src/app.tsx:701–704` | not projected; only the static server list | `global.live.diagnostics.mcp`, last 200 messages (§4.7) | **Regression R3** → step 19 |
| Raw JSON of tool payloads and Agent details | yes | yes, while running | on settled items too | Kept |
| "Other session events" raw JSON panel | events of sessions other than the open one (`web/src/app.tsx:695–699`) | none | — | **Decision P1** |

## Regressions

| ID | Capability | Evidence | Target |
| --- | --- | --- | --- |
| R1 | `set_mode`, `set_config` while a prompt runs | v0.2.8 `tests/browser/acp-ui.pw.ts:2694–2736`; ACP allows the mode at any time (`v1/agent.rs:4898–4899`) | actions that occupy nothing (S4, §7) |
| R2 | close, also during a prompt, with a confirmation | the same test; v0.2.8 `POST /api/v1/sessions/{id}/close` | the `close` action and `end: closed` (§7) |
| R3 | Agent stderr and MCP-over-ACP activity | v0.2.8 `web/src/app.tsx:689–704` | bounded `global.live.diagnostics` (§4.7) |
| R4 | completed and given-up URL flows are distinguishable | v0.2.8 `web/src/lib/state.ts:2967–3003`, `tests/state.test.ts:2502–2538`, `:2570–2622` | the outcome as the flow's `status` (§4.6) |

Their acceptance is redesign §11 cases 28, 29 and 37 (R1, R2), 32 (R3) and 11 (R4). The other regressions are the audit's defects D1–D5, D8–D10, restored as listed above.

## Decisions

| ID | Question | Decision |
| --- | --- | --- |
| P1 | v0.2.8 showed raw events of other sessions, a consequence of its single-session event stream. | Not restored. Each session's state is its own document, and `global.sessions` shows every held session's operation and pending interactions; a client opens any of them. Restoring a raw event list would need an event log the target does not keep. |
| P2 | Stop reason, usage and terminal output of turns observed live are lost when the session is released and loaded again, because ACP replays neither. | Same as v0.2.8, which lost them on a cold load as well. Not kept beyond the session's documents, since that needs persistence. |
| P4 | How a URL flow's outcome reaches clients. v0.2.8 showed it only to pages connected when it happened and kept no record. | The flow stays in `urlFlows` with its outcome until a client dismisses it or the session scope that shows it ends; only `waiting` changes, so a late completion or connection end never overwrites an outcome; it then shows after a reconnect too, which v0.2.8 did not promise and the target does not require. No finished-flow table, request record or reconnect log. v0.2.8 also aborted session flows on a prompt cancel; ACP states no such rule and the target does not (audit Q2). |
| P3 | v0.2.8's unobserved timeout could close a session while it ran; `main` and the target release only a session nothing needs. | Keep the target's rule: a running prompt is never cut by a timer. |
| P5 | Whether the answer to a request-scoped elicitation's parent request ends a URL flow the user already accepted. The pinned schema ties the elicitation to "a specific JSON-RPC request" (`v1/elicitation.rs:1539`) and states no rule for an accepted external flow; it is a product choice, not an ACP requirement. | **Settled: it does not.** The parent's response, success or error, cancels only interactions not yet answered; an accepted flow stays `waiting` until a completion, the connection's end, a dismissal or the end of its document, and the answered request is not remembered. This follows Zed's ACP v1 client at [`28d0a35`](https://github.com/zed-industries/zed/commit/28d0a3547f16b9609b301b48ab363bfca649c864), used as an implementation reference only (our ACP pins stay): [authenticate](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/agent_servers/src/acp/v1.rs#L1545-L1563) does not touch accepted flows when its request returns; the shared thread model's [state changes](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/acp_thread/src/acp_thread.rs#L610-L646) and [`finish_request` / `cancel_request`](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/acp_thread/src/acp_thread.rs#L774-L798) keep accepted URL flows on a finish and end them only on a cancel; [completions](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/agent_servers/src/acp.rs#L757-L780) route by `elicitationId`, only from accepted. Not taken: Zed's display, which [shows](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/agent_ui/src/conversation_view/elicitation.rs#L1822-L1829) only pending and accepted flows (R4 needs both outcomes), and its session cancel ending URL flows (audit Q2 stands). |
