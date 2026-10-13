# Bridge state synchronization

Status: proposal, 2026-10-12. Nothing here is implemented yet. It builds on the
[runtime contract](active-turn-runtime.md), the
[session isolation proposal](session-isolation-proposal.md) and the
[memory retention plan](runtime-memory-retention.md), and does not replace them.
Lifecycle and concurrency changes made under this plan still follow the
[TDD ledger](bridge-state-machine-tdd.md).

## 1. Goal

The bridge owns all ACP state. Every client is a projection of that state with no
state of its own. Clients include the web UI today, and other access layers later
(a terminal UI, a chat bot, another editor).

This document treats delivery as one problem: **keep every observer's projection
equal to the bridge state it was given, using one mechanism.** Bug fixes come out of
that mechanism; they don't each get their own delivery path.

Requirements:

- **One write path.** The code that changes bridge state is the only source of what
  observers receive. No code path can change state without the change being
  delivered.
- **Projections are derived.** Each observer-visible view is a pure function of
  bridge state. Every delta it receives is defined against that function.
- **Streams carry state.** A command's result for the client that sent it is the HTTP
  response. Everything shared between observers is state, and a client that
  reconnects can recover it.
- **Access layers choose and trim.** A client subscribes to the topics it needs, at
  the level of detail it needs. Privacy rules, such as releasing finished turn
  process content, apply on the server for every client.

## 2. Current delivery paths

Two mechanisms exist side by side.

| Path | Producer | Consumer | Revision |
| --- | --- | --- | --- |
| Runtime journal: `RuntimeSnapshot` / `RuntimeDelta` | `SessionRegistry::commit_*` (`src/runtime_state.rs:2163`) via `flush_runtime` (`src/bridge.rs:1074`) | Hub `CanonicalProjection` (`src/server.rs:1330`), used only for the reset hint and the epoch | per-session `live.revision`, global `seq` |
| Session business events | hand-built at each call site: `advance_session_delta` (`src/bridge.rs:508`), whole `bridge/session_view` values | `/api/v1/sessions/{id}/events`, observers of that session only | mirror `view_revision`, bumped by `touch` (`src/session_mirror.rs:638`) and mirror transitions |
| Global events | `Lifecycle` wire strings (`src/server.rs:1789`) | `/api/v1/events`, every client | `catalog_changed.revision` only |

The session view itself is assembled on demand by `session_view_value`
(`src/bridge.rs:412`). It merges the mirror's history and overlay with the live
runtime state, and the browser fetches it over REST.

The journal already covers every live-state change in one place. The business
events, which are what observers actually receive, are written separately at about
twenty call sites. Each call site has to remember to emit an event and pick the
right one. Defects follow wherever a call site forgets:

| Symptom | Cause | Issue |
| --- | --- | --- |
| Usage, slash commands, title, Agent-initiated mode and config changes go stale | `update_control_state` (`src/runtime_state.rs:1045`) only reaches the journal | #16 |
| URL flow completion and cancellation aren't shown | `complete_elicitation` (`src/bridge/agent_dispatch.rs:603`) only reaches the journal | #16 |
| Background sessions never show "Input needed" | no projection carries per-session interaction state to non-observers | #17 |
| Background "working" markers are stale, and miss turns started by another client | `busySessionIds` (`web/src/app.tsx:492`) reads browser-side `cachedSessions` snapshots | (new) |
| Browser keeps snapshot caches, a background event panel and request-scoped paths | compensating for state the server doesn't project | #18 |

#8 (early `session/new` updates are lost) is a state defect, not a delivery
defect: the baseline never contains those updates. It is independent of this plan.
The convergence checks in §7 will cover it once it is fixed.

The bridge already derives the per-session activity that #17 needs. `SessionWork::of`
(`src/session_observers.rs`) classifies each session as `Idle`, `Running` or
`AwaitingInteraction`, but only to drive unobserved retirement. It reads
`SessionEntry.resources` (materialization, history sync), which the journal doesn't
carry.

## 3. Model

```text
ACP layer ──▶ bridge state (SessionRegistry: mirror + live + resources, connection)
                   │ commit step: derive topic changes from state (§6)
                   ▼
              topic changes ──▶ Hub: route, cut, coalesce, trim per profile
                                        │
                     ┌──────────────────┼──────────────────┐
                 web UI             terminal UI          chat bot …
```

### Topics

A topic is a named projection with its own revision. All revisions are scoped to
the bridge epoch.

| Topic | Contents | Size | Who subscribes |
| --- | --- | --- | --- |
| `runtime` | connection phase and error, agent info (hello, initialize response), auth state and any auth terminal, connection-scoped interactions | small | every client |
| `sessions` | catalog revision token, plus one summary per live session (below) | small, O(live sessions) | clients with a session list |
| `session/<id>` | the full session view as today: baseline, active overlay, controls, interactions, URL flows, terminals, operation, turn outcomes | large | clients showing that session |

A session summary has no conversation content:

