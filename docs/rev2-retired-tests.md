# Revision 2 tests retired by the cutover

The revision 2 engine (publisher, hub, `/api/v2/stream`, `/api/v2/scopes/…`) is removed in steps
(docs/state-sync.md §7.5). Each step deletes the tests that could only run on it. This ledger lists
them, with the commit that still holds them, so that the cases that matter on revision 3 are
restored from there rather than rewritten from memory. A case is *restored* when a revision 3 test
checks the same behavior; the pull request that does so ticks it here.

## Bridge gate: hub streams (`src/bridge/protocol_gate_tests/stream.rs`)

Retired with the port of the gate to the feed. Read them at `e4033d6`. They drive the Hub's
`Subscription` and check the S6 hold/release rules, loads started by subscriptions and the
reconnection of a stream; on revision 3 the same rules run behind `FeedSubscription`.

- [ ] a_stream_aligns_each_scope_at_once_and_follows_a_prompt_through_its_commit
- [x] a_held_session_ignores_the_load_directory (`protocol_gate_tests/loads.rs`)
- [ ] a_cold_reopen_keeps_the_client_log_and_confirms_it_when_the_load_ends
- [ ] a_cold_reopen_with_a_changed_history_replaces_the_client_log
- [x] two_subscriptions_share_one_load_and_each_gets_its_log_from_its_own_head → two_subscriptions_share_one_load_and_both_end_with_the_replayed_entries (`protocol_gate_tests/loads.rs`)
- [x] reconnecting_before_during_and_after_a_load_keeps_one_load_and_converges (`protocol_gate_tests/loads.rs`)
- [x] an_unheld_session_without_cwd_or_whose_load_fails_ends_with_load_failed (`protocol_gate_tests/loads.rs`)
- [ ] a_paused_stream_follows_its_tail_and_is_coalesced_past_its_budget
- [x] subscribers_hold_a_loadable_session_and_the_last_one_leaving_releases_it (`protocol_gate_tests/holding.rs`)
- [x] a_loadable_session_without_close_is_released_locally (`protocol_gate_tests/holding.rs`)
- [x] a_session_released_before_its_first_subscription_is_loaded_by_it (`protocol_gate_tests/holding.rs`)
- [x] a_session_the_agent_can_neither_load_nor_resume_stays_held (`protocol_gate_tests/holding.rs`)
- [x] a_session_the_agent_can_only_resume_is_released (`protocol_gate_tests/holding.rs`)
- [x] an_accepted_url_flow_alone_keeps_the_session_needed (`protocol_gate_tests/holding.rs`)
- [x] an_exited_terminal_not_yet_released_keeps_the_session_needed (`protocol_gate_tests/holding.rs`)
- [x] a_new_subscription_or_an_occupation_cancels_the_release_timer (`protocol_gate_tests/holding.rs`)
- [x] review_a_sessions_own_admitted_operation_keeps_it_held_before_dispatch (`protocol_gate_tests/holding.rs`)
- [x] review_a_subscription_after_delete_admission_waits_for_its_end (`protocol_gate_tests/loads.rs`)
- [x] a_subscription_waits_for_a_cold_delete_and_ends_with_deleted (`protocol_gate_tests/loads.rs`)
- [x] a_subscription_waiting_for_a_failed_cold_delete_loads_the_session (`protocol_gate_tests/loads.rs`)
- [x] the_next_connection_s_equal_replay_ends_the_waiting_load → the_next_connections_equal_replay_ends_the_waiting_load (`protocol_gate_tests/reconnection.rs`)
- [x] the_next_connection_s_diverging_replay_rebuilds_from_the_common_prefix → the_next_connections_diverging_replay_replaces_what_differs (`protocol_gate_tests/reconnection.rs`)
- [x] review_an_unfollowed_session_is_released_when_the_agent_connection_ends (`protocol_gate_tests/reconnection.rs`)
- [x] a_session_the_agent_cannot_load_again_ends_with_load_failed_at_the_connection_end (`protocol_gate_tests/reconnection.rs`)
- [ ] four_consumers_converge_one_by_one_lagging_reconnecting_and_paused
- [x] review_resume_only_session_waits_across_eof_and_preserves_its_known_log → …known_entries (`protocol_gate_tests/reconnection.rs`)
- [x] review_waiting_session_uses_the_new_connections_resume_capability (`protocol_gate_tests/reconnection.rs`)
- [x] review_refused_automatic_close_waits_for_a_new_subscription_cycle (`protocol_gate_tests/reconnection.rs`)
- [x] a_new_sessions_early_controls_survive_its_creation_response (`protocol_gate_tests/reconnection.rs`)

