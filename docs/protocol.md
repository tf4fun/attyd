# attyd protocol v2

Status: draft, 2026-10-12. Nothing here is implemented yet. This document defines
the client-facing protocol that replaces today's `/api/v1` business API. The
[implementation plan](state-sync.md) describes how the bridge produces it. The
[runtime contract](active-turn-runtime.md) still governs ACP semantics: history
authority, CAS, idempotency and epoch and incarnation isolation.

## 1. Model

attyd puts a stateless protocol layer on top of ACP. It works like single-leader log
replication, as in MySQL primary/replica replication or Raft's log, without leader
election or quorum.

- **The bridge is the leader and the only writer.** It alone speaks ACP, holds
  state and decides every transition.
- **Clients are replicas.** A client holds a copy of the state it subscribes to and
  nothing authoritative. It may keep local UI state: drafts, unsent queued prompts,
  expanded panels, scroll positions. Anything other clients would need to see is
  bridge state.
- **Writes are commands sent to the leader.** A client never changes its replica
  directly. It sends a command over HTTP, and the effect comes back through
  replication.
- **The log is row-based.** Entries describe the resulting state as document
  operations (§4), not the ACP calls that caused them. Applying an entry needs no
  ACP knowledge.

The protocol has three parts:

| Part | Transport | Purpose |
| --- | --- | --- |
| State documents (§3) | `GET` snapshots | replicated state, one document per scope |
| Replication stream (§4) | SSE | ordered operations on those documents |
| Commands and resources (§5, §6) | HTTP | writes, plus immutable or Agent-owned reads |

All paths are under `/api/v2`. JSON field names are camelCase. Embedded ACP types
(`SessionUpdate`, `RequestPermissionRequest`, `InitializeResponse`, …) are carried
verbatim, as defined by the ACP schema.

## 2. Positions

```ts
interface Position { epoch: string; seq: number }
```

- `epoch` identifies one bridge connection to the Agent. A new Agent connection
  starts a new epoch, and every replica discards all state from the old epoch (as a
  replica does after a primary change).
- `seq` is the log position inside the epoch. It starts at 1 and increases by one
  for each committed entry, across all scopes.
- Every snapshot reports the position it was taken at. Every command response
  reports the position its effect was committed at (§5.1).

## 3. State documents

A **scope** is the unit of replication. There are two kinds:

- `global`: always exists in an epoch;
- `session/<sessionId>`: exists while the bridge holds that session. A new
  incarnation of the same session ID is a new document.

There is no third kind. Anything not tied to a session, including ACP requests made
before a session exists, belongs to `global`.

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
  sessions: Record<string, SessionSummary>;  // sessions the bridge currently holds
  requests: Record<string, GlobalRequest>;   // in-flight commands outside any session
  interactions: Record<string, Interaction>; // user input requested outside any session
}

interface SessionSummary {
  sessionId: string;
  incarnation: number;
  cwd: string;
  title: string | null;
  lifecycle: "attaching" | "active" | "closing" | "closing_for_delete" | "deleting" | "closed" | "uncertain";
  phase: SessionPhase;
  work: "idle" | "running" | "awaiting_input";
  pending: { permissions: number; elicitations: number; urlFlows: number };
}

