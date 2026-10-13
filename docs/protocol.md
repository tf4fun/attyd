# attyd protocol v2

Status: draft, 2026-10-13. The state documents (§3), the replication stream (§4),
the session commands (§5.1–§5.3, §5.5, §5.6, request acknowledgement from §5.4)
and the reads of §6 except the catalog are implemented (migration steps 1–4); the
other global commands, cold delete and the catalog are not yet. This document defines
the client-facing protocol that replaces today's `/api/v1` business API. The
[implementation plan](state-sync.md) describes how the bridge produces it. The
[runtime contract](active-turn-runtime.md) still governs ACP semantics: history
authority, CAS, idempotency, and epoch and incarnation isolation. The
[ACP coverage ledger](acp-coverage.md) lists the capabilities this protocol must
carry.

## 1. Model

attyd puts a stateless access layer on top of ACP. It works like single-leader log
replication: MySQL primary/replica replication, or Raft's log without election or
quorum.

- **The bridge is the leader and the only writer.** It alone speaks ACP, owns the
  connection, the sessions, their resources and every in-flight operation, and
  decides every transition.
- **Clients are replicas.** A client holds a copy of the scopes it subscribes to and
  nothing authoritative. It may keep local UI state: drafts, unsent queued prompts,
  expanded panels, scroll positions. Anything another client would need to see is
  bridge state.
- **Writes are commands sent to the leader.** A client never changes its replica
  directly. It sends a command over HTTP, and the effect comes back through
  replication.
- **The log is row-based.** Entries describe the resulting state as document
  operations (§4), not the ACP calls that caused them. Applying an entry needs no
  ACP knowledge.

**Stateless means rebuildable.** A client can discard its replica at any moment,
then re-read the public state and recover:

- every interaction that can still be answered;
- every operation that is still running;
- the result of every operation that finished within the retention bounds (§3.4).

Every later command needs only the request itself and public state. No result
exists only in a single HTTP response.

**Coverage.** The protocol carries ACP v1 as pinned in `Cargo.lock` (schema
`1.7.0`), the unstable features attyd negotiates, and the product boundary in the
[coverage ledger](acp-coverage.md). Provider configuration, NES and editor document
integration are product exclusions. Filesystem, terminal and MCP requests from the
Agent are fulfilled inside the bridge as the ACP client. They need no client
endpoints, but every user-visible effect of them is projected.

The protocol has three parts:

| Part | Transport | Purpose |
| --- | --- | --- |
| State documents (§3) | `GET` snapshots | replicated state, one document per scope |
| Replication stream (§4) | SSE | ordered operations on those documents |
| Commands and reads (§5, §6) | HTTP | writes, plus immutable or Agent-owned reads |

All paths are under `/api/v2`. JSON field names are camelCase. Embedded ACP types
(`SessionUpdate`, `RequestPermissionRequest`, `InitializeResponse`, …) are carried
verbatim, as the ACP schema defines them.

## 2. Positions

```ts
interface Position { epoch: string; seq: number }
```

- `epoch` identifies one bridge connection to the Agent. A new Agent connection
  starts a new epoch. Every replica then discards all state from the old epoch, as
  a replica does after a primary change.
- `seq` is the log position inside the epoch. It starts at 1 and increases by one
  for each committed entry, across all scopes.
- Every snapshot reports the position it was taken at. Every command response
  reports the position its effect was committed at (§5.1).

## 3. State documents

A **scope** is the unit of replication. There are exactly two kinds:

- `global` always exists in an epoch.
- `session/<sessionId>` exists while the bridge holds that session. A new
  incarnation of the same session ID is a new document.

Anything outside a held session belongs to `global`. That includes ACP requests made
before a session exists, requests on sessions the bridge does not hold, and the
request-scoped interactions those requests trigger.

### 3.1 Global

