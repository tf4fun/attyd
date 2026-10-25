# 简易 Agent 交互协议

This is attyd's project-defined protocol between the FE (and other presentation
clients) and the bridge. The bridge remains an ordinary ACP client toward the Agent.
The `/api/v2` endpoint namespace and the revision numbers belong to this application
protocol, separately from ACP's versions.

Status: **revision 4, designed in [protocol-r4-design](protocol-r4-design.md) (#201, #202), implemented and served under `/api/v2`** (tracked in #203). Layer 1, what the bridge
keeps in memory and how ACP input changes it, is defined only by the
[bridge memory model](bridge-memory-model.md). This document defines layer 2: how that
memory reaches clients and how clients write to it. Where the two disagree, the memory
model wins. The design record (requirements, scenarios, alternatives, decisions) is
[protocol-r4-design](protocol-r4-design.md); this document is the specification. Revision 3
(documents per scope, `epoch` + `rev`, whole-document digests) and revision 2 are at the
fixed baselines linked in the [history index](history.md); §11 lists what changed.

Related documents: [design rationale](client-api-redesign.md),
[numbered acceptance cases](protocol-acceptance.md),
[implementation plan](state-sync.md), [test evidence](acp-coverage.md).

## 1. Model

```text
client  <  bridge memory  <  ACP server
```

The bridge is the single authority for what clients show. Clients are replicas of the part
of its memory they select and send writes to it. The protocol has these goals:

1. **Partial, verifiable, recoverable synchronization.** A client follows any set of
   paths of one state tree; receiving, storing and verifying cost scales with the
   selection. It can tell that what it holds equals the bridge's at a known version,
   detects when it does not, and recovers by replacing what differs, independently per path.
2. **Push to every follower.** Every change to a followed path reaches the follower, in the
   bridge's order, without polling; a client that cannot hold a stream polls instead.
3. **Correct concurrent and repeated writes.** Several clients may write at once and may
   repeat a write whose answer was lost; each request takes effect at most once, and a
   client can find out from the tree, or ask the bridge, whether it did.

**Bridge rules**

- **B1 Tree.** Everything the bridge publishes is one tree (§2): `/global`, `/catalog`,
  `/sessions/<id>`, and derived `/views/…`. A session carries the session's memory as the
  memory model defines it, with byte content replaced by references (§3.1).
- **B2 Versions.** The tree has an `instance` (random per bridge process) and a revision
  `rev` that rises by exactly one with every commit that changes anything published. Every
  node has the revision at which its value last changed, so a client holds, per followed
  path, the path version of what it holds (§2.1, §2.7).
- **B3 Hashes.** Every node has a content hash (§9). At stated points (§5.5) the bridge
  sends the hash of a followed path at a path version. Equal hashes mean equal subtrees.
- **B4 Recovery.** A client that cannot apply an event, or whose hash disagrees,
  subscribes again with what it holds, or without it for a path whose content disagreed.
  The bridge answers with a snapshot: only what changed since the client's path version
  when its instance is current, else the whole subtree (§5.3).
- **B5 Writes.** A write is an operation or an action. An **operation** occupies targets
  and is admitted against the current operation (`expect`); its ID is chosen by the
  client, so repeating it is harmless (§6). An **action** acts on something that exists
  now; each states its repeat semantics (§7).
- **B6 Holding.** The bridge holds a session while it is needed (memory model §8.4) or
  observed. Following any path of a session or of a view of it, and a `GET` of such a
  path, observe it (§8.2). A session that nothing needs is released after
  `--session-unobserved-timeout`, only if the Agent can load or resume it. Observing an
  unheld session starts a `load`. The bridge starts an Agent connection only for a
  session path a stream follows or a read asks for, for the next connection after a
  terminal login, and for `restart`; a read of `global` or `catalog`, a stream of them,
  and a write do not (a write without a connection is `unavailable`).

**Client rules**

- **C1 Projection.** A client's presentation is a pure function of its replica and its
  own UI state (drafts, folding, selection). Turns are the partition the bridge publishes
  (§3.3); the client derives none.
- **C2 Refusal.** A refused write leaves its input with the user. Queued input that was
  never sent, or was refused with a definite answer, may be submitted automatically once
  the replica shows the operation ended; after a lost answer only `superseded` allows a
  new request (§6.6).
- **C3 Uncertainty.** A write whose answer was lost is resolved by its outcome (§6.6); an
  interaction answer may be repeated (§7). Anything else whose effect is unknown is left to
  the user, never resent automatically.

**Conventions.** All paths are under `/api/v2`. JSON field names are camelCase. Embedded
ACP types (`ContentBlock`, `ToolCallUpdate`, `PromptResponse`, `RequestPermissionRequest`,
`InitializeResponse`, …) are carried as the ACP schema pinned in `Cargo.lock` defines
them, `_meta` included. Types the bridge defines itself are never presented as ACP types.

| Part | Transport | Section |
| --- | --- | --- |
| The tree and its nodes | in stream events and `GET /api/v2/nodes/{path}` | §2–§4 |
| Stream | SSE | §5 |
| Operations | `POST`, one endpoint per target | §6 |
| Actions | `POST` | §7 |
| Reads | `GET` | §8 |
| Hashes | computed on both sides | §9 |


## 2. The state tree

### 2.1 Version

- `instance`: 128 random bits (unpadded base64url), chosen when the bridge process starts.
  Agent restarts within the process keep it; nothing published survives a process restart.
- `rev`: an integer, starting at 1, increased by exactly one by every commit that changes
  anything published. One commit may change several paths (installing a session adds
  `/global/sessions/<id>` and `/sessions/<id>` in the same `rev`).
- Versions of one instance are totally ordered. Versions of different instances are not
  comparable (§5.6, §8.3).

### 2.2 Paths and nodes

A path is a JSON Pointer into the tree (a session ID containing `/` or `~` is escaped as
RFC 6901 says). In query parameters of the form `<path>:<value>`, the value follows the
**last** `:`; in `cwd=<id>:<dir>` the ID is percent-encoded and ends at the **first** `:`.

The tree's shape is fixed by this document: some positions are **nodes** (they have their
own path revision and hash), the rest are plain JSON inside a node.

| Kind | Content | Children |
| --- | --- | --- |
| value | any JSON | none (but may contain text positions, §2.6) |
| object | own fields (JSON) | named child nodes |
| map | the set of keys | one node per key |
| list | the length | one node per index |

Shape (`·` marks a node, indentation children):

