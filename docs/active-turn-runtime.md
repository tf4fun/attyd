# Active-turn-only bridge runtime

This document defines the target runtime and recovery contract for attyd. It supersedes any earlier
design that kept completed conversation history in the Rust bridge.

## Decision

The ACP Agent is the only authority for completed session history. The bridge is an in-memory ACP
client runtime, not a session store, presentation cache, checkpoint database, or write-ahead log.

The bridge retains only:

- one active turn per active session;
- bounded prompts accepted behind that active turn;
- live ACP resources whose protocol lifecycle has not ended;
- bounded session identity, cwd, capability, control, lifecycle and operation metadata;
- bounded transient delivery state for active-turn snapshots and `session/load` replay.

The bridge never retains a completed turn, transcript, Agent replay, completed terminal output, or
completed debug history after the turn's terminal transition has retired. This rule applies whether
or not the Agent supports `session/load`.

The browser may retain the conversation it has rendered for the lifetime of the page. That state is
a disposable view and is never accepted by the bridge as session authority.

## Deliberate product consequences

This design intentionally accepts the limits of the connected ACP Agent:

- If `loadSession` is advertised, reopening an idle completed session performs `session/load` and
  replaces the browser view with exactly the replay the Agent supplies.
- If `loadSession` is absent, reopening completed history produces `HistoryUnavailable`.
- `session/resume` does not substitute for replay.
- An Agent may reject a repeated load of a session that is already active on the ACP connection. The
  bridge reports that error and does not emulate load, restart the Agent, or reconstruct history.
- Reloaded history may omit thoughts, terminal output, debug metadata, turn boundaries, stop reasons,
  or any other content the Agent did not persist. The bridge does not fill those gaps.
- Browser reconnect may cause one capability-gated, single-flight `session/load`; observer reconnect
  is no longer required to leave the Agent request trace unchanged.

These are protocol/Agent limitations, not reasons to create a second session database in attyd.

## Runtime ownership

```text
ACP Agent
`- sole completed-history authority

Bridge process
|- bounded connection and session control metadata
|- ActiveTurn[session_id]                  (zero or one per session)
|- bounded queued prompts
|- LiveResourceStore                       (terminal/interaction/URL/MCP)
|- bounded typed delivery queues
`- transient LoadTransaction               (zero or one per session)

Browser page
`- disposable rendered projection
```

An active payload has one materialized business representation. Raw diagnostics may exist in a
small independent ring, but dropping that ring cannot affect liveness, folding, reconnect, cancel,
or terminal delivery.

## Per-session state machine

```text
IdleLoadable | IdleUnavailable
        | prompt admitted
        v
Active(turn_key, revision, normalized_turn)
        | PromptResponse, prompt error, or transport loss
        v
Sealing
        | Hub commits owned terminal event
        v
FinalCommittedPendingRetire
        | active snapshot leases are cancelled or released
        v
