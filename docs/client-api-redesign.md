# Client API redesign

Status: **revision 2, draft for review, 2026-10-16 (#51)**; revision 1 was settled
in review on 2026-10-14 (sixth version) and is what `main` implements. §3 is the
complete rule set; the later sections specify and derive from it. Revision 2 keeps
revision 1's replication (alignment, admission, holding) and changes what the
documents carry: revision 1 kept only message text and removed a turn's process when
it ended, which drops content ACP gave (the
[ACP fidelity audit](acp-fidelity-audit.md)). **The runtime code is not migrated
yet**; [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) is the
plan. It replaces the earlier v2 draft of the client protocol and its replication
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
derivations. Revision 2 rewrites S2 and S7, adds S8, and narrows S4–S6 (§3.4).

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

**S4 Operation.** Every write that starts bridge work is an operation of a scope.
Every scope's `live` holds exactly one operation, `op = { id, kind, status, … }`. The
bridge generates `id` at random and never reuses it. An operation's ID identifies it
and is what writes are admitted against; it says nothing about what a replica has
seen.

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
it, an operation occupies it, or one of its resources can still act (a pending
interaction, an unreleased terminal, a waiting URL flow). It may release a session
that is not needed only if the Agent can load or resume it; a session it cannot bring
back stays held until it is deleted or the Agent connection ends. Subscribing to a
session the bridge does not hold makes the bridge hold it, which starts a `load`
operation. A one-off read holds nothing.

**S7 Resources.** Interactions, terminals and URL flows are resources. They live in
their scope's `live`, each by the lifecycle ACP gives it, not by the operation that
was running when it appeared:

- an **interaction** is an Agent request waiting for a client, in the scope its ACP
  request names. It ends when it is answered, when the Agent cancels it, when its
  parent request ends (a request-scoped elicitation), when a cancel of its session's
  prompt answers it (permissions, as ACP requires), or with its scope or the Agent
  connection;
- a **URL flow** starts when a client accepts a URL elicitation, and ends when the
  Agent completes it, a client dismisses it, or its scope or the Agent connection
  ends;
- a **terminal** ends its process when the Agent releases it, but its frozen output
  stays while content embeds it, as ACP requires; it is removed when nothing embeds
  it, or its scope or the Agent connection ends. A terminal login is removed when it
  has exited and a client dismisses it.

Resource actions (answer, input, resize, dismiss) address a resource by its ID, take no
`expect` and fall outside S5. Each states its repeat semantics: an answer is delivered
at most once, dismiss has no further effect, resize to the same size leaves the same
state, and input is not idempotent: it is written in order, every time.

**S8 Content.** A session's documents carry what ACP gave for the session. Each
accepted session update is reduced by its own ACP semantics: chunks extend their
message, an update patches the entity it names, a replacement replaces, a removal
removes. Nothing else removes or rewrites content: not a turn's end, not an error, not
a release, not a client's display. Fields the bridge does not interpret (`_meta`,
annotations) are carried as received. Control updates are reduced into `controls`;
every other update becomes timeline content.

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

**C3 Uncertainty.** A write or terminal input that was sent without a definite answer
may have taken effect; a later `stale` proves nothing. The client never resends it
automatically, with a new `expect` or otherwise. It keeps the input for the user to
judge. An interaction answer is the exception: it is delivered at most once (S7), so
resending it is safe.

### 3.3 Derivations

| Behavior | From |
| --- | --- |
| Open, reconnect, a slow subscriber, a replaced stream, a bridge restart and a reopen are all one alignment | S3 |
| After a reopen, a matching history is kept; a diverged one is truncated and the bridge's installed | S1, S3 |
| A completed, failed or cancelled turn keeps its tool calls, thoughts, plans and compactions, live and after a load | S2, S8 |
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
| S4: all work that may wait for a client is an operation; Agent requests join or start one (`agent_request`) | S4: operations are client writes and loads; Agent requests are interactions (S7) | D11 |
| S5: an answer covers exactly the pending interactions | an answer is a resource action on one interaction | D10 |
| S6/S7: a terminal is removed on release | its frozen output stays while embedded and holds nothing | D8 |
| S1: a known log | complete or partial | D9 |
| — | S8 Content | D1–D7 |

