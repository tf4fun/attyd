# Sync protocol implementation plan

Status: implemented through step 8, 2026-10-15; step 9 is in progress (§5). This document is how
the bridge implements
[attyd protocol v2](protocol.md), settled in
[client-api-redesign.md](client-api-redesign.md). It covers the internal design,
the invariants tests enforce, and the migration from the earlier v2 draft and
`/api/v1`.
Lifecycle and concurrency changes follow the
[TDD ledger](bridge-state-machine-tdd.md): bridge behavior is tested before browser
projections.

## 1. Starting point

This section records where the migration started, on 2026-10-14. An earlier v2 draft was implemented on `main` (its migration steps 1–5, last at
`11d04d0`) until step 2 removed it. It was never completed: no client used it, and
the web UI then ran on `/api/v1`. It was also
over-designed for the settled model (incarnation owner tags, epochs and log
positions, a retained log with resume, request records, intent tables, process
pages). It is therefore **removed first** (§5, step 2), not kept beside the new
protocol, which takes its place under `/api/v2`.

Three generic parts implement rules the new protocol keeps unchanged. Step 2 keeps
them, with their tests:

| Part | Path | Rule it implements |
| --- | --- | --- |
| Control reduction | `src/protocol/controls.rs` | protocol §4.5 |
| JSON Pointer patcher and generic diff | `src/protocol/diff.rs` | protocol §5.3 |
| DTO-to-TypeScript generation | `src/protocol/typescript.rs` | §3 |

Two more techniques of the draft were reused in the new code, while the draft's own
modules went with it in step 2: the identity diff of turn updates (now in
`src/protocol/publish.rs`, for `op.process` in §2.3) and byte-budgeted subscriber
queues (now in `src/protocol/hub.rs`, §2.4). Nothing else of the draft was a
constraint: its routes, documents, records, intents, log, owner handling and tests
went.

Layer 1 (bridge ↔ ACP server) is not changed by this plan until its last step (9,
still open): the bridge keeps its sessions, incarnations, ingress and coordinator
internally. The protocol never exposes them.

## 2. Design

```text
ACP ──▶ bridge actor: transition ──▶ commit step ──────────────▶ Hub ──▶ replicas
                                       │ log: canon, hash chain     │ cut, queues,
                                       │ live: project → diff → ops │ coalescing
                                       ▼                            │
                              published documents (per scope)  ◀────┘ alignment reads them
```

### 2.1 Bridge state added

- **Operation slots.** Every scope has an `OpSlot { id, kind, status, input,
  cancel_requested, result, error, attachments, completion }` in bridge state
  (`src/protocol/op.rs`); `attachments` are the blocks a prompt input references,
  and `completion` is the held response of a `create`, `fork` or `list`. A held session keeps it in
  its registry entry; `global` and `catalog` keep theirs in the registry. The
  operation's pending interactions are the scope's pending Agent requests, which
  the bridge keeps as today; the projection reads them from there.
- **Occupation.** An `occupied: HashMap<Scope, OpId>` records scopes an operation
  occupies besides its own (delete occupies `session/<id>`). A held occupied scope
  shows the occupying operation as its `op`.
- **Turn log.** A held session keeps `TurnLog { entries: Vec<Arc<LogEntry>>,
  attachments }`, appended by commits and rebuilt from a load's replay (§2.3).
- **Holding need.** A session's need is computed, not stored: subscribers, occupation,
  or live resources (protocol S6).

### 2.2 Projection

Pure functions define the published documents:

- `project_session(&SessionRegistry, session_id: &str) -> Option<SessionDocument>`
- `project_global(&SessionRegistry, &AgentInfo) -> GlobalLive`
- `project_catalog(&SessionRegistry, revision: u64) -> CatalogLive`

`SessionDocument` hands the commit step what it publishes separately: the `log`
(`Unknown` until the first load completes, else `Known`), the `live` document, the
running prompt's `process`, and the attachments its `op.input` references. They
live in `src/protocol/project.rs` and are the only definition of what clients
see. `op.process` comes from the running turn's `SharedTurnUpdates`; it is absent
once the turn committed, so released process content never enters a document.

