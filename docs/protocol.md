# 简易 Agent 交互协议

This is attyd's project-defined protocol between the FE (and other presentation
clients) and the bridge. The bridge remains an ordinary ACP client toward the Agent.
The `/api/v2` endpoint namespace and the revision numbers belong to this application
protocol, separately from ACP's versions.

Status: **revision 3 proposal, 2026-10-21; not implemented.** Layer 1, what the bridge
keeps in memory and how ACP input changes it, is defined only by the
[bridge memory model](bridge-memory-model.md). This document defines layer 2: how that
memory reaches clients and how clients write to it. Where the two disagree, the memory
model wins. Revision 2 (turn log with a hash chain) is at the fixed baseline linked in
the [history index](history.md); §11 lists what changed.

Related documents: [design rationale](client-api-redesign.md),
[numbered acceptance cases](protocol-acceptance.md),
[implementation plan](state-sync.md), [test evidence](acp-coverage.md).

## 1. Model

```text
client  <  bridge memory  <  ACP server
```

The bridge is the single authority for what clients show. Clients are replicas of its
memory and send writes to it. The protocol has three goals:

1. **Reliable, verifiable, recoverable synchronization.** A client can tell that its
   replica equals the bridge's memory at a known version, detects when it does not,
   and recovers by replacing what differs from the bridge.
2. **Push to every subscriber.** Every change to bridge memory reaches every client
   subscribed to that scope, in the bridge's order, without polling.
3. **Correct concurrent and repeated writes.** Several clients may write at once and
   may repeat a write whose answer was lost; each write takes effect at most once, and
   a client can find out from its replica whether it did.

**Bridge rules**

- **B1 Document.** Every scope is one JSON document (§3, §4). A session's document
  carries the session's memory as the memory model defines it, with byte content
  replaced by references (§3.1).
- **B2 Versions.** A document has an `epoch` and a `rev`. The `epoch` is a random value
  that changes whenever the document is rebuilt rather than changed (§5.4). Within an
  epoch, every change is one event that increases `rev` by exactly one. Every
  subscriber of a scope receives the same changes at the same `rev`s.
- **B3 Digest.** At stated points (§5.5) the bridge sends the digest of the document at
  a `rev`. Equal digests mean equal documents.
- **B4 Recovery.** A client that cannot apply an event, or whose digest disagrees,
  subscribes again with what it holds. The bridge answers with a snapshot: only what
  changed since the client's `rev` when its epoch is current, else the whole document.
- **B5 Writes.** A write is an operation or an action. An **operation** occupies scopes
  and is admitted against the current operation (`expect`); its ID is chosen by the
  client, so repeating it is harmless (§6). An **action** acts on something that exists
  now; each states its repeat semantics (§7).
- **B6 Holding.** The bridge holds a session while it is needed (memory model §8.4) or
  subscribed. A session that nothing needs is released after
  `--session-unobserved-timeout`, only if the Agent can load or resume it. Subscribing
  to an unheld session starts a `load`. A one-off read holds nothing.

**Client rules**

- **C1 Projection.** A client's presentation is a pure function of its replica and its
  own UI state (drafts, folding, selection). Turns are derived from entries (§3.3).
- **C2 Refusal.** A refused write leaves its input with the user. Queued input that was
  never sent, or was refused with a definite answer, may be submitted automatically once
  the replica shows the operation ended.
- **C3 Uncertainty.** A write whose answer was lost is resolved from the replica where
  §6.6 says it can be; an interaction answer may be repeated (§7). Anything else whose
  effect is unknown is left to the user, never resent automatically.

**Conventions.** All paths are under `/api/v2`. JSON field names are camelCase. Embedded
ACP types (`ContentBlock`, `ToolCallUpdate`, `PromptResponse`, `RequestPermissionRequest`,
`InitializeResponse`, …) are carried as the ACP schema pinned in `Cargo.lock` defines
them, `_meta` included. Types the bridge defines itself are never presented as ACP types.

| Part | Transport | Section |
| --- | --- | --- |
| Documents | in stream events and one-off `GET` | §2–§4 |
| Stream | SSE | §5 |
| Operations | `POST`, one endpoint | §6 |
| Actions | `POST` | §7 |
| Reads | `GET` | §8 |

## 2. Scopes

| Scope | Exists | Document |
| --- | --- | --- |
| `global` | always | `GlobalDoc` (§4.3) |
| `catalog` | always | `CatalogDoc` (§4.3) |
| `session/<sessionId>` | while the bridge holds the session | `SessionDoc` (§3, §4.2) |

