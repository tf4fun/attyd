# Bridge implementation and migration plan

This document owns implementation structure, migration order and test gates.
[简易 Agent 交互协议](protocol.md) owns the FE ↔ bridge contract;
[design rationale](client-api-redesign.md) explains its choices;
[protocol acceptance](protocol-acceptance.md) keeps the numbered behavior cases.
The [ACP source matrix](acp-bridge-matrix.md) defines the first-layer functionality
and retained-memory scope. Source coverage is not evidence of completed behavior.

Status: the experimental baseline is `experiment/protocol-v2`. Revision 3 of the
protocol is the target and its implementation is in progress; §7.5 records what is
merged. Stable `main` remains v1 at `v0.3.0-rc.5`. Section 7 records current work: complete ACP behavior
and authoritative memory first, then finish client synchronization, verification
and repair. Sections 1–4 identify reusable implementation, not proof that the
baseline meets either milestone. Lifecycle/concurrency work follows the
[TDD ledger](bridge-state-machine-tdd.md).

## 1. Starting point

Reuse the ordinary ACP client, ordered ingress/coordinator, session registry and
host services. Repair their state and lifecycle gaps incrementally; do not add a
parallel bridge or restore the removed public owner/intent/request-history model.

| Existing part | Path | Role in the current work |
| --- | --- | --- |
| Ordered ACP dispatch and completion | `src/ordered_ingress.rs`, `src/bridge/scheduling.rs`, `src/bridge/coordinator.rs` | Actual request/session routing and ordered state transitions |
| Session ownership | `src/session_registry.rs`, `src/session_state.rs`, `src/session_resources.rs` | One installed entry; business state and non-cloneable resources share its lifetime |
| Load candidate | `src/history_cache.rs`, `src/history_replay.rs` | Private attempt-local replay; replace installed history only on successful completion |
| Control reduction | `src/protocol/controls.rs` | Preserve ordering; extend source metadata in step 18 |
| Publication and generic diff | `src/bridge/publication.rs`, `src/protocol/project.rs`, `src/protocol/publish.rs`, `src/protocol/diff.rs` | Commit, pure projection and incremental delivery |
| Subscription Hub | `src/protocol/hub.rs`, `src/bridge/streams.rs` | Atomic cut, queue coalescing and delivery |
| Generated types | `src/protocol/typescript.rs`, `shared/protocol.ts` | Keep Rust DTOs and browser types aligned |

The full completed migration and abandoned-draft details are linked in §5 rather
than maintained as a second implementation plan.

## 2. Reusable implementation

```text
ACP → ordered coordinator → authoritative state transition → commit → Hub → clients
                                                            │
                                                    published read cache
```

The commit and Hub remain useful while the retained content changes. Their current
success does not establish full ACP fidelity or client-side verification.

### 2.1 Bridge state

`SessionRegistry` owns each `SessionEntry`; `SessionState` owns its installed
history and execution state, and `SessionResources` owns responders and other
non-cloneable resources. `HistoryCache` holds only unfinished load candidates.
The connection has its Agent state and global/catalog work; lookup maps and entity
indexes must remain indexes of those facts, not independently updated authority.

Existing operation slots and scope occupation implement the current admission
baseline. They do not own all ACP interactions. Section 7.1 defines the required
content, independent resource lifecycles and removal of operation-bound interaction
cleanup. Holding is derived from actual work/resources and subscribers, not stored
as another business state.

### 2.2 Projection

The current pure entry points in `src/protocol/project.rs` are:

- `project_session(&SessionRegistry, session_id: &str) -> Option<SessionDocument>`;
- `project_global(&SessionRegistry, &AgentInfo) -> GlobalLive`;
- `project_catalog(&SessionRegistry, revision: u64) -> CatalogLive`.

They produce a displayable representation of retained business state. Publication
may share immutable history and cache unchanged item projections; it cannot be the
only owner of attachment bytes or frozen terminal output. Rebuilding that cache
must preserve content and resource reads. The target replaces the running
operation's process projection with the session tail (§7.1), while retaining the
identity-diff technique.

### 2.3 Commit step

`BridgeLock`'s mutable guard runs one commit when released, on every return path:

1. Compare a session's retained log with the last published one. Shared unchanged
   content needs no work; an append emits the tail; a replacement or entity rewrite
   emits from the first changed entry and updates subsequent chain identities.
2. Project live state and diff against its published value. Reuse pointer identity
   for unchanged content, value comparison for small fields, and `trim` plus
   `append` for bounded terminal output.
3. Store the published cut and enqueue its events under the same bridge lock. A
   scope's related log and live changes are published together.

`src/protocol/canon.rs` remains the home for the timeline reduction, attachment
normalization/resolution and chain encoding. Step 11 replaces its message-only
semantics; live, replay and settled content use one set of rules (§7.1). A content
hash identifies retained data; computing it in the bridge does not prove that
clients verify received bodies.

State transitions decide what is retained. The publisher observes and emits them;
its queue or cache budget cannot justify deleting authoritative ACP content.

### 2.4 Hub

