# Bridge state-machine TDD ledger

This ledger defines the regression gates for the authoritative history projection in
[active-turn-runtime.md](active-turn-runtime.md). The detailed test
matrix is [active-turn-runtime-tests.md](active-turn-runtime-tests.md).

## Product boundary

```text
Agent durable history
        |
        | session/load only on cold materialization
        v
Bridge immutable baseline + one active/reconciling overlay + live resources
        |
        | versioned snapshot/reset + ordered deltas
        v
Disposable browser presentation
```

The bridge is the complete ACP client. Browser commands are business intents; ACP request lifetime,
update routing, permission/elicitation responders, cancellation, cold-load replay and turn commit are
owned by the bridge after command acceptance. Unsent drafts and queued prompts remain browser-local.
The bridge remains memory-only and the Agent remains the sole persistent authority.

## Canonical state

```text
ConnectionRuntime
|- epoch and ordered publication sequence
|- negotiated capabilities and connection phase
|- listed session metadata
|- sessions: Map<SessionId, SessionRuntime>
`- bounded connection-scoped live resources and diagnostics

SessionRuntime                         HistoryCache (separate allocation)
|- incarnation and view revision       `- SessionKey -> Arc<HistorySnapshot>
|- phase: Cold | Loading | Ready
|         | Running | Reconciling
|         | Blocked | Closing | Closed
|- opaque historyRevision
|- one ActiveOverlay
|- one LoadAttempt metadata             LoadTransaction (separate allocation)
|- live resources                       `- one byte-accounted replay candidate
`- bounded intent tombstone
```

Complete history payload must not be placed inside `SessionRuntime`, runtime deltas or raw debug
journals. Those structures are cloned and serialized frequently. They carry only baseline identity
and revision metadata; observers access the shared snapshot through the history store.

## Transition ledger

| From | Intent/event | To | Agent I/O |
| --- | --- | --- | --- |
| Cold | first observer | Loading | one joined `session/load` |
| Loading | valid response | Ready | none |
| Loading | retryable failure | Loading | one delayed retry after termination |
| Loading | deterministic incompatibility | Blocked | none |
| Ready | valid append CAS | Running | one `session/prompt` |
| Ready | stale CAS/duplicate slot | Ready | none |
| Running | observer churn | Running | none |
| Running | PromptResponse | Reconciling | none |
| Reconciling | atomic local promotion | Ready | none |
| Ready after completion | no session subscriber | closing/closed | capability-gated `session/close` |
| any live | close/delete/shutdown | closing/closed | capability-gated lifecycle I/O |

Same-session prompt, load, close, delete, fork and config mutations are mutually exclusive.
Different sessions may progress concurrently under request, transport and delivery backpressure
bounds.

## Append contract

A turn request carries the prompt payload, an `If-Match: historyRevision` header and a stable
`Idempotency-Key`. The opaque history revision binds the bridge epoch, session incarnation and
append position; callers do not construct those parts separately.

History revision is a bridge-generated opaque token. Intent ID identifies a retry; history revision
identifies the append position. Neither can replace the other.

The actor atomically validates and consumes the append slot before Agent dispatch. Same key plus
same digest returns the existing operation. Same key plus another digest, another key for the same
consumed slot, or any stale epoch/incarnation/revision is rejected before Agent I/O.

The Bridge accepts turn submission only while Ready. The browser may still offer queued prompts
while Running or Reconciling, but those items are only a local send buffer. After the prior Turn is
committed, the browser receives the new history revision and submits the queue head as an ordinary
CAS-protected turn. Until that moment the Bridge has not accepted it and owns no queued payload.

This keeps queue semantics independent of ACP: page disposal may discard unsent queue items, and
two pages with queued items compete for the next append slot through the normal history CAS.

## Turn-commit contract

Prompt terminal delivery and history checkpoint are separate transitions for every Agent:

1. preserve the completed overlay;
2. enter Reconciling and exclude new same-session mutations;
3. fold the accepted prompt and validated overlay onto the prior in-memory baseline;
4. atomically swap the shared baseline and advance history revision;
5. clear the overlay exactly once;
6. publish one coherent replacement and enter Ready.

