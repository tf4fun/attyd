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

Entries are shared individually (copy-on-write). Views of the timeline are held while
the session keeps changing, so cloning a timeline copies pointers and an update copies
only the entry it changes. An update must therefore not allocate in proportion to
content it does not touch; this is gated by allocation tests
(`src/protocol/timeline/memory_tests.rs`).

## 3. Timeline entries

### 3.1 Entry kinds

```ts
type Entry = MessageEntry | ToolEntry | PlanEntry | CompactionEntry;

interface MessageEntry {
  kind: "message";
  role: "user" | "agent" | "thought";   // user_message_chunk, agent_message_chunk, agent_thought_chunk
  messageId?: string;                   // first present messageId of its chunks
  chunks: Chunk[];
}

interface Chunk {
  content: ContentBlock;
  _meta?: unknown;          // ContentChunk._meta
  notificationMeta?: unknown;   // SessionNotification._meta of the chunk's notification (§3.5)
}

interface ToolEntry {
  kind: "tool";
  call: ToolCallUpdate;     // reduced sparse state; every field but toolCallId may be absent
  notificationMeta?: unknown;   // §3.5
}

type PlanEntry =
  | { kind: "plan"; legacy: Plan; notificationMeta?: unknown }                       // `plan` (no ID), §3.4
  | { kind: "plan"; update: PlanUpdate | PlanRemoved; notificationMeta?: unknown };  // by planId

interface CompactionEntry {
  kind: "compaction";
  compactionId: string;
  status: CompactionStatus;
  summary: Chunk[];
  error?: string;
  _meta?: unknown;
  notificationMeta?: unknown;   // §3.5
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
   chunk is text and all other fields (annotations, block `_meta`, chunk `_meta`,
   `notificationMeta`) are equal; otherwise it is a new chunk. Equal content therefore gives equal entries
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
- An update for a session that is being installed is staged with that installation and
  reduced in arrival order when it succeeds (§9); for example
  `session/new` → `agent_message_chunk(S)` → response `S` keeps the chunk. An update for
  a session neither held nor being installed is dropped.

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

### 3.5 Notification envelope metadata

`SessionNotification._meta` qualifies one update, not the entity as a whole, and is
kept without a notification log:

- **chunks** (message and compaction summary): on the chunk the notification produced,
  as `notificationMeta`; rule 3 joins text only when it is equal.
- **tool, plan and compaction entries**: on the entry, by the patch rule: absent or
  `null` keeps, a value replaces. Only the latest envelope `_meta` that set a value is
  kept.
- **control updates**: with the control they set (§7).
- A permission's `toolCall` has no envelope; the request's `_meta` stays with the
  interaction (§8.2).

## 4. Live-only additions

### 4.1 The sent prompt

When the bridge sends `session/prompt`, it first reduces the prompt as user content:
one message entry, role `user`, no `messageId` (the pinned `PromptRequest` has none),
with an internal flag `sent` (not part of content). Its chunks are the prompt blocks
reduced by rule 3 (§3.2), like any other chunks: sending `[text("A"), text("B")]` stores
one chunk `"AB"`, as a replay of the same two chunks would. While `sent` lasts, the
bridge also keeps the original prompt blocks internally; echo matching uses them, not
the joined chunks.

Some Agents echo the prompt as `user_message_chunk`s; others do not. The **echo rule**
keeps one entry when the echo is exact (after Zed, `handle_session_update`,
`UserMessageChunk`, which also checks ID compatibility):

- While the last entry is the `sent` user message, an incoming user chunk is an echo
  when its content equals the next not-yet-matched prompt block **and** its
  `messageId` is compatible with the entry's (the entry has none yet, or they are
  equal). An echo adds no chunk. It completes the matched block with what the echo
  carries: the entry takes the echo's `messageId` if it has none, and the matched block
  takes the echo's chunk `_meta` and `notificationMeta`. The entry's chunks are then
  the completed blocks reduced again by rule 3, so metadata that now differs between
  blocks splits a joined chunk exactly as a replay would.
- Any other user chunk is not an echo. It ends `sent` and starts a new user entry; it
  never joins the `sent` entry, so a mismatched echo cannot corrupt the sent text.
- `sent` also ends when every block was matched or another entry follows.

Matching is exact and per block; there is no fuzzy or semantic matching. So:

- sending `[A, B]` and receiving `A (u1)`, `B (u2)` matches `A`; `B` has an
  incompatible ID and starts a second user entry `u2` with `B`;
- an echo split differently from the sent blocks (`"hello"` echoed as `"hel"`, `"lo"`)
  does not match and yields a second user entry `"hello"`.

Both are visible duplicates, not lost or merged content; §4.3 states the boundary.
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

Live and replay use one structure and one reducer for every Agent. Their **entries are
equal**, ignoring `prompts` and the internal `sent` flag, when the Agent's replay of
each prompt is one user message whose chunks, before rule 3, equal the sent blocks one
to one, and live it either:

- echoes the prompt exactly that way (§4.1), or
- does not echo, and its replay of the prompt carries no `messageId` and no chunk or
  envelope `_meta`.

Outside that boundary (a differently split or normalized echo, an ID or metadata only
the replay has) the two may differ: live may hold a duplicate user entry, or a user
entry without the replay's ID. That is accepted; a load replaces live entries anyway
(§6.2). All non-user content (agent and thought messages, tools, plans, compactions)
is equal under the same update sequence without condition. These are required tests
(§10).

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
| `complete` | `entries` start at the session's beginning | `session/new` (empty); a successful `session/load` ("the entire conversation history", `v1/agent.rs:4845–4848`), including a fork target's (§6.3) |
| `partial` | `entries` may lack what came before | `session/resume` of a session whose history was unknown ("without returning previous messages", `v1/agent.rs:1277`); a fork without load (§6.3) |

An empty `complete` history and an `unknown` one are different and are never confused
(D9). New content after a resume is appended to a `partial` history as usual.

### 6.2 Load

A load reduces the replay with the §3 reducer into a **candidate** session structure,
private to that attempt. On a successful response the candidate replaces `entries`,
clears `prompts` and sets `complete`, in one step. On failure the candidate is
discarded and the installed state is unchanged.

A load is needed whenever the session is not attached on the **current** Agent
connection: a cold open, and reattaching a held session after the connection was
replaced. To reattach, the bridge uses `session/load` when advertised (the candidate
replaces the retained entries on success; on failure they stay unchanged and the
session stays unattached), otherwise `session/resume`, which keeps the retained
entries and their `history`. This is the existing reattach order.
The bridge never loads a session already attached on the current connection, with one
exception: a fork target, whose response attaches it without history (§6.3). It
performs no load after a prompt: live content is already in memory by §4.

### 6.3 Fork

`session/fork` "creates a new session based on the context of an existing one"
(`v1/agent.rs:1105–1108`). It promises no transcript or ID inheritance, and tool,
plan and compaction IDs are unique only within their own session. Copying the source's
entries into the target would let a target tool `"1"` patch an inherited source tool
`"1"`, and a target replay of source messages would append them twice. So the target
never inherits source entries:

- **Agent advertises load:** after the fork response the bridge loads the target
  (§6.2); `history` is `unknown` until it ends. On success its entries are what the
  Agent replays, `history: complete`. On failure the target keeps what the Agent sent
  for it, as below, `history: partial`.
- **Otherwise:** the target holds only what the Agent sent for it, staged during the
  fork and reduced on installation (§9), then later updates; `history: partial`.

No resource of the source is copied: execution handles (terminals, responders) belong
to their session, and the target's entries reference only the target's own resources.
This changes the earlier convention that a fork without load copies the source log
([release parity](release-parity.md#session-operations-and-settings)).

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

Each control keeps, next to the value it qualifies (D12), the source metadata of the
latest notification or response that set it: the update's own `_meta` (for example
`CurrentModeUpdate._meta`, the response `modes._meta`) and the envelope's
`SessionNotification._meta` (§3.5), separately. No control-event history is kept. Clients prefer
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

A session is **observed** while a client follows it, or a client's read of it is being served and
for the delay after (protocol revision 4 [§7.2](protocol-r4-design.md#72-holding): a `GET` of a
session or view path observes the session like a short subscription; revision 3's reads observe
nothing). A session is needed while it is observed, occupied by an operation, has an outstanding
control or close request, or has a resource that can still act: a pending interaction,
a terminal with a handle, a `waiting` URL flow. Frozen output and finished flows do not
keep it needed.

## 9. Concurrency

- **One installation at a time.** `create` and `fork` both use the global operation
  slot, so at most one session is being installed at any moment. A resource naming a
  session ID the bridge does not hold yet, arriving while an installation runs, belongs
  to that installation: an interaction or URL flow is shown there and moves into the
  session when installed, or is cancelled if installation fails; a timeline or control
  notification is staged and reduced into the new session in arrival order on success,
  or discarded on failure (§3.2, §7). With no installation running it is refused or
  dropped. This
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
- **Echo boundary (§4.1).** Exact echo merges and adopts ID and chunk metadata;
  `[A, B]` echoed as `A (u1)`, `B (u2)` gives two user entries; a split echo gives a
  duplicate entry, never concatenated text.
- **Envelope metadata (§3.5).** Chunk, entity and control envelope `_meta` are retained
  at the stated position, live and replayed.
- **Pre-installation content.** `session/new` → chunk → response keeps the chunk; a
  failed creation discards it.
- **Reattach.** After the Agent connection is replaced, a load-only Agent gets
  `session/load` for a held session with known history; failure keeps the retained
  entries.
- **Fork.** With load: target entries come from its replay. Without load: only target
  notifications, `partial`; a target tool reusing a source tool ID does not touch any
  source content.
- **Sent text joining.** `[text("A"), text("B")]` sent with an exact per-block echo,
  without an echo, and replayed all give one chunk `"AB"`. Metadata that differs between
  the blocks splits them into two chunks only on the paths that receive it: when it is
  on the original prompt blocks, all three paths give two chunks; when only the echo
  and the replay carry it, those two give two chunks and the path without an echo
  keeps one `"AB"`.
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
| Connection lost, no thread | the dead connection stays in the store and a new thread fails with `incoming_transport_closed`; expected: the next new thread starts the Agent again, or an "agent exited — restart" affordance ([#62828](https://github.com/zed-industries/zed/issues/62828)); idle connections reaped after 30–40 s ([#62922](https://github.com/zed-industries/zed/pull/62922)) | `restart` action on `global`; a followed session or a login still starts the next connection (audit Q8) |
| Closing a session | `close_session`; `should_close_sessions_on_release` defaults to `supports_close_session` ([connection.rs](https://raw.githubusercontent.com/zed-industries/zed/main/crates/acp_thread/src/connection.rs)) | an unneeded session is closed at the Agent when released (§8.4); `close` is an action for API clients and R2; no extra button (audit Q9) |
| Accepted URL entries | kept after success so a late completion arrives; cancelling an accepted flow emits no second response ([#64609](https://github.com/zed-industries/zed/pull/64609)); a disconnect wakes the waiters and retries nothing ([#65646](https://github.com/zed-industries/zed/pull/65646)) | a flow `ended` by the connection's end stays in the final documents; the next connection starts without it (audit Q10) |

Zed's editor services, local thread storage, checkpoints and vendor `_meta`
interpretation are not adopted. The last three rows were compared on 2026-10-23 from Zed's
trait documentation and from its issues and pull requests; #62828 and #62922 were still open.

## 12. Open items

- Whether a looser echo match is needed after testing the supported Agents (§4.1).
