# Protocol revision 4: partial synchronization and serialized writes

Status: **proposed design, 2026-10-24, for review** (issue #201). Nothing here is
implemented. Revision 3 ([protocol.md](protocol.md)) stays the served protocol until
this design is accepted and its plan (§13) lands. Layer 1, the
[bridge memory model](bridge-memory-model.md), is unchanged: this revision only changes
how that memory is published (layer 2) and how clients write to it.

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
| Per-scope random epochs (§5.4) | versions of different scopes, or of one scope across a rebuild, have no order; `GET` ordering needs special rules (§8) |
| `after` names only the issuing scope's predecessor (§6.6) | a lost answer is unknown after two further operations, or when an operation of another scope replaced the `op` |

What revision 3 already does right and this revision keeps: client-chosen operation
IDs, admission against `expect`, actions for control input, content-addressed
attachments, differential snapshots by change revision, client-computed verification,
coalescing a slow subscriber into a snapshot.

One correction to #201: **submit idempotence already holds unconditionally** (§9.2). What
revision 3 lacks is a defined window in which the *outcome* can be found.

## 3. Overview

1. **One state tree, one version.** Everything the bridge publishes is one tree:
   `/global`, `/catalog`, `/sessions/<id>`, and derived `/views/…`. One commit of the
   bridge lock is one revision `rev`; the version of anything is `(instance, rev)`, with a
   random `instance` per bridge process. Epochs disappear.
2. **Path subscriptions.** A client follows any set of paths. For each path it holds the
   subtree and the version at which that subtree last changed; events carry, per path,
   the version they apply to, so a gap is detected per path.
3. **Path revisions.** Every node has the revision at which the value *at its path* last
   changed. Differential snapshots of any subtree follow from it.
4. **Content hashes per node.** A Merkle hash over the tree lets a client verify exactly
   the subtree it holds. Long text is hashed as a chain of fixed segments, so a client can
   hold, verify and page through a window of a message larger than its memory.
5. **Turns published by the bridge**, with their state and output, so no client derives
   them from the whole history. Selections that move (the latest turn, a turn's current
   output) are derived **views**.
6. **Writes** keep `id` + `expect`. Every operation is recorded, in each target it
   occupies, in a bounded log; a lost answer is resolved from it with three defined
   outcomes.

| Revision 3 | Revision 4 |
| --- | --- |
| scopes `global`, `catalog`, `session/<id>` as documents | one tree; those are its top-level paths; any path can be followed |
| `epoch` + `rev` per scope | `(instance, rev)` for the whole bridge; a session rebuild is a subtree replacement |
| `have=<scope>:<epoch>:<rev>` | `have=<path>:<rev>` with one `instance` per stream |
| document digest at snapshots and checkpoints | node hash of each followed path |
| flat `entries`, client-derived turns | `turns/<k>/entries/<j>`, derived by the bridge |
| — | `/views/<id>/latest/output`, `/views/<id>/turns/<k>/output` |
| — | text windows and segment reads for long text |
| `after` on the operation | per-target operation log (last `K`), resolution read |

## 4. The state tree

### 4.1 Version

- `instance`: 128 random bits (unpadded base64url), chosen when the bridge process starts.
  Agent restarts within the process keep it; nothing published survives a process restart.
- `rev`: an integer, starting at 1, increased by exactly one by every commit that changes
  anything published. One commit may change several paths (installing a session adds
  `/global/sessions/<id>` and `/sessions/<id>` in the same `rev`).
- Versions of one instance are totally ordered. Versions of different instances are not
  comparable: a client seeing a new instance drops everything it holds.

### 4.2 Paths and nodes

A path is a JSON Pointer into the tree (a session ID containing `/` or `~` is escaped as
RFC 6901 says). In query parameters of the form `<path>:<value>`, the value follows the
**last** `:`; in `cwd=<id>:<dir>` the ID is percent-encoded and ends at the **first** `:`.
The tree's shape is fixed by this document: some
positions are **nodes** (they have their own path revision and hash), the rest are plain
JSON inside a node.

| Kind | Content | Children |
| --- | --- | --- |
| value | any JSON | none (but may contain text positions, §4.4) |
| object | own fields (JSON) | named child nodes |
| map | the set of keys | one node per key |
| list | the length | one node per index |

Shape (`·` marks a node, indentation children):

```text
· /global                       object: { agent }                   (§4.3 of revision 3)
    · op                        value: Operation
    · ops                       list of OpRecord (§9.3)
    · sessions                  map sessionId → value SessionSummary
    · terminals                 map → value LoginTerminal (output is a text position)
    · interactions              map → value Interaction
    · urlFlows                  map → value UrlFlow
· /catalog                      object: { revision }
    · op, · ops, · interactions, · urlFlows   as in /global
    · pages                     list of value Page
· /sessions/<id>                object: { sessionId, cwd, history, closing }
    · controls                  value: SessionControls
    · op, · ops, · interactions, · urlFlows
    · terminals                 map → value Terminal (output is a text position)
    · turns                     list (absent while history is unknown), §4.3
        · <k>                   object: TurnHead
            · entries           list of value Entry
· /views/<id>/latest/output     value: OutputView (§4.5)
· /views/<id>/turns/<k>/output  value: OutputView
```

Entries, prompts, controls, operations, interactions, terminals and attachments keep
their revision 3 wire types; only their place in the tree changes.

### 4.3 Turns

The bridge publishes the turns that memory model §5 defines, instead of the flat entry
list:

```ts
interface TurnHead {
  start: number;             // absolute index of the turn's first entry in layer-1 entries
  messageId?: string;        // the opening user message's messageId, if any
  prompts: Prompt[];         // revision 3 Prompt, for the prompts whose entry is in this turn
  state: "running" | "ended";
  output: number | null;     // index in `entries` of the turn's last agent message entry
}
```

- `state` is `running` exactly while one of `prompts` has no `end`. Anything else is
  `ended`, including turns of a loaded history (memory model §6.2 clears prompts).
- **Revisable.** `ended` does not freeze a turn: a late tool update (memory model §3.2
  rule 1) or an idle update (D5) can still change its entries, and its `output` can change.
  Such a change is a change like any other: its paths get new revisions and followers are
  told.
- **Partition changes.** The turn partition is a function of the entries. It can change
  after the fact: a user entry without `messageId` that later takes the opener's ID by a
  joining chunk stops starting a turn. A history replacement (load) replaces all turns.
  The bridge then republishes the affected turns; nothing requires turns to be
  append-only (§4.6).
- `turns/<k>` is addressed by ordinal. It names "the k-th turn now", not a fixed piece of
  content; `start` and `messageId` let a client notice that the turn at `k` is a different
  one after a partition change or a load.

The flat entry index stays meaningful (`start + j`) for prompts, attachments and export.
Revision 3's client-side turn derivation is removed.

### 4.4 Text positions

These string positions are **text positions**:

- the `text` of every ContentBlock of type `text` inside an `Entry` (message chunks,
  tool call content, compaction summaries);
- a terminal's `output`;
- an `OutputView`'s `text`.

A text position is a string in the JSON, like any other. It differs in how it is hashed
(§5) and in that a subscription may hold only a window of it (§6.4). Segmentation is a
pure function of the string: the UTF-8 bytes are split greedily from the start into
segments of at most `L = 4096` bytes, each ending on a code point boundary. Appending
changes only the last segment and adds new ones; earlier segments never change.

Other large values (a tool's `rawInput`/`rawOutput`, plan bodies) are not segmented. A
small client controls their cost by not following the paths that contain them, which the
tree makes possible.

### 4.5 Views

A view is a node computed by the bridge from the tree at the same `rev`. It is read-only
and never contains anything the canonical tree does not: it exists so that a selection
that **moves** can be followed at one fixed path, without the client re-subscribing (and
racing) each time the target changes.

```ts
interface OutputView {
  turn: { k: number; start: number; messageId?: string; state: "running" | "ended" } | null;
  entry: number | null;      // absolute entry index of the output message, null when none yet
  text: string;              // the text blocks of that message, concatenated in order (a text position)
  other: number;             // how many non-text blocks the message also has (not included)
}
```

- `/views/<id>/latest/output`: the output of the last turn; it moves to the next turn when
  one starts and to the next agent message when the Agent writes one after a tool call.
- `/views/<id>/turns/<k>/output`: the output of turn `k`.

A view is materialized while it is followed (or read). Its path revision is the revision
at which its value last changed, or at which it was materialized, whichever is later; a
conservative revision only costs a fuller snapshot, never a wrong one (§4.6). Following a
view holds its session (B6), like following `/sessions/<id>`.

### 4.6 The path revision invariant

> For every node `N`: `rev(N)` is the revision at which the value at `N`'s path last
> changed, and `srev(N)` is the largest `rev` in `N`'s subtree. If `srev(N) ≤ r`, the
> value at `N`'s path is the same now as at `r`.

The invariant is about **paths**, not objects: when a turn partition change puts a
different entry at `turns/3/entries/0`, that path changed even if no entry did. The
bridge maintains it by comparing, at each commit, the new value at each affected path with
the old (structural sharing makes unchanged subtrees pointer-equal; otherwise hashes
compare). A revision may be later than the actual change (views, §4.5) but never earlier.

Everything about synchronization follows from it: differential snapshots (§6.3), the
per-path version a client holds (§6.2), and R3 (§10).

## 5. Hashing

`H` is SHA-256; on the wire a hash is the unpadded base64url of its first 16 bytes;
internally the full 32 bytes are used. `canon` is JCS (RFC 8785), with revision 3's
double-domain caveat (§5.5 there).

- **Text** (a text position) with segments `s_0 … s_{m-1}` and `len` UTF-8 bytes:
  `c_{-1} = 0^32`, `c_i = H(c_{i-1} ‖ s_i)`, `h = H("t" ‖ u64be(len) ‖ c_{m-1})`.
  `c_i` is the **chain** after segment `i`.
- **Value:** `h = H("v" ‖ canon(value'))`, where `value'` is the value with each text
  position replaced by the string `"#" + b64(h(text))`.
- **Object:** `h = H("o" ‖ canon(own') ‖ ∥_{name ascending} (canon(name) ‖ h(child)))`.
- **Map:** `h = H("m" ‖ ∥_{key ascending} (canon(key) ‖ h(child)))`.
- **List:** `h = H("l" ‖ u64be(n) ‖ h(child_0) ‖ … ‖ h(child_{n-1}))`.

Names and keys are ordered as JCS orders object keys (UTF-16 code units).

Properties:

- The hash of a path depends only on its content, not on any version: equal hashes mean
  equal content, also across revisions.
- Verifying a subtree needs only that subtree. A client verifying one output view hashes
  one small object and one text chain.
- An append to a streamed message rehashes one segment and the path up to the root of the
  followed subtree, not the whole message.
- Both sides cache hashes by content identity, as revision 3 §5.5 requires (frozen
  replica values; a changed value is a new value and a cache miss).

## 6. Stream

### 6.1 Subscribing

```text
GET /api/v2/stream?instance=<i>&follow=<path>&have=<path>:<rev>&window=<path>:<bytes>&cwd=<id>:<dir>
```

| Parameter | Meaning |
| --- | --- |
| `follow` | a path to follow; repeatable. Paths may not overlap (one may not contain another). |
| `instance` | the instance the client's holdings belong to; omitted when it holds nothing |
| `have` | per followed path: the revision of the subtree it holds and has applied without error, or verified; omitted after a failed verification |
| `window` | per followed path: hold text positions in it as windows of at least `bytes` (§6.4); without it, whole strings |
| `cwd` | the working directory to load an unheld session with (B6) |

One stream carries any set of paths. To change the set the client opens a new stream and
ignores the old one (as revision 3). Following any path under `/sessions/<id>` or
`/views/<id>` holds session `<id>`; an unheld session is loaded as revision 3 §5.1
describes, and its paths are absent until the load installs it.

### 6.2 Events

```ts
type Event =
  | { instance: string; rev: number; changes: Change[] }   // first event of a stream
  | { rev: number; changes: Change[] }                     // one commit
  | { rev: number; check: Array<[path: string, hash: string]> };   // checkpoint

type Change =
  | { path: string; snapshot: Snapshot }
  | { path: string; from: number; ops: Op[] }              // ops relative to `path`
  | { path: string; end: { reason: "deleted" | "closed" | "load_failed" | "absent"; error?: ProtocolError } };
```

- A **commit event** carries, for each followed path the commit changed, one change. A
  path the commit did not touch is not mentioned. All changes of one commit arrive in one
  event: a client applies them together, so everything it follows is consistent at
  `rev` (cross-path atomicity, which revision 3 lacked).
- **Per-path chain.** The client holds, per followed path `p`, the version `v_p` of its
  subtree. An ops change carries `from = srev(p)` before the commit; the client applies it
  only if `from == v_p`, then sets `v_p = rev`. A change whose `rev ≤ v_p` is a duplicate
  and is ignored (**receive idempotence**). Any other mismatch, or an op that does not
  apply, is a gap: the client resubscribes with its `have`s.
- `Op` is revision 3's (§5.3), with pointers relative to the followed path.
- **`end`**: the path no longer exists (session deleted, closed, never loadable, or a path
  such as `turns/9` that is not there). The client drops what it holds there. For a
  session path the stream keeps following it: a later install or load starts with a
  snapshot again (`absent` ends are not final).

### 6.3 Snapshots

```ts
interface Snapshot {
  from: number | null;       // the `have` it builds on; null: replaces everything
  hash: string;              // of the subtree after installing it
  node: SNode;
}

type SNode =
  | { v: unknown }                                         // the whole subtree as JSON
  | { o: unknown; c: Record<string, SNode> }               // object: own fields, changed children
  | { m: string[]; c: Record<string, SNode> }              // map: all keys, changed children
  | { l: number; c: Record<string, SNode> };               // list: length, changed children
```

The bridge builds a differential snapshot of path `p` for `have = h` by walking the
subtree: a node with `srev ≤ h` is omitted (the client keeps its own); a value node with
`srev > h` is sent whole; a container with `srev > h` is sent as its structure (own
fields, all keys, or the length) plus its children with `srev > h`. The client installs
into a copy: it keeps every child the structure names and the snapshot omits, drops every
child the structure no longer names, and replaces the rest. By §4.6 a kept child is
exactly the bridge's current value. The copy is adopted only if its hash matches.

A full snapshot is `{ from: null, node: { v } }`. A differential snapshot needs no
knowledge of what the client held beyond `h`, since structure is always sent where
anything below changed.

### 6.4 Text windows

With `window=<p>:<N>`, every text position under `p` is held as a window instead of a
string:

```ts
interface TextWindow {
  length: number;            // UTF-8 bytes of the whole text
  offset: number;            // bytes before the window; a segment boundary
  before: string;            // chain c before the window's first segment (all zeros when offset = 0)
  text: string;              // segments from `offset` to the end: at least N bytes when the text has them
}
```

- It hashes to the same `h` as the whole string: `c_{-1}` is replaced by `before`.
- As text is appended, the bridge emits `append` on `…/text` and `set` on `…/length`; once
  `text` exceeds `N + L` bytes, a `trim` drops whole leading segments and `set`s `offset`
  and `before`. A client's memory for that text is bounded by about `N + 2L`.
- Replacing the text (a `set` of the message) resends the window.

### 6.5 Checkpoints, coalescing, keepalive

- **Checkpoint**: after the changes to a followed path pause for 500 ms, and at least
  every 2 s while they continue, the bridge sends `check` with the path's hash at `rev`.
  The client compares; on a mismatch it drops that path and resubscribes without its
  `have`. Snapshots carry their hash, so every installation is verified.
- **Slow subscriber**: when a stream's queue exceeds its budget, the bridge drops the
  queued events and queues one event at the current `rev` carrying, per followed path, a
  differential snapshot from the last `v_p` it actually wrote. Ends are kept.
- An SSE comment at least every 15 s on an idle stream.

## 7. Reads

| Read | Result |
| --- | --- |
| `GET /api/v2/nodes/{path}?instance&have&window` | `{ instance, rev, change }`: the same change a stream would send first; holds nothing |
| `GET /api/v2/nodes/{path}/segments/{i}?at=<rev>` | `{ rev, before: c_{i-1}, segment: s_i }` for the text position at `path`; `409 stale` when segment `i` changed after `at` |
| attachments, context search and read | as revision 3 §8 |

- **Ordering.** Within an instance, versions are ordered, so revision 3's request
  counters go away: a client installs a `GET` result for a path it does not follow only
  if it is newer than what it holds there. A followed path is installed from the stream
  only, as before.
- **Paging a long text.** A device reads segments `0, 1, …` with `at` set to the version
  it holds, chaining `c_i` as it goes, and checks the chain against `before` of its
  window or against the text's hash. It needs memory for one segment. Earlier segments of
  a streaming message do not change while the Agent appends, so paging works while the
  message is still growing.

## 8. Write targets

Operations and actions are written to a **target**: `global`, `catalog` or
`sessions/<id>`, each with its `op`, `ops`, `interactions` and `urlFlows` nodes.

```text
POST /api/v2/targets/{target}/ops                         { id, expect, kind, args }
POST /api/v2/targets/{target}/actions/{action}            { …args }
POST /api/v2/targets/{target}/resources/{id}/{action}     { …args }
GET  /api/v2/targets/{target}/ops/{id}?instance=<i>&expect=<e>
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

### 9.2 At most once: an unconditional guarantee

> A write with operation ID `x` and `expect = e` executes at most once, whatever is
> repeated, lost or restarted.

Admission requires `op.id == e` and makes `op.id = x`. IDs are 128 random bits, never
reused, so after admission the target's `op.id` is never `e` again. A repeat of the same
write therefore finds either `op.id == x` (answered `202`, no effect) or `op.id ≠ e`
(`stale`). After a bridge restart every target's `op` has a new bridge-allocated ID, so a
repeat is `stale`. No request table is needed. This is submit idempotence (S3).

Two submissions of the same text with different IDs are two requests. The second is
admitted only if its `expect` is still current, which it is not if the first was
admitted: the client must have seen the first one's effect (or its refusal) to write it.

### 9.3 Outcomes: a bounded operation log

```ts
interface OpRecord {
  id: string;
  after: string;             // this target's op.id it replaced
  kind: OperationKind;
  rev: number;               // the commit that admitted it
  status: OperationStatus;   // updated until it ends
  error?: ProtocolError;
}
```

Every target keeps `ops`: the records of the last `K = 64` operations that occupied it,
**including those written to another target** (a `fork` written to a session is
recorded in the session and in `global`, each with its own `after`). So each target's log
is the complete chain of its `op`s within the window, which revision 3's single `after`
was not.

The outcome of a write `(x, e)` to target `W`, written while the client held instance `i`:

| Condition | Outcome |
| --- | --- |
| instance is not `i` | `unknown` (`restarted`) |
| `x` is in `W.ops` | `admitted`, with its record |
| `W.op.id == e` | `not_admitted`: nothing replaced `e`, and by §9.2 nothing can admit `x` later with this `expect` |
| a record in `W.ops` has `after == e` and `id ≠ x` | `not_admitted`: that operation replaced `e` |
| otherwise (`e` is older than the window) | `unknown` (`expired`) |

`GET /targets/{W}/ops/{x}?instance=i&expect=e` answers this; a client that follows
`W/ops` and `W/op` can compute the same from its replica (the rule is implemented once in
`shared/` and once in the bridge, tested on the same fixtures).

Client rules (revision 3's C2, C3, made precise):

- `admitted`: the write took effect; its result is in the tree.
- `not_admitted`: the input is still the user's; a queued prompt may be submitted again
  automatically with a **new** `id` and the current `expect`.
- `unknown`: left to the user, never resent automatically.

The guarantee is therefore: *a lost answer is resolved while fewer than `K` further
operations occupied the target and the bridge did not restart*. Execution at most once
(§9.2) does not depend on that window.

### 9.4 Lost action answers

Unchanged from revision 3 §7: an interaction answer may be repeated (`404` then means it
was delivered or the interaction ended; the tree shows which); controls show their value;
anything else is left to the user.

## 10. Scenarios

**S1. A device showing the latest output.**

```text
GET /api/v2/stream?follow=/views/S/latest/output&window=/views/S/latest/output:2048
                   &follow=/sessions/S/op&cwd=S:/work
```

The first event carries two small snapshots: the output view (a `TextWindow` of at most
about 10 KiB) and the `op`. Streamed text arrives as `append` on the window's `text`;
when the Agent writes a new message after a tool call, or a new turn starts, the view
changes as a whole (`set`). The device verifies both paths at checkpoints by hashing one
small object and one chain. After a break it resubscribes with `instance` and both
`have`s and receives differential snapshots of those two paths only. To send a prompt it
writes to `sessions/S` with `expect = op.id`. Nothing it receives grows with the
session's history (R1, R2).

**S2. Desktop and device together.** The desktop follows `/sessions/S` without a window.
The device's view is a function of the same tree at the same `rev`; at equal
`(instance, v_p)` every client holds the same content of `p`, which their hashes confirm
(R3). They need not receive updates at the same moment, and a slow one may be coalesced;
coalescing never changes what a version contains.

**S3. Duplicates and retries.** A change with `rev ≤ v_p` is ignored, and a gap is never
applied (receive). A retried write executes at most once (§9.2); its outcome is found by
§9.3 (submit).

**S4. Conflicting writes.** Two clients write prompts to `sessions/S` with the same
`expect`. One is admitted; the other gets `stale`, keeps its input, and sees the winner's
record in `ops` with `after == expect`.

**S5. Control during a prompt.** `cancel` is admitted against the running prompt's `id`;
an interaction answer is an action delivered at most once; both act at once. A device
that answers permissions also follows `/sessions/S/interactions`, which holds only the
pending requests.

## 11. Costs

| Client | Holds | Per update | Verification |
| --- | --- | --- | --- |
| full replica (`/sessions/S`) | the session | the change | hashes along the changed path; list nodes hash their child hashes (`O(turns)` and `O(entries in a turn)` per checkpoint) |
| one turn (`/sessions/S/turns/k`) | that turn | changes in it | that subtree |
| one output with a window | `N + 2L` bytes of text, a few fields | appends, occasionally a whole view | one value, one chain |
| paging a long text | one segment | — | the chain, one segment at a time |

The bridge keeps per node `rev`, `srev` and a cached hash, and per followed view its
materialized value. Hashing is lazy: computed for checkpoints and snapshots of followed
paths, cached by content identity.

## 12. Alternatives considered

- **Views as extra scopes on top of revision 3** (each with its own epoch and `rev`).
  It works for one view, but every view repeats the version and recovery machinery, and
  versions of a view, its session and another view have no order, so switching selection
  or combining two selections needs new rules. One tree with one version removes the
  problem instead of handling it.
- **Client-side filtering of whole scopes.** Fails R1: the device still receives and
  verifies the session.
- **Flat entries plus an index of turns.** Keeps the flat list as the unit of
  differentials and hashing; a turn could not be verified without the whole list's hash.
- **Immutable final messages.** Rejected by #201 and by the memory model: tool updates and
  idle updates revise ended turns.
- **A request ledger or server queue.** Not needed: §9.2 gives at-most-once without one,
  and a bounded log gives a defined window for outcomes.
- **Segment trees over all large JSON.** More general than text chains, but tools'
  structured output is not what small clients show; it is avoided by not following it.

## 13. Open questions for review

1. `K = 64` and `L = 4096`: enough? `L` trades segment reads against per-append rehash
   size.
2. Turn addressing by ordinal (`turns/<k>`). Alternatives: by `start` (stable across
   later turns, but not across a load) or by `messageId` (absent for many Agents).
3. Should `OutputView` concatenate text (as proposed, simplest for devices) or carry the
   message entry (faithful, more work for devices)?
4. Should following a view of an unheld session start a load, as following the session
   does (proposed: yes), or should only `/sessions/<id>` hold?
5. A device that cannot hold SSE: is `GET /nodes/{path}?have=` polling enough (it returns
   a differential snapshot, or nothing changed), or is long-polling needed?

## 14. Plan

Bridge before browser, each step test-first ([testing method](bridge-state-machine-tdd.md)),
each its own PR into `experiment/protocol-v2`:

1. Tree and versions in the bridge: `(instance, rev)`, nodes, path revisions with the
   §4.6 invariant, turns derived in the bridge (memory model §5, against the existing
   fixtures). Published wire unchanged.
2. Hashing (§5) in Rust and `shared/`, with cross-language vectors, including text chains
   and windows.
3. Stream revision 4: path follows, per-path chains, commit events, differential
   snapshots, checkpoints, coalescing (§6). Served beside revision 3 until step 7.
4. Text windows and segment reads (§6.4, §7).
5. Operation logs and the resolution read (§9.3); the write paths renamed to targets.
6. Views (§4.5).
7. Browser client on revision 4: a tree replica, turn projection from `turns`, removal of
   client turn derivation and of revision 3 serving.
8. A minimal reference client of S1 (no browser) in the integration tests, holding only
   the output view, to prove R1–R2 with measured bounds.

Acceptance cases to add to [protocol-acceptance](protocol-acceptance.md) when accepted:
the §10 scenarios, each with its required observation; a path whose content changes
without any object changing (turn partition change, load) yields a new revision and a
correct differential snapshot; a window verifies against the whole text's hash; a stale
segment read is refused; every row of §9.3, including a `fork` lost while another
operation replaced `global`'s `op`; and revision 3's §12.4 cases restated per path.