Failure before step 4 cannot alter the installed baseline or completed overlay. This path sends no
Agent request and is identical whether or not the Agent advertises load. The projection survives
browser replacement but not bridge replacement; Cold history remains unavailable without load.

After a terminal turn commit is published, zero session-specific subscribers schedules a
capability-gated upstream close. A successful close releases the projection, making the next
observer cold-load from the Agent. Subscriber loss while Running never cancels or closes the turn.
If close is unavailable or fails, the projection remains in memory.

## Observer contract

Opening another page never starts load for a materialized Running or Reconciling session. Subscriber
registration and snapshot cut are one operation. A subscriber sees either:

- a full reset containing baseline + overlay + live resources; or
- deltas whose first `fromRevision` exactly equals the subscriber cursor.

A gap, old epoch/incarnation, expired suffix or ordering error forces reset. Slow subscribers are
evicted without affecting Agent work. Baseline payload is shared/chunked rather than cloned into
every subscriber queue.

## Authority and compatibility

The following are explicit compatibility rules, not heuristics:

- `loadSession` is used for cold recovery, never routine post-turn synchronization;
- without load, only new/already materialized sessions are recoverable and history ends with the
  bridge process;
- all prompt updates precede PromptResponse;
- one bridge is the sole writer to a materialized Agent session;
- bridge crash recovery provides at-most-once caution, not exactly-once resubmission.

Violation produces a visible incompatible/blocked/uncertain state. The bridge never uploads browser
history, invents missing Agent data or silently treats a partial replay as authoritative.

## Memory invariants

- One installed `Arc<HistorySnapshot>` per materialized session.
- At most one overlay and one candidate per session.
- Old baseline + overlay bytes are accounted throughout normal turn commit; a candidate exists only
  for load/attachment transactions.
- Protocol-valid baseline, candidate and overlay growth has no bridge-defined cumulative cap.
- Wire values, live resources and subscriber delivery retain their independent safety and
  backpressure limits.
- Successful completion with no session subscriber closes and releases the session when supported.
- Running, Reconciling, observed and interaction-bearing sessions are pinned.
- Close/delete/generation shutdown drops baselines, candidates and retry tasks.
- No history payload is persisted.

## Proof gates

| Gate | Required evidence | Current implementation / verification |
| --- | --- | --- |
| S0 specification | authority/state/API/memory contract and adversarial test plan | this ledger and the linked runtime documents |
| S1 state tests | baseline, CAS, terminal retention, commit/failure | `session_mirror.rs` and `history_cache.rs` native tests |
| S2 state implementation | separate shared HistoryCache and session phases | `session_mirror.rs`, `history_cache.rs` |
| S3 ACP orchestration | cold load, retry, validation, cross-session concurrency | `bridge.rs` and `server.rs` native tests |
| S4 observer delivery | atomic baseline+overlay snapshot, suffix/reset, shared payload | `server.rs` subscriber, revision-gap and byte-accounting tests |
| S5 command API | CAS/idempotency and browser-local deferred queue dispatch | native append tests, `use-acp.test.ts`, Chromium queue cases |
| S6 browser projection | business snapshot/SSE rendering; no ACP lifecycle ownership | `use-acp.test.ts`, `state.test.ts`, Chromium reconnect cases |
| S7 removal | no browser raw-ACP transport or competing prompt admission registry | REST/SSE routes, protocol types and cleanup regressions |
| S8 release | Rust/TS/browser/transport/protocol/memory gates all green | run the suites in [testing.md](testing.md) against the release commit; CI enforces the gates |

## Removal ledger

Keep these retired behaviors out of the production path:

- dropping the completed overlay before the baseline commit;
- retaining only control updates from an attachment replay;
- requester-private load replay in `attachment_subscribers`;
- `ActiveRuntimeProjection::clear_turn_events` on PromptResponse;
- reconnect-driven session/load from `useAcp`;
- browser shadowing/ignoring canonical runtime state;
- completed intent IDs becoming immediately reusable;
- Bridge-owned unversioned queued prompt admission.

Epoch/incarnation isolation, semantic folding, live-resource registries, per-session operation
exclusion, bounded channels and canonical suffix-gap handling are retained and adapted.
