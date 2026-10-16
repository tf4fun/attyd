# Client API redesign

Status: **revision 2, second draft for review, 2026-10-16 (#51)**; revision 1 was settled
in review on 2026-10-14 (sixth version) and is what `main` implements. §3 is the
complete rule set; the later sections specify and derive from it. Revision 2 keeps
revision 1's replication (alignment, admission, holding) and changes what the
documents carry: revision 1 kept only message text and removed a turn's process when
it ended, which drops content ACP gave (the
[ACP fidelity audit](acp-fidelity-audit.md)). **The runtime code is not migrated
yet**; [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) is the
plan. Revision 2 must also lose no capability of the last release, v0.2.8; the
[release parity matrix](release-parity.md) is that check. It replaces the earlier v2 draft of the client protocol and its replication
rules, and is specified normatively in [protocol.md](protocol.md). The review that led
to revision 1 is recorded in [protocol-v2-review.md](protocol-v2-review.md) and in the
comments of PR #35.

## 1. Scope

attyd is a mediator without a persistence layer. Authority runs one way:

```text
FE (browser and other clients)  <  bridge memory  <  ACP server
```

There are two layers of interaction:

1. **bridge ↔ ACP server.** The bridge is an ordinary ACP client. It connects,
   loads, prompts, cancels and closes as any ACP client does, and it handles ACP
   failures itself. This layer is not specified here; ACP specifies it.
2. **FE ↔ bridge.** Clients replicate the bridge's in-memory representation and ask
   the bridge to change it. This document specifies this layer only.

The goal of layer 2 is a small set of rules, in the manner of Raft, from which
every behavior (normal operation, concurrency, reconnection, reopen) follows. A
client that implements these rules can present the bridge's state in any form: the
web UI is one presentation layer among possible others.

## 2. Roles

| Raft | attyd layer 2 |
| --- | --- |
| leader | the bridge's memory; there is exactly one, and no election |
| follower | a client: a read-only replica that never votes and persists nothing |
| log, entries identified by `(index, term)` | `log`: the settled turns of the session's timeline, entries identified by `(index, hash)` of a hash chain |
| uncommitted entries, which may be overwritten | `live`: everything else, replaced as a whole on alignment |
| AppendEntries consistency check | every log change names the position it follows; the client checks it |
| a follower truncates a conflicting suffix | a client truncates the suffix that differs and installs the bridge's |
| clients send requests to the leader | clients send writes to the bridge, which handles them serially |
| a new leader may hold a different log | a rebuilt bridge (reload, restart) may hold a different log; clients converge the same way |

Unlike Raft there is no majority, no term and no persistence. The bridge's memory
is the authority for clients; the ACP server is the authority behind it.

## 3. Rules

The rules come in two groups: S1–S8 bind the bridge and the shape of the documents,
and C1–C3 bind clients. Every behavior in §4–§8 follows from them; §3.3 lists the main
derivations. Revision 2 rewrites S2, S4 and S7, adds S8, and narrows S5–S6 (§3.4).

### 3.1 Bridge rules

**S1 Document.** Every scope is one document with two parts: `log`, a sequence of
entries, and `live`, one JSON value. Each log entry has a chain hash
`h_i = H(h_{i-1} ‖ canon(entry_i))`, with `h_0` fixed, computed over the entry exactly
as it is published. A session scope's log is its settled timeline; every other scope's
log is always empty. A session's log is **unknown** until the bridge holds a history
for it, which is not the same as empty. Once known, the history is **complete** when
it starts at the session's beginning (created here, or replayed by `session/load`), and
**partial** when the Agent did not provide what came before (a `session/resume` of a
session whose log was unknown).

**S2 Settling.** A session's timeline is a sequence of turns. Its settled part is the
log; the turn still being received is `live.tail`. The log changes only at a settle
point: a prompt starts, a prompt ends (with any outcome), the Agent connection ends, a
load completes, or an update arrives for an entity already in the log. A settle point
is one event: the log change with the `live` change that goes with it. Every other
change happens in `live` only.

**S3 Alignment.** A client subscribes with the head of the log it holds, `(n, h_n)`;
this head is the subscription's **baseline**. The bridge takes one cut of the scope and
sends it as one event: the whole of `live` and, once the scope's log is known, its log
from the baseline if it has hash `h_n` at index `n`, otherwise from `0`. While the log
is unknown, the client keeps its own log, unconfirmed. Taking the cut and continuing
with the changes after it is one serial step of the bridge. The baseline advances only
with the log changes handed to the subscriber's output stream; every log change is
computed from it and names the position `(index, hash)` it follows. A client that
cannot apply a log change closes the stream and subscribes again with the head it
holds.

**S4 Writes.** A client write is an **operation** or an **action**. An operation
starts bridge work that occupies scopes (`prompt`, `fork`, `create`, `delete`,
`authenticate`, `logout`, `terminal_login`, `list`, and the bridge's `load`). Every
scope's `live` holds exactly one operation, `op = { id, kind, status, … }`. The bridge
generates `id` at random and never reuses it. An operation's ID identifies it and is
what writes are admitted against; it says nothing about what a replica has seen. An
action acts on something that exists now and occupies nothing: a session's controls
(`set_mode`, `set_config`), the session itself (`close`), or a resource (`answer`,
`input`, `resize`, `dismiss`). Actions take no `expect`, fall outside S5, and each
states its precondition and its repeat semantics.

**S5 Admission.** Every operation write names `expect`, an operation ID, and the
bridge handles writes and loads one at a time:

- a write that **starts an operation** is admitted when `expect` equals `op.id`, `op`
  has ended, and every other scope the operation acts on is free (below); the new
  operation replaces `op` in the same step;
- a **cancel** is admitted when `expect` equals `op.id`, `op` is running and its kind
  accepts a cancel.

An admitted operation **occupies** every scope it acts on until it actually ends, a
requested cancel included: it is the `op` of each such scope the bridge holds, and it
holds back the load of each such scope the bridge does not hold. A scope is free when
its `op` has ended and no operation occupies it. Anything else is refused.

**S6 Holding.** The bridge holds a session while it is needed: a client subscribes to
it, an operation occupies it, an action on it is still waiting for the Agent's
answer, or one of its resources can still act (a pending interaction, an unreleased
terminal, a waiting URL flow). It may release a session
that is not needed only if the Agent can load or resume it; a session it cannot bring
back stays held until it is closed, deleted, or the Agent connection ends. Subscribing to a
session the bridge does not hold makes the bridge hold it, which starts a `load`
operation. A one-off read holds nothing.

**S7 Resources.** Interactions, terminals and URL flows live by their ACP identity:
a resource belongs to the scope its ACP message names (a session, or one JSON-RPC
request) and ends at the ACP event that ends it, never because some operation ended.
Where a client sees it never changes its life:

- an **interaction** is an Agent request waiting for a client. A session-scoped one
  ends when it is answered, when the Agent cancels it, when a cancel or close of its
  session answers it (permissions, as ACP requires), or with its session; a
  request-scoped one ends when it is answered, cancelled, or when the JSON-RPC request
  it names is answered. While its session is being created, it is shown in the
  creating operation's scope and moves into the session when the session is
  installed;
- a **URL flow** starts when a client accepts a URL elicitation and keeps that
  elicitation's scope. It stops waiting once, when the Agent completes it or the
  Agent connection ends (its request's response does not end it), and keeps that
  first outcome until a client dismisses it or the document that shows it ends;
- a **terminal** has a **handle** (the process and its ID's use in `terminal/*`) and
  **output**. The handle ends with `terminal/release` or the Agent connection; the
  output is content, frozen then, and stays while content embeds it, as ACP requires.
  A terminal login is removed when it has exited and a client dismisses it.

Resource actions address a resource by its ID. Each states its repeat semantics: an
answer is delivered at most once, dismiss has no further effect, resize to the same
size leaves the same state, and input is not idempotent: it is written in order, every
time.

**S8 Content.** A session's documents carry what ACP gave for the session. Every
timeline update belongs to an ACP **entity**: a message (its `messageId`, or, without
one, the run of chunks it continues), or a tool call, ID-addressed plan or compaction
(its ID, unique in the session). An update is reduced into its entity by that
entity's ACP semantics. Nothing else removes or rewrites content: not a turn's end,
not an error, not a release, not a client's display. Every field is kept at the level
it arrived on, `_meta` and annotations included; the bridge interprets none of them.
Control updates are reduced into `controls`; every other update becomes timeline
content.

### 3.2 Client rules

**C1 Projection.** A client's presentation is a pure function of `log`, `live` and its
own UI state (drafts, queued input, disclosure, scroll position, and any earlier log
it chooses to keep as unconfirmed). UI state never enters the protocol, and the client
holds no authoritative state. Folding and collapsing content are UI state; they never
need the bridge to remove anything.

**C2 Refusal.** A refused write leaves its input in the client, for the user to submit
again once the replica is in sync. Queued input that was never sent, or that was
refused with a definite answer, may be submitted automatically once the replica shows
the operation ended, naming that operation.

**C3 Uncertainty.** A write, action or terminal input that was sent without a definite
answer may have taken effect; a later `stale` proves nothing. The client never resends it
automatically, with a new `expect` or otherwise. It keeps the input for the user to
judge. An interaction answer is the exception: it is delivered at most once (S7), so
resending it is safe.

### 3.3 Derivations

| Behavior | From |
| --- | --- |
| Open, reconnect, a slow subscriber, a replaced stream, a bridge restart and a reopen are all one alignment | S3 |
| After a reopen, a matching history is kept; a diverged one is truncated and the bridge's installed | S1, S3 |
| A completed, failed or cancelled turn keeps its tool calls, thoughts, plans and compactions, live and after a load | S2, S8 |
| A message split by a tool keeps its identity; a turn's mixed user message stays one turn | S8 |
| A plan updated or removed in a later turn is one entity; no older copy stays current | S8 |
| The mode or a configuration option changes while a prompt runs; a running session can be closed | S4 |
| An interaction ends with the JSON-RPC request it names, not with an operation that outlives or precedes it | S7 |
| A tool's terminal output survives release and the Agent connection's end while the log holds the tool | S7 |
| A tool call first seen through a sparse update or a permission keeps only what the Agent sent; later updates fill it in | S8 |
| A control change or close still waiting for the Agent is never cut by a release | S6 |
| A late tool or compaction update, or content the Agent sends between prompts, reaches every replica: it changes the entry it belongs to, or the tail | S2, S8 |
| Two logs that align hold the same items, results and metadata, not only the same text | S1, S8 |
| A resume never presents an unknown history as empty | S1 |
| No event shows a settled entry beside the same content in the tail | S2, S3 |
| Once a write that starts an operation is admitted, a competing write or a resend that still names the old `expect` gets `stale`, a fork resend included | S4, S5 |
| Delete never races a prompt on the same session, nor a load triggered by subscription | S5 |
| A result is the final `op`; no request records, intent tables or result ledgers | S4 |
| Two clients can answer two interactions of one scope; one interaction is answered once | S7 |
| An interaction is not cancelled because an unrelated operation ended | S7 |
| A session released before its first subscription is loaded again; one that cannot be brought back is never released | S6 |
| An accepted URL flow survives its operation and a client reload, and stops holding its session once removed | S6, S7 |
| A released terminal embedded in a tool call still shows its output, and no longer holds its session | S6, S7 |
| A lost response never turns into a duplicate prompt, fork or terminal input through an automatic resend; a submission the user makes again is new input | S5, C3 |

### 3.4 Changes from revision 1

| Revision 1 | Revision 2 | Why |
| --- | --- | --- |
| S2 Commit: only a completed turn enters the log; its process is removed | S2 Settling: every sent prompt settles; content outside a prompt settles too; nothing is removed | D1, D4, D5 |
| entries are the final messages (`prompt`, `reply`) | entries are reduced timeline items with the turn's `end` | D1–D3 |
| `canon` drops `_meta`; the hash covers text | entries carry `_meta`; the hash covers the published entry | D6, D7 |
| S4: all work that may wait for a client is an operation; Agent requests join or start one (`agent_request`); `set_mode`, `set_config` are operations | S4 Writes: operations occupy, actions (controls, `close`, resource actions) do not; Agent requests are interactions (S7) | D11, R1, R2 |
| S5: an answer covers exactly the pending interactions | an answer is a resource action on one interaction | D10 |
| S6/S7: a terminal is removed on release | handle and output separated: its frozen output stays while embedded and holds nothing | D8 |
| — | `global.live.diagnostics`: Agent stderr and MCP-over-ACP activity, bounded | R3 |
| S1: a known log | complete or partial | D9 |
| — | S8 Content | D1–D7 |

The finding IDs are those of the [audit](acp-fidelity-audit.md#2-defects) (D) and of
the [release parity matrix](release-parity.md#regressions) (R).

## 4. Documents

### 4.1 Log entries

A log entry is one settled turn of the session's timeline:

```ts
interface Turn {
  items: Item[];            // the turn's timeline, in order
  end?: { response: PromptResponse } | { error: ProtocolError };  // observed live only
}

type Item =                 // the bridge's own types around ACP values (protocol §3.1)
  | { type: "message"; role: "user" | "agent" | "thought"; messageId?: string;
      chunks: Array<{ content: Block; _meta?: unknown }> }
  | { type: "tool"; call: ToolCallUpdate }   // sparse: absent until provided
  | { type: "plan"; update: Plan | PlanUpdate | PlanRemoved }
  | { type: "compaction"; compactionId: string; status: CompactionStatus;
      summary: Array<{ content: Block; _meta?: unknown }>; error?: string; _meta?: unknown };
```

**Reduction** follows three rules (protocol §3.2):

1. **Identity.** A tool call, an ID-addressed plan and a compaction each have one item
   in the session, at the position of their first update; later updates reduce into
   it wherever it is, so a plan updated or removed in a later turn changes its one
   item, and a removed plan is an item in its removed state, never a still-current
   older copy. A message is identified by role and `messageId`; its contiguous chunks
   form one item, and a chunk after an intervening item starts a new item with the
   same `messageId`, so `message(m1,"a") → tool → message(m1,"b")` gives two items of
   the one message m1.
2. **Patch.** Sparse updates apply by the pinned type: for a tool call the field rule
   of `ToolCall::update`, where absent and `null` both keep and collections are
   replaced whole (`content: []` clears). The state is a `ToolCallUpdate`, not a
   `ToolCall`, because ACP's `ToolCall` requires a `title` that a first sparse update
   or a permission's `toolCall` (`{ toolCallId }`) need not carry; an unprovided field
   stays absent, never invented, and no update is dropped for lacking one. For a
   compaction the schema's three states (absent
   keeps, `null` clears, a value replaces). Nothing is deep-merged.
3. **Fragments.** Every chunk keeps its own fields, including a chunk's `_meta` apart
   from its block's. Text joins the previous chunk only when every other field of the
   two is equal, so chunking never changes the result and metadata never merges away.

The legacy `plan` has no ID. Each turn keeps a snapshot of its last state, as v0.2.8
did; the current plan is derived from the latest snapshot and is not an entity that
updates reduce into.

**Content.** Image, audio and embedded resource blocks are replaced by an attachment
reference `{ type: "attachment", id, … }`, where `id` is the SHA-256 of the canonical
block as received. The bridge keeps the block; a read or a prompt that references it
gets it back unchanged. `_meta` and annotations stay everywhere. The entry is
serialized as canonical JSON (RFC 8785), and the hash covers all of it, `end`
included.

**Turns.** A turn is the part of the timeline one prompt produced. Live, it is what
arrives from the prompt's admission to its end, starting with the user message the
bridge sent (one item, one chunk per prompt block); user chunks the Agent sends are
items and start no turn. A replay has no prompt boundaries, so a turn starts at each
user message item, except one that continues the opening user message of the current
turn (same, present `messageId`): a user message of text and an image is one turn.

**Settling.** The tail holds the turn being received. A prompt's end appends the tail
as a new entry with its `end`, whatever the stop reason or error: output the Agent
sent is not withdrawn by a later failure. Content that arrives outside a prompt
continues the last turn in the tail and settles into the last entry when the next
prompt starts or the connection ends. An update for an entity already in the log
rewrites that entry at once; the log change starts at its index, the later turns stay
as they were, and their hashes follow the rewritten one.

The same updates give the same reduced items, however they were chunked, live or
replayed; an entry also holds its `end`, which only a live turn has. Updates that
differ in IDs, metadata, results or order give different items; the
bridge does not make them equal. A live turn and its replay usually differ (the replay
has no `end`; an Agent may replay less, or differently), and alignment then replaces
the client's log. That costs a redrawn history, never a wrong write, and is the price
of letting ACP's replay decide. Identity serves synchronization only. Admission never
compares hashes (S5).

### 4.2 Live state

```ts
interface ScopeLive {            // what every scope's live has
  op: Operation;
  interactions: Record<string, Interaction>;  // pending Agent requests, by interaction
  urlFlows: Record<string, UrlFlow>;          // accepted URL elicitations, by interaction
}

interface SessionLive extends ScopeLive {
  cwd: string;
  history: "complete" | "partial";            // absent while the log is unknown
  controls: Controls;            // modes, configOptions, commands, info, usage (reduced)
  tail: { continues: boolean; items: Item[] };   // the turn being received
  terminals: Record<string, Terminal>;        // with a handle, or frozen and still embedded
  closing: boolean;                           // a close action is in flight
}

interface GlobalLive extends ScopeLive {
  agent: AgentState;             // connection phase, launch, initialize, authentication
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  terminals: Record<string, LoginTerminal>;  // terminal logins, each with its methodId
  diagnostics: Diagnostics;      // Agent stderr, MCP-over-ACP connections and activity, bounded
}

interface CatalogLive extends ScopeLive {
  revision: string;              // changes when the Agent's session list may have changed
}
```

- `live` contains only what the bridge holds now. Nothing is retained for clients
  that come later.
- **Every scope has `op`, `interactions` and `urlFlows`.** Any scope can receive Agent
  requests, so any scope can hold interactions and URL flows. A new scope gets them
  without special handling.
- The tail is `live` and is not hashed; its own path to every replica is the `live`
  patch stream, like every other `live` field.

### 4.3 Resources

This section specifies S7.

- **Interactions.** An Agent request belongs to its ACP scope. A permission request
  and a session-scoped elicitation belong to their session and are shown in it; while
  that session is being created or forked they are shown in the creating operation's
  scope and move into the session when it is installed, or end (answered with their
  cancel outcome) when it is not. A request-scoped elicitation belongs to the JSON-RPC
  request it names and has that request as its `parent`; it is shown in the scope of
  the write or load that sent the request, or in `global` for a request no write sent
  (`initialize`, a release's close, a forwarded MCP request), and it ends when that
  request is answered. A delete's `session/close` and `session/delete` are two
  requests, so an elicitation of the first ends before the delete does. An elicitation
  naming no outstanding request (`null`, or an ID not outstanding) is refused at once
  and never shown. The bridge knows its outstanding requests from send to response
  only; no request record is kept. Otherwise an interaction ends when it is answered,
  when the Agent cancels it, when a cancel of its session's prompt or a close of its
  session answers it (permissions only, `cancelled`, as ACP requires), or with its scope or the Agent
  connection. No operation's end touches it.
- **A URL flow** starts when a client accepts a URL elicitation: the interaction leaves
  `interactions` and the flow enters `urlFlows` as `waiting`. Accepting only consents;
  the external flow starts then. It stops waiting when the Agent sends its completion
  notification (`completed`), or when the Agent connection ends (`ended`); only
  `waiting` changes, so a later completion or connection end never overwrites an
  outcome. The response of a request-scoped flow's parent request, success or error,
  ends only interactions not yet answered; an accepted flow keeps waiting, and the
  answered request is not remembered (protocol §4.6, release parity P5). A finished
  flow stays, with its outcome, so that every client can tell success from giving up, also when the Agent completes
  it much later. It is removed when:
  - a client dismisses it (§6.1), waiting or finished; ACP makes the completion
    notification optional and the user must be able to give up waiting;
  - the document that shows it ends (a session scope's end), or its session is not
    installed.

  It keeps its elicitation's scope and moves with installation. Only a waiting flow
  keeps its session needed. No record of dismissed flows is kept.
- **A terminal** has a handle and output. The handle ends when the Agent releases the
  terminal or the Agent connection ends (the bridge kills the process then). ACP still
  has tool calls that contain it display its output, so the bounded output and exit
  status stay, frozen and marked `released`, while an item of the log or the tail
  embeds it: through a reconnect and a resume that keeps the log. They go only with
  the session's documents. A terminal login is removed when it exits and a client
  dismisses it.
- **Holding.** A resource that can still act (a pending interaction, an unreleased
  terminal with a handle, a waiting URL flow) keeps its session needed (S6). Frozen
  output does not. When the last such resource of an unsubscribed, unoccupied session
  is removed, the session may be released.

### 4.4 Operations

```ts
interface Operation {
  id: string;
  kind: string;                  // see §6.1
  status: "running" | "completed" | "failed" | "cancelled";
  input: unknown;                // the write's arguments: the prompt, the mode, …
  cancelRequested: boolean;
  result?: unknown;              // completed: the stop reason, a new session ID, …
  error?: ProtocolError;         // failed
}
```

- An operation's result is its final `op`, visible until the next operation replaces
  it. A failed turn keeps its prompt and error there for a retry, and its content and
  error in the log; a refused mode change keeps the ACP error.
- Waiting for the user is a non-empty `interactions` of a scope, not a state of an
  operation. A request-scoped elicitation names its JSON-RPC request as `parent`; the
  operation that sent the request only decides where it is shown.
- A session the bridge has just held starts with a `load` operation. A session
  created by `create` or `fork` starts with a completed `open`.

## 5. Replication

### 5.1 Stream

```text
GET /api/v2/stream?scope=global&scope=session/S&have=session/S:12:<hash>
```

`have` gives a scope's log head; without it the head is `(0, h_0)`. The stream
carries three kinds of event, in order per stream:

```ts
type Event =
  | { scope: string; log: { from: number; prev: string; entries: Array<{ turn: Turn; hash: string }> };
      live?: LiveChange }
  | { scope: string; live: LiveChange }
  | { scope: string; end: { reason: "deleted" | "closed" | "load_failed"; error?: ProtocolError } };

type LiveChange = { replace: unknown } | { patch: JsonPatchOperation[] };
```

A client applies a `log` event by checking that its hash at index `from` is
`prev`, truncating its log to `from` entries, appending `entries`, and applying the
event's `live` change. One event kind serves:

- **alignment (S3):** `from` is the matched index and `live` is `{ replace }`; while
  the session's log is unknown, the alignment carries `live` only;
- **settle point (S2):** `from` is the index of the first entry that changed (the
  previous length for an appended turn), and `live` is the patch that goes with it,
  for a prompt's end the patch that ends the operation and empties the tail;
- **a first load's end:** the log, from the baseline, with the patch that ends the
  load.

After every event the client's `live` is a `live` the bridge held at some moment,
and its log is the bridge's log at that moment, or, while that log is unknown, the
client's own. No event shows a settled entry beside the same content in the tail.

### 5.2 Synchronization

Every synchronization is one alignment event (S3). In particular:

- **Open and reconnect** are the same: subscribe with the log head the client
  holds, if any. A client always subscribes with the head it holds at that
  moment, never by retrying an earlier URL whose `have` may be out of date.
- **A cold reopen** keeps the client's log while the session loads: the client
  shows its own log as unconfirmed, and the load's interactions are shown and
  answered as usual. When the load ends, a matching history is confirmed with its
  suffix; a different one replaces the client's.
- **A cold reopen by resume** gives a `partial` history: the bridge holds only what
  arrives after the resume. The client's earlier entries are no longer confirmed;
  it may keep them as its own UI state, shown as not confirmed, and never presents
  them as the bridge's.
- **A broken stream** is not resumed. The client subscribes again with its head.
  It keeps every entry the bridge still has and replaces `live`.
- **A diverged log** (after a reopen whose replay differs, or a bridge restart)
  fails the head match. The bridge sends its whole log, and the client replaces
  its own. If the bridge's log is shorter, the same happens.
- **A slow subscriber** is treated as if it had subscribed again: the bridge drops
  its queued events for the scope. For a scope that still exists, it queues one
  alignment from the subscription's baseline: the last log position handed to the
  output stream, never one that was only queued. For a scope that ended, it keeps
  the `end` event and sends nothing after it.
- **Changing the set of scopes** opens a new stream. A client ignores events from
  a stream it replaced.

UI state survives every synchronization: a draft or a queued prompt does not
depend on the log it was written against.

### 5.3 End

A scope ends when its session was deleted or closed, or when its load failed. The event
gives the reason and, for `load_failed`, the error. The client drops the document.
Subscribing again after `load_failed` tries the load again; the client does this
only on the user's request.

## 6. Writes

### 6.1 Writes by scope

| Scope | Starts an operation | Acts on the running operation | Actions |
| --- | --- | --- | --- |
| `session/<id>` | `prompt { prompt }`, `fork { cwd }` | `cancel` (prompt) | `set_mode { modeId }`, `set_config { configId, value }`, `close`; resource actions |
| `global` | `create { cwd }`, `delete { sessionId }`, `authenticate { methodId }`, `logout`, `terminal_login { methodId }` | `cancel` (terminal login) | resource actions |
| `catalog` | `list { cursor }` | — | resource actions |

- **No open command.** Subscribing holds the session (S6).
- **Close** is an action of the session, as in v0.2.8, also while a prompt runs: the
  Agent cancels the session's work as if `session/cancel` had been sent and frees it.
  The bridge marks the session `closing`, answers pending permissions `cancelled`, and
  on success ends the scope with `closed`; the session stays in the catalog and a later
  subscription loads or resumes it. A session that is not needed is still released
  (S6).
- **Delete** is a global operation, so it works for sessions the bridge does not
  hold. It occupies `global` and `session/<id>` (S5):
  - while the session is held, the delete is its `op`, so a prompt naming the
    previous operation gets `stale`;
  - while the session is not held, a subscription to it waits; it receives
    `end: deleted` if the delete succeeds, and its `load` starts if the delete
    fails.

  The bridge closes a held session, deletes it on the Agent and ends the scope with
  `deleted`. Its result or error is on `global.op`.
- **Controls** are actions: they occupy nothing and run while a prompt runs, as ACP
  allows for the mode and v0.2.8 did for both. Each response is held until the Agent
  answers and carries its error, if any; the new state reaches every replica through
  `controls`. A queued control change is therefore never needed.
- **The catalog** is its own scope, so that listing does not contend with global
  writes. `list` runs the Agent's `session/list`. An elicitation the Agent sends
  during it and scoped to that request is an interaction of `catalog` with the
  `session/list` request as its parent, answered like any other.
- **Actions are not operations.** Controls and close address their session; answering
  an interaction, terminal input, resize, and dismissing a URL flow or an exited
  terminal login address a resource by its ID. None takes `expect`:

  ```text
  POST /api/v2/scopes/{scope}/actions/{action}
  POST /api/v2/scopes/{scope}/resources/{id}/{action}
  ```

  They occupy nothing and fall outside S5. Resource actions never wait for a client;
  a control or close waits for the Agent's answer, and the Agent may ask a client
  something meanwhile (an elicitation), which is an interaction like any other. Until
  that answer the action keeps its session held (S6), through the outstanding-request
  entry the bridge already keeps; the answer settles only that action's session. The resource ID identifies the
  resource, not one action on it, so each action states its own repeat semantics:

  | Action | Repeated |
  | --- | --- |
  | set_mode, set_config | sets a value: the same value again leaves the same state |
  | close | while one is in flight, `not_allowed`; after success the scope is gone (`404`) |
  | answer | **at most once**: checked against its request, delivered and removed in one step; a repeat, or an answer after the interaction ended, gives `404` and leaves the same state |
  | dismiss | removes the flow, waiting or finished, or the exited login; a second dismissal gives `404` and leaves the same state |
  | resize | sets a size: repeating the same size leaves the same state |
  | input | **not idempotent**: the bytes are written to the terminal's stdin each time, in the order the bridge receives them |

  A missing resource gives `404`.

### 6.2 Request and response

```text
POST /api/v2/scopes/{scope}/ops   { "expect": "<op.id>", "kind": "...", "args": { … } }
```

- **Admitted:** `{ op: "<id>" }`. This is the new operation's ID, or the running
  operation's ID for cancel. A success means the write was admitted;
  nothing more.
- **Operations whose result the initiator consumes** (`create`, `fork`, `list`)
  answer when they end, with `{ op, result }` or `{ op, error }`. The result is also
  on the scope's `op` until the next operation replaces it. If that response is
  lost, the initiator loses the result: a created session is still listed in
  `global.sessions` while held and in the catalog, and a page can be listed again.
- **Actions** answer `204`, or, for controls and close, `200 {}` or `200 { error }`
  once the Agent answers. An Agent error is a definite answer.
- **Refused:** `{ error: { code, message } }`. The codes are:

  | Code | Meaning |
  | --- | --- |
  | `stale` | `expect` is not the current `op.id` |
  | `not_allowed` | the operation is running and the write starts one, or the operation has ended and the write acts on it, or another scope the operation acts on is not free, or the session is `closing`, or an action's precondition does not hold |
  | `invalid_request` | the request is malformed |
  | `unsupported` | the Agent did not advertise the capability |
  | `unavailable` | the Agent is not connected |

- **What the replica shows.** The replica shows the bridge's current state, which
  may lag behind or already be past a write. The protocol makes no promise that a
  replica observes the effect of a particular write. A delivered answer may still
  show its interaction until the stream catches up; a completed operation may
  already have been replaced by the next one.

### 6.3 Consequences of S5

- **Concurrency.** Two clients that start an operation naming the same `expect`
  race; the bridge admits one, and the other gets `stale`.
- **Lost responses.** For a write that starts an operation: if the first send was
  admitted, a resend still names the operation it replaced and gets `stale`; if it
  was not, the resend may be the first one admitted. For writes that act on the
  running operation, `op.id` does not change: a resent cancel succeeds again
  without further effect. A resent answer gets `404` once the first was delivered
  (S7).
- **No ABA.** A fork returns the source to the same history and to an ended
  operation, but with a new `op.id`.
- **Independent answers.** Each interaction is answered on its own. An interaction
  that arrives or ends while the user composes another answer does not affect it;
  ACP requests do not change after they arrive, so the ID is enough.
- **Operations across scopes.** An operation that acts on several scopes occupies
  all of them, so no conflicting write and no load can run on any of them meanwhile.
- **No intent tables.** No idempotency key, intent table, tombstone or retention of
  results is needed.

### 6.4 Client obligations

This section applies C2 and C3 to writes.

- **Refused input stays in the client.** The prompt, answer or control change
  remain, for the user to submit again once the replica is in sync.
- **An uncertain write is never resent automatically.** If a write was sent and no
  definite answer came back (the connection broke, the request timed out), a later
  `stale` does not prove it was not admitted. The client must not resubmit that
  input with a new `expect` by itself. It keeps the input as a draft for the user
  to judge.
- **Terminal input follows the same rule.** Input whose delivery is uncertain is
  never resent automatically, since the bridge may already have written it. A
  client sends terminal input in order and one request at a time per terminal.
  Input the user types again is new input and is delivered normally. No sequence
  numbers or deduplication are needed.
- **Queued input may be submitted automatically.** A queued prompt that was never
  sent, or that was refused with a definite answer, is
  submitted when the replica shows the operation ended, naming that operation.

## 7. Holding

- **Needed sessions are held.** A session is needed while a client subscribes to
  it, an operation occupies it, a `set_mode`, `set_config` or `close` on it waits for
  the Agent's answer, or one of its resources can still act: a pending interaction,
  an unreleased terminal, a waiting URL flow (S6). An action needs no slot or record
  for this: its outstanding JSON-RPC request is the need, from send to response.
  A release never starts while a client close is outstanding, and its own close sets
  `closing`, so the two never overlap.
- **Release only what can be loaded again.** The bridge releases a session that is
  not needed, as an ordinary ACP client would close it, only if the Agent
  advertises `session/load` or `session/resume`. How long it waits first affects
  performance only, apart from one consequence: a session the Agent can only resume
  comes back with a `partial` history, because resume replays nothing. Without
  either capability the session stays held until it is deleted or the Agent
  connection ends.
- **No hand-over race.** A session released between `create` or `fork` and the
  client's subscription can be loaded again, so that subscription loads it, and
  alignment (S3) continues its log. A session that cannot be loaded again was never
  released.
- **Reopen continues.** A release and a later hold, an ACP reconnect, and a bridge
  restart all end in a `load` operation and a new alignment. If the replay gives
  the same entries, the client keeps its log; otherwise the replay's log replaces
  it.
- **Agent unavailable.** While the Agent is disconnected, a subscribed session's
  operation is a running `load` waiting for the connection; `global.agent` shows
  why. A session the Agent cannot load again ends with `load_failed` once the
  connection that held it ends.
- **Only subscriptions hold.** A one-off read of a scope (§8) does not make the
  bridge hold anything. A client that wants a session held subscribes to it.

## 8. Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/scopes/{scope}` | the document of a held scope once, as one cut; `404` when not held; holds nothing |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | content-addressed; served while the session's `log` or `live` references the ID, else `404` |
| `GET /api/v2/sessions/{id}/files?query=`, `…/files/content?path=` | workspace context |

Reads that may wait for a client are operations instead: the catalog is listed by
the `list` operation (§6.1).

**Markdown export** is assembled by the client from the log, the tail, the session's
terminals and the attachment read. There is no export endpoint. It covers every
section v0.2.8 exported ([release parity](release-parity.md#markdown-export)).

## 9. Extending

- **A new presentation layer** (a CLI, an IDE extension, a mobile client)
  implements the generic replica engine (alignment, log truncation and append,
  `prev` checks, `live` replace and patch, subscribing again) and its own view
  function.
- **A new operation** is a new `kind` with its arguments, the scopes it occupies
  and its result. Admission, responses and replication do not change.
- **New state** is a new field of `live`. The engine does not change.

## 10. What changes

This section records the move from the earlier v2 draft to revision 1. Revision 2's
changes to it are in §3.4: the log now holds whole turns rather than final messages,
`agent_request` is gone, and interactions are resources.

### 10.1 Removed

- Session incarnations, owner-qualified scopes, `If-Match` owner tags,
  `end: replaced`, retired-incarnation tables, first-subscription leases.
- Epochs, `agent.predecessor`, log positions (`seq`, `Position`, `at`), the
  retained log, `Last-Event-ID` resume, per-scope `prev` positions, `reset` and
  snapshot fetch, the received, checkpoint and applied positions, read-your-writes.
- `Idempotency-Key`, the intent table, tombstones, `resultExpired`, intent errors.
- `global.requests` and `agent_request` records, their retention and
  `DELETE /api/v2/requests/{id}`; turn outcomes and their retention; URL flow
  completion records.
- The compact history's process pages: `historyRevision`, `collapsedTurns`,
  `visibleRanges`, `GET …/process`. In revision 1 a completed turn's process was gone; revision 2 settles it into the log instead (S2, S8).
- Open and close commands, the per-command endpoints, and `GET /api/v2/catalog`.
  (Revision 2 restores close as an action, §6.1.)
- In the client: owner handling, intent recovery, request-record waits, the watched
  process cache (`ObservedProcesses`), process-page export, and the v1
  compensating state.

### 10.2 Kept

- JSON Pointer patches for `live`; the generic replica engine's patcher and
  validation.
- Control reduction (protocol §3.3), terminals, URL flows keyed by interaction,
  content-addressed attachments.

### 10.3 Added

- The hash-chained log and `canon`, with message reduction.
- Atomic alignment by log head, and one `log` event for alignment and commit.
- The operation slot, `expect`, occupation across scopes, and one write endpoint.
- The `catalog` scope. (`agent_request` operations were added in revision 1 and are
  removed by revision 2.)
- `urlFlows` on every scope; resource lifecycles independent of operations;
  resource actions, including dismissing a URL flow.
- Hold by subscription and need, release only of loadable sessions; delete as a
  global operation.

## 11. Acceptance

Each rule gets tests at the bridge first and then in the client engine. Document
consistency is checked after **every** event, not only once synchronization ends.
Revision 2 adds a second kind of check: a replica that equals the bridge's projection
proves nothing if the projection dropped content. Cases 1, 2, 13, 19–27 and 30–33
therefore also assert, **from the ACP input**, which items and fields the log or the
tail holds (audit §8). Cases 11, 28, 29, 32, 34 and 35 restore v0.2.8 capabilities and are
checked against that release's behavior; a test deleted, or changed to assert that a
capability is gone, never counts ([release parity](release-parity.md)). Cases marked
r2 are new or changed in revision 2; their runtime code is not migrated yet.

1. **Identity (S1, S8, §4.1)** r2. Three separate checks:
   - **Reduction is chunking-invariant.** The same ACP update sequence, streamed in
     different chunks (also a user message of text and an image), gives the same
     items. Chunks that differ in `_meta` or annotations stay separate chunks.
   - **Equal entries, equal hashes.** Two entries that are equal as published (items
     and `end`) have the same hash, whether reduced live and settled or rebuilt;
     entries that differ in a tool result, a `messageId`, a message boundary, a
     chunk's `_meta` or the presence of `end` have different hashes.
   - **The replay is authoritative.** A replay of a live-observed turn, which has no
     `end`, gives an entry that differs from the live one; alignment replaces the
     live entry with it and the client applies that normally. Live and replayed
     entries are not required to be equal.

   Normalized content with attachment references and prompts that reference
   attachments pass the generated types and validation; the bridge resolves
   references to the blocks as received before it calls the Agent, and an attachment
   read returns them unchanged, `_meta` included.
2. **Settling (S2)** r2. A prompt's end appends its entry with `end`, empties the tail
   and ends its operation in one event, for a completed, a cancelled, a failed and an
   interrupted prompt alike. The failed prompt keeps its prompt and error on `op` too.
3. **Alignment (S3).**
   - A client with a matching head receives only the suffix.
   - A client with a diverged or longer log receives the whole log and replaces its
     own.
   - After a reopen with the same replay, the client keeps its log.
   - Alignment at a settle point never yields a settled entry beside the same
     content in the tail, for a new subscription, a reconnect and a slow subscriber
     alike.
   - A cold reopen with the same history never clears the client's log first, and
     interactions during the load can be answered. A disconnect before the load,
     during it, and after it but before its log was delivered all converge on
     reconnect. A history that did change is replaced by hash.
4. **Synchronization (§5.2).** A broken stream, a slow subscriber and a replaced stream
   all end with the client equal to the bridge. Events of a replaced stream change
   nothing. With the client's consumption paused, a log change queued but not sent
   and then coalesced leads, once consumption resumes, to an alignment the client
   applies directly, without subscribing again. This holds for a log change that
   rewrites an earlier entry (case 21) as for one that appends.
5. **Live changes (S1, S4).** Controls and interactions change while the log hash
   stays the same, and the replica follows.
6. **Admission (S5).** For each operation kind:
   - once a write that starts an operation is admitted, a concurrent second write and
     a resend with the same `expect` get `stale`; a resend of a write that was not
     admitted may be admitted;
   - a fork resend does not fork twice;
   - a resent cancel succeeds without further effect;
   - a cancel of a kind that does not accept it gets `not_allowed`, and a requested
     cancel keeps the operation's scopes occupied until it actually ends.
7. **Answers (S7)** r2. Two clients answer two different interactions of one scope,
   and both are delivered. Two answers to one interaction deliver exactly one; the
   other gets `404`. An answer that does not fit its request is `invalid_request`
   and leaves the interaction pending.
8. **Interactions everywhere (§4.3)** r2. Interactions during `load`, `fork`,
   `create`, `set_config`, `list` and outside any operation are shown and answered
   the same way. A session-scoped elicitation that arrives during a `set_mode`
   survives the mode change's end. A cancel of a prompt answers the session's pending
   permissions `cancelled`. A URL flow accepted outside an operation stays visible
   after the next operation replaces `op`. Request scopes (each from the ACP input,
   asserting where the interaction is shown and when it ends):
   - a delete of a held session: an elicitation naming its `session/close` ends with
     the close's response while the delete still runs; one naming its
     `session/delete` ends with the delete;
   - an elicitation naming a request no write sent (`initialize` on reconnect, a
     release's close) is shown in `global` and ends with that request;
   - an elicitation with `requestId: null`, and one naming an answered request, are
     refused with `invalid_params`, with the two cases distinguishable, and never
     shown;
   - a permission and a URL elicitation for the session a `create` is creating are
     shown in `global`, move into the session in the event that installs it, and the
     accepted URL flow stays with the session; when the `create` fails they are
     answered with their cancel outcome and the flow is removed.
9. **Responses (§6.2).**
   - An answer's success arriving before the stream shows the change, and an
     operation B replaced by C before a slow subscriber sees B, both leave the replica
     correct and the client waiting on nothing.
   - `create` and `fork` deliver the new session ID in their response even when `op`
     was already replaced.
10. **Occupation (S5, §6.1).**
    - A prompt on a held session racing its delete is refused.
    - A subscription to an unheld session during its delete gets `end: deleted`, or
      its load after a failed delete.
11. **URL flows (S7)** r2, release.
    - A URL flow of `global` (authenticate), of `catalog` (`list`) and of a session
      survives a reload of the client between accept and completion.
    - The Agent's completion, also long after the accept, shows it `completed` on
      every replica; the Agent connection's end shows it `ended`; the two are
      distinguishable in the browser.
    - A request-scoped flow outlives its parent request: accept, then the parent's
      response (once success, once a JSON-RPC error), and the flow is still
      `waiting` and keeps its session needed; a late `elicitation/complete` then
      shows it `completed`. In the same run an interaction of that request not yet
      answered ends with the response, answered with its cancel outcome, and the
      bridge no longer has the request outstanding: an elicitation naming it
      afterwards is refused with `invalid_params`.
    - A finished flow keeps its first outcome: a `completed` flow stays `completed`
      when the connection ends afterwards, and an `ended` flow stays `ended` when a completion for its `elicitationId` arrives
      late (which then completes no other flow).
    - A finished flow no longer keeps its session needed. It is removed on a
      client's dismissal (also when dismissed twice; a second gives `404`) and when
      the session scope that shows it ends; a flow of `global` or `catalog` stays
      until dismissed.
12. **Holding (S6, §7).**
    - A session released before its first subscription is loaded by that
      subscription and continues its log.
    - A session created on an Agent without `session/load` and `session/resume` is
      never released while the connection lasts, and a late first subscription finds
      it.
    - A one-off read holds nothing.
    - A session kept only by a URL flow, a pending interaction or a running terminal
      is released (if it can be brought back) once that resource ends, and is not
      released before. A released terminal's output does not keep it.
13. **Attachments (§8)** r2. Attachments are readable while referenced by `log` or
    `live`, return the block as received, and stop being readable once nothing
    references them.
14. **Delete (§6.1).** Delete of a held session ends its scope with `deleted`; a failed
    delete leaves its error on `global.op`.
15. **Client obligations (C2, C3).** After a write without a definite answer, the
    client does not resubmit it with a new `expect` when it later sees `stale`;
    never-sent and definitely refused queued input is resubmitted.
16. **Terminal input (S7, C3).** Input that the bridge wrote but whose response was
    lost is not resent by the client; the same text typed again by the user is
    delivered as new input. Repeated dismissal and repeated resize to the same size
    leave the same state.
17. **Terminal logins (S7).** A login with method A fails and is not dismissed; a
    login with method B starts. A fresh replica shows both terminals with their own
    method.
18. **Controls (§4.2)** r2. A successful `set_mode` or `set_config` whose response
    comes without a notification shows the new mode or the confirmed config options
    when its response arrives; a failed one changes nothing and returns its error.
19. **Retained content (S8)** r2. A scripted turn with thoughts, message chunks of two
    `messageId`s around a tool call, chunks of one `messageId` on both sides of a
    tool call (`m1 "a" → tool → m1 "b"`: two items, both `m1`), a tool call with sparse updates (status, content
    with a diff and a terminal, locations, raw input and output, `_meta`), a legacy
    plan and an ID-addressed plan, and a compaction with summary chunks: after it
    completes, after it fails, after it is cancelled, and after a `load` replays it,
    the log holds each entity in its final reduced state, in timeline order, with the
    message boundaries.
20. **Sparse updates (§4.1)** r2. A `tool_call_update` that changes only `content`
    produces a document change and a new hash; one that omits a collection keeps it;
    one that sends a collection replaces it whole, and `content: []` clears it; one
    with `{ name: null, content: null, rawOutput: null }` leaves name, content and raw
    output as they were; one for an unknown ID starts an item. A compaction update
    with `summary: null` clears the summary, and one without `summary` keeps it.
21. **Late and idle updates (S2)** r2. A `tool_call_update` and a compaction summary
    chunk that arrive after the prompt's response change the settled entry they
    belong to, by a log change from its index; the later entries' turns are unchanged
    and their hashes are recomputed; every replica converges, including one that was
    coalesced. A message, plan or compaction the Agent sends while no
    prompt runs appears in the tail and settles into the last entry when the next
    prompt starts.
22. **Failure keeps output (S2, S8)** r2. A prompt that streams messages and a tool
    call and then fails with an Agent error, and one cut by the Agent connection's
    end, both settle an entry with that content and `end.error`.
23. **Partial history (S1, §5.2)** r2. A resume-only session that was released and is
    opened again ends its load with `history: "partial"`; a client that held its old
    log is never told the history is empty. A resume of a held session keeps its log
    and status.
24. **Released terminals (S7)** r2. A terminal embedded in a tool call and then
    released keeps its final output and exit status, marked `released`, for every
    replica and a fresh one; it no longer keeps its session needed; it is removed
    once nothing embeds it. Then, in one scenario: embed → exit → release → stay
    subscribed → the Agent connection ends (and, separately, a terminal login
    succeeds and the Agent reconnects) → a resume that keeps the log: the output,
    `exitStatus` and `truncated` are unchanged, `terminal/*` on the ID fails, and the
    terminal does not keep the session needed. Persistence across a bridge restart is
    not required.
25. **Opaque fields (S8)** r2. `_meta` on a chunk, a content block, a tool call, a plan,
    a compaction, a compaction summary chunk and a prompt response, and annotations on
    blocks, are in the log as received. Two text chunks, and two summary chunks, that
    differ only in the chunk's `_meta` stay two chunks with their own `_meta`.
26. **User chunks (§4.1)** r2. User message chunks the Agent sends during a live
    prompt are items of its turn, in order.
27. **Replay authority (§4.1)** r2. A replay that differs from what was observed live
    (fewer details, other IDs) replaces the log; nothing the replay did not provide is
    invented, and the observed `end` of replaced turns is not carried over.
28. **Controls during a prompt (S4)** r2, release. While a prompt runs, `set_mode` and
    a boolean `set_config` are delivered to the Agent and their confirmed values reach
    every replica; the prompt is not affected. In the browser, the v0.2.8 case: the
    "Verbose" switch toggles during a running prompt.
29. **Close (S4, §6.1)** r2, release. In the browser, the v0.2.8 case: closing a
    running session asks for confirmation; dismissing it sends no request; confirming
    sends exactly one close and returns to the project. At the bridge: a close during
    a prompt answers pending permissions `cancelled`, refuses a new prompt while
    `closing`, ends the scope with `closed` on success and leaves the session in the
    catalog; an Agent error leaves the session as it was; a later subscription loads
    it again.
30. **Message identity and turns (§4.1)** r2. A replay of a user message of text and an
    image with one `messageId` is one turn; `user m1 → tool → user m1` in a replay is
    one turn; two user messages with different IDs are two turns. Live, the bridge's
    user item has one chunk per prompt block, and an Agent user chunk starts no turn.
31. **Plan identity (§4.1)** r2. Turn A creates ID plans P and Q; turn B updates and
    then removes P. The log holds one item for P, at its position in turn A, in its
    removed state, and one current item for Q; no older state of P remains current.
    A legacy plan sent in turns A and B leaves one snapshot in each, and the current
    plan is B's.
32. **Diagnostics (§4.2)** r2, release. MCP-over-ACP connect, message and disconnect
    appear in `global.live.diagnostics.mcp` (connection count and messages, bounded,
    oldest dropped with `truncated`); Agent and MCP stderr appear in `agentStderr`;
    a fresh replica sees the same; nothing is kept across a bridge restart.
33. **Markdown export (§8)** r2, release. The export case of the
    [release parity matrix](release-parity.md#markdown-export), from a settled log,
    from the tail and after a reconnect.
34. **Per-interaction answers in the browser** r2, release. Two pending interactions
    of one session can be answered separately, in either order, as in v0.2.8.
35. **Settled tool cards in the browser** r2, release. Completed, failed and reloaded
    turns show tool cards, thoughts, plans and compactions collapsed by default and
    expandable, with the disclosure kept across new turns; thread search and change
    review find their content.
36. **First sparse tool state (§4.1)** r2. From the ACP input, for each of: a
    permission whose `toolCall` is `{ toolCallId: "t" }` before any `tool_call`; a first
    `tool_call_update` `{ toolCallId: "u", status: "pending" }`; then later updates
    that add `title`, `kind`, `content` and `rawInput`, and a `tool_call` for `t`:
    - the item exists from the first update, at its position, and holds exactly the
      fields received so far; no `title`, `kind` or `status` is filled in, no update
      is dropped, and no `null` appears in the state;
    - each later update fills in its fields by the patch rule, and the `tool_call`
      gives its fields, its schema defaults included;
    - the same holds in the tail, in the settled log, in a replay that sends the same
      updates, and in the hash; the generated TypeScript types the item's `call` as
      `ToolCallUpdate`, and the browser renders an item without `title` (as not
      reported) in the live turn, a settled turn, a reload and the export.
37. **Actions hold their session (S6, §6.1)** r2. With a release delay of `0`, an
    idle session gets a `set_config` the scripted Agent leaves unanswered, and the
    last subscription ends:
    - the session stays held and is not released, closed or loaded again while the
      request is outstanding;
    - a session-scoped elicitation the Agent sends meanwhile is shown in the session
      and answered; a subscription made meanwhile aligns with the held documents;
    - the delayed response folds into that session's `controls` and answers the
      original HTTP request; then, with nothing else needed, the session is released
      after the delay, once;
    - the same for `set_mode` and for `close`, where the release sends no second
      `session/close` and the close ends the scope with `closed`; while a release's
      own `session/close` is outstanding, a client `close` gets `not_allowed`;
    - the Agent connection ending while the request is outstanding answers it
      `unavailable` and changes no `controls`.

The bridge's handling of ACP failures and its internal clean-up belong to layer 1 and
are tested there. They add no duties to clients.

## 12. Defaults open to review

Revision 1's defaults 1 and 5 (`canon` takes message content only; a completed turn's
process disappears from every client) are withdrawn by S8. The remaining ones, and
revision 2's own choices:

1. A turn whose prompt the bridge sent always settles, even if it failed; after a
   reopen, the replay decides (§4.1).
2. A turn's `end` is part of the entry and of its hash, so a reload whose replay has
   no `end` replaces live-observed entries. The alternative, an unhashed `end`, would
   let two different logs align as equal (audit D6).
3. Legacy plans keep one snapshot per turn, as v0.2.8 did; ID-addressed plans are one
   item per session (audit Q3).
4. An update for a settled entity rewrites the log from that entry. Late updates are
   rare; the alternative, a patch kept in `live` beside the log, would split one
   entity over two places.
5. Content between prompts stays in the tail until the next prompt starts or the
   connection ends, so streaming it costs `live` patches only.
6. Alignment negotiates the log head only and otherwise sends the whole log. Several
   sample points per subscription (`n`, `n-1`, `n-2`, `n-4`, …) are a compatible
   extension if long logs of whole turns make full alignment costly.
7. Global operations (create, delete, authentication) are serialized through one
   `global.op`. Catalog listing is a separate scope and is not blocked by them.
8. `create`, `fork` and `list` deliver their result in the HTTP response; a lost
   response loses it for that client (§6.2).
9. Sessions the Agent cannot load or resume are never released while the connection
   lasts (§7). A resume-only session is released, and comes back `partial`, as v0.2.8
   released it and showed a missing-history notice; keeping it held instead trades
   memory for history (audit L4).
10. Controls and close are actions, so they run during a prompt (release parity R1,
    R2). Close sets `closing`, which is the only new session state they need.
11. Diagnostics keep the last 200 MCP-over-ACP messages and a bounded stderr tail in
    memory (release parity R3); the bound is a default, not a protocol constant.
12. A request-scoped elicitation that names no outstanding request is refused rather
    than shown in `global` (audit Q6).
13. A URL flow keeps its outcome in `urlFlows` until dismissed (release parity R4,
    P4); the alternative, a transient notice outside the documents, would break C1.
    An accepted request-scoped flow is not ended by its parent request's response
    (P5, settled).
14. A tool item's state is ACP's `ToolCallUpdate`, so a call first seen sparsely is
    represented as received (§4.1). No own type and no placeholder values are added.
15. An action waiting for the Agent keeps its session held through the outstanding
    request the bridge already tracks (§7); no slot, owner or record is added.

## 13. Migration

The protocol is specified in [protocol.md](protocol.md) and the bridge work in
[state-sync.md](state-sync.md). Revision 1 was reached in the steps below (§5 of the
plan, all done). Revision 2 is migrated by
[state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2), in steps that each
land with their acceptance cases.

The earlier v2 draft was never completed, has no client on `main`, and is
over-designed for this model, so it was removed rather than kept beside the new one;
the new protocol took its place under `/api/v2`. In outline:

1. **Settle this proposal** and write the protocol and its implementation plan
   (docs only).
2. **Remove the earlier v2 draft** from the bridge: its routes, records, intents,
   retained log, owner tags and their tests. The web UI keeps running on `/api/v1`.
3. **Bridge:** `canon` and the hash chain; documents, operations with admission and
   occupation, and the commit step; the stream with alignment and hold by need; the
   operation endpoint, the catalog scope and resource actions. Bridge tests first
   (acceptance 1–16), following
   [bridge-state-machine-tdd.md](bridge-state-machine-tdd.md).
4. **Client:** the generic replica engine and the web projection on the new v2. PR
   #34, built on the earlier draft, is not landed; the parts that fit (the replica
   engine and validation) are carried over.
5. **Remove v1**, including the remaining v1 reads (attachment links, Markdown
   export).
6. **Layer 1 clean-up**, separately: remove incarnation-based isolation from the
   bridge internals, now that no client-visible behavior depends on it.
