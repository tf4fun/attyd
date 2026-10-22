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
- [ ] a_held_session_ignores_the_load_directory
- [ ] a_cold_reopen_keeps_the_client_log_and_confirms_it_when_the_load_ends
- [ ] a_cold_reopen_with_a_changed_history_replaces_the_client_log
- [ ] two_subscriptions_share_one_load_and_each_gets_its_log_from_its_own_head
- [ ] reconnecting_before_during_and_after_a_load_keeps_one_load_and_converges
- [ ] an_unheld_session_without_cwd_or_whose_load_fails_ends_with_load_failed
- [ ] a_paused_stream_follows_its_tail_and_is_coalesced_past_its_budget
- [ ] subscribers_hold_a_loadable_session_and_the_last_one_leaving_releases_it
- [ ] a_loadable_session_without_close_is_released_locally
- [ ] a_session_released_before_its_first_subscription_is_loaded_by_it
- [ ] a_session_the_agent_can_neither_load_nor_resume_stays_held
- [ ] a_session_the_agent_can_only_resume_is_released
- [ ] an_accepted_url_flow_alone_keeps_the_session_needed
- [ ] an_exited_terminal_not_yet_released_keeps_the_session_needed
- [ ] a_new_subscription_or_an_occupation_cancels_the_release_timer
- [ ] review_a_sessions_own_admitted_operation_keeps_it_held_before_dispatch
- [ ] review_a_subscription_after_delete_admission_waits_for_its_end
- [ ] a_subscription_waits_for_a_cold_delete_and_ends_with_deleted
- [ ] a_subscription_waiting_for_a_failed_cold_delete_loads_the_session
- [ ] the_next_connection_s_equal_replay_ends_the_waiting_load
- [ ] the_next_connection_s_diverging_replay_rebuilds_from_the_common_prefix
- [ ] review_an_unfollowed_session_is_released_when_the_agent_connection_ends
- [ ] a_session_the_agent_cannot_load_again_ends_with_load_failed_at_the_connection_end
- [ ] four_consumers_converge_one_by_one_lagging_reconnecting_and_paused
- [ ] review_resume_only_session_waits_across_eof_and_preserves_its_known_log
- [ ] review_waiting_session_uses_the_new_connections_resume_capability
- [ ] review_refused_automatic_close_waits_for_a_new_subscription_cycle
- [ ] a_new_sessions_early_controls_survive_its_creation_response

## Bridge gate: tests rewritten in place

These stay in `src/bridge/protocol_gate_tests.rs` with revision 3 expectations (entries and
prompts instead of the turn log and the process): `a_streamed_prompt_with_an_answered_permission_ends_in_one_event`,
`a_failed_prompt_keeps_what_streamed_and_ends_with_its_error`,
`attachments_are_served_while_the_entries_that_reference_them_are_published`,
`the_same_history_gives_the_same_entries_after_a_restart`, and the gate's own counterexamples
(`the_gate_rejects_an_entry_change_without_its_event`,
`the_atomic_check_rejects_a_prompt_that_ends_before_its_operation`).
