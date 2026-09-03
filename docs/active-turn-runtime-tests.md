# Authoritative-history runtime TDD plan

This is the executable acceptance plan for
[active-turn-runtime.md](active-turn-runtime.md). The production transition is not complete until
the old active-only assertions have been replaced and every P0 test below is green.

Tests use deterministic barriers, paused time and logical byte accounting. Timing sleeps and RSS
alone are not correctness evidence.

## Layers

| Layer | Subject | Harness |
| --- | --- | --- |
| L1 | HistoryCache, session actor, folding, CAS and accounting | pure Rust state/reference model |
| L2 | ACP prompt/load orchestration and retry | scripted Agent with request/notification barriers |
| L3 | snapshot cut, suffix, Hub backpressure and shared payloads | in-memory observers |
| L4 | stdio, HTTP/SSE and WebSocket Agent transports | production binary fixtures |
| L5 | browser business commands and SSE rendering | reducer/component/Playwright |
| L6 | memory and lifecycle regression | logical counters, drop probes and Goose soak |

## P0 red tests

### Initial baseline

- `cold_session_first_observer_starts_one_load`
- `concurrent_cold_observers_join_one_load`
- `valid_load_atomically_installs_complete_baseline`
- `empty_load_installs_an_empty_baseline`
- `load_error_exposes_no_partial_candidate`
- `invalid_or_oversized_replay_preserves_cold_state`
- `old_epoch_or_incarnation_load_cannot_install_baseline`
- `session_without_load_can_create_and_rebuild_while_bridge_lives`
- `session_without_load_cannot_cold_materialize_after_bridge_restart`

### Turn CAS and idempotency

- `two_tabs_same_history_revision_dispatch_exactly_one_prompt`
- `stale_history_revision_is_rejected_before_agent_io`
- `same_intent_and_digest_returns_existing_operation`
- `same_intent_with_different_digest_is_rejected`
- `lost_post_response_retry_while_running_does_not_redispatch`
- `lost_post_response_retry_after_fast_reconcile_does_not_redispatch`
- `identical_prompt_with_new_key_and_new_revision_is_not_content_deduplicated`
- `old_bridge_epoch_revision_is_rejected_after_restart`
- `running_and_reconciling_never_dispatch_browser_queued_turns`
- `queued_prompt_waits_for_reconcile_revision_before_submission`
- `queued_prompt_submission_uses_the_new_history_revision`
- `two_browser_queues_compete_through_normal_history_cas`
- `closing_a_page_discards_only_its_unsent_queue`

### Active observation

- `observer_during_running_reads_baseline_plus_overlay_without_load`
- `observer_during_reconciling_reads_baseline_plus_completed_overlay_without_load`
- `reconnect_storm_during_turn_performs_zero_loads`
- `snapshot_plus_contiguous_suffix_equals_uninterrupted_projection`
- `subscribe_publish_race_has_one_atomic_cut`
- `gap_or_expired_suffix_forces_reset`
- `reconcile_commit_observer_sees_old_or_new_generation_not_both`
- `slow_or_disconnected_observer_never_cancels_agent_work`

### Reconciliation

- `prompt_terminal_retains_overlay_and_enters_reconciling`
- `prompt_terminal_triggers_exactly_one_internal_load`
- `same_session_prompt_and_load_never_overlap`
- `load_candidate_is_not_broadcast_before_commit`
- `retryable_load_failure_keeps_baseline_and_completed_overlay`
- `retry_is_singleflight_backed_off_and_bounded`
- `retry_waits_for_prior_load_request_to_terminate`
- `deterministic_load_failure_enters_blocked_without_hot_loop`
- `stale_replay_or_missing_completed_turn_does_not_commit`
- `successful_retry_atomically_swaps_baseline_then_drops_overlay`
- `successful_commit_always_advances_history_revision`
- `next_prompt_is_blocked_until_reconciliation_commits`
- `different_sessions_reconcile_independently`
- `no_load_terminal_atomically_promotes_prompt_and_overlay`
- `no_load_second_turn_preserves_the_first_turn_baseline`
- `rejected_no_load_promotion_preserves_baseline_and_completed_overlay`
- `no_load_terminal_performs_zero_session_load_requests`

### Live resources

- `pending_permission_survives_observer_reconnect_and_remains_actionable`
- `pending_elicitation_survives_observer_reconnect_and_remains_actionable`
- `turn_scoped_interactions_end_at_their_protocol_terminal_point`
- `live_terminal_is_not_removed_by_baseline_commit`
- `live_url_or_mcp_resource_can_cross_reconciliation`
- `late_old_generation_resource_event_is_ignored`

### Memory and eviction

