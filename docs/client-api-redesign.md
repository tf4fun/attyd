# Client API redesign

Status: proposal for review, 2026-10-14 (second version). It replaces the client
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
| uncommitted entries, which may be overwritten | `live`: everything else, replaced as a whole on synchronization |
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
its turn history; the `global` scope's log is always empty.

**S2 Commit.** Only a completed turn enters the log. Appending its entry, removing
its process from `live` and moving its operation to a final status are one event.
Every other change happens in `live` only.

**S3 Alignment.** A client subscribes with the head of its log, `(n, h_n)`. If the
bridge's log has hash `h_n` at index `n`, the bridge sends its log from `n`;
otherwise from `0`. It then sends the whole of `live`, followed by changes in
order. Every log change names the position `(index, hash)` it follows; a client
that cannot match it subscribes again.

**S4 Operation.** Every scope's `live` holds exactly one operation,
`op = { id, kind, status, … }`. The bridge generates `id` at random and never
reuses it.

**S5 Admission.** Every write names `expect`, an operation ID, and the bridge
handles writes one at a time:

- a write that **starts an operation** is admitted when `expect` equals `op.id` and
  `op` has ended; the new operation replaces `op` in the same step;
- a write that **acts on the running operation** (cancel, answer) is admitted when
  `expect` equals `op.id` and `op` is running; an answer must also cover exactly
  the operation's pending interactions.

Anything else is refused.

**S6 Holding.** The bridge holds a session while a client subscribes to it or an
operation on it runs, and may release it otherwise. Subscribing to a session the
bridge does not hold makes the bridge hold it, which starts a `load` operation.

A client's presentation is a pure function of `log`, `live` and its own UI state
(drafts, queued input, disclosure, scroll position). UI state never enters the
protocol.

## 4. Documents

### 4.1 Log entries

A log entry is one completed turn:

```ts
interface Turn {
  prompt: ContentBlock[];   // what the user sent
  reply: ContentBlock[];    // the Agent's message content, in order
}
```

`canon(turn)` defines its identity:

- `reply` holds the content of the turn's `agent_message_chunk` updates, in order,
  with adjacent text blocks merged. Thoughts, tool calls and plans are process,
  not part of the entry.
- Image, audio and resource blocks are replaced by an attachment reference
  `{ type: "attachment", id }`, where `id` is the SHA-256 of the canonical block.
- `_meta` fields are dropped. The result is serialized as canonical JSON
  (RFC 8785).
- `h_0` is 32 zero bytes and `H` is SHA-256. On the wire a hash is the base64url
  form of its first 16 bytes.

The same final content gives the same entry, however it was streamed, and the same
history gives the same chain, however often the session was opened.

A turn enters the log when the Agent answered its prompt, with any stop reason,
`cancelled` included. A turn whose prompt failed with an error does not; its
prompt and error stay on its operation (§4.3). When the bridge loads a session, it
builds the log from the replay: a `user_message_chunk` starts an entry, and the
Agent's messages until the next user message are its reply. Replayed tool calls
and thoughts are dropped.

Identity serves synchronization only. Admission never compares hashes (S5), so an
imperfect `canon` costs at most a redrawn history after a reopen, never a wrong
write.

### 4.2 Live state

```ts
interface SessionLive {
  cwd: string;
  controls: Controls;            // modes, configOptions, commands, info, usage (reduced)
  op: Operation;
  terminals: Record<string, Terminal>;   // terminals the Agent has not released
  urlFlows: Record<string, UrlFlow>;     // accepted URL elicitations, by interaction
}

interface GlobalLive {
  agent: AgentState;             // connection phase, launch, initialize, authentication
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  catalog: { revision: string };
  op: Operation;
  terminals: Record<string, Terminal>;   // terminal login
}
```

`live` contains only what the bridge holds now. Nothing is retained for clients
that come later.

### 4.3 Operations

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
`prev`, truncating its log to `from` entries and appending `entries`. One event
kind serves both alignment (S3) and commit (S2): at alignment `from` is the matched
index; at a commit `from` is the client's length and the event also carries the
`live` patch that ends the turn.

