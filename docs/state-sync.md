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

- `project_global(&BridgeState) -> GlobalState`
- `project_session(&SessionEntry) -> SessionState`

They are the only definition of what clients see. Snapshots (`GET /api/v2/global`,
`GET /api/v2/sessions/{id}`) call them directly. Privacy rules, such as releasing
finished process content and keeping private payloads out, live inside them.

### 2.2 Commit step

Every transition in the bridge actor ends with one commit step for the scopes it
touched:

1. Project each touched scope.
2. Diff the result against the document last published for that scope, and emit
   ops for every difference (protocol §4).
3. If any ops exist, append one entry `{ scope, seq, prev, ops }` to the log, and
   store the new document as the published one.

Length alone can't tell what changed in the large sequence fields, so diffing them
uses identity:

| Field | Changes | Diff |
| --- | --- | --- |
| `history.timeline`, `turn.updates` | growth; in-place folds (`tool_call_update` keeps its original slot, see `fold_history_update` in `src/history_cache.rs`; merged chunks); whole replacement at baseline commit | Elements are `Arc`-shared. Active turn updates already are (`SharedTurnUpdates`); history entries become so in step 2, because `fold_history_update` returns plain values today. Compare published and current element by pointer: equal pointers mean unchanged. A changed element is an index `set`, a new tail is `append`, and a new history revision is one `set` of the field. |
| terminal `output` | growth, plus front trimming when the tail exceeds its bound (`src/terminal.rs`, reader loop) | The bridge keeps an absolute counter of trimmed and appended UTF-16 units per terminal. The difference against the published counters gives `trim` + `append`. |
| maps (`interactions`, `urlFlows`, `terminals`, `outcomes`, `requests`, `sessions`) | upsert, remove | per key; values compared by pointer first, then by value |
| scalars and small structs (`controls`, `operation`, headers) | any | compared by value |

Comparing pointers is linear in the number of elements, with no content comparison.
If profiling shows that is too costly for long timelines, a per-field change list
recorded by the transition can replace it. The gate still compares against full
projections, so neither strategy can hide a change.

The commit step emits whatever differs from the last published document, so a call
site can neither forget an event nor choose the wrong one. `advance_session_delta`,
`touch`, the hand-built `bridge/session_view` values, and the paired `view_revision`
and `live.revision` counters all go away. A scope's position is the `seq` of its last
entry.

The published documents keep only what diffing needs: `Arc` handles shared with
bridge state, plus terminal counters. Publishing therefore doesn't duplicate
conversation content. The
[memory retention](runtime-memory-retention.md) tests extend to the published
documents and the log.

Global summaries are part of `global`. When a session transition changes its
summary, the commit step therefore touches both `session/<id>` and `global`. They
get consecutive `seq`s in one actor turn.

### 2.3 Log and Hub

The log is a bounded in-memory suffix per epoch, accounted by bytes. Compaction
drops the oldest entries. A replica asking to resume from before the retained suffix
gets a `reset` (protocol §4).

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
epoch, keyed by `clientIntentId`. Each entry stores a fingerprint, `(method, target sessionId, owner
tag, body digest)`, and the record or operation ID it produced. Equal bodies sent to
different targets therefore conflict instead of aliasing. Turns already keep an
intent table (H07), which gains the target fields. Create and fork gain the table, so
a retry or a lost response resolves to the existing record. The same holds for every
other operation command.

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
| 4 | Session commands with `at`/`scopes`, `If-Match` tags and `ProtocolError`; the open command and its lease; intents for every operation command; interaction response records; the reads | — |
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

- **Pointer-diff cost.** Measure the pointer-compare diff on long timelines in step 2.
  Switch to transition-recorded change lists only if it shows up.
- **Log retention budget.** Choose the byte bound for the per-epoch log, and how it
  interacts with the existing subscriber budgets and the protocol §3.4 minimums.
