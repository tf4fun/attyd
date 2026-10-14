# Sync protocol implementation plan

Status: proposal, 2026-10-12. This document is how the bridge implements
[attyd protocol v2](protocol.md). The protocol document defines the client-facing
contract. This one covers the internal design, the invariants that tests must
enforce, and the migration from `/api/v1`. Lifecycle and concurrency changes still
follow the [TDD ledger](bridge-state-machine-tdd.md) and the
[runtime contract](active-turn-runtime.md).

## 1. Current delivery paths

Two mechanisms run side by side today.

| Path | Producer | Consumer | Revision |
| --- | --- | --- | --- |
| Runtime journal (`RuntimeSnapshot` / `RuntimeDelta`) | `SessionRegistry::commit_*` (`src/runtime_state.rs:2163`) via `flush_runtime` (`src/bridge.rs:1074`) | Hub `CanonicalProjection` (`src/server.rs:1330`). It is used only for the reset hint and the epoch. | per-session `live.revision`, global `seq` |
| Session business events | hand-built at each call site: `advance_session_delta` (`src/bridge.rs:508`), whole `bridge/session_view` values | `/api/v1/sessions/{id}/events`, observers of that session only | mirror `view_revision`, bumped by `touch` (`src/session_mirror.rs:638`) and mirror transitions |
| Global events | `Lifecycle` wire strings, filtered by `global_business_event` (`src/server.rs:1789`) | `/api/v1/events`, every client | only `catalog_changed.revision` |

The session view is assembled on demand by `session_view_value`
(`src/bridge.rs:412`). It merges the mirror's history and overlay with the live
runtime state.

The journal records every live-state change in one place. What observers actually
receive, though, is the business events, which are written separately at about twenty
call sites. Each call site has to remember to emit an event and pick the right one.
Defects follow from that:

| Symptom | Cause | Issue |
| --- | --- | --- |
| Usage, slash commands, title, and mode or config changes made by the Agent go stale | `update_control_state` (`src/runtime_state.rs:1045`) only reaches the journal | #16 |
| URL flow completion and cancellation never show | `complete_elicitation` (`src/bridge/agent_dispatch.rs:603`) only reaches the journal | #16 |
| Background sessions never show "Input needed" | no projection carries per-session interaction state to non-observers | #17 |
| Background "working" markers are stale, and miss turns started elsewhere | `busySessionIds` (`web/src/app.tsx:492`) reads browser-side `cachedSessions` | (new) |
| The browser keeps snapshot caches, a background event panel and request-scoped paths | compensating for state the server doesn't project | #18 |
| Payload shapes drift between Rust `json!` and handwritten TypeScript | no typed DTOs | #5 |

#8 is different. Early `session/new` updates are lost because the baseline never
contains them, so it is a state defect, not a delivery defect. It is independent of
this plan. The convergence gate (§4) covers it once it's fixed.

The bridge already derives the per-session activity that `SessionSummary.work`
needs. `SessionWork::of` (`src/session_observers.rs`) classifies each session as
`Idle`, `Running` or `AwaitingInteraction`, but today only to drive unobserved
retirement. It reads `SessionEntry.resources`, and the journal doesn't carry those.

## 2. Design

```text
ACP ──▶ bridge actor: transition ──▶ commit step ──▶ log (epoch, seq) ──▶ Hub
                                       │  project → diff → ops            │ cut, prev, coalesce,
                                       ▼                                  │ resume, position
                              published documents                        ▼
                              (per scope, last sent)              replicas (web UI, …)
```

### 2.1 Projection

Two pure functions define the documents of protocol §3:

- `project_global(&AgentState, catalog_revision, &SessionRegistry) -> GlobalState`
- `project_session(&SessionRegistry, session_id) -> Option<SessionState>`

Both read the session registry rather than a single `SessionEntry`, because the
history baseline and retained terminals live in registry-wide caches. `agent` is
owned by the connection task, so the caller supplies it. They live in
`src/protocol/project.rs`.

They are the only definition of what clients see. Snapshots (`GET /api/v2/global`,
`GET /api/v2/sessions/{id}`) call them directly. Privacy rules, such as releasing
finished process content and keeping private payloads out, live inside them.

### 2.2 Commit step

Every transition in the bridge actor ends with one commit step:

1. Project each scope.
2. Diff the result against the document last published for that scope, and emit
   ops for every difference (protocol §4).