The finding IDs are those of the [audit](acp-fidelity-audit.md#2-defects).

## 4. Documents

### 4.1 Log entries

A log entry is one settled turn of the session's timeline:

```ts
interface Turn {
  items: Item[];            // reduced session updates of timeline kinds, in order
  end?: { response: PromptResponse } | { error: ProtocolError };  // observed live only
}

type Item = SessionUpdate;  // message, thought, tool, plan and compaction kinds,
                            // with byte-carrying blocks as attachment references
```

**Reduction** follows each update's ACP semantics (protocol §3.2):

- **Messages.** A text chunk extends the previous item when that item is a text chunk
  of the same kind and `messageId` that differs only in its text; any other chunk is
  a new item. Consecutive items of one kind and `messageId` are one message; a
  different `messageId`, or any other item between them, starts a new one.
- **Entities.** A `tool_call` upserts and a `tool_call_update` patches the item of its
  `toolCallId`, anywhere in the session; collections are replaced whole. A
  `compaction_update` upserts the item of its `compactionId`, whose position its first
  update fixed, with patch semantics; summary chunks append to it.
- **Plans.** A plan update replaces the plan's last item in the turn being received,
  or starts one; a removal is the plan's last state. The current plan is the last plan
  item of the timeline.
- **Content.** Image, audio and embedded resource blocks are replaced by an attachment
  reference `{ type: "attachment", id, … }`, where `id` is the SHA-256 of the
  canonical block as received. The bridge keeps the block; a read or a prompt that
  references it gets it back unchanged. `_meta` and annotations stay everywhere.
- The result is serialized as canonical JSON (RFC 8785), and the hash covers all of
  it, `end` included.

**Turns.** A turn starts with a user message and holds what follows until the next
one. A live prompt's turn starts with the user message the bridge sent; a replayed
turn starts with each user message of the replay.

**Settling.** The tail holds the turn being received. A prompt's end appends the tail
as a new entry with its `end`, whatever the stop reason or error: output the Agent
sent is not withdrawn by a later failure. Content that arrives outside a prompt
continues the last turn in the tail and settles into the last entry when the next
prompt starts or the connection ends. An update for an entity already in the log
changes that entry at once; the log change starts at its index.

The same updates give the same entries, however they were chunked, live or replayed.
Updates that differ in IDs, metadata, results or order give different entries; the
bridge does not make them equal. In particular a live turn and its replay usually
differ (the replay has no `end`; an Agent may replay less, or differently), and
alignment then replaces the client's log. That costs a redrawn history, never a wrong
write, and is the price of letting ACP's replay decide. Identity serves
synchronization only. Admission never compares hashes (S5).

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
  terminals: Record<string, Terminal>;        // running, and released but still embedded
}

interface GlobalLive extends ScopeLive {
  agent: AgentState;             // connection phase, launch, initialize, authentication
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  terminals: Record<string, LoginTerminal>;  // terminal logins, each with its methodId
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

- **Interactions.** An Agent request lives in the scope its ACP request names: a
  permission request and a session-scoped elicitation in their session, a
  request-scoped elicitation in the scope of the operation whose request it names
  (that operation is its `parent`), anything else in `global`. While the session is
  not held yet, its interactions are shown in the scope of the operation that brings
  it. An interaction ends when it is answered, when the Agent cancels it, when its
  parent ends, when a requested cancel of its session's prompt answers it (permissions
  only, `cancelled`, as ACP requires), or with its scope or the Agent connection. No
  other operation's end touches it.
- **A URL flow** starts when a client accepts a URL elicitation: the interaction leaves
  `interactions` and the flow enters `urlFlows`. Accepting only consents; the external
  flow starts then. The flow is removed when one of these happens:
  - the Agent sends its completion notification;
  - a client dismisses it (§6.1), because ACP makes the completion notification
    optional and the user must be able to give up waiting;
  - its scope ends, or the Agent connection that started it ends.

  No completion record is kept.
