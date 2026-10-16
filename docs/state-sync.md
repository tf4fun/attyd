# Sync protocol implementation plan

Status: revision 1 implemented, steps 1–9, 2026-10-15 (§5). Revision 2 of the
protocol (#51) is planned in §7 and **not implemented**; where this document
describes `canon`, the commit of a turn, `op.process` or I8, it describes revision 1.
This document is how the bridge implements
[attyd protocol v2](protocol.md), settled in
[client-api-redesign.md](client-api-redesign.md). It covers the internal design,
the invariants tests enforce, and the migration from the earlier v2 draft and
`/api/v1`.
Lifecycle and concurrency changes follow the
[TDD ledger](bridge-state-machine-tdd.md): bridge behavior is tested before browser
projections.

## 1. Starting point

This section records where the migration started, on 2026-10-14. An earlier v2 draft was implemented on `main` (its migration steps 1–5, last at
`11d04d0`) until step 2 removed it. It was never completed: no client used it, and
the web UI then ran on `/api/v1`. It was also
over-designed for the settled model (incarnation owner tags, epochs and log
positions, a retained log with resume, request records, intent tables, process
pages). It is therefore **removed first** (§5, step 2), not kept beside the new
protocol, which takes its place under `/api/v2`.

Three generic parts implement rules the new protocol keeps unchanged. Step 2 keeps
them, with their tests:

| Part | Path | Rule it implements |
| --- | --- | --- |
| Control reduction | `src/protocol/controls.rs` | protocol §4.5 |
| JSON Pointer patcher and generic diff | `src/protocol/diff.rs` | protocol §5.3 |
| DTO-to-TypeScript generation | `src/protocol/typescript.rs` | §3 |

Two more techniques of the draft were reused in the new code, while the draft's own
modules went with it in step 2: the identity diff of turn updates (now in
`src/protocol/publish.rs`, for `op.process` in §2.3) and byte-budgeted subscriber
queues (now in `src/protocol/hub.rs`, §2.4). Nothing else of the draft was a
constraint: its routes, documents, records, intents, log, owner handling and tests
went.

Layer 1 (bridge ↔ ACP server) was not changed by this plan until its last step
(9): until then the bridge kept its sessions, incarnations, ingress and
coordinator internally, and the protocol never exposed them. Step 9 removed the
session incarnation (§5); the bridge now identifies a session by its ID only.

## 2. Design

```text
ACP ──▶ bridge actor: transition ──▶ commit step ──────────────▶ Hub ──▶ replicas
                                       │ log: canon, hash chain     │ cut, queues,
                                       │ live: project → diff → ops │ coalescing
                                       ▼                            │
                              published documents (per scope)  ◀────┘ alignment reads them
```

### 2.1 Bridge state added

- **Operation slots.** Every scope has an `OpSlot { id, kind, status, input,
  cancel_requested, result, error, attachments, completion }` in bridge state
  (`src/protocol/op.rs`); `attachments` are the blocks a prompt input references,
  and `completion` is the held response of a `create`, `fork` or `list`. A held session keeps it in
  its registry entry; `global` and `catalog` keep theirs in the registry. The
  operation's pending interactions are the scope's pending Agent requests, which
  the bridge keeps as today; the projection reads them from there.
- **Occupation.** An `occupied: HashMap<Scope, OpId>` records scopes an operation
  occupies besides its own (delete occupies `session/<id>`). A held occupied scope
  shows the occupying operation as its `op`.
- **Turn log.** A held session keeps `TurnLog { entries: Vec<Arc<LogEntry>>,
  attachments }`, appended by commits and rebuilt from a load's replay (§2.3).
- **Holding need.** A session's need is computed, not stored: subscribers, occupation,
  or live resources (protocol S6).

### 2.2 Projection

Pure functions define the published documents:

- `project_session(&SessionRegistry, session_id: &str) -> Option<SessionDocument>`
- `project_global(&SessionRegistry, &AgentInfo) -> GlobalLive`
- `project_catalog(&SessionRegistry, revision: u64) -> CatalogLive`

`SessionDocument` hands the commit step what it publishes separately: the `log`
(`Unknown` until the first load completes, else `Known`), the `live` document, the
running prompt's `process`, and the attachments its `op.input` references. They
live in `src/protocol/project.rs` and are the only definition of what clients
see. `op.process` comes from the running turn's `SharedTurnUpdates`; it is absent
once the turn committed, so released process content never enters a document.

### 2.3 Commit step

Every transition in the bridge actor ends with one commit step, run when the bridge
lock's mutable guard is released (as in the earlier draft):

1. **Log.** For each held session, compare its `TurnLog` with the published one.
   - unchanged (the same `Arc`): nothing;
   - appended: a `LogChange { from: old length, prev, entries: new tail }`;
   - rebuilt (a load replaced it): `from` is the longest common prefix of the two
     chains, found by comparing hashes.
2. **Live.** Project, then diff against the published `live` with the identity
   diff (pointer comparison for `op.process`, value comparison for small fields,
   `trim` plus `append` for terminal tails).
3. **Publish.** Emit one event per changed scope. A log change and the same scope's
   live patch are one event, so a commit carries both (protocol S2). Store the new
   published documents and hand the events to the Hub under the same lock.

**`canon` and hashing** live in `src/protocol/canon.rs`:

- `reduce_messages(&[SessionUpdate]) -> Vec<Message>` applies protocol §3.2 step 1;
- `turns(messages) -> Vec<TurnContent>` applies step 2;
- `normalize(&[ContentBlock], &mut Attachments) -> Vec<Block>` applies step 3,
  where `Block` is the protocol's normalized content (text, resource links and
  `AttachmentRef`), a bridge DTO rather than an ACP type; it keeps each referenced
  block in the attachment table;
