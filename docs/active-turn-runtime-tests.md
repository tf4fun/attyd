# Active-turn-only TDD plan

This plan is executable acceptance evidence for [active-turn-runtime.md](active-turn-runtime.md).
Production behavior is not changed until the corresponding red tests exist. Tests use injectable
small limits and deterministic `Barrier`, `Notify`, oneshot and paused-time gates; correctness tests
must not depend on wall-clock sleeps.

## Test layers

| Layer | Subject | Harness |
| --- | --- | --- |
| L1 | active-turn reducer, accounting and live-resource registries | pure Rust state with injected limits and drop probes |
| L2 | ACP adaptation, prompt terminal paths and load | scripted Rust/fake Agent with request and notification gates |
| L3 | Hub, active bootstrap, final linearization and bounded delivery | in-memory subscribers and explicit ACK/snapshot gates |
| L4 | stdio, HTTP/SSE, WS and subprocess cleanup | production binary with raw/scripted Agents and PTYs |
| L5 | browser replacement generations and active projection | reducer tests plus Playwright against the production backend |
| L6 | resource regression | deterministic counters plus manual/CI soak and parent/child RSS sampling |

Coverage percentages are secondary. Every invariant and transition below must have a named test at
the lowest useful layer plus transport/browser evidence where integration changes its behavior.

## Implemented P0 proof set

The first implementation slice is intentionally smaller than the complete matrix below. These are
executable tests, not planned names:

| Invariant | Executable evidence |
| --- | --- |
| completed turns leave no bridge history | `ten_thousand_terminal_turns_leave_no_folded_updates_or_payload_deltas`, `new_subscriber_receives_no_completed_history`, `completed_idle_session_shell_does_not_suppress_authoritative_reload` |
| active updates are bounded and folded | `active_turn_folds_only_adjacent_compatible_text_chunks_but_publishes_raw_deltas`, `active_turn_deep_merges_tool_updates_at_the_first_position_without_changing_start_kind`, `active_turn_replaces_plan_slots_in_place_and_removal_clears_only_its_slot`, `rejected_fold_replacements_leave_retained_state_revision_and_deltas_unchanged` |
| canonical active reconnect uses the same fold | `canonical_projection_folds_typed_snapshot_and_contiguous_deltas`, `subscriber_bootstrap_contains_atomic_canonical_snapshot_and_suffix` |
| live session opens are not overwritten by old snapshots | `live_session_open_is_not_followed_by_a_redundant_runtime_snapshot`, UI smoke fork early-update assertions |
| same-session load is atomic and best effort | `same_session_reload_success_does_not_create_a_second_business_session`, `same_session_reload_error_remains_actionable_and_does_not_loop`, `rejected_same_session_load_rolls_back_and_following_prompt_runs` |
| load replay is private and non-authoritative | `real_same_session_load_uses_tracked_cwd_and_isolates_other_subscribers`, `load_replacement_is_private_and_never_becomes_completed_replay`, browser reducer load commit/error/disconnect cases |
| terminal and URL resources retire | `released_terminal_is_not_retained_or_resurrected`, `terminal_chunks_merge_once_and_release_drops_all_live_output`, `settled_url_flows_are_removed_from_live_state_but_remain_idempotent`, `aborted_url_elicitation_is_removed_from_live_replay` |
| queues have count and byte bounds | `queue_enforces_item_and_byte_limits_and_cancels_on_saturation`, `subscriber_backlog_is_bounded_by_bytes_not_only_event_count`, byte-lease teardown tests |

The remaining entries are the forward test backlog. In particular, RSS soak, every-transport fault
injection, a reference-model property suite, and load single-flight/rate limiting are not claimed by
the P0 proof set.

## Production test probes

Tests consume production accounting rather than serializing state to estimate its size:

```text
RuntimeMemoryStats
|- tracked_sessions
|- active_turns / active_turn_bytes / active_turn_items
|- queued_prompts / queued_prompt_bytes
|- live_permissions / bytes
|- live_elicitations / bytes
|- live_url_flows / bytes
|- live_terminals / output_bytes
|- bridge_queue_items / bytes / high_water
|- subscriber_items / bytes / high_water
|- active_snapshots / suffix_bytes
|- reloads / reload_delivery_bytes
`- in_flight_event_bytes
```

Test-only limits use production limit types. A `DropProbe`/`Weak` handle proves that an active turn or
resource is unreachable after cleanup. Queue tests hold real byte permits until the consumer or
failed send releases them.

## L1 active turn and accounting

### Admission and terminal retirement

- `one_active_turn_per_session_and_distinct_sessions_commute`
- `start_reserves_per_session_and_global_turn_bytes_transactionally`
- `failed_start_leaves_state_and_global_reservation_unchanged`
- `terminal_response_drops_prompt_updates_indices_and_operation_payload`
- `prompt_error_drops_all_turn_payload_after_terminal_delivery`
- `cancel_is_nonterminal_until_prompt_response_then_drops_the_turn`
- `transport_loss_produces_one_uncertain_terminal_then_drops_the_turn`
- `retired_session_contains_no_completed_conversation_payload`
- `new_turn_can_reuse_prior_message_and_tool_ids`
- `late_update_or_terminal_for_old_turn_key_cannot_mutate_the_next_turn`
- `duplicate_terminal_is_idempotent_and_does_not_underflow_accounting`
- `sequential_completed_turns_leave_active_and_completed_payload_bytes_at_zero`

### Semantic folding and coalescing

- `message_and_thought_chunks_append_in_protocol_order`
- `adjacent_compatible_chunks_coalesce_without_changing_rendered_order`
- `interleaved_message_tool_and_plan_items_keep_first_appearance_order`
- `tool_updates_fold_by_id_without_retaining_prior_full_values`
- `partial_tool_patches_merge_before_keyed_coalescing`
- `invalid_tool_status_regression_is_transactional`
- `plan_and_control_updates_replace_by_semantic_key`
- `plan_removal_releases_item_and_byte_accounting`
- `ten_thousand_same_key_replacements_keep_item_count_constant`
- `failed_replacement_preserves_value_order_and_accounting`
- `terminal_reference_retains_only_identity_not_output`
- `turn_terminal_drops_all_turn_scoped_semantic_state`

### Hard limits

- `active_turn_exact_byte_limit_succeeds_and_plus_one_is_atomic`
- `active_turn_item_limit_blocks_many_tiny_events`
- `identifier_count_and_length_limits_bound_semantic_indices`
- `global_turn_limit_is_shared_across_sessions_and_reused_after_retirement`
- `queued_prompt_count_and_bytes_are_bounded_per_session_and_globally`
- `checked_accounting_rejects_integer_overflow`
- `buffer_pressure_reserves_terminal_metadata_and_requests_cancel`
- `thirty_two_session_stress_never_exceeds_global_accounting`

### Intent and control metadata

- `in_flight_intent_keeps_fixed_digest_not_full_command`
- `same_intent_and_digest_is_duplicate_while_collision_is_rejected`
- `terminal_transition_removes_intent_and_result_payload`
- `uncertain_tombstone_contains_identity_and_digest_only`
- `sequential_completed_intents_do_not_grow_intent_maps`

## L1 live resources

### Permission, elicitation and URL

- `permission_count_and_byte_limits_are_transactional`
- `permission_resolution_or_cancel_removes_payload_and_reservation`
- `turn_scoped_interaction_is_cancelled_at_turn_terminal`
- `request_scoped_interaction_survives_unrelated_turn_terminal`
- `late_responder_completion_cannot_resurrect_removed_interaction`
- `elicitation_count_and_byte_limits_are_transactional`
- `url_accept_moves_accounting_from_elicitation_to_live_flow_once`
- `url_complete_cancel_or_abort_removes_the_flow_immediately`
- `live_url_flow_can_cross_turn_without_entering_history`
- `ten_thousand_resolved_interactions_leave_live_maps_at_zero`

### Terminal

- `terminal_output_chunks_append_once_and_emit_incremental_payloads`
- `terminal_n_bytes_produce_linear_internal_and_public_bytes`
- `terminal_ring_and_global_output_limits_are_enforced`
- `terminal_state_coalescing_keeps_one_pending_keyed_update`
- `turn_retirement_does_not_release_a_live_terminal`
- `terminal_release_removes_manager_runtime_output_and_accounting`
- `released_terminal_is_not_replayed_or_tombstoned_with_output`
- `late_terminal_output_for_old_resource_token_is_ignored`
- `one_thousand_create_output_release_cycles_return_to_zero`
- `session_close_and_generation_shutdown_release_owned_terminals`

## L2 prompt and load adaptation

### Prompt lifecycle

- `updates_after_cancel_continue_until_the_matching_prompt_response`
- `prompt_error_after_partial_output_has_one_ephemeral_terminal_outcome`
- `transport_loss_after_possible_dispatch_is_uncertain_not_retried`
- `old_epoch_prompt_response_cannot_retire_a_current_turn`
- `queued_prompt_does_not_start_until_prior_turn_is_retired`
- `different_sessions_can_prompt_and_retire_independently`

### Completed reconnect without load

- `idle_reconnect_without_load_returns_history_unavailable_without_agent_io`
- `active_reconnect_without_load_uses_only_current_active_state`
- `after_active_retirement_reconnect_no_longer_replays_local_messages`
- `resume_is_never_used_as_completed_history_replay`

### Completed reconnect with load

- `completed_reconnect_starts_one_capability_gated_session_load`
- `concurrent_reconnects_share_one_same_session_load`
- `reconnect_flapping_is_singleflight_and_rate_limited`
- `same_session_reload_success_does_not_create_a_second_business_session`
- `same_session_reload_rejection_is_reported_without_close_restart_or_retry`
- `load_and_prompt_are_mutually_exclusive_for_one_session`
- `loads_for_distinct_sessions_can_progress_concurrently`
- `load_early_updates_are_scoped_to_one_replacement_generation`
- `valid_load_response_commits_replay_only_to_waiting_subscribers`
- `load_error_invalid_replay_and_transport_loss_expose_no_partial_replacement`
- `empty_agent_replay_is_marked_unverified_not_invented_as_history`
- `oversized_replay_drains_to_response_then_returns_history_unavailable`
- `last_waiter_disconnect_cancels_or_drains_and_discards_load`
- `load_completion_from_an_old_epoch_or_incarnation_is_ignored`
- `load_completion_leaves_backend_replay_bytes_at_zero`

## L3 Hub and subscriber linearization

### Active bootstrap

- `active_snapshot_plus_suffix_equals_uninterrupted_fold`
- `update_during_snapshot_is_delivered_once_after_the_barrier`
- `final_during_snapshot_is_the_last_catchup_event`
- `snapshot_timeout_or_disconnect_releases_lease_and_suffix_bytes`
- `snapshot_suffix_overflow_evicts_only_that_subscriber`
- `two_subscribers_have_independent_barriers_and_byte_ledgers`

### Final boundary

- `terminal_response_enters_sealing_until_hub_commit_ack`
- `subscribe_before_final_linearization_gets_active_snapshot`
- `subscribe_after_final_linearization_never_reads_active_state`
- `hub_commit_ack_retires_and_drops_active_state_exactly_once`
- `duplicate_final_commit_is_idempotent`
- `final_without_subscribers_still_retires_the_turn`
- `slow_or_disconnected_subscriber_does_not_delay_retirement`
- `new_prompt_waits_while_final_is_committed_pending_retire`
- `hub_failure_uses_raii_cleanup_without_retaining_history`

### Queue bounds

- `bridge_to_hub_queue_is_bounded_by_items_and_bytes`
- `control_and_final_lane_cannot_be_starved_by_bulk_output`
- `subscriber_count_and_aggregate_byte_budgets_are_global`
- `broadcast_shares_immutable_payload_instead_of_deep_cloning_per_subscriber`
- `failed_send_drop_and_receiver_teardown_release_byte_permits`
- `new_subscriber_receives_no_completed_history_or_initial_event_vector`

## L4 transport and resource matrix

Run the common cases through stdio, Streamable HTTP/SSE and WebSocket where the capability exists:

- `transport_matrix_active_reconnect_restores_only_the_running_turn`
- `transport_matrix_completed_reconnect_loads_or_reports_history_unavailable`
- `transport_matrix_turn_retirement_leaves_no_backend_history`
- `transport_matrix_two_sessions_run_concurrently_with_global_limits`
- `transport_matrix_late_old_generation_event_is_ignored`
- `transport_matrix_load_error_does_not_poison_following_prompt`
- `transport_matrix_shutdown_releases_active_turns_and_live_resources`

Stdio-only resource cases:

- `terminal_release_kills_descendants_holding_stdout_open`
- `stdio_fatal_path_runs_terminal_mcp_auth_and_agent_process_cleanup`
- `stdio_shutdown_uses_per_resource_deadlines_and_raii_fallback`
- `agent_stdout_and_mcp_stderr_floods_plateau_at_queue_limits`

## L5 browser behavior

- `active_reconnect_renders_snapshot_then_suffix_without_duplicate_items`
- `active_reconnect_crossing_final_switches_to_load_or_history_unavailable`
- `load_replace_begin_stages_without_mutating_visible_session`
- `load_replace_commit_atomically_replaces_the_session`
- `load_replace_failure_discards_staging_and_shows_precise_reason`
- `load_unsupported_shows_history_unavailable_not_an_empty_authoritative_thread`
- `same_session_reload_error_remains_actionable_and_does_not_loop`
- `current_page_keeps_its_rendered_completed_messages_until_page disposal`
- `refresh_never_uploads_browser_history_to_the_bridge`
- `released_terminal_missing_from_agent_replay_renders_output_unavailable`
- `reconnect_storm_produces_at_most_one_load_request_per_session`

## L6 regression and soak

Deterministic logical-memory tests are required in CI:

- 10,000 small sequential turns: after each terminal transition, completed payload bytes are zero.
- 1,000 near-limit sequential turns: retained logical bytes return to the same baseline.
- 32 concurrent sessions repeated through start/update/terminal: global reservations return to zero.
- 1,000 terminal create/output/release cycles: terminal count/output bytes return to zero.
- large Agent load with slow browser: bridge queues plateau; failure exposes no partial replacement.
- reconnect storm: load request count is single-flight/rate-limited and task/FD counts plateau.

Parent attyd RSS, Agent child/process-tree RSS, queue high-water, active bytes and live-resource bytes
are sampled separately in the Goose canary. Allocator RSS may remain at a warm high-water mark, but
the post-warm-up slope must not track completed-turn count. Suggested acceptance is less than
1 MiB/hour over a two-hour idle soak after warm-up, with logical retained counters stable.

The pinned Goose canary repeats at least 100 cycles of:

```text
prompt -> terminal -> browser reconnect -> same-session load -> prompt
```

It must show no monotonic growth in session count, MCP/terminal count, tasks, file descriptors,
queued bytes or logical retained bytes. A Goose reload rejection is an accepted compatibility result
only when attyd reports it honestly and remains usable for a subsequent explicit operation.

## Property and fuzz suite

- `generated_active_sequences_preserve_single_turn_and_accounting_invariants`
- `failed_generated_transition_is_transactional`
- `generated_stale_epoch_incarnation_and_operation_events_are_inert`
- `generated_resource_lifecycles_never_exceed_or_leak_reservations`
- `generated_subscriber_churn_never_underflows_or_exceeds_byte_ledgers`
- `generated_multisession_interleavings_preserve_per_session_linearizability`
- `cleanup_after_every_generated_prefix_returns_all_owned_reservations`

## Legacy tests that must change or disappear

Tests asserting any of these behaviors are migration tests for the old model, not acceptance tests:

- active turn moves into `observed_turns`;
- completed history remains in `TranscriptBaseline`, `AgentReplay`, `ClientDerivedFork` or
  `RuntimeCache`;
- a Hub/Canonical full snapshot reconstructs completed browser history;
- completed reconnect performs zero Agent calls;
- fork copies bridge-observed completed history;
- released terminal snapshots remain replayable;
- session-event journals retain bounded completed history.

They are replaced by retirement-to-zero, Agent load/`HistoryUnavailable`, active-only snapshot and
live-resource tests above. Browser reducer tests for messages already rendered in the current page
remain valid; they do not establish bridge authority.

## Red-to-green and removal gates

1. Add accounting/drop probes and red L1 retirement tests.
2. Add red terminal linearity and live-resource cleanup tests.
3. Add red L2 completed reconnect and load single-flight tests.
4. Add red L3 final/snapshot race and byte-bounded channel tests.
5. Implement active-only state and make L1-L3 green without deleting old read paths.
6. Add and pass L4/L5 behavior tests against the new protocol.
7. Switch production/browser reads away from completed RuntimeState/Canonical/RuntimeCache data.
8. Delete all legacy history fields, replay vectors, full-session deltas and cumulative terminal
   snapshots plus their now-invalid tests.
9. Run the full unit, integration, browser, transport, fuzz and soak gates.