```ts
interface GlobalState {
  agent: {
    phase: "starting" | "initializing" | "ready" | "stopped" | "error";
    error: ProtocolError | null;
    launch: {
      transport: "stdio" | "http" | "ws";
      command: string[];
      cwd: string;
      readOnly: boolean;
      additionalDirectories: string[];
      mcpServers: Array<{ name: string; type: "stdio" | "http" | "sse" | "acp" }>;
    };
    initialize: InitializeResponse | null;   // ACP, verbatim
    authenticatedMethodId: string | null;
    predecessor: { epoch: string; reason: EpochEndReason } | null;  // why the last epoch ended
  };
  catalog: { revision: string };             // token for the Agent-owned session list (§6)
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  requests: Record<string, Request>;         // accepted commands and out-of-session ACP requests
  interactions: Record<string, Interaction>; // request-scoped user input (requestId set)
  urlFlows: Record<string, UrlFlow>;         // request-scoped URL flows, by interactionId
}

type EpochEndReason = "agent_exit" | "agent_error" | "auth_restart" | "shutdown";

interface SessionSummary {
  sessionId: string;
  incarnation: number;
  cwd: string;
  title: string | null;                      // controls.info.title
  lifecycle: "attaching" | "active" | "closing" | "closing_for_delete"
           | "deleting" | "closed" | "uncertain";
  phase: SessionPhase;
  work: "idle" | "running" | "awaiting_input";
  pending: { permissions: number; elicitations: number; urlFlows: number };
}

interface Request {
  requestId: string;                         // bridge-allocated, unique in the epoch
  operation: string;                         // protocol operation: "open", "create", "fork",
                                             // "set_mode", "set_config", "close", "delete",
                                             // "authenticate", "logout", "auth_terminal",
                                             // "interaction_response", or "agent_request"
                                             // for ACP requests the bridge starts itself
  method: string;                            // ACP method ("authenticate", "logout",
                                             // "session/new", "session/fork",
                                             // "session/set_mode", "session/close",
                                             // "session/delete", "session/list", …)
                                             // or "attyd/auth_terminal"
  clientIntentId: string | null;             // Idempotency-Key of the command; null only for
                                             // ACP requests the bridge starts itself (§5.2)
  target: { sessionId: string; incarnation: number | null } | null;  // null incarnation: cold
  interactionId: string | null;              // interaction responses
  methodId: string | null;                   // authenticate, auth terminal: the chosen method
  params: Record<string, unknown>;           // the command's non-secret parameters (below)
  status: "running" | "succeeded" | "failed" | "cancelled";
  result?: RequestResult;                    // succeeded
  error?: ProtocolError;                     // failed
  terminal?: AuthTerminalState;              // attyd/auth_terminal
}

type RequestResult =
  | { kind: "session"; sessionId: string; incarnation: number }  // create, fork, open
  | { kind: "done" };                                            // everything else

interface AuthTerminalState {
  output: string;                            // bounded tail, see `trim` in §4
  truncated: boolean;
  exitCode: number | null;
  signal: number | null;
  message: string | null;
}
```

**Request records.** `global.requests` has a record for each of these:

- every accepted **operation command** (§5): open, create, fork, set_mode,
  set_config, close, delete, authenticate, logout, auth terminal start, and
  interaction responses;
- every ACP request the bridge sends outside a held session on its own, including
  requests a read starts: `GET /api/v2/catalog` sends `session/list`, so it gets a
  record with `clientIntentId: null`.

Turn start records its result as a `TurnOutcome` instead. **Control actions**
(acknowledgements, turn cancel, auth terminal input, resize and cancel) never create
records. They only change their parent: the acknowledged record goes away, or
`turn.cancelRequested` is set, or the auth terminal record changes status.

`params` holds the parameters needed to tell two submissions apart, such as
`modeId`, `configId` and `value`, `cwd`, or a permission `optionId` and elicitation
`action`. It never holds prompts, form contents or terminal input.

An interaction response record ends `succeeded` only once the bridge has delivered
the response to the Agent. If the interaction ended first (the Agent cancelled it,
its session or request ended) or delivery failed, the record ends `failed`, with
`error.code = "interaction_ended"` and the reason in `error.data`. This keeps the
guarantee tested by `failed_interaction_delivery_is_never_reported_as_resolved`.
A response to an interaction that was already resolved is rejected synchronously
(`conflict`).

A command that is rejected synchronously (any 4xx in §7) commits nothing and gets no
record. Retrying it gives the same answer. Records live in `global` even when they
target a session. A close or delete result therefore outlives the session scope it
ended, and a refused `set_mode` keeps its ACP error after `operation` clears. Every
request-scoped elicitation has a parent that a replica can see.

### 3.2 Session

