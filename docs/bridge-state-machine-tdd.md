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
|- history phase: Cold | Loading | Ready | Running | Reconciling | Blocked
|- live lifecycle: Attaching | Active | Closing | ClosingForDelete
|                  | Deleting | Closed | Uncertain
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
| Unobserved Active + Idle | continuous idle deadline expires | closing/closed | `session/close` when supported, otherwise local retirement |
| Unobserved, non-Idle | turn / operation / load / interaction / running terminal | working/awaiting | no retirement; start a fresh interval after work settles |
| any live | close/delete/shutdown | closing/closed | capability-gated lifecycle I/O |

Mode/config controls and close may coexist with a prompt. Competing controls, attachment, fork,
close and delete remain mutually exclusive; fork/delete still require no running prompt.
Different sessions may progress concurrently under request admission and per-session ordering.

The [history workflow lifecycle proposal](history-sync-lifecycle.md) extends work accounting to
cold materialization and optional-history retry waits. A workflow owner is distinct from both a
single load attempt and exclusive business admission; this extension is pending implementation.

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

An HTTP admission response timeout is not an Agent turn failure. The browser must keep a submitted
prompt pending until a fresh authoritative view resolves its outcome, reject pre-timeout refresh
responses as evidence for that resolution, and never automatically replay the mutation. A preflight
GET timeout has not submitted the prompt and remains a retryable local failure. The finite HTTP
request deadlines do not bound ACP turn execution; see [HTTP timeouts](http-request-timeouts.md).

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

Session-specific observer absence starts the CLI-configured retirement interval only while the
session is idle. Returning observers or live work cancel the countdown; settling back to idle
starts a full new interval. The default is 300 seconds; negatives disable, zero attempts immediate
retirement. Running turns, operations, history loads, live terminals and pending interactions
prevent retirement. Without Agent close support, expiry releases the local allocation only.
A refused automatic close preserves the session and must not rearm itself through its own
Running-to-Idle transition; a fresh observation/absence cycle permits another attempt. Uncertain
outcomes remain explicit. Never-observed materialized sessions count; list rows do not. Queued
timers and late responses must respect incarnation identity.

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
Healthy subscribers receive ordered deltas. The memory-retention acceptance target permits a slow
session subscriber's reconstructible presentation backlog to be replaced by a coalesced latest
reset, followed by an owner-fenced view refresh. Final state must equal uninterrupted observation.
Retirement and failures that the current snapshot cannot reconstruct remain reliable semantic
events; a reset cannot discard their outcome or retry prompt. Baseline payload is shared/chunked
rather than cloned into every subscriber queue. Coalescing never cancels Agent work or evicts an
otherwise connected observer. The retention gates pass after `802c20b`; see
[memory acceptance](runtime-memory-retention-tests.md) for the historical Red baseline and
[memory efficiency](runtime-memory-efficiency.md) for the subsequent implementation and Green gates.

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
- ACP ingress, session dispatch and replay apply valid input in order without arbitrary
  conversation quotas, burst rejection or dropping unapplied records.
- The internal publication journal is temporary: successful ordered handoff releases its
  contiguous published prefix; a failed handoff never advances the successful watermark.
  Old overlay versions and a second raw conversation journal must not survive delivery.
- Session subscriber presentation data may be coalesced into a latest reset when complete state
  can be recovered. Non-reconstructible semantic events remain reliable. Byte ledgers measure
  retained payload and release it on consumption, replacement or teardown; they are not a reason
  to truncate valid conversation content. The first retention gates pass on `802c20b`; the
  [follow-up efficiency gates](runtime-memory-efficiency.md) now pass controlled consumer handoff races.
- After a consumer captures a state payload, a new publication must be in that delivery or a
  subsequent deliverable event; a still-live slot cannot silently absorb an update already missed
  by that consumer. Queue accounting must settle exactly once at the same ownership boundaries.
- Control-only publication must not scale with existing turn body size. Explicit partial changes
  preserve omitted fields; a full upsert continues to mean replacement.
