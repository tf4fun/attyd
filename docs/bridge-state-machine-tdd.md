# Bridge state machine TDD ledger

This is the acceptance ledger for moving ACP lifecycle ownership from the browser into the Rust
bridge. The bridge is a complete, long-lived ACP v1 client. A browser is a disposable view that
sends user intents and renders versioned business state; it does not infer whether an ACP turn,
interaction, or session mutation is active. [active-turn-runtime.md](active-turn-runtime.md) is the
normative retention and recovery contract: the bridge never stores completed conversation history.

The test plan was adversarially reviewed from three independent angles: ACP protocol coverage,
concurrency and lifecycle faults, and model/property testing. A checked item is not complete merely
because a similarly named UI test exists. It must pass at the layer named below.

## Product boundary

The Agent is the only completed-history authority. The bridge owns non-persistent state only for
active turns, accepted queued work, live ACP resources, control/lifecycle metadata and transient
delivery. Browser state is a disposable projection.

```text
Agent completed history --load--> Subscriber replacement projection
          ^                             ^
          |                             |
          `---- Bridge active runtime --'
                 (no completed turns)
```

The browser may own an unsent composer draft and purely visual preferences such as disclosure,
selection, and scroll position. Once an intent is accepted, its lifecycle belongs to the bridge.
This includes submitted prompts, queued follow-up prompts, permission and elicitation responses,
session mutations, and close-before-delete transactions. Disconnecting every browser must not
cancel or abandon accepted active work. After a turn retires, the bridge no longer owns its content;
a later browser obtains Agent replay through `session/load` or receives `HistoryUnavailable`.

The browser-facing protocol will be versioned business semantics:

- an atomic active-turn `generation + revision` snapshot;
- ordered deltas with exact `fromRevision -> toRevision` continuity;
- transactional session replacement generations for `session/load`;
- requester-directed intent acceptance/rejection;
- bounded debug payloads that preserve ACP data but are never the sole source of liveness state.

ACP-shaped debug events may remain inspectable, but the browser must not fold them to determine
`running`, active operations, interactions, terminal ownership, or connection lifecycle.

ACP v1 does not correlate `session/update` notifications with a turn or prompt operation. The
bridge therefore quarantines conversation updates received while a canonical session has no active
turn. Once the next turn is active, a physically late update from the previous turn is
indistinguishable from a valid early update for the new turn; correctness at that boundary requires
the Agent to finish sending a turn's notifications before its PromptResponse. This is an explicit
protocol ambiguity, not a state the bridge can safely guess from payload shape or timing.

### Active materialization and transactional replacement

The replication analogy applies only while a turn or live resource exists. The bridge does not
implement a checkpoint store, completed-history replica, or crash-safe WAL:

```text
Bridge runtime
|- one normalized ActiveTurn per prompting session
|- bounded queued prompts
|- live interactions, URL flows, terminals and MCP operations
|- bounded control/lifecycle metadata
`- bounded active snapshot and load-delivery transactions
```

A PromptResponse, prompt error, or transport loss drives the active turn through one terminal
delivery transition. After the Hub has committed an owned terminal event and any active-snapshot
leases have ended, the bridge drops all turn content. Sending cancel remains nonterminal until one
of those terminal conditions occurs.

An active browser reconnect snapshots the current normalized turn and catches up from a revision
barrier without an Agent request. An idle completed reconnect performs a single-flight
`session/load` only when advertised; otherwise it reports `HistoryUnavailable`. Replay is streamed as
a transactional browser replacement generation and is never installed as bridge session history.
ACP v1 supplies no replay watermark, durable turn IDs, historical PromptResponses, stop reasons, or
formal turn boundaries, so the browser renders exactly what the Agent replays and no more.

Mode/config/session metadata, auth, close/delete, MCP, interactions, URL flows, and terminals are
separate live planes. They are removed at their own protocol terminal point. In particular, a live
terminal may outlive a turn, while a released terminal is not retained to improve future history.

The replication identity is `(epoch, globalSeq)`. Epoch identifies one ACP connection lifetime and
must not repeat across process restarts. Each activation of a session ID also receives an
`incarnation`; optional per-session revisions support stale-intent rejection but never pretend to be
an Agent persistence revision. Non-idempotent Agent operations whose response is lost after possible
dispatch enter an honest `Uncertain` state and are never blindly retried.