A `session/<id>` scope is named by session ID only. Holding the same session again
continues the same scope name with a new epoch.

## 3. Session history

```ts
interface SessionDoc {
  epoch: string;
  rev: number;
  history?: "complete" | "partial";   // absent: unknown (memory model §6.1)
  entries?: Entry[];                  // absent exactly when history is unknown
  prompts: Prompt[];                  // memory model §4.2
  live: SessionLive;                  // §4.2
}
```

`entries` is absent while the bridge holds no history, and `[]` for a known empty one;
the two never stand for each other.

### 3.1 Entries

An `Entry` is the memory model's entry (memory model §3.1) with one change: a content
block that carries bytes (image, audio, embedded resource) is replaced by an
`AttachmentRef`, read separately (§8).

```ts
interface AttachmentRef {   // the bridge's own type, not an ACP ContentBlock
  type: "attachment";
  id: string;               // lowercase hex SHA-256 of the block's exact form (§8)
  block: "image" | "audio" | "resource";
  mimeType?: string;
  uri?: string;             // a resource's URI
}

type Block = ContentBlock /* with byte-carrying blocks as AttachmentRef */;
type PromptBlock = ContentBlock | AttachmentRef;   // what a prompt may contain (§6.2)
```

The `id` identifies the block exactly as received, `_meta` included, with no numeric
rounding (§8); reading it returns that block unchanged, and a client can verify the
read against the `id`. Two blocks that differ anywhere, also in an integer beyond
±2^53, have different IDs.
Text and resource links stay inline.

### 3.2 Prompts

```ts
interface Prompt {
  entry: number;            // index of the user entry the prompt sent
  end?:                     // absent while it runs
    | { response: PromptResponse }   // ACP, verbatim
    | { error: ProtocolError }       // the Agent answered with an error (`agent_error`)
    | { interrupted: true };         // no answer: the Agent connection ended
}
```

### 3.3 Turns

Turns are not sent. A client derives them from `entries` with the memory model's rule
(memory model §5), implemented once in `shared/` and tested against the same fixtures
as the bridge. A `Prompt` belongs to the turn its `entry` starts.

## 4. Live documents

### 4.1 Common fields

```ts
interface Doc<Live> { epoch: string; rev: number; live: Live }

interface ScopeLive {
  op: Operation;                               // §4.4
  interactions: Record<string, Interaction>;   // pending, by interactionId (§4.4)
  urlFlows: Record<string, UrlFlow>;           // by interactionId (§4.6)
}
```

### 4.2 Session

```ts
interface SessionLive extends ScopeLive {
  sessionId: string;
  cwd: string;
  controls: SessionControls;                   // §4.5
  terminals: Record<string, Terminal>;         // Agent terminals (§4.6)
  closing: boolean;                            // a close request is outstanding (§7)
}
```

### 4.3 Global and catalog

```ts
type GlobalDoc = Doc<GlobalLive>;
type CatalogDoc = Doc<CatalogLive>;

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
    initialize: InitializeResponse | null;     // ACP, verbatim
    authenticatedMethodId: string | null;
  };
  sessions: Record<string, SessionSummary>;    // sessions the bridge holds
  terminals: Record<string, LoginTerminal>;    // terminal logins, by operation ID
  diagnostics?: Diagnostics;                   // later proposal, only if adopted (§4.7)
}

interface SessionSummary {
  sessionId: string;
  cwd: string;
  title: string | null;                        // controls.info.title
  op: { kind: string; status: OperationStatus };
  pending: { interactions: number; urlFlows: number };
  origin?: string;                             // ID of the create/fork that installed it (§6.6)
}

interface LoginTerminal extends Terminal {
  methodId: string;
}

interface CatalogLive extends ScopeLive {
  revision: string;                            // changes when the Agent's list may have changed
  pages: Array<{ sessions: SessionInfo[]; nextCursor: string | null; _meta?: unknown }>;
                                               // pages listed since `revision` last changed, in order
}
```

### 4.4 Operations and interactions