3. If any ops exist, append one entry `{ scope, seq, prev, ops }` to the log, and
   store the new document as the published one.

The commit step runs when the bridge state lock's guard is released after mutable
access (`BridgeLock` in `src/bridge.rs`). A transition is one critical section, so
it commits exactly once, and no call site can skip the step (S1). The step visits
every held scope instead of a touched list; unchanged parts are skipped by
identity, so an untouched session costs a comparison of its small fields.

Length alone can't tell what changed in the large sequence fields, so diffing them
uses identity:

| Field | Changes | Diff |
| --- | --- | --- |
| `turn.updates` | growth; in-place folds (`tool_call_update` keeps its original slot; merged chunks; replaced plans); removal (`plan_removed`) | Elements are `Arc`-shared (`SharedTurnUpdates`), and a fold gives the changed element a new pointer. Compare published and current element by pointer: equal pointers mean unchanged. A changed element is diffed against its previous value, so a merged chunk is a text `append` and a fold is a `set` inside its index; a new tail is `append`; a shorter list is one `set` of the field. |
| `history.timeline` | a new baseline (turn commit, background update, reload) | The projection collapses completed turns, which renumbers the timeline, so source element identity does not carry over to its indices. The baseline's `Arc<HistorySnapshot>` (with the revision and the turn outcomes) says whether the history changed at all; only then is it projected again. Per-element digests of the projected timeline then find the changed indices: each is an index `set`, a new tail is `append`, and a shorter or mostly changed timeline is one `set` of the field. |
| terminal `output` | growth, plus front trimming when the tail exceeds its bound (`src/terminal.rs`, reader loop) | The published document keeps the output tail the replicas hold. The longest suffix of the published tail that prefixes the new one (found in linear time) gives `trim` + `append`. No counter is added to bridge state; where repeated output makes the cut ambiguous, every choice yields the same document. |
| maps (`interactions`, `urlFlows`, `terminals`, `outcomes`, `requests`, `sessions`) | upsert, remove | per key, by value |
| scalars and small structs (`controls`, `operation`, headers) | any | compared by value |

Comparing pointers is linear in the number of elements, with no content comparison.
`identity_diff_cost_on_long_timelines` streams into a turn of 2,000 updates on top
of a 2,000-entry history and checks that streaming never projects the history again
and that a chunk publishes only its growth. If profiling shows the pointer pass is
too costly, a per-field change list recorded by the transition can replace it. The
gate still compares against full projections, so neither strategy can hide a change.

The commit step emits whatever differs from the last published document, so a call
site can neither forget an event nor choose the wrong one. Mirror and live-state
transitions share the step and the log position. v1 delivery stays unchanged until
step 7, when `advance_session_delta`, `touch`, the hand-built `bridge/session_view`
values, and the paired `view_revision` and `live.revision` counters go away. A
scope's position is the `seq` of its last entry.

The published documents keep only what diffing needs, so publishing doesn't
duplicate conversation content:

- a `Weak` handle to the history baseline and to the active turn's prompt;
- the active turn's `SharedTurnUpdates`, shared with bridge state;
- one 16-byte digest per projected timeline element;
- the small fields, including terminal tails, collapsed-turn descriptors and outcomes.

The active turn's prompt stays out of the per-commit body as its updates do, so a
session whose turn did not change costs no content copies when another session
commits.

`published_documents_and_the_log_release_turn_content` and
`completed_process_content_is_released_from_the_retained_log` extend the
[memory retention](runtime-memory-retention.md) gates: once a turn ends and its
baseline is replaced, neither the published documents nor the log keep its
updates, prompt, process content or old baseline alive, and the log stays within
its byte budget.

Global summaries are part of `global`. When a session transition changes its
summary, the commit step therefore touches both `session/<id>` and `global`. They
get consecutive `seq`s in one actor turn.

### 2.3 Log and Hub

The log is a bounded in-memory suffix per epoch, accounted by bytes (8 MiB by
default, `DEFAULT_LOG_BUDGET` in `src/protocol/log.rs`). Compaction drops the oldest
entries but always keeps the newest. When a turn ends, its process content leaves
the documents (S6), so the log also drops every entry up to the session's previous
one: those entries carried that content, and a replica behind them resets. A
session scope that ends while its turn is active (close or retirement during a
turn) releases the same way. A replica asking to resume from before the
retained suffix gets a `reset` (protocol §4). An entry whose session was retired or
replaced by a new incarnation ends the scope; the next incarnation's first entry
has `prev = 0` and creates its document.