### 2.3 Commit step

Every transition in the bridge actor ends with one commit step, run when the bridge
lock's mutable guard is released (as in the earlier draft):

1. **Log.** For each held session, compare its `TurnLog` with the published one.
   - unchanged (the same `Arc`): nothing;
   - appended: a `LogChange { from: old length, prev, entries: new tail }`;
   - rebuilt (a load replaced it): `from` is the longest common prefix of the two
     chains, found by comparing hashes.
2. **Live.** Project, then diff against the published `live` with the identity
   diff (pointer comparison for `op.process`, value comparison for small fields,
   `trim` plus `append` for terminal tails).
3. **Publish.** Emit one event per changed scope. A log change and the same scope's
   live patch are one event, so a commit carries both (protocol S2). Store the new
   published documents and hand the events to the Hub under the same lock.

**`canon` and hashing** live in `src/protocol/canon.rs`:

- `reduce_messages(&[SessionUpdate]) -> Vec<Message>` applies protocol §3.2 step 1;
- `turns(messages) -> Vec<TurnContent>` applies step 2;
- `normalize(&[ContentBlock], &mut Attachments) -> Vec<Block>` applies step 3,
  where `Block` is the protocol's normalized content (text, resource links and
  `AttachmentRef`), a bridge DTO rather than an ACP type; it keeps each referenced
  block in the attachment table;
- `resolve(&[PromptBlock], &Attachments) -> Result<Vec<ContentBlock>, UnknownAttachment>`
  turns a prompt's references back into the ACP blocks they name, before
  `session/prompt`;
- `chain(&ChainHash, &Turn) -> ChainHash` hashes the JCS serialization
  (`canonical_json`).

`TurnLog::commit(prompt, updates)` builds a live turn from the prompt sent in
`session/prompt` and the Agent messages among the turn's updates;
`TurnLog::from_replay(updates)` builds every turn from a replay. Both go through
the same functions, so the same messages give the same entry (acceptance 1).

Attachment references in entries point at blocks the session keeps in an attachment
table keyed by ID, counted by references from the log and from `live`. A block whose
count reaches zero is released (protocol §8).

### 2.4 Hub

The Hub owns delivery only:

- **Cut.** A subscription is registered under the bridge lock. For each scope the
  Hub builds the alignment event from the published documents: the log from the
  client's head if `have` matches, else from `0`, with `live: { replace }`. Events
  of later commits enter the subscriber's queue in the same critical section that
  produced them, so nothing falls between the cut and the stream (protocol S3).
- **Baseline and queued tail.** For each session scope the Hub keeps two log
  positions per subscription:
  - the **baseline**: the head the client declared, advanced to an event's target
    log position only when that event is taken from the replaceable queue and
    handed to the stream's ordered output. A change still in the queue never
    advances it, since coalescing may drop it;
  - the **queued tail**: the target log position of the last event queued for the
    scope, or the baseline when nothing is queued.

  An ordinary log change is computed when it is queued, from the queued tail, so
  each queued change follows the one before it; the queue is never pre-built from
  a baseline that has not advanced yet. A resync is computed from the baseline. No
  client acknowledgement, sequence number or retained log is needed; the
  per-event convergence gate (§4.1) checks both positions.
- **Coalescing.** A queue past its byte budget (1 MiB, as in the earlier draft) is
  collapsed per scope: queued events of a scope that still exists are replaced by
  one alignment from the baseline, whose target becomes the queued tail; a queued
  `end` is kept, and nothing of its scope follows it.
- **Heartbeats.** An SSE comment on a stream idle for 15 seconds.
- **Holding.** A subscription to a session scope counts as one subscriber of that
  session while the stream is open. Subscribing to an unheld session submits a
  `load` to admission (§2.5); the stream waits for it under occupation.

