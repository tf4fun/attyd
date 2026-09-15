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
`- connection-scoped live resources and diagnostics

SessionRuntime                         HistoryCache (separate allocation)
|- incarnation and view revision       `- SessionKey -> Arc<HistorySnapshot>
|- phase: Cold | Loading | Ready
|         | Running | Reconciling
|         | Blocked | Closing | Closed
|- opaque historyRevision
|- one ActiveOverlay
|- one LoadAttempt metadata             LoadTransaction (separate allocation)
|- live resources                       `- one byte-accounted replay candidate
`- exact intent result map
```

Complete history payload must not be placed inside `SessionRuntime`, runtime deltas or raw debug
journals. Those structures are cloned and serialized frequently. They carry only baseline identity
and revision metadata; observers access the shared snapshot through the history store.

## Transition ledger

| From | Intent/event | To | Agent I/O |
| --- | --- | --- | --- |
| Cold | first observer | Loading | one joined `session/load`, or negotiated resume fallback |
| Loading | valid response | Ready | none |
| Loading | retryable failure | Loading | one delayed retry after termination |
| Loading | deterministic incompatibility | Blocked | none |
| Ready | valid append CAS | Running | one `session/prompt` |
| Ready | stale CAS/duplicate slot | Ready | none |
| Running | observer churn | Running | none |
| Running | PromptResponse | Reconciling | none |
| Reconciling | atomic local promotion | Ready | none |
| Active, including Running | configured absence deadline expires | closing/closed | capability-gated `session/close` |
| any live | close/delete/shutdown | closing/closed | capability-gated lifecycle I/O |

Mode/config controls and close may coexist with a prompt. Competing controls, attachment, fork,
close and delete remain mutually exclusive; fork/delete still require no running prompt.
Different sessions may progress concurrently under request admission and per-session ordering.

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

Session-specific observer absence starts the CLI-configured close interval. Return cancels queued
admission; output/completion do not reset it. The default is -1 (disabled); negatives disable, zero
attempts immediate close. Expiry may close running work. Unsupported/refused close keeps state,
and uncertain outcomes remain explicit. Never-observed materialized sessions count; list rows do
not. Queued timers and late prompt responses must respect incarnation identity.

Successful resume/fork is retained even if optional load fails. Use Agent replay (including empty),
then available memory context, then a missing-history notice. Cached source context is never sent
to the Agent or treated as a live branch subscription.

## Observer contract

Opening another page never starts load for a materialized Running or Reconciling session. Subscriber
registration and snapshot cut are one operation. A subscriber sees either:

- a full reset containing baseline + overlay + live resources; or
- deltas whose first `fromRevision` exactly equals the subscriber cursor.

A revision gap, expired suffix or ordering error within the same owner forces reset. Recovery of
an old incarnation in the same epoch returns an explicit retirement conflict; it cannot
cold-materialize the same session ID. An epoch change instead returns `bridge_replaced` and is
handled by global connection recovery: preserve the route and draft, discard the old observation,
and restore Agent-owned history on the new connection. `/api/v1/runtime` exposes the canonical
epoch; a numeric generation counter alone does not identify a host restart. View refreshes and SSE
subscriptions carry `expectedEpoch` / `expectedIncarnation`; native SSE retries also carry their
`Last-Event-ID`. A confirmed close/delete publishes `bridge/session_retired` before ending the
observation stream. A successful close preceding a failed delete still retires the observation.
Slow subscribers retain their ordered backlog until they consume it or disconnect. Baseline
payload is shared/chunked rather than cloned into every subscriber queue. Queue growth never
cancels Agent work or evicts an otherwise connected observer.

Directory management is global. Cold deletion reserves only its catalog ID, with no session
runtime allocation. Deleting an existing runtime coordinates its lifecycle and resource cleanup.
List operations serialize Agent list requests without an admission count limit;
waiting callers release the execution turn so other global operations can proceed. New/fork/delete advance
the catalog revision and publish a lightweight invalidation. List responses return `catalogRevision`;
business pagination sends it as `expectedCatalogRevision`. A changed revision rejects the old
page before cache installation or response publication, and the browser restarts from page one.

The browser keeps projection identity, not lifecycle authority. Pending deletion preserves the
current view until a confirmed lifecycle result. Retirement and management completions only
remove matching owner projections; delayed HTTP replies cannot discard a reopened incarnation.

## Authority and compatibility

The following are explicit compatibility rules, not heuristics:

- `loadSession` is used for cold recovery, never routine post-turn synchronization;
- without load, only new/already materialized sessions are recoverable and history ends with the
  bridge process;
- valid updates for a materialized session may arrive outside a prompt; accept them without
  inventing a turn boundary and keep message/tool identities session-scoped;
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
- ACP ingress, session dispatch, bridge publication, subscriber delivery and temporary replay
  queues have no item-count or cumulative byte cap. Content is processed in order, without
  rejecting bursts, cancelling the connection or dropping earlier records. Byte ledgers measure
  retained data and release it on consumption/teardown; they are not admission limits.
- Wire values and live resources retain their independent validation and lifecycle rules.
- Load/resume registers one replay candidate before sending its RPC. Historical notifications
  validate and fold directly into that separate allocation; they consume no live ingress items,
  session execution tickets. Format, ownership and lifecycle validation still apply, with no
  record-size or entity-count cap. Candidate bytes are accounted independently of transport buffers.
- The matching response seals the candidate at the ordered wire boundary. The session completion
  ticket checks epoch/incarnation/attempt and publishes the baseline atomically; following updates
  use the live FIFO and cannot pass that commit. Failure/retirement discards the candidate, and a
  retained writer cannot mutate a replacement attempt. Requests for permission/elicitation remain
  live requests; replayed tool records never allocate responders.
- Replay folding updates only the relevant history slot and byte count, without cloning or
  serializing all previous turns per record. Control patches fold into final control state.
  Unknown pre-creation IDs must still correspond to outstanding creation attempts.
- Catalog requests and other sessions can progress while a load response is outstanding. The
  transport hook does not wait for a session execution ticket or a human interaction response.
- Ingress faults that terminate a connection are published in global scope so runtime recovery
  and global SSE retain the original failure instead of exposing only interrupted HTTP requests.
- Continuous unobserved intervals recycle sessions through negotiated close, including running work.
- Observed sessions remain materialized; attachment/control admission defers an expired close.
- Close/delete/generation shutdown drops baselines, candidates and retry tasks.
- No history payload is persisted.

## Proof gates

| Gate | Required evidence | Current implementation / verification |
| --- | --- | --- |
| S0 specification | authority/state/API/memory contract and adversarial test plan | this ledger and the linked runtime documents |
| S1 state tests | baseline, CAS, terminal retention, commit/failure | `session_mirror.rs` and `history_cache.rs` native tests |
| S2 state implementation | separate shared HistoryCache and session phases | `session_mirror.rs`, `history_cache.rs` |
| S3 ACP orchestration | cold load, burst replay, retry, validation, cross-session concurrency | `bridge.rs` and `server.rs` native tests; `burst-load-agent.mjs` replays 10,050 updates exceeding 1 MB; catalog calls complete before the fixture releases the load response. `scheduling.rs` checks response cuts and live-queue isolation; `history_replay.rs` checks incremental folding, poisoning and retirement |
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
exclusion and canonical suffix-gap handling are retained and adapted. Delivery queues are
unbounded so valid bursts cannot be converted into connection failures.

## Lossless burst delivery regression

`tests/fixtures/burst-prompt-agent.mjs` sends 2,048 ordered Chinese/emoji fragments and the
prompt response in one write. `server::tests::live_burst_preserves_all_updates_for_a_slow_subscriber`
leaves session SSE unread until the turn is committed, then verifies both the canonical text and
every streamed fragment, the final `end_turn`, and the unchanged bridge generation. Unit tests
also cover large ingress/publication backlogs, FIFO completion delivery behind notifications,
round-robin progress while another session is busy, early creation replay and active replay
beyond the former queue limits.

The bridge explicitly observes the SDK's clean incoming EOF signal, drains accepted deliveries
without a deadline unless explicitly cancelled, and publishes `stopped`.
Shutdown skips Agent cancellation notifications after EOF so a broken pipe
cannot interrupt the remaining results. The `bridge::tests::stdio_clean_eof_*` regressions cover
idle exit, exit with a pending prompt, and exit immediately after either the full fragment burst
or a 9 MB message and its terminal response, with the event consumer left unread until shutdown.
The browser stops queued session refreshes as soon as the global connection becomes unavailable;
late query results cannot install an old view or start another GET on the stopped bridge.

Queues use available process memory. If producers permanently outpace consumers, retained memory
will grow until consumption, disconnection or lifecycle cleanup releases it; attyd does not impose
an arbitrary truncation threshold.


## No application-imposed resource quotas

attyd does not reject, truncate or evict valid content based on fixed message sizes, queue counts,
concurrency counts, replay/cache sizes, attachment sizes, file sizes, search result counts or prompt
history lengths. This applies through stdio NDJSON, WebSocket frames, HTTP bodies, ACP/MCP
registration, canonical state, subscriber delivery and browser projections. Error details are
forwarded completely. Queries and observation handshakes wait for completion, cancellation or
connection closure; retryable loads have no fixed retry count. Exact idempotency results remain
available for the session incarnation rather than falling back to a probabilistic filter.

Protocol field types, valid encoding, reference consistency, filesystem confinement, negotiated
capabilities, per-session operation ordering and explicit cancellation remain enforced. Agent-owned
`outputByteLimit`, file line ranges, elicitation schema constraints and terminal stop reasons retain
their meaning. Without an explicit `outputByteLimit`, terminal output is retained in full.
Lifecycle cleanup still releases retired resources, and shutdown/process cleanup has a grace period.
Search debouncing and the large-diff approximation select processing strategies without discarding
source content or search matches.

Regression coverage includes a 9 MB stdio update through a slow observer and committed history,
a 65 MiB WebSocket frame, a 6 MiB HTTP prompt with 65 blocks and delayed admission, more than
1 MB of terminal output without an explicit limit, and more than eight queued browser prompts.