The Hub owns delivery only:

- **Cut.** It registers a subscription and captures each scope's start position
  atomically with the bridge actor. This generalizes `BridgeEvent::ObserverReady`.
- **Routing and continuity.** It sends only subscribed scopes and stamps each entry's
  per-scope `prev`.
- **Coalescing.** A lagging subscriber's queue for a scope collapses into one
  `reset`/`snapshot`. This generalizes `canonical_reset_event` (`src/server.rs:968`).
- **Heartbeats.** It sends coalesced `position` entries when the log advances
  through unsubscribed scopes.
- **Backpressure.** It keeps the existing per-subscriber byte accounting.

The Hub needs scope headers and the log suffix, not a replica of bridge state. The
runtime journal replica (`CanonicalProjection`) and its pipeline are removed once v1
is gone (§5, step 7).

Observation stays a bridge input. A stream that includes `session/<id>` holds an
observation lease (H09, H13). The bridge sees only the lease, never the stream.

Step 3 implements the Hub in `src/protocol/hub.rs`, and the routes in
`src/server/v2.rs`:

- **Cut.** `ReplicationSource::subscribe` (`src/bridge.rs`) takes the bridge lock,
  attaches an observation lease to each held owner the stream names, and registers
  the stream with the Hub at the log's current position. The commit step ends by
  handing the Hub its new suffix (`Hub::deliver`), under the same lock, so no entry
  falls between the cut and the stream.
- **Start of a scope.** `global` starts with an inline `snapshot`, a held owner with
  a `reset`, and any other owner with `end`. A `Last-Event-ID` in the current epoch
  whose suffix the log still holds resumes each scope from it, unless the suffix
  ends or creates the scope's document; that scope starts over.
- **`end` reasons.** The transition that retires an incarnation notes `closed`,
  `deleted` or `unobserved` (`retire_session_observation`), and its scope's `End`
  carries the reason. Without a note, the session's last published operation
  decides (`delete` is `deleted`, `close` is `closed`), so a delete that ends the
  scope before its retirement is noted still says `deleted`; otherwise the
  incarnation is `replaced` when the session is held again, else `gone`. Ended incarnations stay in a table of the
  last 1,024 per epoch (`RETIRED_OWNERS`), so a subscription gets its reason after
  the log compacted the `End`. An owner from another epoch is `replaced`.
- **Coalescing.** Each stream's queue is accounted by serialized bytes. Past
  `DEFAULT_SUBSCRIBER_BUDGET` (1 MiB), its queued data for each scope collapses into
  one `reset` (sessions) or `snapshot` (`global`) at the current position; queued
  `end` entries are kept, re-issued at that position. A suffix compacted within the
  commit that appended it starts every scope over the same way (see "Collapse").
- **Heartbeats.** After each commit, a stream whose last message is behind the log
  gets one `position`; a queued `position` is replaced, not stacked.
- **Atomic batches.** Each commit's messages, coalescing and heartbeat enter a
  stream's queue in one critical section. Messages that share a position carry the
  SSE `id` only on the last of them, so a reconnect between them resumes from the
  previous position and receives the whole position again; the replica ignores
  replayed entries it already installed.
- **Collapse.** A lagging queue, a queue holding undelivered entries of a scope
  whose process content the commit released (S6), and a suffix compacted within its
  own commit all collapse the queue the same way: every message in it is replaced
  by messages at the current position, a resync for each scope that had queued
  data and each queued `end` again. No SSE id in a collapsed queue can then pass
  data it dropped.
- **Epoch end.** The connection commits its terminal `agent` state, then ends every
  stream; a subscription that was waiting for the bridge lock gets only its cut, then
  ends. Its final `global` document stays readable with `GET /api/v2/global` until
  the next connection starts. v2 reads never start a connection.

Reads never materialize a session: `GET /api/v2/sessions/{id}` projects a held
session or returns `not_found`, and a stream only attaches leases to owners the
bridge already holds.

### 2.4 Commands

Command handlers run in the bridge actor as today. After the command's own
transition and commit step, the handler records the log position at that moment as
`at`, and the touched scopes as `scopes`. This holds even when the command commits
no change, such as an idempotent duplicate. Every accepted command commits its
`Request` record (or, for a turn, its operation) before the HTTP response. Later
progress and the final result arrive only through replication.

