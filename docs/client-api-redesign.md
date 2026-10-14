# Client API redesign

Status: proposal for review, 2026-10-14. It summarizes the design review recorded
in [protocol-v2-review.md](protocol-v2-review.md) into the changes it implies for
[protocol.md](protocol.md) (v2), [state-sync.md](state-sync.md) and the
implementation. Nothing here is implemented yet.

## 1. Why

The current v2 design mixes two layers and solves both with one mechanism, the
session incarnation:

1. **bridge ↔ ACP server:** how the bridge holds Agent sessions and attributes
   what the Agent sends;
2. **bridge ↔ clients:** how clients replicate the bridge's state and change it.

Incarnations reach all the way into the client protocol: owner-qualified scopes,
`If-Match` owner tags, `end: replaced`, retired-incarnation tables, first-subscription
leases. On top of that, writes carry a second set of machinery for lost responses:
`Idempotency-Key`, an intent table with tombstones and a capacity, and
`global.requests` records with retention and acknowledgement.

This proposal separates the layers. Each gets the smallest model that is correct
for it:

- **Layer 1** needs no incarnation: the bridge holds a session exclusively, closes
  it only when idle, and rebuilds it completely when it opens it again.
- **Layer 2** is a read-only replica of the bridge's memory, plus writes that are
  compare-and-set requests against the bridge's state machines.

The client API is attyd's own client ↔ bridge API, not a projection of ACP. It is
redesigned accordingly; the current v2 becomes an intermediate state.

## 2. Layer 1: bridge ↔ ACP server

### 2.1 Holding a session

- **Exclusive.** At any time the bridge holds at most one instance of a session
  ID.
- **Close only when idle.** The bridge closes a session (on request, on delete, or
  by unobserved retirement) only once no RPC is outstanding for it in either
  direction: no running turn, no load or control request, no pending permission or
  elicitation. A running turn is first cancelled with `session/cancel`, and its
  `cancelled` response is awaited. Unobserved retirement already only fires for an
  idle session, so it follows this rule.
- **Reopen rebuilds completely.** All in-memory state of the previous holding is
  discarded. `session/load` or `session/resume` rebuilds it from the Agent's data,
  which is the only authority. Whether an interrupted turn finished on the Agent is
  settled by that reload, never by redispatch (H14).

Closing and reopening a session is therefore equivalent to closing and reopening an
ACP client.

### 2.2 Attributing what the Agent sends

- **Responses** are claimed by their JSON-RPC request ID. A response whose ID is no
  longer outstanding is dropped.
- **Notifications** (`session/update`, and Agent-initiated requests) carry only a
  session ID. They are attributed by wire order: an update between a prompt and its
  response belongs to that turn. This is the one ordering requirement of this
  layer, and the ordered ingress serves it. Incarnations never could identify
  notifications.

### 2.3 Abnormal close

A crash, a broken transport or an unresponsive Agent is the only way a session ends
with work outstanding. A broken transport ends the ACP connection, so old messages
cannot reach a new connection, and §2.1 rebuilds. Whether late messages can arrive
in any other abnormal case is classified as **unknown**: it gets no dedicated
mechanism, and the reload is the fallback.

### 2.4 Consequences

- Layer 1 needs no session incarnation. The session ID identifies the holding.
- An ACP reconnect is an ordinary state change for clients (the agent's phase
  changes; held sessions disappear and are loaded again), not something clients
  must detect.
- The incarnation-based isolation in the implementation (late-result ownership
  checks, per-incarnation resources, timers and retired tables) is no longer needed
  by the protocol. Removing it from `runtime_state`, `session_registry`, the
  coordinator and their tests is a separate refactor (§7, step 4).

## 3. Layer 2: reading (the replica)

### 3.1 Model

A client is a read-only replica of the bridge's memory, like a MySQL replica. Its
only goal is to hold the same data as the leader for the scopes it shows.

- **Log.** Every state change commits one entry at a position `seq` that increases
  by one per entry. There is no `epoch`: clients care only about being in sync with
  the bridge, not about which run of the bridge or which ACP connection produced the
  state.
