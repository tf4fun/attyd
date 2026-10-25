# Protocol revision 4: partial synchronization and serialized writes

Status: **accepted and implemented** (issue #201, reviewed on PR #202, tracked and implemented in #203; §15 lists what each review changed). This document is the design record: requirements, the reasons revision 3 could not meet them, alternatives and decisions. The specification is [protocol.md](protocol.md). Revision 3 is at the fixed baseline linked in the [history index](history.md).
Layer 1, the [bridge memory model](bridge-memory-model.md), is unchanged: this revision
changes how that memory is published (layer 2), how clients write to it, and one
holding rule (§7.2).

## 1. Requirements

From #201. The bridge keeps the ACP client state; a client chooses what to follow and
should pay only for that. Full replication remains one way to consume the same system,
not the minimum every client must meet.

| | Requirement |
| --- | --- |
| R1 | A client follows the latest content of what it selects: a whole session, one turn, or one message. Receiving, storing and verifying cost scales with the selection, not with the session's history. |
| R2 | A selection recovers independently after a break, a missed update or a corrupted local copy, without holding the rest of the session. Recovery converges to the bridge's current state; intermediate updates may be coalesced. |
| R3 | Several clients view one session independently, with different selections, without taking the right to write. For the same session, the same effective version and the same selection, every client holds the same content. |
| R4 | The bridge decides the order of conflicting writes to one session and executes each request at most once. Serialization may refuse competing requests; no server queue is assumed. A cancel and an interaction answer are control input of the running operation and are never ordered after it. |

Two directions of idempotence are kept apart:

- **receive:** an update delivered twice is applied once;
- **submit:** a request sent twice executes once. The request's identity, not its text,
  decides: two submissions with equal text and different IDs are two requests.

Scenarios the design must explain (§10):

- S1. An ESP32 shows the final output of the latest turn. It connects and recovers without
  downloading the session, and follows changes to that output.
- S2. A desktop client views the whole session while the device views one output; at the
  same effective version the content they share is equal.
- S3. A duplicated update is not shown twice; a retry after a lost answer does not execute
  again.
- S4. Two clients submit conflicting writes to one session at once: no interleaving, and
  each learns whether it was accepted.
- S5. During a prompt, cancels and interaction answers are still handled.

Out of scope, as #201 asks: a general query language, a persistent message queue, and a
ledger of every past request.

## 2. Why revision 3 cannot meet R1–R2

| Revision 3 | Consequence |
| --- | --- |
| A scope is one document; a stream follows whole scopes (§5.1) | the smallest selection is a whole session |
| `epoch` + `rev` and the digest cover the whole document (§5.5) | verifying one message needs every entry's hash |
| Turns are derived by the client from all entries (§3.3) | finding a turn needs the whole history |
| `op.input` carries the whole request (§4.4) | learning the `expect` to write with costs the size of the last prompt |
| Per-scope random epochs (§5.4) | versions of different scopes, or of one scope across a rebuild, have no order |
| `after` names only the issuing scope's predecessor (§6.6) | a lost answer is unknown after two further operations, or when an operation of another scope replaced the `op` |
| A `GET` holds nothing (B6) | a client that only polls cannot open a session |

What revision 3 already does right and this revision keeps: flat entries as the memory
model has them, client-chosen operation IDs, admission against `expect`, actions for
control input, content-addressed attachments, differential snapshots by change revision,
client-computed verification, coalescing a slow subscriber into a snapshot.

## 3. Overview

1. **One state tree, one version.** Everything the bridge publishes is one tree:
   `/global`, `/catalog`, `/sessions/<id>`, and derived `/views/…`. One commit of the
   bridge lock is one revision `rev`; versions are `(instance, rev)`, with a random
   `instance` per bridge process. Per-scope epochs disappear.
2. **Path subscriptions.** A client follows any set of paths. For each it holds the
   subtree and that subtree's **path version**: the revision at which anything in it last
   changed. Snapshots state it, changes chain from it, so gaps are found per path.
3. **Content hashes per node.** A Merkle hash lets a client verify exactly the subtree it
   holds. Long text is hashed as a chain of fixed segments, so a client can hold, verify
   and page through a window of a message larger than its memory, with every event to it
   bounded.
4. **A turn index and views.** The bridge publishes the turns of its flat entries as a
   small index, and derived views for what moves (the latest output) and for one fixed
   turn, so no client derives turns from the whole history and a device follows one fixed
   path.
5. **Writes** keep `id` + `expect`. A small operation head carries what a writer needs.
   Every operation is recorded, in each target it occupies, in a bounded log; a lost
   answer has four defined outcomes, and only one of them allows a new request.

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
| `after` on the operation | per-target operation log (last `K`), resolution read |
| `GET` holds nothing | a `GET` of a session path holds it like a short subscription |

## 4. The state tree

### 4.1 Version

- `instance`: 128 random bits (unpadded base64url), chosen when the bridge process starts.
  Agent restarts within the process keep it; nothing published survives a process restart.
- `rev`: an integer, starting at 1, increased by exactly one by every commit that changes
  anything published. One commit may change several paths (installing a session adds
  `/global/sessions/<id>` and `/sessions/<id>` in the same `rev`).
- Versions of one instance are totally ordered. Versions of different instances are not
  comparable (§6.6, §7.3).

### 4.2 Paths and nodes

A path is a JSON Pointer into the tree (a session ID containing `/` or `~` is escaped as
RFC 6901 says). In query parameters of the form `<path>:<value>`, the value follows the
**last** `:`; in `cwd=<id>:<dir>` the ID is percent-encoded and ends at the **first** `:`.

The tree's shape is fixed by this document: some positions are **nodes** (they have their
own path revision and hash), the rest are plain JSON inside a node.