**Request records.** A `Request` in `global.requests` is created in the same actor
turn that accepts the command or sends the ACP request:

- every accepted operation command: open, create, fork, set_mode, set_config,
  close, delete, authenticate, logout, auth terminal start, and interaction
  responses;
- every ACP request the bridge sends on its own outside a held session, including
  the `session/list` behind a catalog read.

Control actions update their parent and create nothing: acknowledgements, turn
cancel, and auth terminal input, resize and cancel. Acknowledging a record therefore
never creates another record.

An interaction response record follows delivery, not removal. It ends `succeeded`
only after `ensure_interaction_delivered` (`src/bridge.rs:5548`) succeeds.
Otherwise it ends `failed` with `interaction_ended`, which keeps
`failed_interaction_delivery_is_never_reported_as_resolved`. `params` keeps the
distinguishing non-secret parameters (`modeId`, `configId`/`value`, `optionId`,
elicitation `action`), so two clients' submissions differ even when an earlier
response was lost.

A session `operation` started by a command points to its record (`requestId`), and
the record's final status and error stay after the operation clears. Synchronous
rejections (4xx) create nothing. Auth records keep `methodId` for their whole life,
which replaces today's browser-side `AuthTerminalState.methodId` (`web/src/lib/state.ts`).

An out-of-session record is keyed by the bridge's JSON-RPC request ID. A request-scoped elicitation
names that ID (`ElicitationRequestScope.request_id`), so the bridge attaches it to
the record directly. This replaces the connection-level `request_elicitations` and
`request_url_flows`, and the `acp/authenticated`, `acp/logged_out` and
`bridge/auth_terminal_*` events. A global URL flow keeps its `requestId` but outlives
the request (protocol §3.4).

**URL flow identity.** URL flows are keyed by the `interactionId` of the accepted
elicitation, not by the Agent's `elicitationId`. The Agent may legally reuse an
`elicitationId` once a flow ends (regression at `src/runtime_state.rs:6672`).
`elicitation/complete` resolves to the single *waiting* flow with that ID; the
existing `url_flow_in_use` check keeps that unique. Acknowledgement uses the
`interactionId`, so a late acknowledgement of flow A cannot remove a later flow B
that reused A's `elicitationId`.

**Intents.** Every operation command and turn start goes through one intent table per
epoch, keyed by `clientIntentId`. Each entry stores:

- the fingerprint `(operation, target, owner tag, body digest)`, where `target`
  includes every path parameter (`sessionId`, `configId`, `interactionId`,
  `operationId`, `requestId`);
- the record or operation ID it produced;
- that record's final status.

Equal bodies sent to different targets therefore conflict instead of aliasing:
config A versus config B, or two interactions given the same answer. Turns already
keep an intent table (H07), which gains the target fields. The table is independent
of public records, and outlives acknowledgement, retention and scope end. Entries are
never evicted within an epoch. Once its record is gone, an entry shrinks to a
fixed-size tombstone (key, 32-byte fingerprint hash, ID, final status), and the
resend answers `duplicate` with `resultExpired` (protocol §5.2). When the table's
byte budget is full, new intents are refused with `intent_capacity`, and known keys
are still answered. A resend after a lost response therefore never runs twice within
an epoch, which keeps H07 and H14 intact. Tombstones of roughly 100 bytes make a
budget of a few MiB cover tens of thousands of operations. Exhausting it means an
epoch has had an unusual number of commands, and failing closed is the intended
answer.

**First-subscription lease.** Open, create and fork grant a lease on the owner they
return, through the existing `ObservationLease` machinery. A stream subscription to
that owner takes the lease over; otherwise it expires after a grace period. The grace
period is independent of `--session-unobserved-timeout` (`src/options.rs:80`).
Without the lease, an unobserved timeout of `0` lets `SessionObservers` start
retirement as soon as a new session is Active and idle
(`src/session_observers.rs:161`), before its creator can subscribe. That race exists
in v1 today.

Step 4 implements the session commands in `src/bridge/commands.rs`, the record
book and intent table in `src/protocol/commands.rs`, and the routes in
`src/server/v2.rs`:

- **Admission.** One critical section of the bridge lock looks the intent up,
  checks the `If-Match` owner against the published owner, runs the command's own
  admission (capability, active session, mode and config references, a running
  record on the same target, `can_begin`), and commits the record or the turn's
  intent. Only then does the command enter the bridge input, as v1 commands do; its
  response finishes the record. A command refused after admission, for example by
  a race with a v1 client, finishes its record `failed`.
- **Records and intents.** The `CommandBook` lives in the session registry, so a
  new registry (a new epoch) starts empty. A session `operation` started by a
  command carries the record's ID as its operation ID, which is how
  `operation.requestId` is projected. Turn intents settle in the commit step:
  completed once the turn left an outcome, failed otherwise. Intents are accounted
  as their key, subject ID and a fixed overhead (4 MiB per epoch by default).
- **Open.** A held owner is returned at once in a succeeded record. Otherwise
  `reserve_attachment_locked` allocates the incarnation in the admitting critical
  section, the record's `target` names it while the load or resume runs, and the
  command enters the bridge input as a prepared attachment.
- **Leases.** `FirstLeases` (30 s) attaches an `ObservationLease` to the owner.
  `ReplicationSource::subscribe` takes it over when a stream attaches its own
  lease; otherwise its timer withdraws it through `UnobserveSession`. A fork grants
  it in the transition that makes the target held (`returned_owner_held`), before the
  history workflow and the v1 response.
- **Interaction responses** are refused synchronously (`conflict`) when the
  interaction is gone or another response to it is running; otherwise the record
  follows the v1 delivery and ends `failed` with `interaction_ended` if it was not
  delivered.
- **URL flows.** Settled session flows stay in their terminal status (the last 16
  per session) until `DELETE …/url-flows/{interactionId}` removes the one with that
  interaction ID. Global flows follow the same rule (step 5).

Step 5 adds the global commands and out-of-session records in the same files:

- **Out-of-session records.** The v1 handlers of `authenticate`, `logout`,
  `session/new`, `session/list` and a cold `session/delete` open the record before
  they send the ACP request (`out_of_session_request`). A v2 command's request is
  its own record; any other one, a v1 client's included, gets an `agent_request`
  record that finishes with the handler. A read's record (`session/list`) leaves
  as soon as its call ends.
- **Parents.** The ingress keeps every RPC by its JSON-RPC ID until its completion
  has passed the coordinator (`IngressSender::outstanding_owner`). The coordinator
  resolves a request-scoped elicitation's `requestId` when it routes the event, so
  the lookup follows wire order and a fast response can't race it. The record of
  the named request parents the elicitation, out of session or a session command
  such as `set_mode`. A named request-scoped elicitation lives while the RPC it
  names is outstanding, whether or not that request has a record: the first commit
  after the RPC's completion passed the coordinator cancels it, and the Agent gets
  `cancel` (`end_finished_request_children`). That is before anything the handler
  does after the response, such as a fork's optional history load. URL flows it
  started stay. An elicitation naming a request that is no longer outstanding, or
  whose record already finished, is answered `cancel` and never published. A
  running session request without a record (a prompt) shows the interaction in
  `global` without a parent; a `null` request scope is global and unparented.
- **Global URL flows** keep their parent's `requestId`, stay terminal (the last 16)
  until `DELETE /api/v2/url-flows/{interactionId}`, and are keyed by interaction.
- **Auth.** `POST /api/v2/agent/authenticate`, `…/logout` and `…/auth-terminals`
  check the offered method and capability in the admitting critical section. The
  auth terminal manager also delivers its events to the bridge in order
  (`auth_terminal_event`): output extends the record's bounded tail (64 KiB), and
  the exit finishes the record `succeeded`, `failed` or `cancelled`, keeping
  `methodId`; a signalled exit reports `signal` with a `null` `exitCode`. Input, resize and cancel are controls on a running record.
  `agent.authenticatedMethodId` follows authenticate and logout.
- **Create.** `POST /api/v2/sessions` is an operation command; the transition that
  makes the new session held finishes its record and grants the lease
  (`returned_owner_held`), as fork does.
- **Catalog.** `GET /api/v2/catalog` refuses a stale `revision` with
  `catalog_changed` before it sends anything, and maps the bridge's own
  pagination check to the same error.
