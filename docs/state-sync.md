# Sync protocol implementation plan

Status: plan, 2026-10-14. This document is how the bridge implements
[attyd protocol v2](protocol.md), settled in
[client-api-redesign.md](client-api-redesign.md). It covers the internal design,
the invariants tests enforce, and the migration from the earlier v2 draft and
`/api/v1`.
Lifecycle and concurrency changes follow the
[TDD ledger](bridge-state-machine-tdd.md): bridge behavior is tested before browser
projections.

## 1. Starting point

An earlier v2 draft is implemented on `main` (its migration steps 1–5). It was never
completed: no client uses it, and the web UI still runs on `/api/v1`. It is also
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

Two more techniques of the draft are reused when the new code is written, but their
modules go with the draft: the identity diff of turn updates
(`src/protocol/publish.rs`, for `op.process` in §2.3) and byte-budgeted subscriber
queues (`src/protocol/hub.rs`, §2.4). Nothing else of the draft is a constraint:
its routes, documents, records, intents, log, owner handling and tests go.

Layer 1 (bridge ↔ ACP server) is not changed by this plan until its last step: the
bridge keeps its sessions, incarnations, ingress and coordinator internally. The
protocol never exposes them.

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
- **Coalescing.** A queue past its byte budget (1 MiB, as in the earlier draft) is collapsed per
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
3. **Run.** The operation enters the bridge input, as v1 commands do. Interactions
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
`@agentclientprotocol/sdk`, not regenerated. The earlier draft's types are deleted in step 2.

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
acceptance suite. Each lands, as a bridge test, in the step that makes it
reachable (§5), and again in the client engine where it has a client half (3, 4, 9,
15, 16). The [coverage ledger](acp-coverage.md) replaces the earlier draft's chains with one per
capability:

```text
ACP capability → bridge authority → document path or operation → result in op
               → alignment of a fresh replica → lifecycle end
```

## 5. Migration

The web UI stays on `/api/v1` until it moves to the new v2 in one step, so the
product keeps working throughout. Each step is one PR, validated in CI.

| Step | Work | Acceptance |
| --- | --- | --- |
| 1 | The redesign settled, the protocol and this plan (docs only) | — |
| 2 | Remove the earlier v2 draft: `src/server/v2.rs` and its routes, the records and intent table, the replication log and Hub, the commit step, owner-qualified subscriptions, the projections and their tests, and the generated `shared/protocol.ts` types; `controls.rs`, `diff.rs` and `typescript.rs` stay (§1); v1 unchanged | v1 suites still pass |
| 3 | `canon.rs`: message reduction, turns, normalization, hash chain; `TurnLog` built from live commits and from replays | 1 |
| 4 | DTOs and generated TypeScript; operation slots and occupation in bridge state; projections and the commit step for `log` and `live`; the per-event convergence gate | 2, 5, 13 |
| 5 | `GET /api/v2/stream` and `GET /api/v2/scopes/{scope}`: Hub cut, alignment, coalescing, `end`; hold by subscription and need, `load` on subscription, release of loadable sessions only | 3, 4, 12 |
| 6 | `POST /api/v2/scopes/{scope}/ops`: admission, all kinds, `create`/`fork`/`list` responses, occupation, `agent_request`, the `catalog` scope; resource actions | 6–11, 14, 16 (bridge) |
| 7 | Web UI on v2: the generic replica engine (alignment, log truncation and append, `prev` checks, live patches) outside React, and the projection. PR #34, built on the earlier draft, is not landed; `shared/replica.ts` and `shared/protocol-validate.ts` are carried over where they fit | 3, 4, 9, 15, 16 (client) |
| 8 | Remove v1: routes, business events, browser compensating state; Markdown export and attachment links on v2 | — |
| 9 | Layer 1 clean-up: remove incarnation-based isolation from the bridge internals | — |

Dependencies: 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8, and 9 after 8. #16 and #17 are fixed
visibly in step 7.

## 6. Open questions

- **Release timer.** Step 4 keeps `--session-unobserved-timeout` as the linger before
  release. Whether its default should change now that it affects performance only is
  decided with measurements.
- **Several sample points.** Alignment matches the head only (redesign §12.3). If
  long histories make full alignment costly, `have` can carry several points; the
  Hub then picks the longest match.