The existing Hub owns delivery only. Reuse these invariants while the second-stage
wire design is reviewed:

- **Cut.** Register a subscription under the bridge lock and build its alignment
  from one published cut. Later commit events enter its queue in that critical
  section, so none fall between the cut and following delivery.
- **Baseline and queued tail.** For each subscribed session scope, keep two log
  positions. The baseline is the client's declared head, advanced only when an
  event leaves the replaceable queue for the stream's ordered output. The queued
  tail is the last queued event's target, or the baseline when the queue is empty.
  Ordinary increments are computed at enqueue time from the queued tail; each
  follows the preceding queued increment. A resync is computed from the baseline.
  A queued event never advances the delivery baseline.
- **Coalescing.** At the existing 1 MiB queue budget, replace a queued suffix for
  each extant scope with one alignment from the baseline. Its target becomes the
  queued tail. Preserve a queued `end`, with nothing following for that scope.
- **Heartbeats.** The baseline sends an SSE comment after 15 seconds of idleness.
- **Holding.** A session subscription is a lease while its stream is open. Loading
  an unheld session follows admission; occupation delays it. Global/catalog
  subscriptions alone do not hold sessions.
- **Connection handoff.** Publication and Hub outlive an Agent connection.
  `WaitingSession` transfers retained state to its replacement; it is not another
  mutable session authority. The current recovery path is driven by followed
  sessions or terminal-login completion. Step 14 must also provide explicit
  zero-session recovery and correct retained content during handoff.

The baseline has no retained delivery log: reconnect aligns from published state.
Per-event tests check the baseline and queued tail independently. This delivery
model is not a settled guarantee of received-body or mutable-state verification;
that belongs to §7.4.

### 2.5 Execution and completion

Admission, registration and the initial state transition happen under the bridge
guard. ACP sends and waits occur outside it; actual responses and local failures
return through the ordered completion path. A held HTTP response is only a channel
for the initiating client, not another result ledger; dropping it must not undo an
admitted operation.

Reuse `OrderedIngress.pending` and the exact request/attempt association. Preserve
short-lived completion context until the coordinator applies it. A wire response
may already have removed the ingress entry, so scanning that map alone cannot
end a child interaction or release a session before its ordered completion.

Load replay remains a private candidate until its matching successful response;
a failed or stale attempt cannot replace installed content. Step 14 completes
history knowledge, fork history per memory model §6.3 and recovery. Controls, close and
interactions follow the target transitions in §7.1, replacing the baseline's
operation-bound cleanup. Resource actions consume the actual responder at most
once; unknown-result business writes are not automatically replayed.

### 2.6 Services and resource reads

Keep filesystem, terminal, authentication and MCP execution paths. Terminal input
and resize run outside the bridge guard; input uses the terminal's ordered writer,
so stdin backpressure cannot hold the lock needed by output delivery. A URL flow is
addressed by its bridge interaction registration; a delayed action on an old flow
must not reach a new one that reuses the Agent's elicitation ID.

Workspace context reads run outside the actor using the held session's resolved
workspace and existing path checks. Attachment reads may use the published cut,
but retained bridge content must also own the original block. Reads hold, load and
renew nothing. The handle/output and reference-release contracts are in §7.1.

## 3. Types

Rust DTOs define the bridge's documents, events, operations, resource actions,
results and errors. `shared/protocol.ts` is generated and checked for staleness.
HTTP body types are in `src/protocol/wire.rs`; execution and responder types stay
internal. ACP values reference the pinned SDK types rather than a separately
maintained approximation. Representation gaps from the source matrix are completed
with their owning state before the corresponding migration step closes.

## 4. Invariants and gates

| Invariant | Required property |
| --- | --- |
| I1 single write path | Only the commit step publishes documents and events |
| I2 per-event consistency | Each installed event produces a coherent published cut; unknown history stays explicitly unconfirmed |
| I3 exact cut | Alignment and subsequent ordered events have no gap or duplicate effect |
| I4 coalescing safety | Replacing a queued suffix with alignment preserves its final meaning; scope end survives |
| I5 content identity | Equal reduction input yields equal items regardless of eligible chunk splitting; entries differing in published content are not treated as equal |
| I6 admission | Admission is serialized; the current contract's operation/resource preconditions decide execution |
| I7 holding | Needed sessions remain held; automatic release requires load/resume capability |
| I9 fidelity | Accepted timeline content remains represented by its specified reduction until legitimate replacement or release; no display choice removes it |

I8 was the withdrawn process-removal rule. It is not a current gate; its historical
assertions change when their owning migration step changes behavior. Existing
convergence verifies publication consistency, not the second-stage verification
requirements of protocol §12.

### 4.1 Convergence gate

Keep the scripted-Agent harness and compare after every event with the published
cut it represents, not a newer state reached concurrently. Exercise normal delivery,
lag with random coalescing, reconnect at random cuts, and paused consumption with a
queued-but-unsent change that is coalesced. Check both Hub positions; an unchanged
baseline must not be mistaken for the queued tail.