- **Predecessor.** The hub notes why it ended a generation (`auth_restart` after a
  successful terminal login, `shutdown`); otherwise the ended epoch's final agent
  phase decides (`agent_error` or `agent_exit`). The next connection's source
  carries it into `agent.predecessor`. An `auth_restart` starts the next
  connection as soon as the old one finished, so a v2-only client sees the new
  epoch; reading an ended epoch's `global` still never restarts the Agent.

**Cold delete.** `If-Match: "<epoch>:0"` is checked against the registry in the same
actor turn that starts the request, so there is no window in which the session
becomes held unseen. It keeps the path tested by
`cold_session_deletion_uses_global_management_without_loading`
(`src/server.rs:4400`). Fork still requires a held source: it reads the source's
`cwd` from live state (`src/bridge.rs:4271`), and `ForkSessionRequest` requires a
`cwd`.

**Materialization.** Only the open command starts `load`/`resume` (protocol §4.2).
It calls `start_attachment`, which allocates the incarnation synchronously
(`src/runtime_state.rs:542`), so open can return the full owner in its record. Open
grants a lease with a grace period, so the session isn't retired before the
client's stream attaches. `GET /api/v2/sessions/{id}` answers from the held session
or returns `not_found`, so the GET loses its loading side effect.

**Subscriptions never materialize.** Every session subscription names
`(epoch, incarnation)`, so a transport retry is byte-for-byte the original request.
The bridge applies today's owner rule from `prepare_session_view_request_for_owner`
(`src/bridge.rs:1309`): a retired, foreign-epoch or foreign-incarnation owner gets
`end`, never a fresh load. Retired incarnations go into a bounded per-epoch table, so
`end.reason` stays accurate after log compaction. Incarnation numbers restart with
each registry (`src/session_registry.rs:415`), so the epoch in the tag is what
separates `S@1` in epoch E1 from `S@1` in E2.

### 2.5 Rejected alternative

**Project in the Hub from the runtime journal.** The journal covers live state, but
not mirror transitions (history phase, load, baseline commit) or
`SessionEntry.resources`, and `SessionWork` needs those. Completing the journal would
replicate all bridge state into the Hub and keep two pipelines. Projection belongs
next to the state.

## 3. Types

The protocol documents, entries, ops, command bodies, results and errors are typed
Rust DTOs from the start. TypeScript in `shared/` is generated from them (`ts-rs` or
`specta`), and a test fails when the generated output is stale. ACP types embedded
verbatim are referenced from `@agentclientprotocol/sdk`, not regenerated. This
absorbs #5 for v2. v1 shapes are never typed; they are deleted.

## 4. Invariants and gates

- **S1 single write path:** only the commit step appends to the log.
- **S2 convergence:** for every scope, `apply(snapshot@a, entries(a..b)) ==
  snapshot@b`.
- **S3 cut:** each scope's first entry plus its suffix has no gap, no duplicate, and
  no mix of epochs or incarnations (extends H10).
- **S4 coalescing safety:** replacing any queued suffix with the latest snapshot gives
  the same final document.
- **S5 observation independence:** `global` subscribers and reads never affect
  session lifecycle (extends H09, H13).
- **S6 privacy:** released process content and private payloads never enter a
  document, op or log entry.
- **S7 read your writes:** once a replica's *applied* position (protocol §4.1)
  reaches a command's `at`, its subscribed documents in `scopes` reflect that
  command, including while a reset is pending.
- **S8 rebuildability:** a replica that discards everything and re-reads public state
  recovers every answerable interaction, every running operation, and every finished
  result within retention (protocol §1, §3.4).

Two gates enforce these. Neither is enough alone.

### 4.1 Convergence gate (S2–S4, S7)

A Rust harness drives the bridge with a scripted Agent. After every step, it applies
the delivered entries to a replica and compares the result with fresh snapshots. It
repeats the scenario with forced coalescing and with reconnects at random cuts. It
also checks S7 by issuing commands while a scope is waiting for its reset snapshot.

A missed commit step shows up as a divergence, whatever the field. The harness lands
together with the commit step, in the same PR.

Step 2 implements it in `src/protocol/replica.rs`. Three replicas run side by side:
one follows every entry, one lags and replaces a random subset of each scope's
queue with the latest snapshot (S4), and one lags and reconnects, resuming from the
retained log or starting over from snapshots when compaction dropped its position
(S3). Scenarios run in two places:

- on the session registry (`src/protocol/publish_tests.rs`), for control updates,
  streamed and folded turn updates, plans and compactions, URL flow completion and
  cancellation in both scopes, a terminal rolling past its bound, and ended or
  replaced sessions;
- through the real coordinator and ACP transport with a scripted Agent
  (`src/bridge/convergence_tests.rs`), for control updates, a turn started by
  another client, a permission, a background `tool_call_update`, close, delete and
  unobserved retirement.

Step 3 runs the same gate over SSE. `src/protocol/stream_replica.rs` is a replica of
the stream protocol that follows protocol §4 and §4.1 strictly: every scope starts
pending, a `reset` waits for a fetched snapshot, covered entries are ignored before
`prev` is checked, and nothing follows `end`. The Hub scenarios
(`src/protocol/hub_tests.rs`) cover the cut, per-scope `prev`, heartbeats,
coalescing, resume, compaction and `end` reasons. Through the real routes
(`src/bridge/stream_tests.rs`, `src/bridge/stream_client.rs`), SSE replicas join
the coordinator scenario above and are compared at every step, and the step 3
acceptance scenarios run end to end: a page that reconnects after another page
closed the session (before its first event, with the close in the log, and after
compaction), an old-epoch owner, an HTTP snapshot that overtakes its SSE entries, a
coalesced slow subscriber, and `global` subscribers that never observe a session.

Both places mutate state without committing and assert that the harness reports
it. A replica that meets a continuity break or an op that does not apply fails the
gate; snapshots replace entries only at the first cut, a coalesced queue, a
reconnect, or a compacted position. The connection's end commits `agent.phase`
`stopped`, or `error` with the ACP error as `agent_error`, for every exit after the
epoch exists, including workspace setup failures. Step 4 adds the S7 checks over
SSE (`src/bridge/command_tests.rs`): a command that commits while its scope waits
for a coalesced reset never advances `applied`, and a new stream's `applied` waits
for every subscribed scope.

Convergence can't detect a projection that is missing something. If both the
snapshot and the ops leave out global URL flows, they still agree. That is what the
second gate is for.

### 4.2 ACP semantic coverage gate (S8)

For every row of the [coverage ledger](acp-coverage.md), the ledger gains the v2
chain:

```text
ACP capability → bridge authority → public projection / command → finished result
               → recovery after a fresh replica → lifecycle end
```

Each link names the document path or endpoint and the test that proves it. A link
that is "not applicable" says why: for example, filesystem requests are fulfilled
inside the bridge, so only their user-visible effects are projected. A capability
row is done when every link has a test.

These acceptance scenarios come first and run against v2 end to end. Each one
discards the replica at the critical point and rebuilds it from public state:

| Scenario | Proves |
| --- | --- |
| Global URL authentication: refresh after accept, then `elicitation/complete`, and separately cancellation | global `urlFlows` and their lifetime |
| URL flow A completes, the Agent reuses its `elicitationId` for flow B, and a late acknowledgement of A arrives | flow identity by `interactionId` |
| Page A disconnects, page B closes the session, A reconnects (with and without the close still in the log), including a disconnect before A's first event | subscriptions never materialize; `end` reasons |
| An old-epoch page re-subscribing `S@1` after the new epoch also holds `S@1` | the epoch in the owner tag |
| A new stream for `global` + `session/S`: `global` snapshot first, S's snapshot later, with a command on S committed in between | every scope starts pending |
| An HTTP snapshot at 20 overtakes SSE entries 11–20 for the same scope | covered entries are ignored, no second reset |
| Permission and form responses whose HTTP response is lost: delivered, answered by another client first, cancelled by the Agent before delivery | interaction response records |
| Two clients set different modes on one session; one is refused, and one response is lost | `clientIntentId` and `params` on command records |
| One key reused for `config/A` and `config/B` with the same `value`, and for two interactions given the same answer | `target` in the fingerprint |
| Lost response where the record was acknowledged by another page, retained out, or ended with its scope; and a lost response for a turn that is still running | recovery order and `resultExpired`; no second execution |
| A tiny intent budget: an operation succeeds, its response is lost, the record is acknowledged or retained out, the table reaches its budget, then the same key is resent; for create, fork and open | no second ACP dispatch; `resultExpired` from the tombstone; new keys refused with `intent_capacity` and visible as such |
| Create and fork with `--session-unobserved-timeout 0`, subscribing after a delay shorter than the grace period, with an Agent without `load`/`resume` | first-subscription lease |
| Acknowledging a finished request | control actions create no records |
| `set_mode` / `set_config` refused by the Agent, and close of a held session, each with the HTTP response dropped | session command records in `global.requests` |
| Auth terminal running, then failed, with two advertised methods, rebuilt from public state | `methodId` on the record |
| Terminal login succeeds and the Agent restarts | `agent.predecessor` and the epoch boundary |
| `POST /sessions` and fork with the HTTP response dropped; two pages creating in one directory; retry with the same key; one key reused for forks of A and B | intents, fingerprints and typed request results |
| Cold-session delete with an Agent that supports delete but not load or resume | the `"<epoch>:0"` precondition and the no-load path |
| Sparse controls: title then `updatedAt` only; usage with cost, then usage without cost; `null` clears; early commands and title before a response that sets modes | control reduction and its order (protocol §3.3) |
| An Agent that omits `configOptions` versus one that returns `[]` | `null` versus `[]` presence |
| Terminal output that rolls past its bound | the `trim` + `append` diff |
| A background `tool_call_update` folding into an earlier slot | index `set` on the timeline |
| A command completing while its scope waits for a coalesced reset snapshot (applied 10, commit 15, `reset@20`) | `applied` never advances on a reset |
| A request-scoped elicitation during `session/new` and during a catalog read | request records as parents |
| A background permission, and a turn started by another client | `global.sessions` summaries |
| Retirement, close, delete, reattach, epoch change | `end`, incarnation and epoch handling |
| Early `session/new` updates, once #8 is fixed | baseline seeding |