```ts
type OperationStatus = "running" | "completed" | "failed" | "cancelled";

interface Operation {
  id: string;               // chosen by the client that wrote it (§6.5); bridge-chosen for load/open/none
  scope: string;            // the scope it was written to (its issuing scope)
  after: string | null;     // the `expect` it was admitted with: the issuing scope's previous `op.id` (§6.6)
  kind: OperationKind;
  status: OperationStatus;
  input: unknown;           // the write's `args` (a prompt as `Block[]`), or the load's cwd
  cancelRequested: boolean;
  result?: unknown;         // completed (§6.2)
  error?: ProtocolError;    // failed
}
```

- `op` keeps its final status, result or error until the next operation replaces it.
- `cancelled` means the operation ended because of a cancel before it produced a
  result. A prompt the Agent answered with stop reason `cancelled` is `completed`.
- **First operations.** A session held by subscription starts with a running `load`; a
  session installed by `create` or `fork` starts with a completed `open`; `global` and
  `catalog` start with a completed `none`. These have `after: null`.
- When the Agent connection ends, every running operation ends `failed` with
  `unavailable`.

```ts
interface Interaction {
  interactionId: string;    // allocated by the bridge, never reused
  kind: "permission" | "elicitation";
  request: RequestPermissionRequest | CreateElicitationRequest;   // ACP, verbatim
  parent?: { requestId: RequestId; method: string };              // request-scoped elicitation
}
```

Scopes and lifetimes are memory model §8.2. An interaction is shown in the document
of its scope: its session; for a request-scoped elicitation, the scope of the write or
load that sent the request (`global` when none did); for a session being installed,
the scope of the installing operation (`global`, which also runs `fork`, memory model
§9) until installation, when it moves into `session/<id>`.

### 4.5 Controls

The reduction is memory model §7. Wire form:

```ts
interface SessionControls {
  modes: { currentModeId: string; availableModes: SessionMode[]; _meta?: unknown } | null;
  configOptions: SessionConfigOption[] | null;   // null: never provided; []: provided empty
  commands: AvailableCommand[];
  info: { title: string | null; updatedAt: string | null };
  usage: { used: number; size: number; cost: Cost | null } | null;
  meta: {                   // source metadata of the latest source that set each control (D12)
    [control in "modes" | "configOptions" | "commands" | "info" | "usage"]?:
      { update?: unknown; notification?: unknown };
  };
}
```

Clients prefer `configOptions` over legacy `modes` whenever it is non-null.

### 4.6 Resources

Lifetimes are memory model §8.3. Wire form:

```ts
interface UrlFlow {
  interactionId: string;
  elicitationId: string;
  request: CreateElicitationRequest;
  status: "waiting" | "completed" | "ended";
}

interface Terminal {
  terminalId: string;       // session: the ID returned to terminal/create; global: the operation ID
  output: string;           // bounded tail (`trim`, §5.3)
  truncated: boolean;
  exitStatus: { exitCode: number | null; signal: string | null } | null;
  released: boolean;        // session terminals: the handle is gone, output frozen
}
```

A frozen terminal stays in `terminals` while an entry embeds it. A terminal an entry
names but `terminals` lacks (for example after a load) is shown as unavailable.
A terminal login stays until it has exited and a client dismisses it, or the Agent
connection ends.

### 4.7 Diagnostics

A later presentation proposal, not part of the first milestone (plan step 19). If
adopted, `diagnostics` is one bounded part of `global.live`: the Agent stderr tail and
the last 200 relayed MCP-over-ACP messages, in memory only and interpreted by nothing.

```ts
interface Diagnostics {
  agentStderr: { output: string; truncated: boolean };
  mcp: {
    connections: Record<string, { serverId: string; name: string }>;
    activity: Array<{
      direction: "toAgent" | "fromAgent"; connectionId: string; method: string;
      kind: "request" | "notification" | "response";
      params?: unknown; result?: unknown; error?: { code: number; message: string; data?: unknown };
    }>;
    truncated: boolean;
  };
}
```

## 5. Stream

### 5.1 Subscribing

```text
GET /api/v2/stream?scope=global&scope=session/S&have=session/S:<epoch>:<rev>&cwd=session/S:<path>
```

| Parameter | Meaning |
| --- | --- |
| `scope` | a scope to follow; repeatable |
| `have` | `<scope>:<epoch>:<rev>`, the version of the replica the client holds and has verified or applied without error; omitted when it holds none, or after a failed verification (§5.5) |
| `cwd` | `<scope>:<path>`, the working directory to load an unheld session with (B6); ignored for a held session |

One stream carries any set of scopes. To change the set, a client opens a new stream
and ignores every event of the one it replaced. The first event of every scope is a
snapshot (§5.2).