- `resolve(&[PromptBlock], &Attachments) -> Result<Vec<ContentBlock>, UnknownAttachment>`
  turns a prompt's references back into the ACP blocks they name, before
  `session/prompt`;
- `chain(&ChainHash, &Turn) -> ChainHash` hashes the JCS serialization
  (`canonical_json`).

`TurnLog::commit(prompt, updates)` builds a live turn from the prompt sent in
`session/prompt` and the Agent messages among the turn's updates;
`TurnLog::from_replay(updates)` builds every turn from a replay. Both go through
the same functions, so the same messages give the same entry (acceptance 1).

Attachment references in entries point at blocks the session keeps in an attachment
table keyed by ID, counted by references from the log and from `live`. A block whose
count reaches zero is released (protocol §8).

### 2.4 Hub

The Hub owns delivery only:

- **Cut.** A subscription is registered under the bridge lock. For each scope the
  Hub builds the alignment event from the published documents: the log from the
  client's head if `have` matches, else from `0`, with `live: { replace }`. Events
  of later commits enter the subscriber's queue in the same critical section that
  produced them, so nothing falls between the cut and the stream (protocol S3).
- **Baseline and queued tail.** For each session scope the Hub keeps two log
  positions per subscription:
  - the **baseline**: the head the client declared, advanced to an event's target
    log position only when that event is taken from the replaceable queue and
    handed to the stream's ordered output. A change still in the queue never
    advances it, since coalescing may drop it;
  - the **queued tail**: the target log position of the last event queued for the
    scope, or the baseline when nothing is queued.

  An ordinary log change is computed when it is queued, from the queued tail, so
  each queued change follows the one before it; the queue is never pre-built from
  a baseline that has not advanced yet. A resync is computed from the baseline. No
  client acknowledgement, sequence number or retained log is needed; the
  per-event convergence gate (§4.1) checks both positions.
- **Coalescing.** A queue past its byte budget (1 MiB, as in the earlier draft) is
  collapsed per scope: queued events of a scope that still exists are replaced by
  one alignment from the baseline, whose target becomes the queued tail; a queued
  `end` is kept, and nothing of its scope follows it.
- **Heartbeats.** An SSE comment on a stream idle for 15 seconds.
- **Holding.** A subscription to a session scope counts as one subscriber of that
  session while the stream is open. Subscribing to an unheld session submits a
  `load` to admission (§2.5); the stream waits for it under occupation.