```ts
interface SessionState {
  sessionId: string;
  incarnation: number;
  cwd: string;
  controls: SessionControls;                 // reduced state, §3.3
  history: {
    revision: string | null;                 // CAS token for turns (§5.3)
    phase: SessionPhase;
    error: string | null;
    notice: string | null;
    timeline: SessionUpdate[];               // completed history; process bodies collapsed
    collapsedTurns: CollapsedTurn[];         // references to process pages (§6)
  };
  turn: ActiveTurn | null;
  outcomes: Record<string, TurnOutcome>;     // by operationId
  operation: { operationId: string; kind: OperationKind; stage: string;
               requestId: string | null } | null;   // record, when a command started it
  interactions: Record<string, Interaction>;
  urlFlows: Record<string, UrlFlow>;
  terminals: Record<string, TerminalState>;
}

type SessionPhase = "cold" | "loading" | "ready" | "running" | "reconciling" | "blocked";
type OperationKind = "load" | "resume" | "fork" | "close" | "delete" | "set_mode" | "set_config";

interface CollapsedTurn {                   // one completed turn in `timeline`
  turnId: string;                            // page address with history.revision (§6)
  operationId: string | null;                // the turn operation, when known
  beforeUpdate: number;                      // timeline range of the turn
  afterUpdate: number;
  processCount: number;                      // updates on the process page
  visibleRanges: Array<{ start: number; end: number }>;  // prompt and final answer
}

interface ActiveTurn {
  operationId: string;
  clientIntentId: string;
  prompt: ContentBlock[];
  updates: SessionUpdate[];                  // folded overlay; entries may change in place
  cancelRequested: boolean;
}

interface TurnOutcome {
  operationId: string;
  clientIntentId: string;
  status: "completed" | "failed";
  response?: PromptResponse;                 // completed: stop reason, usage
  error?: ProtocolError;                     // failed
  prompt?: ContentBlock[];                   // failed: kept for an exact retry
  afterUpdate: number;                       // timeline index the outcome follows
}

type Interaction =
  | { interactionId: string; kind: "permission"; request: RequestPermissionRequest;
      operationId: string | null }
  | { interactionId: string; kind: "elicitation"; request: CreateElicitationRequest;
      operationId: string | null; requestId?: string };   // requestId: global scope only

interface UrlFlow {                         // keyed by interactionId
  interactionId: string;                     // the accepted elicitation: identifies this flow
  elicitationId: string;                     // Agent-chosen; may be reused by a later flow
  requestId?: string;                        // global scope only
  request: CreateElicitationRequest;
  status: "waiting" | "completed" | "cancelled";
}

interface TerminalState {
  terminalId: string;
  output: string;                            // bounded tail, see `trim` in §4
  truncated: boolean;
  exitStatus: { exitCode: number | null; signal: string | null } | null;
  released: boolean;
}
```

The session document is the compact presentation the web UI uses today. Process
bodies of completed turns are never inlined. They are immutable pages addressed by
`(historyRevision, turnId)` (§6). This keeps snapshots bounded, and privacy release
applies in one place.

A fork requires a held source, as it does today: the bridge takes the source's
`cwd` from its live state, because `ForkSessionRequest` requires one. To fork a cold
session, a client first observes it (§4.2). The fork's record is in
`global.requests`, because its result is a new session. While it runs, the source
shows `operation.kind = "fork"`, which excludes other mutations of the source.

### 3.3 Controls

`controls` is the **reduced current state** of the session's controls. It is not a
log of the latest notifications. It folds three sources, in this order:

1. **Early updates**, which are control notifications that arrive before the
   `session/new`, `load`, `resume` or `fork` response. They fold in arrival order.
2. **The response**, which overrides exactly the fields it provides (`modes`,
   `configOptions`). Fields it doesn't provide keep their early value. For example,
   commands, title and usage from early updates survive.
3. **Later updates**, which fold in arrival order.

```ts
interface SessionControls {
  modes: { currentModeId: string; availableModes: SessionMode[] } | null;
  configOptions: SessionConfigOption[] | null;  // null: never provided; []: provided empty.
                                                // Clients prefer configOptions over legacy
                                                // modes whenever it is non-null (ACP).
                                                // Carries model selection in schema 1.7.0.
  commands: AvailableCommand[];
  info: { title: string | null; updatedAt: string | null; meta: unknown | null };
  usage: { used: number; size: number; cost: Cost | null } | null;
}
```

| ACP source | Reduction |
| --- | --- |
| response `modes` | sets `modes` |
| `current_mode_update` | sets `modes.currentModeId` |
| response `configOptions`, `config_option_update` | replace `configOptions` as a whole. An explicit `[]` stays `[]`, never `null`. |
| `available_commands_update` | replace `commands` as a whole |
| `session_info_update` | patch per field: absent keeps the value, `null` clears it, a value sets it |
| `usage_update` | sets `used` and `size`. `cost` absent or `null` keeps the last known cost. |

These rules are what `fold_control_update` and `SessionLiveState::config_options`
implement today (`src/runtime_state.rs`). The protocol makes them normative, so a
snapshot after any sequence of sparse updates equals the fold of that sequence.

### 3.4 Lifetimes