| Kind | Content | Children |
| --- | --- | --- |
| value | any JSON | none (but may contain text positions, §4.6) |
| object | own fields (JSON) | named child nodes |
| map | the set of keys | one node per key |
| list | the length | one node per index |

Shape (`·` marks a node, indentation children):

```text
· /global                       object: { agent }                   (revision 3 §4.3)
    · op                        value: OpHead (§4.4)
    · opDetail                  value: { input, result?, error? }
    · ops                       list of value OpRecord (§9.3)
    · sessions                  map sessionId → value SessionSummary
    · loadFailures              map sessionId → value ProtocolError (§6.2)
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
    · turns                     list of value TurnHead (absent while history is unknown, §4.3)
· /views/<id>/output/latest     value: OutputView (§4.5)
· /views/<id>/history/<i>/<h>/output/<start>   value: OutputView
· /views/<id>/history/<i>/<h>/turn/<start>     object: TurnHead fields; child · entries (list of value Entry)
```

`historyRev` is the revision at which the session's history was last installed or
replaced (a load, a resume, a fork's load). `heldSince` is the revision at which the
bridge started holding the session this time; it changes only when the session is
released and held again. `/global` and `/catalog` are held for the whole instance. Entries, prompts, controls, interactions,
terminals and attachments keep their revision 3 wire types.

### 4.3 The turn index

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

Revision 3's client-side derivation is removed: the browser reads `turns`.

### 4.4 The operation head

```ts
interface OpHead {           // revision 3 Operation without `input`, `result` and the error's body
  id: string; scope: string; after: string | null; kind: OperationKind;
  status: OperationStatus; cancelRequested: boolean;
  error?: ErrorSummary;      // of `opDetail.error`, when it failed
}

interface ErrorSummary {
  code: ErrorCode;           // revision 3 §9
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

### 4.5 Views

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

  So recovering a broken replica (resubscribing without `have`, §6.5) re-reads the same
  selection, and reconnecting to a new instance (§6.6) ends it; neither can turn it into
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
which its value last changed, or at which it was materialized, whichever is later (§4.7).
Following or reading a view holds its session like following the session (§7.2).

### 4.6 Text positions

These string positions are **text positions**:

- the `text` of every ContentBlock of type `text` inside an `Entry` (message chunks,
  tool call content, compaction summaries);
- a terminal's `output`;
- an `OutputView`'s `text`.

A text position is a string in the JSON like any other. It differs in how it is hashed
(§5) and in that a subscription may hold only a window of it (§6.4). Segmentation is a
pure function of the string: its UTF-8 bytes are split greedily from the start into
segments of at most `L = 4096` bytes, each ending on a code point boundary. `L` is a
protocol constant: it is part of every text hash. Appending changes only the last segment
and adds new ones; earlier segments never change.

Other large values (a tool's `rawInput`/`rawOutput`, an interaction's request, plan
bodies) are not segmented. A small client controls their cost by not following the paths
that contain them, which the tree makes possible.

### 4.7 Path revisions

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

## 5. Hashing

`H` is SHA-256. `canon` is JCS (RFC 8785), with revision 3's double-domain caveat (§5.5
there). Node hashes are 32 bytes internally; **on the wire a node hash is the unpadded
base64url of its first 16 bytes**, used only for comparison. Chain states (below) are
always sent whole: **32 bytes, unpadded base64url**, because a receiver continues the
computation from them.

- **Text** (a text position) with segments `s_0 … s_{m-1}` and `len` UTF-8 bytes:
  `c_{-1} = 0^32`, `c_i = H(c_{i-1} ‖ s_i)`, `h = H("t" ‖ u64be(len) ‖ c_{m-1})`.
  `c_i` is the **chain state** after segment `i`.
- **Value:** `h = H("v" ‖ canon(value'))`, where `value'` is the value with each text
  position replaced by the string `"#" + b64(h(text))` (32 bytes).
  Here `b64` is the unpadded base64url of the 32 bytes. Which strings are text positions is
  fixed by §4.6 and found by the node's kind, not by its content: for an entry, the `text` of
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
- Both sides cache hashes by content identity (frozen replica values), as revision 3 §5.5
  requires.
- The list hash over all entries is `O(entries)` per checkpoint of a full replica, as in
  revision 3; nobody else hashes that list.

## 6. Stream

### 6.1 Subscribing

```text
GET /api/v2/stream?instance=<i>&follow=<path>&have=<path>:<version>&window=<path>:<bytes>&cwd=<id>:<dir>
```

| Parameter | Meaning |
| --- | --- |
| `follow` | a path to follow; repeatable. Paths may not overlap (one may not contain another). |
| `instance` | the instance the client's holdings belong to; omitted when it holds nothing |
| `have` | per followed path: the path version it holds and has applied without error, or verified; omitted after a failed verification |
| `window` | per followed path: hold text positions in it as windows of `bytes` (§6.4); without it, whole strings |
| `cwd` | the working directory to load an unheld session with (§7.2) |

One stream carries any set of paths. To change the set the client opens a new stream and
ignores the old one (as revision 3).

### 6.2 Events

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

- A **snapshot** sets the content of `p` and `v_p = snapshot.at` (§6.3), not the event's
  `rev`: a path last changed at 5 and snapshotted at global revision 100 is held at 5.
- An **ops change** in a commit event at `rev` carries `from`, the path version before the
  commit. The client applies it only if `from == v_p`, then sets `v_p = rev` (the commit
  changed `p`, so its new path version is `rev`).
- A change whose resulting version is `≤ v_p` (a snapshot with `at ≤ v_p` and the same
  instance, or an ops change with `rev ≤ v_p`) is a duplicate and is ignored (**receive
  idempotence**). Any other mismatch, or an op that does not apply, is a gap: the client
  resubscribes with its `have`s.
- All changes of one commit arrive in one event and are applied together: everything a
  client follows is consistent at that commit.
- `Op` is revision 3's (§5.3), with pointers relative to `p`.
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

### 6.3 Snapshots

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
children with `srev > h`. The client installs into a copy: it keeps every child the
structure names and the snapshot omits, drops every child the structure no longer names,
and replaces the rest. By §4.7 a kept child is exactly the bridge's current value. The
copy is adopted only if its hash matches; then `v_p = at`.

A full snapshot is `{ base: null, at, node: { v } }`. A differential snapshot needs no
knowledge of what the client held beyond `h`, since structure is always sent where
anything below changed. When `srev(p) ≤ h`, nothing changed since the client's version: the
snapshot is `{ base: h, at: srev(p), hash, node: { same: true } }`, and the client keeps what
it holds (a `GET` says `unchanged`, §7.1). A list's changed children are keyed by index.

### 6.4 Text windows

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

### 6.5 Checkpoints, recovery, coalescing and keepalive

- **When checkpoints are sent**: for each followed path, after its changes pause for
  500 ms, and at least every 2 s while it keeps changing; also in every snapshot (`hash`).
- **Mismatch**: on a hash mismatch the client drops its replica of that path and
  resubscribes **without** that path's `have`; the bridge sends the whole subtree. If a
  full snapshot fails verification again, the client reports that synchronization failed
  and retries with backoff. A path is shown as confirmed only up to its last verified
  version. Dropping a replica never changes which path is followed: a fixed view keeps its
  `history/<i>/<h>` binding (§4.5).
- **Slow subscriber**: when a stream's queue exceeds its budget, the bridge drops the
  queued events and queues one catch-up event at the current `rev` carrying, per followed
  path, a differential snapshot from the last `v_p` it actually wrote. Ends are kept.
  Coalescing never changes what a path version contains.
- An SSE comment at least every 15 s on an idle stream.

### 6.6 A new instance on a stream

The first event of a stream names its `instance`. If it differs from the client's, the
client drops everything it holds from the old instance and installs the snapshots, which
are full (`base: null`, since its `have`s named another instance). A stream's events
belong to its instance; the client never mixes two.

## 7. Reads and holding

### 7.1 Reads

| Read | Result |
| --- | --- |
| `GET /api/v2/nodes/{path}?instance&have&window&cwd` | `{ instance, rev, change }`: what a stream's first event would carry for that path, or `{ instance, rev, unchanged: true }` when `instance` matches and `srev(path) ≤ have` |
| `GET /api/v2/nodes/{path}/segments/{i}?at=<version>` | `{ before: c_{i-1}, segment: s_i }` for the text position at `path`; `409 stale` when segment `i` changed after `at` |
| attachments, context search and read | as revision 3 §8 |

**Polling.** A client that cannot keep a stream polls `GET /nodes/{path}?have=` with its
path version; it gets a differential snapshot or `unchanged`. Updates between polls are
coalesced, which R2 allows; latency is the polling interval.

**Paging a long text.** A device reads segments `0, 1, …` with `at` set to the version it
holds, chaining `c_i`, and checks the chain against its window's `before`, or the whole
against the text hash. It needs memory for one segment. Earlier segments of a streaming
message do not change while the Agent appends, so paging works while it still grows.

### 7.2 Holding

This replaces revision 3's B6 for clients (memory model §8.4 "observed"):

- Following any path under `/sessions/<id>` or `/views/<id>` holds session `<id>`, like
  following the session itself. If the session is not held, it is loaded with `cwd` as
  revision 3 §5.1 describes; its paths are absent until the load installs it.
- A `GET` of such a path also observes the session: it may start the load (with `cwd`),
  holds the session while it is served, and restarts the session's unobserved timer
  (`--session-unobserved-timeout`). A poll during a load returns what exists: the `op` is
  the running load, history-dependent paths are absent.
- Several followers and pollers hold one session together. After the last one leaves (a
  stream closes, or no `GET` arrives for the timeout), the session is released by the
  existing rule, which also keeps it while an operation, an interaction or a live resource
  needs it.

So a device that only polls opens, keeps and eventually releases a session on its own.

### 7.3 Request ordering

- **Within an instance**, versions are ordered: a client installs a `GET` result for a
  path it does not follow only if its path version is newer than what it holds there. A
  followed path is installed from the stream only.
- **Across instances**, versions have no order. When a client adopts a new instance
  (from a stream's first event or a `GET` result), every request it issued before that
  loses its right to install. A `GET` result from an instance other than the current one
  is installed only if the request was issued after the current instance was adopted;
  otherwise it is discarded. So a late answer of a dead instance never displaces a newer
  one, and the next request issued reaches the live instance.

## 8. Write targets

Operations and actions are written to a **target**: `global`, `catalog` or
`sessions/<id>`, each with its `op`, `opDetail`, `ops`, `interactions` and `urlFlows`.

```text
POST /api/v2/targets/{target}/ops                         { id, expect, kind, args }
POST /api/v2/targets/{target}/actions/{action}            { …args }
POST /api/v2/targets/{target}/resources/{id}/{action}     { …args }
GET  /api/v2/targets/{target}/ops/{id}?expect=<e>&instance=<i>&at=<version>
```

Kinds, actions, arguments, occupation and admission are revision 3's (§6.2, §6.4, §7),
renamed from scopes to targets.

## 9. Writes

### 9.1 Admission (unchanged)

The bridge admits operation writes one at a time against the target's current `op`:
`expect` must equal `op.id`, the `op` must have ended, and every other target the kind
occupies must be free. Otherwise `stale` or `not_allowed`, and nothing changes. There is
no queue: competing writes are refused (R4). `cancel` is admitted while the `op` runs;
interaction answers are actions. Neither waits for the running operation (S5).

### 9.2 At most once for a fixed request

> A request `(x, e)` (operation ID `x`, `expect = e`) executes at most once, however often
> it is sent, lost, delayed or repeated, and across bridge restarts.

Admission requires `op.id == e` and makes `op.id = x`. IDs are 128 random bits, never
reused, so once `x` is admitted the target's `op.id` is never `e` again: a later copy of
`(x, e)` finds `op.id == x` (`202`, no effect) or `op.id ≠ e` (`stale`). After a bridge
restart every `op` has a new bridge-allocated ID, so any copy is `stale`.

The guarantee is about **one request identity**. A client that creates a *new* request
`(y, e')` for the same user input creates a second request, and may cause a second
execution, unless it knows `(x, e)` can no longer be admitted. §9.3 says when it knows.

### 9.3 Outcomes: a bounded operation log

```ts
interface OpRecord {
  id: string;
  after: string;             // this target's op.id it replaced
  kind: OperationKind;
  rev: number;               // the commit that admitted it
  status: OperationStatus;   // updated until it ends
  error?: ErrorSummary;      // as in OpHead
}
```

Every target keeps `ops`: the records of the last `K = 64` operations that occupied it,
**including those written to another target** (a `delete` written to `global` is recorded
in `global` and in the held session it occupies, each with its own `after`). A `fork` is
recorded only in its source session; admission excludes global writes and other forks
while it runs. Within the window, each
target's log is the complete chain of its `op`s. A target's log lives as long as the
target: releasing a session and holding it again starts an empty log.

The outcome of a request `(x, e)` written to target `W`, while the client held instance
`i` and `W` at path version `t`:

| Condition | Outcome | What it means |
| --- | --- | --- |
| the instance is not `i` | `unknown` (`restarted`) | `(x, e)` can no longer be admitted (§9.2), but whether it was is lost |
| `W` was installed after `t` (released and held again) | `unknown` (`released`) | as above |
| `x` is in `W.ops` | `admitted`, with its record | it took effect |
| a record in `W.ops` has `after == e` and `id ≠ x` | `superseded` | another operation replaced `e`; `(x, e)` can never be admitted |
| `W.op.id == e` | `open` | not admitted **yet**: a delayed copy may still be |
| otherwise (`e` is older than the window) | `unknown` (`expired`) | |

`GET /targets/{W}/ops/{x}?expect=e&instance=i&at=t` answers this. A client following
`W/op` and `W/ops` computes the same from its replica; the rule is implemented once in
`shared/` and once in the bridge, tested on the same fixtures. The `released` row compares `t` with
`W`'s `heldSince`.

Client rules (revision 3's C2, C3, made precise):

- `admitted`: done; its effect and result are in the tree.
- `superseded`: definitely never executed; the input is still the user's. A queued prompt
  may be submitted automatically as a **new** request with a new `id` and the current
  `expect`. This is the only outcome that allows a new identity.
- `open`: the client may only **repeat `(x, e)` unchanged**. The repeat is admitted now or
  is a no-op if a delayed copy got there first; either way §9.2 keeps one execution. While
  `e` still runs the repeat is `not_allowed`, which changes nothing; the client asks again
  after `e` ends. It never creates a new `id` from this outcome.
- `unknown`: left to the user, never resent automatically.

So: execution at most once holds per request, always; the outcome is findable while the
target lives, the bridge does not restart, and fewer than `K` further operations occupied
the target.

### 9.4 Lost action answers

Unchanged from revision 3 §7: an interaction answer may be repeated (`404` then means it
was delivered or the interaction ended; the tree shows which); controls show their value;
anything else is left to the user.

## 10. Scenarios

**S1. A device showing the latest output.**

```text
GET /api/v2/stream?follow=/views/S/output/latest&window=/views/S/output/latest:4096
                   &follow=/sessions/S/op&cwd=S:/work
```

The first event carries two bounded snapshots: the output view (a `TextWindow` under
`N + L = 8 KiB` of text plus a few fields) and the operation head (no input, no result).
Streamed text arrives as `append` on the window's `text`, or as a new window when a large
piece arrives at once; every event stays under `N + L` bytes of text. When the Agent
writes a new message after a tool call, or a new turn starts, the view changes as a whole.
The device verifies both paths at checkpoints by hashing one small object, one text chain
continued from `before`, and the head. After a break it resubscribes with `instance` and
both `have`s and receives differential snapshots of those two paths only. To send a
prompt it writes to `sessions/S` with `expect = op.id`. Following the view holds and, if
needed, loads the session (§7.2); nothing it receives grows with the history (R1, R2). A
device without a stream polls the same paths with `GET /nodes/…?have=`.

**S2. Desktop and device together.** The desktop follows `/sessions/S` without a window.
The view is a function of the same tree at the same `rev`; at equal `(instance, path
version)` every client holds the same content of a path, which their hashes confirm (R3).
They need not receive updates at the same moment; coalescing never changes what a
version contains.

**S3. Duplicates and retries.** A change that does not advance `v_p` is ignored, and a
gap is never applied (receive). A request repeated with its own identity executes at most
once (§9.2), and a new identity is only made after `superseded` (§9.3) (submit).

**S4. Conflicting writes.** Two clients write prompts to `sessions/S` with the same
`expect`. One is admitted; the other gets `stale`, keeps its input, and its outcome is
`superseded`: the winner's record in `ops` has `after == expect`.

**S5. Control during a prompt.** `cancel` is admitted against the running prompt's `id`;
an interaction answer is an action delivered at most once; both act at once. A device
that answers permissions also follows `/sessions/S/interactions`; their requests are ACP
values of the Agent's size, so a device that cannot hold them does not offer answers.

## 11. Costs

| Client | Holds | Per event | Verification |
| --- | --- | --- | --- |
| full replica (`/sessions/S`) | the session | the change | hashes along the changed path; the entries list hashes its children's hashes (`O(entries)` per checkpoint) |
| one turn (`/views/S/history/<i>/<h>/turn/<start>`) | that turn | changes in it | that subtree |
| one output with window `N` | `< N + L` bytes of text, a few fields | `< N + L` bytes of text | one value, one chain |
| paging a long text | one segment | — | the chain, one segment at a time |

Peak memory claims are to be measured, not assumed: plan step 8 runs a minimal reference
client through receive, parse, verify and install, and records its peak. What the reference
client counts is the UTF-8 bytes of window text held and carried by an event, a stand-in for
the peak: it does not count the parsed JSON of an event or the copy applying it makes, which
the `2 (N + L)` bound includes only as text. A device's real peak is measured on the device.

Implementation note (#216): a `fork` is installed in its source session's `op`, not in
`global`'s; the exclusion with `global` is enforced at admission, and only a `delete` is
recorded in two targets (§9.3 as written above says both). The specification, [protocol.md](protocol.md),
states what is implemented.

The bridge keeps per node `rev`, `srev` and a cached hash, per materialized view its value
(text shared with the entry where it is one block), and per target `K` small records.

## 12. Alternatives considered

- **Nested turns** (`turns/<k>/entries/<j>` as the canonical tree, as the first draft
  proposed). It makes one turn a canonical subtree and its list hash `O(entries in the
  turn)`. But entries then change paths whenever the partition changes, every such change
  rewrites whole turns for every follower, and the browser projection must change shape.
  Flat entries with an index keep entry paths stable, put partition changes into a small
  index, and give a verifiable turn through a view. Both satisfy R1–R3; flat is the
  smaller change and is chosen.
- **Views as extra scopes on top of revision 3** (each with its own epoch and `rev`).
  Every view repeats the version and recovery machinery, and versions of a view, its
  session and another view have no order. One tree with one version removes the problem.
- **Client-side filtering of whole scopes.** Fails R1.
- **Turn addressing by ordinal.** "The k-th turn now" silently becomes another turn after
  an earlier merge. Fixed views address by `start` and end when that turn goes away or
  the history is replaced; "the current turn" is the moving view. No permanent turn ID is
  invented: neither `start` nor `messageId` (often absent) survives a load in general, so
  a fixed selection is re-confirmed after one.
- **Immutable final messages.** Rejected by #201 and by the memory model.
- **A request ledger or server queue.** Not needed: §9.2 gives at-most-once per request;
  a bounded log gives a defined window for outcomes.
- **Long polling.** Plain polling with `have` meets R2; long polling can be added if
  measured latency or power require it.

## 13. Decisions and remaining questions

Decided in review (#201, PR #202):

1. `K = 64` and `L = 4096`. `K` bounds only how long an outcome can be found, never
   execution. `L` is a protocol constant; a device controls what it holds with `N`. The
   reference client measures the real peak (plan step 8).
2. The current turn is followed through `output/latest`; a fixed turn is addressed by its
   `start` entry and ends explicitly (`absent`, `replaced`).
3. `OutputView.text` is the concatenated text, unchanged, with entry, turn state and the
   number of other blocks.
4. Following a view holds and loads the session.
5. Plain `GET` polling with `have`; a `GET` observes the session (§7.2).

6. A running turn without an agent message has `entry: null`; a thought never stands in
   for the output.
7. No lightweight view of pending permissions for now; it is added when a device needs it
   (S5 notes the cost).

No question is open.

## 14. Plan

Bridge before browser, each step test-first ([testing method](bridge-state-machine-tdd.md)),
each its own PR into `experiment/protocol-v2`:

1. Tree and versions in the bridge: `(instance, rev)`, nodes, path revisions with the §4.7
   invariant, the turn index, the operation head split. Published wire unchanged.
2. Hashing (§5) in Rust and `shared/`, with cross-language vectors, including text chains
   continued from a 32-byte `before`.
3. Stream revision 4: path follows, path versions in snapshots and checks, per-path
   chains, commit events, differential snapshots, coalescing (§6). Served beside revision 3
   until step 7.
4. Text windows with bounded events, and segment reads (§6.4, §7.1).
5. Operation logs, the four outcomes and the resolution read (§9.3); targets.
6. Views, and holding by views and `GET` (§4.5, §7.2), request ordering across instances
   (§7.3).
7. Browser client on revision 4: a tree replica, its projection from `entries`, `prompts`
   and `turns`, removal of client turn derivation and of revision 3 serving.
8. A minimal reference client of S1 (no browser) in the integration tests, holding only the
   output view and the operation head, polling and streaming, with measured peak memory.

Acceptance cases to add to [protocol-acceptance](protocol-acceptance.md):

- each §10 scenario with its required observation;
- a snapshot of a path last changed long before the snapshot's `rev` gives `v_p = at`, and
  the next ops change chains from it;
- a turn partition change rewrites only the index, bumps the affected `TurnHead` paths,
  and ends a fixed view with `absent`; a load ends fixed views with `replaced`;
- a 1 MiB append to a windowed text arrives as one window of at most `N + L` bytes; the
  window verifies from its 32-byte `before`; a stale segment read is refused;
- a delayed request: the query answers `open` while `op.id == e`, the client repeats
  `(x, e)`, the delayed copy is then a no-op, and exactly one prompt runs; `superseded`
  is the only outcome after which a new request is made;
- every row of §9.3, including a lost `fork` answer resolved from its source session's
  log after another session operation, and a session released and held again (`released`);
- a late `GET` answered by an old instance after the client adopted a new one is not
  installed;
- a polling-only client opens an unheld session, keeps it held while polling, and it is
  released after the timeout once polling stops;
- the only change to a path is lost and the path goes quiet: the next checkpoint has
  `at > v_p` and the client recovers;
- a fixed view whose replica fails verification after a load is resubscribed without
  `have` and ends with `replaced`; it never shows the new history's turn;
- after a bridge restart, the new instance happens to have the same `historyRev` and a turn
  at the same `start`: following or reading the old fixed path, on a stream or by `GET`,
  ends with `replaced` and installs nothing from the new instance;
- an append of 12,000 bytes to a 1-byte text in a window of `N = 4096` arrives as one
  `set` of the window, not as an `append`;
- an Agent error of 1 MiB leaves `OpHead`, `OpRecord` and `End` bounded; the whole error
  is in `opDetail`, or in `/global/loadFailures` for a failed load;
- revision 3's §12.4 cases restated per path.

## 15. Changes from earlier drafts

Third to fourth draft, from the review of 804f3fa on PR #202:

| Third draft | Fourth draft | Review point |
| --- | --- | --- |
| fixed views bound to `historyRev` | bound to `(instance, historyRev, start)`, the instance part of the path; another instance ends them with `replaced` on a stream and for a `GET` | P2: a new instance counts revisions again and can reuse the same `historyRev` |
| `opDetail` shown as `{ input, result? }` in the tree | `{ input, result?, error? }`, as §4.4 says | wording |
| "a failed load leaves no session" | only a path that ends with `load_failed`; a failed reattachment that keeps the session keeps its error in `opDetail` | wording |

Second to third draft, from the review of af54130 on PR #202:

| Second draft | Third draft | Review point |
| --- | --- | --- |
| a checkpoint was compared only when `at == v_p` | `at > v_p` means a missed change and starts recovery; `at < v_p` is ignored; the 500 ms / 2 s schedule is restated in §6.5 | P1: losing the last change before a quiet period went unnoticed |
| a fixed view was bound to its history only while a `have` was given; resubscribing without `have` re-confirmed the selection | the binding is part of the path (`history/<h>/…`); a replaced history always ends it with `replaced`; dropping a replica never changes the selection | P2: recovery could silently switch the turn |
| an append was sent as `append` when the resulting window stayed small | the choice depends on the appended bytes alone: `a < N + L` is an `append`, anything larger a new window | P2: 1 byte + 12,000 appended exceeded the event budget |
| `OpHead.error` and `OpRecord.error` were the whole `ProtocolError` | an `ErrorSummary` (code, Agent integer code); the whole error in `opDetail`; `End.error` too, with failed loads kept in `/global/loadFailures` | P2: an Agent's 1 MiB error reached the device |

First to second draft, from the review of c543213 (moved from #201 to PR #202):

| First draft | Second draft | Review point |
| --- | --- | --- |
| `op.id == e` was `not_admitted` and allowed a new request | `open`: only `(x, e)` may be repeated; `superseded` alone allows a new identity | P1: a delayed copy could still be admitted, and a new ID would run the input twice |
| snapshots carried only the event `rev` | snapshots carry `at` (the path version), checks carry `at`, duplicates are judged by the resulting version | P1: a client could not chain from a snapshot of a path last changed long before |
| S1 followed `op` with the whole input | `op` head; input and result in `opDetail` | P2: the "small" second snapshot could hold a 1 MiB prompt |
| windows bounded only their final size | every event carries under `N + L` bytes of one window; large appends become a new window | P2: peak memory during an update |
| `before` was a truncated wire hash | chain states are 32 bytes on the wire | P2: a truncated state cannot be continued |
| a new instance could be adopted from any late `GET` | requests issued before adopting an instance lose their right to install | P2: a dead instance's late answer could displace a new one |
| outcomes limited by instance and `K` | also by the target's lifetime (`released`) | P2: releasing and reloading a session drops its log |
| nested turns, justified by verifying a list | flat entries and a turn index; comparison kept in §12 | review: nesting is not required for partial verification |
| turns by ordinal | fixed turns by `start`, ended explicitly; the current turn by the moving view | §13 recommendation 2 |
| a `GET` held nothing | a `GET` observes the session and may load it | §13 recommendation 5 |