## Canonical model

```text
ConnectionRuntime
|- generation, revision
|- phase: Starting | Initializing | AuthRequired | Ready
|         | RestartPending | Stopping | Stopped | Error
|- auth: Idle | Authenticating | TerminalRunning | Authenticated
|        | LoggingOut | LoggedOut | Failed
|- listed sessions and pagination transaction
|- sessions: Map<SessionId, SessionRuntime>
|- request-scoped elicitations
|- MCP current connections and pending calls
`- bounded diagnostics ring

SessionRuntime
|- lifecycle: Attaching | Active | Closing | ClosingForDelete
|              | Deleting | Closed | Uncertain
|- authoritative cwd, metadata, modes, config, commands, usage
|- operation: Idle | Prompting | Cancelling | Forking | Closing
|              | Deleting | SettingMode | SettingConfig
|- one active normalized turn and bridge-owned follow-up queue
|- pending/responding permissions and elicitations
|- accepted URL flows
|- live terminal resources
`- bounded raw debug references
```

Small completed request tombstones may be retained in a bounded set only to enforce idempotency; they
contain no prompt, response, transcript, tool, thought or terminal payload. `Uncertain` tombstones
are pinned metadata because forgetting one could blindly repeat an Agent mutation. Fork results are
rebuilt from the Agent response/replay and never copy bridge history or live resources.

## Non-negotiable invariants

Tests and implementation comments refer to these identifiers.

- **I01 Epoch isolation:** output or completion from an old ACP connection epoch or old session
  incarnation cannot mutate the current runtime.
- **I02 Revision continuity:** a delta applies only when `fromRevision` equals the subscriber's
  current revision. A gap, duplicate, or reordering requests a new snapshot.
- **I03 Replay equivalence:** ActiveSnapshot@N followed by active deltas N+1..M equals the active
  normalized turn at M.
- **I03a Active-turn retirement:** a live turn exists in exactly one materialized representation.
  Its terminal event atomically makes it unavailable to new active snapshots, then retirement drops
  every turn payload after existing snapshot leases end.
- **I04 Replay idempotency:** applying a snapshot or terminal delta twice cannot duplicate a turn,
  operation, interaction, tool, or message.
- **I05 Request isolation:** in-flight operation identity is bridge-owned and globally unique within
  a generation. A duplicate browser request ID cannot settle or fail the original operation.
- **I06 Per-session exclusion:** a session has at most one mutually exclusive turn/mutation; different
  sessions may progress concurrently.
- **I07 Liveness pinning:** active turns, queued accepted prompts, pending/responding interactions,
  URL flows, current MCP connections, and latest terminals cannot be evicted with debug history.
- **I08 No completed history:** after retirement, the bridge contains no completed prompt, response,
  message, thought, tool, terminal-output, transcript or Agent-replay payload for that turn.
- **I09 Durable authority:** a new bridge generation rebuilds durable history only through Agent ACP
  methods, never from a browser cache.
- **I10 Narrow observer independence:** active subscribe/disconnect/slowness cannot alter Agent work.
  An idle completed reconnect may issue at most one capability-gated, rate-limited `session/load` and
  never prompt, cancel, close, restart or automatically retry the Agent.
- **I11 Exact terminal outcome:** every accepted intent reaches exactly one success, failure,
  cancellation, or explicit uncertain state.
- **I12 Scope isolation:** unknown, pending-open, closed, or different-session updates never pollute
  another session.
- **I13 Irreversible lifecycle:** successful close/delete and terminal resource states cannot be
  resurrected by late events.
- **I14 Capability fidelity:** optional methods and content are never sent unless negotiated.
- **I15 Opaque metadata:** `_meta` and compatible unknown fields are preserved for debug, bounded,
  and never interpreted as business control.
- **I16 Bounded queues:** every journal, subscriber, command, interaction, terminal, and MCP queue has
  an event or byte limit; a slow subscriber cannot block the Agent or a healthy subscriber.
- **I17 Resource closure:** subprocesses, terminals, MCP providers, waiters, responders, and tasks are
  released at their terminal lifecycle point.
