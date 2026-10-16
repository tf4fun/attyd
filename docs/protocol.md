# attyd protocol v2

Status: **target specification, revision 2 (second draft, #51), 2026-10-16.** It is
served under `/api/v2`, the only browser API since `/api/v1` was removed (PR #46).
Revision 1 (2026-10-14) is what the code on `main` implements: it kept only the text of
user and Agent messages in the log and dropped every other session update once a turn
ended. **The runtime code has not been migrated to revision 2.** Each difference between
them is listed in the [ACP fidelity audit](acp-fidelity-audit.md), each capability of the
last release (v0.2.8) is compared in the [release parity matrix](release-parity.md), and
both migrate by the steps of [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2).
§11 summarizes the changes.

This document defines the client ↔ bridge protocol. Its rationale, derivations and
acceptance cases are in [client-api-redesign.md](client-api-redesign.md); how the
bridge produces it is in [state-sync.md](state-sync.md); the capabilities it must carry
are in the [ACP coverage ledger](acp-coverage.md). The baseline is the ACP schema
pinned in `Cargo.lock` (`agent-client-protocol-schema` 1.7.0, module `v1`, SDK feature
`unstable`). Newer ACP releases and drafts are not part of it.

## 1. Model

attyd is a mediator without a persistence layer:

```text
client  <  bridge memory  <  ACP server
```

The bridge is an ordinary ACP client toward the ACP server; that layer is ACP's and is
not specified here. Toward clients, the bridge is the single leader of a replication
model in the manner of Raft: clients are read-only replicas of its memory and send
writes to it.

The protocol is defined by eleven rules. They are restated from
[client-api-redesign.md §3](client-api-redesign.md#3-rules); the rest of this document
specifies their fields, endpoints and behavior.

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
  interprets none of them.

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

Timeline kinds are the message, thought, tool, plan and compaction updates. Control
updates (`available_commands_update`, `current_mode_update`, `config_option_update`,
`session_info_update`, `usage_update`) are reduced into `live.controls` (§4.5) and are
not items.

Three rules decide every reduction. The table applies them per update.

1. **Identity.** An entity with an ID unique in the session (tool call, ID-addressed
   plan, compaction) has exactly one item, at the position of its first update; every
   later update reduces into that item, wherever it is: in the tail, or in a settled
   entry, which is then rewritten (§3.3). A message is identified by its role and
   `messageId`; its chunks form one item while nothing else comes between them. An
   item that comes between ends that run, and the message's next chunk starts a new
   item with the same `messageId`: the timeline keeps the order, and the shared ID
   keeps the message's identity. Chunks without a `messageId` belong to the message
   their run continues.
2. **Patch.** A sparse update applies by its pinned type. For tool calls this is the
   field rule of `ToolCall::update`, applied to the `ToolCallUpdate` state: a field
   absent **or `null`** keeps the value (or its absence), a value replaces it,
   collections are replaced whole, so `content: []` and `locations: []` clear. A
   compaction's `summary`, `error` and `_meta` are three-state: absent keeps, `null`
   clears, a value replaces. A field the type gives no patch rule (`_meta` of a tool
   call update) follows the same rule as the type's other optional fields: absent or
   `null` keeps, a value replaces. Nothing is deep-merged.
3. **Fragments.** Each received fragment keeps its own fields. A text chunk joins the
   previous chunk of its item only when that chunk is text and every other field of
   both (annotations, the block's `_meta`, the chunk's `_meta`) is equal; the text is
   then appended. Otherwise it is a new chunk.

| ACP update | Reduction | ACP basis (schema 1.7.0) |
| --- | --- | --- |
| `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk` | Joins the last item when that item is a message of the same role and `messageId` (both absent counts as the same), by rule 3; otherwise starts a message item. | chunks of one message share `messageId`; a change means a new message (`v1/client.rs:614–617`); `ContentChunk._meta` (`:622–631`) |
| `tool_call` | Reduced as `ToolCallUpdate::from` the call, which carries every field it has, the schema's defaults for `kind`, `status`, `content` and `locations` included: it patches the item of its `toolCallId` by rule 2, or starts one. | `toolCallId` is unique within the session (`v1/tool_call.rs:30`); `From<ToolCall> for ToolCallUpdate` (`:434–465`) |
| `tool_call_update` | Patches the tool item of its `toolCallId` by rule 2; an unknown ID starts an item that holds exactly the update's fields. | `ToolCall::update` applies only present values; collections "are overwritten, not extended" (`v1/tool_call.rs:177–205`); `name` absent or `null` "left unchanged" (`:295–296`); "only changed fields need to be included" (`:210–211`) |
| `plan` | Replaces the legacy plan snapshot of the turn it arrives in, or starts one. | the client replaces the entire plan (`v1/plan.rs:36–37`); the schema gives the legacy plan no ID |
| `plan_update` | Upserts the plan item of its `planId`: the update is the plan's state. | `PlanId` is unique within a session (`v1/plan.rs:79`) |
| `plan_removed` | Sets the plan item of its `planId` to the removal, in place. A removed plan is no current plan; an unknown ID starts a removal item. A later `plan_update` with the ID makes it current again, in the same item. | removal notice for a plan identified by ID (`v1/plan.rs:353`) |
| `compaction_update` | Upserts the compaction item of its `compactionId`; the first update fixes its position. `status` replaces; `summary`, `error` and `_meta` by rule 2; a `summary` value becomes chunks without chunk `_meta`. | `v1/client.rs:214–221`; `compactionId` "unique within the session" (`:231`) |
| `compaction_summary_chunk` | Appends a chunk `{ content, _meta }` to the item's `summary`, by rule 3. | appended to the retained summary (`v1/client.rs:296–299`); "Metadata scoped to this chunk" (`:312–316`) |

- **Current state** is read from the items: the current ID-addressed plans are the
  plan items whose state is not a removal; the current legacy plan is the snapshot of
  the latest turn that has one, unless a later `plan` emptied it. A legacy snapshot is
  a record of that turn, not the current plan, and no update reduces into an older
  turn's snapshot.
- A permission request's `toolCall` is a `ToolCallUpdate` and is reduced like one,
  where its session's timeline is; the first one for an ID starts an item with only
  its fields.
- The same updates give the same items however they were chunked, live or replayed.
  Updates that differ in IDs, `_meta`, results or order give different items; the
  bridge does not make them equal.
- An update the bridge's validation rejects is dropped, as before, and reaches no
  document.

### 3.3 Turns and settling

A turn is the part of the timeline one prompt produced.

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

| `live.history` | The log holds | Set by |
| --- | --- | --- |
| (the log is unknown) | nothing; the client keeps its own log, unconfirmed | holding a session by subscription, until its first load or resume ends |
| `complete` | the timeline from the session's beginning | `create`; a completed `session/load` (ACP: the entire history, `v1/agent.rs:4845–4848`); a `fork`, from its replay or, without one, the source's log and status |
| `partial` | only what the bridge received since the Agent resumed it | a `session/resume` of a session whose log was unknown (ACP: no previous messages, `v1/agent.rs:1277`) |

A resume of a session whose log is known keeps the log and its status. A `partial`
history arises whenever the bridge resumes a session whose log it did not hold: after
a release of a resume-only session, a `close`, a bridge restart, or a first open from
the catalog. v0.2.8 showed the same case as a missing-history notice. A `partial` log
is aligned like any other. A client that held earlier entries may keep them as its own
UI state and show them as not confirmed by the bridge (C1).

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
  diagnostics: Diagnostics;                 // §4.7
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

`controls` is the **reduced current state** of the session's controls, folded from
three sources in this order:

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
P5).

**Handle and output** (S7). A session terminal's handle is its process and the use of
its ID in `terminal/*`. It ends with `terminal/release`, or when the Agent connection
ends (the bridge then kills the process). Either way `released` becomes `true` and
the output and exit status are frozen. ACP keeps that output visible: "tool calls that
already contain it, continue to display its output" (`v1/client.rs:2621–2622`). So the
output is content: it stays for as long as an item of the log or the tail embeds the
terminal, through a reconnect of the Agent and a `session/resume` that keeps the log
(§3.5), and it is gone only with its session's documents (a release, `close`,
`delete`, or a bridge restart). It holds nothing: only a terminal with a handle keeps
its session needed. Its output is what the bridge kept; nothing is restored after the
session's documents are gone. No completion record is kept.

### 4.7 Diagnostics

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

`diagnostics` is what v0.2.8 showed as the Agent's stderr and the MCP-over-ACP
activity panel, now one bounded part of `global.live`. It is in memory only, carried
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
| session | `fork` | `{ cwd? }` (default: the source's `cwd`) | the source | `op` ended | `{ sessionId }` |
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

## 10. Changes from the earlier v2 draft

| Earlier v2 draft | v2 |
| --- | --- |
| `Position`, `epoch`, `seq`, `Last-Event-ID`, `at`, `prev` positions | none; alignment by log head (§5) |
| `snapshot`, `reset` plus `GET /sessions/{id}` | alignment event `{ log, live: { replace } }` |
| `ops` | `{ live: { patch } }`, or a settle point `{ log, live: { patch } }` |
| `end` (`closed`, `unobserved`, `replaced`, `gone`) | `end` (`deleted`, `closed`, `load_failed`); a released session is loaded again on subscription |
| `position` | SSE comments |
| `session/<id>@<epoch>:<incarnation>` | `session/<id>` |
| `history.timeline`, `collapsedTurns`, process pages | `log` of settled turns (§3); `live.tail` |
| `turn`, `outcomes`, `operation` | `op`; a turn's `end` |
| `global.requests`, request records, acknowledgements | `op` of each scope |
| `interactions`, `requestId` parents | `live.interactions`; a request-scoped elicitation's `parent` is the outstanding JSON-RPC request it names (§4.4) |
| URL flow `status` and acknowledgement | `status` `waiting`, then `completed` or `ended`, kept until dismissal or the end of the document that shows it; no acknowledgement (§4.6) |
| `Idempotency-Key`, intents, `If-Match` tags, `historyRevision` CAS | `expect` (§6.4) |
| `POST /sessions/{id}/open`, first-subscription lease | subscription with `cwd` (§5.1) |
| `POST /sessions/{id}/close` | the `close` action (§7); release when not needed (S6) |
| per-command endpoints | `POST /scopes/{scope}/ops` |
| auth terminal input, resize, cancel | actions (§7); `cancel` operation |
| `GET /api/v2/catalog` | `list` on `catalog` |
| `GET /api/v2/global`, `GET /api/v2/sessions/{id}` | `GET /api/v2/scopes/{scope}` |
| `owner_mismatch`, `history_mismatch`, `precondition_required`, `conflict`, `intent_conflict`, `intent_capacity`, `gone`, `interaction_ended` | `stale`, `not_allowed`, `404` |

`/api/v1` no longer exists. For the record of how its routes map here, the earlier
draft's
[mapping from v1](https://github.com/tf4fun/attyd/blob/11d04d0/docs/protocol.md#8-mapping-from-v1)
applies through this table.

## 11. Changes from revision 1

Revision 1 is implemented on `main`; revision 2 is not yet. Each row links to its
audit finding.

| Revision 1 | Revision 2 | Finding |
| --- | --- | --- |
| `Turn { prompt, reply }`: user and Agent message text only; thoughts, tools, plans and compactions dropped at commit and from replays | `Turn { items, end? }`: every timeline update, reduced (§3.1–§3.2) | D1 |
| Agent messages of a turn concatenated; text merged across message boundaries | items keep `messageId` and boundaries | D2 |
| user message chunks of a live turn dropped | kept as items | D3 |
| a failed prompt adds no entry; its process is dropped | every sent prompt settles, with `end.error` | D4 |
| updates outside a prompt dropped; no path for late updates of committed entities | the tail continues the last turn; an update to a settled entity changes its entry (§3.3) | D5 |
| the hash covers message text only | the hash covers the whole published entry (§3.4) | D6 |
| `_meta` dropped at every level, also in attachments | carried; attachments are kept and served as received | D7 |
| a released terminal leaves `live` with its output | it stays, frozen and `released`, while content embeds it (§4.6) | D8 |
| a resume of an unknown log gives a known empty log | `history: "partial"` (§3.5) | D9 |
| `answer` must cover all pending interactions of the operation | each interaction answered on its own, at most once (§7) | D10 |
| interactions join the running operation, or start an `agent_request`; an operation's end cancels them | interactions are resources of their ACP scope; only a request-scoped one has a parent, the JSON-RPC request it names (§4.4) | D11 |
| `op.process` on a running prompt | `live.tail` on the session | D1, D5 |
| S2 Commit, I8 privacy (a committed turn's process never appears again) | S2 Settling, S8 Content | D1 |
| plans reduced per turn by `planId` | an ID-addressed plan is one item per session; a removal sets it, in place (§3.2) | D1 |
| tool updates: every present field replaces, `null` included | `ToolCall::update`: absent and `null` keep, collections replace (§3.2) | D1 |
| tool state as `ToolCall` | `ToolCallUpdate`: a first sparse update or permission keeps only its fields, nothing filled in (§3.1) | D1 |
| chunk `_meta` merged away with the text | each chunk keeps its fields; text joins only equal neighbours (§3.2) | D7 |
| a released terminal's output removed on release and at the connection's end | handle and output separated; the output stays while embedded (§4.6) | D8 |
| `set_mode`, `set_config` are operations, refused during a prompt | actions, allowed during a prompt; an outstanding one keeps its session held (§7) | R1 |
| no `close` | the `close` action, also during a prompt; `end: closed` (§5.2, §7) | R2 |
| request-scoped elicitations parented by operations; unknown request IDs go to `global` | parented by the outstanding JSON-RPC request; no outstanding request is refused (§4.4) | D11 |
| interactions of a session being created ended with the `create` | moved into the session when it is installed (§4.4) | D11 |
| Agent stderr and MCP-over-ACP activity not projected | `global.live.diagnostics`, bounded (§4.7) | R3 |
| a URL flow is removed when it completes or ends, so its outcome is not shown | the flow stays with `status` until dismissed; only `waiting` changes (§4.6) | R4 |
