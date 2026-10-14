# Client API redesign

Status: proposal for review, 2026-10-14 (fourth version). It replaces the client
protocol of [protocol.md](protocol.md) (v2) and the replication rules of
[state-sync.md](state-sync.md). Nothing here is implemented yet. The review that
led here is recorded in [protocol-v2-review.md](protocol-v2-review.md) and in the
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
| log, entries identified by `(index, term)` | `log`: the final messages of completed turns, entries identified by `(index, hash)` of a hash chain |
| uncommitted entries, which may be overwritten | `live`: everything else, replaced as a whole on alignment |
| AppendEntries consistency check | every log change names the position it follows; the client checks it |
| a follower truncates a conflicting suffix | a client truncates the suffix that differs and installs the bridge's |
| clients send requests to the leader | clients send writes to the bridge, which handles them serially |
| a new leader may hold a different log | a rebuilt bridge (reload, restart) may hold a different log; clients converge the same way |

Unlike Raft there is no majority, no term and no persistence. The bridge's memory
is the authority for clients; the ACP server is the authority behind it.

## 3. Rules

**S1 Document.** Every scope is one document with two parts: `log`, an append-only
sequence of entries, and `live`, one JSON value. Each log entry has a chain hash
`h_i = H(h_{i-1} ‖ canon(entry_i))`, with `h_0` fixed. A session scope's log is
its turn history; every other scope's log is always empty.

**S2 Commit.** Only a completed turn enters the log. Appending its entry, removing
its process from `live` and moving its operation to a final status are one event.
Every other change happens in `live` only.

**S3 Alignment.** A client subscribes with the head of its log, `(n, h_n)`. The
bridge takes one cut of the scope and sends it as one event: its log from `n` if
it has hash `h_n` at index `n`, otherwise from `0`, together with the whole of
`live`. Taking the cut and continuing with the changes after it is one serial step
of the bridge, so the changes follow the cut exactly. Every later log change names
the position `(index, hash)` it follows; a client that cannot match it subscribes
again.

**S4 Operation.** All bridge work that may wait for a client is an operation of a
scope. Every scope's `live` holds exactly one operation, `op = { id, kind,
status, … }`. The bridge generates `id` at random and never reuses it. An
operation's ID identifies it and is what writes are admitted against; it says
nothing about what a replica has seen.

**S5 Admission.** Every write names `expect`, an operation ID, and the bridge
handles writes and loads one at a time:

- a write that **starts an operation** is admitted when `expect` equals `op.id`,
  `op` has ended, and every other scope the operation acts on is free (below); the
  new operation replaces `op` in the same step;
- a write that **acts on the running operation** (cancel, answer) is admitted when
  `expect` equals `op.id` and `op` is running; an answer must also cover exactly
  the operation's pending interactions.

An admitted operation **occupies** every scope it acts on until it ends: it is the
`op` of each such scope the bridge holds, and it holds back the load of each such
scope the bridge does not hold. A scope is free when its `op` has ended and no
operation occupies it. Anything else is refused.

**S6 Holding.** The bridge holds a session while it is needed: a client subscribes
to it, an operation occupies it, or a terminal or URL flow of it is alive. It may
release a session that is not needed only if the Agent can load it again; a
session it cannot load again stays held until it is deleted or the Agent
connection ends. Subscribing to a session the bridge does not hold makes the bridge
hold it, which starts a `load` operation.

A client's presentation is a pure function of `log`, `live` and its own UI state
(drafts, queued input, disclosure, scroll position). UI state never enters the
protocol.

## 4. Documents

### 4.1 Log entries

A log entry is one completed turn:

```ts
interface Turn {
  prompt: ContentBlock[];   // the user's message
  reply: ContentBlock[];    // the Agent's message content, in order
}
```

`canon(turn)` defines its identity in three steps.

1. **Messages.** Chunks are reduced to logical messages before anything else.
   Chunks of the same kind (`user_message_chunk` or `agent_message_chunk`) belong to
   one message when they carry the same `messageId`. Without `messageId`, they
   belong to one message when they are consecutive, with only process updates
   (thoughts, tool calls, plans) between them.
2. **Turns.** A user message starts a turn. The Agent messages up to the next user
   message are its reply, concatenated in order. Process updates are not part of
   the entry.
3. **Content.** `prompt` and `reply` are normalized by the same rules:
   - adjacent text blocks are merged;
   - image, audio and resource blocks are replaced by an attachment reference
     `{ type: "attachment", id }`, where `id` is the SHA-256 of the canonical block;
   - `_meta` fields are dropped;
   - the result is serialized as canonical JSON (RFC 8785).

`h_0` is 32 zero bytes and `H` is SHA-256. On the wire a hash is the base64url form
of its first 16 bytes.

The same messages give the same entry, however they were chunked, and the same
history gives the same chain, however often the session was opened. A live turn
and a replayed one go through the same reduction: the live prompt is the
operation's input, and its reply is the turn's Agent messages.

A turn enters the log when the Agent answered its prompt, with any stop reason,
`cancelled` included. A turn whose prompt failed with an error does not; its
prompt and error stay on its operation (§4.4).

Identity serves synchronization only. Admission never compares hashes (S5). Where
the Agent's replay lacks information the live turn had (two prompts without a reply
in between, for example), the entries differ and alignment replaces the client's
log. That costs a redrawn history, never a wrong write.

### 4.2 Live state

```ts
interface ScopeLive {            // what every scope's live has
  op: Operation;
  urlFlows: Record<string, UrlFlow>;     // accepted URL elicitations, by interaction
}