- **I18 Raw/debug separation:** raw ACP payloads cannot be the only representation of active business
  state.
- **I19 Pre-response fidelity:** valid notifications received before new/load/resume/fork responses
  are transactionally bound only to the returned session and contribute to pinned state.
- **I20 Error correlation:** only epoch + internal operation ID + session incarnation + operation
  kind can settle an operation. Stale or requester-local rejection errors are diagnostics only.
- **I21 Honest uncertainty:** after a non-idempotent request may have reached the Agent, loss of its
  terminal response produces `Uncertain`; the bridge must not report failure or automatically retry.
- **I22 Single active representation:** active business state is materialized once. Dropping raw
  diagnostics leaves it unchanged; retirement removes it rather than moving it into completed state.
- **I23 Honest history availability:** completed reconnect either commits one successful Agent load
  replacement or reports a precise unavailable/rejected/invalid/oversized/uncertain result. It never
  presents bridge-derived or partial replay as complete history.

## Test layers and harnesses

| Layer | Target | Harness | Gate |
| --- | --- | --- | --- |
| L0 | ACP surface ledger | compile-time/exhaustive tables | every method, union and capability classified |
| L1 | pure canonical state | `ReferenceModel` + production reducer | all deterministic and property invariants |
| L2 | ACP adapter | channel-driven `ScriptedAgent` and raw JSON-RPC fixture | request/response/notification translation |
| L3 | Bridge Hub | in-memory subscribers with barriers and tiny limits | snapshot cut, revision, routing, backpressure |
| L4 | real binary/transports | stdio process, HTTP/SSE and WS scripted Agents | lifecycle and resource equivalence |
| L5 | browser | snapshot/delta renderer and intent submission only | no ACP state inference in React |

The `ScriptedAgent` must use `Notify`, `Barrier`, and oneshot gates instead of timing sleeps. It records
browser intents, Agent ACP traffic, bridge business events, and cleanup milestones. Tests can pause at:

1. before Agent request dispatch;
2. after Agent receives a request but before commit;
3. after Agent commit but before response;
4. before and after bridge state commit;
5. before and after runtime publication;
6. before every cleanup await;
7. before generation finalization.

Runtime limits must be injectable so eviction tests use, for example, eight events and 512 bytes
rather than allocating tens of MiB. The state core exposes a normalized semantic snapshot to tests.

### Minimal subscriber protocol

Browser intents carry `expectedEpoch`, an epoch-global `clientIntentId`, and for session mutations
the expected `sessionIncarnation` and optional observed session revision. Subscriber identity is
never part of the idempotency key because it changes on reconnect. The bridge assigns its own
operation identity. Acceptance/rejection is directed only to the requester; accepted business state
and terminal outcomes are broadcast.

```text
Intent -> IntentAck(accepted | duplicate | rejected | uncertain)

ActiveSnapshotBegin(epoch, sessionId, turnKey, snapshotId, throughSeq)
  ActiveSnapshotChunk(...)*
  LiveResourceSnapshot(...)*
ActiveSnapshotEnd(epoch, sessionId, turnKey, snapshotId, throughSeq)

ActiveDelta(epoch, sessionId, turnKey, seq, stableEntityId, append | upsert | remove)
TurnTerminal(epoch, sessionId, turnKey, finalSeq, status, result?)

ReplaceBegin(sessionId, loadGeneration)
  ReplaceEvent(sessionId, loadGeneration, event)*
ReplaceCommit(sessionId, loadGeneration) | ReplaceFailed(reason)
HistoryUnavailable(sessionId, reason)
```

Active snapshot capture and subscriber registration share one session-actor transaction. A bounded
catch-up queue retains only active deltas after `throughSeq`; its first live delta is exactly
`throughSeq + 1`. Overflow or timeout disconnects that subscriber and releases its snapshot lease.
After `TurnTerminal` linearizes, no new active snapshot is available and all active catch-up payload
is retired. Completed reconnect starts a transactional Agent replacement or reports
`HistoryUnavailable`; there is no completed delta journal or full-session bridge snapshot.

## Implementation checkpoint

The P0 active-turn-only backend and transactional browser load replacement are production-wired.
ACP-shaped live events still drive ordinary incremental rendering, while the canonical snapshot/
delta stream supplies the bounded active reconnect proof. The current checkpoint is proven by the
following production-state tests, not only by React reducer mocks:

