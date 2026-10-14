# Protocol v2 design review (draft)

Status: working notes, 2026-10-14. Conclusion so far: the client API (v2) needs a
redesign as attyd's own client ↔ bridge API; the current v2 is an intermediate
state. These notes collect a design review that started
from the last v1 dependencies of the web UI (attachment links and the Markdown
export). They will be summarized into concrete changes to
[protocol.md](protocol.md) and [state-sync.md](state-sync.md). Nothing here is
implemented yet.

## Observation

The current design mixes two layers and tries to solve both with one mechanism,
the session incarnation (`(epoch, incarnation)` owner tags, owner-qualified
subscriptions, per-incarnation resources and late-result checks):

1. **bridge ↔ ACP server:** how the bridge holds an Agent session and attributes
   what the Agent sends.
2. **bridge ↔ clients:** how replicas stay in sync with the bridge and address
   its state.

The review treats them separately. Layer 1 is settled below; layer 2 is next.

## Layer 1: bridge ↔ ACP server

### What incarnations were meant to isolate

H10 (generation isolation): results of an earlier holding of the same session ID
(late RPC responses, local tasks, timers, responders) must not change a later
holding. ACP messages carry no local identity, so the bridge tags its own
asynchronous work with `(epoch, incarnation, operation/attempt)` and checks
ownership on completion.

Incarnations cannot identify ACP notifications at all: `session/update` and
Agent-initiated requests carry only the session ID (see
[session-isolation-proposal.md](session-isolation-proposal.md), "本地迟到结果").
So they only isolate the bridge's own bookkeeping.

### Simple model

- **Exclusive, persistent holding.** At any time the bridge holds at most one
  instance of a session ID. Without retirement within an epoch, `(epoch,
  sessionId)` already identifies the holding.
- **Closing and reopening a session is equivalent to closing and reopening an
  ACP client.** It is handled by two rules:
  1. **Close only when idle.** The daemon closes a session only once no RPC is
     outstanding in either direction for it: no running turn (a turn is cancelled
     with `session/cancel` and its `cancelled` response is awaited), no load or
     control request, no pending permission or elicitation.
  2. **Reopen rebuilds completely.** All in-memory state of the previous holding
     is discarded; `session/load` or `session/resume` rebuilds it from the
     Agent's data, which is the only authority. Whether an interrupted turn
     finished on the Agent is settled by that reload, never by redispatch (H14).
- **Responses** are claimed by their JSON-RPC request ID; a response whose ID is
  no longer outstanding is dropped.
- **Attribution of notifications** is by wire order (an update between a prompt
  and its response belongs to that turn). This is the one real ordering
  requirement of this layer, served by the ordered ingress, independent of
  instances.
- **Abnormal close** (crash, broken transport, an unresponsive Agent) is the only
  way to close with work outstanding. A broken connection ends the epoch, so old
  messages cannot reach the new connection, and rule 2 rebuilds. Whether late
  messages can still arrive otherwise is classified as **unknown** and gets no
  dedicated mechanism; the reload is the fallback.

### Conclusion

The bridge ↔ ACP layer needs no incarnation: idle-only close, full rebuild on
reopen, response claiming by JSON-RPC ID, and wire-order attribution are enough.
The incarnation-based isolation in the current implementation (late-result
ownership checks, per-incarnation resources and timers) is over-engineered for
this layer. Removing it is a large refactor (runtime state, session registry,
coordinator and their tests) and is not part of migration step 6; it will be
planned once both layers are settled.

## Layer 2: bridge ↔ clients

### Read-only model

Start from the smallest model: a client never writes to the bridge. Like a MySQL
replica, it only copies the bridge's in-memory state and presents it. Its single
goal is **to hold the same data as the leader** for the scopes it shows.

What such a replica needs:

1. **An ordered log.** Every bridge state change commits one entry at a
   monotonically increasing position (`seq`).
2. **Snapshot plus suffix.** A replica installs a snapshot at position `a` and
   applies the entries after `a`. Reconnecting with its position resumes the
   suffix.
3. **Log identity.** When the leader starts a new log (`seq` restarts), the
   replica must notice, drop its state and take a snapshot. This is the only
   necessary meaning of `epoch`.
4. **Resync on a break.** A gap, a compacted position, or a lagging queue that the
   leader replaces with a fresh snapshot all end in a snapshot.
5. **Partial replication.** A client follows `global` plus the session it shows,
   not all state.

**Opening and reconnecting are the same operation for the replica:** get the
scopes it shows consistent with the bridge's memory. A replica with no
continuous position takes a snapshot; one with a continuous position resumes.
It never distinguishes a first sync from a resync.

"Opening a session" in the UI is two things: a **write** (ask the bridge to hold
the session if it does not) and a **read** (subscribe to its state). The read half
is identical to reconnecting. Writes are discussed separately.

### Consequences

- **Subscriptions carry no expectation.** A replica asks for "the state of session
  S", not for a particular owner. The bridge serves whatever it holds now. No owner
  check on subscriptions, no `end: replaced`, no retired-incarnation table.
- **Every outcome is derived from state.** A closed or deleted session is a removed
  (or closed-marked) document; its reason, if needed, is a field. The UI decides
  from state whether to leave the session.
- **The replica holds no authoritative state.** Only pure UI state stays local
  (drafts, disclosure, scroll position). The v1 compensating browser state
  (`cachedSessions`, outcomes assembled from events, local URL flow state) goes,
  which step 7 planned anyway.
- **One synchronization path.** No dedicated "refresh after reconnect", "probe
  connection health" or "reopen after replaced" logic.