```text
· /global                       object: { agent }                   (§4.3)
    · op                        value: OpHead (§2.4)
    · opDetail                  value: { input, result?, error? }
    · ops                       list of value OpRecord (§6.6)
    · sessions                  map sessionId → value SessionSummary
    · loadFailures              map sessionId → value ProtocolError (§5.2)
    · terminals                 map → value LoginTerminal
    · interactions              map → value Interaction
    · urlFlows                  map → value UrlFlow
· /catalog                      object: { revision }
    · op, · opDetail, · ops, · interactions, · urlFlows   as in /global
    · pages                     list of value Page
· /sessions/<id>                object: { sessionId, cwd, history, historyRev, heldSince, closing }
    · controls                  value: SessionControls
    · op, · opDetail, · ops, · interactions, · urlFlows
    · terminals                 map → value Terminal
    · entries                   list of value Entry (absent while history is unknown)
    · prompts                   list of value Prompt
    · turns                     list of value TurnHead (absent while history is unknown, §2.3)
· /views/<id>/output/latest     value: OutputView (§2.5)
· /views/<id>/history/<i>/<h>/output/<start>   value: OutputView
· /views/<id>/history/<i>/<h>/turn/<start>     object: TurnHead fields; child · entries (list of value Entry)
```

`historyRev` is the revision at which the session's history was last installed or
replaced (a load, a resume, a fork's load). `heldSince` is the revision at which the
bridge started holding the session this time; it changes only when the session is
released and held again. `/global` and `/catalog` are held for the whole instance. Entries, prompts, controls, interactions,
terminals and attachments keep the wire types of §3 and §4.

### 2.3 The turn index

Entries stay a flat list, as layer 1 holds them, so an entry's path never moves when the
turn partition does. The bridge publishes the partition memory model §5 defines:

```ts
interface TurnHead {
  historyRev: number;        // the session's historyRev: which history `start` indexes
  start: number;             // index of the turn's first entry
  length: number;            // its number of entries
  messageId?: string;        // the opening user message's messageId, if any
  state: "running" | "ended";
  output: number | null;     // index of the turn's last agent message entry, null when none
}
```

- `state` is `running` exactly while a prompt whose `entry` is in this turn has no `end`.
  Anything else is `ended`, including the turns of a loaded history (memory model §6.2
  clears prompts).
- **Revisable.** `ended` does not freeze a turn: a late tool update (memory model §3.2
  rule 1) or an idle update (D5) can still change its entries, and `output` can move.
- **Partition changes.** The partition is a function of the entries and can change after
  the fact: a user entry without `messageId` that later takes the opener's ID by a joining
  chunk stops starting a turn. The bridge then rewrites the affected `TurnHead`s; entries
  are untouched. A history replacement replaces entries, prompts and turns and sets
  `historyRev`.

A client derives no turns: the browser reads `turns`.

### 2.4 The operation head

```ts
interface OpHead {           // an operation without its `input`, `result` and the body of its error
  id: string; scope: string; after: string | null; kind: OperationKind;
  status: OperationStatus; cancelRequested: boolean;
  error?: ErrorSummary;      // of `opDetail.error`, when it failed
}

interface ErrorSummary {
  code: ErrorCode;           // §10
  agentCode?: number;        // for `agent_error`: the ACP error's integer `code`
}
```

`opDetail` holds `{ input, result?, error? }` of the same operation, `error` being the
whole `ProtocolError` (its `message` and `data` are the Agent's, of any size), and changes
with it. A writer needs only `op` (its `id` for `expect`, its `status`, and whether and how
it failed), which is bounded however large the last prompt or the Agent's error was.

**Bounded heads.** Everything this design offers a small client as a fixed-size unit
(`OpHead`, `OpRecord`, `TurnHead`, `OutputView` apart from its windowed `text`, `End`)
holds only enums, numbers, booleans, IDs and revisions. IDs the client did not choose
(`messageId`, interaction and terminal IDs) are the Agent's strings; the bridge passes
them through unchanged, so their size is the Agent's.

### 2.5 Views

A view is computed by the bridge from the tree at the same `rev`. It is read-only and
contains nothing the canonical tree does not. Views exist so that a client follows one
fixed path, both for a selection that **moves** and for one turn without the rest of the
entries.

```ts
interface OutputView {
  turn: { historyRev: number; start: number; messageId?: string; state: "running" | "ended" } | null;
  entry: number | null;      // the output message's entry index, null when none yet
  text: string;              // its text blocks' `text`, concatenated in order, nothing inserted (a text position)
  other: number;             // how many of its blocks are not text (not included)
}
```

- `/views/<id>/output/latest` **moves**: to the next turn when one starts, and to the next
  agent message when the Agent writes one after a tool call. It means "the output of the
  current turn".
- `/views/<id>/history/<i>/<h>/output/<start>` and `/views/<id>/history/<i>/<h>/turn/<start>`
  are **fixed** to the turn starting at entry `<start>` of the history installed at
  `historyRev = h` **in instance `i`**. A revision alone does not name a history: a new
  instance counts revisions from 1 again and can install a different history at the same
  `h`. The selection `(i, h, start)` is the path itself, separate from any cached baseline:
  - when no turn starts at `<start>` in that history (it was merged into another, or never
    existed), the path ends with `absent`;
  - when `i` is not the bridge's instance, or the session's `historyRev` is not `h` (the
    history was replaced), the path ends with `replaced`. This holds on a stream and for a
    `GET`, whenever it is followed or read, with or without `have` or `instance`.

  So recovering a broken replica (resubscribing without `have`, §5.5) re-reads the same
  selection, and reconnecting to a new instance (§5.6) ends it; neither can turn it into
  another one. A client that wants a turn of the new history chooses it explicitly: it
  reads the instance and `historyRev` (from the stream, `TurnHead`s or `OutputView.turn`)
  and follows a new path. A client picks a fixed turn from the turn index or from
  `output/latest`, both of which state `historyRev`.
- `text` is the message's text as the Agent wrote it, Markdown included; the bridge
  converts, summarizes and separates nothing. A client wanting the whole entry (blocks,
  annotations, attachments) follows `/sessions/<id>/entries/<n>` or the turn view.
- `/views/<id>/history/<i>/<h>/turn/<start>` holds the turn's entries as a copy (shared in memory with the
  canonical list), so the turn verifies without the other entries.

A view is materialized while it is followed or read. Its path revision is the revision at
which its value last changed, or at which it was materialized, whichever is later (§2.7).
Following or reading a view holds its session like following the session (§8.2).

### 2.6 Text positions

These string positions are **text positions**:

- the `text` of every ContentBlock of type `text` inside an `Entry` (message chunks,
  tool call content, compaction summaries);
- a terminal's `output`;
- an `OutputView`'s `text`.

A text position is a string in the JSON like any other. It differs in how it is hashed
(§9) and in that a subscription may hold only a window of it (§5.4). Segmentation is a
pure function of the string: its UTF-8 bytes are split greedily from the start into
segments of at most `L = 4096` bytes, each ending on a code point boundary. `L` is a
protocol constant: it is part of every text hash. Appending changes only the last segment
and adds new ones; earlier segments never change.

Other large values (a tool's `rawInput`/`rawOutput`, an interaction's request, plan
bodies) are not segmented. A small client controls their cost by not following the paths
that contain them, which the tree makes possible.

### 2.7 Path revisions

> For every node `N`: `rev(N)` is the revision at which the value at `N`'s path last
> changed, and `srev(N)` is the largest `rev` in `N`'s subtree. If `srev(N) ≤ r`, the
> value at `N`'s path is the same now as at `r`.

The invariant is about paths, not objects. With flat entries most changes are local: an
entry changes in place, an append adds an index. Paths whose value changes without any
underlying object changing are the derived ones (`turns/<k>` after a partition change, a
moving view); the bridge compares their new value with the old at each commit and bumps
them when they differ. A revision may be later than the actual change (a view
materialized late) but never earlier: a late one costs a fuller snapshot, never a wrong
one.

The **path version** of a followed path `p` is `srev(p)`. It is what a client holds, what
a snapshot states, and what a change chains from.


Names of the top-level nodes in this document: a **target** is `global`, `catalog` or
`sessions/<id>`; it is where writes go (§6) and each has its `op`, `opDetail`, `ops`,
`interactions` and `urlFlows`.


## 3. Session history

A session's history is published at `/sessions/<id>` (§2.2): the field `history`
(`"complete" | "partial"`, absent while unknown, memory model §6.1) and the nodes `entries`
(a list of `Entry`, absent exactly when the history is unknown), `prompts` (a list of
`Prompt`, memory model §4.2) and `turns` (§3.3). `entries` is absent while the bridge holds
no history, and `[]` for a known empty one; the two never stand for each other.

### 3.1 Entries

An `Entry` is the memory model's entry (memory model §3.1) with one change: a content
block that carries bytes (image, audio, embedded resource) is replaced by an
`AttachmentRef`, read separately (§8.4).

```ts
interface AttachmentRef {   // the bridge's own type, not an ACP ContentBlock
  type: "attachment";
  id: string;               // lowercase hex SHA-256 of the block's exact form (§8.4)
  block: "image" | "audio" | "resource";
  mimeType?: string;
  uri?: string;             // a resource's URI
}

type Block = ContentBlock /* with byte-carrying blocks as AttachmentRef */;
type PromptBlock = ContentBlock | AttachmentRef;   // what a prompt may contain (§6.2)
```

The `id` identifies the block as layer 1 retains it, `_meta` included, without rounding
its integers to doubles (§8.4); reading it returns that block unchanged, and a client can verify the
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

Turns are the partition of memory model §5, derived by the bridge and published as the
index `turns` (§2.3): a client reads it and derives none. A `Prompt` belongs to the turn its
`entry` starts.


## 4. Live nodes

The nodes below are the live state of the targets, as nodes of the tree (§2.2); their
JSON types are generated from the bridge's own (`shared/protocol.ts`).

### 4.1 Common nodes

Every target has the nodes `op` (the `OpHead`, §4.4), `opDetail` (its input, result and
error), `ops` (the operation log, §6.6), `interactions` and `urlFlows` (maps by
interaction ID, §4.4, §4.6).

### 4.2 Session

`/sessions/<id>` is an object whose own fields are `sessionId`, `cwd`, `closing` (a close
request is outstanding, §7), `history`, `historyRev` and `heldSince` (§2.2), and whose
nodes are the common ones, `controls` (`SessionControls`, §4.5), `terminals` (Agent terminals
by ID, §4.6) and the history of §3.

### 4.3 Global and catalog

```ts
interface GlobalFields {              // the own fields of /global
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
}
// its nodes: the common ones, `sessions` (SessionSummary by session ID, for the sessions the
// bridge holds), `terminals` (LoginTerminal by operation ID), `loadFailures` (§5.2) and
// optionally `diagnostics` (§4.7)

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

interface CatalogFields {             // the own fields of /catalog
  revision: string;                            // changes when the Agent's list may have changed
}
// its nodes: the common ones and `pages`, the pages listed since `revision` last changed, in
// order: Array<{ sessions: SessionInfo[]; nextCursor: string | null; _meta?: unknown }>
```

### 4.4 Operations and interactions

```ts
type OperationStatus = "running" | "completed" | "failed" | "cancelled";

interface OpHead {          // the node `op`: bounded, whatever the size of the request or error
  id: string;               // chosen by the client that wrote it (§6.5); bridge-chosen for load/open/none
  scope: string;            // the target it was written to (its issuing target), as `global`, `catalog`, `session/<id>`
  after: string | null;     // the `expect` it was admitted with: the issuing target's previous `op.id` (§6.6)
  kind: OperationKind;
  status: OperationStatus;
  cancelRequested: boolean;
  error?: ErrorSummary;     // when it failed
}

interface ErrorSummary {
  code: ErrorCode;          // §10
  agentCode?: number;       // for `agent_error`: the ACP error's integer `code`
}

// the node `opDetail`, changing with `op`:
interface OpDetail {
  input: unknown;           // the write's `args` (a prompt as `Block[]`), or the load's cwd
  result?: unknown;         // completed (§6.2)
  error?: ProtocolError;    // failed: the whole error, the Agent's message and data of any size
}
```

A writer needs only `op`: its `id` for `expect`, its `status`, and whether and how it failed.

- `op` keeps its final status, result or error until the next operation replaces it.
- `cancelled` means the operation ended because of a cancel before it produced a
  result. A prompt the Agent answered with stop reason `cancelled` is `completed`.
- **First operations.** A session held by observation starts with a running `load`; a
  session installed by `create` or `fork` starts with a completed `open`; `global` and
  `catalog` start with a completed `none`. These have `after: null`.
- When the Agent connection ends, every running operation ends `failed` with
  `unavailable`.
- **Bounded heads.** Everything this revision offers a small client as a fixed-size unit
  (`OpHead`, `OpRecord`, `TurnHead`, `OutputView` apart from its windowed `text`, an
  `End`) holds only enums, numbers, booleans, IDs and revisions. IDs the client did not
  choose (`messageId`, interaction and terminal IDs) are the Agent's strings, passed through
  unchanged.

```ts
interface Interaction {
  interactionId: string;    // allocated by the bridge, never reused
  kind: "permission" | "elicitation";
  request: RequestPermissionRequest | CreateElicitationRequest;   // ACP, verbatim
  parent?: { requestId: RequestId; method: string };              // request-scoped elicitation
}
```

Scopes and lifetimes are memory model §8.2. An interaction is shown in the
`interactions` node of its target: its session; for a request-scoped elicitation, the
target of the write or load that sent the request (`global` when none did); for a session
being installed, `global` stages session-scoped interactions until installation, when
they move into `sessions/<id>`. This staging does not change `global.op` during a fork;
the fork operation and its request-scoped elicitations belong to the source session
(memory model §9).

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
  output: string;           // bounded tail (`trim`; and a text position, §2.6)
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
GET /api/v2/stream?instance=<i>&follow=<path>&have=<path>:<version>&window=<path>:<bytes>&cwd=<id>:<dir>
```

| Parameter | Meaning |
| --- | --- |
| `follow` | a path to follow; repeatable. Paths may not overlap (one may not contain another). |
| `instance` | the instance the client's holdings belong to; omitted when it holds nothing |
| `have` | per followed path: the path version it holds and has applied without error, or verified; omitted after a failed verification |
| `window` | per followed path: hold text positions in it as windows of `bytes` (§5.4); without it, whole strings |
| `cwd` | the working directory to load an unheld session with (§8.2) |

One stream carries any set of paths. To change the set the client opens a new stream and
ignores the old one.

### 5.2 Events

```ts
type Event =
  | { instance: string; rev: number; changes: Change[] }   // first event of a stream
  | { rev: number; changes: Change[] }                     // one commit, or a catch-up
  | { rev: number; check: Check[] };                       // checkpoint

type Change =
  | { path: string; snapshot: Snapshot }
  | { path: string; from: number; ops: Op[] }              // ops relative to `path`
  | { path: string; end: End };

interface Check { path: string; at: number; hash: string }
interface End { reason: "deleted" | "closed" | "load_failed" | "absent" | "replaced"; error?: ErrorSummary }
```

The client holds, per followed path `p`, a path version `v_p`.

- An installed **snapshot** sets the content of `p` and `v_p = snapshot.at` (§5.3), not the event's
  `rev`: a path last changed at 5 and snapshotted at global revision 100 is held at 5.
- An **ops change** in a commit event at `rev` carries `from`, the path version before the
  commit. The client applies it only if `from == v_p`, then sets `v_p = rev` (the commit
  changed `p`, so its new path version is `rev`).
- In the same instance, a snapshot with `at < v_p` and an ops change with `rev ≤ v_p`
  are ignored (**receive idempotence**). A snapshot with `at == v_p` verifies the content
  already held against its `hash`: a match confirms it through `v_p`; a mismatch drops
  the replica and resubscribes without that path's `have`. It never reinstalls the
  snapshot's payload, whether full, `same`, or a differential snapshot from an older
  `base`. Any other version mismatch, or an op that does not apply, is a gap: the client
  resubscribes with its `have`s.
- All changes of one commit arrive in one event and are applied together: everything a
  client follows is consistent at that commit.
- `Op` is the JSON Pointer operation of §5.7, with pointers relative to `p`.
- **`end`**: the path does not exist now. For a session path (`/sessions/<id>/…`,
  `output/latest`) the subscription stays: a later install or load starts it with a
  snapshot (`absent` and `closed` are not final there). A fixed view is finished by
  `absent` and `replaced`; the client drops it.
- An `end` carries an `ErrorSummary`, enough to act on (for example `agentCode` −32000:
  authentication is required). A session path that ends with `load_failed` (a failed cold
  load, or a session that cannot be reattached and ends) leaves no `opDetail` that could
  keep the whole error, so the bridge keeps it in `/global/loadFailures/<sessionId>` until
  that session is held again or deleted, bounded to the last `K` failures. A failed
  reattachment that keeps the session held has no `end`: its error is in the session's
  `opDetail`. A client that shows the message (the browser) follows `/global` anyway; a
  device need not.
- **Checkpoint**: `at` is the path version the hash is of. With `at == v_p` the client
  compares; with `at > v_p` it has missed a change (for example the last one before the
  path went quiet) and resubscribes with its `have`; with `at < v_p` the checkpoint is
  older than what it holds and is ignored.

### 5.3 Snapshots

```ts
interface Snapshot {
  base: number | null;       // the `have` it builds on; null: replaces everything
  at: number;                // the path version after installing it: srev(p) now
  hash: string;              // of the subtree after installing it
  node: SNode;
}

type SNode =
  | { v: unknown }                                         // the whole subtree as JSON
  | { o: unknown; k: string[]; c: Record<string, SNode> } // object: own fields, all child names, changed children
  | { m: string[]; c: Record<string, SNode> }              // map: all keys, changed children
  | { l: number; c: Record<string, SNode> }                // list: length, changed children
  | { same: true };                                        // the root: nothing changed since `base`
```

For `have = h`, the bridge walks the subtree: a node with `srev ≤ h` is omitted (the
client keeps its own); a value node with `srev > h` is sent whole; a container with
`srev > h` is sent as its structure (own fields, all keys, or the length) plus its
children with `srev > h`. When installing a new path or a newer snapshot, the client
installs into a copy: it keeps every child the structure names and the snapshot omits,
drops every child the structure no longer names,
and replaces the rest. By §2.7 a kept child is exactly the bridge's current value. The
copy is adopted only if its hash matches; then `v_p = at`.

A full snapshot is `{ base: null, at, node: { v } }`. A differential snapshot needs no
knowledge of what the client held beyond `h`, since structure is always sent where
anything below changed. When `srev(p) ≤ h`, nothing changed since the client's version: the
snapshot is `{ base: h, at: srev(p), hash, node: { same: true } }`, and the client keeps what
it holds (a `GET` says `unchanged`, §8.1). A list's changed children are keyed by index.

### 5.4 Text windows

With `window=<p>:<N>` (`N ≥ L`), every text position under `p` is held as a window:

```ts
interface TextWindow {
  length: number;            // UTF-8 bytes of the whole text
  offset: number;            // bytes before the window; a segment boundary
  before: string;            // chain state c before the window's first segment: 32 bytes; all zeros when offset = 0
  text: string;              // the segments from `offset` to the end
}
```

- It hashes to the same `h` as the whole string, starting the chain at `before`.
- `text` holds the shortest run of whole trailing segments of at least `N` bytes (all of
  them when the text is shorter), so `|text| < N + L`.
- **Bounded events.** No event carries `N + L` bytes or more of one window's text. Let
  `a` be the bytes a commit appends to the text (all its appends together).
  - If `a < N + L`, the change is `append` on `…/text` and `set` on `…/length`, followed,
    when the window then exceeds the rule above, by `trim` of whole leading segments and
    `set` of `offset` and `before`.
  - Otherwise, and for any replacement of the text, the change is one `set` of the whole
    window: the new tail, under `N + L` bytes.

  The decision depends on `a` alone, never on how short the held window is. A client
  therefore needs, per window, the held window plus one event's text: under `2(N + L)`
  bytes, applying on a copy included.
- Snapshots carry windows by the same rule.

### 5.5 Checkpoints, recovery, coalescing and keepalive

- **When checkpoints are sent**: for each followed path, after its changes pause for
  500 ms, and at least every 2 s while it keeps changing; also in every snapshot (`hash`).
- **Mismatch**: on a hash mismatch the client drops its replica of that path and
  resubscribes **without** that path's `have`; the bridge sends the whole subtree. If a
  full snapshot fails verification again, the client reports that synchronization failed
  and retries with backoff. A path is shown as confirmed only up to its last verified
  version. Dropping a replica never changes which path is followed: a fixed view keeps its
  `history/<i>/<h>` binding (§2.5).
- **Slow subscriber**: when a stream's queue exceeds its budget, the bridge drops the
  queued events and queues one catch-up event at the current `rev` carrying, per followed
  path, a differential snapshot from the last `v_p` it actually wrote. Ends are kept.
  A fixed view's queued final end is kept even though the view is no longer tracked;
  once that end has been written to the stream, coalescing does not emit it again.
  Coalescing never changes what a path version contains.
- An SSE comment at least every 15 s on an idle stream.

### 5.6 A new instance on a stream

The first event of a stream names its `instance`. If it differs from the client's, the
client drops everything it holds from the old instance and installs the snapshots, which
are full (`base: null`, since its `have`s named another instance). A stream's events
belong to its instance; the client never mixes two.


### 5.7 Ops

`ops` are JSON Pointer operations on the subtree of the followed path `p`, with pointers
relative to it (`""` is `p` itself):

```ts
type Op =
  | { op: "set";    path: string; value: unknown }    // create or replace; an array index modifies in place
  | { op: "remove"; path: string }                    // map key or array index
  | { op: "append"; path: string; values: unknown[] } // array tail
  | { op: "append"; path: string; text: string }      // string tail
  | { op: "trim";   path: string; count: number };    // drop the first `count` items or UTF-16 units
```

`set` requires the parent to exist (`set ""` replaces `p`); every other op requires its
target to exist. The bridge picks the op that matches the change: streamed text is
`append` text; a new entry is `append` on the list; a late update to an earlier entry is
`set` on that entry or its changed field; a bounded tail moving forward is `trim` plus
`append`; a list that shrank is `set` on the list; anything else is `set` on the smallest
enclosing value.

## 6. Operations

### 6.1 Request

```text
POST /api/v2/targets/{target}/ops                         { id, expect, kind, args }
POST /api/v2/targets/{target}/actions/{action}            { …args }
POST /api/v2/targets/{target}/resources/{id}/{action}     { …args }
GET  /api/v2/targets/{target}/ops/{id}?expect=<e>&instance=<i>&at=<version>
```

`{target}` is `global`, `catalog` or `sessions/<id>` (the ID percent-encoded as a path
segment). Kinds, actions, arguments, occupation and admission are below (§6.2, §6.4, §7);
the last request asks what became of a write (§6.6).

### 6.2 Kinds

| Target | Kind | `args` | Occupies | Allowed while | Result |
| --- | --- | --- | --- | --- | --- |
| `sessions/<id>` | `prompt` | `{ prompt: PromptBlock[] }` | the session | `op` ended, not `closing` | `{ stopReason, usage? }` |
| `sessions/<id>` | `fork` | `{ cwd? }` (default: the source's `cwd`) | the source; excludes writes to `global` and other forks while it runs | `op` ended, `global`'s `op` ended, no fork running | `{ sessionId }` |
| `global` | `create` | `{ cwd }` | `global` | `op` ended | `{ sessionId }` |
| `global` | `delete` | `{ sessionId }` | `global`, `sessions/<sessionId>` | `op` ended, the session free | `{}` |
| `global` | `authenticate` | `{ methodId }` | `global` | `op` ended | `{}` |
| `global` | `logout` | `{}` | `global` | `op` ended | `{}` |
| `global` | `terminal_login` | `{ methodId, cols, rows }` | `global` | `op` ended | `{ exitCode, signal }` |
| `catalog` | `list` | `{ cursor?, revision? }` | `catalog` | `op` ended | `{ revision, page }`; the page is also appended to `/catalog/pages` |
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
  summary carries `origin`, the operation's `id`. Only one installation runs: admission
  refuses a global write or another session's fork while a fork runs, without replacing
  `global.op` or adding a fork to `global.ops`.
- **Delete** closes a held session (`session/close`, when advertised), deletes it
  (`session/delete`), and ends the paths of `/sessions/<sessionId>` with `deleted`. It needs the
  Agent's delete capability and works on sessions the bridge does not hold.
- **List** runs `session/list`. Without `cursor` it starts a new listing: `pages` is
  replaced by the first page. With `cursor` the page is appended. A `revision` that is
  not `/catalog`'s `revision` is refused with `catalog_changed`.

### 6.3 Response

| Outcome | Status | Body |
| --- | --- | --- |
| admitted, or a repeat of the current operation (§6.6) | `202` | `{ op: "<id>" }` |
| refused | §10 | `{ error: ProtocolError }` |

A `202` means admitted, nothing more; the result arrives in the tree (`opDetail.result`,
and for `create`/`fork`/`list` also `origin` or `pages`). No response is held open.

### 6.4 Admission

The bridge admits operation writes and loads one at a time against the target's current `op`:

| Write | Admitted when | Otherwise |
| --- | --- | --- |
| starts an operation | `id` is new, `expect == op.id`, `op` ended, every other target it occupies free | `202` if `id == op.id` (a repeat, §6.6); `stale` if `expect` differs; `not_allowed` otherwise |
| `cancel` | `expect == op.id`, `op` running, its kind accepts a cancel | `stale` if `expect` differs; `not_allowed` otherwise |

Capability, argument and Agent checks follow: `unsupported`, `invalid_request`,
`unavailable`. A refused write changes nothing. An admitted operation is the `op` of
every held target it occupies, with the same `id`, until it actually ends, and an occupied
target keeps showing it after it ended or failed: a target's `op.id` never returns to an ID it
showed before, which §9.3 relies on. An unheld occupied session cannot be loaded meanwhile. There is no queue: competing writes are
refused (R4 of the design). `cancel` is admitted while the `op` runs; interaction answers
are actions. Neither waits for the running operation.

### 6.5 Operation IDs

A client generates each new operation's `id`: 128 random bits in unpadded base64url
(22 characters). A malformed `id` is `invalid_request`. A client uses a new `id` for
each new intent and the same `id` only to repeat a write whose answer it did not get.
IDs the bridge allocates (`load`, `open`, `none`) have the same form.

### 6.6 Repeats, races and lost answers

#### At most once for a fixed request

> A request `(x, e)` (operation ID `x`, `expect = e`) executes at most once, however often
> it is sent, lost, delayed or repeated, and across bridge restarts.

Admission requires `op.id == e` and makes `op.id = x`. IDs are 128 random bits, never
reused, so once `x` is admitted the target's `op.id` is never `e` again: a later copy of
`(x, e)` finds `op.id == x` (`202`, no effect) or `op.id ≠ e` (`stale`). After a bridge
restart every `op` has a new bridge-allocated ID, so any copy is `stale`.

The guarantee is about **one request identity**. A client that creates a *new* request
`(y, e')` for the same user input creates a second request, and may cause a second
execution, unless it knows `(x, e)` can no longer be admitted. The outcomes below say when it knows.

#### Outcomes: a bounded operation log

```ts
interface OpRecord {
  id: string;
  after: string | null;      // this target's op.id it replaced; null for the first operation seen of the target
  kind: OperationKind;
  rev: number;               // the commit that admitted it
  status: OperationStatus;   // updated until it ends
  error?: ErrorSummary;      // as in OpHead
}
```

Every target keeps `ops`: the records of the last `K = 64` operations that occupied it,
**including those written to another target** (a `delete` written to `global` is recorded
in `global` and in the session it occupies, each with its own `after`; a `fork` is shown
and recorded only in its source session, and excludes writes to `global` while it runs). Within the window, each
target's log is the complete chain of its `op`s. A target's log lives as long as the
target: releasing a session and holding it again starts an empty log.

The outcome of a request `(x, e)` written to target `W`, while the client held instance
`i` and `W` at path version `t`:

| Condition | Outcome | What it means |
| --- | --- | --- |
| the instance is not `i` | `unknown` (`restarted`) | `(x, e)` can no longer be admitted (above), but whether it was is lost |
| `W` was installed after `t` (released and held again) | `unknown` (`released`) | as above |
| `x` is in `W.ops` | `admitted`, with its record | it took effect |
| `W.op.id == e` | `open` | not admitted **yet**: a delayed copy may still be |
| a record in `W.ops` has `after == e` and `id ≠ x` | `superseded` | another operation replaced `e`; `(x, e)` can never be admitted |
| otherwise (`e` is older than the window) | `unknown` (`expired`) | |

The rows are read in order. `open` precedes `superseded` so that a request that may still be
admitted is never answered `superseded`, whatever the log says.

`GET /targets/{W}/ops/{x}?expect=e&instance=i&at=t` answers this. A client following
`W/op` and `W/ops` computes the same from its replica; the rule is implemented once in
`shared/` and once in the bridge, tested on the same fixtures. The `released` row compares `t` with
`W`'s `heldSince`.

Client rules (C2 and C3, made precise):

- `admitted`: done; its effect and result are in the tree.
- `superseded`: definitely never executed; the input is still the user's. A queued prompt
  may be submitted automatically as a **new** request with a new `id` and the current
  `expect`. This is the only outcome that allows a new identity.
- `open`: the client may only **repeat `(x, e)` unchanged**. The repeat is admitted now or
  is a no-op if a delayed copy got there first; either way §6.6 keeps one execution. While
  `e` still runs the repeat is `not_allowed`, which changes nothing; the client asks again
  after `e` ends. It never creates a new `id` from this outcome.
- `unknown`: left to the user, never resent automatically.

So: execution at most once holds per request, always; the outcome is findable while the
target lives, the bridge does not restart, and fewer than `K` further operations occupied
the target.

#### Lost action answers

As in §7: an interaction answer may be repeated (`404` then means it
was delivered or the interaction ended; the tree shows which); controls show their value;
anything else is left to the user.


## 7. Actions

```text
POST /api/v2/targets/{target}/actions/{action}                    { …args }
POST /api/v2/targets/{target}/resources/{resourceId}/{action}     { …args }
```

Actions take no `expect` and are not operations (B5). A refusal before anything is sent
is a §10 error (`404` when the target does not exist).

| Target | Action | Body | Precondition | Response | Repeated or concurrent |
| --- | --- | --- | --- | --- | --- |
| session controls | `set_mode` | `{ modeId }` | held, `op` not a running `load`, not occupied by a `delete`, not `closing`; the mode was offered | `200 {}` or `200 { error }` when the Agent answers | sets a value: repeating leaves the same state; concurrent ones fold in the order of the Agent's responses |
| session controls | `set_config` | `{ configId, value }` | as above; the option and value were offered | as above | as above |
| session | `close` | `{}` | held, `op` ended or a running `prompt`, not `closing`; the Agent advertises close | `200 {}` or `200 { error }` | while one is in flight `not_allowed`; after success `404` |
| Agent (`global`) | `restart` | `{}` | `agent.phase` is `stopped` or `error`: no connection runs and none is scheduled | `204` when a new connection is started (not when it is ready); `global`'s `agent.phase` follows | while a connection starts or runs `not_allowed`; the repeat after `204` is the same |
| interaction (any scope) | `answer` | `{ outcome }` (permission) or `{ response }` (elicitation) | pending | `204` | **at most once**: checked, delivered and removed in one step; any other answer, from any client, gets `404` and changes nothing; answers to different interactions are independent |
| terminal login (`global`) | `input` | `{ data }` | it runs | `204` | **not idempotent**: written each time, in arrival order; input from several clients interleaves |
| terminal login (`global`) | `resize` | `{ cols, rows }` | it runs | `204` | sets the size |
| terminal login (`global`) | `dismiss` | `{}` | it has exited | `204` | a repeat gets `404` |
| URL flow (any scope) | `dismiss` | `{}` | — | `204` | removes it, waiting or finished; a repeat gets `404` |

- **Controls** run during a prompt (`v1/agent.rs:4898–4899`). The response is held until
  the Agent answers; the change reaches every replica through the `controls` node. An Agent error
  changes nothing.
- **Close** sets `closing` and sends `session/close`; like a cancel it answers the
  session's pending permissions `cancelled`. On success one step removes everything the
  bridge held for the session and ends the paths of the session with `closed`; the session leaves
  `global.sessions` and stays in the catalog. On an Agent error `closing` clears.
- A control or close request keeps its session needed while outstanding, and its
  response applies only to the session generation it was sent for (memory model §9).
- A lost action response: an `answer` may be repeated (a `404` then means it was already
  delivered or the interaction ended; the tree shows which); controls show their
  current value; anything else is C3.


## 8. Reads and holding

### 8.1 Reads

| Read | Result |
| --- | --- |
| `GET /api/v2/nodes/{path}?instance&have&window&cwd` | `{ instance, rev, change }`: what a stream's first event would carry for that path, or `{ instance, rev, unchanged: true }` when `instance` matches and `srev(path) ≤ have` |
| `GET /api/v2/nodes/{path}/segments/{i}?at=<version>` | `{ before: c_{i-1}, segment: s_i }` for the text position at `path`; `409 stale` when segment `i` changed after `at` |
| attachments, context search and read | §8.4 |

**Polling.** A client that cannot keep a stream polls `GET /nodes/{path}?have=` with its
path version; it gets a differential snapshot or `unchanged`. Updates between polls are
coalesced, which R2 allows; latency is the polling interval. A view a read materialized
lingers for 30 s, so a client polling a view more often than that finds the same path
version and is told `unchanged`; a slower one finds the view materialized anew at a later
version and receives a whole snapshot each time (correct, but not cheap).

**Paging a long text.** A device reads segments `0, 1, …` with `at` set to the version it
holds, chaining `c_i`, and checks the chain against its window's `before`, or the whole
against the text hash. It needs memory for one segment. Earlier segments of a streaming
message do not change while the Agent appends, so paging works while it still grows.

### 8.2 Holding

This is B6 for clients (memory model §8.4 "observed"):

- Following any path under `/sessions/<id>` or `/views/<id>` holds session `<id>`, like
  following the session itself. If the session is not held, it is loaded with `cwd` as
  §5.1 describes; its paths are absent until the load installs it.
- A `GET` of such a path also observes the session: it may start the load (with `cwd`),
  holds the session while it is served, and restarts the session's unobserved timer
  (`--session-unobserved-timeout`). A poll during a load returns what exists: the `op` is
  the running load, history-dependent paths are absent.
- Several followers and pollers hold one session together. After the last one leaves (a
  stream closes, or no `GET` arrives for the timeout), the session is released by the
  existing rule, which also keeps it while an operation, an interaction or a live resource
  needs it.

So a device that only polls opens, keeps and eventually releases a session on its own.

### 8.3 Request ordering

- **Within an instance**, versions are ordered: a client installs a `GET` result for a
  path it does not follow only if its path version is newer than what it holds there. A
  followed path is installed from the stream only.
- **Across instances**, versions have no order. When a client adopts a new instance
  (from a stream's first event or a `GET` result), every request it issued before that
  loses its right to install. A `GET` result from an instance other than the current one
  is installed only if the request was issued after the current instance was adopted;
  otherwise it is discarded. So a late answer of a dead instance never displaces a newer
  one, and the next request issued reaches the live instance.


### 8.4 Attachments and context

| Read | Notes |
| --- | --- |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | the block as received, in its exact form (below); served while the session's tree references it, else `404` |
| `GET /api/v2/sessions/{id}/files?query=` | context search within the session's workspace |
| `GET /api/v2/sessions/{id}/files/content?path=` | context read |

**Attachment identity.** The exact form of a block is a deterministic serialization of
the typed block layer 1 retains (memory model §8.3), not of the raw RPC bytes: JCS key
order and string escaping; integers the SDK parsed as `i64`/`u64` written exactly;
every other number as its parsed finite `f64` in JCS number form. So two blocks that
differ in an integer beyond ±2^53 differ in form, while a float the SDK already rounded
(for example `Annotations.priority` `0.10000000000000001`, parsed as `0.1`) has the form
of its parsed value. `id` is the SHA-256 of these bytes. The attachment read's body is
exactly these bytes, so a client verifies it by hashing the raw body before parsing;
a body whose hash is not the `id` is unverified (§9); a read whose block does not hash to its `id` is never shown as the attachment.

**Markdown export** is assembled by the client from entries, prompts, terminals and
attachment reads.


## 9. Hashing

`H` is SHA-256. `canon` is JCS (RFC 8785), with the double-domain caveat: JCS works in the IEEE double domain (RFC 8785 §3.1), so integers outside ±2^53 are canonicalized as their double, the value a browser holds; a hash cannot see differences beyond double precision. That limit applies to node hashes only, not to attachment identity (§8.4). Node hashes are 32 bytes internally; **on the wire a node hash is the unpadded
base64url of its first 16 bytes**, used only for comparison. Chain states (below) are
always sent whole: **32 bytes, unpadded base64url**, because a receiver continues the
computation from them.

- **Text** (a text position) with segments `s_0 … s_{m-1}` and `len` UTF-8 bytes:
  `c_{-1} = 0^32`, `c_i = H(c_{i-1} ‖ s_i)`, `h = H("t" ‖ u64be(len) ‖ c_{m-1})`.
  `c_i` is the **chain state** after segment `i`.
- **Value:** `h = H("v" ‖ canon(value'))`, where `value'` is the value with each text
  position replaced by the string `"#" + b64(h(text))` (32 bytes).
  Here `b64` is the unpadded base64url of the 32 bytes. Which strings are text positions is
  fixed by §2.6 and found by the node's kind, not by its content: for an entry, the `text` of
  each text block in a message's `chunks[*].content`, a compaction's `summary[*].content` and
  a tool call's `call.content[*].content` (items of type `content`); for a terminal, `output`;
  for an output view, `text`. Any other string, `text` members included, is plain JSON.
- **Object:** `h = H("o" ‖ canon(own') ‖ ∥_{name ascending} (canon(name) ‖ h(child)))`.
- **Map:** `h = H("m" ‖ ∥_{key ascending} (canon(key) ‖ h(child)))`.
- **List:** `h = H("l" ‖ u64be(n) ‖ h(child_0) ‖ … ‖ h(child_{n-1}))`.

Names and keys are ordered as JCS orders object keys (UTF-16 code units).

Properties:

- A hash depends only on content, not on any version.
- Verifying a subtree needs only that subtree. A client verifying one output view hashes
  one small object and one text chain.
- An append to a streamed message rehashes one segment and the path to the followed root.
- Both sides cache hashes by content identity (frozen replica values), (a cached hash is bound to the content it was computed from, never to an index or a
  revision; the client keeps replica values immutable, so every applied change is a cache miss).
- The list hash over all entries is `O(entries)` per checkpoint of a full replica, nobody else hashes that list.


## 10. Errors

```ts
interface ProtocolError {
  code: ErrorCode;
  message: string;
  data?: unknown;           // agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier; an answer that does not fit
  | "not_found"             // 404: unknown target or resource
  | "stale"                 // 409: `expect` is not the current op.id
  | "not_allowed"           // 409: status, occupied scopes or `closing` do not allow the write
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "unavailable"           // 503: no Agent connection
  | "agent_error";          // in op.error, end.error, Prompt.end: the Agent returned an error
```

The body of every non-2xx response is `{ error: ProtocolError }`. A stream that fails
ends; the client subscribes again with the versions it holds.


## 11. Previous revisions and changes

The [history index](history.md) links the fixed earlier drafts, revision 3 included. They
explain past designs and add no rules. Current work is tracked in
[the implementation plan](state-sync.md#7-fidelity-migration-revision-2).

Changes from revision 3 (reasons: [protocol-r4-design](protocol-r4-design.md) §2):

| Revision 3 | Revision 4 |
| --- | --- |
| scopes `global`, `catalog`, `session/<id>` as documents | one tree; those are its top-level paths; any path can be followed |
| `epoch` + `rev` per scope | `(instance, rev)` for the bridge; a session rebuild is a subtree replacement |
| `have=<scope>:<epoch>:<rev>` | `have=<path>:<path version>` with one `instance` per stream |
| document digest at snapshots and checkpoints | node hash of each followed path, at a stated path version |
| client-derived turns | `/sessions/<id>/turns`, an index derived by the bridge |
| — | `/views/<id>/output/latest`; fixed `/views/<id>/history/<instance>/<historyRev>/output/<start>` and `…/turn/<start>` |
| `op` with input and result | `op` head, `opDetail` with input and result |
| — | text windows with bounded events, segment reads |
| `after` on the operation | per-target operation log (last `K`), the outcome of a request |
| scope endpoints `/api/v2/scopes/…` | target endpoints `/api/v2/targets/…`; reads `/api/v2/nodes/…` |
| a `GET` holds nothing | a `GET` of a session path observes it like a short subscription |

## 12. Synchronization design requirements

Revision 4's mechanisms and the requirements they meet (R1–R4 and the scenarios S1–S5 are
in the [design](protocol-r4-design.md#1-requirements)):

| Requirement | Mechanism |
| --- | --- |
| One authority | a replica is installed only from bridge events or answers; a new instance drops earlier content (§5.6, §8.3) |
| Complete representation | the tree carries the memory model's state; byte content by verifiable reads (§3, §4, §8.4) |
| Atomic installation | the changes of one event are applied to copies and adopted together; a snapshot only after its hash matches (§5.2, §5.3) |
| Verified content | client-computed hashes at snapshots and checkpoints, over the subtree held (§9, §5.5) |
| Repair from the authority | resubscribe: a differential snapshot from the path version when the replica is trusted, a whole one after a mismatch (§5.3, §5.5) |
| Partial selection | follow any set of paths; windows bound text; segment reads page through it (§5.1, §5.4, §8.1) |
| Separate presentation and command outcomes | drafts and folding are UI state (C1); admission (`202`), effect (the tree) and loss (§6.6) are distinct |
| Resource reads | content-addressed attachment IDs; unverified reads are never shown |
| Commands during reconciliation | writes need no stream; their effect is read from the replica after it is current, or asked of the bridge |

### 12.4 Independent acceptance

Expected client trees are written from bridge states and fixtures, never computed by the
production projection or reducer under test. Fault injection happens at the client's actual
decode, apply, verify and install steps.

| Case | Required observation |
| --- | --- |
| Complete initial installation | every path of every kind survives the snapshot and verifies; attachments resolve and verify |
| Gap, duplicate and reorder | a missing, duplicated or reordered change is detected before it is applied (`from == v_p`); recovery by differential snapshot |
| Inapplicable op | an op whose target is missing leaves the replica unchanged and triggers resubscription |
| Wrong content with valid framing | alter a value in an event while keeping versions continuous; the next checkpoint detects it; a whole snapshot repairs it |
| Corrupted local replica | replace a value in the replica without an event; the next checkpoint or reconnect snapshot detects and repairs it; a differential snapshot never confirms it |
| Reconnect at the held version (#224, P2-1) | corrupt an append without changing its version, then disconnect before the checkpoint; a same-version `same` snapshot detects the wrong held content and triggers a resubscription without that path's `have`, whose full snapshot repairs it without any later change or checkpoint. Equal-version full and older-base differential snapshots also verify without reinstalling; older snapshots never roll the replica back |
| A path last changed long ago | a snapshot gives `v_p = at`, and the next ops change chains from it |
| The only change is lost | the path goes quiet; the next checkpoint has `at > v_p` and the client recovers |
| Late update to an early entry | a reconnecting client receives only that entry's path and verifies |
| Slow subscriber | queued events are coalesced into one event with a differential snapshot per path from the version written; ends are kept; the result verifies |
| New instance | a bridge restart gives a whole snapshot; earlier content is not confirmed; fixed views end `replaced` |
| Turn index | a partition change rewrites only the index; a fixed view ends `absent`; a load ends fixed views `replaced`, also after a resubscription without `have` |
| Text windows | a 1 MiB append arrives as one window of at most `N + L` bytes that verifies from its 32-byte `before`; 1 byte then 12,000 bytes at `N = 4096` is a `set`; a stale segment read is refused |
| Several clients | two followers at one version have equal hashes; their writes race as §6.6 states; two answers to one interaction deliver exactly one |
| Lost write answers | each row of §6.6, including a lost `create` resolved by `origin`, a lost `fork` resolved from its source session's log after another operation, a write lost while another operation replaced `global`'s `op`, and a session released and held again (`released`); a delete that occupied a session and failed leaves its `op` in the session, so a request that expected the replaced `op` is `superseded` and no `op.id` returns; only `superseded` allows a new request |
| A delayed request | the query answers `open` while `op.id == e`; the repeat of `(x, e)` is admitted or a no-op; exactly one prompt runs |
| Bounded heads | with a 1 MiB Agent error, `OpHead`, `OpRecord` and `End` stay bounded |
| Reconnect races | a superseded stream's late events cannot overwrite a newer replica; a late `GET` of an old instance is not installed (§8.3) |
| Polling | a client that only polls opens an unheld session, keeps it while polling, and it is released after the timeout once polling stops |
| Ended path in a slow queue | a `close` or `delete` end queued behind events reaches the slow subscriber; no snapshot is generated for that path |
| Restart | after the Agent exits with no session followed: a read, a stream of `global` or `catalog` and a write start no connection (the write is `unavailable`); `restart` answers `204`, starts one connection, and `agent.phase` follows to `ready`; a `restart` while the phase is `ready`, starting or initializing is `not_allowed` and starts nothing |
| Bad attachment read | wrong bytes are not shown; a read racing a release yields `404`, not wrong content |

Per-event convergence tests (replica equals bridge tree after every event) remain as a
separate regression gate.