- live intents retain a fixed digest only; definite terminal outcomes retire them and `Uncertain`
  retains identity without result payload;
- prompt terminal transitions remove prompt/update payload and the payload-bearing delta prefix;
  the 10,000-turn regression leaves no completed payload marker;
- active text, tool-call and plan updates fold semantically while raw incremental deltas remain
  linear, and the Hub applies the identical fold;
- new attachment and same-session load have separate atomic transactions; a same-session load keeps
  its incarnation, uses the tracked Agent cwd, rolls back on rejection and routes replay only to the
  requesting subscriber;
- atomic fork/close/delete/control and prompt transitions, session incarnation rejection,
  independent concurrent sessions, queued prompt retention, and exact cleanup effects;
- transactional permission and form/URL elicitation response state, monotonic URL/terminal state,
  immediate terminal-resource removal, and request/session scope separation;
- snapshot plus contiguous delta equivalence, atomic subscriber bootstrap, two-subscriber equality,
  event-count plus byte-bounded bridge/subscriber queues, bounded subscriber count, gap detection,
  and single-flight replacement snapshots;
- constant-size `turn_update_appended` deltas for constant-size streaming chunks, avoiding cumulative
  active-tail retransmission;
- completed session history, load replay, released terminal output, resolved URL payload and arbitrary
  session `_meta` are absent from retained bridge state.
- production prompt follow-ups are bridge-owned, bounded, claimed in FIFO order, survive browser
  disappearance, dispatch only after the prior PromptResponse, cannot be bypassed during the
  handoff window, and become terminal before bridge shutdown completes;
- a claimed queued prompt re-enters the pre-dispatch lifecycle barrier, so shutdown cannot emit
  cancellation before its ACP PromptRequest has entered the SDK outgoing queue;
- idle-window late conversation updates are quarantined from canonical state, legacy business
  events, and next-turn semantic validation state;
- Agent-cancelled permission/elicitation responders never produce a false browser-facing resolved
  success event;
- subscriber byte accounting remains charged across the asynchronous WebSocket send, rolls back on
  full/closed channels, releases on receiver teardown, and is isolated per subscriber;
- canonical snapshots are idempotent, stale-generation commands/direct events are rejected, and
  dropping all legacy/debug caches leaves the canonical business projection unchanged;
- bridge generation completion has its own runtime-done signal and no longer depends on every event
  sender reaching EOF; already queued terminal events are drained before finalization;
- fatal stdio monitor errors cancel and await the normal connection cleanup path within a bounded
  grace period instead of dropping that cleanup future immediately.

At this checkpoint `cargo test` passes 169 Rust tests and `npm run check` passes 138 TypeScript tests,
all TypeScript type checks, and the production client build. Counts are documentary only; the named
invariants and red-to-green cases remain the acceptance criteria.

