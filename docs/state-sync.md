# Sync protocol implementation plan

Status: plan, 2026-10-14. This document is how the bridge implements
[attyd protocol v3](protocol.md), settled in
[client-api-redesign.md](client-api-redesign.md). It covers the internal design,
the invariants tests enforce, and the migration from protocol v2 and `/api/v1`.
Lifecycle and concurrency changes follow the
[TDD ledger](bridge-state-machine-tdd.md): bridge behavior is tested before browser
projections.

## 1. Starting point

Protocol v2 is implemented on `main` (its migration steps 1–5). The v3 work reuses
what does not depend on the machinery v3 removes:

| v2 piece | Path | In v3 |
| --- | --- | --- |
| Protocol DTOs, generated `shared/protocol.ts` | `src/protocol/mod.rs`, `typescript.rs` | kept; the types are replaced (§3) |
| Control reduction | `src/protocol/controls.rs` | kept as is (protocol §4.5) |
| Projections | `src/protocol/project.rs` | rewritten for `log` and `live` (§2.2) |
| Commit step, identity diff | `src/protocol/publish.rs`, `diff.rs` | kept for `live`; gains the log (§2.3) |
| Replication log | `src/protocol/log.rs` | removed: v3 has no retained log |
| Hub | `src/protocol/hub.rs` | kept for cut, routing, queues and coalescing; resume, `prev` and `position` go (§2.4) |
| Records and intents | `src/protocol/commands.rs` | removed; replaced by operation slots (§2.5) |
| Convergence replicas | `src/protocol/replica.rs`, `stream_replica.rs` | rewritten for alignment events (§4) |
| Routes | `src/server/v2.rs` | v3 routes in `src/server/v3.rs`; v2 removed with the web client's move |

Layer 1 (bridge ↔ ACP server) is not changed by this plan until its last step: the
bridge keeps its sessions, incarnations, ingress and coordinator internally. v3
never exposes them.

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
  interactions, result, error, cancel_requested }` in bridge state. A held session
  keeps it in its registry entry; `global` and `catalog` keep theirs next to the
  agent state.
- **Occupation.** An `occupied: HashMap<Scope, OpId>` records scopes an operation
  occupies besides its own (delete occupies `session/<id>`). A held occupied scope
  shows the occupying operation as its `op`.
- **Turn log.** A held session keeps `TurnLog { entries: Vec<Arc<LogEntry>> }`,
  appended by commits and rebuilt from a load's replay (§2.3).
- **Holding need.** A session's need is computed, not stored: subscribers, occupation,
  or live resources (protocol S6).

### 2.2 Projection

Pure functions define the published documents:

- `project_session(&SessionRegistry, session_id) -> Option<(Arc<TurnLog>, SessionLive)>`
- `project_global(&AgentState, &SessionRegistry, &OpSlot) -> GlobalLive`
- `project_catalog(&CatalogState) -> CatalogLive`

They live in `src/protocol/project.rs` and are the only definition of what clients
see. `op.process` comes from the running turn's `SharedTurnUpdates`; it is absent
once the turn committed, so released process content never enters a document.

### 2.3 Commit step

Every transition in the bridge actor ends with one commit step, run when the bridge
lock's mutable guard is released (as in v2):

1. **Log.** For each held session, compare its `TurnLog` with the published one.
   - unchanged (the same `Arc`): nothing;
   - appended: a `LogChange { from: old length, prev, entries: new tail }`;
   - rebuilt (a load replaced it): `from` is the longest common prefix of the two
     chains, found by comparing hashes.
2. **Live.** Project, then diff against the published `live` with the v2 identity
   diff (pointer comparison for `op.process`, value comparison for small fields,
   `trim` plus `append` for terminal tails).
3. **Publish.** Emit one event per changed scope. A log change and the same scope's
   live patch are one event, so a commit carries both (protocol S2). Store the new
   published documents and hand the events to the Hub under the same lock.

**`canon` and hashing** live in `src/protocol/canon.rs`:

- `reduce_messages(&[SessionUpdate]) -> Vec<Message>` applies protocol §3.2 step 1;
- `turns(messages) -> Vec<Turn>` applies step 2;
- `normalize(&[ContentBlock]) -> Vec<ContentBlock>` applies step 3;
- `chain(prev, &Turn) -> Hash` hashes the JCS serialization.

A live commit builds the turn from the operation's prompt and the turn's Agent
messages; a load builds every turn from the replay. Both go through the same
functions, so the same messages give the same entry (acceptance 1).

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
- **Per-subscriber position.** For each scope the Hub remembers the log length and
  head it last queued to the subscriber, which is all a resync needs.
- **Coalescing.** A queue past its byte budget (1 MiB, as in v2) is collapsed per
  scope: queued events of a scope that still exists are replaced by one alignment
  from the remembered position; a queued `end` is kept, and nothing of its scope
  follows it.
- **Heartbeats.** An SSE comment on a stream idle for 15 seconds.
- **Holding.** A subscription to a session scope counts as one subscriber of that
  session while the stream is open. Subscribing to an unheld session submits a
  `load` to admission (§2.5); the stream waits for it under occupation.

No log is retained: a client that reconnects is aligned from the published
documents.

### 2.5 Operations

Operation writes are handled in the bridge actor, one at a time:

1. **Admission** (protocol §6.4), in one critical section: compare `expect` with the
   scope's `OpSlot`, check its status and the occupied scopes, then the capability,
   arguments and Agent connection.
2. **Install.** A starting operation replaces the scope's slot and occupies its
   other scopes, in the same critical section. The commit step publishes it.
3. **Run.** The operation enters the bridge input, as v2 commands do. Interactions
   of its ACP requests are added to its slot; answers are delivered by the existing
   interaction delivery path.
4. **End.** The transition that ends it sets its final status, result or error,
   frees its occupation, and wakes waiting subscriptions and loads.

`create`, `fork` and `list` keep their HTTP request open on a completion channel
that the ending transition fulfils. A dropped connection only drops the channel.

**Agent requests.** The coordinator routes each Agent request to a scope as today:
by session, or for a request-scoped elicitation by the JSON-RPC ID it names
(`ElicitationRequestScope.request_id`), resolved in wire order. The request joins a
running operation of that scope, starts an `agent_request` operation if the scope is
free, or is answered with its cancel outcome otherwise.

**Loads.** A subscription to an unheld session is admitted like a write without
`expect`: it waits while the session is occupied, then installs a running `load`
with the given `cwd`. A load reuses the existing `session/load` / `session/resume`
attachment path and ends by rebuilding the `TurnLog` from the replay.

**Release.** A session whose need ends starts the existing retirement timer
(`--session-unobserved-timeout`), now only a performance setting. When it fires and
the session is still not needed, the bridge closes it only if the Agent advertises
`session/load` or `session/resume`. The timer is cancelled by any new need.

### 2.6 Resources

Session terminals and URL flows keep their current bridge representation, keyed by
terminal ID and interaction ID. They leave `live` as protocol §4.6 states;
acknowledgement records and terminal statuses go. The terminal login keeps the auth
terminal manager and its ordered events; its operation ends with the process exit,
and its terminal stays until dismissed. Terminal input is written as received, in
order; the bridge does not deduplicate it.

## 3. Types

The documents, events, ops, operation requests and results, and errors are typed
Rust DTOs. TypeScript in `shared/protocol.ts` is generated from them, and a test
fails when the generated output is stale. ACP types are referenced from
`@agentclientprotocol/sdk`, not regenerated. v2 types are deleted with v2.

## 4. Invariants and gates

| Invariant | Statement |
| --- | --- |
| I1 single write path | only the commit step publishes documents and events |
| I2 per-event consistency | after every event, a replica's document equals a document the bridge published |
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
- one disconnects at random cuts and subscribes again with its head (I3).

Scenarios run on the session registry (control updates, streamed and folded turns,
commits, reloads with equal and diverging replays, terminals rolling past their
bound, URL flows) and through the real coordinator and ACP transport (prompts,
permissions, a turn started by another client, delete, release, Agent restart). The
harness also mutates state without committing and asserts that it reports the
divergence.

### 4.2 Acceptance

The acceptance cases of
[client-api-redesign.md §11](client-api-redesign.md#11-acceptance) (1–16) are the
v3 acceptance suite. Each lands, as a bridge test, in the step that makes it
reachable (§5), and again in the client engine where it has a client half (3, 4, 9,
15, 16). The [coverage ledger](acp-coverage.md) gains a v3 chain per capability:

```text
ACP capability → bridge authority → document path or operation → result in op
               → alignment of a fresh replica → lifecycle end
