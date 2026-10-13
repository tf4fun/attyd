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
   `set`/`remove` ops for every changed path. Append-only fields
   (`history.timeline`, `turn.updates`, terminal `output`) are not compared value by
   value. A length cursor per field emits `append`, so large content is never
   diffed or copied.
3. If any ops exist, append one entry `{ scope, seq, prev, ops }` to the log, and
   store the new document as the published one.

The commit step emits whatever differs from the last published document, so a call
site can neither forget an event nor choose the wrong one. `advance_session_delta`,
`touch`, the hand-built `bridge/session_view` values, and the paired `view_revision`
and `live.revision` counters all go away. A scope's position is the `seq` of its last
entry.

The published documents keep only what diffing needs. Append-only fields are
represented by their cursors, and other fields share `Arc` payloads with bridge
state, so publishing doesn't duplicate conversation content. The
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
transition and commit step, the handler records the log position at that moment and
returns it as `at`. This holds even when the command commits no change, such as an
idempotent duplicate. Long-running effects (turns, auth flows) commit their
acceptance before the HTTP response. Later progress arrives only through
replication.

Global requests (`authenticate`, `logout`, `auth_terminal`, `create_session`) become
entries in `GlobalState.requests`. Request-scoped ACP elicitations attach to the
request whose JSON-RPC call is in flight. This replaces `request_elicitations` and
`request_url_flows`, and the `acp/authenticated`, `acp/logged_out` and
`bridge/auth_terminal_*` events.

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

## 4. Invariants and the convergence gate

- **S1 single write path:** only the commit step appends to the log.
- **S2 convergence:** for every scope, `apply(snapshot@a, entries(a..b)) ==
  snapshot@b`.
- **S3 cut:** each scope's first entry plus its suffix has no gap, no duplicate, and
  no mix of epochs or incarnations (extends H10).
- **S4 coalescing safety:** replacing any queued suffix with the latest snapshot gives
  the same final document.
- **S5 observation independence:** `global` subscribers and snapshot reads never
  affect session lifecycle (extends H09, H13).
- **S6 privacy:** released process content and private payloads never enter a
  document, op or log entry.
- **S7 read your writes:** once a replica's stream position reaches a command's `at`,
  its documents reflect that command.

**Convergence gate.** A Rust harness drives the bridge with a scripted Agent. After
every step, it applies the delivered entries to a replica and compares the result
with fresh snapshots (S2). It repeats the scenario with forced coalescing (S4) and
with reconnects at random cuts (S3). The scenarios cover:

- control updates from the Agent;
- URL flow completion and cancellation;
- a background permission;
- a turn started by another client;
- a global request with a request-scoped elicitation;
- retirement, close, delete, fork and reattach;
- an epoch change;
- early `session/new` updates, once #8 is fixed.

A missed commit step shows up as a divergence, whatever the field. The harness lands
together with the commit step, in the same PR.

Browser tests check only the browser's half: applying a valid entry stream gives the
same replica as loading the final snapshot. These extend the existing fuzz tests.

## 5. Migration

v2 is built next to v1 and the web UI moves to it in one step, so the product keeps
working throughout. Each step is one PR, validated in CI.

| Step | Work | Absorbs |
| --- | --- | --- |
| 1 | Protocol DTOs with generated TypeScript; `project_global` / `project_session` with unit tests | #5 |
| 2 | Commit step, log, diff and append cursors; convergence harness (in process, no HTTP) | — |
| 3 | `GET /api/v2/global`, `GET /api/v2/sessions/{id}`, `GET /api/v2/stream`: Hub cut, `prev`, coalescing, resume, `position` | — |
| 4 | v2 commands with `at`, owner `If-Match` and `ProtocolError`; the interaction endpoint; reads (catalog, process pages, attachments, files) | — |
| 5 | Global requests: auth flows, auth terminal and `create_session`, request-scoped elicitations | — |
| 6 | Web UI on v2: a generic replica engine (JSON Pointer patcher and validation) outside React; sidebar busy and attention markers from `global.sessions` | #16, #17 (visible fix) |
| 7 | Remove v1: routes, business events, `advance_session_delta`, `touch`, `cachedSessions` and compensating browser state, and the journal replica if it has no other consumer | #18 |
| 8 | (optional) Path-subset profiles and an access-layer guide, when a second access layer is concrete | — |

Dependencies: 1 → 2 → 3 → 4 → 5 → 6 → 7. Steps 4 and 5 can be developed in parallel
once step 3 lands. #8 is independent and can land at any time, in v1 or v2.

#16 and #17 stay visible until step 6. If either becomes urgent before then, a
targeted v1 fix is possible, but that work would be thrown away in step 7.

## 6. Open questions

- **Diff cost on large maps.** `controls`, `interactions` and `terminals` are small.
  If profiling shows diffing a hot field is costly, the commit step may take a
  "touched paths" hint from the transition, as long as the gate still compares
  against a full projection.
- **Log retention budget.** Choose the byte bound for the per-epoch log, and how it
  interacts with the existing subscriber budgets.
- **Snapshot cost for cold sessions.** `GET /api/v2/sessions/{id}` with `cwd` can
  trigger a load. Decide whether a stream subscription should start the load and
  answer the snapshot fetch from it (as observe-then-fetch does today), or the fetch
  should start it.