| Proven boundary | Executable evidence |
| --- | --- |
| reconnect idempotency | `reconnecting_subscriber_reuses_the_same_epoch_scoped_intent`, `completed_browser_intent_retry_after_reconnect_is_not_redispatched` |
| honest uncertainty under pressure | `uncertain_intent_tombstones_are_pinned_against_automatic_retry` |
| bridge-owned FIFO follow-up | `queued_prompt_claim_is_fifo_and_shutdown_cancellation_is_terminal`, `new_prompt_at_handoff_cannot_bypass_an_existing_queue`, `accepted_follow_up_dispatches_fifo_without_a_browser_owner` |
| pre-dispatch/shutdown ordering | `claimed_queued_prompt_reenters_the_predispatch_starting_barrier`, `prompt_completion_releases_exclusion_before_notifying_shutdown`, `shutdown_cancels_queued_prompt_before_stopped_without_dispatching_it`, `shutdown_cancels_every_queued_prompt_with_exact_terminal_results` |
| bounded prompt memory | `active_turn_has_a_hard_byte_limit_without_losing_cancellability`, `queued_prompt_admission_has_count_and_byte_limits_without_partial_mutation`, `queued_prompt_is_rechecked_against_the_active_turn_limit_before_dispatch` |
| transactional interactions | `permission_response_is_visible_and_exactly_correlated`, `elicitation_response_is_transactional_for_session_and_request_scopes` |
| interaction cancellation truthfulness | `failed_interaction_delivery_is_never_reported_as_resolved` |
| late update quarantine | `late_turn_update_without_active_turn_is_quarantined_from_all_business_streams` |
| linear streaming replication | `streaming_turn_deltas_do_not_republish_the_full_active_tail`, `canonical_projection_folds_typed_snapshot_and_contiguous_deltas`, `stale_turn_update_operation_forces_resnapshot_without_partial_mutation` |
| replay/debug separation | `replaying_the_same_canonical_snapshot_is_idempotent`, `dropping_legacy_and_debug_state_does_not_change_canonical_projection`, `turn_terminal_event_moves_active_to_observed_exactly_once` |
| linearizable subscriber cut | `subscriber_bootstrap_contains_atomic_canonical_snapshot_and_suffix`, `two_subscribers_receive_the_same_canonical_revision_and_state` |
| gap recovery | `gap_resnapshot_is_single_flight_and_reestablishes_contiguous_suffix` |
| count/byte backpressure | `slow_subscriber_is_evicted_without_blocking_a_healthy_subscriber`, `subscriber_backlog_is_bounded_by_bytes_not_only_event_count`, `subscriber_byte_accounting_releases_on_receive_and_failed_send`, `subscriber_byte_ledgers_are_independent_and_release_on_receiver_drop` |
| generation teardown/isolation | `bridge_completion_finishes_generation_with_a_leaked_event_sender`, `old_generation_commands_and_direct_events_are_rejected`, `relays_a_fatal_oversized_stdio_line_before_initialization` |

## P0 deterministic state-machine tests

These tests are written first and must pass before the browser protocol is switched.

### Admission, correlation, and concurrency

- [ ] `distinct_sessions_commute_while_same_session_is_exclusive` (L1, I06)
- [ ] `different_sessions_can_prompt_before_either_completes` (L2, I06)
- [ ] `same_session_turn_and_mutations_never_both_reach_agent` (L2, I06)
- [ ] `duplicate_request_id_rejection_does_not_settle_original_request` (L1/L3, I05/I20)
- [ ] `requester_local_rejection_is_not_broadcast_as_operation_failure` (L3, I05/I20)
- [ ] `stale_error_does_not_clear_newer_operation` (L1, I20)
- [ ] `out_of_order_new_responses_commit_to_their_own_intents` (L1/L2, I20)
- [ ] `each_accepted_intent_has_exactly_one_terminal_outcome` (L1, I11)
- [ ] `operation_guard_releases_on_rpc_error_timeout_and_task_abort` (L1/L2, I11)
- [ ] `rejected_intent_does_not_change_canonical_state` (L1, I11/I20)

### Snapshot, replay, and retention

- [ ] `active_snapshot_plus_suffix_equals_uninterrupted_fold` (L1/L3, I03)
- [ ] `terminal_response_retires_active_payload_without_observed_history` (L1, I03a/I08)
- [ ] `retired_session_contains_no_completed_conversation_payload` (L1, I08/I22)
- [ ] `completed_reconnect_loads_once_or_reports_history_unavailable` (L2/L3, I23)
- [ ] `load_replay_is_transactional_and_never_enters_bridge_history` (L1/L2, I08/I19/I23)
- [ ] `same_session_reload_rejection_has_no_local_fallback_or_retry` (L2, I10/I14/I23)
- [ ] `resume_marks_history_unavailable_instead_of_empty` (L1/L2, I09)
- [ ] `cancel_request_is_nonterminal_until_prompt_settles` (L1/L2, I11)
- [ ] `transport_loss_after_possible_agent_commit_becomes_uncertain` (L1/L2, I21)
- [ ] `dropping_all_raw_debug_does_not_change_active_projection` (L1, I22)
- [ ] `pre_open_updates_interactions_and_terminals_fold_into_open_session` (L1, I19)
- [ ] `turn_retirement_preserves_only_still_live_resources` (L1, I07/I08)
- [ ] `closed_session_late_update_cannot_resurrect_runtime` (L1, I12/I13)
- [ ] `creation_ghost_updates_clear_when_last_creation_settles` (L1, I19)
- [ ] `fork_never_copies_bridge_completed_history_or_live_resources` (L1/L2, I07/I09)

