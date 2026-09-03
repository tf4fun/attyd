# Authoritative-history bridge runtime

This document defines attyd's runtime and recovery contract. It supersedes the earlier
active-turn-only design.

## Decision

The ACP Agent is the only persistent authority for session history. The bridge is a disposable,
in-memory ACP client and read-through projection. The browser is a presentation layer and never
supplies history to the bridge or talks ACP directly.

For every materialized session the bridge owns:

- one immutable baseline produced by the most recent successful `session/load`, or by atomically
  committing observed turns when the Agent does not advertise load;
- at most one active or completed-but-not-yet-reconciled turn overlay;
- live permission, elicitation, URL, terminal, MCP and control state;
- bounded idempotency and revision metadata that contains no conversation payload;
- at most one staged load candidate.

Closing the bridge discards all of this state. Restarting reconstructs an idle session only through
the Agent. No browser state, local database, temporary file or mmap file is a recovery source.

## Agent compatibility profile

`loadSession` is preferred but is not required for a newly created, continuously materialized
session. With load, the Agent replay remains the persistent authority and replaces the bridge
baseline after every terminal turn. Without load, the bridge skips reconciliation I/O and
atomically promotes the exact accepted prompt plus observed turn updates into its in-memory
baseline.

The no-load mode is deliberately process-local. It supports subsequent turns and browser
reconnects while the same bridge process retains the session, but it cannot cold-materialize old
history or recover it after bridge restart. The bridge does not create a database, browser-backed
history, temporary persistence or a private protocol extension to hide that ACP limitation.

While a session is materialized by a bridge, that bridge is assumed to be its only writer. ACP v1
provides no revision or lease that can prevent a different ACP client from concurrently appending to
the same Agent session. A replay that violates the recorded prefix/append contract blocks the
session instead of being guessed into a valid state.

The Agent must emit all updates belonging to a prompt before its `PromptResponse`. ACP v1 does not
correlate `session/update` with a prompt or load request, so late same-session conversation updates
cannot be attributed safely.

## Ownership

```text
ACP Agent
`- sole persistent history authority

Bridge process
|- HistoryCache: SessionKey -> Arc<HistorySnapshot>
|- ActiveOverlay: zero or one per session
|- LoadCandidate: zero or one per session
|- LiveResourceStore
|- bounded intent/revision metadata
`- bounded shared subscriber delivery

Browser
|- rendered projection
|- unsent draft, queued prompts and visual preferences
`- user intents carrying bridge revisions
```

The baseline is a cache, not a second persistent authority. A load-backed baseline is derived from
`session/load`; a no-load baseline is derived only from prompts accepted by this bridge and ACP
updates it directly observed. Neither is persisted. Only a load-backed baseline is freely
evictable, because evicting a no-load baseline would make a live session unrecoverable.

## Session state machine

```text
Cold
  | first observe/open
  v
Loading -------------------------------> Blocked
  | valid load                              ^
  v                                         | deterministic incompatibility
Ready(Bn, Hn)                               |
  | prompt(If-Match: Hn)                    |
  v                                         |
Running(Bn + live overlay)                  |
  | PromptResponse / terminal error         |
  v                                         |
Reconciling(Bn + completed overlay) --------+
  | load: valid current candidate
  | no load: atomic local overlay promotion
  v
