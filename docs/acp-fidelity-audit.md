# ACP fidelity audit

Status: draft for review, 2026-10-16 (#51). It audits the browser protocol as implemented
on `main` (`a7eb3b4`, protocol revision 1) against the pinned ACP schema, and records
the evidence for [protocol revision 2](protocol.md). **The runtime code is not
migrated**: every finding below describes current behavior, and its target is a
specification the code does not meet yet. The migration steps and their acceptance are
in [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2).

## 1. Baseline

| Component | Pinned version | Source |
| --- | --- | --- |
| Rust SDK `agent-client-protocol` | 2.0.0, git rev `754d5aa1` | `Cargo.toml`, `Cargo.lock` |
| Rust schema `agent-client-protocol-schema` | `=1.7.0`, module `v1`, SDK feature `unstable` (plan operations, session compaction, session fork, end-turn usage, tool call name, MCP over ACP, LLM providers) | `Cargo.lock`, SDK `Cargo.toml` |
| TypeScript SDK `@agentclientprotocol/sdk` | 1.4.0 (test peers and browser types) | `package-lock.json` |

Evidence cites the schema crate's doc comments as `v1/<file>.rs:<line>`, relative to
`agent-client-protocol-schema-1.7.0/src`. They are the pinned normative text. The ACP
website describes the current upstream protocol, which may be newer; it is not used to
decide a finding (§6).

Classes:

- **Defect**: the bridge drops, changes or misstates content that ACP gave, or breaks
  attyd's own stated contract (for example "carried verbatim").
- **Limitation**: an attyd product rule that narrows what ACP allows. It is not an ACP
  violation; each is either kept with a reason or scheduled.
- **Boundary**: a deliberate product scope that stays (§4).
- **Open**: the pinned schema does not settle the question; the draft states the rule
  it chose.

## 2. Defects

| ID | Finding | Evidence (current code and docs) | ACP basis | Target (revision 2) |
| --- | --- | --- | --- | --- |
| D1 | A turn's log entry keeps only user and Agent message text. Thoughts, tool calls (status, input, output, content, diffs, locations), plans and compactions are dropped when the turn commits, and are filtered out of a load's replay. Tests assert the loss. | `canon.rs:134–163` (only message chunks), `:496–506` (`commit` keeps Agent messages); `registry.rs:756–770`; `project.rs:244` (`process` only while a prompt runs); protocol r1 S2, §3.2 step 2, §4.4; state-sync I8; `turn-collapse.pw.ts:190–199`, `lazy-process.pw.ts:30–50` | `session/load` streams "the entire conversation history" (`v1/agent.rs:4848`); `SessionUpdate` variants are conversation content (`v1/client.rs:99–159`); a compaction "fixes its timeline position" and retains a "user-displayable summary" (`v1/client.rs:214–221`) | items of every timeline kind, reduced (S8, §3.2) |
| D2 | Agent messages with different `messageId`s are concatenated into one `reply`; adjacent text of different messages merges. The test comment acknowledges it. | `canon.rs:183–200` (`turns` extends `reply`), `:202–256` (merge after concatenation); `turn-collapse.pw.ts:193–194` | "A change in `messageId` indicates a new message has started" (`v1/client.rs:614–617`) | items keep `messageId` and boundaries (§3.2) |
| D3 | User message chunks received during a live prompt are visible while it runs and dropped when it commits. | `canon.rs:496–506` (`filter(MessageKind::Agent)`) | user message chunks are session updates like any other (`v1/client.rs:100–101`) | kept as items; echo handling is presentation (§3.3) |
| D4 | A prompt that ends with an error, or whose outcome is lost with the connection, adds no entry, and its streamed messages and tool calls disappear with `op.process`. | `registry.rs:760–770` (only `TurnEnd::Completed` commits); protocol r1 §3.1 ("adds no entry") | the Agent's output was delivered; after `session/cancel` the client "SHOULD continue accepting tool call updates" (`v1/client.rs:2768–2770`) | every sent prompt settles with `end` (§3.3) |
| D5 | Conversation updates outside a running prompt are accepted and dropped; an update to an entity of a committed turn has no projection, although validation keeps such entities across prompts. | `bridge.rs:2418–2424` → `session_mirror.rs:241–257` (`Ready => Ok(())`); `semantic.rs:40–46` ("compactions can continue outside a prompt's lifetime") | no schema rule ties `session/update` to a running prompt | the tail continues the last turn; an update to a settled entity changes its entry (§3.3) |
| D6 | The hash chain covers `{prompt, reply}` text only, so two histories with different tool results, boundaries or metadata align as equal. | `canon.rs:463–470` (`chain` over `Turn`) | — (attyd's own alignment contract: a matched head means equal history) | the hash covers the whole published entry (§3.4) |
| D7 | `_meta` is dropped at every level, also from the blocks the attachment table keeps, so an attachment read or a prompt that re-sends an attachment does not return the block as received. | `canon.rs:202–256`, `:269–283`; protocol r1 §1 "carried verbatim" | `_meta` is reserved for implementations (every type, e.g. `v1/tool_call.rs:79–88`). ACP does not oblige a client to relay it; dropping it breaks attyd's own "verbatim" contract | `_meta` and annotations carried; attachments kept as received (S8, §3.1) |
| D8 | `terminal/release` removes the terminal and its output from `live`, also when a tool call embeds it. | `runtime_state.rs:1188–1190`; protocol r1 §4.6; `convergence_tests.rs:974`; `acp-ui.pw.ts:2564` | "After release, the `TerminalId` can no longer be used with other `terminal/*` methods, but tool calls that already contain it, continue to display its output." (`v1/client.rs:2621–2622`) | a frozen, `released` snapshot while content embeds it; not a holding need (§4.6, S6) |
| D9 | A resume-only session can be released; its next cold open resumes and turns the unknown log into a known empty one, so a client with an old head receives `from: 0, entries: []` and loses its history as if it were confirmed empty. Existing EOF tests cover only a held log. | `bridge/streams.rs:161–179` (resume counts as reattachable); `registry.rs:845–851` (`adopt_session_log`) | `session/resume` resumes "without returning previous messages" (`v1/agent.rs:1277`) | `history: "partial"`; the client may keep its earlier entries as unconfirmed UI state (§3.5) |
| D10 | An answer must name exactly all pending interactions of the operation; the web client waits for drafts of all of them. Two pages answering two different interactions can never deliver. | `admission.rs:183–207`; `use-acp.ts:813–820`; protocol r1 §6.4 | each Agent request has its own response (JSON-RPC) | each interaction answered on its own, at most once (§7) |
| D11 | An Agent request joins whatever operation runs in its scope, or starts an `agent_request`; the operation's end cancels it. A session-scoped elicitation that arrives during a `set_mode` is cancelled when the mode change ends. | protocol r1 §4.4, §6.2 "Agent requests"; state-sync §2.5 | an elicitation is scoped to a session (optionally a tool call) or to one JSON-RPC request (`v1/elicitation.rs:1484–1547`); only the permission rule ties requests to `session/cancel` (`v1/client.rs:2589–2590`). This is a product narrowing, not a wire violation | interactions are resources of their ACP scope; a request-scoped one has its operation as `parent` (§4.4) |

## 3. Limitations

| ID | Limitation | ACP basis | Decision in this draft |
| --- | --- | --- | --- |
| L1 | `set_mode` and `set_config` are refused while a prompt runs (S5: one operation per scope). | `session/set_mode` "can be called at any time … whether the Agent is idle or actively generating" (`v1/agent.rs:4898–4899`). The schema says nothing of the kind for `set_config_option`. | Kept for now and labeled as attyd's limit (§6.2). Lifting it needs a second slot or a non-occupying kind; that is a separate design step (state-sync §7, step 8). |
| L2 | No explicit close; `session/close` is sent only on release, on a held delete, and on clean-up. | ACP defines the method, not a UI duty. | Kept. Release (S6) is the close path. |
| L3 | A prompt's stop reason and usage were visible only on `op` until the next operation replaced it. | `PromptResponse` (`v1/agent.rs:3096–3118`) | Resolved by D4's target: `Turn.end` carries the response. |
| L4 | A resume-only session is released like a loadable one, which loses the history the bridge held. | resume returns no messages (`v1/agent.rs:1277`) | Kept, made explicit by `history: "partial"` (D9). The alternative, keeping resume-only sessions held, trades memory for history and is open to review (redesign §12). |

## 4. Boundaries kept

These stay as they are; the audit does not widen the product:

- No persistence: no database, file or browser store as a history source; no
  cross-restart terminal output, intents or stop boundaries (difference decisions §1).
- NES, `document/*`, provider configuration: not advertised.
- Remote transports advertise no host filesystem, terminal or terminal authentication.
- MCP servers and additional directories are static launch configuration.
- Unix terminals run through `/bin/sh` (T05); in-place file writes (T08).
- No vendor-specific interpretation of `_meta` or error text: carrying an opaque field
  is not interpreting it, and the UI need not render it.
- ACP methods need no one-to-one HTTP endpoint.

## 5. Open questions

| ID | Question | Rule this draft chose |
| --- | --- | --- |
| Q1 | A chunk with `messageId` A after a message B: one message A continued, or a new one? | A new item with `messageId` A, after B: order is kept and the shared ID still says it is one ACP message (§3.2). |
| Q2 | Does `session/cancel` end pending session-scoped elicitations? The schema states it only for permissions. | No; they end when answered, cancelled by the Agent, or with their scope (§4.4). |
| Q3 | Does a legacy `plan` belong to the turn or to the session? The schema says only that each update replaces the plan. | Each turn keeps the plan's last state within it; the current plan is the last plan item. Nothing is lost and replacement holds (§3.2). |
| Q4 | May a `tool_call_update` or compaction update arrive after the prompt's response? | Yes; the schema does not forbid it and the bridge's validation already keeps those entities. It changes the settled entry (§3.3). |
| Q5 | Agent extension notifications (`_`-prefixed methods). | Unknown requests get `method_not_found` (`bridge/agent_dispatch.rs:209`), as JSON-RPC requires. Notifications: to be confirmed by a test in step 9. |

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

Status of revision 1 (current code) and of revision 2 (this draft). "Kept" means
revision 2 does not change the surface.

| ACP surface | Pinned basis | Revision 1 | Revision 2 |
| --- | --- | --- | --- |
| `initialize`, capability negotiation | `v1/agent.rs:4780–4789` | conforming: version check, client capabilities, `global.agent.initialize` verbatim | kept |
| `authenticate`, terminal auth, `logout` | `v1/agent.rs:4791–4826` | conforming; one operation in `global` each | kept |
| `session/new` | `v1/agent.rs:4828–4839` | conforming; early updates staged | early timeline updates also become items (§3.3) |
| `session/load` | `v1/agent.rs:4841–4850` | replay reduced to message text (D1, D2) | replay reduced to items; `history: "complete"` |
| `session/resume` | `v1/agent.rs:1277`, `:4875–4880` | cold resume shows a known empty log (D9) | `history: "partial"` |
| `session/fork` (unstable) | `v1/agent.rs:4863–4873` | conforming, with r1 log content | the fork's replay, else the source's log and status |
| `session/list`, `session/delete` | `v1/agent.rs:4852–4861` | conforming | kept |
| `session/close` | `v1/agent.rs:4882–4887` | sent on release and delete (L2) | kept |
| `session/set_mode`, `session/set_config_option` | `v1/agent.rs:4889–4903` | conforming when idle; refused during a prompt (L1) | kept (L1 labeled) |
| `session/prompt` | `v1/agent.rs:4905–4916` | result on `op` only; a failure drops output (D4) | `Turn.end`; every sent prompt settles |
| `session/cancel` | `v1/agent.rs:5073–5084` | conforming; pending permissions answered `cancelled` | kept |
| message and thought chunks | `v1/client.rs:100–105`, `:604–632` | text only, boundaries lost, thoughts dropped (D1–D3) | items with `messageId`, `_meta`, annotations |
| `tool_call`, `tool_call_update` | `v1/tool_call.rs:29–88`, `:177–211` | folded while running, dropped after (D1) | items, upserted session-wide by `toolCallId` |
| `plan`, `plan_update`, `plan_removed` | `v1/plan.rs:20–37`, `v1/client.rs:110–126` | per running turn, dropped after (D1) | items (Q3) |
| `compaction_update`, `compaction_summary_chunk` (unstable) | `v1/client.rs:139–158`, `:210–299` | folded while running, dropped after; idle updates dropped (D1, D5) | items, upserted by `compactionId`, also outside prompts |
| control updates (commands, mode, config, info, usage) | `v1/client.rs:127–138` | conforming, also when idle (§4.5) | kept |
| `session/request_permission` | `v1/client.rs:2585–2593`, `:863–874` | joins the running op; group answers (D10, D11) | session interaction; its `toolCall` reduced into the timeline; answered alone |
| `elicitation/create` (form, URL), `elicitation/complete` | `v1/elicitation.rs:1484–1547`; `v1/client.rs:2774–2777` | joins the running op (D11); URL flows conforming | interaction of its ACP scope; URL flows kept |
| `fs/read_text_file`, `fs/write_text_file` | `v1/client.rs` (client methods) | conforming (stdio) | kept |
| `terminal/*` | `v1/client.rs:2594–2645`; `v1/tool_call.rs:575–582` | output removed on release (D8) | frozen snapshot while embedded |
| `$/cancel_request` | `v1/protocol_level.rs:82` | conforming for fs, terminal wait, MCP and interactions | kept |
| MCP over ACP (unstable) | `v1/client.rs:2778–2784` | conforming; not projected | kept |
| extension methods | `v1/client.rs:2785–2792` | requests refused with `method_not_found`; notifications Q5 | kept |
| `_meta` on any type | every type's `meta` field | dropped from log and attachments (D7) | carried |
| NES, `document/*`, providers | unstable | not advertised (§4) | kept |

## 8. Acceptance principle

Replica-equals-projection checks cannot find these defects: the projection itself
dropped the content. Every migration step therefore adds **ACP input → retained
entity** tests, written against this ledger and not against the projection: a scripted
Agent sends ACP updates, and the test asserts which items and fields the log or the
tail holds afterwards, across completion, failure, cancel, load, resume, release, late
updates, answers from several clients, reconnects and queue coalescing. The per-event
convergence gate (state-sync §4.1) still runs on top. The cases are listed with their
steps in [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) and in
[client-api-redesign.md §11](client-api-redesign.md#11-acceptance).