```ts
interface SessionSummary {
  sessionId: string;
  sessionIncarnation: number;
  cwd: string;
  title: string | null;              // folded session_info_update, else the listed title
  lifecycle: SessionLifecycle;       // attaching | active | closing | …
  phase: SessionSyncPhase;           // history sync phase
  work: "idle" | "running" | "awaiting_input"; // SessionWork
  pending: { permissions: number; elicitations: number; urlFlows: number };
  updatedAt: string | null;
}
```

The Agent's paged `session/list` stays a REST resource, because the Agent owns it,
not the bridge. The `sessions` topic carries only its revision token. A client merges
the listed sessions with the live summaries by `sessionId`.

### Profiles

A subscription names a topic and a profile. A profile is a fixed field subset of the
topic's projection, defined in the Rust DTOs. The Hub filters each change against the
profile, so a trimmed client still receives a consistent stream. Initial profiles:

- `session/<id>`: `full` (today's view). Later: `compact`, which has no timeline and
  no process content, for clients that only answer prompts and permissions.
- `sessions`: `full`.
- `runtime`: `full`.

New profiles are added when a new access layer needs one, never per request. Field
selection syntax is a non-goal.

## 4. Sync protocol

Every topic uses the same envelope, whichever transport carries it (SSE today):

```ts
type SyncEvent =
  | { kind: "snapshot"; topic: string; epoch: string; revision: number; body: unknown }
  | { kind: "reset";    topic: string; epoch: string; revision: number; header: unknown }
  | { kind: "delta";    topic: string; epoch: string; fromRevision: number; revision: number; changes: Change[] }
  | { kind: "end";      topic: string; epoch: string; reason: string };
```

- **Cut.** Subscribing atomically captures the topic at revision `R` and registers
  delivery of every change after `R`. This is the existing observer-ready mechanism
  (`BridgeEvent::ObserverReady`), generalized to every topic. Small topics put the
  state in a `snapshot`. Large topics send a `reset`, and the client fetches the view
  over REST at revision `R` or later, as `bridge/session_reset` works today.
- **Continuity.** A client applies a `delta` only when `fromRevision` equals its own
  revision. On any gap, epoch change or `reset`, it discards its local projection and
  resynchronizes.
- **Coalescing.** A subscriber that falls behind on a topic loses its queued deltas
  for that topic and gets one `reset` (or `snapshot`) at the latest revision. This
  generalizes `canonical_reset_event` (`src/server.rs:968`). It is always safe,
  because streams carry only state.
- **End.** `end` replaces `bridge/session_retired`. It means the topic no longer
  exists (closed, deleted, unobserved, or a new incarnation). The client drops the
  projection.

Change vocabulary: keyed maps (interactions, URL flows, terminals, controls,
summaries) use `upsert`/`remove` by key. Append-only logs (active turn updates) use
`append`. Scalar headers (lifecycle, phase, operation, title) use `set`. The set is
closed and typed (§9, step 8); an unknown change kind is a protocol error, not a
silent refetch.

## 5. State versus events

Each message on today's wire, and where it goes under this model:

| Today | Under this model |
| --- | --- |
| `bridge/session_view` → `bridge/session_reset` | `session/<id>` `reset` |
| `bridge/session_delta` (all kinds) | `session/<id>` `delta` |
| `bridge/session_retired` | `session/<id>` `end` |
| `bridge/session_turn_complete` / `_failed` | `session/<id>` delta upserting a turn outcome keyed by `operationId` and carrying `clientIntentId`. The client that started the turn matches it by intent. Other clients see it too. |
| `bridge/connection`, `bridge/connection_error` | `runtime` header fields |
| `GET /api/v1/runtime` `hello` / `initialized` wrappers | `runtime` snapshot body (plain resource, the reshape in #5) |
| `acp/authenticated`, `acp/logged_out` | `runtime` auth state, plus the HTTP response for the caller |
| `bridge/auth_terminal_*` | `runtime` auth terminal state (status, bounded output tail, exit), shaped like session terminals |
| `bridge/catalog_changed` | `sessions` catalog revision |
| request-scoped elicitations (no session) | `runtime` connection-scoped interactions |

Nothing is left as a "global event". A client that reconnects at any moment gets
back everything other clients can see.

## 6. Where projection happens

**Decision (recommended): project at the bridge's commit step, and deliver from the
Hub.**

After each state transition, the bridge actor runs one commit step per touched scope
(connection, or one session). The step:

1. computes that scope's published projections: the `sessions` summary, the session
   view header and keyed maps, and the `runtime` fields;
2. diffs them against the projections it last published for that scope, emitting
   `set`/`upsert`/`remove` for every difference. Logs are emitted as `append` from a
   length cursor, so large content is never diffed;
3. bumps the topic revision only when something changed, and hands the changes to the
   Hub.

Every transition ends with this step, and the step emits whatever changed, so call
sites can no longer forget or choose an event: `advance_session_delta`, `touch` and
the hand-built `bridge/session_view` values all go away. One session revision
replaces the mirror `view_revision` and `live.revision` pair.

The Hub owns delivery only: subscriptions, the cut, per-topic coalescing,
backpressure and profile trimming. It needs topic headers for coalescing, not a
replica of bridge state. Once no consumer is left, the runtime journal replica
(`CanonicalProjection`) and its separate pipeline can be removed. Retirement must
first be verified against the memory retention tests.

Rejected alternative: **project in the Hub from the runtime journal.** The journal
already covers live state, but not mirror transitions (history phase, load, baseline
commit) or `SessionEntry.resources`, and `SessionWork` needs those. Making the journal
complete would replicate the whole bridge state into the Hub and keep two pipelines.
Trimming still happens in the Hub under the recommended design, so per-client
projection doesn't need the Hub to own projection.

Observation stays a command to the bridge. Subscribing to `session/<id>` registers an
observation lease, which drives materialization and unobserved retirement (H09, H13).
Subscribing to `sessions` or `runtime` never counts as observing a session.

## 7. Invariants and the convergence gate

- **S1 single write path:** only the commit step produces topic changes.
- **S2 derivation:** for every topic and profile, `apply(snapshot@R, changes(R..R'))
  == snapshot@R'`.
- **S3 cut:** a subscriber's first state plus its suffix has no gap, duplicate or mix
  of epochs or incarnations (extends H10).
- **S4 coalescing safety:** replacing any queued suffix with the latest snapshot gives
  the same final projection.
- **S5 observation independence:** `sessions`/`runtime` subscribers never affect
  session lifecycle (extends H09, H13).
- **S6 privacy:** released turn process content and private payloads never re-enter
  any topic or profile.
- **S7 no events:** every shared stream item is reconstructible from current state.

**Convergence gate.** A Rust test harness drives the bridge with a scripted Agent
through a scenario. After every step it checks S2 for every topic: the projection
built from the delivered stream must equal a fresh snapshot. It also re-checks after
forced coalescing (S4) and after reconnecting at random cuts (S3). The scenarios cover
the cases in §2: Agent control updates, URL flow completion and cancellation, a
background permission, a turn started by another client, retirement, reattach, and
early `session/new` updates once #8 is fixed.

The harness lands first and records today's divergences as expected failures. Each
migration step turns some of them green. A new projection field is covered
automatically, so a forgotten event is caught by the gate instead of being shipped.

Browser tests then check only the browser's half: applying a valid stream gives the
same reducer state as hydrating the final snapshot. This reuses the existing fuzz
tests.

## 8. Access layers

An access layer is a client of this protocol. It needs no bridge internals:

1. Subscribe to `runtime`, then to `sessions`, and to `session/<id>` (with a profile)
   for each session it shows.
2. Apply `snapshot`/`delta`/`reset`/`end` with one generic sync engine, and keep no
   other state.
3. Send commands over REST, with intent IDs where the API asks for them.

The web UI becomes the first access layer written to this contract. Its sync engine
should be a small module, separate from React, that another JavaScript client could
reuse.

## 9. Migration

Each step is one PR, validated in CI, and leaves the product working.

| Step | Work | Absorbs |
| --- | --- | --- |
| 1 | Convergence harness (§7), with current divergences as expected failures | test baseline for #16, #17 |
| 2 | One session revision and one commit step; mirror transitions commit through it. The wire is unchanged. | — |
| 3 | Session topic changes derived by the commit step; remove call-site deltas and `touch` | #16 |
| 4 | `sessions` topic with summaries on the global stream; browser busy and attention markers read it | #17 |
| 5 | `runtime` topic: connection, agent info, auth state, auth terminal, connection-scoped interactions | part of #5 (runtime reshape) |
| 6 | Turn outcomes as session state; remove the remaining global and session events (S7) | — |
| 7 | Browser generic sync engine; remove `cachedSessions`, background events and other compensating state | #18 |
| 8 | Typed Rust DTOs for the envelope, topics, profiles and changes; generated TypeScript | #5 |
| 9 | `compact` session profile and an access-layer guide, when a second access layer is concrete | — |
| 10 | Retire the runtime journal replica after checking its remaining consumers | — |

Dependencies: 1 → 2 → 3. Steps 4, 5 and 6 can follow 3 in any order. Step 7 follows
4–6. Step 8 can start for finished topics but closes last. Steps 9 and 10 are
optional and independent. #8 is independent and can land at any time.

Steps 3–6 change the wire format. The web UI moves in the same PRs, so no
compatibility layer for old browsers is kept (consistent with the
[runtime contract](active-turn-runtime.md)).

## 10. Open questions

- **Stream multiplexing.** The browser keeps the global stream (`runtime`,
  `sessions`) plus one session stream. Recommended: keep that until a client needs to
  observe several sessions at once, then add multiplexed subscriptions on one stream.
- **Connection-scoped interactions.** These are elicitations without a session,
  placed in `runtime` here. The alternative is to drop them as unreachable (#18).
  Recommended: keep them, because the ACP schema allows them.
- **Auth terminal output retention.** Choose the tail size. It should be bounded like
  session terminals, and cleared when the flow ends.
- **Summary update rate.** `updatedAt` and `work` can change on every turn update.
  Recommended: emit a summary change only when a summary field changes, and leave
  `updatedAt` out of per-update diffs.