Ready(Bn+1, Hn+1)
```

`Bn` is an immutable Agent replay. `Hn` is an opaque bridge history revision containing bridge
epoch, session incarnation and a monotonic generation. It is not a message count, ACP message ID or
content hash. A canonical replay digest and item count may be stored separately for consistency
checks.

New sessions start in `Ready(empty, H0)`. Sessions discovered by `session/list` stay `Cold` until
selected; attyd does not load every listed session at process startup.

### Initial materialization

The first observer of a Cold session starts one single-flight load. Replay updates are validated and
staged privately. Only a successful response and valid complete candidate atomically install a
baseline. Concurrent observers join the same load and never see a partial candidate.

A transient load failure retries with bounded exponential backoff and jitter. A retry begins only
after the prior request has definitively terminated; if its outcome is uncertain the connection
must be drained or replaced first. Unsupported, not-found, authorization, invalid replay and hard
resource-limit failures become visible `Blocked` states rather than hot retry loops.

If the Agent does not advertise load, a Cold historical session cannot be materialized. This does
not affect a new or already materialized no-load session whose baseline is still in bridge memory.

### Turn admission and idempotency

The browser appends a turn with two independent guards:

```text
Idempotency-Key: clientIntentId
If-Match: historyRevision
```

Inside one session actor transaction the bridge verifies that the session is `Ready`, the history
revision matches, and the append slot is unused; it then records the fixed-size intent identity and
payload digest, transitions to `Running`, and only then dispatches `session/prompt`.

- Same key and same digest returns the existing operation without another Agent request.
- Same key and different digest is an idempotency conflict.
- A different key for a consumed or stale revision is rejected.
- A new bridge epoch rejects all revisions from a previous process lifetime.
- Bridge crash after possible Agent dispatch is `Uncertain`; exactly-once across an unpersisted
  bridge restart is intentionally not promised.

Queued prompts are a browser send buffer, not ACP state and not accepted Bridge work. While a
session is `Running` or `Reconciling`, the browser may keep and edit local queued prompts but does
not send them to the Bridge. After reconciliation commits, the Bridge publishes the new
`historyRevision`; the browser takes the queue head, attaches that new revision and submits it as an
ordinary turn. Only that submission transfers lifecycle ownership to the Bridge.

Consequently no queue append cursor or speculative Agent-history position is required. If the page
closes before dispatch, its unsent queue disappears like an unsent composer draft. Two pages may
have independent local queues; the normal history CAS ensures that at most one can consume the new
append slot, and the loser must refresh before retrying.

### Active delivery and observers

Prompt input and normalized ACP updates are folded once into the active overlay. Browser disconnect,
backgrounding, slow delivery, refresh and observer count do not cancel Agent work or call load.

An observer registered during `Running` or `Reconciling` receives one coherent view:

```text
immutable baseline + current overlay + independent live resources
```

Snapshot capture and suffix registration are one actor operation. Each snapshot/delta carries
bridge epoch, session incarnation and view revision. A continuous cursor receives the bounded
suffix; an old epoch, gap, reordering or expired suffix receives a reset snapshot. Crossing a
reconciliation commit yields either old baseline plus overlay or the new baseline, never a mixture.

### Reconciliation

A terminal Prompt result does not immediately retire its overlay. It transitions the session to
`Reconciling` and selects one of two commit paths from the immutable initialized capability set.
With load support, it starts one bridge-owned `session/load`; that load is not associated with a
browser or observer. Without load support, it performs no Agent I/O and promotes the completed
overlay into the existing in-memory baseline.

Replay is built in a separately accounted candidate. Before commit the bridge validates:

- ACP structural and semantic validity;
- item and byte limits;
- session identity and current attempt/connection generation;
- the configured stable-prefix/append rule;
- visibility of the completed accepted prompt/turn.

On success, one atomic transaction swaps the baseline, advances `historyRevision`, removes the
overlay and candidate, and changes the phase to `Ready`. Even an identical replay advances the
revision so the consumed append slot cannot be reused.

The no-load transaction folds synthetic `user_message_chunk` updates for the accepted prompt and
the already validated active updates onto the prior baseline. Limit, phase, incarnation and
operation checks complete before replacement. Failure preserves the prior baseline and completed
overlay and exposes `Blocked`; success advances the same opaque revision/idempotency ledger used by
the load-backed path.

On retryable failure, only the candidate is dropped. The old baseline and completed overlay remain
visible while a single retry task backs off. No prompt, load, close, delete, fork or config mutation
may overlap that same-session reconciliation.

## Live resources

Permission and elicitation state needed by an active turn is part of the live projection, but live
resources are not assumed to be history. A terminal, accepted URL flow or MCP operation may outlive
the turn that introduced it and is retained until its own protocol terminal transition. Baseline
commit clears only the reconciled overlay and turn-scoped interactions.

## Memory model

History must not be embedded in `SessionRuntime`, canonical deltas, raw event journals or one String
per subscriber. Existing runtime snapshots clone sessions on every mutation, so embedding complete
history there would multiply memory and serialization work.

The implementation uses an independent store:

```text
HistoryCache<SessionKey, Arc<HistorySnapshot>>
RuntimeState { phase, historyRevision, overlay, live resources, ... }
LoadTransaction { candidate, byte reservation, attempt }
```

Budgets cover per-session and global baseline bytes, overlay bytes, candidate bytes, shared snapshot
delivery, subscribers and all live-resource classes. Reconciliation peak memory includes the old
baseline, completed overlay and candidate simultaneously. A candidate that cannot reserve capacity
fails without truncating or modifying the current view.

Pressure-triggered LRU may evict only a load-backed `Ready` session with no observers, operation,
interaction or live resource. `Loading`, `Running`, `Reconciling`, observed and no-load sessions
are pinned. Eviction keeps only a small Cold handle; the next observation performs one load. Close,
delete and bridge shutdown release baselines, candidates and retry tasks.

## Browser API direction

The browser-facing surface is business semantics, independent of ACP transport details:

```text
GET  /api/v1/sessions/:id
GET  /api/v1/sessions/:id/events       (SSE)
POST /api/v1/sessions/:id/turns
POST /api/v1/sessions/:id/turns/:turn/cancel
POST /api/v1/sessions/:id/interactions/:interaction/response
```

A view includes `bridgeEpoch`, `sessionIncarnation`, `historyRevision`, `viewRevision`, phase,
baseline, active overlay, live resources and any synchronization error. HTTP command acknowledgement
and SSE delivery may race; stable intent/entity IDs make either order converge.

The removed raw browser WebSocket protocol is not a compatibility fallback. The business REST/SSE
surface never owns ACP lifecycle semantics, history folding or reconnect decisions.

## Non-negotiable invariants

- **H01 Authority:** a load-backed baseline comes from one successful Agent load; a no-load
  baseline contains only prior bridge memory plus the exact accepted prompt and observed updates.
- **H02 Atomicity:** a candidate is either installed in full or never visible.
- **H03 Single overlay:** one session has at most one running/reconciling turn.
- **H04 No active load:** observers never cause load during Running or Reconciling.
- **H05 Reconcile retention:** terminal output remains in the overlay until baseline commit.
- **H06 CAS:** one history revision admits at most one distinct turn append.
- **H07 Idempotency:** one intent key/digest dispatches at most once per bridge epoch.
- **H08 Per-session exclusion:** load, prompt and session mutations never overlap for one session.
- **H09 Observer independence:** subscriber lifecycle cannot affect Agent work.
- **H10 Generation isolation:** old epoch/incarnation/attempt events cannot mutate current state.
- **H11 Live-resource independence:** baseline replacement cannot retire unrelated live resources.
- **H12 Bounded memory:** baseline, overlay, candidate and delivery are byte-accounted globally.
- **H13 Safe eviction:** only unobserved Ready baselines are evictable.
- **H14 Honest uncertainty:** lost non-idempotent outcomes are never blindly redispatched.
- **H15 No persistence:** bridge history state disappears with the bridge process.

## Delivery order

Implementation follows TDD in this order:

1. replace the old active-only specification and tests;
2. introduce the independent shared HistoryCache and pure state transitions;
3. retain completed overlay and implement single-flight reconciliation;
4. expose coherent baseline-plus-overlay snapshots to observers;
5. enforce CAS/idempotency and move unsent queued prompts entirely into the browser;
6. move browser reads and commands to the business API/SSE surface;
7. remove the old requester-private load replacement and active-only projection paths;
8. pass transport, browser, fault-injection, memory and Goose compatibility gates.