Subscribing to a session scope:

- **held:** a snapshot at once;
- **held, history unknown** (another subscription's load still runs): a snapshot
  without `entries`; the load's end is a new epoch and a new snapshot;
- **not held, free:** the bridge starts holding it with a running `load`; the same as
  above. A missing `cwd` ends the scope with `load_failed` (`invalid_request`);
- **not held, occupied** by a delete: the subscription waits until the delete ends,
  then gets `end: deleted` or starts the load.

Subscribing to `global` or `catalog` holds nothing.

### 5.2 Events

Each SSE message carries one JSON event. Events carry no SSE `id`; there is no SSE
resume, since `have` does that job.

```ts
type Event =
  | { scope: string; snapshot: Snapshot }
  | { scope: string; rev: number; ops: Op[] }
  | { scope: string; rev: number; digest: string }              // checkpoint (§5.5)
  | { scope: string; end: { reason: "deleted" | "closed" | "load_failed"; error?: ProtocolError } };

interface Snapshot {
  epoch: string;
  rev: number;
  digest: string;                       // of the document after installing this snapshot
  doc: Omit<SessionDoc, "entries" | "epoch" | "rev"> | Omit<Doc<unknown>, "epoch" | "rev">;
                                        // everything but entries, complete
  entries?: {
    base: "replace" | "update";         // update: the client's entries, same epoch
    length: number;                     // the entry count after installing
    changed: Array<[index: number, entry: Entry]>;
  };
}
```

- A **snapshot** sets `epoch` and `rev` and replaces the document. With
  `base: "replace"`, `changed` holds every entry `0 … length-1`. With `base: "update"`
  (only when the subscriber's `have` named the current epoch), the client keeps its
  entries, extends them to `length`, and replaces the listed ones: exactly the entries
  changed after its `rev`. Entries are never removed within an epoch.
- An **ops event** with `rev = r` applies to a replica at `r - 1` and leaves it at `r`.
- A **checkpoint** states the digest at `rev` (§5.5).
- **`end`**: the client drops the document; nothing follows for that scope.

### 5.3 Ops

`ops` are JSON Pointer operations on the document, below `epoch` and `rev`
(`/entries/…`, `/prompts/…`, `/history`, `/live/…`):

```ts
type Op =
  | { op: "set";    path: string; value: unknown }    // create or replace; an array index modifies in place
  | { op: "remove"; path: string }                    // map key or array index
  | { op: "append"; path: string; values: unknown[] } // array tail
  | { op: "append"; path: string; text: string }      // string tail
  | { op: "trim";   path: string; count: number };    // drop the first `count` items or UTF-16 units
```

`set` requires the parent to exist; every other op requires its target to exist. The
bridge picks the op that matches the change: streamed text is `append` text on the
chunk's `text`; a new entry is `append` on `/entries`; a late update to an earlier
entry is `set` on that entry or its changed field; a bounded tail moving forward is
`trim` plus `append`; anything else is `set` on the smallest enclosing value.

### 5.4 Synchronization

The client applies events in order. Its replica is **current** at `(epoch, rev)` after
each successfully applied event, and **verified** at `rev` after a matching digest.

| Situation | What happens |
| --- | --- |
| first open | no `have`; a full snapshot |
| reconnect after a break, epoch unchanged | `have` with the current epoch; a snapshot with `base: "update"` carrying only the entries changed since, plus the rest of the document |
| an ops event whose `rev` is not the replica's `rev + 1`, or an op that does not apply | the client discards the event, closes the stream, and subscribes again with its last current `have` |
| a slow subscriber | per scope, the bridge drops that subscriber's queued events and queues one snapshot from the last `rev` actually written to its output; a queued event never counts as delivered. A scope whose queue holds an `end` keeps only that `end` and gets no snapshot |
| a load replaces the history, a session is reopened, the bridge restarts | new epoch; a full snapshot (`base: "replace"`) |
| a resume of a session whose history was unknown | new epoch; `history: partial` with only the content since |
| the Agent connection ends | the documents stay; each subscribed session's `op` becomes a running `load` waiting for reattachment (memory model §6.2); a session that cannot be reattached ends with `load_failed` |
| the stream breaks | the client subscribes again with its last current `have`. It does not let `EventSource` retry the original URL. |

A new epoch never reuses content from an earlier one: a client may keep an earlier
epoch's entries only as its own unconfirmed UI state (C1), for example while a reload
runs, and drops them at the snapshot.

The bridge sends an SSE comment at least every 15 seconds on an idle stream.

### 5.5 Verification

**Digest.** `canon` is JCS (RFC 8785) of a JSON value as both sides parse it. JCS
works in the IEEE double domain (RFC 8785 §3.1), so integers outside ±2^53 are
canonicalized as their double, the value a browser holds; the digest therefore cannot
see differences beyond double precision. That limit applies to document digests only,
not to attachment identity (§8). `H` is SHA-256; a digest on the wire is the unpadded
base64url of its first 16 bytes.

```text
e_i    = H(canon(entries[i]))
E      = H(e_0 ‖ e_1 ‖ … ‖ e_{n-1})             (absent entries: E = H("unknown"))
digest = H(canon({ epoch, rev, history, prompts, live }) ‖ E)
```

For `global` and `catalog`, `digest = H(canon({ epoch, rev, live }))`. Clients need a
synchronous SHA-256 implementation, since `crypto.subtle` is missing on non-secure
origins.

**Caching `e_i`.** A cached entry hash must be bound to the content it was computed
from, never to an index or a `rev` alone; otherwise an entry changed without an event
keeps a stale hash and passes verification. The client keeps replica entries
immutable (deep-frozen) and caches `e_i` by the entry value's identity, so every
applied change yields a new value and a cache miss. A full snapshot's verification
computes every `e_i` without the cache. The bridge, being the authority, may cache by
its own change tracking.

**When the bridge sends a digest:**

- in every snapshot, for the document after installing it;
- as a checkpoint after a scope's changes pause for 500 ms, and at least every 2 s while
  it keeps changing.

**What the client does.** At a snapshot, it installs into a copy, computes the digest,
and adopts the copy only if it matches. At a checkpoint, its replica is at that `rev`
(events are ordered), so it computes the digest and compares. On a mismatch it
discards its replica of that scope and subscribes again **without** `have`, so the
bridge sends the whole document. If a full snapshot fails verification again, the
client shows that synchronization failed and retries with backoff. A replica is shown
as confirmed only up to its last verified `rev`.

**Attachments** are verified by their `id` (§3.1, §8). A read whose block does not hash
to its `id` is unverified and retried; it is never shown as the attachment.

Verification detects accidental divergence: lost or reordered events, wrong patch
application, a stale or corrupted cache. It does not prove a faulty bridge truthful.

### 5.6 Several clients

- Every subscriber of a scope receives the same changes with the same `rev`s; at equal
  `(epoch, rev)` every verified replica equals the bridge's document at that version.
- Coalescing differs per subscriber (a slow one gets a snapshot) but never changes the
  `rev` a state has.
- Events of different scopes on one stream follow the bridge's order of the changes.
  There is no atomicity across scopes: installing a session adds it to
  `global.live.sessions` in one event and starts its document in another, and a client
  sees the new session's interactions only once it subscribes to it.
- A client's own write is not echoed separately: it sees its effect through the same
  events as every other client.

## 6. Operations

### 6.1 Request

```text
POST /api/v2/scopes/{scope}/ops
{ "id": "<new op id>", "expect": "<op.id>", "kind": "<kind>", "args": { … } }
```

`{scope}` is URL-encoded (`session%2FS`).

### 6.2 Kinds

| Scope | Kind | `args` | Occupies | Allowed while | Result |
| --- | --- | --- | --- | --- | --- |
| session | `prompt` | `{ prompt: PromptBlock[] }` | the session | `op` ended, not `closing` | `{ stopReason, usage? }` |
| session | `fork` | `{ cwd? }` (default: the source's `cwd`) | the source and `global` (memory model §9) | `op` ended, `global` free | `{ sessionId }` |
| global | `create` | `{ cwd }` | `global` | `op` ended | `{ sessionId }` |
| global | `delete` | `{ sessionId }` | `global`, `session/<sessionId>` | `op` ended, the session free | `{}` |
| global | `authenticate` | `{ methodId }` | `global` | `op` ended | `{}` |
| global | `logout` | `{}` | `global` | `op` ended | `{}` |
| global | `terminal_login` | `{ methodId, cols, rows }` | `global` | `op` ended | `{ exitCode, signal }` |
| catalog | `list` | `{ cursor?, revision? }` | `catalog` | `op` ended | `{ revision, page }`; the page is also appended to `catalog.live.pages` |
| any | `cancel` | `{}` | — | `op` running, of kind `prompt` or `terminal_login` | — |

- `load`, `open` and `none` are started by the bridge, never written by clients.
- **Prompt content.** A prompt may contain `AttachmentRef`s to attachments of the
  session; the bridge resolves each to the block it names before `session/prompt`. An
  unknown ID is refused with `invalid_request`.
- **Cancel** applies to `prompt` (`session/cancel`, which answers the session's pending
  permissions `cancelled`) and `terminal_login`. It sets `cancelRequested`; repeating it
  changes nothing; the operation keeps its scopes until it actually ends. A cancel is
  not an operation of its own: its `id` is ignored and `op` is unchanged.
- **Fork** and **create** install the returned session (memory model §6.3, §9); its
  summary carries `origin`, the operation's `id`.
- **Delete** closes a held session (`session/close`, when advertised), deletes it
  (`session/delete`), and ends `session/<sessionId>` with `deleted`. It needs the
  Agent's delete capability and works on sessions the bridge does not hold.
- **List** runs `session/list`. Without `cursor` it starts a new listing: `pages` is
  replaced by the first page. With `cursor` the page is appended. A `revision` that is
  not `catalog.live.revision` is refused with `catalog_changed`.

### 6.3 Response

| Outcome | Status | Body |
| --- | --- | --- |
| admitted, or a repeat of the current operation (§6.6) | `202` | `{ op: "<id>" }` |
| refused | §9 | `{ error: ProtocolError }` |

A `202` means admitted, nothing more; the result arrives in the document (`op.result`,
and for `create`/`fork`/`list` also `origin` or `pages`). No response is held open.

### 6.4 Admission

The bridge handles operation writes and loads one at a time:

| Write | Admitted when | Otherwise |
| --- | --- | --- |
| starts an operation | `id` is new, `expect == op.id`, `op` ended, every other scope it occupies free | `202` if `id == op.id` (a repeat, §6.6); `stale` if `expect` differs; `not_allowed` otherwise |
| `cancel` | `expect == op.id`, `op` running, its kind accepts a cancel | `stale` if `expect` differs; `not_allowed` otherwise |

Capability, argument and Agent checks follow: `unsupported`, `invalid_request`,
`unavailable`. A refused write changes nothing. An admitted operation is the `op` of
every held scope it occupies, with the same `id`, until it actually ends; an unheld
occupied session cannot be loaded meanwhile.

### 6.5 Operation IDs

A client generates each new operation's `id`: 128 random bits in unpadded base64url
(22 characters). A malformed `id` is `invalid_request`. A client uses a new `id` for
each new intent and the same `id` only to repeat a write whose answer it did not get.
IDs the bridge allocates (`load`, `open`, `none`) have the same form.

### 6.6 Repeats, races and lost answers

`expect` makes concurrent writes safe; the client-chosen `id` and the recorded `after`
make repeats safe and let a client resolve a lost answer from its replica, without a
table of past requests.

`after` is the predecessor **in the operation's issuing scope** only. An operation that
also occupies other scopes replaces their `op` too, but its `after` says nothing about
them. A client resolving a lost write therefore reads the `op` of the scope it wrote to
(`W`, with its `id` and `expect`), and uses `after` only of an operation whose `scope`
is `W`:

| Situation | Outcome |
| --- | --- |
| two clients start operations with the same `expect` | exactly one is admitted; the other gets `stale` and keeps its input (C2) |
| a client repeats a write (same `id`) while it is the scope's `op` | `202`, no effect |
| a client's answer was lost; the replica shows `op.id == id` | admitted |
| `W`'s `op.id == expect`, ended | not admitted (any admitted operation occupying `W` would have replaced it); the client may repeat with the same `id` |
| `W`'s `op` has `scope == W`, `after == expect` and another `id` | not admitted: that operation won the race in `W` (C2) |
| `W`'s `op` has `scope == W` and `after == id` | admitted, then followed by that operation |
| anything else, including an `op` written to another scope (for example a `fork` from a session that also occupies `global`) | unknown (C3): left to the user |
| a `create`/`fork` answer was lost | `global.live.sessions` shows the session whose `origin` is the `id` while it is held |

## 7. Actions

```text
POST /api/v2/scopes/{scope}/actions/{action}                    { …args }
POST /api/v2/scopes/{scope}/resources/{resourceId}/{action}     { …args }
```

Actions take no `expect` and are not operations (B5). A refusal before anything is sent
is a §9 error (`404` when the target does not exist).

| Target | Action | Body | Precondition | Response | Repeated or concurrent |
| --- | --- | --- | --- | --- | --- |
| session controls | `set_mode` | `{ modeId }` | held, `op` not a running `load`, not occupied by a `delete`, not `closing`; the mode was offered | `200 {}` or `200 { error }` when the Agent answers | sets a value: repeating leaves the same state; concurrent ones fold in the order of the Agent's responses |
| session controls | `set_config` | `{ configId, value }` | as above; the option and value were offered | as above | as above |
| session | `close` | `{}` | held, `op` ended or a running `prompt`, not `closing`; the Agent advertises close | `200 {}` or `200 { error }` | while one is in flight `not_allowed`; after success `404` |
| interaction (any scope) | `answer` | `{ outcome }` (permission) or `{ response }` (elicitation) | pending | `204` | **at most once**: checked, delivered and removed in one step; any other answer, from any client, gets `404` and changes nothing; answers to different interactions are independent |
| terminal login (`global`) | `input` | `{ data }` | it runs | `204` | **not idempotent**: written each time, in arrival order; input from several clients interleaves |
| terminal login (`global`) | `resize` | `{ cols, rows }` | it runs | `204` | sets the size |
| terminal login (`global`) | `dismiss` | `{}` | it has exited | `204` | a repeat gets `404` |
| URL flow (any scope) | `dismiss` | `{}` | — | `204` | removes it, waiting or finished; a repeat gets `404` |

- **Controls** run during a prompt (`v1/agent.rs:4898–4899`). The response is held until
  the Agent answers; the change reaches every replica through `controls`. An Agent error
  changes nothing.
- **Close** sets `closing` and sends `session/close`; like a cancel it answers the
  session's pending permissions `cancelled`. On success one step removes everything the
  bridge held for the session and ends the scope with `end: closed`; the session leaves
  `global.sessions` and stays in the catalog. On an Agent error `closing` clears.
- A control or close request keeps its session needed while outstanding, and its
  response applies only to the session generation it was sent for (memory model §9).
- A lost action response: an `answer` may be repeated (a `404` then means it was already
  delivered or the interaction ended; the replica shows which); controls show their
  current value; anything else is C3.

## 8. Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/scopes/{scope}` | the document as one snapshot (§5.2, `base: "replace"`, with `digest`); `404` for an unheld session; holds nothing |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | the block as received, in its exact form (below); served while the session's document references it, else `404` |
| `GET /api/v2/sessions/{id}/files?query=` | context search within the session's workspace |
| `GET /api/v2/sessions/{id}/files/content?path=` | context read |

A `GET` result is a snapshot, but its ordering against the stream is unknown: epochs are
random, so `(epoch, rev)` has no order across epochs, and a `GET` issued before a new
epoch may return after it. Hence:

- a client never installs a `GET` result into a scope it follows on a stream; the
  stream is that scope's only source;
- for a scope it does not follow, it installs only the result of its latest `GET` for
  that scope (a per-scope request counter discards older ones), and within the same
  epoch only if `rev` is not older than the replica's.
**Attachment identity.** The exact form of a block is JCS key order and string
escaping with every number written as the decimal digits the bridge received, without
conversion to double. `id` is the SHA-256 of these bytes. The attachment read's body is
exactly these bytes, so a client verifies it by hashing the raw body before parsing;
a body whose hash is not the `id` is unverified (§5.5).

**Markdown export** is assembled by the client from entries, prompts, terminals and
attachment reads.

## 9. Errors

```ts
interface ProtocolError {
  code: ErrorCode;
  message: string;
  data?: unknown;           // agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier; an answer that does not fit
  | "not_found"             // 404: unknown scope or resource
  | "stale"                 // 409: `expect` is not the current op.id
  | "not_allowed"           // 409: status, occupied scopes or `closing` do not allow the write
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "unavailable"           // 503: no Agent connection
  | "agent_error";          // in op.error, end.error, Prompt.end: the Agent returned an error
```

The body of every non-2xx response is `{ error: ProtocolError }`. A stream that fails
ends; the client subscribes again with its heads.

## 10. Previous revisions

The [history index](history.md) links the fixed earlier drafts. They explain past
designs and add no rules. Current work is tracked in
[the implementation plan](state-sync.md#7-fidelity-migration-revision-2).

## 11. Changes from revision 2

| Revision 2 | Revision 3 | Why |
| --- | --- | --- |
| turn log + `live.tail`, settle points | flat `entries` and `prompts` in the document; turns derived by the client | the memory model has no turns; load and live share one structure |
| hash chain over turns, `have=<n>:<h_n>` | `epoch` + `rev`, `have=<epoch>:<rev>`; per-entry `rev` for differential snapshots | live and replayed turns never hash equal, so the chain only served same-process reconnects, where a version suffices; late updates no longer rehash the chain |
| hashes compared, content never checked | digest of the document in snapshots and checkpoints, computed by the client over what it installed | goal 1: verifiable, including live state |
| bridge-chosen op IDs; `stale` cannot tell a lost answer from a race | client-chosen `id`, `after` on the operation | goal 3: repeats are harmless and lost answers are mostly resolvable |
| `create`/`fork`/`list` hold their HTTP response | every operation answers `202`; results in the document (`origin`, `catalog.pages`) | one result path for every client |
| `fork` occupies the source | the source and `global` | one installation at a time (memory model §9) |
| implied cross-scope installation in one event | stated per-scope atomicity only | events are per scope |

## 12. Synchronization design requirements

Revision 2's second-stage brief listed requirements; this is how revision 3 meets them.

| Requirement | Mechanism |
| --- | --- |
| One authority | the replica is installed only from bridge events; a new epoch drops earlier content (§5.4) |
| Complete representation | the document carries the memory model's state; byte content by verifiable reads (§3, §4, §8) |
| Atomic installation | an event or snapshot is applied to a copy and adopted whole; a snapshot only after its digest matches (§5.5) |
| Verified content | client-computed digests at snapshots and checkpoints, over the content it holds (§5.5) |
| Repair from the authority | resubscribe: differential snapshot within an epoch, full snapshot after a mismatch (§5.4, §5.5) |
| Separate presentation and command outcomes | drafts and folding are UI state (C1); admission (`202`), effect (document) and loss (§6.6) are distinct |
| Mutable-state divergence | `live` and `prompts` are in every digest; checkpoints follow every change burst |
| Resource reads | content-addressed attachment IDs; unverified reads are never shown |
| Commands during reconciliation | writes need no stream; their effect is read from the replica after it is current |

### 12.4 Independent acceptance

Expected client documents are written from bridge states and fixtures, never computed
by the production projection or reducer under test. Fault injection happens at the
client's actual decode, apply, verify and install steps.

| Case | Required observation |
| --- | --- |
| Complete initial installation | every field of every document kind survives the snapshot and verifies; attachments resolve and verify |
| Rev gap and reorder | a missing, duplicated or reordered ops event is detected before it is applied; recovery by differential snapshot |
| Inapplicable op | an op whose target is missing leaves the replica unchanged and triggers resubscription |
| Wrong content with valid framing | alter an entry, a `live` value or a prompt result in an event while keeping `rev` continuous; the next checkpoint detects it; a full snapshot repairs it |
| Corrupted local replica | replace an entry in the replica without an event; the hash cache misses, the next checkpoint or reconnect snapshot detects and repairs it; a differential snapshot never confirms it |
| Late update to an early entry | `set` on that entry; a reconnecting client receives only that entry and verifies |
| Slow subscriber | queued events are coalesced into one snapshot from the delivered `rev`; the result verifies |
| New epoch | load replacement, reopen and bridge restart give a full snapshot; earlier entries are not confirmed |
| Several clients | two subscribers at equal `rev` have equal digests; their writes race as §6.6 states; two answers to one interaction deliver exactly one |
| Lost write answers | each row of §6.6, including a lost `create` resolved by `origin`, and a multi-scope race (`authenticate` in `global` lost while a `fork` from a session replaces `global`'s `op`) resolved as unknown, never as not admitted |
| Large integers | two attachments differing only in an integer beyond ±2^53 get different IDs and read back unchanged; document digests agree between bridge and client for such values |
| Reconnect races | a superseded stream's late events cannot overwrite a newer replica; a `GET` from an older epoch returning after a new epoch's snapshot is not installed |
| Ended scope in a slow queue | a `close` or `delete` `end` queued behind events reaches the slow subscriber; no snapshot is generated for that scope |
| Bad attachment read | wrong bytes are not shown; a read racing a release yields `404`, not wrong content |

Per-event convergence tests (replica equals bridge document after every event) remain
as a separate regression gate.