### 5.2 Synchronization

Every synchronization is a subscription (S3). In particular:

- **Open and reconnect** are the same: subscribe with the log head the client
  holds, if any.
- **A broken stream** is not resumed. The client subscribes again with its head.
  It keeps every entry the bridge still has and replaces `live`.
- **A diverged log** (after a reopen whose replay differs, or a bridge restart)
  fails the head match. The bridge sends its whole log, and the client replaces
  its own. If the bridge's log is shorter, the same happens.
- **A slow subscriber** is treated as if it had subscribed again: the bridge drops
  its queued events for the scope and sends a fresh alignment from the last log
  position it sent, followed by `live`.
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
| session | `prompt { prompt }`, `set_mode { modeId }`, `set_config { configId, value }`, `fork { cwd }` | `cancel`, `answer { answers }` |
| global | `create { cwd }`, `delete { sessionId }`, `authenticate { methodId }`, `logout`, `terminal_login { methodId }` | `cancel`, `answer { answers }` |

- **No open command.** Subscribing holds the session (S6).
- **No close command.** A session the bridge no longer needs is released (S6).
- **Delete** is a global operation, so it works for sessions the bridge does not
  hold. For a held session it is admitted only while that session's operation has
  ended. The bridge closes the session, deletes it on the Agent and ends the scope
  with `deleted`. Its result or error is on `global.op`.
- **Create and fork** complete with `result: { sessionId }`. The client subscribes
  to the new session.
- **Terminal input and resize** are not writes. They go to a terminal the bridge
  runs and change no document state the client decides on.

### 6.2 Request and response

```text
POST /api/v2/scopes/{scope}/ops   { "expect": "<op.id>", "kind": "...", "args": { … } }
```

- **Admitted:** `{ op: "<id>" }`. This is the new operation's ID, or the running
  operation's ID for cancel and answer.
- **Refused:** `{ error: { code, message } }`. The codes are:

  | Code | Meaning |
  | --- | --- |
  | `stale` | `expect` is not the current `op.id` |
  | `not_allowed` | the operation is running and the write starts one, or the operation has ended and the write acts on it, or an answer does not cover exactly the pending interactions |
  | `invalid_request` | the request is malformed |
  | `unsupported` | the Agent did not advertise the capability |
  | `unavailable` | the Agent is not connected |

- **Read your writes.** The write is visible once the replica's `op.id` equals the
  returned ID. IDs are never reused, so this holds across bridge restarts.
- **Refused input stays in the client.** The prompt, answers or control change
  remain, for the user to submit again once the replica is in sync. A queued prompt
  or control change is submitted when the replica shows the operation ended,
  naming that operation.

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
- **No intent tables.** No idempotency key, intent table, tombstone or retention of
  results is needed.

## 7. Holding

- **Held while needed.** The bridge holds a session while a client subscribes to it
  or one of its operations runs (S6). It releases a session that needs neither, as
  an ordinary ACP client would close it. How long it waits first affects
  performance only.
- **No hand-over race.** A session released between `create` or `fork` and the
  client's subscription is loaded again by that subscription, and alignment (S3)
  continues its log.
- **Reopen continues.** A release and a later hold, an ACP reconnect, and a bridge
  restart all end in a `load` operation and a new alignment. If the replay gives
  the same messages, the client keeps its log.
- **Agent unavailable.** While the Agent is disconnected, a subscribed session's
  operation is a running `load` waiting for the connection; `global.agent` shows
  why.

## 8. Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/scopes/{scope}` | the document once, for clients that do not stream; `404` when not held |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | content-addressed; served while the session's `log` or `live` references the ID, else `404` |
| `GET /api/v2/catalog?cursor=&revision=` | the Agent's paged `session/list` |
| `GET /api/v2/sessions/{id}/files?query=`, `…/files/content?path=` | workspace context |

**Markdown export** is assembled by the client from the log and the attachment
read. There is no process to fetch and no export endpoint.

## 9. Extending