### Interaction lifecycle

- [ ] `permission_request_is_an_idempotent_self_contained_tool_upsert` (L1, I04)
- [ ] `permission_response_accepts_only_an_offered_option` (L2, I14)
- [ ] `two_subscribers_can_observe_but_only_one_can_resolve_permission` (L1/L3, I04)
- [ ] `permission_response_failure_restores_pending_interaction` (L1/L2, I11)
- [ ] `form_elicitation_response_is_revalidated_in_bridge` (L2, I14)
- [ ] `request_and_session_elicitation_scopes_never_cross` (L1/L2, I12)
- [ ] `url_accept_moves_pending_to_waiting_complete` (L1, I11)
- [ ] `url_complete_routes_to_origin_session_and_cleans_mapping` (L1, I12)
- [ ] `url_reject_cancel_abort_and_duplicate_complete_are_idempotent` (L1, I04/I13)
- [ ] `unknown_elicitation_mode_preserves_raw_and_safely_cancels` (L2, I15)
- [ ] `closing_one_session_cancels_only_its_interactions` (L1/L2, I12)

### Lifecycle transactions

- [ ] `successful_close_commits_non_active_before_cleanup_await` (L1/L2, I13)
- [ ] `close_success_ignores_oversized_unrelayed_response_metadata` (L2, I13/I15)
- [ ] `delete_active_session_is_atomic_close_then_delete` (L2, I10/I13)
- [ ] `disconnect_at_every_close_delete_boundary_does_not_abort_delete` (L2/L3, I10)
- [ ] `close_failure_never_sends_delete` (L2, I11)
- [ ] `close_success_delete_failure_leaves_closed_retryable_session` (L2, I11/I13)
- [ ] `delete_and_attach_are_bidirectionally_exclusive` (L1/L2, I06)
- [ ] `late_close_or_delete_response_has_one_terminal_effect` (L1/L2, I11/I13)
- [ ] `new_load_resume_and_fork_replays_commit_or_rollback_atomically` (L1/L2, I19)
- [ ] `rejected_allocation_cleanup_never_closes_existing_session` (L2, I13)

### Prompt ownership and queue

- [ ] `prompt_becomes_visible_only_after_bridge_acceptance` (L1/L3, I11)
- [ ] `accepted_follow_up_survives_all_browser_disconnects` (L1/L2/L3, I10)
- [ ] `queued_prompts_are_fifo_and_independent_per_session` (L1/L2, I06)
- [ ] `cancel_then_send_next_has_one_active_turn_at_each_revision` (L1/L2, I06/I11)
- [ ] `disconnect_mid_prompt_sends_no_cancel_close_or_restart` (L2/L3, I10)
- [ ] `prompt_error_after_partial_output_replays_partial_plus_failure` (L1/L2, I03)
- [ ] `cancel_and_prompt_complete_race_has_one_terminal_outcome` (L1/L2, I11)
- [ ] `shutdown_cancels_each_active_session_once` (L2, I11/I17)

### Hub and shutdown

- [ ] `subscribe_publish_race_forms_linearizable_snapshot_cut` (L3, I02/I03)
- [ ] `two_subscribers_reach_same_revision_and_state` (L3, I03)
- [ ] `slow_subscriber_is_evicted_without_blocking_fast_subscriber` (L3, I16)
- [ ] `snapshot_memory_is_bounded_independent_of_subscriber_count` (L3, I16)
- [ ] `shutdown_preempts_full_command_queue_during_initialize` (L3/L4, I17)
- [ ] `bridge_task_completion_finishes_generation_despite_sender_leak` (L3, I01/I17)
- [ ] `restart_rejects_old_generation_commands` (L3, I01)
- [ ] `fatal_connection_path_runs_terminal_mcp_auth_and_process_cleanup` (L2/L4, I17)

## ACP surface ledger

Every row needs success, Agent error, invalid/unsupported input, reconnect, and resource cleanup where
applicable. `[N]` is ACP-required behavior, `[C]` is a compatibility policy, and `[P]` is an attyd
product/experimental policy.