- **Across connections.** The published documents and the Hub belong to the host
  and outlive one Agent connection, so a stream stays open across it. While the
  Agent initializes, a new subscription is aligned from the published cut and its
  loads wait for readiness. One cancellable recovery task starts the next
  connection after the previous one's final commit, when an open stream follows a
  session scope that has not ended, or once after a successful terminal login. It
  decides the need again when it starts and backs off while attempts never become
  ready. A global or catalog stream alone, and a read, start no connection.

No log is retained: a client that reconnects is aligned from the published
documents.

### 2.5 Operations

Operation writes are handled in the bridge actor, one at a time:

1. **Admission** (protocol §6.4), in one critical section: compare `expect` with the
   scope's `OpSlot`, check its status and the occupied scopes, then the capability,
   arguments and Agent connection.
2. **Install.** A starting operation replaces the scope's slot and occupies its
   other scopes, in the same critical section. The commit step publishes it.
3. **Run.** `bridge::operations` turns the operation into the bridge command of its kind,
   which the coordinator runs over ACP. Interactions
   of its ACP requests are added to its slot; answers are delivered by the existing
   interaction delivery path.
4. **End.** The transition that ends it sets its final status, result or error,
   frees its occupation, and wakes waiting subscriptions and loads.

`create`, `fork` and `list` keep their HTTP request open on a completion channel
that the ending transition fulfils. A dropped connection only drops the channel.

**Agent requests.** The coordinator routes each Agent request to a scope as today:
by session, or for a request-scoped elicitation by the JSON-RPC ID it names
(`ElicitationRequestScope.request_id`), resolved in wire order. The request joins
the scope's running operation, its own or the one occupying it, or starts an
`agent_request` operation when none runs.

**Cancel.** A cancel is admitted only for `prompt`, `terminal_login` and
`agent_request`. It sets `cancel_requested` and starts the kind's cancellation;
the operation keeps its occupation until the transition that actually ends it.

**Controls.** A successful `set_mode` or `set_config` folds its acknowledgement
into `controls` in the transition that ends the operation, as the bridge does today
(`complete_control_operation`): the requested mode, or the response's
`configOptions`. A failure folds nothing.

**Loads.** A subscription to an unheld session is admitted like a write without
`expect`: it waits while the session is occupied, then installs a running `load`
with the given `cwd`. Until the first load completes, the session's `TurnLog` is
`Unknown`, distinct from empty: alignments carry `live` only and leave each
subscriber's baseline where the client declared it. The load reuses the existing
`session/load` / `session/resume` attachment path and ends by building the
`TurnLog` from the replay; the transition publishes, per subscriber, the log from
its baseline together with the patch that ends the load. A reload of a held session
(after an Agent restart) keeps its known log until the replay replaces it.

**Where step 6 lands.** `POST /api/v2/scopes/{scope}/ops` (`server::write_scope`)
reads the body field by field and sends it to the actor as one input.
`protocol::admission` checks it on the published documents in admission order;
`bridge::operations` installs a starting operation and builds the command of its
kind under the same guard, or applies `cancel` and `answer` there. An `answer` is
checked whole before anything is delivered. Each interaction is found by its ID
wherever it waits, and is resolved and delivered to its pending responder in the
same guard (`resolve_permission`, `resolve_elicitation`). An accepted URL
elicitation becomes a flow in that step. A cancel sets `cancel_requested` once on the owning slot. A prompt or login
whose ACP call has not started yet sees the flag when it starts.

**Release.** A session whose need ends starts the existing retirement timer
(`--session-unobserved-timeout`), now only a performance setting. When it fires and
the session is still not needed, the bridge closes it only if the Agent advertises
`session/load` or `session/resume`. The timer is cancelled by any new need.

### 2.6 Resources

Session terminals and URL flows keep their current bridge representation, keyed by
terminal ID and interaction ID. A login terminal keeps its `methodId` with the
terminal itself, not on the operation that started it. They leave `live` as protocol §4.6 states;
acknowledgement records and terminal statuses go. The terminal login keeps the auth
terminal manager and its ordered events; its operation ends with the process exit,
and its terminal stays until dismissed. Terminal input is written as received, in
order; the bridge does not deduplicate it.