interface GlobalRequest {
  requestId: string;
  kind: "authenticate" | "logout" | "auth_terminal" | "create_session";
  methodId?: string;
  status: "running" | "succeeded" | "failed" | "cancelled";
  error?: ProtocolError;
  terminal?: TerminalState;                  // auth_terminal only
}
```

Global interactions are ACP elicitations with request scope: the Agent asks for input
while answering a global request, such as `authenticate` or a `session/new` that
hasn't returned yet. Each one names the `requestId` it belongs to and is removed when
answered or when that request ends. Session permissions are always session-scoped in
ACP and never appear here.

A finished request stays in `requests` until the client acknowledges it (§5.2), so a
replica that reconnects still sees the outcome. The bridge may also drop finished
requests after a bounded retention.

### 3.2 Session

```ts
interface SessionState {
  sessionId: string;
  incarnation: number;
  cwd: string;
  agent: Omit<NewSessionResponse, "sessionId"> | null;  // modes, models, configOptions as returned
  controls: Record<string, SessionUpdate>;             // latest control update by kind
  history: {
    revision: string | null;                // CAS token for turns (§5.3)
    phase: SessionPhase;
    error: string | null;
    notice: string | null;
    timeline: SessionUpdate[];              // completed history, process bodies collapsed
    collapsedTurns: CollapsedTurn[];        // references to process pages (§6)
  };
  turn: ActiveTurn | null;
  outcomes: Record<string, TurnOutcome>;    // by operationId, bounded retention
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
  updates: SessionUpdate[];
  cancelRequested: boolean;
}

interface TurnOutcome {
  operationId: string;
  clientIntentId: string;
  status: "completed" | "failed";
  response?: PromptResponse;               // completed
  error?: ProtocolError;                   // failed
  prompt?: ContentBlock[];                 // failed: kept so the user can retry
  afterUpdate: number;                     // timeline index the outcome follows
}

type Interaction =
  | { interactionId: string; kind: "permission"; request: RequestPermissionRequest;
      operationId: string | null; requestId?: never }
  | { interactionId: string; kind: "elicitation"; request: CreateElicitationRequest;
      operationId: string | null; requestId?: string };  // requestId: global scope only

interface UrlFlow {
  elicitationId: string;
  request: CreateElicitationRequest;
  status: "waiting" | "completed" | "cancelled";
}

interface TerminalState {
  terminalId: string;
  output: string;                          // bounded tail
  truncated: boolean;
  exitStatus: { exitCode: number | null; signal: string | null } | null;
  released: boolean;
}
```

The session document is the compact presentation used by the web UI today. Process
bodies of completed turns are never inlined. They are immutable pages addressed by
`(historyRevision, turnId)` (§6). This keeps snapshots bounded, and lets privacy
release apply to one place.

## 4. Replication stream

```text
GET /api/v2/stream?scope=global&scope=session/<id>[&cwd=<dir>]
Last-Event-ID: <epoch>:<seq>        (optional, set by EventSource on reconnect)
```

One stream carries any set of scopes. Subscribing to a `session/<id>` scope is an
**observation**: it keeps the session materialized and stops unobserved retirement.
`cwd` lets a cold session be found before it is listed. Subscribing to `global` never
observes any session. To change the scope set, open a new stream.

Each SSE message has `id: <epoch>:<seq>` and one JSON entry:

```ts
type Entry =
  | { type: "snapshot"; scope: string; at: Position; state: GlobalState | SessionState }
  | { type: "reset";    scope: string; at: Position }
  | { type: "ops";      scope: string; at: Position; prev: number; ops: Op[] }
  | { type: "end";      scope: string; at: Position; reason: "closed" | "deleted" | "unobserved" | "replaced" }
  | { type: "position"; at: Position };

type Op =
  | { op: "set";    path: string; value: unknown }  // replace or create
  | { op: "remove"; path: string }
  | { op: "append"; path: string; values: unknown[] }  // array tail
  | { op: "append"; path: string; text: string };      // string tail
