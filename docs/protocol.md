# attyd protocol v2

Status: draft, 2026-10-12. Nothing here is implemented yet. This document defines
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
  };
  catalog: { revision: string };             // token for the Agent-owned session list (§6)
  sessions: Record<string, SessionSummary>;  // sessions the bridge holds
  requests: Record<string, GlobalRequest>;   // ACP requests outside any held session
  interactions: Record<string, Interaction>; // request-scoped user input (requestId set)
  urlFlows: Record<string, UrlFlow>;         // request-scoped URL flows (requestId set)
}

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

interface GlobalRequest {
  requestId: string;                         // bridge-allocated, unique in the epoch
  method: string;                            // ACP method ("authenticate", "logout",
                                             // "session/new", "session/fork",
                                             // "session/delete", "session/list", …)
                                             // or "attyd/auth_terminal"
  clientIntentId: string | null;             // Idempotency-Key of the starting command
  sessionId: string | null;                  // addressed session, if any (fork source, cold delete)
  status: "running" | "succeeded" | "failed" | "cancelled";
  result?: RequestResult;                    // succeeded
  error?: ProtocolError;                     // failed
  terminal?: TerminalState;                  // attyd/auth_terminal
}

type RequestResult =
  | { kind: "session"; sessionId: string; incarnation: number }  // session/new, session/fork
  | { kind: "authenticated"; methodId: string }                  // authenticate, auth terminal
  | { kind: "done" };                                            // everything else
```

**A request record exists for every ACP request the bridge sends outside a held
session, while that request is in flight.** That includes requests a read starts:
`GET /api/v2/catalog` sends `session/list`, so it gets a record. Every request-scoped
elicitation therefore has a parent that a replica can see. Finished records are
retained per §3.4.

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
  operation: { operationId: string; kind: OperationKind; stage: string } | null;
  interactions: Record<string, Interaction>;
  urlFlows: Record<string, UrlFlow>;
  terminals: Record<string, TerminalState>;
}

type SessionPhase = "cold" | "loading" | "ready" | "running" | "reconciling" | "blocked";
type OperationKind = "load" | "resume" | "fork" | "close" | "delete" | "set_mode" | "set_config";

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

interface UrlFlow {
  elicitationId: string;
  interactionId: string;                     // the accepted elicitation
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

A fork is tracked as a global request (`session/fork` with `sessionId` = source),
because its result is a new session and the source may be cold. While the request
runs, a held source also shows `operation.kind = "fork"`, which excludes other
mutations of the source. This is the current admission rule.

### 3.3 Controls

`controls` is the **reduced current state** of the session's controls. It is not a
log of the latest notifications. The initial `session/new`, `load`, `resume` or `fork`
response comes first, and every later update folds into it in arrival order:

```ts
interface SessionControls {
  modes: { currentModeId: string; availableModes: SessionMode[] } | null;
  configOptions: SessionConfigOption[];      // also carries model selection in schema 1.7.0
  commands: AvailableCommand[];
  info: { title: string | null; updatedAt: string | null; meta: unknown | null };
  usage: { used: number; size: number; cost: Cost | null } | null;
}
```

| ACP source | Reduction |
| --- | --- |
| response `modes` | sets `modes` |
| `current_mode_update` | sets `modes.currentModeId` |
| response `configOptions`, `config_option_update` | replace `configOptions` as a whole |
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
| `UrlFlow` | a URL elicitation is accepted. Accepting only consents to open the URL (ACP). | stays after its parent request ends. Becomes `completed` on `elicitation/complete` and `cancelled` on Agent cancellation or session end. Kept in a terminal status until acknowledged or retained out. |
| `GlobalRequest` | the bridge sends the ACP request | when running: never. When finished: acknowledged (§5.2) or retained out. |
| `TurnOutcome` | the turn ends | retained out |
| `TerminalState` | the Agent creates the terminal | released and no longer referenced by the active turn or the timeline |

"Retained out" means a bounded retention that the bridge enforces. It is at least
the last 16 finished records per scope and at least 10 minutes. A replica that
reconnects within those bounds sees every result.

## 4. Replication stream

```text
GET /api/v2/stream?scope=global&scope=session/<id>[&cwd=<dir>]
Last-Event-ID: <epoch>:<seq>        (optional, set by EventSource on reconnect)
```

One stream carries any set of scopes. To change the set, open a new stream.

Each SSE message has `id: <epoch>:<seq>` and one JSON entry:

```ts
type Entry =
  | { type: "snapshot"; scope: string; at: Position; state: GlobalState | SessionState }
  | { type: "reset";    scope: string; at: Position }
  | { type: "ops";      scope: string; at: Position; prev: number; ops: Op[] }
  | { type: "end";      scope: string; at: Position; reason: "closed" | "deleted" | "unobserved" | "replaced" }
  | { type: "position"; at: Position };