- **Incarnations are not part of the client protocol.** A session closed and held
  again is, in the totally ordered log, a removal followed by a creation; applying
  them in order is unambiguous.
- **`applied` and read-your-writes belong to the write model.** A read-only replica
  needs only its received position.

### Simplifications of the current design to evaluate

- Subscriptions become `scope=session/<id>`; the epoch identifies the log only and
  is not part of a scope.
- `end` only means "the leader no longer has this document"; reasons move into
  state.
- **`epoch` as the bridge process lifetime.** Following layer 1, an ACP
  reconnect can be an ordinary state change (agent phase, held sessions removed
  and loaded again) inside one continuous log, instead of a new epoch that forces
  every replica to rebuild.
- **Inline snapshots.** Today a session scope starts with `reset` and the replica
  fetches the snapshot over HTTP, which brings "an HTTP snapshot overtakes the
  stream", covered-entry filtering and per-scope pending. Snapshots sent inside the
  stream (as MySQL's initial sync uses the replication channel) would leave one
  ordered stream to follow. The reason for the separate fetch (stream size and the
  coalescing budget) needs weighing.
- **Per-scope `prev`.** If a change of subscribed scopes always opens a new stream
  that starts every scope from a snapshot, continuity could be checked on the
  stream's `seq` alone.

### Attachments and export

- **Attachments are read-only and content-addressed.** An attachment ID is the
  SHA-256 of its content block (`src/attachments.rs`, `id()`), so the same ID
  always means the same bytes, and an owner tag adds no correctness. The read only
  needs: never materialize (an unheld session answers `404`), and serve only
  attachments the held session's current history references. Proposal: drop the
  owner requirement from `GET /api/v2/sessions/{id}/attachments/{attachmentId}`,
  so links are plain `href`s; resolving references in a resent prompt follows the
  same rule.
- **Markdown export** can be assembled by the client: process pages addressed by
  `(historyRevision, turnId)` (a `410` mid-export aborts it), and attachments from
  the simplified read. No new protocol read.

### Write model

Three requirements:

1. **A write is a request to change the current state.** Only a client that has
   fully synchronized the bridge's state can make one: the write names the state it
   was decided on, and the bridge accepts it only if that state is still current.
2. **The bridge handles writes serially.** Accepting a write moves the state to a
   "waiting for the ACP server" state in the same step. With requirement 1 this
   settles concurrency between clients (the second write names a state that is
   gone) and makes a single request idempotent (a resend after a lost response
   names the same outdated state and is refused).
3. **ACP failures are the bridge's.** The bridge keeps a reliable ACP state; the
   client only projects the replicated state. A command's response says only
   *accepted* (with its position `at`) or *refused* (stale state, invalid request,
   not allowed in this state). ACP results and errors become state of the entity
   they affect: a turn outcome, a control's error, a load failure in history, the
   agent's authentication state.

#### Granularity

The version a write names is **the transition count of the bridge's session state
machine** (and of `global`'s), not ACP messages or chunks. Streaming content inside
a state (chunks, tool call updates, terminal output, title, usage) does not change
it. **Waiting for the user is itself a state.**

Draft session state machine:

| State | Meaning | Writes allowed | Next |
| --- | --- | --- | --- |
| `loading` | open in progress: waiting for load/resume | none | `idle`, or a load failure |
| `idle` | nothing in flight | turn start, set_mode/set_config, fork, close, delete | the matching waiting state |
| `running` | turn in flight | cancel | `awaiting_input`, `cancelling`, `idle` |
| `awaiting_input` | turn waits for the user's answer to its pending interactions | answer the group, cancel | `running`, `cancelling` |
| `cancelling` | cancel sent, waiting for the turn's response | none | `idle` |
| `controlling` | set_mode/set_config in flight | none | `idle` |
| `forking` | the source waits for the fork | none | `idle` |
| `closing` / `deleting` | close or delete in flight | none | removed |

`global` has its own machine (agent connection phase; authentication idle,
authenticating, terminal login running; create in progress). Request-scoped
interactions during a create put `global` in `awaiting_input`.

#### Decisions

1. **No controls during a turn.** Changing mode or config while a prompt runs has
   little value. The client keeps the change and submits it once the turn ends,
   as it already queues prompts. `running` allows only cancel.
2. **Pending interactions are answered as one group.** `awaiting_input` holds every
   pending permission and elicitation; one write answers all of them, so a group is
   never split across clients. An interaction that arrives, or that the Agent
   cancels, while a user composes an answer changes the state, and the answer is
   refused. The bridge then replies to each ACP request of the group.
3. **Terminal input and resize are not writes.** They go to a terminal the bridge
   runs (the auth terminal's PTY), not to the ACP state, so they take no version
   and stay outside the state machine.
4. **A refusal only happens under concurrency.** The client keeps the input (prompt,
   answers, control change) so the user can retry after it synchronized. No
   request ID in state, no intent table.

#### Carrying the version

The client ↔ bridge API is attyd's own, not a projection of the ACP API, so it has
no reason to borrow HTTP precondition headers. **The checkpoint goes in the
request body** (for example `{ "checkpoint": 42, "prompt": [...] }`); `If-Match`
is dropped. Reads, attachments included, never carry a version: they are
read-only and attachments are content-addressed.

#### What this replaces

- Owner tags and the `historyRevision` CAS: one state version per machine.
- `Idempotency-Key`, the intent table, tombstones, `resultExpired`,
  `intent_capacity`: the version CAS makes a resend harmless.
- `global.requests` and its retention and acknowledgement: results live on the
  entities they affect.
- Read-your-writes stays: a command returns `at`, and the effect shows once the
  replica received `at`.