- **Scopes.** `global`, and `session/<sessionId>` for a held session. Scopes are
  addressed by session ID only; there is no owner or incarnation.
- **Stream.** `GET /api/v2/stream?scope=global&scope=session/<id>` delivers, for
  each scope, a `snapshot` first and then `ops`, in log order, on one stream.
  Snapshots are sent inside the stream; there is no separate `reset` plus HTTP
  fetch.
- **Continuity.** A stream carries a fixed set of scopes. Changing the set opens a
  new stream, which starts every scope with a snapshot. Continuity is checked on
  the stream's `seq` alone; no per-scope `prev`.
- **Resume.** The SSE `id` is an opaque cursor that the bridge issued. `Last-Event-ID`
  resumes the stream's suffix when the bridge still holds that log position;
  otherwise every scope starts with a snapshot. The cursor is valid only for the log
  that issued it, so a restarted bridge never mistakes an old cursor for one of its
  own; clients never parse it.
- **Compaction.** Like MySQL log compaction, the bridge may drop the intermediate
  entries of a slow subscriber's queue (chunks, folded tool call updates, terminal
  output steps) and send the consolidated data instead: one snapshot of the scope at
  the latest position. Replicas never depend on seeing intermediate states.
- **End.** When the bridge stops holding a session (or never held it), the scope
  gets `end`. The entry may carry an informational reason (`closed`, `deleted`,
  `retired`, `not_held`) for presentation; the replica's behavior does not depend
  on it. After `end` the replica drops the document.

### 3.2 Opening and reconnecting

For the replica, opening a session and reconnecting are the same operation: get
the scopes it shows consistent with the bridge's memory. A replica without a
continuous position takes snapshots; one with a continuous position resumes. It
never distinguishes a first sync from a resync.

Opening a session in the UI is a write (hold the session, §4.4) followed by this
read. The read never loads a session: subscribing to a session the bridge does not
hold yields `end`.

### 3.3 Consequences

- Subscriptions carry no expectation. The bridge serves whatever it holds now.
- Every outcome is derived from state: a closed or deleted session is an ended
  scope; what the UI does about it follows from state.
- The replica holds no authoritative state. Only UI state stays local: drafts,
  queued prompts and control changes (§4.3), disclosure, scroll position.
- One synchronization path: no "refresh after reconnect", health probes, or "reopen
  after replaced".

### 3.4 Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/global`, `GET /api/v2/sessions/{id}` | snapshots, for clients that do not stream; `404` when not held |
| `GET /api/v2/sessions/{id}/history/{historyRevision}/turns/{turnId}/process?offset=` | immutable process page; `410` once the revision is gone |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | no version: attachments are content-addressed (the ID is the SHA-256 of the content block). Served only from a held session whose current history references the ID, else `404`. Links are plain `href`s. |
| `GET /api/v2/catalog?cursor=&revision=` | the Agent's paged `session/list` |
| `GET /api/v2/sessions/{id}/files?query=`, `…/files/content?path=` | workspace context |

**Markdown export** is assembled by the client from the session document, the
process pages of its turns (all of one `historyRevision`; a `410` mid-export aborts
it) and the attachment read. There is no export endpoint.

## 4. Layer 2: writing

### 4.1 Requirements

1. **A write is a request to change the current state.** It names the state it was
   decided on, and the bridge accepts it only if that state is still current. Only
   a client that has synchronized the bridge's state can make one.
2. **The bridge handles writes serially.** Accepting a write moves the state to a
   waiting state in the same step. With 1, this settles concurrency between
   clients (the second write names a state that is gone) and makes a single request
   idempotent (a resend after a lost response names the same outdated state and is
   refused).
3. **ACP failures are the bridge's.** The bridge keeps a reliable ACP state; the
   client only projects state. ACP results and errors become state of the entity
   they affect.

### 4.2 Checkpoints

The state a write names is a **checkpoint**: the transition count of a bridge
state machine.