## Bridge gate: tests rewritten in place

These stay in `src/bridge/protocol_gate_tests.rs` with revision 3 expectations (entries and
prompts instead of the turn log and the process): `a_streamed_prompt_with_an_answered_permission_ends_in_one_event`,
`a_failed_prompt_keeps_what_streamed_and_ends_with_its_error`,
`attachments_are_served_while_the_entries_that_reference_them_are_published`,
`the_same_history_gives_the_same_entries_after_a_restart`, and the gate's own counterexamples
(`the_gate_rejects_an_entry_change_without_its_event`,
`the_atomic_check_rejects_a_prompt_that_ends_before_its_operation`).

## Server streams and reads (`src/server/stream_tests.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. The revision 2 stream and scope reads over HTTP: alignment, `have`, resumption across Agent EOF, recovery and login restart. `stream_tests/feed.rs` covers the revision 3 stream, loads and the next connection; the recovery cases below are to be restored there.

- [ ] reads_return_one_cut_hold_nothing_and_tell_an_unknown_log_from_an_empty_one
- [ ] a_stream_aligns_several_scopes_without_ids_and_heartbeats_while_idle
- [ ] have_selects_the_suffix_and_a_mismatched_or_longer_head_gets_the_whole_log
- [ ] each_session_uses_its_own_have_and_cwd_on_a_multi_scope_stream
- [ ] an_invalid_stream_query_is_refused
- [ ] after_an_agent_eof_reads_serve_the_last_documents_and_a_global_stream_restarts_nothing
- [ ] a_session_stream_waits_through_an_agent_eof_and_converges_on_the_next_connection
- [ ] a_session_stream_applies_a_diverging_replay_after_an_agent_eof
- [ ] a_successful_terminal_login_starts_the_next_connection_for_a_global_only_stream
- [ ] a_new_global_stream_aligns_while_initialize_is_unanswered
- [ ] a_known_session_aligns_on_a_new_stream_while_the_next_initialize_is_unanswered
- [ ] an_initialization_failure_keeps_recovering_while_a_session_stream_needs_it
- [ ] recovery_continues_for_a_remaining_subscriber_and_stops_after_the_last_leaves
- [ ] a_cold_load_cut_by_an_eof_keeps_the_cache_and_converges_on_the_next_connection
- [ ] a_session_ended_with_load_failed_is_no_recovery_need_behind_a_global_stream
- [ ] a_login_restart_is_consumed_and_the_next_ordinary_eof_starts_nothing
- [ ] review_a_subscription_crossing_generations_loads_on_the_current_agent
- [ ] review_shutdown_during_initialize_publishes_a_final_agent_before_closing_streams
- [ ] review_an_http_transport_construction_error_is_published_to_v2
- [ ] review_a_cold_subscription_registered_during_initialize_is_served_after_reloads

## Writes over HTTP (`src/server/stream_tests/ops/actions.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [x] a_permission_is_answered_alone_and_at_most_once
- [x] an_interaction_that_does_not_exist_is_not_found
- [x] a_mode_is_set_when_the_agent_answers_and_the_answer_is_the_response
- [x] an_agent_error_is_the_response_and_changes_nothing
- [x] a_boolean_option_is_set_and_one_not_offered_is_refused_before_the_agent_hears
- [x] a_control_runs_while_a_prompt_runs
- [x] a_control_of_a_session_that_is_not_held_is_not_found
- [x] a_session_is_closed_when_the_agent_closes_it_and_leaves_the_bridge
- [x] an_agent_error_leaves_the_session_held_and_not_closing
- [x] an_agent_that_does_not_advertise_close_is_refused_as_unsupported
- [x] closing_during_a_prompt_answers_its_pending_permissions_cancelled

Restored in `src/server/stream_tests/actions.rs`.

## Writes over HTTP (`src/server/stream_tests/ops/admission.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [x] session_writes_need_the_current_ended_op_and_replace_it
- [x] global_writes_need_the_current_ended_op_and_replace_it
- [x] a_fork_is_refused_while_a_global_operation_runs
- [x] a_global_write_is_refused_while_a_fork_runs
- [x] a_write_with_a_client_id_is_installed_under_that_id
- [x] a_malformed_client_id_is_refused_before_anything_happens
- [x] a_repeated_write_is_accepted_again_and_does_nothing_more
- [x] a_write_that_is_not_a_repeat_is_judged_by_expect_and_status_as_before
- [x] a_list_needs_the_current_ended_catalog_op
- [x] of_two_writes_with_one_expect_only_one_is_admitted
- [x] a_refused_write_can_be_submitted_again_on_the_same_expect
- [x] refusals_follow_the_admission_order
- [x] without_a_connection_writes_are_unavailable_after_expect_and_arguments
- [x] malformed_and_foreign_writes_are_refused_with_the_protocol_error_body
- [x] a_session_id_with_slashes_is_written_through_its_encoded_scope