Step 6 lands the resource actions (protocol §7) in `bridge::resources`:
- The resource is found by the full scope and resource ID. It is checked under
  one guard, without `expect` and without an operation.
- A URL flow is named by its interaction ID (the flow's registration). Dismissing
  it reuses the settle and route primitives that end a flow with its scope, so a
  late dismissal of an old flow never reaches a new flow that reuses its
  `elicitationId`.
- A login terminal's dismissal is allowed only after the terminal exits, and it
  leaves the ended operation as it is.
- Input and resize run after the guard is released. Input goes on the terminal's
  own blocking writer, in the order the actor receives it. A stdin under
  backpressure therefore never holds the bridge lock that the terminal's output
  observer takes.

The session reads (protocol §8) are `server::read_session`:
- An attachment comes from the published cut (`Publisher::attachment`).
- The context search and read run outside the actor, in the workspace the actor
  returns for the held session's own `cwd`, with the existing workspace checks.
- A read holds, loads and renews nothing.

## 3. Types

The documents, events, ops, operation requests and results, and errors are typed
Rust DTOs. TypeScript in `shared/protocol.ts` is generated from them, and a test
fails when the generated output is stale. The HTTP bodies of writes, resource
actions, reads and errors are in `src/protocol/wire.rs`. The bridge's own
admission and delivery types are internal. ACP types are referenced from
`@agentclientprotocol/sdk`, not regenerated. The earlier draft's types are deleted in step 2.

## 4. Invariants and gates

| Invariant | Statement |
| --- | --- |
| I1 single write path | only the commit step publishes documents and events |
| I2 per-event consistency | after every event, a replica's `live` equals a published `live`, and its log equals the published log, or its own unconfirmed log while the bridge's is unknown |
| I3 exact cut | an alignment plus the events after it reproduce the published documents, with no gap and no duplicate |
| I4 coalescing safety | replacing any queued suffix by an alignment gives the same final document; `end` survives |
| I5 identity | the same messages give the same entries and hashes, live or replayed, however chunked. Revision 2: the same updates give the same items however chunked, and entries that differ in any published field have different hashes |
| I6 admission | writes and loads are serial; `expect`, status and occupation decide; IDs never repeat |
| I7 holding | a needed session is held; only a loadable session is released |
| I8 privacy | a committed turn's process never appears in a later document or event. **Withdrawn by revision 2** (S8): it encoded the removal of content; it is replaced by I9 |
| I9 fidelity (revision 2) | every accepted timeline update is reflected in the log or the tail by its reduction (protocol §3.2), and only a reduction or a replacing load removes content |

### 4.1 Convergence gate (I1–I4, I8)

A Rust harness drives the bridge with a scripted Agent. Three replicas apply events
**one at a time** and are compared, after each event, against the sequence of
published documents:

- one follows every event;
- one lags and has random suffixes of its queue coalesced (I4);
- one disconnects at random cuts and subscribes again with its head (I3);
- one pauses consumption, has a queued but unsent log change coalesced, and
  resumes; it must apply the alignment directly, without subscribing again.

Scenarios run on the session registry (control updates, streamed and folded turns,
commits, reloads with equal and diverging replays, terminals rolling past their
bound, URL flows) and through the real coordinator and ACP transport (prompts,
permissions, a turn started by another client, delete, release, Agent restart). The
harness also mutates state without committing and asserts that it reports the
divergence.

### 4.2 Acceptance

The acceptance cases of
[client-api-redesign.md §11](client-api-redesign.md#11-acceptance) (1–18) are the
acceptance suite. Each lands, as a bridge test, in the step that makes it
reachable (§5), and again in the client engine where it has a client half (3, 4, 9,
15, 16). The [coverage ledger](acp-coverage.md) replaces the earlier draft's chains with one per
capability:

```text
ACP capability → bridge authority → document path or operation → result in op
               → alignment of a fresh replica → lifecycle end
```

## 5. Migration

The web UI stayed on `/api/v1` until it moved to v2 in one step (7), so the
product kept working throughout. Each step is one PR, validated in CI. Steps 1–8 are
done: step 8 (PR #46) removed the v1 routes and SSE, the business events and
journal, the browser's v1 store and compensation, and drove the backend smokes over
v2. Step 9 (PR #47) is done, below.

| Step | Work | Acceptance |
| --- | --- | --- |
| 1 | The redesign settled, the protocol and this plan (docs only) | — |
| 2 | Remove the earlier v2 draft: `src/server/v2.rs` and its routes, the records and intent table, the replication log and Hub, the commit step, owner-qualified subscriptions, the projections and their tests, and the generated `shared/protocol.ts` types; `controls.rs`, `diff.rs` and `typescript.rs` stay (§1); v1 unchanged | v1 suites still pass |
| 3 | `canon.rs`: message reduction, turns, normalization, prompt resolution, hash chain; `TurnLog` built from live commits and from replays | 1 |
| 4a | DTOs and generated TypeScript; operation slots and occupation in bridge state; projections and the commit step for `log` and `live`, driven by the session registry's transitions; the per-event convergence gate on the session registry | 2, 5, 13, 18 |
| 4b | The commit step in the bridge actor, run when the bridge lock's mutable guard is released; the bridge's remaining transitions (global and catalog operations, login terminals, the Agent connection); the gate through the real coordinator and ACP transport | — |
| 5 | `GET /api/v2/stream` and `GET /api/v2/scopes/{scope}`: Hub cut, alignment, coalescing, `end`; hold by subscription and need, `load` on subscription, release of loadable sessions only; the publication shared across connections and their automatic recovery | 3, 4, 12 |
| 6 | `POST /api/v2/scopes/{scope}/ops`: admission, all kinds, `create`/`fork`/`list` responses, occupation, `agent_request`, the `catalog` scope; resource actions | 6–11, 14, 16, 17 (bridge) |
| 7 | Web UI on v2: the generic replica engine (alignment, log truncation and append, `prev` checks, live patches) outside React, and the projection. PR #34, built on the earlier draft, is not landed; `shared/replica.ts` and `shared/protocol-validate.ts` are carried over where they fit | 3, 4, 9, 15, 16 (client) |
| 8 | Remove v1: routes, business events, browser compensating state; Markdown export and attachment links on v2 | — |
| 9 | Layer 1 clean-up: remove incarnation-based isolation from the bridge internals | existing gates |

Step 9 removed the session incarnation entirely, without a renamed successor
(no session generation, owner token or tombstone). That covered:

- its allocation and storage: `SessionRegistry::next_incarnation` and
  `SessionState.incarnation`;
- its use in resource addressing: `SessionResourceOwner`, the permission,
  elicitation and URL routes, terminals, the history-sync owner, and the unused
  v1 attachment reference;
- its use in the ingress, the per-session FIFO (`SessionHandle` with its
  registration allocation), the coordinator, RPC completion hand-off, and
  load/replay continuations;
- every `StaleIncarnation` check and error, and the tests that only simulate an
  old incarnation number.

It also removed three mechanisms that only served that identity:

- the per-registration allocation of a session queue (`SessionHandle`'s
  `allocation`, `next_allocation` and `StaleHandle`): registering an existing
  queue returns it, with its queued events and busy delivery;
- the retired-ID filter (`SessionRegistry.retired_ids`, `is_retired`) for late
  notifications of a session the connection had once retired;
- the pointer identity of a session update's validation owner
  (`SessionUpdateOwner` and its `Arc<()>` allocation): an ordinary
  `session/update` is validated against the current session's validation or the
  creation staging under one lock.

What stays is ordinary ACP: JSON-RPC request/response correlation, the exact end
of the current op, cancellation and shutdown draining, one load's candidate
committed atomically or rolled back, and each resource's own ID and responder
(permission and elicitation requests, URL registrations, terminals). The bridge
generation (one Agent connection, its `epoch`) and an outstanding RPC's own
allocation also stay. A session is identified by its ID, as in ACP.

The result, in the code:

- A load's history candidate is keyed by session ID; every append, commit and
  discard still checks the load's attempt ID. A replay validation backup records
  the actual request (`request_id` of an explicit load, `attempt_id` of an
  automatic sync), and only that request commits or rolls it back.
- A session installed under an ID that names an idle entry replaces it: the old
  entry's resources are cancelled and removed (`retire_entry`) and its history is
  cleared.
- A delete takes the session path when the entry has a running session, and the
  catalog path otherwise (an unopened history row or a cold placeholder).
- Agent input naming a cold placeholder (an entry without a running session) takes
  the connection-level path, so a `session/update` before the creation response
  is staged and replayed into the created session.
- A `session/update` naming no running session is dropped, unless a creation is
  pending: then it is staged, bounded by the number of pending creations, and only
  the created session's ID is replayed into it. ACP notifications carry only a
  `sessionId`, so the bridge does not tell a late notification of a session
  replaced under the same ID from one of the current session; it applies to the
  session that runs under that ID.

The browser protocol (`/api/v2`) did not change.

Dependencies: 1 → 2 → 3 → 4a → 4b → 5 → 6 → 7 → 8, and 9 after 8. #16 and #17 are fixed
visibly in step 7.

## 6. Open questions

- **Release timer.** Step 4 keeps `--session-unobserved-timeout` as the linger before
  release. Whether its default should change now that it affects performance only is
  decided with measurements.
- **Several sample points.** Alignment matches the head only (redesign §12.3). If
  long histories make full alignment costly, `have` can carry several points; the
  Hub then picks the longest match.

## 7. Fidelity migration (revision 2)

Status: **planned, not started.** [Protocol revision 2](protocol.md) changes what the
documents carry, not how they are replicated: the commit step, the Hub, alignment,
coalescing, admission and holding stay. The [audit](acp-fidelity-audit.md) lists the
defects (D1–D11) and the [release parity matrix](release-parity.md) the v0.2.8
regressions (R1–R4) each step removes; every regression step is required. Each step is one PR, validated in CI, and lands
with its acceptance cases ([redesign §11](client-api-redesign.md#11-acceptance)). Tests
that assert revision 1's removals (`turn-collapse.pw.ts`, `lazy-process.pw.ts`, the
release case in `acp-ui.pw.ts`, `terminals_roll_their_bounded_tail_with_trim_and_append_and_leave_on_release`,
the I8 assertions) change in the step that changes the rule they encode, to assert
the retained content instead; no other test is weakened.

### 7.1 Design

- **One reducer.** `canon` becomes the timeline reducer of protocol §3.2: it takes
  session updates and gives items, and the same function serves the tail, a settle
  point and a replay. The active-turn fold (`fold_shared_active_turn_update`) and the
  replay builder (`history_replay::HistoryBuilder`) already fold tool calls, plans and
  text chunks; they become this reducer, implementing the three rules of protocol
  §3.2 (identity, patch, fragments): message items by role and `messageId`, one item
  per `toolCallId`, `planId` and `compactionId` in the session, `ToolCall::update` for
  tool patches, the compaction's three-state patch, and chunks with their own `_meta`.
  An index from entity ID to its entry (or the tail) makes a late update cheap to
  place.
- **Tail instead of process.** The running prompt's `SharedTurnUpdates` becomes the
  session's tail, which also exists between prompts. `project_session` projects it as
  `live.tail`; the identity diff that served `op.process` serves it.
- **Settle points in the commit step.** The transitions that end a prompt, admit a
  prompt, end the connection, complete a load, or place an update in a settled entry
  change the `TurnLog`; the commit step already turns an appended or rebuilt
  `TurnLog` into a `LogChange` from the first changed index (§2.3). A rewritten entry
  is a rebuilt log whose common prefix ends before it.
- **Attachments.** The table keeps blocks as received, keyed by the hash of the whole
  block; reference counting from the log and `live` is unchanged.
- **Interactions.** Pending permissions and elicitations move from the operation slot
  to the scope's `live.interactions`, routed by their ACP scope (protocol §4.4). The
  bridge notes each JSON-RPC request it sends, from send to response, with the scope
  that shows its elicitations (an in-memory map, no record after the response); a
  response ends the elicitations that name the request. Interactions of a session being created wait in the creating
  operation's scope and move in the installing step. The `answer` admission path
  becomes a resource action that resolves one interaction under the bridge guard.
  `agent_request` and its completion rule go.
- **Terminals.** The terminal service keeps the handle; the session document keeps
  the frozen output with `released: true` while the reducer's index says an item
  embeds it, also when the Agent connection ends for a session that stays held.
  Holding counts only terminals with a handle.
- **Actions.** `set_mode`, `set_config` and `close` leave the operation slot: they are
  sent under the bridge guard without occupying the session, their HTTP response is
  held until the Agent answers, and `close` sets `closing` until then. The v1 close
  path of v0.2.8 (`close_session`) is the behavioral reference, not code to restore.
- **Diagnostics.** The MCP relay and the Agent's stderr callback, which today are not
  projected (stderr goes to the bridge log), write into `global.live.diagnostics` through the commit step, bounded as the
  terminal tail is.
- **History status.** `SessionLog::Known` carries `complete` or `partial`;
  `adopt_session_log` sets `partial` instead of building a known empty log.

Nothing here adds persistence, vendor branches, or the earlier draft's intents,
request records or epochs.

### 7.2 Steps

| Step | Work | Removes | Acceptance (redesign §11) |
| --- | --- | --- | --- |
| 10 | Docs: this revision, the audit, the release parity matrix, the coverage ledger | — | review |
| 11 | Timeline reducer and entry shape: items, `end`, `_meta`, message boundaries, attachments as received, hash over the entry; live commits and replays through it; generated TypeScript; the web projection renders items from the log as it renders `op.process` today. Prompt `end` on completed and cancelled turns. | D1, D2, D3, D6, D7 | 1, 13, 19, 20, 25, 26, 27, 30, 31 |
| 12 | Settling of failed and interrupted prompts | D4 | 2, 22 |
| 13 | The tail between prompts; settle at prompt start and connection end; late updates rewrite their entry; the convergence gate gains rewrite and idle scenarios | D5 | 4, 21 |
| 14 | History status `partial` for a cold resume; the client keeps a replaced log as unconfirmed UI state | D9 | 23 |
| 15 | Terminal handle and output separated: frozen output through release and the connection's end; the holding change | D8 | 12, 24 |
| 16 | Interactions as scope resources with ACP lifecycles; per-interaction answers in bridge and client; URL flow outcomes; `agent_request` removed | D10, D11, R4 | 7, 8, 11 |
| 17 | Coverage ledger chains for the new paths; an extension-notification test (audit Q5) | — | the ledger's rows |
| 18 | Actions: `set_mode`, `set_config` and `close` outside the operation slot, `closing`, `end: closed`; the client's controls enabled during a prompt and the close confirmation | R1, R2 | 18, 28, 29 |
| 19 | `global.live.diagnostics`: Agent and MCP stderr, MCP-over-ACP connections and activity; the collapsible panels | R3 | 32 |
| 20 | Markdown export over items, `end` and terminal snapshots; the browser cases of settled content and per-interaction answers | (D1, D4, D8 in the client) | 33, 34, 35 |

Dependencies: 10 → 11 → 12 → 13; 14 and 15 after 11; 16 after 10 and independent of
11–15; 17 after the step whose chain it records; 18 and 19 after 10 and independent of
11–17; 20 after 12, 15 and 16. Each step leaves the product working: steps 11–15 only
add content to documents the client already renders, 16 and 18 change one write and
its client together, and 19 adds a `live` field. No step is optional: R1–R4 and the
defects are closed only when their cases pass against the target.

### 7.3 Gates

- **Fidelity cases** (audit §8): a scripted ACP Agent sends a fixed update sequence;
  the test asserts the reduced items and fields of the log or the tail, written from
  the ACP input, not from the projection.
- **Convergence** (§4.1): the three replicas, plus settle points that rewrite an
  entry, idle tail growth, and a coalesced rewrite.
- **Browser**: completed, failed and reloaded turns show their tool cards, thoughts,
  plans and compactions collapsed by default and expandable; a released terminal's
  output stays in its tool card; the v0.2.8 cases of live configuration and close
  (`acp-ui.pw.ts` at the release, from line 2694) pass again.
- **Release parity**: each row of the [matrix](release-parity.md) marked Regression
  has a passing case; a test changed to assert a loss is changed back, not counted.