- **A terminal** runs until the Agent releases it. After release ACP still has tool
  calls that contain it display its output, so its bounded output and exit status stay,
  frozen and marked `released`, while an item of the log or the tail embeds it. It is
  removed when none does, or when its scope or the Agent connection ends. A terminal
  login is removed when it exits and a client dismisses it.
- **Holding.** A resource that can still act (a pending interaction, an unreleased
  terminal, a waiting URL flow) keeps its session needed (S6). A released terminal's
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
  operation. An operation that waits for its own ACP request's elicitation is the
  `parent` of that interaction.
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
  | { scope: string; end: { reason: "deleted" | "load_failed"; error?: ProtocolError } };

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

A scope ends when its session was deleted, or when its load failed. The event
gives the reason and, for `load_failed`, the error. The client drops the document.
Subscribing again after `load_failed` tries the load again; the client does this
only on the user's request.

## 6. Writes

### 6.1 Operations by scope

| Scope | Starts an operation | Acts on the running operation |
| --- | --- | --- |
| `session/<id>` | `prompt { prompt }`, `set_mode { modeId }`, `set_config { configId, value }`, `fork { cwd }` | `cancel` (prompt) |
| `global` | `create { cwd }`, `delete { sessionId }`, `authenticate { methodId }`, `logout`, `terminal_login { methodId }` | `cancel` (terminal login) |
| `catalog` | `list { cursor }` | — |

- **No open command.** Subscribing holds the session (S6).
- **No close command.** A session that is not needed is released (S6).
- **Delete** is a global operation, so it works for sessions the bridge does not
  hold. It occupies `global` and `session/<id>` (S5):
  - while the session is held, the delete is its `op`, so a prompt naming the
    previous operation gets `stale`;
  - while the session is not held, a subscription to it waits; it receives
    `end: deleted` if the delete succeeds, and its `load` starts if the delete
    fails.

  The bridge closes a held session, deletes it on the Agent and ends the scope with
  `deleted`. Its result or error is on `global.op`.
- **Controls during a prompt.** `set_mode` and `set_config` wait for the session's
  operation to end. ACP allows setting the mode while the Agent generates; this is
  attyd's limit (audit L1), kept until a separate design lifts it.
- **The catalog** is its own scope, so that listing does not contend with global
  writes. `list` runs the Agent's `session/list`. An elicitation the Agent sends
  during it and scoped to that request is an interaction of `catalog` with the `list`
  as its parent, answered like any other.
- **Resource actions are not operations.** Answering an interaction, terminal input,
  resize, and dismissing a URL flow or an exited terminal login address a resource by
  its ID and take no `expect`:

  ```text
  POST /api/v2/scopes/{scope}/resources/{id}/{action}
  ```

  They never wait for a client and fall outside S5 (S7). The resource ID identifies the
  resource, not one action on it, so each action states its own repeat semantics:

  | Action | Repeated |
  | --- | --- |
  | answer | **at most once**: checked against its request, delivered and removed in one step; a repeat, or an answer after the interaction ended, gives `404` and leaves the same state |
  | dismiss | no further effect: a second dismissal, or one after completion, gives `404` and leaves the same state |
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
- **Refused:** `{ error: { code, message } }`. The codes are:

  | Code | Meaning |
  | --- | --- |
  | `stale` | `expect` is not the current `op.id` |
  | `not_allowed` | the operation is running and the write starts one, or the operation has ended and the write acts on it, or another scope the operation acts on is not free |
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
- **Queued input may be submitted automatically.** A queued prompt or control
  change that was never sent, or that was refused with a definite answer, is
  submitted when the replica shows the operation ended, naming that operation.

## 7. Holding

- **Needed sessions are held.** A session is needed while a client subscribes to
  it, an operation occupies it, or one of its resources can still act: a pending
  interaction, an unreleased terminal, a waiting URL flow (S6).
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