- **A new presentation layer** (a CLI, an IDE extension, a mobile client)
  implements the generic replica engine (log truncation and append, `prev` checks,
  `live` replace and patch, subscribing again) and its own view function.
- **A new operation** is a new `kind` with its arguments and result. Admission,
  responses and replication do not change.
- **New state** is a new field of `live`. The engine does not change.

## 10. What changes

### 10.1 Removed

- Session incarnations, owner-qualified scopes, `If-Match` owner tags,
  `end: replaced`, retired-incarnation tables, first-subscription leases.
- Epochs, `agent.predecessor`, log positions (`seq`, `Position`, `at`), the
  retained log, `Last-Event-ID` resume, per-scope `prev` positions, `reset` and
  snapshot fetch, the received, checkpoint and applied positions.
- `Idempotency-Key`, the intent table, tombstones, `resultExpired`, intent errors.
- `global.requests` and `agent_request` records, their retention and
  `DELETE /api/v2/requests/{id}`; turn outcomes and their retention.
- The compact history's process pages: `historyRevision`, `collapsedTurns`,
  `visibleRanges`, `GET …/process`. A completed turn's process is gone (S2).
- Open and close commands, and the per-command endpoints.
- In the client: owner handling, intent recovery, request-record waits, the watched
  process cache (`ObservedProcesses`), process-page export, and the v1
  compensating state.

### 10.2 Kept

- JSON Pointer patches for `live`; the generic replica engine's patcher and
  validation.
- Control reduction (protocol §3.3), terminals, URL flows keyed by interaction,
  content-addressed attachments.

### 10.3 Added

- The hash-chained log and `canon`.
- Alignment by log head, and one `log` event for alignment and commit.
- The operation slot, `expect`, and one write endpoint.
- Hold by subscription; delete as a global operation.

## 11. Acceptance

Each rule gets tests at the bridge first and then in the client engine:

1. The same final content, streamed in different chunks or replayed by `load`,
   gives the same entry and hash (S1).
2. A completed turn appends its entry, removes its process and ends its operation
   in one event. A failed turn appends nothing and keeps its prompt and error on
   `op` (S2).
3. A client with a matching head receives only the suffix. A client with a
   diverged or longer log receives the whole log and replaces its own. After a
   reopen with the same replay, the client keeps its log (S3, §7).
4. A broken stream, a slow subscriber and a replaced stream all end with the
   client equal to the bridge; events of a replaced stream change nothing (§5.2).
5. Interactions and controls change while the log hash stays the same, and the
   replica follows (S1, S4).
6. For each operation kind: a concurrent second write and a resend after a lost
   response get `stale`; a fork resend does not fork twice; a resent answer is
   refused; a resent cancel is harmless (S5).
7. An answer is refused after an interaction arrives or is cancelled (S5).
8. Interactions of `load`, `fork`, `create` and `set_config` are answered through
   the same path as a turn's (§4.3).
9. A session released before its first subscription is loaded by that subscription
   and continues its log (S6).
10. Attachments are readable while referenced by `log` or `live`, and stop being
    readable once only a released process referenced them (§8).
11. Delete of a held session ends its scope with `deleted`; a failed delete leaves
    its error on `global.op` (§6.1).

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
   is answered.
5. A completed turn's process disappears from every client, including the one that
   watched it run.

## 13. Migration

1. **Settle this proposal**, then rewrite [protocol.md](protocol.md) and
   [state-sync.md](state-sync.md) to match.
2. **Bridge:** the document model, `canon` and the hash chain, alignment and the
   event stream, operations and admission, hold by subscription. Bridge tests
   first (acceptance 1–11), following
   [bridge-state-machine-tdd.md](bridge-state-machine-tdd.md).
3. **Client:** the generic replica engine and the web projection on the new API.
   PR #34 is reworked onto it rather than landed: its replica engine and
   validation are reused; its process cache and compact-history projection are
   removed.
4. **Remove** the current v2 machinery listed in §10.1 and v1, including the
   remaining v1 reads (attachment links, Markdown export).
5. **Layer 1 clean-up**, separately: remove incarnation-based isolation from the
   bridge internals, now that no client-visible behavior depends on it.