- Small changes must not allocate in proportion to unrelated retained body size; held snapshots
  stay immutable and invalid changes leave revision/state/accounting untouched.
- Browser diagnostic source arrays keep the latest event per logical entity/chunk. Current
  rawInput/rawOutput, complete message text, permissions and reliable errors remain business data.
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
- Continuous unobserved idle intervals recycle sessions through negotiated close or local retirement.
- Observed sessions remain materialized; work cancels the countdown and completion starts a full interval.
- Close/delete/generation shutdown drops baselines, candidates and retry tasks.
- No history payload is persisted.

## Proof gates

### Cancellation intent presentation (2026-09-26)

`session_view_exposes_cancel_intent_before_completion_and_keeps_late_updates` initially failed
because the public active turn omitted cancellation intent. The correction projects the existing
execution flag in `session_view_value`, preserving the canonical lifecycle and avoiding another
turn payload serialization. The same test now passes: cancellation remains nonterminal, an older
snapshot and another session stay unchanged, late tool results survive, and a new turn resets the
flag. Browser tests follow this native gate. HTTP/SSE and WebSocket smoke fixtures hold the final
prompt response until the public view exposes `cancelRequested: true` with a null terminal result.

### Existing migration gates

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

## Idle-retirement regression gate

Run `npm run test:idle-retirement` before implementing the fixes reviewed against
`a5b07ff11ccdc8e338d222a578854ab2cb2158d6`. These native tests live under
`src/bridge/unobserved_tests/` and exercise the real bridge coordinator and ACP protocol over an
in-memory transport. Tokio's paused clock advances the deadlines; the tests do not launch Node,
open network sockets, use external fixtures or wait for wall-clock sleeps.

| Case | Trigger | Required behavior |
| --- | --- | --- |
| `idle_retirement_refused_immediate_close_does_not_spin` | timeout 0; Agent rejects close | one attempt; no immediate retry loop |
| `idle_retirement_refused_close_does_not_restart_the_timeout` | timeout 30s; Agent rejects close; wait another 90s | no self-rearming close request |
| `idle_retirement_resume_history_completion_starts_a_full_interval` | resume followed by a 15s empty history load; timeout 10s | no close during load; a complete 10s idle interval after completion; eventual close |
| `idle_retirement_fork_history_completion_starts_a_full_interval` | same delayed load for a fork target; source remains observed | target gets its own full idle interval and eventually closes |
| `idle_retirement_short_resume_history_completion_starts_a_full_interval` | resume followed by a 4s empty history load; timeout 10s | cancel the old deadline before it expires; wait a complete 10s after loading, then close |
| `idle_retirement_short_fork_history_completion_starts_a_full_interval` | same short load for a fork target; source remains observed | target gets a complete 10s idle interval even when loading ends before the old deadline |
| Late retired-session update during creation | locally retire old ID; batch old update, new updates and new response | preserve the new session's exact early replay and canonical controls |
| Successful close | Agent acknowledges automatic close | one retirement and no repeated close |
| Refusal followed by reobservation | observe refused session, submit a turn, then leave | session remains usable; observation blocks retirement; fresh absence rearms |
| Creation without stale updates | same creation replay without the old ID notification | normal early replay and canonical controls remain intact |
| Explicit rematerialization | reopen the retired ID and complete authoritative load | new incarnation with the loaded history; retirement filtering must not ban the ID |

The recorded Red baseline on `a5b07ff` is **5 failures and 4 passes**: both refusal cases,
both optional-load cases and the late-update case fail at their behavior assertions; the four
control cases pass. The existing 474 Rust tests pass when this new module is excluded, and
`npm run check` passes. No production fix is included in this baseline.

On `9485496`, the original nine cases pass, but the two added short-load cases fail:
both resume and fork close after only **6s of idle**, rather than the required **10s**.
The expanded gate therefore records **2 failures and 9 passes**. These cases preserve the
original assertions and vary only the load duration; they catch a fix that cancels the old
countdown only when its deadline happens to expire during loading. That follow-up added tests
and documentation only; the production correction followed in `a65d5d1`.