Browser tests check only the browser's half: applying a valid entry stream gives the
same replica as loading the final snapshot. These extend the existing fuzz tests.

## 5. Migration

v2 is built next to v1, and the web UI moves to it in one step, so the product keeps
working throughout. Each step is one PR, validated in CI.

| Step | Work | Absorbs |
| --- | --- | --- |
| 1 | v2 chain columns in the coverage ledger. Protocol DTOs with generated TypeScript. `project_global` and `project_session` with unit tests, including control reduction. | #5 |
| 2 | Commit step, log and identity-based diff; in-process convergence harness | — |
| 3 | `GET /api/v2/global`, `GET /api/v2/sessions/{id}` and `GET /api/v2/stream`: Hub cut, `prev`, coalescing, resume, `position`; owner-qualified session subscriptions that never materialize; every scope pending until installed; covered-entry filtering; retired-incarnation table | — |
| 4 | Session commands with `at`/`scopes`, `If-Match` tags and `ProtocolError`; the open command and the first-subscription lease for open, create and fork; intents for every operation command; interaction response records; the reads | — |
| 5 | Request records for every accepted command and out-of-session ACP request; global interactions and URL flows keyed by interaction; auth flows and the auth terminal; create and fork intents; cold delete; `agent.predecessor` | — |
| 6 | Web UI on v2, with a generic replica engine (JSON Pointer patcher plus validation, with received and applied positions) outside React; sidebar markers from `global.sessions` | #16, #17 (visible fix) |
| 7 | Remove v1: routes, business events, `advance_session_delta`, `touch`, `cachedSessions` and compensating browser state, and the journal replica if it has no other consumer | #18 |
| 8 | (optional) Path-subset profiles and an access-layer guide, when a second access layer is concrete | — |

Dependencies: 1 → 2 → 3, then 4 and 5 in either order, then 6 → 7. Each step adds
the acceptance scenarios from §4.2 that it makes reachable. #8 is independent and can
land at any time, in v1 or v2.

#16 and #17 stay visible until step 6. If either becomes urgent before then, a
targeted v1 fix is possible, but that work would be thrown away in step 7.

## 6. Open questions

- **Pointer-diff cost.** Step 2 measures it (§2.2): streaming never projects the
  history again, and a chunk publishes only its growth. Switch to
  transition-recorded change lists only if profiling shows the pointer pass.
- **Log retention budget.** Resolved in step 3. The log keeps 8 MiB per epoch; it
  bounds how far back a stream can resume. Each stream's queue coalesces past
  1 MiB, so a slow subscriber costs at most that plus one resync per scope, and
  never holds the log back. `end` reasons outlive the log in the retired-incarnation
  table. The protocol §3.4 minimums apply to records, which arrive in steps 4 and 5.