Restored in `src/server/stream_tests/bench/admission.rs`, on the revision 3 bench (`bench.rs`).

## Writes over HTTP (`src/server/stream_tests/ops/execution.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [x] a_prompt_answers_202_with_its_op_and_commits_its_result
- [x] create_fork_and_list_answer_with_their_result_or_error_when_they_end
- [x] a_fork_uses_its_explicit_cwd_and_otherwise_the_source_cwd
- [~] a_fork_response_survives_its_replacement_by_an_agent_request — not applicable: there is no held response: the result is in the document while the operation is the scope's current one, and a client follows the stream (protocol §6.6)
- [~] a_create_response_survives_its_replacement_and_a_late_reader_converges — not applicable: there is no held response; a late reader converges through the stream's snapshot (covered by the replica and engine tests)
- [x] a_new_sessions_early_messages_are_its_log_for_reads_and_every_subscription → a_new_sessions_early_messages_are_its_entries_for_reads_and_every_subscription
- [x] admitted_create_fork_and_list_keep_their_errors_when_the_agent_disconnects
- [x] a_dropped_create_response_does_not_withdraw_the_create → a_create_runs_to_its_end_without_its_response_being_read
- [x] prompt_attachment_references_are_resolved_and_an_unknown_one_is_refused
- [x] review_setup_config_options_are_used_for_admission_and_execution
- [x] control_acknowledgements_fold_into_the_ending_event_and_failures_change_nothing
- [x] a_list_returns_its_page_and_a_stale_revision_is_catalog_changed

Restored in `src/server/stream_tests/bench/execution.rs` (`[~]`: not applicable on revision 3, with the reason).

## Writes over HTTP (`src/server/stream_tests/ops/interactions.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [x] interactions_of_create_fork_set_config_and_list_are_answered_in_their_scope
- [x] a_load_interaction_is_answered_in_the_loading_session
- [x] an_agent_request_without_a_running_op_starts_one_that_completes_with_its_last_interaction
- [x] an_answer_is_refused_whole_unless_it_covers_exactly_the_pending_interactions
- [x] a_prompt_cancel_asks_the_agent_once_and_keeps_the_session_occupied
- [x] a_cancelled_agent_request_answers_its_interactions_with_their_cancel_outcomes
- [x] other_kinds_refuse_a_cancel

Restored in `src/server/stream_tests/bench/interactions.rs`.

## Writes over HTTP (`src/server/stream_tests/ops/login_terminals.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] a_login_terminal_takes_input_in_order_and_is_dismissed_after_it_exits
- [ ] a_signalled_login_completes_and_a_dismissal_removes_only_its_terminal
- [ ] a_successful_login_completes_and_a_global_only_stream_sees_the_next_connection

## Writes over HTTP (`src/server/stream_tests/ops/long_requests.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] create_fork_and_list_responses_outlive_the_json_deadline

## Writes over HTTP (`src/server/stream_tests/ops/occupation.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] a_held_sessions_prompt_and_its_delete_are_never_both_admitted
- [ ] a_close_interaction_is_answered_through_either_scope_that_shows_it
- [ ] a_failed_held_delete_keeps_its_error_and_the_subscription
- [ ] a_subscription_during_the_first_load_keeps_its_recovery_directory
- [ ] a_new_subscription_after_a_partially_failed_delete_still_needs_cwd
- [ ] a_cold_delete_holds_back_the_load_until_it_ends

## Writes over HTTP (`src/server/stream_tests/ops/reads.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] attachments_are_served_while_their_own_session_references_them
- [ ] a_compaction_summary_block_is_served_until_its_turn_commits
- [ ] reads_hold_nothing_and_a_released_session_serves_no_attachment
- [ ] context_reads_follow_each_sessions_own_workspace

## Writes over HTTP (`src/server/stream_tests/ops/retired_v1.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] v1_reads_writes_and_event_streams_are_unknown_routes_that_reach_no_agent

## Writes over HTTP (`src/server/stream_tests/ops/review_terminal_io.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] terminal_input_backpressure_does_not_block_its_output_observer