type Op =
  | { op: "set";    path: string; value: unknown }   // create or replace; an array index modifies in place
  | { op: "remove"; path: string }                   // map key or array index
  | { op: "append"; path: string; values: unknown[] }// array tail
  | { op: "append"; path: string; text: string }     // string tail
  | { op: "trim";   path: string; count: number };   // drop the first `count` items or UTF-16 units
```

**Ops.** `path` is a JSON Pointer into the scope's document. Any op on a missing
target or parent is a protocol error. The server picks the op that matches the
change:

- growth uses `append`;
- an entry folded in place (a `tool_call_update`, a merged chunk) uses `set` on its
  index;
- a bounded tail moving forward (terminal output) uses `trim` plus `append`;
- anything else uses `set` on the smallest enclosing value.

Applying entries is generic. A replica needs a JSON Pointer patcher and validation of
the result against the schema, not a reducer for each kind of change.

**Continuity.** `prev` is the `seq` of the previous entry delivered for the same
scope, which may be a `snapshot` or the `at` of a fetched snapshot. A replica
applies `ops` only when `prev` equals the last `seq` it applied to that scope.
Otherwise it treats the scope as reset. This is Raft's `prevLogIndex` check, per
scope.

**Start of a scope.** The first entry for each subscribed scope is a `snapshot` or a
`reset`:

- `global` comes inline as a `snapshot`;
- a `session/*` scope comes as a `reset`.

After a `reset`, the replica fetches `GET /api/v2/sessions/<id>` (§6), whose `at` is
at or after the reset position. It buffers entries for that scope until the snapshot
arrives, drops buffered entries with `seq <= at.seq`, and applies the rest.

**Lag.** For a subscriber that falls behind, the server may replace its queued
entries for a scope with one `reset` or `snapshot` at the latest position. A replica
must not depend on seeing every intermediate state.

**Resume.** If `Last-Event-ID` is in the current epoch, the server resumes each
scope from its retained log, or sends a `reset` if the suffix has been compacted.
Any other epoch starts every scope fresh.

**`position`.** This entry advances the stream without data. The server sends it,
coalesced, when the log moves through scopes the stream doesn't carry.

**`end`.** The document no longer exists for this subscription. For a new
incarnation, the old document ends with `replaced`, and a new `reset` follows.

### 4.1 Received and applied position

A replica tracks two positions:

- **received:** the `at.seq` of the last entry it read from the stream;
- **applied:** the largest `p` such that every subscribed scope reflects the log
  through `p`. A scope that is waiting for a snapshot holds `applied` at its reset
  position until the snapshot is installed and its buffer is drained.

Read-your-writes (§5.1) uses **applied**, never received.

### 4.2 Observation and materialization

Subscribing to `session/<id>` is an **observation**. It holds an observation lease
that keeps the session materialized and stops unobserved retirement (H09, H13).
Subscribing to `global` never observes a session.

**Only observation starts materialization.** When a stream subscribes to a session
the bridge doesn't hold, the bridge:

1. creates the session document with `history.phase = "cold"`;
2. starts `load` or `resume` as the session's `operation`, following today's
   capability rules and the stream's `cwd`;
3. sends the scope's `reset` once the document exists.

Load failure, retry and backoff appear as `history.phase`, `history.error` and
`operation`, as they do today. Reads never start materialization (§6).

## 5. Commands

Every command is a `POST`, `PUT` or `DELETE` that returns:

```ts
interface CommandResult<T = unknown> {
  at: Position;           // the effect is committed at or before this position
  scopes: string[];       // scopes the effect touched
  result: T;
}
```

or an error (§7). A command whose effect continues asynchronously (a turn, an auth
flow, a fork) commits its acceptance, and the request record or operation that
tracks it, at `at`. Its progress and result arrive through replication.

### 5.1 Read your writes

A replica shows a command's effect once both of these hold:

- its **applied** position (§4.1) is at least `at.seq`;
- it subscribes to the scopes in `scopes` that it wants to show.

No event correlation and no extra fetch are needed.

### 5.2 Intents and retries

The commands that create something take an `Idempotency-Key: <clientIntentId>`
header. It is required on turn start, `POST /sessions` and fork. Within one epoch:

- the same key with the same body returns the original result with
  `disposition: "duplicate"`, and creates nothing new;
- the same key with a different body fails with `intent_conflict`;
- the request record carries `clientIntentId`. A replica whose HTTP response was lost
  finds its result in `global.requests` (or `outcomes` for a turn) by its own intent
  ID, without retrying.

Exactly-once doesn't cross epochs. A new Agent connection forgets intents, in line
with H07 and H15.

### 5.3 Preconditions

Every session-addressed command names the state it expects in `If-Match`:

| Tag | Meaning | Used by |
| --- | --- | --- |
| `"<epoch>:<incarnation>"` | the bridge holds exactly this incarnation | every command on a held session |
| `"<epoch>:<incarnation>:<historyRevision>"` | as above, plus the history CAS | turn start |
| `"<epoch>:0"` | the bridge holds no incarnation of this session | commands on a cold session: delete, fork |

Interactions use `"<epoch>"` (global) or `"<epoch>:<incarnation>"` (session). A
cold-session command never loads the session. It runs as a global request, so for
example `session/delete` follows the existing cold-deletion path without
`new`/`load`/`resume`/`close`. If the session became held in the meantime, the tag
fails with `owner_mismatch`. The current incarnation is then in `global.sessions`.

### 5.4 Global commands

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/agent/authenticate` | `{ methodId }` | `{ requestId }` |
| `POST /api/v2/agent/logout` | `{}` | `{ requestId }` |
| `POST /api/v2/agent/auth-terminals` | `{ methodId, cols, rows }` | `{ requestId }` |
| `POST /api/v2/agent/auth-terminals/{requestId}/input` | `{ data }` | `{}` |
| `POST /api/v2/agent/auth-terminals/{requestId}/resize` | `{ cols, rows }` | `{}` |
| `POST /api/v2/agent/auth-terminals/{requestId}/cancel` | `{}` | `{}` |
| `POST /api/v2/sessions` (`Idempotency-Key`) | `{ cwd }` | `{ requestId, disposition }` |
| `DELETE /api/v2/requests/{requestId}` | — | `{}`: acknowledge a finished request |
| `DELETE /api/v2/url-flows/{elicitationId}` | — | `{}`: acknowledge a finished global URL flow |

### 5.5 Session commands

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/sessions/{id}/turns` (`Idempotency-Key`) | `{ prompt: ContentBlock[] }` | `{ operationId, disposition }` |
| `POST /api/v2/sessions/{id}/turns/{operationId}/cancel` | `{}` | `{}` |
| `PUT /api/v2/sessions/{id}/mode` | `{ modeId }` | `{}` |
| `PUT /api/v2/sessions/{id}/config/{configId}` | `{ value }` | `{}` |
| `POST /api/v2/sessions/{id}/fork` (`Idempotency-Key`) | `{}` | `{ requestId, disposition }` |
| `POST /api/v2/sessions/{id}/close` | `{}` | `{}` |
| `DELETE /api/v2/sessions/{id}` | — | `{}` held, or `{ requestId }` cold |
| `DELETE /api/v2/sessions/{id}/url-flows/{elicitationId}` | — | `{}`: acknowledge a finished URL flow |

### 5.6 Interactions

```text
POST /api/v2/interactions/{interactionId}
If-Match: "<epoch>" | "<epoch>:<incarnation>"
```

| Kind | Body |
| --- | --- |
| permission | `{ outcome: RequestPermissionOutcome }` |
| elicitation | `{ response: CreateElicitationResponse }` |

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
| `GET /api/v2/catalog?cursor=&revision=` | Agent-owned | the Agent's paged `session/list`; `409 catalog_changed` when `revision` is stale |
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
  | "conflict"              // 409: refused by admission (operation in progress, wrong phase)
  | "intent_conflict"       // 409: Idempotency-Key reused with a different body
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "gone"                  // 410: immutable resource no longer retained
  | "agent_error"           // 502: the Agent returned an error
  | "unavailable";          // 503: no Agent connection in this epoch
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
| `bridge/session_reset` | `reset` |
| `bridge/session_delta` (every kind) | `ops` |
| `bridge/session_retired` | `end` |
| `bridge/session_turn_complete` / `_failed` | ops on `/outcomes/<operationId>`, with `/turn` cleared |
| `view.controls` (latest notification by kind) | `controls` (reduced state, §3.3) |
| `view.workspace.session` (`modes`, `configOptions`) | `controls.modes`, `controls.configOptions` |
| `viewRevision`, `fromRevision` | `at.seq`, `prev` |
| `expectedEpoch` / `expectedIncarnation` query, `If-Match: <historyRevision>` | `If-Match` tags (§5.3) |
| `GET …/sessions/{id}?cwd=` loading a cold session | stream observation with `cwd` (§4.2) |
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