```

## 5. Migration

v3 is built next to v2 and v1 under `/api/v3`, and the web UI moves to it in one
step, so the product keeps working throughout. Each step is one PR, validated in CI.

| Step | Work | Acceptance |
| --- | --- | --- |
| 1 | This plan, protocol v3 and the redesign settled (docs only) | — |
| 2 | `canon.rs`: message reduction, turns, normalization, hash chain; `TurnLog` built from live commits and from replays | 1 |
| 3 | v3 DTOs and generated TypeScript; operation slots and occupation in bridge state; projections and the commit step for `log` and `live`; the per-event convergence gate | 2, 5, 13 |
| 4 | `GET /api/v3/stream` and `GET /api/v3/scopes/{scope}`: Hub cut, alignment, coalescing, `end`; hold by subscription and need, `load` on subscription, release of loadable sessions only | 3, 4, 12 |
| 5 | `POST /api/v3/scopes/{scope}/ops`: admission, all kinds, `create`/`fork`/`list` responses, occupation, `agent_request`, the `catalog` scope; resource actions | 6–11, 14, 16 (bridge) |
| 6 | Web UI on v3: the generic replica engine (alignment, log truncation and append, `prev` checks, live patches) outside React, and the projection; PR #34 is reworked onto it, reusing `shared/replica.ts` and `shared/protocol-validate.ts` | 3, 4, 9, 15, 16 (client) |
| 7 | Remove v2 and v1: routes, records, intents, the retained log, process pages, owner tags, the v1 events and browser compensating state; Markdown export and attachment links on v3 | — |
| 8 | Layer 1 clean-up: remove incarnation-based isolation from the bridge internals | — |

Dependencies: 1 → 2 → 3 → 4 → 5 → 6 → 7, and 8 after 7. #16 and #17 are fixed
visibly in step 6.

## 6. Open questions

- **Release timer.** Step 4 keeps `--session-unobserved-timeout` as the linger before
  release. Whether its default should change now that it affects performance only is
  decided with measurements.
- **Several sample points.** Alignment matches the head only (redesign §12.3). If
  long histories make full alignment costly, `have` can carry several points; the
  Hub then picks the longest match.