On `a65d5d1`, all 11 existing cases pass after `begin_load` cancels the previous idle countdown.
That result covers the listed scenarios, not the entire retry workflow. A later review confirmed
that an unobserved fork target can still retire during a 2s load backoff with a 1s idle timeout
and a concurrent catalog refresh. The follow-up Red/Green gates are W1–W8 in
[history-sync-lifecycle.md](history-sync-lifecycle.md#8-tdd-合并准入). The 31 follow-up behavior
tests now live in `src/bridge/unobserved_tests/workflow*.rs`; they extend the gate to 42 cases
and are separate from that historical 11-case green result. `0857250` subsequently implemented
the workflow owner, retry protection, business-operation handoff, and identity-checked cleanup.
All 42 cases now pass; the Red records below describe the earlier test-only baseline.

Keep the defect cases as ordinary failing tests during the Red phase: do not ignore them, mark
them `should_panic`, weaken their assertions or make the gate accept a nonzero exit status.
The Green phase requires every case above to pass with the same assertions. A fix must preserve
both the negative checks (no duplicate close or lost replay) and the positive checks (eventual
retirement, usable sessions and explicit rematerialization). Then run `npm run test:rust` and
`npm run check`, followed by the relevant transport/API suites before browser projection checks.

## History-workflow Red baseline

Against unchanged production code at `a65d5d1`, the 31 workflow tests record **25 passes and
6 failures**. Together with the original 11 tests, `npm run test:idle-retirement` records
**36 passes and 6 failures**. The failures are behavior assertions, not build failures or
unanswered fixture requests:

| Failing test | Observed behavior |
| --- | --- |
| `history_workflow_retry_backoff_blocks_agent_close` | `session/close` arrives during the target's 2s retry wait with a 1s idle timeout |
| `history_workflow_retry_backoff_blocks_local_retirement` | without close support, `bridge/session_retired` reports `unobserved` during the same retry wait |
| `history_workflow_completed_prompt_supersedes_backoff_without_losing_turn_boundary` | the old flow loads after the new prompt completed; faithful message replay still clears the retained turn outcome |
| `history_workflow_completed_control_supersedes_backoff_without_reloading` | the old flow loads after the new control completed and replaces the history revision |
| `idle_retirement_workflow_explicit_reload_supersedes_same_incarnation_retry` | the old flow loads again after a successful explicit reload |
| `idle_retirement_workflow_old_fork_cannot_finish_replacement_fork_scope` | a second fork using the retired ID receives close instead of its next retry; successor workflow protection is missing |

The full Rust run records **510 passes and these same 6 failures**: all **485 pre-existing
tests pass**. `npm run check` passes type checking, all **318 frontend/shared tests**, and the
client build. Formatting and diff checks pass. Integration transport/API/browser validation
remains a Green-phase requirement after the production fix; this Red test change does not claim
those release gates were rerun.

See the [W1–W8 coverage mapping](history-sync-lifecycle.md#可执行覆盖映射) for the exact modules
and current cleanup/handoff coverage. The six defect cases and 25 control cases remain executable
with their original behavior requirements and now pass together.

## Current merge acceptance

The [successful CI run for `5f66a6a`](https://github.com/tf4fun/attyd/actions/runs/35682667713)
on September 22, 2026 passed all 14 applicable jobs: dependency audits, release metadata,
frontend checks/build, backend integrations, browser tests, coverage, and seven native release
targets. It includes 588 Rust tests, 456 frontend/shared tests, and 94 browser tests;
instrumented Rust line coverage is 93.39%, above the unchanged 85% gate. The release publication
job was correctly skipped because this was a branch build, not a version tag.

These results supersede the historical Red counts above. Long-running workload RSS and actual
browser heap measurements remain deployment follow-ups, as described in the memory ledgers;
the passing gates validate behavior, ownership, and retained payloads rather than a fixed RSS limit.

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