| Entity | Created | Removed |
| --- | --- | --- |
| `Interaction` | the Agent sends `session/request_permission` or `elicitation/create` | answered, cancelled by the Agent, or its session or request ends |
| `UrlFlow` | a URL elicitation is accepted. Accepting only consents to open the URL (ACP). | stays after its parent request ends. `elicitation/complete` for its `elicitationId` completes the one waiting flow with that ID. It is `cancelled` on Agent cancellation or session end. Kept in a terminal status until acknowledged by `interactionId` or retained out. |
| `Request` | the command is accepted, or the bridge sends the ACP request | when running: never. When finished: acknowledged (§5.4) or retained out. |
| `TurnOutcome` | the turn ends | retained out |
| `TerminalState` | the Agent creates the terminal | released and no longer referenced by the active turn or the timeline |

"Retained out" means a bounded retention that the bridge enforces: at least the last
16 finished records per scope, and at least 10 minutes. The promise has two
boundaries:

- **Scope.** A record lives at most as long as its scope. Session-scoped records
  (`outcomes`, session `urlFlows`) end with their session's `end`. Results that must
  outlive a session, such as close, delete and fork, are `global.requests`.
- **Epoch.** Nothing survives an epoch change. The new epoch's
  `agent.predecessor` says why the old one ended, for example `auth_restart` after a
  terminal login that restarts the Agent. A replica holding a running request from
  the old epoch treats it as ended for that reason.

Within those boundaries, a replica that reconnects sees every result.

## 4. Replication stream

```text
GET /api/v2/stream?scope=global&scope=session/<id>@<epoch>:<incarnation>
Last-Event-ID: <epoch>:<seq>        (optional, set by EventSource on reconnect)
```

One stream carries any set of scopes. To change the set, open a new stream.