- Every session document has `checkpoint`; `global` has its own.
- It changes on every state-machine transition, and only then. Content inside a
  state (chunks, tool call updates, terminal output, title, usage, commands) does
  not change it.
- **Waiting for the user is itself a state.**

### 4.3 Session state machine

| State | Meaning | Writes allowed | Next |
| --- | --- | --- | --- |
| `loading` | being held: waiting for load/resume | none | `idle`, or the session ends with its load error |
| `idle` | nothing in flight | turn start, set_mode, set_config, fork, close, delete | the matching waiting state |
| `running` | turn in flight | cancel | `awaiting_input`, `cancelling`, `idle` |
| `awaiting_input` | the turn waits for answers to its pending interactions | answer the group, cancel | `running`, `cancelling` |
| `cancelling` | cancel sent, waiting for the turn's response | none | `idle` |
| `controlling` | set_mode or set_config in flight | none | `idle` |
| `forking` | the source waits for the fork | none | `idle` |
| `closing`, `deleting` | close or delete in flight | none | ended |

- **No controls during a turn.** `running` allows only cancel. A mode or config
  change made during a turn stays in the client and is submitted after the turn,
  like a queued prompt.
- **Interactions are answered as one group.** `awaiting_input` holds every pending
  permission and elicitation, and one write answers all of them, so a group is
  never split across clients. An interaction that arrives, or that the Agent
  cancels, while the user composes changes the checkpoint, and the answer is
  refused. The bridge then replies to each ACP request of the group. A URL
  elicitation the user accepted becomes a URL flow in the session's state.
- **Terminal input and resize are not writes.** They go to a terminal the bridge
  runs, not to ACP state, and take no checkpoint.

### 4.4 Global state machine

`global` carries the agent connection (`starting`, `initializing`, `ready`,
`stopped`, `error`), authentication (idle, authenticating, logging out, terminal
login running) and session creation. Global writes (open, create, authenticate,
logout, start a terminal login) name `global.checkpoint`.

- **Writes that bring a session into existence answer with it.** Open, create and
  fork end in a held session or in a failure the client cannot get past, so their
  response waits for that end:
  - **Open** (`{ checkpoint, sessionId, cwd }`): asks the bridge to hold a session.
    It answers with the held session at `idle` (`{ sessionId, checkpoint, at }`), or
    with the load error. A load failure is the bridge's problem: the bridge cannot go
    on, so neither can the client, and the error is returned explicitly rather than
    kept as state. A session that is already held needs no write: the client
    subscribes directly.
  - **Create** (`{ checkpoint, cwd }`): answers with the new session at `idle`, or
    the `session/new` error.
  - **Fork** is an input on the source session, like a prompt: it names the source's
    checkpoint, moves the source to `forking`, and answers with the new session at
    `idle` (or the error) while the source returns to `idle`.
- Request-scoped interactions during a create put `global` in `awaiting_input`,
  answered as a group like a session's.
- **Terminal login** input and resize are not writes (§4.3).

### 4.5 Commands

```text
POST /api/v2/…   { "checkpoint": 42, …body }
```

- The checkpoint goes in the request body. `If-Match` is not used: this is
  attyd's own API, with no reason to borrow HTTP precondition headers.
- **Accepted:** `{ at }`. The effect shows once the replica received `at` (read your
  writes). Open, create and fork answer with the session they produced (§4.4).
- **Refused:** `{ error: { code, message } }` with `stale` (the checkpoint is not
  current), `invalid_request`, `not_allowed` (the state does not allow this write),
  `unsupported` (the Agent did not advertise it) or `unavailable`.
- Other ACP results never come back in a response; they are state (§4.6).
- A refusal only happens under concurrency. The client keeps the user's input
  (prompt, answers, control change) so the user can retry once synchronized. There
  is no request ID in state and no intent table.

### 4.6 Results as state

| Write | Where its result lives |
| --- | --- |
| turn start | `turn` while it runs; `outcomes` (completed, or failed with the ACP error and the exact prompt) |
| set_mode, set_config | `controls`; a refusal as the control's error |
| open, create, fork | the response: the session at `idle`, or the error (§4.4) |
| close, delete | the scope's `end` |
| answer interactions | the group leaves state; a URL flow appears for an accepted URL elicitation |
| authenticate, logout, terminal login | `agent` authentication state, terminal output and exit |