## Writes over HTTP (`src/server/stream_tests/ops/url_flows.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Drove `POST /api/v2/scopes/…` and the revision 2 stream through a per-event gate; revision 3 answers every operation `202` and shows the result in the document, so these are restored with that bench.

- [ ] flows_outlive_their_operation_and_end_by_their_own_lifecycle
- [ ] a_reused_elicitation_id_names_a_new_flow
- [ ] the_connection_end_clears_the_global_flow

## Hub (`src/protocol/hub_tests.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. The revision 2 Hub: queues, coalescing, heartbeat. The feed has its own (`feed/tests.rs`, `subscription/queue_tests.rs`).

- [ ] a_subscription_aligns_held_sessions_with_a_known_an_unknown_and_an_empty_log
- [ ] have_selects_the_suffix_and_a_mismatched_or_longer_head_gets_the_whole_log
- [ ] a_scope_published_later_is_aligned_when_it_appears
- [ ] a_first_load_sends_each_subscription_its_log_from_its_own_baseline
- [ ] an_unknown_log_keeps_the_declared_baseline_through_live_only_changes
- [ ] queued_log_changes_follow_the_tail_and_only_the_output_advances_the_baseline
- [ ] an_undelivered_reload_to_the_same_length_is_followed_by_the_next_append
- [ ] an_undelivered_shrink_is_followed_by_the_next_append
- [ ] an_equal_reload_of_a_known_log_changes_no_log
- [ ] a_coalesced_queue_aligns_an_unsent_log_from_the_baseline_and_applies_directly
- [ ] a_coalesce_after_a_partial_delivery_aligns_from_the_advanced_baseline
- [ ] a_coalesce_keeps_a_queued_end_and_nothing_of_its_scope_follows
- [ ] a_turn_completion_and_a_reload_coalesce_to_the_same_cut_a_follower_reaches
- [ ] session_subscribers_count_open_subscriptions_only
- [ ] a_replica_rejects_a_log_change_or_patch_that_does_not_apply
- [ ] a_stream_query_names_scopes_heads_and_load_directories
- [ ] review_an_uncoalesced_first_event_is_the_subscription_cut_before_later_commits
- [ ] review_end_filters_later_same_scope_events_before_and_after_delivery

## Registry convergence (`src/protocol/convergence_tests.rs`)

Retired with the removal of the revision 2 engine. Read them at `9906369`. Registry transitions checked against the revision 2 publisher with a hash-chain replica. The same transitions are checked through the bridge gate and the engine tests.

- [ ] global_and_catalog_start_with_a_completed_none_and_an_empty_log
- [ ] a_created_session_starts_with_a_completed_open_and_a_known_empty_log
- [ ] a_first_load_runs_with_an_unknown_log_and_ends_with_the_replayed_log
- [ ] a_failed_first_load_ends_the_scope_with_load_failed
- [ ] a_completed_turn_appends_its_entry_removes_its_process_and_ends_its_op_in_one_event
- [ ] a_failed_turn_appends_nothing_and_keeps_its_prompt_and_error_on_op
- [ ] a_requested_cancel_shows_on_the_running_prompt_and_a_cancelled_stop_completes_it
- [ ] interactions_and_controls_change_in_live_while_the_log_stays
- [ ] an_interaction_joins_the_running_prompt
- [ ] attachments_are_readable_while_log_or_live_references_them
- [ ] a_set_mode_acknowledged_without_a_notification_shows_the_mode_when_its_op_ends
- [ ] a_set_config_acknowledged_without_a_notification_shows_the_confirmed_options
- [ ] a_failed_set_mode_changes_no_control
- [ ] control_updates_fold_after_the_response
- [ ] a_delete_occupies_its_session_and_ends_it_with_deleted
- [ ] a_failed_delete_frees_its_session_and_keeps_its_error_on_global
- [ ] a_running_operation_is_not_replaced
- [ ] reloads_with_an_equal_replay_change_no_log_and_a_diverging_one_rebuilds_from_the_common_prefix
- [ ] terminals_roll_their_bounded_tail_with_trim_and_append_and_leave_on_release
- [ ] an_installed_operation_remembers_its_scope_and_the_operation_it_replaced
- [ ] url_flows_appear_on_accept_and_leave_on_completion
- [ ] request_scoped_elicitations_run_as_an_agent_request_of_global
- [ ] the_gate_reports_a_transition_that_was_not_committed
- [ ] every_event_round_trips_its_documented_wire_shape
- [ ] later_identical_mode_notification_overrides_setup_response
- [ ] later_identical_config_notification_overrides_setup_response
- [ ] streamed_text_publishes_only_the_new_suffix