**Markdown export** is assembled by the client from the log, the tail and the
attachment read. There is no export endpoint.

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
- The `catalog` scope and `agent_request` operations.
- `urlFlows` on every scope; resource lifecycles independent of operations;
  resource actions, including dismissing a URL flow.
- Hold by subscription and need, release only of loadable sessions; delete as a
  global operation.

## 11. Acceptance

Each rule gets tests at the bridge first and then in the client engine. Document
consistency is checked after **every** event, not only once synchronization ends.
Revision 2 adds a second kind of check: a replica that equals the bridge's projection
proves nothing if the projection dropped content. Cases 1, 2, 13 and 19–27 therefore
also assert, **from the ACP input**, which items and fields the log or the tail holds
(audit §8). Cases marked r2 are new or changed in revision 2; their runtime code is
not migrated yet.

1. **Identity (S1, S8, §4.1)** r2. The same updates give the same entries and hashes
   when:
   - streamed in different chunks;
   - a user message has several content blocks (text and an image), live or replayed
     as several chunks;
   - reduced live into the tail and settled, or replayed by `load`.

   Updates that differ only in a tool result, a `messageId`, a message boundary or a
   `_meta` value give different hashes. Normalized content with attachment references
   and prompts that reference attachments pass the generated types and validation;
   the bridge resolves references to the blocks as received before it calls the
   Agent, and an attachment read returns them unchanged, `_meta` included.
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
   the same way. A request-scoped elicitation ends with its parent's request. A
   session-scoped elicitation that arrives during a `set_mode` survives the mode
   change's end. A cancel of a prompt answers the session's pending permissions
   `cancelled`. Interactions of a delete's `session/close` are shown in the session
   and can be answered. A URL flow accepted outside an operation stays visible after
   the next operation replaces `op`.
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
11. **URL flows (S7).**
    - A URL flow of `global` (authenticate), of `catalog` (`list`) and of a session
      survives a reload of the client between accept and completion.
    - It is removed on the Agent's completion, on a client's dismissal (also when
      dismissed twice), and when its scope or the Agent connection ends.
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
18. **Controls (§4.2).** A successful `set_mode` or `set_config` whose response comes
    without a notification shows the new mode or the confirmed config options when
    its operation ends; a failed one changes nothing.
19. **Retained content (S8)** r2. A scripted turn with thoughts, message chunks of two
    `messageId`s around a tool call, a tool call with sparse updates (status, content
    with a diff and a terminal, locations, raw input and output, `_meta`), a legacy
    plan and an ID-addressed plan, and a compaction with summary chunks: after it
    completes, after it fails, after it is cancelled, and after a `load` replays it,
    the log holds each entity in its final reduced state, in timeline order, with the
    message boundaries.
20. **Sparse updates (§4.1)** r2. A `tool_call_update` that changes only `content`
    produces a document change and a new hash; one that omits a collection keeps it;
    one that sends a collection replaces it whole; one for an unknown ID starts an
    item.
21. **Late and idle updates (S2)** r2. A `tool_call_update` and a compaction summary
    chunk that arrive after the prompt's response change the settled entry they
    belong to, by a log change from its index; every replica converges, including
    one that was coalesced. A message, plan or compaction the Agent sends while no
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
    once nothing embeds it.
25. **Opaque fields (S8)** r2. `_meta` on a chunk, a content block, a tool call, a plan,
    a compaction and a prompt response, and annotations on blocks, are in the log as
    received.
26. **User chunks (§4.1)** r2. User message chunks the Agent sends during a live
    prompt are items of its turn, in order.
27. **Replay authority (§4.1)** r2. A replay that differs from what was observed live
    (fewer details, other IDs) replaces the log; nothing the replay did not provide is
    invented, and the observed `end` of replaced turns is not carried over.

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
3. Legacy plans keep one item per turn with the plan's last state in it (audit Q3).
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
   lasts (§7). A resume-only session is released, and comes back `partial`; keeping
   it held instead trades memory for history (audit L4).
10. Controls wait for a running prompt to end (audit L1).

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