Each SSE message carries one JSON entry and `id: <epoch>:<seq>` of its position.
Several entries can share a position (a stream's first entries, a coalesced queue);
only the last of them carries the `id`, so `Last-Event-ID` moves past a position
only once every entry at it has arrived, and a reconnect in between starts that
position again.

```ts
type Entry =
  | { type: "snapshot"; scope: string; at: Position; state: GlobalState | SessionState }
  | { type: "reset";    scope: string; at: Position }
  | { type: "ops";      scope: string; at: Position; prev: number; ops: Op[] }
  | { type: "end";      scope: string; at: Position; reason: "closed" | "deleted" | "unobserved" | "replaced" | "gone" }
  | { type: "position"; at: Position };

type Op =
  | { op: "set";    path: string; value: unknown }   // create or replace; an array index modifies in place
  | { op: "remove"; path: string }                   // map key or array index
  | { op: "append"; path: string; values: unknown[] }// array tail
  | { op: "append"; path: string; text: string }     // string tail
  | { op: "trim";   path: string; count: number };   // drop the first `count` items or UTF-16 units
```

**Ops.** `path` is a JSON Pointer into the scope's document. `set` requires the
parent to exist; its leaf may be missing (create) or present (replace). Every other
op requires its target to exist. A violation is a protocol error, and the replica
resets the scope. The server picks the op that matches the
change:

- growth uses `append`;
- an entry folded in place (a `tool_call_update`, a merged chunk) uses `set` on its
  index;
- a bounded tail moving forward (terminal output) uses `trim` plus `append`;
- anything else uses `set` on the smallest enclosing value.

Applying entries is generic. A replica needs a JSON Pointer patcher and validation of
the result against the schema, not a reducer for each kind of change.

**Continuity.** `prev` is the `seq` of the previous entry delivered for the same
scope on this stream: an `ops` entry, or the `at` of a `snapshot` or `reset`. On a
resumed stream (`Last-Event-ID`), a scope's first entry carries the `seq` of the
scope's previous log entry instead. A replica applies `ops` only when `prev` equals
the last `seq` it applied to that scope. Right after it installs a snapshot at `a`,
it also accepts any `prev <= a`: a fetched snapshot can be later than every entry
the stream delivered for the scope, and the snapshot already covers that earlier
position. Otherwise it treats the scope as reset. This is Raft's `prevLogIndex`
check, per scope.

**Start of a scope.** The first entry for each subscribed scope is a `snapshot` or a
`reset`:

- `global` comes inline as a `snapshot`;
- a `session/*` scope comes as a `reset`.

After a `reset`, the replica fetches `GET /api/v2/sessions/<id>` (§6), whose `at` is
at or after the reset position. It buffers entries for that scope until the snapshot
arrives, then applies the rest.

**Covered entries.** Once a snapshot at `a` is installed for a scope, the replica
**ignores** every later-arriving entry for that scope, in the same epoch and owner,
with `seq <= a`. It does this before checking `prev`, whether the entry was buffered
or arrives over SSE after an HTTP snapshot that overtook it. Only entries with
`seq > a` are checked against `prev`. A snapshot fetched ahead of the stream
therefore never causes a second reset. The same holds for entries the replica
already applied: a scope ignores every entry with `seq` at or before the last one it
installed, `reset` and `snapshot` included, so a resumed stream may replay what the
replica already has and an older snapshot never replaces a newer one.

**Lag.** For a subscriber that falls behind, the server may replace its queued
entries for a scope with one `reset` or `snapshot` at the latest position. A replica
must not depend on seeing every intermediate state.

**Resume.** If `Last-Event-ID` is in the current epoch, the server resumes each
scope from its retained log, or sends a `reset` if the suffix has been compacted.
Any other epoch starts every scope fresh. Resuming never reopens a session (§4.2).

**`position`.** This entry advances the stream without data. The server sends it,
coalesced, when the log moves through scopes the stream doesn't carry.

**`end`.** The document no longer exists for this subscription, and the scope stays
ended for the rest of the stream. A new stream for the same owner starts with `end`
again, even when the replica already received it. A subscription is bound to one owner (§4.2), so
nothing follows `end` for that scope. If the session is held under a new
incarnation, `end` says `replaced`, and the replica opens the session again to get
the new owner.

### 4.1 Received and applied position

A replica tracks three positions:

- **received:** the `at.seq` of the last entry it read from the stream. A new
  stream starts it again at the replica's `Last-Event-ID` (or `0` without one),
  since the stream replays from there;
- **checkpoint:** the `at.seq` of the last entry that carried an SSE `id`, which is
  the last position all of whose entries arrived. A new stream starts it at the
  same value as `received`;
- **applied:** `min(checkpoint, min over pending scopes of installed(scope))`, where:
  - `installed(scope)` is the last `seq` actually applied to that scope: a snapshot's
    `at.seq`, or the `seq` of the last applied `ops`. It is `0` while the scope has
    nothing installed;
  - **every subscribed scope starts pending when the subscription is made**. It
    leaves pending once its first snapshot is installed and its buffer drained, or
    once it receives `end`;
  - a scope becomes pending again on a `reset` or a failed `prev` check, until its
    next snapshot is installed and its buffer drained.

  So on a new stream for `global` and `session/S`, `applied` stays `0` after
  `global`'s `snapshot@20` arrives, until S's snapshot is installed too.

  A `reset` never advances `applied`. For example, a replica applied through 10, a
  command commits at 15, and a coalesced `reset@20` arrives: `applied` stays at 10
  until the snapshot (`at >= 20`) is installed.

  The checkpoint matters when a coalesced queue puts several scopes at one
  position. `global`'s `snapshot@20` can arrive before `session/S`'s `reset@20`;
  until that reset arrives, S still looks installed at its older position, but the
  checkpoint is still below 20, so a command on S committed at 15 is not yet
  reported applied.

Read-your-writes (§5.1) uses **applied**, never received.

### 4.2 Observation and materialization

Observation keeps a session materialized and stops unobserved retirement (H09, H13).
It has two halves:

- **Open** is a command (§5.5). It is an explicit user action, can be retried with
  `Idempotency-Key`, and is the only thing that materializes a session.
- **Subscribe** is a read. It always names a complete owner, and never
  materializes anything.

**Open.** `POST /api/v2/sessions/{id}/open` with `{ cwd }`:

- **Held session:** the bridge returns the current owner.
- **Session the bridge doesn't hold:** in the same actor turn, the bridge creates
  the session document (`history.phase = "cold"`), allocates its incarnation, and
  starts `load` or `resume` as its `operation`. Today's capability rules and the
  given `cwd` apply.
- **Result:** in both cases, `{ sessionId, incarnation }` in the record's `result`,
  plus a *first-subscription lease*.
- **Retry:** a retry with the same key returns the same owner and never
  materializes again.
- **Failures:** load failure, retry and backoff appear as `history.phase`,
  `history.error` and `operation`, as they do today.

**First-subscription lease.** Every command whose result names a session owner the
client hasn't subscribed to yet grants a lease on that owner: open, create, and fork
(on the new target). The lease counts as an observation (H13). It ends when a stream
subscribes to that owner, or when a grace period expires (default 30 s, independent
of `--session-unobserved-timeout`). Even with an unobserved timeout of `0`, a session
that was just created or forked isn't retired before its creator can subscribe. This
matters most for Agents without `load`/`resume`, where a retired session couldn't be
opened again. A retried command grants a fresh lease on the same owner.

**Subscribe.** A session scope is always written with its full owner:

```text
scope=session/<sessionId>@<epoch>:<incarnation>
```

The stream holds an observation lease on exactly that owner. If the bridge doesn't
hold that owner, the scope gets `end` at once and nothing is loaded:

- `closed`, `deleted` or `unobserved`: the incarnation was retired, per a bounded
  per-epoch table of retired incarnations that outlives log compaction;
- `replaced`: the session is held under a different incarnation, or the tag names
  an older epoch;
- `gone`: neither applies, for example after the incarnation aged out of the table.

**Reconnects.** The subscription URL already carries the owner, so an EventSource
retry is the same request as the original subscription, whether or not it carries
`Last-Event-ID`. It can only resume. When page A disconnects before its first event,
and page B then closes the session, A's retry gets `end: closed`. The session doesn't
load again. An observer from an old epoch carries that epoch in its tag and gets
`replaced`; it can't attach to the new epoch's same-numbered incarnation. This keeps
the guarantee tested by
`view_refresh_and_sse_resume_return_the_retired_owner_without_fresh_open`. To show
the session again after `end`, the replica sends a new open.

Subscribing to `global` never observes a session.

## 5. Commands

Every command is a `POST`, `PUT` or `DELETE` that returns:

```ts
interface CommandResult<T = unknown> {
  at: Position;           // the effect is committed at or before this position
  scopes: string[];       // scopes the effect touched
  result: T;
}
```

or an error (§7). An accepted command commits its record (§3.1) or turn at `at`,
whether its effect finishes immediately or continues asynchronously. Its progress and
result arrive through replication. A lost HTTP response loses nothing.

### 5.1 Read your writes

A replica shows a command's effect once both of these hold:

- its **applied** position (§4.1) is at least `at.seq`;
- it subscribes to the scopes in `scopes` that it wants to show.

No event correlation and no extra fetch are needed.

### 5.2 Intents and retries

Every operation command (§3.1) and turn start requires an
`Idempotency-Key: <clientIntentId>` header. The client chooses it before sending, so
it can find its own submission among those of other clients. Control actions take no
key. Within one epoch:

- the intent fingerprint is `(operation, target, owner tag, body digest)`:
  - `operation` is the protocol operation, for example `set_config` or
    `interaction_response`, not the ACP method;
  - `target` is the complete addressed resource: every path parameter (`sessionId`,
    `configId`, `interactionId`, `operationId`, `requestId`) and the scope it
    belongs to.

  Forks of A and B, the same prompt sent to two sessions, `value: "on"` sent to
  config A and to config B, or the same answer to two interactions are therefore all
  different intents, even when their bodies are equal;
- the same key with the same fingerprint returns the original result with
  `disposition: "duplicate"`, and creates nothing new;
- the same key with a different fingerprint fails with `intent_conflict`;
- the bridge keeps the intent table for the whole epoch, separately from the public
  records. **An intent is never evicted within its epoch.** After its record goes
  away, the intent shrinks to a fixed-size tombstone: key, fingerprint hash, original
  ID and final status. The table outlives acknowledgement, record retention and the
  end of the target scope;
- the table has a memory budget. When the budget is full, the bridge **refuses new
  intents** with `intent_capacity` (503) and commits nothing. A known key is still
  answered from its tombstone. Capacity is recovered only by a new epoch. An unknown
  key therefore always means the command was never accepted in this epoch, and
  running it is a true first submission.

**Recovery after a lost response.** A replica looks for its `clientIntentId` in this
order:

1. `turn.clientIntentId` of the subscribed session (a turn that is still running);
2. that session's `outcomes`;
3. `global.requests`.

A match is the answer. **No match proves nothing.** The command may never have been
accepted, or its record may have been acknowledged, retained out, or ended with its
scope. The replica resolves this by resending the same command with the same key:

| Intent table says | Response |
| --- | --- |
| unknown key | the command runs now, as a first submission. Within the epoch this is safe, because intents are never evicted. |
| unknown key, table full | `intent_capacity`; nothing runs |
| known, same fingerprint, result still public | `disposition: "duplicate"` with the original `requestId` or `operationId` |
| known, same fingerprint, result no longer public | `disposition: "duplicate"`, `resultExpired: true`, the original ID and its final `status`; nothing runs again |
| known, different fingerprint | `intent_conflict` |

Exactly-once doesn't cross epochs. A new Agent connection forgets intents, in line
with H07 and H15.

### 5.3 Preconditions

Every session-addressed command names the state it expects in `If-Match`, with these
exceptions. Open (§4.2) and `POST /sessions` take no owner tag, because they exist to
learn one. Global commands (§5.4) take no tag either.

| Tag | Meaning | Used by |
| --- | --- | --- |
| `"<epoch>:<incarnation>"` | the bridge holds exactly this incarnation | every command on a held session |
| `"<epoch>:<incarnation>:<historyRevision>"` | as above, plus the history CAS | turn start |
| `"<epoch>:0"` | the bridge holds no incarnation of this session | delete of a cold session |

Interactions use `"<epoch>"` (global) or `"<epoch>:<incarnation>"` (session). Cold
delete never loads the session. It follows the existing cold-deletion path, without
`new`/`load`/`resume`/`close`. If the session became held in the meantime, the tag
fails with `owner_mismatch`, and the current incarnation is in `global.sessions`.
Apart from open, delete is the only command that works on a session the bridge
doesn't hold. Everything else needs a held session.

### 5.4 Global commands

All operation commands below take `Idempotency-Key` and return `disposition` with
their `requestId`. Rows marked *control* take no key and create no record.

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/agent/authenticate` | `{ methodId }` | `{ requestId }` |
| `POST /api/v2/agent/logout` | `{}` | `{ requestId }` |
| `POST /api/v2/agent/auth-terminals` | `{ methodId, cols, rows }` | `{ requestId }` (the record keeps `methodId` from start to end) |
| `POST /api/v2/agent/auth-terminals/{requestId}/input` | `{ data }` | `{}` (*control*) |
| `POST /api/v2/agent/auth-terminals/{requestId}/resize` | `{ cols, rows }` | `{}` (*control*) |
| `POST /api/v2/agent/auth-terminals/{requestId}/cancel` | `{}` | `{}` (*control*: the record ends `cancelled`) |
| `POST /api/v2/sessions` | `{ cwd }` | `{ requestId }` |
| `DELETE /api/v2/requests/{requestId}` | — | `{}` (*control*: acknowledge a finished request) |
| `DELETE /api/v2/url-flows/{interactionId}` | — | `{}` (*control*: acknowledge a finished global URL flow) |

### 5.5 Session commands

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/sessions/{id}/open` | `{ cwd }` | `{ requestId }`; no `If-Match` (§4.2) |
| `POST /api/v2/sessions/{id}/turns` | `{ prompt: ContentBlock[] }` | `{ operationId, disposition }` |
| `POST /api/v2/sessions/{id}/turns/{operationId}/cancel` | `{}` | `{}` (*control*) |
| `PUT /api/v2/sessions/{id}/mode` | `{ modeId }` | `{ requestId }` |
| `PUT /api/v2/sessions/{id}/config/{configId}` | `{ value }` | `{ requestId }` |
| `POST /api/v2/sessions/{id}/fork` | `{}` (held source) | `{ requestId }` |
| `POST /api/v2/sessions/{id}/close` | `{}` | `{ requestId }` |
| `DELETE /api/v2/sessions/{id}` | — | `{ requestId }` (held or cold) |
| `DELETE /api/v2/sessions/{id}/url-flows/{interactionId}` | — | `{}` (*control*: acknowledge a finished URL flow) |

### 5.6 Interactions

```text
POST /api/v2/interactions/{interactionId}
If-Match: "<epoch>" | "<epoch>:<incarnation>"
```

| Kind | Body |
| --- | --- |
| permission | `{ outcome: RequestPermissionOutcome }` |
| elicitation | `{ response: CreateElicitationResponse }` |

This is an operation command. It takes `Idempotency-Key` and returns
`{ requestId, disposition }`. Its record reports delivery, not just removal (§3.1).

Interaction IDs are bridge-allocated and unique within an epoch, so one endpoint
serves both scopes. Accepting a URL elicitation removes the interaction and creates
the `UrlFlow` in the same commit.

## 6. Reads

A read never changes a state document, and never starts materialization or another
session operation. The only state a read can create is the request record of an ACP
call it makes while that call is in flight (§3.1).

| Read | Kind | Notes |
| --- | --- | --- |
| `GET /api/v2/global` | snapshot | `{ at, state: GlobalState }` |
| `GET /api/v2/sessions/{id}` | snapshot | `{ at, state: SessionState }` for a held session, else `404 not_found` |
| `GET /api/v2/sessions/{id}/history/{historyRevision}/turns/{turnId}/process?offset=` | immutable | process page; cacheable forever; `410` once the revision is gone |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | immutable | `If-Match` owner |
| `GET /api/v2/catalog?cursor=&revision=` | Agent-owned | `{ revision, sessions, nextCursor }`: the Agent's paged `session/list` (`sessions` are ACP `SessionInfo`); `409 catalog_changed` when `revision` is stale |
| `GET /api/v2/sessions/{id}/files?query=` | workspace | context search within the session's workspace |
| `GET /api/v2/sessions/{id}/files/content?path=` | workspace | context read |

The catalog is not replicated, because the Agent owns it and it can be arbitrarily
large. `global.catalog.revision` tells replicas when to re-read it, and
`global.sessions` overlays the live state of the sessions the bridge holds.

## 7. Errors

```ts
interface ProtocolError {
  code: ErrorCode;
  message: string;
  data?: unknown;           // agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier
  | "not_found"             // 404: unknown or unheld session, turn, interaction or request
  | "owner_mismatch"        // 409: If-Match epoch/incarnation is not current
  | "history_mismatch"      // 412: If-Match history revision is not current
  | "precondition_required" // 428: missing If-Match or Idempotency-Key
  | "conflict"              // 409: refused by admission (operation in progress, wrong phase,
                            //      interaction already resolved)
  | "intent_conflict"       // 409: Idempotency-Key reused with a different body
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "gone"                  // 410: immutable resource no longer retained
  | "agent_error"           // 502: the Agent returned an error
  | "unavailable"           // 503: no Agent connection in this epoch
  | "intent_capacity"       // 503: intent table full for this epoch; known keys still answer
  | "interaction_ended";    // record only: the interaction ended before delivery
```

The body of every non-2xx response is `{ error: ProtocolError }`. Stream-level
failures end the stream; the replica reconnects with `Last-Event-ID`.

## 8. Mapping from v1

| v1 | v2 |
| --- | --- |
| `GET /api/v1/runtime` | `GET /api/v2/global` (`agent`) |
| `GET /api/v1/events`, `GET /api/v1/sessions/{id}/events` | one `GET /api/v2/stream` with scopes |
| `bridge/connection`, `bridge/connection_error` | `global` ops on `/agent/phase`, `/agent/error` |
| `bridge/catalog_changed` | `global` op on `/catalog/revision` |
| `acp/authenticated`, `acp/logged_out`, `bridge/auth_terminal_*` | `global.requests`, `/agent/authenticatedMethodId` |
| connection-level `request_elicitations` / `request_url_flows` | `global.interactions` / `global.urlFlows` with `requestId` |
| mode, config, close, delete, fork results in the HTTP response only | `global.requests` records |
| `expectedEpoch`/`expectedIncarnation` on the session event stream | owner-qualified scope `session/<id>@<epoch>:<incarnation>` (§4.2) |
| `bridge/session_reset` | `reset` |
| `bridge/session_delta` (every kind) | `ops` |
| `bridge/session_retired` | `end` |
| `bridge/session_turn_complete` / `_failed` | ops on `/outcomes/<operationId>`, with `/turn` cleared |
| `view.controls` (latest notification by kind) | `controls` (reduced state, §3.3) |
| `view.workspace.session` (`modes`, `configOptions`) | `controls.modes`, `controls.configOptions` |
| `viewRevision`, `fromRevision` | `at.seq`, `prev` |
| `expectedEpoch` / `expectedIncarnation` query, `If-Match: <historyRevision>` | `If-Match` tags (§5.3) |
| `GET …/sessions/{id}?cwd=` or an observing stream loading a cold session | `POST /api/v2/sessions/{id}/open` (§4.2) |
| `POST /api/v1/sessions`, `…/fork` returning the view | request records with `clientIntentId` and a typed `result` |
| `presentation=full\|compact` | compact only; process through immutable pages |
| `includeProcessFrom`, `excludeProcessFor` | removed (§9) |
| `GET /api/v1/sessions` (list) | `GET /api/v2/catalog` |
| `POST /api/v1/sessions/{id}/interactions/{iid}/response` | `POST /api/v2/interactions/{iid}` |
| `GET /api/v1/context/search?sessionId=`, `POST …/context/read` | `GET /api/v2/sessions/{id}/files…` |
| `{ error: string }` and ad hoc codes | `{ error: ProtocolError }` |

## 9. Open questions

- **Inline process for the turn a client just watched.** v1's `includeProcessFrom`
  keeps the process of a turn the client watched inline across commit, so it doesn't
  flicker. In v2 the client already holds those updates from `turn`, and can keep
  them as a local cache keyed by `(historyRevision, turnId)` until it fetches the
  page. Recommended: a client-side cache, with no per-client snapshot variants.
- **Profiles for other access layers.** A subscription may later name a profile that
  is a fixed subset of document paths, for example a session profile without
  `history.timeline` for a chat bot. This is not part of v2.0. The op format already
  lets the server filter by path.
- **Retention bounds.** §3.4 sets minimums. The real values follow the memory
  retention budget.
- **Multiple streams.** A client that changes its observed session opens a new
  stream. If that proves costly, a later minor version could add a
  subscription-update command for an existing stream, keyed by a stream ID.