IdleLoadable | IdleUnavailable
```

`turn_key` is `(epoch, session_id, incarnation, operation_id)`. Every update, cancel, terminal
outcome, reconnect snapshot, and late-event check uses the entire key. A session actor serializes
these transitions; no ACP callback, terminal reader, subscriber task, or command handler mutates an
active turn independently.

The transition from `Active` to `FinalCommittedPendingRetire` is the linearization boundary between
the two reconnect modes:

- a reconnect registered before the boundary obtains an expiring active-snapshot lease;
- a reconnect registered after the boundary must load or receive `HistoryUnavailable`;
- no reconnect can observe or combine both representations.

The Hub terminal event must own its small payload. It cannot borrow a slice or reference from the
active turn. A slow subscriber is evicted under the existing byte budget and never pins turn
retirement. Once all snapshot leases have ended, retirement drops the complete active turn,
semantic indexes, prompt payload, terminal response/error payload, and turn-scoped diagnostics.

## Active-turn folding

ACP updates are normalized once in the active turn and published as bounded typed increments.
There is no cumulative session snapshot and no completed `SessionUpsert`.

Updates have explicit merge semantics:

| Update class | Fold rule |
| --- | --- |
| user/agent/thought text chunks | append in ACP order; adjacent compatible chunks may be concatenated |
| tool call start/update | fold by tool-call ID while preserving first-appearance order and patch semantics |
| plan/config/mode/usage/session info | latest valid semantic value by stable key |
| compaction | apply its replacement/removal semantics; never infer missing history |
| permission/elicitation/URL flow | live-resource state transition, not conversation history |
| terminal output | append delta in the terminal live-resource store; never publish cumulative output per read |
| unknown valid ACP data | bounded diagnostics only |

Coalescing occurs before enqueueing. Append data can coalesce only when order is preserved. Keyed
state can coalesce only after partial patches have been folded into a complete entity. Final/control
traffic has reserved queue capacity and cannot be starved by bulk text or terminal output.

## Active reconnect

An active reconnect must not call the Agent. Snapshot capture and suffix registration form one
session-actor operation:

1. capture a normalized active snapshot through revision `r`;
2. register the subscriber for increments starting at `r + 1`;
3. stream the snapshot in bounded chunks;
4. buffer only the bounded suffix produced during the snapshot;
5. release the suffix in order after the snapshot completes.

Snapshot timeout, suffix overflow, disconnect, or slow delivery cancels the snapshot lease and
disconnects that subscriber. It does not cancel the Agent turn. A subscriber retries from the
current state and may cross the terminal boundary into load/`HistoryUnavailable`.

## Completed reconnect and load

An idle reconnect follows the negotiated capability exactly:

```text
loadSession = false  -> HistoryUnavailable
loadSession = true   -> single-flight Reloading operation
```

`Reloading` is mutually exclusive with prompt, close, delete, fork, mode/config mutation and another
load for that session. Reconnect flapping joins the same operation and is rate-limited; it cannot
create an unbounded sequence of Agent requests.

A load is a transient replacement transaction, not retained history:

1. send `replace_begin(load_generation)` to the requesting browser view;
2. relay replay updates through byte/count-bounded delivery using that generation;
3. do not install replay into bridge session state;
4. on a valid successful response, send `replace_commit` and discard all transient replay state;
5. on Agent error, invalid replay, transport loss, delivery failure, or resource overflow, send a
   precise failure/`HistoryUnavailable`, discard the transaction, and expose no partial replacement;
6. if the ACP request can no longer be cancelled, continue draining its messages to the terminal
   response while discarding them so the connection remains usable.

The browser stages replay by generation and swaps its rendered session only on `replace_commit`.
This browser staging is disposable rendering state, not bridge or Agent authority. A new prompt is
not admitted until replay delivery terminates, preventing old replay from appearing after new live
output.

Repeated load of an already active Agent session is best effort because ACP v1 does not provide a
read-only snapshot or reload idempotency guarantee. Rejection is surfaced; the bridge does not
automatically close, reconnect, or retry. Agent-specific compatibility, including Goose, is a
release gate rather than hidden protocol emulation.

## Terminal and other live resources

A live resource may outlive the turn that created or referenced it. It is not completed history.

- A terminal remains live until `terminal/release`, session close/delete, or connection shutdown.
- Terminal output is stored once in a bounded rolling buffer and sent as output increments.
- Exit without release remains live because the Agent may still request output or release it.
- Release immediately removes the output and metadata; a future Agent replay containing only its
  terminal ID renders `output unavailable`.
- Pending permission, elicitation, accepted URL flow, MCP call, responder and waiter each have an
  explicit terminal transition and are removed at that point.
- Turn completion removes only turn-scoped state. It cannot remove a resource whose ACP lifecycle is
  still live.

Every live-resource class has both count and aggregate-byte limits. Terminal, MCP and auth processes
also require process-group termination, reader cancellation, saved join handles and bounded cleanup.

## Memory and queue bounds

The initial implementation remains entirely in memory. File-backed or mmap storage is deliberately
deferred until measurements show that a single bounded active representation is still too large.

Hard limits are required for:

- active-turn bytes and event/entity count per session;
- aggregate active-turn bytes across all sessions;
- queued prompt count and bytes per session and globally;
- live-resource count and bytes by class and globally;
- bridge-to-Hub, Hub-to-subscriber and SDK-facing queue items and bytes;
- simultaneous large-event processing bytes;
- active snapshot count, suffix bytes and deadline;
- concurrent load count and replay delivery bytes.

Admission reserves global capacity before dispatching a prompt or load. Capacity exhaustion causes
backpressure, a precise rejection, or an explicit uncertain/truncated terminal path; it never causes
an unbounded enqueue or silent semantic loss.

After warm-up, repeated completed turns must return logical retained bytes to the same bounded
baseline. The allocator may keep anonymous pages at a high-water mark, but RSS must plateau rather
than grow with completed-turn count.

## Removal ledger

The completed-history paths from the previous implementation have been removed:

- `SessionRuntime.transcript`, `observed_turns`, history gaps and replay baselines no longer exist;
- completed intent results/tombstones no longer exist; a live intent stores only a fixed SHA-256
  digest and identity, and terminal intents are retired;
- prompt terminal transitions remove the active prompt, folded updates and every payload-bearing
  delta prefix before publishing the idle session revision;
- the Hub's canonical projection contains only the bounded active runtime and control metadata;
- the browser delivery projection filters idle session shells and retains only active-turn/live
  resource events; it is named `ActiveRuntimeProjection` to make that boundary explicit;
- load replay is requester-private transient delivery and is never inserted into either projection;
- terminal output is published incrementally and retained only in a bounded live-terminal buffer;
  release deletes that buffer and its metadata immediately;
- session responses retained by the bridge are reduced to `sessionId`, `modes` and
  `configOptions`; arbitrary Agent `_meta` payloads remain one-shot protocol output.

The remaining canonical snapshot/delta and active delivery projection are active-reconnect
mechanisms, not completed-history fallbacks. Neither may make an idle session locally replayable.

## TDD gates

1. **Specification gate:** this authority, retention, reconnect and resource contract is reflected in
   the state-machine ledger and ACP coverage documentation.
2. **Red-test gate:** deterministic unit/integration tests fail against every old retention path and
   cumulative terminal path before production behavior changes.
3. **State gate:** active folding, terminal retirement, active reconnect and load/
   `HistoryUnavailable` transitions pass with injected small limits.
4. **Bound gate:** byte/count limits, slow consumers, reconnect storms and multiple sessions plateau
   under deterministic fault injection.
5. **Protocol gate:** stdio, HTTP/SSE and WS fixtures plus the Goose canary pass the same contract.
6. **Browser gate:** replacement generations commit/rollback atomically and active reconnect equals
   uninterrupted rendering.
7. **Removal gate:** all completed-history/cache code is deleted, then the complete suite and soak
   tests pass without compatibility fallback.