| Surface | Required cases |
| --- | --- |
| `initialize` [N] | v1 negotiation, clientInfo/capabilities, null/omitted defaults, wrong version, duplicate IDs, bounded values, error phase |
| authenticate/logout [N] | exact advertised methods, auth-required retry state, global serialization, request-scoped elicitation, error data |
| terminal auth [N/P] | stdio-only advertisement, argv/env privacy, input/resize/cancel, nonzero/signal, output bound, restart policy with active turns |
| new [N] | cwd/roots/MCP, early updates, concurrent creations, duplicate/invalid ID, safe rejected-allocation close |
| list [N] | cwd transport rule, pagination, duplicate upsert, cyclic cursor, stale refresh, invalid metadata without state loss |
| load/resume [N] | independent capability gates, pre-response replay, atomic commit/rollback, active/attach/delete conflicts |
| fork [P] | distinct ID, no/full/incremental Agent replay, source lock, normalized inheritance without live resources |
| close/delete [N] | capability gate, cleanup, timeout/late response, close-before-delete transaction, retryable partial failure |
| set mode/config [N] | offered value, dynamic Agent update, complete response authority, stale/failed/overlapping mutation |
| prompt/cancel [N] | all content blocks, five stop reasons, usage, partial output/error, cancellation race, per-session concurrency |
| permission [N] | self-contained tool patch, offered options, duplicate response, Agent/session cancellation, eviction/reconnect |
| form elicitation [N] | supported schema primitives, defaults/formats/unions, all JSON-RPC ID shapes, server-side response validation |
| URL elicitation [N] | accept/complete, reject/cancel, HTTP(S), unique ID, reuse/duplicate/early complete, scoped abort/reconnect |
| terminal [N] | create after spawn, output/wait/kill/release, ENOENT fallback, bounds, owner isolation, waiter cancellation |
| MCP [P] | current connections, inner request scope, notification, cancellation, duplicate/late response, provider exit, disconnect |
| NES/document [P exclusion] | never advertised, browser intent rejected before Agent I/O, unsolicited capability creates no editor state |
| unknown/optional [N/C] | `_meta` opaque, unknown fields retained, malformed optional follows SDK defaults, unknown discriminant is one bounded diagnostic |

### `session/update` exhaustive parameterized ledger

Each supported variant is exercised for active, background, pending load/resume, unknown/closed, and
reconnected sessions. Invalid payloads must not end the active prompt and `_meta` remains debug-only.

- [ ] `user_message_chunk`
- [ ] `agent_message_chunk` across all five content block kinds
- [ ] `agent_thought_chunk`
- [ ] `tool_call`
- [ ] sparse/missing-start `tool_call_update`
- [ ] legacy `plan`
- [ ] experimental ID `plan_update`
- [ ] experimental `plan_removed`
- [ ] `available_commands_update`
- [ ] dynamic `current_mode_update`
- [ ] complete `config_option_update`
- [ ] `session_info_update`
- [ ] `usage_update`
- [ ] experimental `compaction_update`
- [ ] experimental `compaction_summary_chunk`

Tool coverage is further parameterized across content/diff/terminal/raw payloads, every status and
tool kind, duplicate start, update-before-start, cross-session terminal reference, and late update.

## P1 transport and resource tests

Run the same scripted business trace through stdio, Streamable HTTP/SSE, and WebSocket. Only local
filesystem, terminal, Agent-process, and terminal-auth capabilities may differ.

- [ ] `transport_matrix_two_sessions_run_concurrently`
- [ ] `transport_matrix_browser_disconnect_preserves_turn`
- [ ] `transport_matrix_active_reconnect_restores_only_the_running_turn`
- [ ] `transport_matrix_completed_reconnect_loads_or_reports_history_unavailable`
- [ ] `transport_matrix_turn_retirement_leaves_no_backend_history`
- [ ] `transport_matrix_close_delete_is_linearizable`
- [ ] `transport_matrix_eof_settles_all_operations`
- [ ] `transport_matrix_late_old_generation_response_is_ignored`
- [ ] `stdio_disconnect_preserves_agent_pid`
- [ ] `stdio_shutdown_cancels_then_kills_agent_process_group`
- [ ] `stdio_fatal_after_terminal_creation_cleans_all_children`
- [ ] `stdio_hung_initialize_obeys_shutdown_deadline`
- [ ] `remote_terminal_filesystem_and_terminal_auth_are_method_not_found`