- **Across connections.** The published documents and the Hub belong to the host
  and outlive one Agent connection, so a stream stays open across it. While the
  Agent initializes, a new subscription is aligned from the published cut and its
  loads wait for readiness. One cancellable recovery task starts the next
  connection after the previous one's final commit, when an open stream follows a
  session scope that has not ended, or once after a successful terminal login. It
  decides the need again when it starts and backs off while attempts never become
  ready. A global or catalog stream alone, and a read, start no connection.

No log is retained: a client that reconnects is aligned from the published
documents.

### 2.5 Operations

Operation writes are handled in the bridge actor, one at a time:

1. **Admission** (protocol §6.4), in one critical section: compare `expect` with the
   scope's `OpSlot`, check its status and the occupied scopes, then the capability,
   arguments and Agent connection.
2. **Install.** A starting operation replaces the scope's slot and occupies its
   other scopes, in the same critical section. The commit step publishes it.
3. **Run.** `bridge::operations` turns the operation into the bridge command of its kind,
   which the coordinator runs over ACP. Interactions
   of its ACP requests are added to its slot; answers are delivered by the existing
   interaction delivery path.
4. **End.** The transition that ends it sets its final status, result or error,
   frees its occupation, and wakes waiting subscriptions and loads.

`create`, `fork` and `list` keep their HTTP request open on a completion channel
that the ending transition fulfils. A dropped connection only drops the channel.

**Agent requests.** The coordinator routes each Agent request to a scope as today:
by session, or for a request-scoped elicitation by the JSON-RPC ID it names
(`ElicitationRequestScope.request_id`), resolved in wire order. The request joins
the scope's running operation, its own or the one occupying it, or starts an
`agent_request` operation when none runs.

**Cancel.** A cancel is admitted only for `prompt`, `terminal_login` and
`agent_request`. It sets `cancel_requested` and starts the kind's cancellation;
the operation keeps its occupation until the transition that actually ends it.

**Controls.** A successful `set_mode` or `set_config` folds its acknowledgement
into `controls` in the transition that ends the operation, as the bridge does today
(`complete_control_operation`): the requested mode, or the response's
`configOptions`. A failure folds nothing.

**Loads.** A subscription to an unheld session is admitted like a write without
`expect`: it waits while the session is occupied, then installs a running `load`
with the given `cwd`. Until the first load completes, the session's `TurnLog` is
`Unknown`, distinct from empty: alignments carry `live` only and leave each
subscriber's baseline where the client declared it. The load reuses the existing
`session/load` / `session/resume` attachment path and ends by building the
`TurnLog` from the replay; the transition publishes, per subscriber, the log from
its baseline together with the patch that ends the load. A reload of a held session
(after an Agent restart) keeps its known log until the replay replaces it.

**Where step 6 lands.** `POST /api/v2/scopes/{scope}/ops` (`server::write_scope`)
reads the body field by field and sends it to the actor as one input.
`protocol::admission` checks it on the published documents in admission order;
`bridge::operations` installs a starting operation and builds the command of its
kind under the same guard, or applies `cancel` and `answer` there. An `answer` is
checked whole before anything is delivered. Each interaction is found by its ID
wherever it waits, and is resolved and delivered to its pending responder in the
same guard (`resolve_permission`, `resolve_elicitation`). An accepted URL
elicitation becomes a flow in that step. A cancel sets `cancel_requested` once on the owning slot. A prompt or login
whose ACP call has not started yet sees the flag when it starts.

**Release.** A session whose need ends starts the existing retirement timer
(`--session-unobserved-timeout`), now only a performance setting. When it fires and
the session is still not needed, the bridge closes it only if the Agent advertises
`session/load` or `session/resume`. The timer is cancelled by any new need.

### 2.6 Resources

