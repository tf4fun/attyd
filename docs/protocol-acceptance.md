# Protocol acceptance cases

These are the 37 acceptance cases originally numbered in
`client-api-redesign.md` §11. Their numbers are stable; `a01`–`a37` provide direct
links. They specify expected behavior, not a claim that tests or implementation
already pass. The [implementation plan](state-sync.md#7-fidelity-migration-revision-2)
assigns the work; the [ACP source matrix](acp-bridge-matrix.md) adds field-level
first-layer coverage.

**Revision 3 (2026-10-21).** Rules S1–S8 of revision 2 map to
[memory model](bridge-memory-model.md) (content, settling, resources) and to protocol
rules B1–B6 (documents, versions, holding); C1–C3 keep their names
([protocol rules](protocol.md#1-model)). Cases that assert the hash chain, `have=<n>:<h_n>`,
settle points or held `create`/`fork`/`list` responses are read as the revision 3
mechanism: `epoch`/`rev`, differential snapshots, digests, and `202` with results in the
document. The section references in case titles retain the original proposal's
numbering: old §4.1 maps to [session history](protocol.md#3-session-history), §4.2/§4.3 to
[live documents and resources](protocol.md#4-live-nodes), §5.2 to
[synchronization](protocol.md#55-checkpoints-recovery-coalescing-and-keepalive), §6.1/§6.2 to
[operations](protocol.md#6-operations) and [actions](protocol.md#7-actions),
§7 to holding (S6), and §8 to [reads](protocol.md#8-reads-and-holding).
They are historical cross-references, not a second contract.

The design must also satisfy the independent verification and repair cases in
[protocol §12.4](protocol.md#124-independent-acceptance).
Baseline convergence cases below do not substitute for those failure-injection
requirements. Cases 32–35 contain later presentation work; implementation status
and release-parity classifications remain in their respective ledgers.

**Revision 4 (2026-10-24).** The failure-injection cases are
[protocol §12.4](protocol.md#124-independent-acceptance). The scenarios S1–S5 of the
[design](protocol-r4-design.md#10-scenarios) are run by a client that is not the page,
`scripts/reference-client-smoke.ts`, against a real server:

| Scenario | Observation required |
| --- | --- |
| S1 stream | the device holds the latest output as a window of `N = 4096` and the operation head; a 1 MiB output reaches it verified at a checkpoint; no event carries `N + L` bytes of the window's text, none is held beyond it, and applying an event needs under `2(N + L)` bytes of text |
| S1 poll | a device without a stream asks the same paths with `GET /nodes/…`, holds the same content at the same version, and is told `unchanged` when nothing changed |
| S2 | the window is the end of the desktop's latest message: equal length, and `offset` is the bytes before it |
| S3 | a request repeated with its own `id` and `expect` answers `202` with the same operation and the prompt runs once |
| S4 | two prompts with the same `expect`: one `202`, the other `stale`, whose outcome is `superseded` |
| S5 | two answers to one interaction: one delivered, one refused; a `cancel` against the running prompt is admitted and the prompt ends |

**Delayed writes and occupation (issue #216, P1-1).** The coordinator tests in
`src/bridge/protocol_gate_tests.rs` use the real ACP transport and explicit admission
and response boundaries, without a timing sleep:

- `a_delayed_prompt_and_its_unchanged_retries_execute_once`: hold the original before
  ingress, read `open`, admit its unchanged retry, then deliver the original while the
  Agent's response is pending and repeat again after completion. All copies identify
  the same operation; the Agent receives exactly one `session/prompt`, one user entry
  is retained, and the outcome is `admitted`.
- `a_failed_delete_rejects_a_delayed_prompt_and_allows_one_new_request`: prepare a
  prompt against the session's current operation, let a global delete occupy the
  session, then refuse its ACP close. Both targets keep the failed delete's ID and the
  old prompt is permanently `superseded`. A new request against the failed delete
  runs once; the delayed old request is `stale`, and copies of the new request do not
  send another ACP prompt. EOF joins the coordinator before the remaining outgoing
  messages are checked, so the absence check includes all completed sends.

The projector also asserts in debug builds that a changing operation ID is absent
from the retained records and their predecessors. Status updates of the current
operation are allowed; the bounded log is not an archive of older IDs. Unit cases in
`src/protocol/outcome/tests.rs` cover the operation, scope, session and connection-end
failure paths; `src/protocol/tree_doc/tests.rs` checks the invariant assertion.

## Acceptance

Revision 4 review boundaries (#216) have these concrete gates in addition to S1–S5:

| Boundary | Gate |
| --- | --- |
| A lost fork answer stays resolvable in its source after a later prompt; `global.op` and `global.ops` do not acquire the fork | `server::stream_tests::tree::a_lost_fork_answer_is_resolved_on_its_source_after_another_operation`, plus shared Rust/TypeScript outcome vectors |
| Forks from different source sessions cannot install together; a refused fork changes nothing and is accepted after the first ends | `server::stream_tests::bench::admission::forks_of_different_sessions_cannot_install_together` |
| Poll replies from an abandoned instance or an older path version never replace current state; overlapping differential replies use the base still held | `tests/reference-client.test.ts` |
| A cold polling read sends the directory as `cwd`, without the stream query's session prefix | `tests/reference-client.test.ts` |
| A non-node `follow` or `GET` is refused before observing or loading its session | `server::stream_tests::tree::non_node_paths_are_refused_before_a_session_is_loaded_or_observed` |

Late session demand after EOF (#224, P1-1) is covered by
`server::stream_tests::tree::late_demand`, over real HTTP and a scripted ACP Agent:

| Boundary | Gate |
| --- | --- |
| With no session observed at EOF, a later session or view stream/read starts the next connection and receives replayed content | The four `a_session_*` / `a_view_*_after_unobserved_eof_starts_and_loads_the_next_connection` cases wait for the old runtime's input to be gone before registering demand |
| Concurrent late streams and reads share one connection and one load | `concurrent_late_streams_and_reads_share_one_connection_and_one_load` |
| A short output read retains its load demand and cwd through connection backoff, even after a second operation read supplies no cwd and both HTTP bodies have ended | `late_short_reads_keep_the_first_directory_and_demand_through_connection_backoff` |
| Global/catalog reads and streams alone do not start a connection | `late_global_and_catalog_reads_and_streams_do_not_start_a_connection` |
| An unheld session followed without cwd ends with `load_failed` / `invalid_request`, without sending `session/load` | `a_late_session_stream_without_cwd_ends_instead_of_waiting_forever` |

Same-version reconnect verification (#224, P2-1) has these client gates. A snapshot at
the held version checks the held value and updates its verified version; it does not
install the payload again, even when its differential `base` is older. A hash mismatch
drops that path and requests a full snapshot without its `have`.

| Boundary | Gate |
| --- | --- |
| Equal-version full, `same`, and older-base differential snapshots confirm the current value without replacing it or applying it twice | [tests/tree-replica.test.ts](../tests/tree-replica.test.ts): `verifies a same-version ${form} snapshot without installing it again` |
| All three forms reject a different hash, including when the held version was already verified, and omit `have` on recovery | [tests/tree-replica.test.ts](../tests/tree-replica.test.ts): `asks for the whole path when a same-version ${form} snapshot has a different hash` |
| Older full and differential snapshots neither roll back the value/version nor clear its verification | [tests/tree-replica.test.ts](../tests/tree-replica.test.ts): `keeps a newer verified replica when an older snapshot carries a different hash` |
| After warming the text-chain cache, a corrupt append followed by a disconnect before the checkpoint is detected by the reconnect's `same` snapshot; a full snapshot repairs it without a later update or checkpoint | [tests/tree-feed.test.ts](../tests/tree-feed.test.ts): `repairs a corrupted append on reconnect even when no later change or checkpoint arrives` |

The reference client's memory figures count UTF-8 window text, not the actual heap used
by parsed JSON and replica copies. View polling below the 30-second `VIEW_LINGER` can
receive `unchanged`; after eviction it receives a full snapshot again. These are the
measurement and retention limits described in the design §11 and protocol §7.1.

Each rule gets tests at the bridge first and then in the client engine. Document
consistency is checked after **every** event, not only once synchronization ends.
Revision 2 adds a second kind of check: a replica that equals the bridge's projection
proves nothing if the projection dropped content. Cases 1, 2, 13, 19–27 and 30–33
therefore also assert, **from the ACP input**, which items and fields the log or the
tail holds (audit §8). Compare release behavior against stable rc.5, distinguishing
regressions from gaps shared with that release and from later product work
([release parity](release-parity.md)). The older v0.2.8 references are historical
evidence, not proof of rc.5 behavior. In particular, case 32's diagnostics remain
a later proposal, outside the ACP-to-bridge completion gate. A test deleted, or
changed to assert that a capability is gone, never establishes parity. Cases marked
r2 are new or changed in revision 2; their runtime code is not migrated yet.

<a id="a01"></a>

1. **Identity (S1, S8, §4.1)** r2. Three separate checks:
   - **Reduction is chunking-invariant.** The same ACP update sequence, streamed in
     different chunks (also a user message of text and an image), gives the same
     items. Chunks that differ in `_meta` or annotations stay separate chunks.
   - **Equal entries, equal hashes.** Two entries that are equal as published (items
     and `end`) have the same hash, whether reduced live and settled or rebuilt;
     entries that differ in a tool result, a `messageId`, a message boundary, a
     chunk's `_meta` or the presence of `end` have different hashes.
   - **The replay is authoritative.** A replay of a live-observed turn, which has no
     `end`, gives an entry that differs from the live one; alignment replaces the
     live entry with it and the client applies that normally. Live and replayed
     entries are not required to be equal.

   Normalized content with attachment references and prompts that reference
   attachments pass the generated types and validation; the bridge resolves
   references to the blocks as received before it calls the Agent, and an attachment
   read returns them unchanged, `_meta` included.

<a id="a02"></a>

2. **Settling (S2)** r2. A prompt's end appends its entry with `end`, empties the tail
   and ends its operation in one event, for a completed, a cancelled, a failed and an
   interrupted prompt alike. The failed prompt keeps its prompt and error on `op` too.

<a id="a03"></a>

3. **Alignment (S3).**
   - A client with a matching head receives only the suffix.
   - A client with a diverged or longer log receives the whole log and replaces its
     own.
   - After a reopen with the same replay, the client keeps its log.
   - Alignment at a settle point never yields a settled entry beside the same
     content in the tail, for a new subscription, a reconnect and a slow subscriber
     alike.
   - A cold reopen with the same history never clears the client's log first, and
     interactions during the load can be answered. A disconnect before the load,
     during it, and after it but before its log was delivered all converge on
     reconnect. A history that did change is replaced by hash.

<a id="a04"></a>

4. **Synchronization (§5.2).** A broken stream, a slow subscriber and a replaced stream
   all end with the client equal to the bridge. Events of a replaced stream change
   nothing. With the client's consumption paused, a log change queued but not sent
   and then coalesced leads, once consumption resumes, to an alignment the client
   applies directly, without subscribing again. This holds for a log change that
   rewrites an earlier entry (case 21) as for one that appends.

<a id="a05"></a>

5. **Live changes (S1, S4).** Controls and interactions change while the log hash
   stays the same, and the replica follows.

<a id="a06"></a>

6. **Admission (S5).** For each operation kind:
   - once a write that starts an operation is admitted, a concurrent second write and
     a resend with the same `expect` get `stale`; a resend of a write that was not
     admitted may be admitted;
   - a fork resend does not fork twice;
   - a resent cancel succeeds without further effect;
   - a cancel of a kind that does not accept it gets `not_allowed`, and a requested
     cancel keeps the operation's scopes occupied until it actually ends.

<a id="a07"></a>

7. **Answers (S7)** r2. Two clients answer two different interactions of one scope,
   and both are delivered. Two answers to one interaction deliver exactly one; the
   other gets `404`. An answer that does not fit its request is `invalid_request`
   and leaves the interaction pending.

<a id="a08"></a>

8. **Interactions everywhere (§4.3)** r2. Interactions during `load`, `fork`,
   `create`, `set_config`, `list` and outside any operation are shown and answered
   the same way. A session-scoped elicitation that arrives during a `set_mode`
   survives the mode change's end. A cancel of a prompt answers the session's pending
   permissions `cancelled`. A URL flow accepted outside an operation stays visible
   after the next operation replaces `op`. Request scopes (each from the ACP input,
   asserting where the interaction is shown and when it ends):
   - a delete of a held session: an elicitation naming its `session/close` ends with
     the close's response while the delete still runs; one naming its
     `session/delete` ends with the delete;
   - an elicitation naming a request no write sent (`initialize` on reconnect, a
     release's close) is shown in `global` and ends with that request;
   - an elicitation with `requestId: null`, and one naming an answered request, are
     refused with `invalid_params`, with the two cases distinguishable, and never
     shown;
   - a permission and a URL elicitation for the session a `create` is creating are
     shown in `global`, move into the session in the event that installs it, and the
     accepted URL flow stays with the session; when the `create` fails they are
     answered with their cancel outcome and the flow is removed.

<a id="a09"></a>

9. **Responses (§6.2).**
   - An answer's success arriving before the stream shows the change, and an
     operation B replaced by C before a slow subscriber sees B, both leave the replica
     correct and the client waiting on nothing.
   - `create` and `fork` deliver the new session ID in their response even when `op`
     was already replaced.

<a id="a10"></a>

10. **Occupation (S5, §6.1).**
    - A prompt on a held session racing its delete is refused.
    - A subscription to an unheld session during its delete gets `end: deleted`, or
      its load after a failed delete.

<a id="a11"></a>

11. **URL flows (S7)** r2, release.
    - A URL flow of `global` (authenticate), of `catalog` (`list`) and of a session
      survives a reload of the client between accept and completion.
    - The Agent's completion, also long after the accept, shows it `completed` on
      every replica; the Agent connection's end shows it `ended`; the two are
      distinguishable in the browser.
    - A request-scoped flow outlives its parent request: accept, then the parent's
      response (once success, once a JSON-RPC error), and the flow is still
      `waiting` and keeps its session needed; a late `elicitation/complete` then
      shows it `completed`. In the same run an interaction of that request not yet
      answered ends with the response, answered with its cancel outcome, and the
      bridge no longer has the request outstanding: an elicitation naming it
      afterwards is refused with `invalid_params`.
    - A finished flow keeps its first outcome: a `completed` flow stays `completed`
      when the connection ends afterwards, and an `ended` flow stays `ended` when a completion for its `elicitationId` arrives
      late (which then completes no other flow).
    - A finished flow no longer keeps its session needed. It is removed on a
      client's dismissal (also when dismissed twice; a second gives `404`) and when
      the session scope that shows it ends; a flow of `global` or `catalog` stays
      until dismissed.

<a id="a12"></a>

12. **Holding (S6, §7).**
    - A session released before its first subscription is loaded by that
      subscription and continues its log.
    - A session created on an Agent without `session/load` and `session/resume` is
      never released while the connection lasts, and a late first subscription finds
      it.
    - A one-off read holds nothing.
    - A session kept only by a URL flow, a pending interaction or a running terminal
      is released (if it can be brought back) once that resource ends, and is not
      released before. A released terminal's output does not keep it.

<a id="a13"></a>

13. **Attachments (§8)** r2. Attachments are readable while referenced by `log` or
    `live`, return the block as received, and stop being readable once nothing
    references them.

<a id="a14"></a>

14. **Delete (§6.1).** Delete of a held session ends its scope with `deleted`; a failed
    delete leaves its operation on both `global` and the held session, with the error
    summary in `op` and its body in `opDetail`. The session does not restore the ID the
    delete replaced; a delayed write against that ID stays `superseded` and is refused
    as `stale`, while a new write against the failed delete may proceed.

<a id="a15"></a>

15. **Client obligations (C2, C3).** After a write without a definite answer, the
    client does not resubmit it with a new `expect` when it later sees `stale`;
    never-sent and definitely refused queued input is resubmitted.

<a id="a16"></a>

16. **Terminal input (S7, C3).** Input that the bridge wrote but whose response was
    lost is not resent by the client; the same text typed again by the user is
    delivered as new input. Repeated dismissal and repeated resize to the same size
    leave the same state.

<a id="a17"></a>

17. **Terminal logins (S7).** A login with method A fails and is not dismissed; a
    login with method B starts. A fresh replica shows both terminals with their own
    method.

<a id="a18"></a>

18. **Controls (§4.2)** r2. A successful `set_mode` or `set_config` whose response
    comes without a notification shows the new mode or the confirmed config options
    when its response arrives; a failed one changes nothing and returns its error.

<a id="a19"></a>

19. **Retained content (S8)** r2. A scripted turn with thoughts, message chunks of two
    `messageId`s around a tool call, chunks of one `messageId` on both sides of a
    tool call (`m1 "a" → tool → m1 "b"`: two items, both `m1`), a tool call with sparse updates (status, content
    with a diff and a terminal, locations, raw input and output, `_meta`), a legacy
    plan and an ID-addressed plan, and a compaction with summary chunks: after it
    completes, after it fails, after it is cancelled, and after a `load` replays it,
    the log holds each entity in its final reduced state, in timeline order, with the
    message boundaries.

<a id="a20"></a>

20. **Sparse updates (§4.1)** r2. A `tool_call_update` that changes only `content`
    produces a document change and a new hash; one that omits a collection keeps it;
    one that sends a collection replaces it whole, and `content: []` clears it; one
    with `{ name: null, content: null, rawOutput: null }` leaves name, content and raw
    output as they were; one for an unknown ID starts an item. A compaction update
    with `summary: null` clears the summary, and one without `summary` keeps it.

<a id="a21"></a>

21. **Late and idle updates (S2)** r2. A `tool_call_update` and a compaction summary
    chunk that arrive after the prompt's response change the settled entry they
    belong to, by a log change from its index; the later entries' turns are unchanged
    and their hashes are recomputed; every replica converges, including one that was
    coalesced. A message, plan or compaction the Agent sends while no
    prompt runs appears in the tail and settles into the last entry when the next
    prompt starts.

<a id="a22"></a>

22. **Failure keeps output (S2, S8)** r2. A prompt that streams messages and a tool
    call and then fails with an Agent error, and one cut by the Agent connection's
    end, both settle an entry with that content and `end.error`.

<a id="a23"></a>

23. **Partial history (S1, §5.2)** r2. A resume-only session that was released and is
    opened again ends its load with `history: "partial"`; a client that held its old
    log is never told the history is empty. A resume of a held session keeps its log
    and status.

<a id="a24"></a>

24. **Released terminals (S7)** r2. A terminal embedded in a tool call and then
    released keeps its final output and exit status, marked `released`, for every
    replica and a fresh one; it no longer keeps its session needed; it is removed
    once nothing embeds it. Then, in one scenario: embed → exit → release → stay
    subscribed → the Agent connection ends (and, separately, a terminal login
    succeeds and the Agent reconnects) → a resume that keeps the log: the output,
    `exitStatus` and `truncated` are unchanged, `terminal/*` on the ID fails, and the
    terminal does not keep the session needed. Persistence across a bridge restart is
    not required.

<a id="a25"></a>

25. **Opaque fields (S8)** r2. `_meta` on a chunk, a content block, a tool call, a plan,
    a compaction, a compaction summary chunk and a prompt response, and annotations on
    blocks, are in the log as received. Two text chunks, and two summary chunks, that
    differ only in the chunk's `_meta` stay two chunks with their own `_meta`.

<a id="a26"></a>

26. **User chunks (§4.1)** r2. User message chunks the Agent sends during a live
    prompt are items of its turn, in order.

<a id="a27"></a>

27. **Replay authority (§4.1)** r2. A replay that differs from what was observed live
    (fewer details, other IDs) replaces the log; nothing the replay did not provide is
    invented, and the observed `end` of replaced turns is not carried over.

<a id="a28"></a>

28. **Controls during a prompt (S4)** r2, release. While a prompt runs, `set_mode` and
    a boolean `set_config` are delivered to the Agent and their confirmed values reach
    every replica; the prompt is not affected. In the browser, the v0.2.8 case: the
    "Verbose" switch toggles during a running prompt.

<a id="a29"></a>

29. **Close (S4, §6.1)** r2, release. In the browser, the v0.2.8 case: closing a
    running session asks for confirmation; dismissing it sends no request; confirming
    sends exactly one close and returns to the project. At the bridge: a close during
    a prompt answers pending permissions `cancelled`, refuses a new prompt while
    `closing`, ends the scope with `closed` on success and leaves the session in the
    catalog; an Agent error leaves the session as it was; a later subscription loads
    it again.

<a id="a30"></a>

30. **Message identity and turns (§4.1)** r2. A replay of a user message of text and an
    image with one `messageId` is one turn; `user m1 → tool → user m1` in a replay is
    one turn; two user messages with different IDs are two turns. Live, the bridge's
    user item has one chunk per prompt block, and an Agent user chunk starts no turn.

<a id="a31"></a>

31. **Plan identity (§4.1)** r2. Turn A creates ID plans P and Q; turn B updates and
    then removes P. The log holds one item for P, at its position in turn A, in its
    removed state, and one current item for Q; no older state of P remains current.
    A legacy plan sent in turns A and B leaves one snapshot in each, and the current
    plan is B's.

<a id="a32"></a>

32. **Diagnostics (§4.2)** subsequent presentation proposal, based on v0.2.8;
    not a first-layer acceptance gate. If retained in step 19's scope, MCP-over-ACP connect, message and disconnect
    appear in `global.live.diagnostics.mcp` (connection count and messages, bounded,
    oldest dropped with `truncated`); Agent and MCP stderr appear in `agentStderr`;
    a fresh replica sees the same; nothing is kept across a bridge restart.

<a id="a33"></a>

33. **Markdown export (§8)** r2, release. The export case of the
    [release parity matrix](release-parity.md#markdown-export), from a settled log,
    from the tail and after a reconnect.

<a id="a34"></a>

34. **Per-interaction answers in the browser** r2, release. Two pending interactions
    of one session can be answered separately, in either order, as in v0.2.8.

<a id="a35"></a>

35. **Settled tool cards in the browser** r2, release. Completed, failed and reloaded
    turns show tool cards, thoughts, plans and compactions collapsed by default and
    expandable, with the disclosure kept across new turns; thread search and change
    review find their content.

<a id="a36"></a>

36. **First sparse tool state (§4.1)** r2. From the ACP input, for each of: a
    permission whose `toolCall` is `{ toolCallId: "t" }` before any `tool_call`; a first
    `tool_call_update` `{ toolCallId: "u", status: "pending" }`; then later updates
    that add `title`, `kind`, `content` and `rawInput`, and a `tool_call` for `t`:
    - the item exists from the first update, at its position, and holds exactly the
      fields received so far; no `title`, `kind` or `status` is filled in, no update
      is dropped, and no `null` appears in the state;
    - each later update fills in its fields by the patch rule, and the `tool_call`
      gives its fields, its schema defaults included;
    - the same holds in the tail, in the settled log, in a replay that sends the same
      updates, and in the hash; the generated TypeScript types the item's `call` as
      `ToolCallUpdate`, and the browser renders an item without `title` (as not
      reported) in the live turn, a settled turn, a reload and the export.

<a id="a37"></a>

37. **Actions hold their session (S6, §6.1)** r2. With a release delay of `0`, an
    idle session gets a `set_config` the scripted Agent leaves unanswered, and the
    last subscription ends:
    - the session stays held and is not released, closed or loaded again while the
      request is outstanding;
    - a session-scoped elicitation the Agent sends meanwhile is shown in the session
      and answered; a subscription made meanwhile aligns with the held documents;
    - the delayed response folds into that session's `controls` and answers the
      original HTTP request; then, with nothing else needed, the session is released
      after the delay, once;
    - the same for `set_mode` and for `close`, where the release sends no second
      `session/close` and the close ends the scope with `closed`; while a release's
      own `session/close` is outstanding, a client `close` gets `not_allowed`;
    - the Agent connection ending while the request is outstanding answers it
      `unavailable` and changes no `controls`.

The bridge's handling of ACP failures and its internal clean-up belong to layer 1 and
are tested there. They add no duties to clients.
