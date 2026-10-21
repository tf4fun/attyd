# Bridge memory model

Status: **layer-1 specification, proposed 2026-10-21; not implemented.** This document
is the single definition of what the bridge keeps in memory as an ACP client and how
ACP input changes it. It replaces the reduction, settling and lifetime rules previously
spread across [protocol.md](protocol.md) §3–§4 and
[audit §9.1–§9.2](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract); where they
disagree, this document wins.

Related documents keep separate roles:

| Document | Role |
| --- | --- |
| this document | layer 1: ACP → bridge memory (normative) |
| [protocol.md](protocol.md) | layer 2: how that memory is replicated to clients (revision pending) |
| [acp-fidelity-audit.md](acp-fidelity-audit.md) | defects D1–D12 and their evidence; direct acceptance (§9.4) |
| [acp-bridge-matrix.md](acp-bridge-matrix.md) | per-method / per-field source inventory |
| [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) | implementation steps and gates |

The ACP baseline is the locked schema 1.7.0 and SDK revision named in the
[source matrix](acp-bridge-matrix.md#1-来源和版本). Citations `v1/<file>.rs:<line>` refer
to `agent-client-protocol-schema-1.7.0/src`. The reference client is Zed at
[`9f63850`](https://github.com/zed-industries/zed/tree/9f63850f7affbd57772a4f99554d4fd333b50a6f)
(`crates/acp_thread/src/acp_thread.rs`, `crates/agent_servers/src/acp/v1.rs`); see §11.

## 1. Principles

1. **An ordinary ACP client, in memory only.** The bridge executes the negotiated ACP
   client contract and keeps no database, file or browser store. The Agent is the only
   persistent authority; bridge memory is the authority for what the bridge knows now.
2. **`session/load` is the only history recovery path.** History the bridge did not
   observe live can come only from a `session/load` replay. Nothing else (a client
   cache, a resume, a guess) supplies or confirms it.
3. **One structure, one reducer.** A load replay and the updates of later prompts are
   reduced by the same function into the same session structure. No part of the
   structure can be produced by only one of the two paths. Facts only the live path
   observes (the prompt the bridge sent, its `PromptResponse`) are optional additions
   to that structure (§4), never a different shape.
4. **Reduce content, not meaning.** Updates are folded into their entities. Nothing is
   removed because a prompt ended, failed or was cancelled, because a field has no UI,
   or because a client is not connected. `_meta` and annotations are kept at the level
   they arrived on and are not interpreted.
5. **Real lifetimes.** A resource lives by its ACP scope and ends at the ACP event that
   ends it, never because an unrelated operation ended. A handle (a responder, a
   process) ends separately from the content that references it.

## 2. Structure

```text
Bridge
|- connection: phase, InitializeResponse, auth state, structured failure
|- catalog: current session/list page(s), cursor
|- outstanding: JSON-RPC requests sent and not yet answered (§8.1)
`- sessions: SessionId -> Session

Session
|- generation          internal; new each time the bridge installs this ID (§9)
|- cwd, info
|- history             unknown | partial | complete (§6)
|- entries: [Entry]    the timeline, in order (§3)
|- prompts: [Prompt]   live-only additions (§4)
|- controls            reduced current controls (§7)
`- resources           interactions, URL flows, terminals, attachments (§8)
```

`entries` is a flat list. It holds no turn container; turns are derived (§5).

## 3. Timeline entries

### 3.1 Entry kinds

```ts
type Entry = MessageEntry | ToolEntry | PlanEntry | CompactionEntry;

interface MessageEntry {
  kind: "message";
  role: "user" | "agent" | "thought";   // user_message_chunk, agent_message_chunk, agent_thought_chunk
  messageId?: string;                   // first present messageId of its chunks
  chunks: { content: ContentBlock; _meta?: unknown }[];
}

interface ToolEntry {
  kind: "tool";
  call: ToolCallUpdate;   // reduced sparse state; every field but toolCallId may be absent
}

type PlanEntry =
  | { kind: "plan"; legacy: Plan }                         // `plan` (no ID), §3.4
  | { kind: "plan"; update: PlanUpdate | PlanRemoved };    // by planId

interface CompactionEntry {
  kind: "compaction";
  compactionId: string;
  status: CompactionStatus;
  summary: { content: ContentBlock; _meta?: unknown }[];
  error?: string;
  _meta?: unknown;
}
```

Every ACP value inside an entry is the pinned schema type, `_meta` included. Content
blocks are kept as received, including their bytes; how clients receive large blocks is
a layer-2 matter.

Tool state is the sparse `ToolCallUpdate` (`v1/tool_call.rs:220–236`), not `ToolCall`:
the first thing the bridge sees for an ID may be a `tool_call_update` or a permission's
`toolCall` holding only `toolCallId`, and `ToolCall` requires a `title` (`:33`). A field
the Agent has not provided stays absent; the bridge invents no title, kind or status.

### 3.2 Reduction rules

Three rules decide every timeline update.

1. **Identity.** A tool call, an ID-addressed plan and a compaction have exactly one
   entry, at the position of their first update; every later update for the ID changes
   that entry wherever it is, also after its prompt ended (ACP gives these IDs session
   scope: `v1/tool_call.rs:30`, `v1/plan.rs:79`, `v1/client.rs:231`).
   A message chunk joins the **last** entry when it is a message of the same role and
   the two `messageId`s do not differ (equal, or either absent); otherwise it starts a
   new message entry. So a chunk after an intervening entry starts a new entry even with
   the same `messageId`: order is kept, the shared ID keeps identity
   (`v1/client.rs:614–617`; Zed `can_merge_message_chunks`). An entry without a
   `messageId` takes the first one a joining chunk carries.
2. **Patch.** A tool update applies the field rule of `ToolCall::update`
   (`v1/tool_call.rs:177–205`): absent **or `null`** keeps, a value replaces, collections
   are replaced whole (`content: []` clears). The call's `_meta` follows the same rule.
   A compaction's `summary`, `error` and `_meta` are three-state: absent keeps, `null`
   clears, a value replaces. Nothing is deep-merged.
3. **Fragments.** A text chunk is appended to the previous chunk of its entry when that
   chunk is text and all other fields (annotations, block `_meta`, chunk `_meta`) are
   equal; otherwise it is a new chunk. Equal content therefore gives equal entries
   however it was split into chunks.

| ACP update | Reduction |
| --- | --- |
| `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk` | rule 1 (join or start), rule 3; user chunks after a sent prompt also pass the echo rule (§4.1) |
| `tool_call` | `ToolCallUpdate::from(call)` (`v1/tool_call.rs:434–465`), patched into the ID's entry by rule 2, or starts it |
| `tool_call_update` | patches the ID's entry by rule 2; an unknown ID starts an entry with exactly its fields |
| `plan` | §3.4 |
| `plan_update` | upserts the `planId` entry; the update is the plan's state |
| `plan_removed` | sets the `planId` entry to the removal, in place; an unknown ID starts a removal entry; a later `plan_update` revives the same entry |
| `compaction_update` | upserts the `compactionId` entry; `status` replaces; `summary`, `error`, `_meta` by rule 2; a `summary` value becomes chunks without chunk `_meta` |
| `compaction_summary_chunk` | appends `{ content, _meta }` to the entry's `summary` by rule 3 (`v1/client.rs:296–316`) |
| control updates | not entries; §7 |

Further rules:

- A permission request's `toolCall` is a `ToolCallUpdate` and is reduced into the
  session's entries like one (rule 2), even without a preceding tool notification.
- Updates are accepted whether or not a prompt runs (D5). An idle update is reduced by
  the same rules; there is no separate tail.
- An update the bridge's semantic validation rejects is dropped and reaches no entry.
  An update for a session the bridge does not hold is dropped.

### 3.3 Determinism

`entries` is a pure function of the sequence of reduced inputs: timeline updates, the
permission `toolCall`s, and the sent prompts of §4.1. The same sequence gives the same
entries, live or replayed. Inputs that differ in IDs, `_meta`, results or order give
different entries; the bridge does not make them equal.

### 3.4 Legacy plans

The legacy `plan` has no ID; each update replaces the whole plan (`v1/plan.rs:36–37`).
It is kept per derived turn (§5), as v0.2.8 showed it: a `plan` replaces the legacy plan
entry of the current turn in place, or, if the current turn has none, appends a new
one. The current legacy plan is the one of the latest turn that has one. Older turns'
entries are records and are never updated. Zed keeps only one session-level legacy
plan; attyd keeps the per-turn record so that history shows how the plan evolved.

## 4. Live-only additions

### 4.1 The sent prompt

When the bridge sends `session/prompt`, it first reduces the prompt as user content:
one message entry, role `user`, no `messageId` (the pinned `PromptRequest` has none),
one chunk per prompt block, with an internal flag `sent` (not part of content).

Some Agents echo the prompt as `user_message_chunk`s; others do not. The **echo rule**
keeps one entry either way (after Zed, `handle_session_update`, `UserMessageChunk`):

- While the last entry is the `sent` user message, an incoming user chunk whose
  content equals the next not-yet-matched prompt block is an echo: it adds no chunk; if
  the entry has no `messageId`, it takes the chunk's.
- Any other user chunk is reduced normally. Following Zed, a chunk with a `messageId`
  does not join a `sent` entry that still has none and did not match; it starts a new
  user entry.
- The `sent` flag ends when every block was matched or another entry follows.

An Agent that replays a different form of the prompt in a later load (for example a
resource as text) produces a different user entry in that load. That is expected; the
load is authoritative (§6.2).

### 4.2 Prompt outcomes

```ts
interface Prompt {
  entry: number;     // index of the user entry it sent (§4.1)
  end?:              // absent while it runs
    | { response: PromptResponse }   // verbatim: stopReason, usage, _meta
    | { error: AcpError }            // the Agent answered with an error
    | { interrupted: true };         // no answer: the connection ended (outcome unknown)
}
```

- Every prompt the bridge sent has a `Prompt`, whatever its outcome. Content already
  received is never withdrawn (D4); a cancelled prompt keeps its content and its
  `cancelled` stop reason.
- A lost answer is `interrupted`, never an invented stop reason.
- A later operation does not erase an earlier outcome.
- A load replaces `entries` and clears `prompts` (§6.2): a replay streams updates, not
  `PromptResponse`s, and the bridge does not fabricate one.

### 4.3 Equivalence requirement

For any ACP content, the entries built from the live path (sent prompt, optional echo,
updates) and from a load replay of the same content are equal, ignoring `prompts` and
the internal `sent` flag. This is a required test (§10), and the reason no structure
may depend on which path produced it.

## 5. Derived turns

A turn is not stored. It is computed from `entries`, identically for both paths: a turn
starts at each user message entry, except one whose present `messageId` equals that of
the user entry that started the current turn (a user message interrupted by another
entry is still one turn). Entries before the first user entry form a first turn.

A `Prompt` attaches to the turn its `entry` starts. Derived turns are used by the legacy
plan rule (§3.4), by presentation (folding) and by export; they have no identity of
their own in memory.

## 6. History knowledge and materialization

### 6.1 Knowledge

| `history` | Meaning | Set by |
| --- | --- | --- |
| `unknown` | the bridge holds no history for the session | a session that is being opened, until its load or resume ends |
| `complete` | `entries` start at the session's beginning | `session/new` (empty); a successful `session/load` ("the entire conversation history", `v1/agent.rs:4845–4848`) |
| `partial` | `entries` may lack what came before | `session/resume` of a session whose history was unknown ("without returning previous messages", `v1/agent.rs:1277`); `session/fork` (§6.3) |

An empty `complete` history and an `unknown` one are different and are never confused
(D9). New content after a resume is appended to a `partial` history as usual.

### 6.2 Load

A load reduces the replay with the §3 reducer into a **candidate** session structure,
private to that attempt. On a successful response the candidate replaces `entries`,
clears `prompts` and sets `complete`, in one step. On failure the candidate is
discarded and the installed state is unchanged. The bridge never loads a session it
holds with a known history, and performs no load after a prompt: live content is
already in memory by §4.

### 6.3 Fork (proposal)

`session/fork` "creates a new session based on the context of an existing one"
(`v1/agent.rs:1105–1108`); it promises no replay. Proposal: the target starts with a
copy of the source's `entries` at admission (without `prompts`), `history: partial`
(inherited, not confirmed by the Agent). Updates the Agent sends for the target before
or after the response are reduced into it by §3. A later cold reopen of the target uses
`session/load` like any session (§6.2) and becomes `complete`. Resources are not copied.

### 6.4 Release

A session that nothing needs (§8.4) may be released after the configured delay, only if
the Agent can load or resume it. Release drops its whole structure. Reopening goes
through §6.2 (load) or §6.1 (resume, `partial`); no client state restores it.

## 7. Controls

`controls` is the reduced current state, folded in this order:

1. control notifications that arrive before the `session/new`, `load`, `resume` or
   `fork` response, in arrival order;
2. that response, overriding exactly the fields it provides (`modes`, `configOptions`);
3. later notifications and successful action acknowledgements, in arrival order,
   whether or not a prompt runs. A failed action folds nothing.

| ACP source | Reduction |
| --- | --- |
| response `modes` | sets `modes` (with the object's `_meta`) |
| `current_mode_update` | sets `modes.currentModeId` |
| response `configOptions`, `config_option_update` | replace `configOptions` whole; `null` (never provided) and `[]` differ |
| `available_commands_update` | replaces `commands` whole |
| `session_info_update` | `title`/`updatedAt`: absent keeps, `null` clears, a value sets |
| `usage_update` | sets `used`, `size`; `cost` absent or `null` keeps the last known cost |
| successful `session/set_mode` | sets `modes.currentModeId` to the requested ID |
| successful `session/set_config_option` | replaces `configOptions` with the response's |

Each control keeps the `_meta` of the latest notification or response that set it,
next to the value it qualifies (D12). No control-event history is kept. Clients prefer
`configOptions` over legacy `modes` when it is non-null.

## 8. Resources

### 8.1 Outstanding requests

The bridge records each JSON-RPC request it sends to the Agent — `{ id, method,
sessionId?, generation? }` — from send until its response, and removes it then. This is
the only request record. It parents request-scoped elicitations (§8.2), keeps a session
needed while a control or close request is outstanding (§8.4), and guards late
responses (§9).

### 8.2 Interactions

An interaction is an Agent request waiting for the user: `session/request_permission` or
`elicitation/create`. It holds the request verbatim and its original responder.

| Request | Scope | Ends (besides its answer) |
| --- | --- | --- |
| permission for session S | S | Agent cancel (`$/cancel_request`); a `session/cancel` or `session/close` of S answers it `cancelled` (`v1/client.rs:2589–2590`); S not installed; S closed or deleted; connection end |
| elicitation, session scope S (optionally a `toolCallId`) | S | Agent cancel; S not installed; S closed or deleted; connection end. A `session/cancel` does **not** end it (Q2) |
| elicitation, request scope R | outstanding request R | Agent cancel; R's response (answered with its cancel outcome); connection end |

- A request-scoped elicitation whose `requestId` is `null` or names no outstanding
  request is refused with `invalid_params` and never becomes an interaction (Q6). A
  session-scoped request for a session neither held nor being installed is refused the
  same way.
- Each interaction is answered independently and at most once (D10). An answer that
  does not fit the request (an option not offered, a form value failing its schema) is
  refused without consuming the responder.
- An operation's end ends no interaction (D11).

### 8.3 URL flows, terminals, attachments

- **URL flow.** Accepting a URL elicitation ends the interaction and creates a flow
  `{ request, elicitationId, status: waiting }` in the same scope. `elicitation/complete`
  for its `elicitationId` sets `completed`; the connection's end sets `ended`; a finished
  status never changes again. The parent request's response does not end an accepted
  flow (Q7). A dismissal or the end of its scope removes it.
- **Terminal.** A `terminal/create` creates a handle (process, ID usable in `terminal/*`)
  and output (bounded tail, truncation flag, exit status). `terminal/release` or the
  connection's end kills the process and ends the handle; the output and exit status are
  frozen and kept while any entry embeds the terminal
  (`v1/client.rs:2621–2622`; Zed keeps the terminal entity referenced by its tool call
  after `release_terminal`). Unreferenced frozen output is freed (D8). A load cannot
  restore terminal output: an embedded terminal ID with no output in memory is shown as
  unavailable, not invented.
- **Attachment.** A content block with bytes, from the Agent or a prompt, is kept as
  received, `_meta` included, while an entry or interaction references it (D7).

### 8.4 Needed sessions

A session is needed while it is observed, occupied by an operation, has an outstanding
control or close request, or has a resource that can still act: a pending interaction,
a terminal with a handle, a `waiting` URL flow. Frozen output and finished flows do not
keep it needed.

## 9. Concurrency

- **One installation at a time.** `create` and `fork` both use the global operation
  slot, so at most one session is being installed at any moment. A resource naming a
  session ID the bridge does not hold yet, arriving while an installation runs, belongs
  to that installation: it is shown there and moves into the session when installed, or
  is cancelled if installation fails. With no installation running it is refused. This
  closes the unknown-session attribution question of audit §9.3 without a provisional
  owner scheme.
- **Generations.** Each installation of a session ID (new, load, resume, fork) gets a
  fresh internal `generation`. An outstanding request records the generation it was sent
  for; its response applies only if the session still has that generation (Zed guards
  late prompt responses the same way with `turn_id`). After close and reopen under the
  same ID, an old acknowledgement settles only its own request and changes nothing.
  Generations are never exposed to clients.
- **Actions during a prompt.** `set_mode`, `set_config_option` and `close` may be sent
  while a prompt runs (`v1/agent.rs:4898–4899`, `:1432–1436`); each folds or applies when
  its own response arrives. Prompt, fork, delete and load remain mutually exclusive per
  session.

## 10. Acceptance

The cases in [audit §9.4](acp-fidelity-audit.md#94-direct-acceptance) apply, driven by a
scripted Agent through the real transport, with expected values written from the ACP
input, not computed by the reducer under test. This model adds:

- **Live/replay equivalence (§4.3).** For each fixture of every timeline kind (messages
  with several IDs, mixed blocks, tools with sparse updates, plans, compactions,
  `_meta`): run it live (with and without an echo) and as a load replay; the entries are
  equal and equal to the fixture's expected entries.
- **Idle and late updates.** An update after the prompt's response, or between prompts,
  changes the existing entry by §3.2; no new structure appears.
- **Load atomicity.** A failed load leaves the installed entries unchanged; a successful
  one replaces them and clears `prompts`.
- **Installation and generations.** Concurrent create and fork are serialized; a stale
  action acknowledgement after close/reopen changes nothing.

## 11. Zed reference

| Concern | Zed `9f63850` | attyd |
| --- | --- | --- |
| Timeline | `AcpThread.entries: Vec<AgentThreadEntry>`, flat | same (§3) |
| Load vs live | both via `handle_session_update` into `entries` | same reducer (§3.3) |
| Turns | runtime `RunningTurn { id, first_entry_index }` only | derived (§5) |
| Message joining | `can_merge_message_chunks`: differ only if both IDs present and unequal | same (§3.2 rule 1) |
| Prompt echo | optimistic user message; matching echoed chunks skipped, ID adopted | same (§4.1) |
| Tool updates | `tool_call_mut` searches all entries | same (§3.2 rule 1) |
| Stop reason | an event (`Stopped`), not kept in history | kept per prompt (§4.2) |
| Elicitations | thread entries | resources, not entries: a load never replays them (§8.2) |
| Legacy plan | one session-level plan | per derived turn (§3.4) |
| Terminal release | handle removed, entity kept by tool content | handle/output split (§8.3) |
| Stale responses | `turn_id` check | session generation (§9) |

Zed's editor services, local thread storage, checkpoints and vendor `_meta`
interpretation are not adopted.

## 12. Open items

- §6.3 fork history is a proposal to review.
- The echo rule (§4.1) compares content blocks for equality; an Agent that echoes a
  normalized form of a block yields a second user entry live. Decide whether that needs
  a looser match after testing the supported Agents.