```

- `path` is a JSON Pointer into the scope's document. `set` on a missing parent is a
  protocol error. Any other op on a missing target is also a protocol error.
- `prev` is the `seq` of the previous entry delivered for the same scope, which may be
  a `snapshot`. A replica applies `ops` only when `prev` equals the last `seq` it
  applied to that scope. Otherwise it treats the scope as `reset`. This is Raft's
  `prevLogIndex` check, per scope.
- **Start of a scope:** the first entry for each subscribed scope is a `snapshot` or a
  `reset`. A small document (`global`) comes inline as a `snapshot`. A large one
  (`session/*`) comes as a `reset`. After a `reset`, the replica fetches
  `GET /api/v2/sessions/<id>` (§6). The fetched snapshot's `at` will be at or after
  the reset position. The replica then drops buffered entries with `seq <= at.seq`
  and applies the rest.
- **Lag:** if a subscriber falls behind, the server may replace its queued entries for
  a scope with one `reset` or `snapshot` at the latest position. A replica must not
  depend on seeing every intermediate state.
- **Resume:** with `Last-Event-ID` in the current epoch, the server resumes each scope
  from its retained log, or sends a `reset` when the suffix has been compacted. With
  any other epoch, every scope starts fresh.
- **`position`:** advances the replica's stream position without data. The server
  sends it, coalesced, when the log moves through scopes the stream doesn't carry.
  A replica can therefore always tell when it has caught up to a command's commit
  position (§5.1).
- **`end`:** the document no longer exists for this subscription. A new incarnation
  ends the old one (`replaced`), then a new snapshot or reset follows.

Applying entries is generic. A replica needs no per-kind reducer, only a JSON Pointer
patcher plus validation of the resulting document against the schema.

## 5. Commands

Every command is a `POST`/`PUT`/`DELETE` that returns:

```ts
interface CommandResult<T = unknown> { at: Position; result: T }
```

or an error (§7). A command that returns `at` has committed its effect at or before
that position. A command whose effect continues asynchronously (a turn, an auth flow)
commits the acceptance at `at`, and reports progress through replicated state.

### 5.1 Read your writes

To show a command's effect, a replica waits until its stream position reaches the
returned `at`. Its replica then reflects the command. No event correlation, intent
matching or extra fetch is needed.

### 5.2 Global commands

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/agent/authenticate` | `{ methodId }` | `{ requestId }` |
| `POST /api/v2/agent/logout` | `{}` | `{ requestId }` |
| `POST /api/v2/agent/auth-terminals` | `{ methodId, cols, rows }` | `{ requestId }` |
| `POST /api/v2/agent/auth-terminals/{requestId}/input` | `{ data }` | `{}` |
| `POST /api/v2/agent/auth-terminals/{requestId}/resize` | `{ cols, rows }` | `{}` |
| `POST /api/v2/agent/auth-terminals/{requestId}/cancel` | `{}` | `{}` |
| `DELETE /api/v2/requests/{requestId}` | — | `{}`: acknowledge a finished request |
| `POST /api/v2/sessions` | `{ cwd }` | `{ sessionId, incarnation }` |

`POST /sessions` creates a `create_session` request first. Request-scoped
elicitations that happen before the Agent returns the session ID attach to that
request.

### 5.3 Session commands

Every session command carries the expected owner, so a stale replica cannot act on a
new incarnation:

```text
If-Match: "<epoch>:<incarnation>"          (owner precondition, all session commands)
If-Match: "<epoch>:<incarnation>:<historyRevision>"   (turn start only: adds history CAS)
Idempotency-Key: <clientIntentId>          (turn start only)
```

| Command | Body | Result |
| --- | --- | --- |
| `POST /api/v2/sessions/{id}/turns` | `{ prompt: ContentBlock[] }` | `{ operationId, disposition: "accepted" \| "duplicate" }` |
| `POST /api/v2/sessions/{id}/turns/{operationId}/cancel` | `{}` | `{}` |
| `PUT /api/v2/sessions/{id}/mode` | `{ modeId }` | `{}` |
| `PUT /api/v2/sessions/{id}/config/{configId}` | `{ value }` | `{}` |
| `POST /api/v2/sessions/{id}/fork` | `{}` | `{ sessionId, incarnation }` |
| `POST /api/v2/sessions/{id}/close` | `{}` | `{}` |
| `DELETE /api/v2/sessions/{id}` | — | `{}` |

### 5.4 Interactions

```text
POST /api/v2/interactions/{interactionId}
If-Match: "<epoch>"                         (global interaction)
If-Match: "<epoch>:<incarnation>"           (session interaction)
```

| Kind | Body |
| --- | --- |
| permission | `{ outcome: RequestPermissionOutcome }` |
| elicitation | `{ response: CreateElicitationResponse }` |

Interaction IDs are bridge-allocated and unique within an epoch, so one endpoint
serves both scopes.

## 6. Reads

Reads are snapshots of state documents, immutable resources, or data the bridge
passes through without owning it. None of them change state.

| Read | Kind | Notes |
| --- | --- | --- |
| `GET /api/v2/global` | snapshot | `{ at, state: GlobalState }` |
| `GET /api/v2/sessions/{id}?cwd=` | snapshot | `{ at, state: SessionState }`; `cwd` allows loading a cold session. Fetching is not an observation. |
| `GET /api/v2/sessions/{id}/history/{historyRevision}/turns/{turnId}/process?offset=` | immutable | process page; cacheable forever; `410` once the revision is gone |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | immutable | `If-Match` owner |
| `GET /api/v2/catalog?cursor=&revision=` | Agent-owned | Agent's paged `session/list`; `409 catalog_changed` when `revision` is stale |
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
  data?: unknown;           // for agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier
  | "not_found"             // 404: unknown session, turn, interaction or request
  | "owner_mismatch"        // 409: If-Match epoch/incarnation is not current
  | "history_mismatch"      // 412: If-Match history revision is not current
  | "precondition_required" // 428: missing If-Match or Idempotency-Key
  | "conflict"              // 409: refused by admission (operation in progress, wrong phase)
  | "intent_conflict"       // 409: Idempotency-Key reused with a different prompt
  | "catalog_changed"       // 409: stale catalog revision
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
| `acp/authenticated`, `acp/logged_out`, `bridge/auth_terminal_*` | `global` `requests` and `/agent/authenticatedMethodId` |
| `bridge/session_reset` | `reset` |
| `bridge/session_delta` (every kind) | `ops` |
| `bridge/session_retired` | `end` |
| `bridge/session_turn_complete` / `_failed` | ops on `/outcomes/<operationId>`, plus `/turn` cleared |
| `viewRevision`, `fromRevision` | `at.seq`, `prev` |
| `expectedEpoch` / `expectedIncarnation` query, `If-Match: <historyRevision>` | `If-Match` owner tags (§5.3) |
| `presentation=full\|compact` | compact only; process via immutable pages |
| `includeProcessFrom`, `excludeProcessFor` | removed; see open questions |
| `GET /api/v1/sessions` (list) | `GET /api/v2/catalog` |
| `POST /api/v1/sessions/{id}/interactions/{iid}/response` | `POST /api/v2/interactions/{iid}` |
| `GET /api/v1/context/search?sessionId=`, `POST …/context/read` | `GET /api/v2/sessions/{id}/files…` |
| `{ error: string }` and ad hoc codes | `{ error: ProtocolError }` |

## 9. Open questions

- **Inline process for the turn a client just watched.** v1's `includeProcessFrom`
  keeps the process of a turn the client watched inline across commit, so it doesn't
  flicker. In v2, the client already holds those updates from `turn`. It can keep
  them locally as a cache keyed by `(historyRevision, turnId)` until it fetches the
  page. Recommended: client-side cache, with no per-client snapshot variants.
- **Profiles for other access layers.** A subscription may later name a profile that
  is a fixed subset of document paths, for example a session profile without
  `history.timeline` for a chat bot. This is not part of v2.0. The op format already
  allows the server to filter by path.
- **Outcome and request retention.** Choose the bounds: last N outcomes per session,
  and the time-to-live for finished global requests that are never acknowledged.
- **Multiple streams.** A client that changes its observed session opens a new
  stream. If that proves costly, add a subscription-update command for an existing
  stream (keyed by a stream ID) in a later minor version.
