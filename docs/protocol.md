# attyd protocol v2

Status: specification, 2026-10-14. Not implemented yet. This document defines the
client ↔ bridge protocol settled in [client-api-redesign.md](client-api-redesign.md),
which holds its rationale, derivations and acceptance cases. It replaces the earlier
v2 draft ([`11d04d0`](https://github.com/tf4fun/attyd/blob/11d04d0/docs/protocol.md)),
which was never completed and whose implementation is removed, and then `/api/v1`.
The
[implementation plan](state-sync.md) describes how the bridge produces it. The
[ACP coverage ledger](acp-coverage.md) lists the capabilities it must carry.

## 1. Model

attyd is a mediator without a persistence layer:

```text
client  <  bridge memory  <  ACP server
```

The bridge is an ordinary ACP client toward the ACP server; that layer is ACP's and
is not specified here. Toward clients, the bridge is the single leader of a
replication model in the manner of Raft: clients are read-only replicas of its
memory and send writes to it.

The protocol is defined by ten rules. They are restated from
[client-api-redesign.md §3](client-api-redesign.md#3-rules); the rest of this
document specifies their fields, endpoints and behavior.

**Bridge rules**

- **S1 Document.** Every scope is one document with two parts: `log`, an
  append-only sequence of entries, and `live`, one JSON value. Each log entry has a
  chain hash `h_i = H(h_{i-1} ‖ canon(entry_i))`, with `h_0` fixed. A session
  scope's log is its turn history; every other scope's log is always empty. A
  session's log is known once the bridge holds its history (from creation, or
  once its first load completed); until then it is unknown, not empty.
- **S2 Commit.** Only a completed turn enters the log. Appending its entry, removing
  its process from `live` and moving its operation to a final status are one event.
  Every other change happens in `live` only.
- **S3 Alignment.** A client subscribes with the head of the log it holds,
  `(n, h_n)`, the subscription's baseline. The bridge sends one cut as one event:
  the whole of `live` and, once the log is known, its log from the baseline if it
  has hash `h_n` at index `n`, otherwise from `0`; while the log is unknown, the
  client keeps its own, unconfirmed. Taking the cut and continuing is one serial
  step. The baseline advances only with log changes handed to the subscriber's
  output stream; every log change is computed from it and names the position it
  follows. A client that cannot apply one subscribes again with the head it holds.
- **S4 Operation.** All bridge work that may wait for a client is an operation of a
  scope. Every scope's `live` holds exactly one operation `op`, whose `id` is random
  and never reused. The ID identifies the operation and is what writes are admitted
  against; it says nothing about what a replica has seen.
- **S5 Admission.** Every operation write names `expect`, and the bridge handles
  writes and loads one at a time. A write that starts an operation needs
  `expect == op.id`, `op` ended, and every other scope it acts on free; the new
  operation replaces `op` in the same step. A write that acts on the running
  operation (cancel, answer) needs `expect == op.id`, `op` running and a kind that
  accepts it; an answer must cover exactly the pending interactions. An admitted
  operation occupies every scope it acts on until it actually ends, a requested
  cancel included: it is the `op` of each held one and holds back the
  load of each unheld one. Anything else is refused.
- **S6 Holding.** The bridge holds a session while it is needed: subscribed,
  occupied, or with a live terminal or URL flow. It releases a session that is not
  needed only if the Agent can load it again; otherwise it keeps it until it is
  deleted or the Agent connection ends. Subscribing to an unheld session starts a
  `load` operation. A one-off read holds nothing.
- **S7 Resources.** Terminals and URL flows live in their scope's `live` by their
  own lifecycle; an operation's end clears its pending interactions but ends no
  resource. Resource actions take no `expect`, fall outside S5, and state their
  repeat semantics; terminal input is not idempotent.

**Client rules**

- **C1 Projection.** A client's presentation is a pure function of `log`, `live`
  and its own UI state. It holds no authoritative state.
- **C2 Refusal.** A refused write leaves its input with the user. Queued input that
  was never sent, or was refused with a definite answer, may be submitted
  automatically once the replica shows the operation ended.
- **C3 Uncertainty.** A write or terminal input sent without a definite answer may
  have taken effect. The client never resends it automatically.

**Conventions.** All paths are under `/api/v2`. JSON field names are camelCase.
Embedded ACP types (`ContentBlock`, `SessionUpdate`, `RequestPermissionRequest`,
`InitializeResponse`, …) are carried verbatim, as the ACP schema pinned in
`Cargo.lock` defines them. Types the bridge defines itself, such as
`AttachmentRef` (§3.1), are never presented as ACP types.

| Part | Transport | Section |
| --- | --- | --- |
| Documents | in stream events and one-off `GET` | §2–§4 |
| Stream | SSE | §5 |
| Operations | `POST`, one endpoint | §6 |
| Resource actions | `POST` | §7 |
| Reads | `GET` | §8 |

## 2. Scopes

| Scope | Exists | Log |
| --- | --- | --- |
| `global` | always | empty |
| `catalog` | always | empty |
| `session/<sessionId>` | while the bridge holds the session | the session's turns |

A `session/<id>` scope is named by session ID only. Holding the same session again
continues the same scope name; S3 decides what a client keeps.

## 3. Log

### 3.1 Entries

```ts
interface Turn {
  prompt: Block[];          // the user's message, normalized (§3.2)
  reply: Block[];           // the Agent's message content, in order, normalized
}

type Block =                // normalized content
  | Extract<ContentBlock, { type: "text" | "resource_link" }>   // ACP, verbatim
  | AttachmentRef;

interface AttachmentRef {   // the bridge's own type, not an ACP ContentBlock
  type: "attachment";
  id: string;               // lowercase hex SHA-256 of the block's canonical JSON
  block: "image" | "audio" | "resource";   // the type of the block it stands for
  mimeType?: string;        // from the block, when it has one
  uri?: string;             // a resource's URI
}

type PromptBlock = ContentBlock | AttachmentRef;   // what a prompt may contain (§6.2)

interface LogEntry {
  turn: Turn;
  hash: string;             // h_i, encoded as in §3.3
}
```

A turn enters the log when the Agent answers its `session/prompt`, with any stop
reason, `cancelled` included. A prompt that fails with an error adds no entry; its
prompt and error stay on the operation (§4.4).

### 3.2 `canon`

`canon(turn)` is computed in three steps. The live commit and the reconstruction
from a `session/load` or `session/resume` replay apply the same steps.

1. **Messages.** `user_message_chunk` and `agent_message_chunk` updates are reduced
   to logical messages. Chunks of the same kind belong to one message when they
   carry the same `messageId`. Without `messageId`, they belong to one message when
   they are consecutive, with no other message chunk between them: process updates
   (thoughts, tool calls, tool call updates, plans) and control updates do not
   separate them.
2. **Turns.** A user message starts a turn; for a live turn, the prompt is the
   operation's input. The Agent messages up to the next user message are its
   reply, concatenated in order. Agent messages before the first user message form
   a turn with an empty prompt. Process updates are not part of any entry.
3. **Content.** `prompt` and `reply` are normalized by the same rules:
   - adjacent text blocks are merged into one when, `_meta` dropped, they differ
     only in `text`; blocks with different annotations keep their boundary;
   - image, audio and embedded resource blocks are replaced by an `AttachmentRef`
     (§3.1), whose `id` is the lowercase hex SHA-256 of the block's canonical JSON
     with `_meta` dropped; resource links stay as they are;
   - `_meta` fields are dropped at every level;
   - the result is serialized as canonical JSON (RFC 8785, JCS).

### 3.3 Hash chain

- `h_0` is 32 zero bytes. `h_i = SHA-256(h_{i-1} ‖ JCS(turn_i))`.
- On the wire, a hash is the unpadded base64url encoding of its first 16 bytes
  (22 characters). Comparisons use this form.
- Two logs with the same hash at index `i` are treated as equal up to `i`.

Hashes serve alignment only. Admission never compares them.

## 4. Live documents

### 4.1 Common fields

```ts
interface ScopeLive {
  op: Operation;
  urlFlows: Record<string, UrlFlow>;      // by interactionId (§4.6)
}
```

Every scope's `live` extends `ScopeLive`. A new scope gets both fields.

### 4.2 Session

```ts
interface SessionLive extends ScopeLive {
  sessionId: string;
  cwd: string;
  controls: SessionControls;              // reduced state (§4.5)
  terminals: Record<string, Terminal>;    // Agent terminals not yet released
}
```

### 4.3 Global and catalog

```ts
interface GlobalLive extends ScopeLive {
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
    initialize: InitializeResponse | null;  // ACP, verbatim
    authenticatedMethodId: string | null;
  };
  sessions: Record<string, SessionSummary>; // sessions the bridge holds
  terminals: Record<string, LoginTerminal>; // terminal logins, by operation ID
}

interface LoginTerminal extends Terminal {
  methodId: string;                         // the authentication method it runs
}

interface SessionSummary {
  sessionId: string;
  cwd: string;
  title: string | null;                     // controls.info.title
  op: { kind: string; status: OperationStatus };
  pending: { interactions: number; urlFlows: number };
}

interface CatalogLive extends ScopeLive {
  revision: string;                         // changes when the Agent's list may have changed
}
```

### 4.4 Operations

```ts
type OperationStatus = "running" | "completed" | "failed" | "cancelled";

interface Operation {
  id: string;                               // random, never reused (§6.5)
  kind: OperationKind;
  status: OperationStatus;
  input: unknown;                           // the write's `args` (a prompt as normalized
                                            // `Block[]`), or the load's cwd
  cancelRequested: boolean;
  interactions: Record<string, Interaction>;  // pending, by interactionId
  process?: SessionUpdate[];                // prompt while running: its updates, folded
  result?: unknown;                         // completed (§6.2)
  error?: ProtocolError;                    // failed
}
```

- `op` keeps its final status, result or error until the next operation replaces
  it. Nothing else retains results.
- `process` exists only on a running `prompt`. Entries fold in place as today: a
  `tool_call_update` folds into its `tool_call`, chunks of one message merge, a
  plan replaces the previous plan. The commit removes it (S2).
- `cancelled` means the operation ended because of a cancel before it produced a
  result. A prompt the Agent answered with stop reason `cancelled` is `completed`
  with that stop reason.
- **First operations.** A session the bridge starts holding by subscription starts
  with a running `load`. A session created by `create` or `fork` starts with a
  completed `open`. `global` and `catalog` start with a completed `none`.
- **Agent connection.** When the Agent connection ends, every running operation
  ends `failed` with `unavailable`, and pending interactions end with it.

```ts
type Interaction =
  | { interactionId: string; kind: "permission"; request: RequestPermissionRequest }
  | { interactionId: string; kind: "elicitation"; request: CreateElicitationRequest };
```

Interaction IDs are allocated by the bridge and never reused. An Agent request does
not change after it arrives; it leaves `interactions` when answered, when the Agent
cancels it, or when its operation ends.

### 4.5 Controls

`controls` is the **reduced current state** of the session's controls, folded from
three sources in this order:

1. **Early updates**: control notifications that arrive before the `session/new`,
   `load`, `resume` or `fork` response, in arrival order.
2. **The response**, which overrides exactly the fields it provides (`modes`,
   `configOptions`). Fields it does not provide keep their early value.
3. **Later updates and acknowledgements**, in arrival order. A successful
   `set_mode` or `set_config` operation folds its acknowledgement when its response
   arrives, after every notification that arrived before it, whether or not the
   Agent also sent a notification. A failed one folds nothing.

```ts
interface SessionControls {
  modes: { currentModeId: string; availableModes: SessionMode[] } | null;
  configOptions: SessionConfigOption[] | null;  // null: never provided; []: provided empty.
                                                // Clients prefer configOptions over legacy
                                                // modes whenever it is non-null (ACP).
  commands: AvailableCommand[];
  info: { title: string | null; updatedAt: string | null; meta: unknown | null };
  usage: { used: number; size: number; cost: Cost | null } | null;
}
```

| ACP source | Reduction |
| --- | --- |
| response `modes` | sets `modes` |
| `current_mode_update` | sets `modes.currentModeId` |
| response `configOptions`, `config_option_update` | replace `configOptions` as a whole; an explicit `[]` stays `[]` |
| `available_commands_update` | replaces `commands` as a whole |
| `session_info_update` | per field: absent keeps the value, `null` clears it, a value sets it |
| `usage_update` | sets `used` and `size`; `cost` absent or `null` keeps the last known cost |
| successful `set_mode` | sets `modes.currentModeId` to the requested `modeId` |
| successful `set_config` | replaces `configOptions` with the response's `configOptions` |

### 4.6 Resources

```ts
interface UrlFlow {
  interactionId: string;                    // the accepted elicitation; identifies the flow
  elicitationId: string;                    // Agent-chosen; may be reused after the flow ends
  request: CreateElicitationRequest;
}

interface Terminal {
  terminalId: string;                       // session: the Agent's ID; global: the operation ID
  output: string;                           // bounded tail (`trim`, §5.3)
  truncated: boolean;
  exitStatus: { exitCode: number | null; signal: string | null } | null;
}
```

| Resource | Created | Removed |
| --- | --- | --- |
| URL flow | a client accepts a URL elicitation; the interaction leaves `interactions` in the same step. Accepting only consents. | the Agent's `elicitation/complete` for its `elicitationId` (the one flow with that ID); a client's dismissal (§7); its scope ending; the Agent connection ending |
| session terminal | the Agent's `terminal/create` | the Agent's `terminal/release`; its scope ending; the Agent connection ending |
| terminal login | a `terminal_login` operation starts; it keeps its `methodId` for its whole life, whatever operation follows | it has exited and a client dismisses it (§7); the Agent connection ending |

No completion record is kept. A live resource keeps its session needed (S6).

## 5. Stream

### 5.1 Subscribing

```text
GET /api/v2/stream?scope=global&scope=session/S&have=session/S:12:<hash>&cwd=session/S:<path>
```

| Parameter | Meaning |
| --- | --- |
| `scope` | a scope to follow; repeatable |
| `have` | `<scope>:<n>:<h_n>`, the head of the log the client holds now for a session scope; without it the head is `(0, h_0)`. It is the subscription's baseline (S3). |
| `cwd` | `<scope>:<path>`, the working directory to load an unheld session with (S6); ignored for a held session |

One stream carries any set of scopes. To change the set, a client opens a new
stream and ignores every event of the one it replaced.

Subscribing to a session scope:

- **held, log known:** the scope is aligned at once, log and `live`;
- **held, log unknown** (another client's subscription started the first load,
  which still runs): the alignment carries `live` only, and the subscription
  follows the load as below;
- **not held, free:** the bridge starts holding it with a running `load`. Its log is
  unknown, so the alignment carries `live` only: the client keeps its log,
  unconfirmed, and shows and answers the load's interactions. When the load ends,
  one event carries the log from the baseline (the suffix if the baseline matches,
  else the whole log) with the patch that ends the load. A missing `cwd` ends the
  scope with `load_failed` (`invalid_request`);
- **not held, occupied** (a delete runs on it, §6.3): the subscription waits until
  the occupying operation ends, then gets `end: deleted` or starts the load.

Subscribing to `global` or `catalog` holds nothing.

### 5.2 Events

Each SSE message carries one JSON event. Events carry no SSE `id`; there is no
resume.

```ts
type Event =
  | { scope: string; log: LogChange; live?: LiveChange }
  | { scope: string; live: LiveChange }
  | { scope: string; end: { reason: "deleted" | "load_failed"; error?: ProtocolError } };

interface LogChange {
  from: number;             // truncate the log to this length, then append
  prev: string;             // h_from, as the client must hold it
  entries: LogEntry[];
}

type LiveChange = { replace: unknown } | { patch: Op[] };
```

A client applies an event to its document for `scope`:

1. **`log`.** If `from` exceeds its log length, or its hash at `from` is not `prev`,
   the client closes the stream and subscribes again with the head it holds (S3).
   Otherwise it truncates to `from` entries and appends `entries`; its log is then
   confirmed.
2. **`live`.** `replace` installs the value; `patch` applies the ops in order.
3. **`end`.** The client drops the document. Nothing follows `end` for that scope
   on this stream.

Both parts of one event are applied together. After every event the client's
`live` equals a `live` the bridge held at some moment, and its log equals the
bridge's log at that moment, or, while that log is unknown, the client's own.

| Event | When |
| --- | --- |
| `{ log, live: { replace } }` | **alignment**: the first event of every subscribed scope, and every resync (§5.4). `from` is the matched index, or `0`. While the log is unknown, the alignment is `{ live: { replace } }`. |
| `{ log, live: { patch } }` at a first load's end | the log from the baseline, with the patch that ends the load |
| `{ log, live: { patch } }` | **commit** (S2): `from` is the previous length; the patch ends the operation and removes its process. Also a rebuilt log after a reload (`from` is the longest common prefix with the log last sent). |
| `{ live: { patch } }` | every other change |
| `{ end }` | the session was deleted, or its load failed |

### 5.3 Ops

`patch` ops are JSON Pointer operations on `live`:

```ts
type Op =
  | { op: "set";    path: string; value: unknown }   // create or replace; an array index modifies in place
  | { op: "remove"; path: string }                   // map key or array index
  | { op: "append"; path: string; values: unknown[] }// array tail
  | { op: "append"; path: string; text: string }     // string tail
  | { op: "trim";   path: string; count: number };   // drop the first `count` items or UTF-16 units
```

`set` requires the parent to exist; every other op requires its target to exist. An
op that does not apply is a protocol error, and the client subscribes again. The
bridge picks the op that matches the change: growth uses `append`, an entry folded
in place uses `set` on its index, a bounded tail moving forward uses `trim` plus
`append`, anything else `set` on the smallest enclosing value.

### 5.4 Synchronization

| Situation | What happens |
| --- | --- |
| first open, reconnect after a break, bridge restart | the client subscribes with the head it holds; one alignment event |
| the Agent's replay after a reload differs from the client's log | the head does not match; the alignment sends the whole log and the client replaces its own |
| a slow subscriber | the bridge drops the subscriber's queued events of each scope that still exists and queues one alignment from the baseline, the last log position handed to the output stream (a change still in the queue never advances it); an ended scope keeps its `end` |
| a cold reopen | the client's log stays, unconfirmed, while the session loads; the load's end confirms it with the suffix, or replaces it when the history changed |
| the Agent connection ends | each subscribed session's `op` becomes a running `load` waiting for the connection; `global.agent` shows why; a session the Agent cannot load again ends with `load_failed` |
| the stream breaks | the client subscribes again with the head it holds then. It does not let `EventSource` retry the original URL, whose `have` may no longer be what it holds. |

The bridge sends an SSE comment at least every 15 seconds on an idle stream.

## 6. Operations

### 6.1 Request

```text
POST /api/v2/scopes/{scope}/ops
{ "expect": "<op.id>", "kind": "<kind>", "args": { … } }
```

`{scope}` is URL-encoded (`session%2FS`).

### 6.2 Kinds

| Scope | Kind | `args` | Occupies | Allowed while | Result |
| --- | --- | --- | --- | --- | --- |
| session | `prompt` | `{ prompt: PromptBlock[] }` | the session | `op` ended | `{ stopReason, usage? }` |
| session | `set_mode` | `{ modeId }` | the session | `op` ended | `{}` |
| session | `set_config` | `{ configId, value }` | the session | `op` ended | `{}` |
| session | `fork` | `{ cwd? }` (default: the source's `cwd`) | the source | `op` ended | `{ sessionId }` |
| global | `create` | `{ cwd }` | `global` | `op` ended | `{ sessionId }` |
| global | `delete` | `{ sessionId }` | `global`, `session/<sessionId>` | `op` ended, and the session free | `{}` |
| global | `authenticate` | `{ methodId }` | `global` | `op` ended | `{}` |
| global | `logout` | `{}` | `global` | `op` ended | `{}` |
| global | `terminal_login` | `{ methodId, cols, rows }` | `global` | `op` ended | `{ exitCode, signal }` |
| catalog | `list` | `{ cursor?, revision? }` | `catalog` | `op` ended | `{ revision, sessions: SessionInfo[], nextCursor }` |
| any | `cancel` | `{}` | — | `op` running, of kind `prompt`, `terminal_login` or `agent_request` | — |
| any | `answer` | `{ answers: Record<interactionId, Answer> }` | — | `op` running, keys equal to `op.interactions` | — |

```ts
type Answer =
  | { outcome: RequestPermissionOutcome }    // permission
  | { response: CreateElicitationResponse }; // elicitation
```

- **Bridge-started kinds.** `load` (a subscription, §5.1), `open` (the first
  operation of a created session), `none` (the first operation of `global` and
  `catalog`) and `agent_request` (below) are never written by clients.
- **Prompt content.** A prompt may contain `AttachmentRef`s to attachments of the
  session. The bridge resolves each to the ACP block it names before it calls
  `session/prompt`; an unknown ID is refused with `invalid_request`. The
  operation's `input.prompt` is the normalized `Block[]`.
- **Cancel** applies to `prompt` (`session/cancel`), `terminal_login` (the login
  process is stopped) and `agent_request` (every pending interaction is answered
  with its cancel outcome). Other kinds refuse it with `not_allowed`. A cancel sets
  `cancelRequested`; repeating it changes nothing. Requesting a cancel does not end
  the operation: it keeps its scopes occupied until it actually ends.
- **Answer** delivers each answer as the response to its Agent request. Accepting a
  URL elicitation moves it to `urlFlows` (§4.6).
- **Fork** creates a session the bridge holds, starting with a completed `open`.
- **Delete** closes a held session (`session/close`, when advertised), deletes it
  (`session/delete`), and ends `session/<sessionId>` with `deleted`. It needs the
  Agent's delete capability. It works on sessions the bridge does not hold.
- **List** runs the Agent's `session/list`. A `revision` that is not
  `catalog.live.revision` is refused with `catalog_changed`.
- **Agent requests.** An Agent request (permission or elicitation) joins the
  running operation of its scope: the scope's own, or the operation that occupies
  it. A delete's `session/close` may ask for input in the session it occupies; that
  interaction joins the delete and is answered like any other. If no operation
  runs, the request starts an `agent_request` operation in its scope. An occupied
  held scope always shows its occupying operation, so there is no other case. A
  request-scoped elicitation belongs to the scope of the operation whose ACP
  request it names. An `agent_request` completes when its interactions are gone.

### 6.3 Response

| Outcome | Status | Body |
| --- | --- | --- |
| admitted | `202` | `{ op: "<id>" }`: the new operation's ID, or the running one's for cancel and answer |
| `create`, `fork`, `list` | `200` | `{ op, result }` or `{ op, error }`, sent when the operation ends |
| refused | §9 | `{ error: ProtocolError }` |

- A `202` means admitted, nothing more. The replica shows the bridge's current
  state, which may lag behind or already be past the write.
- `create`, `fork` and `list` hold the response until the operation ends, including
  while it waits for interactions. Clients give these requests no short timeout. A
  lost response loses the result for that client: a created session stays listed in
  `global.sessions` while held and in the catalog.

### 6.4 Admission

The bridge handles operation writes and loads one at a time:

| Write | Admitted when | Otherwise |
| --- | --- | --- |
| starts an operation | `expect == op.id`, `op` ended, every other scope in "Occupies" free | `stale` if `expect` differs; `not_allowed` otherwise |
| `cancel`, `answer` | `expect == op.id`, `op` running, its kind accepts the write (§6.2), and for `answer` the keys equal `op.interactions` | `stale` if `expect` differs; `not_allowed` otherwise |

Capability, argument and Agent checks follow: `unsupported`, `invalid_request`,
`unavailable`. A refused write changes nothing.

An admitted operation is the `op` of every held scope it occupies, with the same
`id`, until it actually ends; an unheld occupied session cannot be loaded meanwhile. A scope
is free when its `op` has ended and no operation occupies it.

### 6.5 Operation IDs

Operation IDs are 128-bit random values in unpadded base64url. They never repeat,
across scopes, Agent connections and bridge restarts.

## 7. Resource actions

```text
POST /api/v2/scopes/{scope}/resources/{resourceId}/{action}
```

Resource actions take no `expect` and are not operations (S7). They answer `204`, or
`404` when the resource does not exist.

| Resource | Action | Body | Repeated |
| --- | --- | --- | --- |
| terminal login (`global`) | `input` | `{ data }` | **not idempotent**: the bytes are written to stdin each time, in the order the bridge receives them |
| terminal login (`global`) | `resize` | `{ cols, rows }` | sets the size; the same size leaves the same state |
| terminal login (`global`) | `dismiss` | `{}` | only after exit; removes it; a repeat gives `404` and changes nothing |
| URL flow (any scope) | `dismiss` | `{}` | removes it; a repeat gives `404` and changes nothing |

A client sends terminal input in order, one request at a time per terminal, and
never resends input whose delivery is uncertain (C3).

## 8. Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/scopes/{scope}` | `{ live, log? }` of an existing scope, as one cut; `404` for an unheld session; holds nothing. `log` is omitted while the session's log is unknown (S1); an empty log is `[]`, and the two never stand for each other. |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | the block, content-addressed; served while the session's `log` or `live` references it, else `404` |
| `GET /api/v2/sessions/{id}/files?query=` | context search within the session's workspace |
| `GET /api/v2/sessions/{id}/files/content?path=` | context read |

The catalog is listed by the `list` operation (§6.2), not by a read, because the
Agent may ask for input while it lists. **Markdown export** is assembled by the
client from the log and the attachment read.

## 9. Errors

```ts
interface ProtocolError {
  code: ErrorCode;
  message: string;
  data?: unknown;           // agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier
  | "not_found"             // 404: unknown scope or resource
  | "stale"                 // 409: `expect` is not the current op.id
  | "not_allowed"           // 409: the operation's status, the occupied scopes or the
                            //      pending interactions do not allow the write
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "unavailable"           // 503: no Agent connection
  | "agent_error";          // on op.error and end.error: the Agent returned an error
```

The body of every non-2xx response is `{ error: ProtocolError }`. A stream that
fails ends; the client subscribes again with its heads.

## 10. Changes from the earlier v2 draft

| Earlier v2 draft | v2 |
| --- | --- |
| `Position`, `epoch`, `seq`, `Last-Event-ID`, `at`, `prev` positions | none; alignment by log head (§5) |
| `snapshot`, `reset` plus `GET /sessions/{id}` | alignment event `{ log, live: { replace } }` |
| `ops` | `{ live: { patch } }`, or a commit `{ log, live: { patch } }` |
| `end` (`closed`, `unobserved`, `replaced`, `gone`) | `end` (`deleted`, `load_failed`); a released session is loaded again on subscription |
| `position` | SSE comments |
| `session/<id>@<epoch>:<incarnation>` | `session/<id>` |
| `history.timeline`, `collapsedTurns`, process pages | `log` of final messages (§3); `op.process` while running |
| `turn`, `outcomes`, `operation` | `op` |
| `global.requests`, request records, acknowledgements | `op` of each scope |
| `interactions`, `requestId` parents | `op.interactions`; `agent_request` |
| URL flow `status` and acknowledgement | removal on completion or dismissal (§4.6) |
| `Idempotency-Key`, intents, `If-Match` tags, `historyRevision` CAS | `expect` (§6.4) |
| `POST /sessions/{id}/open`, first-subscription lease | subscription with `cwd` (§5.1) |
| `POST /sessions/{id}/close` | release when not needed (S6) |
| per-command endpoints | `POST /scopes/{scope}/ops` |
| auth terminal input, resize, cancel | resource actions; `cancel` operation |
| `GET /api/v2/catalog` | `list` on `catalog` |
| `GET /api/v2/global`, `GET /api/v2/sessions/{id}` | `GET /api/v2/scopes/{scope}` |
| `owner_mismatch`, `history_mismatch`, `precondition_required`, `conflict`, `intent_conflict`, `intent_capacity`, `gone`, `interaction_ended` | `stale`, `not_allowed` |

For `/api/v1`, the earlier draft's
[mapping from v1](https://github.com/tf4fun/attyd/blob/11d04d0/docs/protocol.md#8-mapping-from-v1)
applies through this table.