interface SessionLive extends ScopeLive {
  cwd: string;
  controls: Controls;            // modes, configOptions, commands, info, usage (reduced)
  terminals: Record<string, Terminal>;   // terminals the Agent has not released
}

interface GlobalLive extends ScopeLive {
  agent: AgentState;             // connection phase, launch, initialize, authentication
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  terminals: Record<string, Terminal>;   // terminal login
}

interface CatalogLive extends ScopeLive {
  revision: string;              // changes when the Agent's session list may have changed
}
```

- `live` contains only what the bridge holds now. Nothing is retained for clients
  that come later.
- **Every scope has `op` and `urlFlows`.** Any scope with operations can receive
  interactions, so any scope can hold URL flows. A new scope gets both without
  special handling.

### 4.3 Resources

Terminals and URL flows are **resources**: they live in their scope's `live` by
their own lifecycle, not by the lifecycle of the operation that created them.

- **An operation's end** clears its pending interactions. It does not end the
  resources it created.
- **A URL flow** starts when a client accepts a URL elicitation: the interaction
  leaves the pending set and the flow enters `urlFlows`. Accepting only consents;
  the external flow starts then. The flow is removed when one of these happens:
  - the Agent sends its completion notification;
  - a client dismisses it (§6.1), because ACP makes the completion notification
    optional and the user must be able to give up waiting;
  - its scope ends, or the Agent connection that started it ends.

  No completion record is kept.
- **A terminal** is removed when the Agent releases it, or when its scope or the
  Agent connection ends. A terminal login is removed when it exits and a client
  dismisses it.
- **Holding.** A live resource keeps its session needed (S6). When the last
  resource of an unsubscribed, unoccupied session is removed, the session may be
  released.

### 4.4 Operations

```ts
interface Operation {
  id: string;
  kind: string;                  // see §6.1
  status: "running" | "completed" | "failed" | "cancelled";
  input: unknown;                // the write's arguments: the prompt, the mode, …
  cancelRequested: boolean;
  interactions: Record<string, Interaction>;   // pending permissions and elicitations
  process?: SessionUpdate[];     // a running turn: its updates, folded
  result?: unknown;              // completed: the stop reason, a new session ID, …
  error?: ProtocolError;         // failed
}
```

- An operation's result is its final `op`, visible until the next operation
  replaces it. A failed turn keeps its prompt and error there; a refused mode
  change keeps the ACP error.
- Any operation may wait for the user: waiting is a non-empty `interactions`, not a
  separate state.
- A session the bridge has just held starts with a `load` operation. A session
  created by `create` or `fork` starts with a completed `open`.
- **Agent requests outside an operation.** An Agent request that arrives while no
  operation of its scope runs starts an `agent_request` operation in that scope,
  which holds the request's interactions. If the scope is not free, the bridge
  declines the request as ACP allows.

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
event's `live` change. One event kind serves both:

- **alignment (S3):** `from` is the matched index and `live` is `{ replace }`;
- **commit (S2):** `from` is the client's length and `live` is the patch that ends
  the turn.

After every event the client's document is a document the bridge held at some
moment. No event leaves a committed entry beside its own process.

### 5.2 Synchronization

Every synchronization is one alignment event (S3). In particular:

- **Open and reconnect** are the same: subscribe with the log head the client
  holds, if any.
- **A broken stream** is not resumed. The client subscribes again with its head.
  It keeps every entry the bridge still has and replaces `live`.
- **A diverged log** (after a reopen whose replay differs, or a bridge restart)
  fails the head match. The bridge sends its whole log, and the client replaces
  its own. If the bridge's log is shorter, the same happens.
- **A slow subscriber** is treated as if it had subscribed again: the bridge drops
  its queued events for the scope. For a scope that still exists, it sends one
  alignment event, from the last log position it sent to that subscriber. For a
  scope that ended, it keeps the `end` event and sends nothing after it.
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
| `session/<id>` | `prompt { prompt }`, `set_mode { modeId }`, `set_config { configId, value }`, `fork { cwd }` | `cancel`, `answer { answers }` |
| `global` | `create { cwd }`, `delete { sessionId }`, `authenticate { methodId }`, `logout`, `terminal_login { methodId }` | `cancel`, `answer { answers }` |
| `catalog` | `list { cursor }` | `cancel`, `answer { answers }` |

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
- **The catalog** is its own scope, so that listing does not contend with global
  writes. `list` runs the Agent's `session/list`. An elicitation the Agent sends
  during it is an interaction of the `list` operation, answered like any other.
- **Resource actions are not operations.** Terminal input, resize, and dismissing
  a URL flow or an exited terminal login address a resource by its ID and take no
  `expect`:

  ```text
  POST /api/v2/scopes/{scope}/resources/{id}/{action}
  ```

  They never wait for a client and are idempotent by resource ID: dismissing a
  flow twice, or one that already completed, leaves the same state. A missing
  resource gives `404`.

### 6.2 Request and response

```text
POST /api/v2/scopes/{scope}/ops   { "expect": "<op.id>", "kind": "...", "args": { … } }
```

- **Admitted:** `{ op: "<id>" }`. This is the new operation's ID, or the running
  operation's ID for cancel and answer. A success means the write was admitted;
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
  | `not_allowed` | the operation is running and the write starts one, or the operation has ended and the write acts on it, or another scope the operation acts on is not free, or an answer does not cover exactly the pending interactions |
  | `invalid_request` | the request is malformed |
  | `unsupported` | the Agent did not advertise the capability |
  | `unavailable` | the Agent is not connected |

- **What the replica shows.** The replica shows the bridge's current state, which
  may lag behind or already be past a write. The protocol makes no promise that a
  replica observes the effect of a particular write. An admitted answer may still
  show its interactions until the stream catches up; a completed operation may
  already have been replaced by the next one.

### 6.3 Consequences of S5

- **Concurrency.** Two clients that name the same operation race; the bridge admits
  one, and the other gets `stale`.
- **Lost responses.** A resend names an operation that the first send already
  replaced, and gets `stale`. A resent cancel is harmless. A resent answer finds
  the interactions gone and is refused.
- **No ABA.** A fork returns the source to the same history and to an ended
  operation, but with a new `op.id`.
- **Group answers.** An interaction that arrives or is cancelled while the user
  composes changes the pending set, and the answer is refused. ACP requests do not
  change after they arrive, so the IDs are enough to compare.
- **Operations across scopes.** An operation that acts on several scopes occupies
  all of them, so no conflicting write and no load can run on any of them meanwhile.
- **No intent tables.** No idempotency key, intent table, tombstone or retention of
  results is needed.

### 6.4 Client obligations

- **Refused input stays in the client.** The prompt, answers or control change
  remain, for the user to submit again once the replica is in sync.
- **An uncertain write is never resent automatically.** If a write was sent and no
  definite answer came back (the connection broke, the request timed out), a later
  `stale` does not prove it was not admitted. The client must not resubmit that
  input with a new `expect` by itself. It keeps the input as a draft for the user
  to judge.
- **Queued input may be submitted automatically.** A queued prompt or control
  change that was never sent, or that was refused with a definite answer, is
  submitted when the replica shows the operation ended, naming that operation.

## 7. Holding

- **Needed sessions are held.** A session is needed while a client subscribes to
  it, an operation occupies it, or one of its terminals or URL flows is alive (S6).
- **Release only what can be loaded again.** The bridge releases a session that is
  not needed, as an ordinary ACP client would close it, only if the Agent
  advertises `session/load` or `session/resume`. How long it waits first affects
  performance only. Without either capability the session stays held until it is
  deleted or the Agent connection ends.
- **No hand-over race.** A session released between `create` or `fork` and the
  client's subscription can be loaded again, so that subscription loads it, and
  alignment (S3) continues its log. A session that cannot be loaded again was never
  released.
- **Reopen continues.** A release and a later hold, an ACP reconnect, and a bridge
  restart all end in a `load` operation and a new alignment. If the replay gives
  the same messages, the client keeps its log.
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

**Markdown export** is assembled by the client from the log and the attachment
read. There is no process to fetch and no export endpoint.

## 9. Extending

- **A new presentation layer** (a CLI, an IDE extension, a mobile client)
  implements the generic replica engine (alignment, log truncation and append,
  `prev` checks, `live` replace and patch, subscribing again) and its own view
  function.
- **A new operation** is a new `kind` with its arguments, the scopes it occupies
  and its result. Admission, responses and replication do not change.
- **New state** is a new field of `live`. The engine does not change.

## 10. What changes

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
  `visibleRanges`, `GET …/process`. A completed turn's process is gone (S2).
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

1. **Identity (S1, §4.1).** The same messages give the same entry and hash when:
   - streamed in different chunks;
   - a user message has several content blocks (text and an image), live or
     replayed as several chunks;
   - committed live or replayed by `load`.
2. **Commit (S2).** A completed turn appends its entry, removes its process and
   ends its operation in one event. A failed turn appends nothing and keeps its
   prompt and error on `op`.
3. **Alignment (S3).**
   - A client with a matching head receives only the suffix.
   - A client with a diverged or longer log receives the whole log and replaces
     its own.
   - After a reopen with the same replay, the client keeps its log.
   - Alignment while a turn commits never yields a committed entry beside its own
     process, for a new subscription, a reconnect and a slow subscriber alike.
4. **Synchronization (§5.2).** A broken stream, a slow subscriber and a replaced
   stream all end with the client equal to the bridge. Events of a replaced stream
   change nothing.
5. **Live changes (S1, S4).** Interactions and controls change while the log hash
   stays the same, and the replica follows.
6. **Admission (S5).** For each operation kind:
   - a concurrent second write and a resend after a lost response get `stale`;
   - a fork resend does not fork twice;
   - a resent answer is refused;
   - a resent cancel is harmless.
7. **Group answers (S5).** An answer is refused after an interaction arrives or is
   cancelled.
8. **Interactions everywhere (§4.4).** Interactions of `load`, `fork`, `create`,
   `set_config`, `list` and `agent_request` are shown, answered through the same
   path as a turn's, and cleared when their operation ends. A URL flow accepted in
   an `agent_request` stays visible after that operation ends and after the next
   operation replaces `op`.
9. **Responses (§6.2).**
   - An answer's HTTP success arriving before the stream shows the change, and an
     operation B replaced by C before a slow subscriber sees B, both leave the
     replica correct and the client waiting on nothing.
   - `create` and `fork` deliver the new session ID in their response even when
     `op` was already replaced.
10. **Occupation (S5, §6.1).**
    - A prompt on a held session racing its delete is refused.
    - A subscription to an unheld session during its delete gets `end: deleted`,
      or its load after a failed delete.
11. **URL flows (§4.3).**
    - A URL flow of `global` (authenticate), of `catalog` (`list`) and of a
      session survives a reload of the client between accept and completion.
    - It is removed on the Agent's completion, on a client's dismissal (also when
      dismissed twice), and when its scope or the Agent connection ends.
12. **Holding (S6, §7).**
    - A session released before its first subscription is loaded by that
      subscription and continues its log.
    - A session created on an Agent without `session/load` and `session/resume` is
      never released while the connection lasts, and a late first subscription
      finds it.
    - A one-off read holds nothing.
    - A session kept only by a URL flow or terminal is released (if it can be
      loaded again) once that resource is removed, and is not released before.
13. **Attachments (§8).** Attachments are readable while referenced by `log` or
    `live`, and stop being readable once only a released process referenced them.
14. **Delete (§6.1).** Delete of a held session ends its scope with `deleted`; a
    failed delete leaves its error on `global.op`.
15. **Client obligations (§6.4).** After a write without a definite answer, the
    client does not resubmit it with a new `expect` when it later sees `stale`;
    never-sent and definitely refused queued input is resubmitted.

The bridge's handling of ACP failures and its internal clean-up belong to layer 1
and are tested there. They add no duties to clients.

## 12. Defaults open to review

1. `canon` takes the Agent's message content only. Tool call titles or results are
   not part of the entry (§4.1).
2. A turn whose prompt failed does not enter the log; after a reopen, the replay
   decides (§4.1).
3. Alignment negotiates the log head only and otherwise sends the whole log. The
   log holds final messages only, so this is cheap. Several sample points per
   subscription (`n`, `n-1`, `n-2`, `n-4`, …) are a compatible extension.
4. Global operations (create, delete, authentication) are serialized through one
   `global.op`. An interaction during a create blocks other global writes until it
   is answered. Catalog listing is a separate scope and is not blocked by them.
5. A completed turn's process disappears from every client, including the one that
   watched it run.
6. `create`, `fork` and `list` deliver their result in the HTTP response; a lost
   response loses it for that client (§6.2).
7. Sessions the Agent cannot load again are never released while the connection
   lasts (§7).

## 13. Migration

1. **Settle this proposal**, then rewrite [protocol.md](protocol.md) and
   [state-sync.md](state-sync.md) to match.
2. **Bridge:** the document model, `canon` and the hash chain, alignment and the
   event stream, operations with admission and occupation, the catalog scope, hold
   by need. Bridge tests first (acceptance 1–15), following
   [bridge-state-machine-tdd.md](bridge-state-machine-tdd.md).
3. **Client:** the generic replica engine and the web projection on the new API.
   PR #34 is reworked onto it rather than landed: its replica engine and
   validation are reused; its process cache and compact-history projection are
   removed.
4. **Remove** the current v2 machinery listed in §10.1 and v1, including the
   remaining v1 reads (attachment links, Markdown export).
5. **Layer 1 clean-up**, separately: remove incarnation-based isolation from the
   bridge internals, now that no client-visible behavior depends on it.