Session terminals and URL flows keep their current bridge representation, keyed by
terminal ID and interaction ID. A login terminal keeps its `methodId` with the
terminal itself, not on the operation that started it. They leave `live` as protocol §4.6 states;
acknowledgement records and terminal statuses go. The terminal login keeps the auth
terminal manager and its ordered events; its operation ends with the process exit,
and its terminal stays until dismissed. Terminal input is written as received, in
order; the bridge does not deduplicate it.

Step 6 lands the resource actions (protocol §7) in `bridge::resources`:
- The resource is found by the full scope and resource ID. It is checked under
  one guard, without `expect` and without an operation.
- A URL flow is named by its interaction ID (the flow's registration). Dismissing
  it reuses the settle and route primitives that end a flow with its scope, so a
  late dismissal of an old flow never reaches a new flow that reuses its
  `elicitationId`.
- A login terminal's dismissal is allowed only after the terminal exits, and it
  leaves the ended operation as it is.
- Input and resize run after the guard is released. Input goes on the terminal's
  own blocking writer, in the order the actor receives it. A stdin under
  backpressure therefore never holds the bridge lock that the terminal's output
  observer takes.

The session reads (protocol §8) are `server::read_session`:
- An attachment comes from the published cut (`Publisher::attachment`).
- The context search and read run outside the actor, in the workspace the actor
  returns for the held session's own `cwd`, with the existing workspace checks.
- A read holds, loads and renews nothing.

## 3. Types

The documents, events, ops, operation requests and results, and errors are typed
Rust DTOs. TypeScript in `shared/protocol.ts` is generated from them, and a test
fails when the generated output is stale. The HTTP bodies of writes, resource
actions, reads and errors are in `src/protocol/wire.rs`. The bridge's own
admission and delivery types are internal. ACP types are referenced from
`@agentclientprotocol/sdk`, not regenerated. The earlier draft's types are deleted in step 2.

## 4. Invariants and gates

| Invariant | Statement |
| --- | --- |
| I1 single write path | only the commit step publishes documents and events |
| I2 per-event consistency | after every event, a replica's `live` equals a published `live`, and its log equals the published log, or its own unconfirmed log while the bridge's is unknown |
| I3 exact cut | an alignment plus the events after it reproduce the published documents, with no gap and no duplicate |
| I4 coalescing safety | replacing any queued suffix by an alignment gives the same final document; `end` survives |
| I5 identity | the same messages give the same entries and hashes, live or replayed, however chunked |
| I6 admission | writes and loads are serial; `expect`, status and occupation decide; IDs never repeat |
| I7 holding | a needed session is held; only a loadable session is released |
| I8 privacy | a committed turn's process never appears in a later document or event |

### 4.1 Convergence gate (I1–I4, I8)

A Rust harness drives the bridge with a scripted Agent. Three replicas apply events
**one at a time** and are compared, after each event, against the sequence of
published documents:

- one follows every event;
- one lags and has random suffixes of its queue coalesced (I4);
- one disconnects at random cuts and subscribes again with its head (I3);
- one pauses consumption, has a queued but unsent log change coalesced, and
  resumes; it must apply the alignment directly, without subscribing again.

Scenarios run on the session registry (control updates, streamed and folded turns,
commits, reloads with equal and diverging replays, terminals rolling past their
bound, URL flows) and through the real coordinator and ACP transport (prompts,
permissions, a turn started by another client, delete, release, Agent restart). The
harness also mutates state without committing and asserts that it reports the
divergence.

### 4.2 Acceptance

The acceptance cases of
[client-api-redesign.md §11](client-api-redesign.md#11-acceptance) (1–18) are the
acceptance suite. Each lands, as a bridge test, in the step that makes it
reachable (§5), and again in the client engine where it has a client half (3, 4, 9,
15, 16). The [coverage ledger](acp-coverage.md) replaces the earlier draft's chains with one per
capability:

```text
ACP capability → bridge authority → document path or operation → result in op
               → alignment of a fresh replica → lifecycle end
```

## 5. Migration

The web UI stayed on `/api/v1` until it moved to v2 in one step (7), so the
product kept working throughout. Each step is one PR, validated in CI. Steps 1–8 are
done: step 8 (PR #46) removed the v1 routes and SSE, the business events and
journal, the browser's v1 store and compensation, and drove the backend smokes over
v2. Step 9 is in progress.

| Step | Work | Acceptance |
| --- | --- | --- |
| 1 | The redesign settled, the protocol and this plan (docs only) | — |
| 2 | Remove the earlier v2 draft: `src/server/v2.rs` and its routes, the records and intent table, the replication log and Hub, the commit step, owner-qualified subscriptions, the projections and their tests, and the generated `shared/protocol.ts` types; `controls.rs`, `diff.rs` and `typescript.rs` stay (§1); v1 unchanged | v1 suites still pass |
| 3 | `canon.rs`: message reduction, turns, normalization, prompt resolution, hash chain; `TurnLog` built from live commits and from replays | 1 |
| 4a | DTOs and generated TypeScript; operation slots and occupation in bridge state; projections and the commit step for `log` and `live`, driven by the session registry's transitions; the per-event convergence gate on the session registry | 2, 5, 13, 18 |
| 4b | The commit step in the bridge actor, run when the bridge lock's mutable guard is released; the bridge's remaining transitions (global and catalog operations, login terminals, the Agent connection); the gate through the real coordinator and ACP transport | — |
| 5 | `GET /api/v2/stream` and `GET /api/v2/scopes/{scope}`: Hub cut, alignment, coalescing, `end`; hold by subscription and need, `load` on subscription, release of loadable sessions only; the publication shared across connections and their automatic recovery | 3, 4, 12 |
| 6 | `POST /api/v2/scopes/{scope}/ops`: admission, all kinds, `create`/`fork`/`list` responses, occupation, `agent_request`, the `catalog` scope; resource actions | 6–11, 14, 16, 17 (bridge) |
| 7 | Web UI on v2: the generic replica engine (alignment, log truncation and append, `prev` checks, live patches) outside React, and the projection. PR #34, built on the earlier draft, is not landed; `shared/replica.ts` and `shared/protocol-validate.ts` are carried over where they fit | 3, 4, 9, 15, 16 (client) |
| 8 | Remove v1: routes, business events, browser compensating state; Markdown export and attachment links on v2 | — |
| 9 | Layer 1 clean-up: remove incarnation-based isolation from the bridge internals | — |

Step 9 removes the session incarnation entirely, without a renamed successor
(no session generation, owner token or tombstone). That covers:

- its allocation and storage: `SessionRegistry::next_incarnation` and
  `SessionState.incarnation`;
- its use in resource addressing: `SessionResourceOwner`, the permission,
  elicitation and URL routes, terminals, the history-sync owner, and the unused
  v1 attachment reference;
- its use in the ingress, the per-session FIFO (`SessionHandle` with its
  registration allocation), the coordinator, RPC completion hand-off, and
  load/replay continuations;
- every `StaleIncarnation` check and error, and the tests that only simulate an
  old incarnation number.

What stays is ordinary ACP: JSON-RPC request/response correlation, the exact end
of the current op, cancellation and shutdown draining, one load's candidate
committed atomically or rolled back, and each resource's own ID and responder
(permission and elicitation requests, URL registrations, terminals). The bridge
generation (one Agent connection, its `epoch`) and an outstanding RPC's own
allocation also stay. A session is identified by its ID, as in ACP.

Dependencies: 1 → 2 → 3 → 4a → 4b → 5 → 6 → 7 → 8, and 9 after 8. #16 and #17 are fixed
visibly in step 7.

## 6. Open questions

- **Release timer.** Step 4 keeps `--session-unobserved-timeout` as the linger before
  release. Whether its default should change now that it affects performance only is
  decided with measurements.
- **Several sample points.** Alignment matches the head only (redesign §12.3). If
  long histories make full alignment costly, `have` can carry several points; the
  Hub then picks the longest match.