- `history_snapshot_is_shared_not_cloned_by_runtime_delta`
- `many_observers_hold_shared_baseline_payloads`
- `baseline_overlay_candidate_peak_is_fully_accounted`
- `candidate_growth_is_accounted_without_admission_rejection`
- `large_protocol_valid_history_is_not_rejected_by_cache_accounting`
- `large_protocol_valid_active_overlay_is_not_rejected`
- `pressure_evicts_least_recent_unobserved_ready_baseline`
- `observed_running_reconciling_and_live_resource_sessions_are_pinned`
- `evicted_session_next_observer_starts_one_load`
- `close_delete_and_shutdown_release_cache_candidate_and_retry_task`
- `ten_thousand_turns_keep_one_baseline_and_zero_retired_overlays`

### API and SSE

- `session_view_contains_epoch_incarnation_history_and_view_revisions`
- `if_match_and_idempotency_key_are_required_for_turn_append`
- `post_ack_before_or_after_sse_converges_to_same_state`
- `last_event_id_continuity_replays_suffix`
- `old_or_unknown_last_event_id_sends_reset_snapshot`
- `background_resume_does_not_reload_or_cancel`
- `browser_discards_old_epoch_events_after_bridge_restart`
- `reconciling_disables_composer_with_visible_sync_state`

### Transport and Agent compatibility

Run the common prompt-terminal-reconcile and retry cases over stdio, remote HTTP/SSE and WebSocket.

- `transport_matrix_terminal_reconciles_before_next_prompt`
- `transport_matrix_browser_disconnect_does_not_affect_prompt_or_load`
- `transport_matrix_lost_prompt_response_is_uncertain_not_redispatched`
- `transport_matrix_lost_load_response_never_overlaps_retry`
- `transport_matrix_two_sessions_progress_independently`
- `goose_repeated_same_session_load_is_read_after_turn_consistent`
- `goose_replay_contains_the_just_completed_prompt_and_agent_output`
- `no_load_transport_matrix_reconnects_from_bridge_memory`

## Consistency oracle

The load validator records canonical item count and digest, but does not use either as the browser
revision. For a normal append-only turn the candidate must preserve the prior baseline and contain
the accepted prompt plus its completed result. Without load, the corresponding oracle is the old
baseline plus normalized `user_message_chunk` values synthesized from the accepted prompt plus the
validated overlay. Compaction and future protocol replacement semantics require an explicit typed
rule; they cannot silently bypass either oracle.

An Agent replay may repartition text chunks while remaining semantically equal. Tests compare the
normalized conversation/tool/plan model, not raw JSON chunk boundaries. Optional message IDs are
not assumed stable across loads.

## Retry harness

The scripted Agent exposes barriers at:

1. load accepted but before any replay;
2. between replay updates;
3. after replay but before LoadResponse;
4. after PromptResponse is ready but before bridge receipt;
5. before candidate validation;
6. before atomic commit;
7. before retry scheduling and dispatch;
8. before observer snapshot registration.

Every failure-prefix test asserts Agent request counts, phase, baseline identity, overlay identity,
candidate bytes, retry-task count and observer output. A new retry may start only after the previous
attempt has a definite terminal outcome.

## Existing proof that remains reusable

The current epoch/incarnation checks, intent payload digests, semantic update folding, session
operation exclusion, live-resource lifecycle, canonical sequence continuity, bounded subscriber
queues and cross-session concurrency tests remain useful. Their history-retirement expectations do
not.

## Old assertions to reverse or remove

The following behaviors describe the superseded implementation:

- Prompt terminal immediately deletes the active turn.
- Completed history bytes must always be zero.
- A new idle subscriber receives no completed history.
- Browser reconnect starts requester-private `session/load`.
- Load replay never becomes bridge state.
- A completed browser request ID can immediately be reused as new work.
- Bridge accepts and owns an unversioned queued prompt before the prior Turn reconciles.

Named tests such as `new_subscriber_receives_no_completed_history`,
`load_replacement_is_private_and_never_becomes_completed_replay`,
`completed_idle_session_shell_does_not_suppress_authoritative_reload`, the 10,000-turn
zero-history assertion, and queued-prompt automatic dispatch must be replaced by the P0 cases above.

## Green gates

1. Specification and red-test names land before production changes.
2. L1 HistoryCache/state tests pass without browser code.
3. L2 reconciliation and fault tests pass with exact Agent call counts.
4. L3 observation and memory-sharing tests pass.
5. L4 transport matrix and Goose canary pass.
6. L5 switches the browser from ACP-shaped lifecycle inference to business snapshots/commands.
7. Old requester-private load, active-only cache and Bridge-owned queued-prompt paths are deleted;
   the browser-local send queue remains.
8. Full Rust, TypeScript, browser, smoke and bounded soak suites pass.