## P2 model/property suite

Use `proptest` with small ID domains to force collisions and preserve failing seeds. Generate 1-200
steps with approximately 25% turn/update, 20% interaction, 15% mutation, 10% lifecycle, 10%
duplicate/stale/failure, 10% subscriber churn, and 10% capacity pressure.

- [ ] `model_matches_production_transition_for_every_generated_action`
- [ ] `active_reconnect_after_every_active_prefix_is_observationally_equivalent`
- [ ] `completed_reconnect_after_every_terminal_prefix_is_load_or_unavailable`
- [ ] `subscriber_stutter_invariance`
- [ ] `distinct_session_permutation_invariance`
- [ ] `same_session_agent_trace_is_linearizable`
- [ ] `duplicate_retry_never_executes_mutation_twice`
- [ ] `turn_retirement_preserves_live_projection_and_erases_conversation`
- [ ] `no_operation_or_resource_survives_closed_deleted_session`
- [ ] `fault_at_every_await_boundary_preserves_invariants`
- [ ] `cleanup_leaves_no_responder_waiter_task_or_process`

Invalid actions are generated deliberately. The reference model returns Accepted/Rejected, and a
rejection must leave canonical state unchanged. Random operation and interaction IDs must be
injectable or alpha-renamed so shrinking remains deterministic.

## Fixture work

- [ ] Rust/channel `ScriptedAgent` with explicit response, notification, error, and transport gates
- [ ] raw NDJSON/HTTP/WS Agent for unknown discriminants, malformed optional fields, duplicate JSON-RPC IDs, and late responses
- [ ] persistent Agent fixture that survives bridge process restart for list/load authority tests
- [ ] fork fixture modes: no replay, full replay, incremental replay
- [ ] dual-session barrier fixture
- [ ] slow/reordering/gap subscriber harness
- [ ] injectable runtime and queue limits
- [ ] controlled MCP provider fixture
- [ ] controlled PTY/process-group fixture
- [ ] normalized snapshot oracle independent of WebSocket and React
- [ ] pinned Goose compatibility canary; later add other Agent implementations as non-blocking canaries

## TDD gates

1. **Model gate:** canonical types and transitions exist; every P0 L1 test is green.
2. **Adapter gate:** every ACP method/update ledger row is classified and L2 success/error/cancel is green.
3. **Hub gate:** revision, snapshot cut, direct result routing, bounded subscribers, and shutdown tests are green.
4. **Transport gate:** the stdio/HTTP/WS P1 matrix is green and Goose canary passes.
5. **Frontend switch gate:** only after gates 1-4, replace browser reconstruction with active
   snapshot/delta and transactional load replacement rendering.
6. **Removal gate:** prove terminal retirement and load/`HistoryUnavailable` behavior, then delete all
   completed RuntimeState, canonical, delivery-projection, delta-journal and cumulative terminal
   paths while preserving their bounded active-only equivalents.

Coverage percentage is secondary. Acceptance requires 100% classification of ACP methods,
`SessionUpdate` variants and `ContentBlock` variants; success/error/cancel/reconnect coverage for every
state transition; all replay-prefix properties green; bounded slow-subscriber behavior; and the
multi-session/request-collision/close-delete adversarial cases green.

## Remaining gates after the P0 switch

These are intentionally explicit; a green pure-state test is not treated as end-to-end proof:

- canonical snapshot/delta remains a shadow verifier in React; active browser reconstruction still
  uses the bounded `ActiveRuntimeProjection`, so a later simplification can remove one active-only
  representation after terminal-output snapshot semantics move to the canonical stream;
- same-session load is single-flight per session, but reconnect waiters do not yet join an existing
  load or use a rate limiter;
- prompt queues and stdio shutdown ordering are production-wired, but the complete close/active-limit
  fault matrix still needs transport-level tests;
- L4 transport equivalence and the pinned Goose compatibility canary must pass before the frontend
  canonical authority switch;
- the independent reference-model/property suite and await-boundary fault injection harness remain
  to be implemented;
- production logical-memory counters and long-running RSS/FD/task soak gates remain follow-up work.

These are migration gates, not reasons to weaken the invariants above.
