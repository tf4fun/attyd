# 简易 Agent 交互协议

This is attyd's project-defined protocol between the FE (and other presentation
clients) and the bridge. “Private protocol” here means project-defined. The bridge
remains an ordinary ACP client toward the Agent. The existing `/api/v2` endpoint
namespace and revision numbering belong to this application protocol, separately
from ACP's versions.

Status: **revision 2 target and synchronization working baseline**. Complete the
[ACP memory contract](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract) first,
then finalize this protocol's synchronization, verification and repair design.
[§12](#12-synchronization-design-requirements) is the brief for that second stage.
The current implementation and migration status live in [the plan](state-sync.md#7-fidelity-migration-revision-2);
the [source matrix](acp-bridge-matrix.md#1-来源和版本) fixes the ACP versions in scope.

**Layer split (2026-10-21).** What the bridge keeps in memory and how ACP input changes
it is defined only by the [bridge memory model](bridge-memory-model.md): one flat entry
list built by one reducer for load replay and live prompts, derived turns, prompt
outcomes, history knowledge, controls and resource lifetimes. This document describes
how that memory is carried to clients. Where the two disagree, the memory model wins.
§3's turn-shaped log and hash chain are the current wire baseline, to be revised in the
second stage.

This is the single definition of the client contract. Related documents have
separate roles: [design decisions](client-api-redesign.md),
[numbered acceptance cases](protocol-acceptance.md),
[implementation](state-sync.md), and [test evidence](acp-coverage.md).
Earlier protocol revisions and draft comparisons are in the [history index](history.md).

## 1. Model

attyd is a mediator without a persistence layer:

```text
client  <  bridge memory  <  ACP server
```

The bridge is an ordinary ACP client toward the ACP server; ACP defines that wire
behavior. Its implementation and retained displayable state must first pass the
[ACP-to-bridge memory contract](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract).
Toward clients, the bridge is the single leader of a replication
model in the manner of Raft: clients are read-only replicas of its memory and send
writes to it.

These are separate acceptance layers. A replica matching a lossy projection is not
ACP fidelity. This revision first completes the bridge's memory; client replication,
content verification and repair are then designed and reviewed independently (plan §7).
The synchronization design must carry the retained ACP state, not narrow it to fit
an existing wire shape.

The current proposal has eleven rules. This document defines them and their
fields, endpoints and behavior; §12 identifies what must be resolved before the
synchronization design is final.

**Bridge rules**

- **S1 Document.** Every scope is one document with two parts: `log`, a sequence of
  entries, and `live`, one JSON value. Each log entry has a chain hash
  `h_i = H(h_{i-1} ‖ canon(entry_i))`, with `h_0` fixed, over the entry exactly as it
  is published. A session scope's log is its settled timeline; every other scope's log
  is always empty. A session's log is unknown until the bridge holds a history for it,
  which is not the same as empty. Once known, its history is `complete` when it starts
  at the session's beginning, and `partial` when the Agent did not provide what came
  before.
- **S2 Settling.** A session's timeline is a sequence of turns. The part that has
  settled is the log; the turn still being received is `live.tail`. The log changes
  only at a settle point (§3.3), and a settle point is one event: the log change
  together with the `live` change that goes with it. Every other change happens in
  `live` only.
- **S3 Alignment.** A client subscribes with the head of the log it holds, `(n, h_n)`,
  the subscription's baseline. The bridge sends one cut as one event: the whole of
  `live` and, once the log is known, its log from the baseline if it has hash `h_n` at
  index `n`, otherwise from `0`; while the log is unknown, the client keeps its own,
  unconfirmed. Taking the cut and continuing is one serial step. The baseline advances
  only with log changes handed to the subscriber's output stream; every log change is
  computed from it and names the position it follows. A client that cannot apply one
  subscribes again with the head it holds.
- **S4 Writes.** A client write is an operation or an action. An **operation** starts
  bridge work that occupies scopes: `prompt`, `fork`, `create`, `delete`,
  `authenticate`, `logout`, `terminal_login`, `list`, and the bridge's own `load`.
  Every scope's `live` holds exactly one operation `op`, whose `id` is random and never
  reused; it identifies the operation and is what writes are admitted against, and
  says nothing about what a replica has seen. An **action** acts on something that
  exists now, without occupying anything: a session's controls (`set_mode`,
  `set_config`), the session itself (`close`), or a resource (`answer`, `input`,
  `resize`, `dismiss`). Actions take no `expect`, fall outside S5, and each states its
  precondition and its repeat semantics (§7).
- **S5 Admission.** Every operation write names `expect`, and the bridge handles
  operation writes and loads one at a time. A write that starts an operation needs
  `expect == op.id`, `op` ended, and every other scope it acts on free; the new
  operation replaces `op` in the same step. A cancel needs `expect == op.id`, `op`
  running and a kind that accepts it. An admitted operation occupies every scope it
  acts on until it actually ends, a requested cancel included: it is the `op` of each
  held one and holds back the load of each unheld one. Anything else is refused.
- **S6 Holding.** The bridge holds a session while it is needed: subscribed, occupied,
  with an action whose ACP request is outstanding (§7), or with a resource that can
  still act (a pending interaction, an unreleased terminal, a waiting URL flow). It
  releases a session that is not needed, after the
  release delay (`--session-unobserved-timeout`), only if the Agent can load or resume
  it; any other session stays held until it is closed, deleted, or the Agent
  connection ends. Subscribing to an unheld session starts a
  `load` operation. A one-off read holds nothing.
- **S7 Resources.** Interactions, terminals and URL flows live by their ACP identity:
  a resource belongs to the scope its ACP message names (a session, or one JSON-RPC
  request) and ends at the ACP event that ends it, never because some operation
  ended. Where a client sees it is a separate matter that never changes its life: a
  resource of a session not installed yet is shown in the scope of the operation
  installing it and moves into the session when it is installed. A terminal's
  **handle** (the process and its ID's use in `terminal/*`) and its **output** are
  separate: the handle ends with `terminal/release` or the Agent connection; the
  output is content and stays while content embeds it (§4.6).
- **S8 Content.** A session's documents carry what ACP gave for the session. Every
  timeline update belongs to an ACP **entity**: a message (its `messageId`, or, without
  one, the run of chunks it continues), or a tool call, ID-addressed plan or
  compaction (its ID, unique in the session). An update is reduced into its entity by
  that entity's ACP semantics (§3.2). Nothing else removes or rewrites content: not a
  turn's end, not an error, not a release, not a client's display. Every field is
  kept at the level it arrived on, `_meta` and annotations included; the bridge
  interprets none of them. The reduction itself is
  [memory model §3](bridge-memory-model.md#3-timeline-entries).

**Client rules**

- **C1 Projection.** A client's presentation is a pure function of `log`, `live` and
  its own UI state. It holds no authoritative state. Folding, collapsing and hiding
  are UI state.
- **C2 Refusal.** A refused write leaves its input with the user. Queued input that
  was never sent, or was refused with a definite answer, may be submitted
  automatically once the replica shows the operation ended.
- **C3 Uncertainty.** A write, action or terminal input sent without a definite answer
  may have taken effect. The client never resends it automatically. An interaction answer
  is the exception: it is delivered at most once (§7), so resending it is safe.

**Conventions.** All paths are under `/api/v2`. JSON field names are camelCase.
Embedded ACP types (`ContentBlock`, `SessionUpdate`, `ToolCallUpdate`, `PromptResponse`,
`RequestPermissionRequest`, `InitializeResponse`, …) are carried as the ACP schema
pinned in `Cargo.lock` defines them, `_meta` included. The one position the bridge
changes is a content block that carries bytes, which is replaced by an `AttachmentRef`
(§3.1). Types the bridge defines itself are never presented as ACP types.

| Part | Transport | Section |
| --- | --- | --- |
| Documents | in stream events and one-off `GET` | §2–§4 |
| Stream | SSE | §5 |
| Operations | `POST`, one endpoint | §6 |
| Actions | `POST` | §7 |
| Reads | `GET` | §8 |

## 2. Scopes

| Scope | Exists | Log |
| --- | --- | --- |
| `global` | always | empty |
| `catalog` | always | empty |
| `session/<sessionId>` | while the bridge holds the session | the session's settled turns |

A `session/<id>` scope is named by session ID only. Holding the same session again
continues the same scope name; S3 decides what a client keeps.

## 3. Log

### 3.1 Entries

```ts
interface Turn {
  items: Item[];            // the turn's timeline, in order (§3.2)
  end?: TurnEnd;            // how its prompt ended, when the bridge observed it
}

type Item = MessageItem | ToolItem | PlanItem | CompactionItem;

interface MessageItem {     // one contiguous run of one message's chunks (§3.2)
  type: "message";
  role: "user" | "agent" | "thought";       // user_message_chunk, agent_message_chunk,
                                            // agent_thought_chunk
  messageId?: string;       // the chunks' `messageId`; absent when they had none
  chunks: Chunk[];
}

interface Chunk {           // one received chunk, its own fields kept
  content: Block;
  _meta?: unknown;          // the chunk's `_meta`, apart from the block's own
}

interface ToolItem {
  type: "tool";
  call: ToolCallUpdate;     // ACP's sparse type, reduced (§3.2): a field is absent
                            // until the Agent provides it; `_meta` is the call's
}

type PlanItem =
  | { type: "plan"; update: Plan }                    // legacy: this turn's snapshot (§3.2)
  | { type: "plan"; update: PlanUpdate | PlanRemoved };  // by `planId`: the plan's state

interface CompactionItem {
  type: "compaction";
  compactionId: string;
  status: CompactionStatus;
  summary: Chunk[];         // from `summary` (no chunk `_meta`) and summary chunks
  error?: string;
  _meta?: unknown;          // the compaction's `_meta`, patched
}

type TurnEnd =
  | { response: PromptResponse }      // ACP, verbatim: stopReason, usage, _meta
  | { error: ProtocolError };         // the prompt failed, or its outcome is unknown

type Block = ContentBlock /* image, audio and embedded resource as AttachmentRef */;

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
  hash: string;             // h_i, encoded as in §3.4
}
```

- Item types are the bridge's own. Every ACP value inside them (`ContentBlock`,
  `ToolCallUpdate`, `Plan`, `PlanUpdate`, `PlanRemoved`, `PromptResponse`) is carried as the
  pinned schema defines it, `_meta` included.
- An attachment's `id` is computed over the whole block as the Agent or the client
  gave it, `_meta` included, and the bridge keeps that block. Reading an attachment
  (§8) and resolving a prompt's reference (§6.2) return it unchanged.
- Text and resource links stay inline. Annotations and `_meta` stay on every block.
- **Tool state.** A tool item holds the reduced state as the pinned `ToolCallUpdate`
  (`v1/tool_call.rs:220–236`), in which every field but `toolCallId` is optional. The
  pinned `ToolCall` cannot hold it: its `title` is required (`:33`), and
  `TryFrom<ToolCallUpdate>` fails without one (`:395–420`), while the first update the
  bridge sees for an ID may be a sparse `tool_call_update` or a permission's
  `toolCall`, e.g. `{ "toolCallId": "t" }`. A field the Agent has not provided is
  absent: the bridge fills in no title, kind or status, and drops no update for a
  missing field. A later update or `tool_call` provides it by rule 2. The state never
  holds `null`, since `null` keeps (rule 2). The same type is in the log, the tail, a
  replay and the generated TypeScript (`ToolCallUpdate` of the TS SDK). A client shows
  an absent field as not reported.

### 3.2 Reduction

Timeline updates are reduced into entries by
[memory model §3](bridge-memory-model.md#3-timeline-entries), identically for live
prompts and load replays; control updates go to `live.controls` (§4.5). On the wire, a
`Turn`'s `items` are the entries of one [derived turn](bridge-memory-model.md#5-derived-turns),
and its `end` is that turn's [prompt outcome](bridge-memory-model.md#42-prompt-outcomes).
Memory entry kinds map to items one to one: `message`, `tool`, `plan`, `compaction`.

### 3.3 Turns and settling

A turn is the part of the timeline one prompt produced. In memory it is derived from
the entries ([memory model §5](bridge-memory-model.md#5-derived-turns)); the settle points
below are when this wire baseline publishes a derived turn to the log.

- **Live.** When a `prompt` operation is admitted, the turn it starts is the tail. Its
  first item is the user message the bridge sends: one message item, role `user`,
  without `messageId` (the pinned `PromptRequest` has none), one chunk per prompt
  block, in order. Every update for the session that arrives while the prompt runs is
  reduced into the tail or, for an entity already in the log, into its entry (§3.2).
  User message chunks the Agent sends are items like any other and never start a turn
  live: the bridge does not guess which ones echo the prompt.
- **Replay.** A `session/load` replay has no prompt boundaries. It is reduced the same
  way, and a turn starts at each user message item, except one that continues the
  message (same, present `messageId`) of the user item that started the current turn.
  So a user message of text and an image is one turn, and a user message interrupted
  by another item is still one turn. Items before the first user message form a first
  turn. Replayed turns have no `end`.
- **Outside a prompt.** An update for an entity already in the log is reduced into its
  entry. Any other update is reduced into the tail, which then continues the last
  turn.

Settle points. Each is one event (S2):

| Settle point | Log change | `live` change in the same event |
| --- | --- | --- |
| a `prompt` operation is admitted | a tail that continues the last turn is reduced into the last entry, or becomes the first entry if the log is empty | the new operation; the tail is the new turn |
| the prompt ends, with any stop reason, an error, or an unknown outcome | the tail is appended as a new entry with `end` | the operation's final status; the tail is empty |
| the Agent connection ends | the tail settles as above; a running prompt ends with `end.error` (`unavailable`) | the operations' final status |
| an update for an entity already in the log | from that entry's index: the entry rewritten; the later entries' turns unchanged, their hashes recomputed along the chain | — |
| a load completes | the log rebuilt from the replay | the load's end; the tail is empty |

A prompt that the bridge refuses before it sends `session/prompt` is no operation and
adds nothing. Every prompt the bridge sent adds an entry, whether it failed or was
cancelled: content the Agent sent is not withdrawn by a later error.

### 3.4 Hash chain

- `h_0` is 32 zero bytes. `h_i = SHA-256(h_{i-1} ‖ JCS(turn_i))`, over the whole `Turn`
  as published: its items with their `_meta`, its attachment references, its `end`.
- On the wire, a hash is the unpadded base64url encoding of its first 16 bytes
  (22 characters). Comparisons use this form.
- Two logs with the same hash at index `i` hold the same entries up to `i`.
- Rewriting entry `i` (a late update, §3.3) changes `h_i` and therefore every later
  hash, although the later turns themselves are unchanged.

Hashes serve alignment only. Admission never compares them.

### 3.5 History status

Defined by [memory model §6](bridge-memory-model.md#6-history-knowledge-and-materialization),
including fork history; the table below is its wire form, and its fork paragraph is superseded by memory model §6.3.

| `live.history` | The log holds | Set by |
| --- | --- | --- |
| (the log is unknown) | nothing; the client keeps its own log, unconfirmed | holding a session by subscription, until its first load or resume ends |
| `complete` | the timeline from the session's beginning | `create`; a completed `session/load` (ACP: the entire history, `v1/agent.rs:4845–4848`); a fork only with an established complete history source (below) |
| `partial` | the known timeline is not guaranteed to start at the session's beginning | a `session/resume` of a session whose log was unknown (ACP: no previous messages, `v1/agent.rs:1277`); fork conditions remain to be specified below |

A resume of a session whose log is known keeps the log and its status. A `partial`
history arises whenever the bridge resumes a session whose log it did not hold: after
a release of a resume-only session, a `close`, a bridge restart, or a first open from
the catalog. v0.2.8 showed the same case as a missing-history notice. A `partial` log
is aligned like any other. A client that held earlier entries may keep them as its own
UI state and show them as not confirmed by the bridge (C1).

**Fork rule to finish before implementation.** Capturing a source for a fork must
include its timeline at admission, including an idle tail and referenced resources,
without mutating the source. A few target notifications during `session/fork` are
not evidence of a full replay. The choice between inherited source content and
target content, and its complete/partial status, needs an explicit rule (audit §9);
the earlier shorthand “fork replay, else source log” is not sufficient. A successful
`session/load` remains the explicit complete-replay path.

## 4. Live documents

### 4.1 Common fields

```ts
interface ScopeLive {
  op: Operation;
  interactions: Record<string, Interaction>;  // pending, by interactionId (§4.4)
  urlFlows: Record<string, UrlFlow>;          // by interactionId (§4.6)
}
```

Every scope's `live` extends `ScopeLive`. A new scope gets these fields empty.

### 4.2 Session

```ts
interface SessionLive extends ScopeLive {
  sessionId: string;
  cwd: string;
  history: "complete" | "partial";          // absent while the log is unknown (§3.5)
  controls: SessionControls;                // reduced state (§4.5)
  tail: { continues: boolean; items: Item[] };  // the turn being received (§3.3);
                                            // `continues`: it extends the last entry
  terminals: Record<string, Terminal>;      // Agent terminals (§4.6)
  closing: boolean;                         // a `close` action, or a release's
                                            // `session/close`, is outstanding (§7)
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
  diagnostics?: Diagnostics;                // later proposal, only if adopted (§4.7)
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

### 4.4 Operations and interactions

Interaction scopes and lifetimes are defined by
[memory model §8](bridge-memory-model.md#8-resources), and installation by its §9 (one
installation at a time, so a resource naming a not-yet-installed session has one owner).
This section shows where they appear on the wire.

The installation rule below requires an unambiguous association with the installing
request. The memory model proposes serializing `create` and `fork` through `global`, so
a resource naming a not-yet-installed session belongs to the one running installation
([memory model §9](bridge-memory-model.md#9-concurrency)); this is under review and not
implemented.

```ts
type OperationStatus = "running" | "completed" | "failed" | "cancelled";

interface Operation {
  id: string;                               // random, never reused (§6.5)
  kind: OperationKind;
  status: OperationStatus;
  input: unknown;                           // the write's `args` (a prompt as `Block[]`),
                                            // or the load's cwd
  cancelRequested: boolean;
  result?: unknown;                         // completed (§6.2)
  error?: ProtocolError;                    // failed
}
```

- `op` keeps its final status, result or error until the next operation replaces it.
  A prompt's content and outcome are in the log (§3.3); `op` keeps its input and error
  for a retry.
- `cancelled` means the operation ended because of a cancel before it produced a
  result. A prompt the Agent answered with stop reason `cancelled` is `completed`
  with that stop reason.
- **First operations.** A session the bridge starts holding by subscription starts
  with a running `load`. A session created by `create` or `fork` starts with a
  completed `open`. `global` and `catalog` start with a completed `none`.
- **Agent connection.** When the Agent connection ends, every running operation ends
  `failed` with `unavailable`.

```ts
interface Interaction {
  interactionId: string;                    // allocated by the bridge, never reused
  kind: "permission" | "elicitation";
  request: RequestPermissionRequest | CreateElicitationRequest;  // ACP, verbatim; its
                                            // ACP scope (sessionId, requestId) is in it
  parent?: { requestId: RequestId; method: string };  // a request-scoped elicitation:
                                            // the outstanding bridge request it names
}
```

An interaction is an Agent request waiting for a client. Its life follows its ACP
scope (S7); the scope whose `live` shows it follows from the same scope:

| ACP request | Shown in | Ends when (besides being answered, §7) |
| --- | --- | --- |
| `session/request_permission` for session S | `session/S`; while S is being installed, the scope of the installing operation (below) | the Agent cancels it (`$/cancel_request`); a cancel of S's prompt or a `close` of S is requested, which answers it `cancelled` (ACP MUST, `v1/client.rs:2589–2590`); S is not installed; S's scope or the connection ends |
| `elicitation/create`, session scope S (optionally a `toolCallId`) | as above | the Agent cancels it; S is not installed; S's scope or the connection ends |
| `elicitation/create`, request scope naming an outstanding request R of the bridge | the scope of the write or load that sent R; `global` when R belongs to none (`initialize`, a release's `session/close`, a forwarded MCP request) | the Agent cancels it; R's response arrives, and the bridge answers it with its cancel outcome; the connection ends |

- **Outstanding requests.** The bridge knows the JSON-RPC requests it has sent and not
  yet seen answered; an entry lives from send to response and is not kept after. A
  delete sends `session/close` and then `session/delete`: an elicitation naming the
  close ends with the close's response, although the delete continues. A `parent` is
  that request, never an operation, so no longer or shorter operation stands for it.
- **No outstanding request.** A request-scoped elicitation whose `requestId` names no
  outstanding request is answered at once with the JSON-RPC error `invalid_params` and
  never becomes an interaction. Two cases are told apart in the error's data: `null`
  (the bridge never sends a request with a null ID, so it names none) and a non-null
  ID that is not outstanding (never sent, or already answered). A session-scoped
  request for a session that is neither held nor being installed is refused the same
  way.
- **Installation.** `create` and `fork` install the session the Agent returns. Until
  their response, the session's interactions and URL flows are shown in the scope of
  the operation (`global` for `create`, the source session for `fork`). On success they
  move into `session/<id>` in the event that adds it to `global.sessions`. If the
  operation fails, the session is not installed: its permissions are answered
  `cancelled`, its elicitations `cancel`, and its URL flows removed. A session held by
  subscription exists before its `load` ends, so the load's interactions are shown in
  it directly.
- An Agent request does not change after it arrives. An operation's end ends no
  interaction; the end of a request ends those scoped to it.

### 4.5 Controls

**Representation gap (D12).** The DTO below describes reduced control values but
cannot yet carry all notification-level `_meta` or setup `modes._meta` promised by
S8. Complete the representation before implementing it: keep metadata with its ACP
source, separate from unrelated notifications, without interpreting vendor keys.
The reduction order below remains valid; it does not authorize dropping those
fields. A late action acknowledgement must also remain associated with the actual
request that sent it, rather than mutate a replacement session merely sharing its
ID (audit §9).

`controls` is the **reduced current state** of the session's controls, reduced by
[memory model §7](bridge-memory-model.md#7-controls). In summary, folded from three
sources in this order:

1. **Early updates**: control notifications that arrive before the `session/new`,
   `load`, `resume` or `fork` response, in arrival order.
2. **The response**, which overrides exactly the fields it provides (`modes`,
   `configOptions`). Fields it does not provide keep their early value.
3. **Later updates and acknowledgements**, in arrival order, whether or not a prompt
   runs. A successful `set_mode` or `set_config` action folds its acknowledgement when
   its response arrives, after every notification that arrived before it, whether or
   not the Agent also sent a notification. A failed one folds nothing. Several actions
   may be in flight, also during a prompt; each folds when its own response arrives.

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
| `session_info_update` | `title`/`updatedAt`: absent keeps, `null` clears, a value sets; `_meta` is `Option`, so typed ingress treats null and omission alike; preserve reported metadata at its source level |
| `usage_update` | sets `used` and `size`; `cost` absent or `null` keeps the last known cost |
| successful `set_mode` | sets `modes.currentModeId` to the requested `modeId` |
| successful `set_config` | replaces `configOptions` with the response's `configOptions` |

### 4.6 Resources

Lifetimes follow [memory model §8](bridge-memory-model.md#8-resources); this section is
their wire form.

```ts
interface UrlFlow {
  interactionId: string;                    // the accepted elicitation; identifies the flow
  elicitationId: string;                    // Agent-chosen; may be reused after the flow ends
  request: CreateElicitationRequest;
  status: "waiting" | "completed" | "ended";          // the flow's outcome, below
}

interface Terminal {
  terminalId: string;                       // session: the ID the bridge returned to
                                            // `terminal/create`, never reused in the
                                            // session; global: the operation ID
  output: string;                           // bounded tail (`trim`, §5.3)
  truncated: boolean;
  exitStatus: { exitCode: number | null; signal: string | null } | null;
  released: boolean;                        // session terminals: the handle is gone
}
```

| Resource | Created | Removed | Keeps its session needed |
| --- | --- | --- | --- |
| interaction | an Agent request arrives (§4.4) | it is answered or ends by its ACP scope (§4.4) | yes |
| URL flow | a client accepts a URL elicitation, with `status: "waiting"`; the interaction leaves `interactions` in the same step, and the flow keeps its ACP scope and where it is shown, moving with installation (§4.4). Accepting only consents. | a client's dismissal (§7); its session not being installed; the end of the document that shows it (a session scope ending; `global` and `catalog` never end). Before that it stays with its outcome: the Agent's `elicitation/complete` for its `elicitationId` (the one waiting flow with that ID) sets `completed`; the Agent connection's end sets `ended`. Its parent request's response does not end it (below). Only `waiting` changes: a finished flow keeps its outcome against a later connection end or completion | while `waiting` |
| session terminal | the Agent's `terminal/create` | once its handle is gone and no item of the log or the tail embeds it (`ToolCallContent` of type `terminal`); its scope ending | while its handle exists |
| terminal login | a `terminal_login` operation starts; it keeps its `methodId` for its whole life, whatever operation follows | it has exited and a client dismisses it (§7); the Agent connection ending | — |

**URL flow outcomes.** v0.2.8 told the user whether a flow completed or was given up,
also when the Agent completed it later, asynchronously. The outcome is therefore part
of the flow, not a separate record: a flow that stops waiting stays in `urlFlows`
with its `status` until a client dismisses it or the document that shows it ends, and
only a `waiting` flow keeps its session needed. `waiting` is the only status that
changes, once, to `completed` or `ended`. Nothing else is kept: no table of finished flows, no
request record, and a flow already dismissed is not shown again after a reconnect.

**Parent request of an accepted flow.** The pinned schema ties a request-scoped
elicitation to "a specific JSON-RPC request" (`v1/elicitation.rs:1539`) and states
no rule for an external URL flow the user already accepted when that request is
answered. The parent's response, success or error, ends only an interaction not yet
answered (§4.4). An accepted flow is identified by its `interactionId` and
`elicitationId`, not by the request, so it keeps `waiting` until a completion, the
connection's end, a dismissal or the end of the document that shows it, and no
mapping of the answered request is kept. This follows Zed's ACP v1 client at
[`28d0a35`](https://github.com/zed-industries/zed/commit/28d0a3547f16b9609b301b48ab363bfca649c864):
its [authenticate](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/agent_servers/src/acp/v1.rs#L1545-L1563)
awaits the response without touching accepted flows, and its shared thread model
[separates](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/acp_thread/src/acp_thread.rs#L774-L798)
finishing a request (pending interactions only) from cancelling it. Only the
lifecycle is taken, not Zed's display, which hides finished flows (release parity
P5). The shared finish/cancel methods do not establish that the v1 adapter tracks
parent RPC completion. The [newer fixed-source review](acp-bridge-matrix.md#5-zed-参考的适用范围)
explicitly separates those call paths; this remains a bridge lifecycle choice.

**Handle and output** (S7). A session terminal's handle is its process and the use of
its ID in `terminal/*`. It ends with `terminal/release`, or when the Agent connection
ends (the bridge then kills the process). Either way `released` becomes `true` and
the output and exit status are frozen. ACP keeps that output visible: "tool calls that
already contain it, continue to display its output" (`v1/client.rs:2621–2622`). So the
output is content: it stays for as long as an item of the log or the tail embeds the
terminal, through a reconnect of the Agent and a `session/resume` that keeps the log
(§3.5). Once the handle is gone, the output can be released when nothing embeds it;
ending its owning document also releases that ownership. It holds nothing: only a terminal with a handle keeps
its session needed. Its output is what the bridge kept; nothing is restored after the
session's documents are gone. No completion record is kept.

### 4.7 Diagnostics

This is a subsequent presentation proposal based on older UI capabilities, not a
requirement for basic ACP support or the first-layer memory milestone. Its scope is
reviewed separately (plan step 19); a full history of fs/MCP RPCs is not part of the
session content contract.

```ts
interface Diagnostics {
  agentStderr: { output: string; truncated: boolean };   // bounded tail (`trim`)
  mcp: {
    connections: Record<string, { serverId: string; name: string }>;  // open MCP-over-ACP
                                            // connections, by connectionId
    activity: McpActivity[];                // the last 200 messages, oldest first (`trim`)
    truncated: boolean;                     // older messages were dropped
  };
}

interface McpActivity {                     // one MCP-over-ACP message the bridge relayed
  direction: "toAgent" | "fromAgent";
  connectionId: string;
  method: string;
  kind: "request" | "notification" | "response";
  params?: unknown;                         // as relayed
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}
```

If this later proposal is adopted, `diagnostics` is one bounded part of
`global.live` for Agent stderr and MCP-over-ACP activity. It is in memory only, carried
as relayed, interpreted by nothing, and bounded so that it costs a fixed amount: the
stderr tail by the terminal bound, the activity by count. A connection leaves
`connections` when it is disconnected or the Agent connection ends; the activity
stays until it rolls off or the bridge restarts. MCP stderr lines are appended to
`agentStderr` with their server's name, as before. Diagnostics hold nothing and are
not an RPC console: they cannot be replayed, filtered or answered.

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

One stream carries any set of scopes. To change the set, a client opens a new stream
and ignores every event of the one it replaced.

Subscribing to a session scope:

- **held, log known:** the scope is aligned at once, log and `live`;
- **held, log unknown** (another client's subscription started the first load, which
  still runs): the alignment carries `live` only, and the subscription follows the
  load as below;
- **not held, free:** the bridge starts holding it with a running `load`. Its log is
  unknown, so the alignment carries `live` only: the client keeps its log,
  unconfirmed, and shows and answers the load's interactions. When the load ends, one
  event carries the log from the baseline (the suffix if the baseline matches, else
  the whole log) with the patch that ends the load and sets `history`. A missing
  `cwd` ends the scope with `load_failed` (`invalid_request`);
- **not held, occupied** (a delete runs on it, §6.2): the subscription waits until the
  occupying operation ends, then gets `end: deleted` or starts the load.

Subscribing to `global` or `catalog` holds nothing.

### 5.2 Events

Each SSE message carries one JSON event. Events carry no SSE `id`; there is no resume.

```ts
type Event =
  | { scope: string; log: LogChange; live?: LiveChange }
  | { scope: string; live: LiveChange }
  | { scope: string; end: { reason: "deleted" | "closed" | "load_failed"; error?: ProtocolError } };

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
3. **`end`.** The client drops the document. Nothing follows `end` for that scope on
   this stream.

Both parts of one event are applied together. After every event the client's `live`
equals a `live` the bridge held at some moment, and its log equals the bridge's log at
that moment, or, while that log is unknown, the client's own.

| Event | When |
| --- | --- |
| `{ log, live: { replace } }` | **alignment**: the first event of every subscribed scope, and every resync (§5.4). `from` is the matched index, or `0`. While the log is unknown, the alignment is `{ live: { replace } }`. |
| `{ log, live: { patch } }` at a first load's end | the log from the baseline, with the patch that ends the load |
| `{ log, live?: { patch } }` | **settle point** (S2, §3.3): `from` is the index of the first entry that changed, or the previous length for an appended entry. Also a rebuilt log after a reload (`from` is the longest common prefix with the log last sent). |
| `{ live: { patch } }` | every other change, including growth of the tail |
| `{ end }` | the session was deleted or closed, or its load failed |

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

`set` requires the parent to exist; every other op requires its target to exist. An op
that does not apply is a protocol error, and the client subscribes again. The bridge
picks the op that matches the change: growth uses `append`, an item folded in place
uses `set` on its index, a bounded tail moving forward uses `trim` plus `append`,
anything else `set` on the smallest enclosing value.

### 5.4 Synchronization

| Situation | What happens |
| --- | --- |
| first open, reconnect after a break, bridge restart | the client subscribes with the head it holds; one alignment event |
| the Agent's replay after a reload differs from the client's log | the head does not match; the alignment sends the whole log and the client replaces its own |
| a slow subscriber | the bridge drops the subscriber's queued events of each scope that still exists and queues one alignment from the baseline, the last log position handed to the output stream (a change still in the queue never advances it); an ended scope keeps its `end` |
| a cold reopen | the client's log stays, unconfirmed, while the session loads; the load's end confirms it with the suffix, or replaces it when the history differs |
| a cold reopen by `session/resume` | the history becomes `partial` (§3.5); the client's earlier entries are no longer confirmed and may stay as its own UI state |
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
| session | `prompt` | `{ prompt: PromptBlock[] }` | the session | `op` ended, not `closing` | `{ stopReason, usage? }` |
| session | `fork` | `{ cwd? }` (default: the source's `cwd`) | the source and `global` ([memory model §9](bridge-memory-model.md#9-concurrency)) | `op` ended, `global` free | `{ sessionId }` |
| global | `create` | `{ cwd }` | `global` | `op` ended | `{ sessionId }` |
| global | `delete` | `{ sessionId }` | `global`, `session/<sessionId>` | `op` ended, and the session free | `{}` |
| global | `authenticate` | `{ methodId }` | `global` | `op` ended | `{}` |
| global | `logout` | `{}` | `global` | `op` ended | `{}` |
| global | `terminal_login` | `{ methodId, cols, rows }` | `global` | `op` ended | `{ exitCode, signal }` |
| catalog | `list` | `{ cursor?, revision? }` | `catalog` | `op` ended | `{ revision, sessions: SessionInfo[], nextCursor }` |
| any | `cancel` | `{}` | — | `op` running, of kind `prompt` or `terminal_login` | — |

- **Bridge-started kinds.** `load` (a subscription, §5.1), `open` (the first
  operation of a created session) and `none` (the first operation of `global` and
  `catalog`) are never written by clients.
- **Prompt content.** A prompt may contain `AttachmentRef`s to attachments of the
  session. The bridge resolves each to the ACP block it names, unchanged, before it
  calls `session/prompt`; an unknown ID is refused with `invalid_request`. The
  operation's `input.prompt` is the normalized `Block[]`.
- **Cancel** applies to `prompt` (`session/cancel`, which also answers the session's
  pending permissions `cancelled`, §4.4) and `terminal_login` (the login process is
  stopped). Other kinds refuse it with `not_allowed`. A cancel sets
  `cancelRequested`; repeating it changes nothing. Requesting a cancel does not end
  the operation: it keeps its scopes occupied until it actually ends.
- **Controls and close** are actions (§7), not operations: they run while a prompt
  runs, as in v0.2.8.
- **Fork** creates a session the bridge holds, starting with a completed `open`.
- **Delete** closes a held session (`session/close`, when advertised), deletes it
  (`session/delete`), and ends `session/<sessionId>` with `deleted`. It needs the
  Agent's delete capability. It works on sessions the bridge does not hold.
- **List** runs the Agent's `session/list`. A `revision` that is not
  `catalog.live.revision` is refused with `catalog_changed`.

### 6.3 Response

| Outcome | Status | Body |
| --- | --- | --- |
| admitted | `202` | `{ op: "<id>" }`: the new operation's ID, or the running one's for cancel |
| `create`, `fork`, `list` | `200` | `{ op, result }` or `{ op, error }`, sent when the operation ends |
| refused | §9 | `{ error: ProtocolError }` |

- A `202` means admitted, nothing more. The replica shows the bridge's current state,
  which may lag behind or already be past the write.
- `create`, `fork` and `list` hold the response until the operation ends, including
  while their interactions wait. Clients give these requests no short timeout. A lost
  response loses the result for that client: a created session stays listed in
  `global.sessions` while held and in the catalog.

### 6.4 Admission

The bridge handles operation writes and loads one at a time:

| Write | Admitted when | Otherwise |
| --- | --- | --- |
| starts an operation | `expect == op.id`, `op` ended, every other scope in "Occupies" free | `stale` if `expect` differs; `not_allowed` otherwise |
| `cancel` | `expect == op.id`, `op` running, its kind accepts a cancel | `stale` if `expect` differs; `not_allowed` otherwise |

Capability, argument and Agent checks follow: `unsupported`, `invalid_request`,
`unavailable`. A refused write changes nothing.

An admitted operation is the `op` of every held scope it occupies, with the same `id`,
until it actually ends; an unheld occupied session cannot be loaded meanwhile. A scope
is free when its `op` has ended and no operation occupies it.

### 6.5 Operation IDs

Operation IDs are 128-bit random values in unpadded base64url. They never repeat,
across scopes, Agent connections and bridge restarts.

## 7. Actions

```text
POST /api/v2/scopes/{scope}/actions/{action}                    { …args }
POST /api/v2/scopes/{scope}/resources/{resourceId}/{action}     { …args }
```

Actions take no `expect` and are not operations (S4). A refusal before anything is
sent is a §9 error (`404` when the target does not exist). Otherwise:

| Target | Action | Body | Precondition | Response | Repeated |
| --- | --- | --- | --- | --- | --- |
| session controls | `set_mode` | `{ modeId }` | the session is held, its `op` is not a running `load`, not occupied by a `delete`, not `closing`; the mode was offered | `200 {}` or `200 { error }` when the Agent answers | sets a value: the same value again leaves the same state |
| session controls | `set_config` | `{ configId, value }` | as above; the option and value were offered | as above | as above |
| session | `close` | `{}` | the session is held, its `op` has ended or is a running `prompt`, not `closing`; the Agent advertises `sessionCapabilities.close` | `200 {}` or `200 { error }` when the Agent answers | while one is in flight, `not_allowed`; after success the scope is gone (`404`) |
| interaction (any scope) | `answer` | `{ outcome: RequestPermissionOutcome }` (permission) or `{ response: CreateElicitationResponse }` (elicitation) | it is pending | `204` | **at most once**: the answer is checked against the request, delivered as its response and removes the interaction in one step; a repeat, or an answer after the interaction ended, gives `404` and changes nothing |
| terminal login (`global`) | `input` | `{ data }` | it runs | `204` | **not idempotent**: the bytes are written to stdin each time, in the order the bridge receives them |
| terminal login (`global`) | `resize` | `{ cols, rows }` | it runs | `204` | sets the size; the same size leaves the same state |
| terminal login (`global`) | `dismiss` | `{}` | it has exited | `204` | removes it; a repeat gives `404` and changes nothing |
| URL flow (any scope) | `dismiss` | `{}` | — | `204` | removes it, waiting or finished (a waiting one stops being waited for); a repeat gives `404` and changes nothing |

- **Controls** run whatever the session's `op` is doing, a prompt included: ACP lets a
  client set the mode "at any time … whether the Agent is idle or actively generating"
  (`v1/agent.rs:4898–4899`), and v0.2.8 did the same for configuration options. The
  response is held until the Agent answers; the change reaches every replica through
  `controls` (§4.5). An Agent error is a definite answer and changes nothing.
- **Close** sets `closing` and sends `session/close`, after which the Agent "must
  cancel any ongoing work … and then free up any resources" (`v1/agent.rs:1432–1436`).
  Like a cancel, it answers the session's pending permissions `cancelled`. While
  `closing`, the session admits no operation (`not_allowed`). On success, one step
  removes everything the bridge held for the session: its running operations end
  (`cancelled`), its interactions are answered with their cancel outcome, its handles
  are killed, and the scope ends with `end: closed`; the session leaves
  `global.sessions` and stays in the catalog. Subscribing to it again loads it (S6),
  or resumes it with a `partial` history. On an Agent error `closing` clears and the
  session stays as it was. A lost response is uncertain (C3): the replica shows which
  it was.
- **Holding.** A control or close action whose ACP request is outstanding keeps its
  session needed (S6), from the request's send to its response, by the same
  outstanding-request entry that parents its elicitations (§4.4); nothing else is
  recorded. So a release never cuts it, whatever the release delay, `0` included,
  and the Agent requests it causes (a session-scoped elicitation, a permission) find
  their session held. Once it is answered, the session is released as usual if
  nothing else needs it. A release never starts while a client `close` is
  outstanding, and while a release's own `session/close` is outstanding the session
  is `closing`, so the two never overlap.
- **Response.** The Agent's response settles only the action it answers: it folds
  into the session that action named (§4.5) and answers that HTTP request. If the
  session's documents are gone by then (a delete was admitted meanwhile), it changes
  no document; if the Agent connection ends first, the action answers `unavailable`.
- An answer that does not fit its request (an option the request does not offer, a
  form response that fails its schema) is `invalid_request` and changes nothing.
- Each interaction is answered on its own. Two clients may answer two interactions of
  one scope; of two answers to one interaction, exactly one is delivered.
- Accepting a URL elicitation moves it to `urlFlows` (§4.6).
- A client sends terminal input in order, one request at a time per terminal, and
  never resends input whose delivery is uncertain (C3).

## 8. Reads

| Read | Notes |
| --- | --- |
| `GET /api/v2/scopes/{scope}` | `{ live, log? }` of an existing scope, as one cut; `404` for an unheld session; holds nothing. `log` is omitted while the session's log is unknown (S1); an empty log is `[]`, and the two never stand for each other. |
| `GET /api/v2/sessions/{id}/attachments/{attachmentId}` | the block as received, content-addressed; served while the session's `log` or `live` references it, else `404` |
| `GET /api/v2/sessions/{id}/files?query=` | context search within the session's workspace |
| `GET /api/v2/sessions/{id}/files/content?path=` | context read |

The catalog is listed by the `list` operation (§6.2), not by a read, because the Agent
may ask for input while it lists. **Markdown export** is assembled by the client from
the log, the tail, `live.terminals` and the attachment read; it covers everything the
v0.2.8 export did (messages with their `messageId`, thoughts, tool calls with
locations, content, diffs, terminals, raw input and output, plans, compactions,
attachments, each turn's stop reason and usage or error), as listed in the
[release parity matrix](release-parity.md#markdown-export).

## 9. Errors

```ts
interface ProtocolError {
  code: ErrorCode;
  message: string;
  data?: unknown;           // agent_error: the ACP error { code, message, data }
}

type ErrorCode =
  | "invalid_request"       // 400: malformed body, query or identifier; an answer that
                            //      does not fit its request
  | "not_found"             // 404: unknown scope or resource
  | "stale"                 // 409: `expect` is not the current op.id
  | "not_allowed"           // 409: the operation's status, the occupied scopes or
                            //      `closing` do not allow the write, or the target's
                            //      state does not allow the action
  | "catalog_changed"       // 409: stale catalog revision
  | "unsupported"           // 501: the Agent did not advertise the capability
  | "unavailable"           // 503: no Agent connection
  | "agent_error";          // on op.error, end.error and TurnEnd: the Agent returned an error
```

The body of every non-2xx response is `{ error: ProtocolError }`. A stream that fails
ends; the client subscribes again with its heads.

## 10. Previous revisions

The [history index](history.md) links the fixed earlier drafts and their migration
comparisons. They are evidence of past designs, not additional protocol rules.
Current work is tracked only in [the implementation plan](state-sync.md#7-fidelity-migration-revision-2).

## 11. Fidelity changes

The [ACP fidelity audit](acp-fidelity-audit.md#2-defects) owns D1–D12 and their source
evidence. [Release parity](release-parity.md) distinguishes stable-release regressions
from shared gaps and optional presentation work. Their target behavior is defined
in §§1–9 here; those findings are not duplicated as a second change table.

## 12. Synchronization design requirements

Status: **second-stage design requirements, 2026-10-20; not implementation
coverage.** Complete and directly verify the ACP-to-bridge memory contract first.
Then use that state to finish 简易 Agent 交互协议. Sections 1–9 provide the current
wire baseline; this section identifies what its later design must establish.
It does not select new fields, version numbers or repair algorithms.

### 12.1 Basic requirements

1. **One authority.** The bridge supplies the state clients can confirm. A client
   cache, draft or previously displayed history cannot restore facts that the
   bridge no longer holds or has not learned from the Agent.
2. **Complete representation.** Carry all displayable and actionable business state
   established by the first layer, preserving its meaning and lifecycle. Internal
   execution handles stay in the bridge. Referenced content may be read separately;
   collapsing a card does not remove its data from the protocol's coverage.
3. **Atomic installation.** A client installs the related content and state of one
   event together, from one bridge cut. Failure leaves no half-applied document and
   advances no confirmed position. The design states the boundary of a cut, including
   any related scopes; it must not imply cross-scope atomicity it cannot deliver.
4. **Verified content.** Confirmation concerns the content actually installed.
   Checking a supplied hash against another cached hash, or successfully applying
   a patch, is not by itself content verification. Define what is checked, when it
   is checked, and how failure becomes visible, for both history and mutable state.
5. **Repair from the authority.** Replace inconsistent or missing state using the
   bridge's current representation. Equal, verified content can be reused across
   reopen; unknown and partial history remain distinguishable from confirmed empty
   history. Repair does not replay business commands.
6. **Separate presentation and command outcomes.** Drafts, folding and selection
   remain client UI state. Sending a command, receiving its answer, and installing
   its effect are distinct events. Specify definite refusal, known completion and
   uncertain delivery without inventing success or automatically repeating a write
   whose result is unknown.

Use the same small set of rules for first open, reconnect, a slow subscriber and
reopen. Neither the bridge nor this protocol adds a persistence layer. Existing
runtime request correlation remains internal; synchronization does not require a
completed-request history.

### 12.2 Representation coverage

This maps semantic ownership, not new DTOs. Each row must have a defined document
or read representation before the synchronization design is considered complete.
The [source matrix](acp-bridge-matrix.md) remains the first-layer authority.

| Bridge state | Client-visible meaning to carry | Internal execution detail |
| --- | --- | --- |
| Connection and current work | Initialization/capabilities, authentication state, current work and its actual result or structured error | SDK connection, execution tasks and cancellation handles |
| Catalog and session knowledge | Current Agent session metadata, page/cursor, held state and unknown/partial/complete history | Load candidates, execution queues and subscription counts |
| Timeline and tail | Message/thought/tool/plan/compaction entities, order, boundaries, IDs, annotations and applicable source metadata | Entity indexes and superseded patches |
| Turn result | Complete received PromptResponse or error/interruption/unknown outcome associated with its content | Completed RPC correlation |
| Controls | Modes, configuration, commands, info, usage/cost and their source metadata; omitted/null/empty remain distinct where meaningful | Setup candidates and callback association |
| Pending interactions | Complete request/options/schema, scope, answerable identity and current availability | Original responder, locks and cancellation handles; the bridge sends the ACP reply |
| Accepted URL flows | Original request, URL, flow identity and waiting/completed/ended outcome | ACP completion routing |
| Referenced resources | Original attachment content through resolvable references; terminal output, truncation, known exit state and release state | Processes, PTYs, pipes and resource lookup indexes |

Catalog results and metadata, controls metadata, and notification-envelope metadata
still need their first-layer representation decisions. The current DTOs do not close
those items. Diagnostics are separate optional presentation work (§4.7).
Filesystem and MCP services do not automatically create conversation records.

### 12.3 Decisions required before the wire design is final

| Decision | Required answer |
| --- | --- |
| Representation and encoding | Show where every row above is carried. Define stable content equality, including metadata, field presence and numeric values supported by both runtimes. Preserve ACP meaning across encoding. |
| History verification | Explain how clients verify received entry bodies and their chain, and validate locally retained content before advertising it as confirmed. Specify the relation between the full chain digest and its wire encoding if §3.4 is retained. |
| Mutable-state verification | Define how a divergent `live` value is detected and repaired while the log/head remains unchanged, including idle periods. State the verification trigger and guarantee; a later prompt or accidental reconnect is not a verification policy. |
| Atomicity and ordering | Define snapshot/subscription handoff, patch failure, queue coalescing and delayed old-stream/GET results. A queued event is not a delivered event. State what consistency related scopes provide. |
| Repair | Define which verified content can be retained, which inconsistent portion is replaced, and how repeated failure is surfaced. A full authoritative replacement is a valid simple fallback; partial repair requires a demonstrated benefit. |
| Resource reads | Relate a separately fetched block or terminal result to the installed content. Distinguish pending reads, validation failure and genuine release; specify behavior when a read races replacement or release. |
| Commands and actions | State repeat semantics and the relationship between HTTP answers and replicated state, including concurrency and uncertain delivery. Reconciliation must not create a second business execution path. |

The baseline in §5 checks the preceding log position and patch applicability. Those
checks alone do not settle body verification, detection of a valid-looking but wrong
`live`, or validation of separately read resources. The design must close these
questions before claiming that clients can verify and repair the bridge projection.
This is an accidental inconsistency/corruption check, not proof that a faulty or
hostile authority is truthful.

### 12.4 Independent acceptance

These are required cases, not tests claimed to exist. Start with bridge states whose
ACP fidelity has already been verified. Write expected client content from those
states and independent fixtures; do not use the production projector or reducer to
generate its own expected answer.

| Case | Required observation |
| --- | --- |
| Complete initial installation | All applicable rows in §12.2 survive initial read/subscription, including metadata and resource resolution. Compare against the specified bridge cut, not a newer state reached concurrently. |
| Received body disagrees with its hash | Alter content, metadata or a result while keeping valid framing and unchanged supplied hashes. Neither GET nor stream installation may confirm it; repair restores the bridge content. |
| Local content disagrees with cached identity | Alter an installed entry but leave its cached head/hash intact. At the design's stated verification boundary the client detects and repairs it; a matching cached head cannot permanently confirm wrong content. |
| Live-only changes and divergence | With an unchanged log/head, update controls, interactions, URL outcomes, tail and terminals. Separately inject valid-looking wrong live content. The declared verification trigger must detect and repair it without relying on a subsequent log change. |
| Atomic failure in either half | Combine a valid log change with an invalid live patch, and invalid log content with valid live changes. Neither leaves half a new document or advances confirmation past failed content. |
| Rewritten history and queue coalescing | Start with a conflicting suffix, rewrite a settled entity and coalesce queued events. Remove every conflicting item/field and install a valid cut; unsent data cannot advance the delivery baseline. |
| Reconnect and competing sources | Break a stream during changes, replace the subscription and delay old events/GET results. Superseded sources cannot overwrite the new installed state; all accepted updates follow the declared ordering rule. |
| Referenced content | Return wrong bytes, interrupt a read, and race a resource release. The client distinguishes unavailable/unverified/released content; retained referenced content remains readable, including frozen terminal output. |
| History knowledge and restart | Distinguish incomplete load, confirmed empty load, resume without replay, Agent reconnection and bridge process restart. Cached UI content cannot become confirmed history without the bridge establishing it. |
| Commands during reconciliation | Vary HTTP/stream response order, use two clients and lose a write response. State repair preserves drafts, follows resource answer rules and causes no automatic replay of an uncertain business write. |

Keep per-event convergence checks as well. Add failure injection at the actual
client decode, verification and installation boundaries, including after it
cached an identity; comparing two copies of the same lossy projection is not enough.
The implementation must demonstrate its stated detection trigger, not merely
repair after the test manually forces a reconnect.