**No retention beyond the current state.** attyd persists nothing; every durable
fact lives on the ACP server. The bridge keeps only the state that matches what the
Agent would return on `load`, plus what is in flight. Results are not retained for
replicas that come later: a turn outcome, for example, is part of the history the
Agent returns, and nothing else outlives the state it belongs to.

## 5. What changes

### 5.1 Removed

- Session incarnations in the client protocol: owner-qualified scopes,
  `session/<id>@<epoch>:<incarnation>`, `end: replaced`, the retired-incarnation
  table, first-subscription leases.
- `If-Match` owner tags and the `historyRevision` CAS on turn start: replaced by
  checkpoints.
- `Idempotency-Key`, the intent table, tombstones, `resultExpired`,
  `intent_capacity`, `intent_conflict`.
- `global.requests`, `agent_request` records, their retention and
  `DELETE /api/v2/requests/{id}`; retention bounds for results beyond the current
  state (protocol §3.4).
- `epoch` in positions and scopes, `agent.predecessor`, and epoch handling in
  clients.
- Per-scope `prev`, `reset` plus HTTP snapshot fetch, covered-entry filtering, and
  the `applied` position with pending scopes: one stream with inline snapshots needs
  only the received position.
- In the client: owner handling, intent recovery, `end: replaced` reopening, request
  record waits, the auth-request projection, and the v1 compensating state.

### 5.2 Kept

- The log, snapshots and JSON Pointer ops; the generic replica engine's patcher and
  validation.
- `global` and session documents, control reduction (protocol §3.3), compact
  history with immutable process pages, terminals, URL flows keyed by interaction.
- The client cache of watched turns' processes keyed by `(historyRevision, turnId)`.
- Layer 1's ordered ingress, response claiming and wire-order attribution.

### 5.3 Added

- `checkpoint` on `global` and on every session document, and the two state
  machines of §4.3 and §4.4.
- Group answers for interactions.
- Idle-only close for every path (request, delete, retirement), with cancel and
  wait for a running turn.
- Inline snapshots on the stream, compaction of slow queues into consolidated
  snapshots, and an opaque resume cursor; `end` with an informational reason.
- Attachment reads without a version.

## 6. Decisions from review

1. **Create and fork results.** Create answers with the new session at its `idle`
   checkpoint. Fork is an input on the source session, like a prompt, and answers
   the same way with the new session (§4.4).
2. **Slow subscribers.** Coalescing is log compaction: drop intermediate entries
   (chunks and other steps) and send the consolidated data (§3.1).
3. **Load failure.** A bridge-level failure the client cannot get past; the open
   answers with the error explicitly (§4.4).
4. **Retained results.** Almost none: nothing is persisted, so the bridge's state
   matches what the Agent returns on `load` (§4.6).
5. **Epoch.** No meaning for clients, which care only about being in sync with the
   bridge; removed. The resume cursor is opaque and bound to the log that issued it
   (§3.1).

## 7. Migration

1. **Review and settle this proposal**, then rewrite [protocol.md](protocol.md) and
   [state-sync.md](state-sync.md) to match.
2. **PR #34** (web UI on the current v2) either lands as an intermediate step, which
   fixes #16 and #17 now and keeps the replica engine and projection that §5.2
   keeps, or stays open and is reworked onto the new API. To be decided in review.
3. **Bridge API:** checkpoints and the two state machines, group answers,
   idle-only close, inline snapshots, the new scope naming and `end`, attachment
   reads without a version; then remove intents, records, owner tags and
   incarnations from the client protocol. The web client follows in the same step.
4. **Layer 1 refactor:** remove incarnation-based isolation and per-epoch
   replication sources from the bridge internals; one log for the bridge's
   lifetime.
5. **Remove v1** (step 7 of the current plan), including the remaining v1 reads.