Run applicable scenarios at the registry and through the real coordinator/ACP
transport: controls, streamed/folded turns, late rewrites, replay replacement,
terminals, URL flows, permissions, deletion, release and Agent restart. Retain the
negative test that mutates state without committing. Direct ACP/memory assertions
in §7.3 are independent of the projector and supplement this gate.

### 4.2 Acceptance

[Cases 1–37](protocol-acceptance.md) retain their original numbers and subcases.
Each lands with the change that makes it reachable; client cases run again at the
client boundary. The [source matrix](acp-bridge-matrix.md) adds method/field/domain
coverage, and [protocol §12](protocol.md#12-synchronization-design-requirements)
adds independent synchronization-verification failures.

The [coverage ledger](acp-coverage.md) links a capability to its actual execution,
authoritative state, direct reply/result and lifecycle test. Document delivery and
client repair are checked separately; neither chain can substitute for the other.

## 5. Completed migration history

The removal of the abandoned draft, implementation of revision 1, migration of the
browser and removal of `/api/v1` are completed history. Their steps and code details
remain at the fixed baseline
[`991b053`](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/state-sync.md#5-migration).
They are not instructions to reproduce the old semantics.

That migration removed session incarnation-based isolation. Ordinary JSON-RPC
correlation, request/attempt-local completion and rollback, resource IDs/responders,
and connection shutdown remain necessary. A session ID alone cannot justify
applying an old local completion to a replacement entry; §7 resolves this through
the real outstanding request, without restoring a public owner/CAS/intent system.

## 6. Open implementation choices

- **Release delay.** Keep `--session-unobserved-timeout` until measurements justify
  a change. Releasing a resume-only session also changes which history the bridge
  can recover; it is not exclusively a performance choice.
- **Alignment optimization.** Multiple matching sample points are a possible
  optimization only after the second-stage correctness/verification contract is
  settled and full replacement is measurably costly. They are not required now.

## 7. Fidelity migration (revision 2)

Status: **the first layer is implemented in bridge memory except where §7.5 lists a gap; the second layer is being built beside the revision 2 engine, to be cut over at once.** (PR #53 was superseded by the flat timeline.)
The current milestone is **ACP → bridge functionality and displayable memory**,
defined in the [first-layer contract](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)
and specified by the [bridge memory model](bridge-memory-model.md) (proposed
2026-10-21): load replay and live prompts build one flat entry list with one reducer.
The [audit](acp-fidelity-audit.md) distinguishes runtime defects, shared v1
limitations and open rules; the [release parity matrix](release-parity.md) compares
the stable rc.5 baseline and keeps older release evidence separately. A draft PR or
a target rule is not implemented coverage.

[Protocol revision 2](protocol.md) supplies the target content model. The existing
commit step, Hub and alignment machinery are reused; that does not by itself prove
that clients verify and repair all replicated content. That is the separate second
milestone (§7.4). The existing convergence suites remain regression checks during
first-layer work. Each implementation step lands with its acceptance cases
([protocol acceptance](protocol-acceptance.md)). Tests
that assert revision 1's removals (`turn-collapse.pw.ts`, `lazy-process.pw.ts`, the
release case in `acp-ui.pw.ts`, `terminals_roll_their_bounded_tail_with_trim_and_append_and_leave_on_release`,
the I8 assertions) change in the step that changes the rule they encode, to assert
the retained content instead; no other test is weakened.

### 7.0 First milestone: ACP and bridge memory

The bridge remains an ordinary ACP client, with no persistence layer. Its one
authoritative memory model must retain the ACP content needed for presentation,
independently of whether a browser is connected or a card is expanded. This is a
logical state contract, not a requirement to combine every store into one struct or
keep a second raw event archive.

**Implementation objective (2026-10-20).** Execute the supported ACP capabilities
in the [source matrix](acp-bridge-matrix.md), and keep their complete semantic state
in bridge memory. A method is not complete merely because its RPC succeeds: its
received content, current state, result and referenced resources must remain
available for their proper lifetimes. Optional features marked as excluded remain
explicit boundaries; this milestone does not silently enable a newer ACP version.
The protocol between the bridge and Agent remains ordinary ACP. Browser replicas
do not supply missing Agent history or determine the bridge's facts.

Implementation order:

1. **Finish the content foundation (11, PR #53).** Complete the reducer, items,
   message boundaries, metadata, original attachments and successful/cancelled
   prompt results. Resolve the existing regression cases. Keep the generated types
   and minimum client adaptation needed to leave the application usable.
2. **Complete content and resource lifecycles (12–16, 18).** Settle failed and
   interrupted prompts; accept idle and late updates; state history completeness
   honestly; retain referenced terminal output; manage interactions by ACP scope;
   restore controls during generation and explicit close. Include controls metadata
   in 18, form-schema and request-ID support in 16, and no-session connection
   recovery plus current catalog/auth results in 14. These are incremental repairs
   to the current owners and execution paths, not a second bridge implementation.
3. **Close first-layer coverage (17, alongside each step).** Verify the pinned ACP
   methods, notifications, responses and cancellation directly, as well as the
   resulting memory state. Existing filesystem, terminal, authentication and MCP
   services are retained, not rebuilt as a new request-history system.

The outstanding rules in the audit's §9 are settled by the
[bridge memory model](bridge-memory-model.md) and must be reviewed before the affected
path is implemented: fork history (§6.3: the target never inherits source entries),
resources and notifications before a new session's ID is installed (§9: one
installation at a time, staged), and the destination of a late control response (§9:
session generations).
State the required isolation using actual pending requests; no public owner token,
intent table or retained request log is needed. An explicit recovery path must work
without an open session, but may not resend a business command whose result is
unknown.

Diagnostics, export and UI redesign are subsequent presentation work (19–20), not
proof that this milestone is complete. Necessary client adaptations still accompany
each changed operation or resource so an intermediate version remains usable.

### 7.1 Design

- **One reducer.** `canon` becomes the timeline reducer of protocol §3.2: it takes
  session updates and gives items, and the same function serves the tail, a settle
  point and a replay. The active-turn fold (`fold_shared_active_turn_update`) and the
  replay builder (`history_replay::HistoryBuilder`) already fold tool calls, plans and
  text chunks; they become this reducer, implementing the three rules of protocol
  §3.2 (identity, patch, fragments): message items by role and `messageId`, one item
  per `toolCallId`, `planId` and `compactionId` in the session, the tool state as a
  `ToolCallUpdate` patched by the field rule of `ToolCall::update` (a first sparse
  update or permission keeps only its fields; today's fold into a `ToolCall` would
  need an invented `title`), the compaction's three-state patch, and chunks with their own `_meta`.
  An index from entity ID to its entry (or the tail) makes a late update cheap to
  place.
- **Tail instead of process.** The running prompt's `SharedTurnUpdates` becomes the
  session's tail, which also exists between prompts. `project_session` projects it as
  `live.tail`; the identity diff that served `op.process` serves it.
- **Settle points in the commit step.** The transitions that end a prompt, admit a
  prompt, end the connection, complete a load, or place an update in a settled entry
  change the `TurnLog`; the commit step already turns an appended or rebuilt
  `TurnLog` into a `LogChange` from the first changed index (§2.3). A rewritten entry
  is a rebuilt log whose common prefix ends before it.
- **Attachments.** The table keeps blocks as received, keyed by the hash of the whole
  block. References from both the log and live content keep them available; replacing
  tool content or clearing a compaction summary releases only blocks with no remaining
  reference. The retention rule must be tested, not assumed from the current table.
- **Interactions.** Pending permissions and elicitations move from the operation slot
  to the scope's `live.interactions`, routed by their ACP scope (protocol §4.4). The
  bridge reuses `OrderedIngress.pending` and its ordered completion path to associate
  an elicitation with the actual outstanding JSON-RPC request. Capture the request
  identity and scope at ingress; apply parent completion in coordinator order, not
  by scanning a table after a later wire response has already removed its entry.
  Queued completion context lasts only until that completion is handled. Do not add
  an independent pending-request authority or a completed-request table. A response
  ends unanswered interactions that name that request. For a session being created,
  the association follows [memory model §9](bridge-memory-model.md#9-concurrency):
  `create` and `fork` share the global slot, so a resource or notification naming the
  not-yet-installed session belongs to the one running installation; with none running
  it is refused or dropped. That rule needs its direct test before the path is extended.
  The `answer` admission path
  becomes a resource action that resolves one interaction under the bridge guard.
  `agent_request` and its completion rule go.
- A parent request's response cancels only that request's interactions not yet
  answered and drops its outstanding entry; an accepted URL flow is keyed by its
  interaction and `elicitationId`, so it keeps waiting. The flow keeps no parent and
  no reason field.
- **Terminals.** The terminal service keeps the handle; the session document keeps
  the frozen output with `released: true` while the reducer's index says an item
  embeds it, also when the Agent connection ends for a session that stays held.
  Holding counts only terminals with a handle.
- **Actions.** `set_mode`, `set_config` and `close` leave the operation slot: they are
  admitted and registered under the bridge guard without occupying the session;
  sending and waiting happen outside the guard. Their HTTP response is held until
  the Agent answers and the coordinator applies the completion; `close` sets
  `closing` until then. Existing execution and transient completion state hold the
  session through that transition, including the interval after the wire response
  removes the ingress registration. The release timer must not depend only on
  scanning that registration map. A release sets `closing` while its own close is
  out. The v1 close
  path of stable rc.5 (`close_session`) is the behavioral reference, not code to restore.
- **Diagnostics (later proposal).** If step 19 is adopted, the MCP relay and Agent
  stderr callback can feed a bounded presentation through the commit step. Neither
  its presence nor a particular buffer shape is a first-layer requirement.
- **History status.** `SessionLog::Known` carries `complete` or `partial`;
  `adopt_session_log` sets `partial` instead of building a known empty log.
- **Controls.** Preserve the existing early-update → setup-response → later-update
  order, including acknowledgements. The state must also retain the setup's
  `modes._meta` and each control notification's outer metadata, without flattening
  unrelated sources or interpreting vendor keys. Protocol §4.5's current DTO is
  incomplete for that requirement; define its representation before implementing 18.
- **Other source containers.** The [source matrix](acp-bridge-matrix.md) also identifies
  notification-envelope, setup/catalog and current-result metadata losses. Decide
  their representation alongside the owning timeline, controls or current operation
  before closing its migration step. Preserve the semantic association; do not add
  an all-notification archive or a completed-request table. Direct acceptance also
  covers the declared form/ID/numeric domain limits, not merely happy-path values.

Nothing here adds persistence, vendor branches, or the earlier draft's intents,
request records or epochs.

#### Ownership and write path

Keep the existing ownership boundaries. The names below identify current code,
not new mandatory Rust types or browser DTOs.

| Owner | Authoritative facts and required change | Derived or temporary data |
| --- | --- | --- |
| `BridgeState` / connection | Initialization result, negotiated capabilities, authentication and connection state; current global/catalog work and its actual result/error | Capability/auth lookup maps are indexes of those facts, updated with their source; no independent truth or request history |
| `SessionRegistry` → `SessionEntry` | One installed session's lifecycle, cwd, history knowledge, timeline/tail, controls, interactions, URL flows and referenced display resources | Entity-ID indexes locate the same items. `SessionState` and `SessionResources` share ownership; no parallel session store |
| Pending request / `SessionResources` | Actual execution stage, responder and cancellation handle while work is outstanding | Existing ingress registration and short-lived completion identity; one reply consumes the original responder |
| `HistoryCache` load candidate | Private candidate and validation state for the current load attempt | Temporary replacement transaction, not another installed history. `SessionState.log` remains the only completed history; failure discards only the candidate |
| Terminal/filesystem/MCP services | Real process, file operation and MCP routing lifetimes; actual results and failures | Preserve referenced terminal output separately from its handle; fs/MCP execution does not automatically create chat history |
| `Publisher` / Hub | No independent ACP business facts | Rebuildable publication/diff caches and delivery queues. `WaitingSession` transfers retained state between connections; it does not create a second mutable owner |

An ACP input follows the existing ordered coordinator path: resolve its actual
request/session, validate against the negotiated contract, update the owning memory,
then commit its published representation. External I/O waits happen outside the
bridge guard; their completions re-enter the same ordered path. A publication or
subscriber failure cannot undo an ACP result that the bridge has already received.

Live, replay and settled updates must use one set of reduction rules. Replace the
content-reduction responsibilities currently spread across `TurnOverlay` updates,
`HistoryBuilder` and `TurnLog`; do not add a fourth independently evolving reducer.
Replay keeps a separate candidate because atomic replacement and rollback need it,
but uses the same content semantics. Presentation folding never changes this state.

`SessionState.operation` currently describes execution admission/stages, while
`SessionLiveState.op` describes the current published work. Maintain both in the
same transition, or derive the latter; do not let them separately decide success,
interaction cleanup or session lifetime. This does not require a preliminary
rewrite of the entire state machine.

Resource reads must still resolve the same content after discarding and rebuilding
the Publisher cache from retained bridge state. A cache is not the sole owner of
attachment bytes or a released terminal result. Use existing shared ownership and
content references for retention; no generic garbage-collection framework is
required. Referenced immutable values may be shared rather than copied per commit.

#### End and release contract

| Event | Retain in the authoritative memory while its owner remains | End or release |
| --- | --- | --- |
| Prompt succeeds, is refused or is cancelled | Received timeline and the complete actual PromptResponse | Prompt execution and its own completed RPC; no deletion of process content or unrelated session interactions |
| Prompt fails or its connection ends without a response | Received prefix and structured error, or an explicitly unknown/interrupted result | Pending execution; never invent a successful ACP stop reason or resend the prompt automatically |
| Another operation starts | Earlier turn content/results, current controls and independent scope resources | Replace only the current-work slot where the protocol permits it; no all-command history is added |
| Parent RPC completes | Any accepted URL flow and unrelated session-scoped interactions | That RPC's still-unanswered request-scoped interactions, then its transient correlation |
| `terminal/release` or connection teardown | Frozen output/truncation/known exit state still embedded in held content | ACP handle and associated live waits/process resources; frozen output is not running work |
| Content is replaced or removed | Still-referenced blocks and results; explicit removal state where the reducer requires it | Only resources with no remaining owner/reference; an old patch is not a permanent event record |
| Full load succeeds | Atomically installed replay with its established knowledge level | Previous replaced content and the matching candidate; following notifications use the installed state |
| Load fails; cold resume or partial fork occurs | Previously known content on failed replacement; honest unknown/partial status when no full replay exists | Failed attempt only; absence of replay never manufactures complete empty history |
| Session/document is released | Only facts still owned elsewhere, such as an independently retained fork | Its responders, handles and unreferenced content; a disconnected FE alone does not cancel active ACP work |
| Bridge process exits | Nothing is promised to survive as bridge state | All runtime memory/handles. A new process learns content from the Agent according to its capabilities |

No database, session-state file, WAL, recovery journal or browser backup is added.
Reading launch configuration, executing ACP `fs/write_text_file`, or the Agent's
own storage does not constitute bridge-state persistence; those existing client
services continue to work. An Agent connection restart within the same process is
different from a bridge process restart: memory still held by the bridge can remain,
but expired execution handles cannot become live again.

### 7.2 Steps

| Step | Work | Removes | Acceptance (cases 1–37) |
| --- | --- | --- | --- |
| 10 | Docs: this revision, the audit, the release parity matrix, the coverage ledger | — | review (done) |
| 11 | One timeline reducer over a flat entry list ([memory model](bridge-memory-model.md) §3–§5), shared by load replay and live prompts, with the prompt echo rule and per-prompt outcomes; turns derived for the wire. Gate: the live/replay equivalence test (§4.3). Also: items, `end`, message boundaries, block/entity/notification-source metadata, original attachments, hash over the entry; live commits and replay through it; complete PromptResponse for successful/cancelled turns; validate numeric-domain limits against the locked schema. Generated types and minimum item rendering accompany the change | D1, D2, D3, D6, D7; source matrix content fields | 1, 13, 19, 20, 25, 26, 27, 30, 31, 36; content matrix CN/CB/TC/PL/EN cases |
| 12 | Settling of failed and interrupted prompts | D4 | 2, 22 |
| 13 | The tail between prompts; settle at prompt start and connection end; late updates rewrite their entry; the convergence gate gains rewrite and idle scenarios | D5 | 4, 21 |
| 14 | Honest load/resume/fork knowledge and attempt-local rollback; reattach by load on a new connection; fork per [memory model §6.3](bridge-memory-model.md#63-fork): the target never inherits source entries or resources, loads when advertised (`complete`), else keeps only its own staged and later notifications (`partial`); create/fork serialized through `global` with staged early notifications (§9); zero-session Agent recovery; retain current catalog pages/cursors and setup/auth/logout result metadata without request history. State the unknown-session association boundary | D9; source matrix lifecycle/metadata gaps and open rules | 23; direct history, fork (with/without load, colliding tool IDs, no source content), pre-installation notification, reattach, catalog/current-result and zero-session recovery cases |
| 15 | Terminal handle and output separated: frozen output through release and the connection's end; the holding change | D8 | 12, 24 |
| 16 | Independent scope interactions and original responders; actual parent-RPC lifetime using existing ingress registration; permission tool patch contributes to timeline; accepted URL outcomes; remove `agent_request`; form schema/value support, ordinary patterns, exact i64 request IDs and integer-bound validation | D10, D11; shared form, ID-domain and URL-result gaps | 7, 8, 11; services S01–S03/F01–F11, including two simultaneous answers and multi-RPC parent lifetime |
| 17 | Close matrix-to-direct-test coverage alongside each change; exercise retained transport/fs/terminal/auth/MCP paths and extension fallback; keep optional exclusions explicit. This adds tests and evidence, not another runtime state system | Unverified coverage; audit Q5 | Matrix methods, fields, capabilities, replies/cancellation and cleanup; existing convergence remains a separate regression gate |
| 18 | Actions: `set_mode`, `set_config` and `close` outside the operation slot, `closing`, `end: closed`; complete controls metadata and isolate late acknowledgements by their actual request; necessary client controls and close confirmation | R1, R2, D12 | 18, 28, 29, 37; direct metadata and late-response cases |
| 19 | Subsequent presentation work: bounded Agent/MCP diagnostics and collapsible panels; scope decided separately from basic ACP conformance | Historical R3; not a first-layer gap | 32 if retained in the presentation scope |
| 20 | Subsequent presentation work: Markdown export over items, `end` and terminal snapshots; browser cases of settled content and per-interaction answers | (D1, D4, D8 in the client) | 33, 34, 35 |

Dependencies: 10 → 11 → 12 → 13; 15 follows 11. Step 14's history-knowledge and
recovery work can start after 11, and so can its fork gate: the target inherits no
source tail or terminal output, so it no longer waits for 13 or 15. Step 16's interaction work can proceed alongside the
content work, but its permission-only tool gate requires 11. Steps 16 and 18 reuse
the same existing pending-RPC registration/completion foundation; whichever changes
it first establishes the shared behavior, not a second map. Step 17 accompanies
each implementation increment. Step 20 follows 12, 15 and 16.
§7.0 puts first-layer completion before presentation work in 19–20. Each step includes the minimum client
adaptation necessary to keep the product working. A first-layer defect closes only
when its direct ACP/memory case passes; later presentation acceptance remains open
until its own case passes. Historical UI features are not silently promoted to
mandatory ACP capabilities.

### 7.3 Gates

- **Fidelity cases** (audit §8): a scripted ACP Agent sends a fixed update sequence;
  the test asserts the bridge's retained items, fields, controls and resources, and
  the actual ACP replies/cancellation. Expected values are written from the ACP
  input, not calculated by the reducer or projection under test. The full
  first-layer matrix is in [audit §9](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract).
- **Memory ownership:** repeat the applicable cases with no browser subscriber;
  inspect the owning session/resource state before projection. Rebuilding derived
  publication caches preserves content and resource reads. Check both sides of
  cleanup: retained results remain readable, consumed responders cannot answer
  again, dead handles are unusable, and unreferenced resources can be released.
- **Ordered lifetime cases:** use explicit barriers around early notifications,
  response receipt, coordinator completion and delayed local callbacks. Distinguish
  scope lifetime from operation lifetime; use the actual request/attempt association,
  without reinstating historical owner/CAS/intent machinery.
- **Convergence** (§4.1): the three replicas, plus settle points that rewrite an
  entry, idle tail growth, and a coalesced rewrite.
- **Browser (presentation acceptance)**: completed, failed and reloaded turns show their tool cards, thoughts,
  plans and compactions collapsed by default and expandable; a released terminal's
  output stays in its tool card; the v0.2.8 cases of live configuration and close
  (`acp-ui.pw.ts` at the release, from line 2694) pass again.
- **Release parity**: each first-layer regression against rc.5 in the
  [matrix](release-parity.md) has a passing case. Shared gaps and stronger target
  guarantees have their own cases and are not mislabeled as regressions. A test
  changed to assert a loss is changed back, not counted as fidelity coverage.

### 7.5 Implementation status (2026-10-23)

Each item is one merged pull request into `experiment/protocol-v2`, with its tests
written first and run by CI only. The bridge is verified through its own memory
(gate and unit tests that read it); the published rev 2 documents did not change
while the first layer was built, so the browser suite kept passing. The revision 2
engine has been removed since (#122): the wire, the clients and the tests are
revision 3's, served under `/api/v2`.

| Part | Merged | Not yet |
| --- | --- | --- |
| **Design** | memory model (#54), client protocol revision 3 (#55) | |
| **Step 11 — timeline** | reducer over flat entries with the echo rule and derived turns (#56); entries shared structurally (#58); held through load, resume and reattach (#59); live prompts (#60); notification envelope `_meta` (#63); a new session's early notifications (#65) | |
| **Step 12 — prompt outcomes** | response, Agent error, invalid response and interruption recorded; content kept (#61) | |
| **Step 13 — idle updates** | updates outside a prompt reduced into the timeline (#62) | |
| **Step 14 — history knowledge** | unknown / partial / complete (#64); a fork target never inherits source entries (#66); one installation at a time (#67); each timeline named by an epoch (#74) | catalog pages and cursors, setup/auth result metadata, zero-session recovery, session generations (with step 18) |
| **Step 15 — terminals** | a released terminal a tool embeds keeps its frozen output, also across the connection's end (#68) | shown in its tool card: layer 2 |
| **Step 16 — interactions** | an interaction belongs to its scope and is shown with or without an operation (#126–#137); an elicitation names a session or an outstanding request, or is refused (#138); answered one at a time at its resource, the `answer` operation removed (#139); the `agent_request` operation kind removed (#140); a URL flow stays `completed` or `ended` until dismissed (#148) | the parent request's response ending its request-scoped interactions on its own (the end of their operation does so, and the two coincide in practice); exact request IDs; a flow `ended` by the connection's end cannot be dismissed (the bridge that applies resource actions is gone) until the next connection starts without it; answers to forms are already validated against the request's schema (`elicitation_validation`) |
| **Step 18 — actions** | controls metadata: each control keeps the `_meta` of the update and of the notification that set it (D12, #117) | session generations guarding late responses (a control is registered under its own operation ID, which a closed and reopened session no longer has, so a stale acknowledgement already changes nothing; an explicit generation is not planned unless a test shows a gap). Controls and close during a prompt (R1, R2) are served and tested (`stream_tests/actions.rs`) |
| **Layer 2 — building blocks** | document digest, Rust and TypeScript, against independently computed vectors (#69, #70); the timeline as entries with attachment references (#71); the verifying replica (#72); the publisher with epoch, rev and differential snapshots (#73); the live facts of a session (#76); client-chosen operation IDs and repeats (#77); the global and catalog facts (#78); the operation a scope shows, with its scope and `after` (#80); interactions and URL flows in `live`, the whole `live` of a scope (#81), and the publisher's input (#82); the stream query (#83) and the per-subscriber queue that coalesces to snapshots (#84); the feed that publishes every commit (#85) behind a shared handle streams wait on (#86) and is committed to by the bridge beside the rev 2 publisher (#87); checkpoint pacing, 500 ms after a pause and at least every 2 s (#88, #89); `GET /api/v2/stream` with the heartbeat (#90); loading the session a stream follows with its `cwd`, and ending a scope with the reason the bridge recorded (#91, #92) | restoring the acceptance cases of the removed revision 2 tests on revision 3 ([the ledger](rev2-retired-tests.md)); the porting of the browser protocol cases ([list](protocol-browser-cases.md)) |
| **Layer 2 — wire and clients** | operations, actions and resources over HTTP: every operation answers 202 and the result is in the document, answers per interaction, set_mode/set_config/close and terminal actions (#93–#113); the client stream, the verifying replica set and the write queue in TypeScript; the app on revision 3, with the thread projected from entries (tool calls of old turns shown again), create/fork read from the documents, catalog pages in the document; `GET /api/v2/scopes/{scope}` as one snapshot (#116) | |
| **Cutover** | rev 2 TypeScript removed (#115); admission, answers, controls and reads served from the feed (#118, #119); the bridge gate checks the feed (#121); the publisher, hub, `/api/v2/stream`, the rev 2 scope routes and their tests removed (#122); the revision 3 routes take the `/api/v2` names (#123); the rev 2 events and reads leave the generated types (#124) | done: the revision 2 layer-1 leftovers are removed (the unused session projection #144, the per-session turn log #145, `canon`'s turn log #146); the generated types now match the documents' terminals, URL flows and operations (#143, #147) |
| **Dependencies** | vite (#57), webpki crates (#57), vitest (#75) advisories | |

Since then (#150–#156): a catalog page keeps the Agent's `_meta` (#150); a draft pull
request runs only the fast checks and the full validation starts when it is marked ready
(#151); attachment identity is one definition, the hash of the block's exact form, for
a prompt's references (#152) and for the read, whose body is those bytes (#153), which
the client now verifies before showing anything (#156); the independent acceptance
rows for concurrent answers (#154) and for integers beyond 2^53 in document digests
(#155) have their tests.

The three decisions that were left open were settled by comparison with Zed (audit
Q8–Q10, memory model §11). **Recovery without a session**: an explicit `restart` action
on `global` (valid when the Agent is `stopped` or `error`), because nothing passive may
start a connection. Implemented in three pull requests, each tests first: the action in
the bridge and the server (#159); the write client and the page's *Reconnect*, which
already shows when the Agent is stopped or failed and now sends it (#160); these
documents and the test of a restart while the connection initializes. **Public close**: no separate button, since releasing a session already
closes it at the Agent when it is unneeded and `close` stays an action; no code. **A flow
`ended` by the connection's end**: stays in the final documents until the next connection
starts without it; no code.

Deviations from the plan, on purpose:

- Layer-1 pull requests did not change the published documents. Frozen terminal
  output is therefore memory only until the layer 2 wire carries entries (a first
  attempt to project it left output on the page without its tool card; #68).
- Step 11 did not extend the rev 2 turn log with items (the superseded #53). The
  wire moves from the turn log to entries in one cutover.
- Waiting on bridge memory, not on published documents, is how gate tests observe a
  change that publishes nothing.

### 7.4 Second milestone: bridge-to-client replication

Status (2026-10-21): the design is proposed as [protocol revision 3](protocol.md):
replication of the memory model's documents with `epoch`/`rev`, differential
snapshots, client-computed digests at snapshots and checkpoints, client-chosen
operation IDs and results in the document. Its acceptance cases are
[protocol §12.4](protocol.md#124-independent-acceptance). Implementation follows the
first-layer gate.

This stage designs **简易 Agent 交互协议**, the project's private protocol between
FE/other presentation clients and the bridge. ACP continues to govern the bridge's
communication with the Agent.
The design brief is [protocol §12](protocol.md#12-synchronization-design-requirements):
basic requirements, semantic representation coverage, unresolved wire decisions
and independent failure-injection acceptance. “Complete state” means all displayable
and actionable business state, including resolvable resources; responders and
execution handles remain internal.

**Sequence:** complete and directly verify ACP functionality and its authoritative
memory representation first; then design the FE ↔ bridge synchronization protocol
from that state. This second milestone includes protocol design, not only testing
an already-fixed synchronization scheme. The existing wire rules and implementation
are a working baseline to reassess, not constraints that justify removing ACP
content or changing its lifecycle.

The design must explain how clients receive the full displayable state, verify what
they have installed, and replace inconsistent or missing state from the bridge,
including initial subscription, concurrent changes, disconnection and reopen.
Browser UI state is separate; browser replicas cannot supply authority missing
from bridge memory. Derive these behaviors from a small shared set of rules and
retain the memory-only boundary.

Retaining full content and hashing it in the bridge does not prove that a replica
validates the content it installs. Check the trust placed in received log hashes,
the detection and replacement of divergent live state, and recovery from a
mismatched suffix or interrupted stream. Specify the verification points before
choosing fields or mechanisms. This first-layer plan does not settle the final
wire shape or repair algorithm.

After the first-layer gate, finish this stage in order:

1. Map the validated memory to documents and resource reads: protocol §3–§4
   (proposed).
2. One synchronization, verification and repair scheme with its write semantics:
   protocol §5–§7 (proposed). Replace the turn log, hash chain and held responses in
   the bridge DTOs and `shared/` together, so only one contract remains.
3. Add protocol §12.4's independent and failure-injection cases, then implement the
   smallest bridge and client changes that satisfy them. Keep the existing working paths
   usable during migration; optional UI and diagnostics remain separate.

Keep existing atomic-application and per-event convergence tests throughout the
migration. They prove a different property from ACP fidelity and cannot replace the
direct memory assertions in §7.3.
