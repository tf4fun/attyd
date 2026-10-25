# ACP v1 client coverage

This is the **test-evidence ledger**, not a second capability specification.
The [source matrix](acp-bridge-matrix.md) owns the pinned source inventory and
field-level requirements; [audit §9](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)
owns the first-layer memory contract. [State-sync §7](state-sync.md#7-fidelity-migration-revision-2)
assigns implementation and acceptance work. Test commands and fixture setup are in
[testing.md](testing.md).

Evidence below describes `experiment/protocol-v2` at `991b053` (revision 1), compared
with stable rc.5 `60f8606`. Revision 2 and draft PR #53 are targets, not completed
coverage. A named test proves only its specific assertions: an RPC handler or a
replica matching its projection does not certify field retention. D/L/Q identifiers
refer to the [fidelity audit](acp-fidelity-audit.md), R/P to
[release parity](release-parity.md). Neither a deleted test nor an assertion changed
to expect content loss closes a gap.

## Compatibility contract

The pinned Rust schema, SDK transport profile and negotiated capabilities define
the compatibility target; see the [fixed versions and actual features](acp-bridge-matrix.md#1-来源和版本).
The TypeScript SDK 1.4.0 supplies independent test peers, not the full Rust 1.7
field inventory. SDK upgrades require a source-difference review and stdio,
HTTP/SSE and WebSocket regressions. New drafts, ACP v2 and particular Agent behavior
do not automatically change the target; no vendor is a conformance gate.

## Client → Agent

Method fields, capability gates and out-of-scope providers/NES/document methods are
specified in [lifecycle](acp-matrix/lifecycle.md). The following groups locate the
existing evidence and the assertions still needed; “path” is not complete support.

| Surface | Evidence location below | Open direct-memory acceptance |
| --- | --- | --- |
| initialize; stdio/HTTP/WebSocket | current chain step 5; backend/remote suites | Transport T01–T19 and zero-session recovery (step 14) |
| new/load/resume/fork | current chains; Backend behavior | Full replay, history knowledge, fork cut/resources, setup metadata and unknown-session attribution: D1–D3/D7/D9; steps 11/14 |
| list/delete | current chain step 6; Backend behavior cleanup | Complete catalog/current-result metadata and failure ordering; step 14 |
| prompt/cancel; all prompt content kinds | current chain step 6; native semantic/content fixtures | Retained outcomes, failure/idle content, exact fields/numeric domain and proper interaction lifetime: D1–D7/D11; steps 11–13/16 |
| mode/config/close | current chain acknowledgements; cleanup tests | Prompt-time controls and explicit close R1/R2; source metadata D12 and late acknowledgements; step 18 |
| authenticate/logout/terminal login | current chains; `auth_terminal::tests` | Metadata, no-session recovery and actual pending-parent lifetime; steps 14/16 |
| MCP setup/additionalDirectories | native CLI/configuration and capability fixtures | Assert exact outbound setup fields and host policy; lifecycle capability rows, step 17 |

## Agent → Client

Per-method execution and resource semantics are in [services](acp-matrix/services.md);
notification and reduction semantics are in [content](acp-matrix/content.md).

| Surface | Evidence location below | Open direct-memory acceptance |
| --- | --- | --- |
| message/thought/tool/plan/compaction updates | native semantic/runtime tests; existing convergence gates | Completed/replayed and idle/late retention, IDs/boundaries/metadata, first sparse tool and removals: D1–D7; steps 11–13 |
| commands/mode/config/info/usage updates | current chain control acknowledgements; native controls tests | Full source metadata and early/response/later ordering: D12; step 18 |
| permission; form/URL elicitation and completion | current chain interactions/flows; captured cancellation tests | Independent answers, parent/scope lifetime, permission-only tool, URL outcomes, form patterns and i64 precision/domain: D10/D11/L5; step 16 |
| fs read/write | native filesystem and cancellable-I/O suites | Direct replies, root/read-only/remote boundaries and mutation-stage cancellation: services S04/S05 |
| terminal create/output/wait/kill/release | native terminal suite; Backend behavior resources | Retained referenced output after release/connection end and stale local callbacks: D8; step 15 |
| MCP connect/message/disconnect, both directions | native/remote MCP fixtures | Results/errors, nested callbacks, cancellation and process cleanup: services S11–S14 |
| unknown extensions; `$/cancel_request` | native dispatch/coordinator/inbound-request suites | Unknown-notification liveness (Q5), one response and allocation-exact cancellation: services S15–S17; step 17 |

## Protocol chain (current protocol)

The rows below were written for revision 3. Their *Document or route* column names its documents; in revision 4 the same facts are paths of the state tree (`session/<id>` is `/sessions/<id>`, `<scope>.live.op` is the target's `op` and `opDetail`, `global.live.agent` is `/global/agent`) and writes go to `/api/v2/targets/{target}/…` ([protocol §11](protocol.md#11-previous-revisions-and-changes)). The tests named are the evidence of the bridge behaviour, which did not change.

The revision 1 chains at `991b053`, added in migration steps 5 and 6
([plan §5](state-sync.md#5-completed-migration-history)); since step 8 they are the only client API. Tests in `bridge::protocol_gate_tests::stream`
run the coordinator over a real ACP transport; tests in `server::stream_tests` add
real HTTP. Step 5:

| ACP capability | Bridge authority | Document or route | Result | Fresh replica | Lifecycle end | Test |
| --- | --- | --- | --- | --- | --- | --- |
| `initialize` | connection task; host recovery | `global.live.agent` | `phase = ready`, or `error` | `GET /api/v2/stream?follow=/global` aligns from the published cut, also while initialize is unanswered | final commit; the next connection only for a session stream or once after a login | `a_new_global_stream_aligns_while_initialize_is_unanswered`, `after_an_agent_eof_reads_serve_the_last_documents_and_a_global_stream_restarts_nothing`, `an_initialization_failure_keeps_recovering_while_a_session_stream_needs_it`, `recovery_continues_for_a_remaining_subscriber_and_stops_after_the_last_leaves`, `a_session_ended_with_load_failed_is_no_recovery_need_behind_a_global_stream` |
| `session/load` | stream serving; session view load | `session/<id>` `op` (`load`) | log from the completed replay | alignment from `have`; an unknown log omitted | `end: load_failed` | `a_cold_reopen_keeps_the_client_log_and_confirms_it_when_the_load_ends`, `two_subscriptions_share_one_load_and_each_gets_its_log_from_its_own_head`, `reads_return_one_cut_hold_nothing_and_tell_an_unknown_log_from_an_empty_one`, `each_session_uses_its_own_have_and_cwd_on_a_multi_scope_stream`, `a_cold_load_cut_by_an_eof_keeps_the_cache_and_converges_on_the_next_connection` |
| `session/load` or `session/resume` on the next connection | waiting sessions seeded before the first commit | `session/<id>` `op` (`load`) | a load replaces the known log only when it completes; a resume keeps it | the same stream, or a new one while initialize is unanswered | `end: load_failed` when the Agent cannot load again | `a_session_stream_waits_through_an_agent_eof_and_converges_on_the_next_connection`, `a_session_stream_applies_a_diverging_replay_after_an_agent_eof`, `a_known_session_aligns_on_a_new_stream_while_the_next_initialize_is_unanswered`, `review_resume_only_session_waits_across_eof_and_preserves_its_known_log`, `a_session_the_agent_cannot_load_again_ends_with_load_failed_at_the_connection_end` |
| `session/close` | release on need (S6) | the session leaves `global.sessions` | released only when loadable or resumable | `404` on read | — | `subscribers_hold_a_loadable_session_and_the_last_one_leaving_releases_it`, `a_session_the_agent_can_neither_load_nor_resume_stays_held`, `review_an_unfollowed_session_is_released_when_the_agent_connection_ends` |
| Terminal authentication | auth terminal manager; host recovery | `global.live.op` (`terminal_login`) | one next connection | the same stream follows it | the authorization is consumed by that connection | `a_successful_terminal_login_starts_the_next_connection_for_a_global_only_stream`, `a_login_restart_is_consumed_and_the_next_ordinary_eof_starts_nothing` |

Step 6 (`POST /api/v2/targets/{target}/ops`, the resource actions and the session reads; tests in `server::stream_tests::ops`):

| ACP capability | Bridge authority | Document or route | Result | Fresh replica | Lifecycle end | Test |
| --- | --- | --- | --- | --- | --- | --- |
| admission | `protocol::admission` + `bridge::operations`, one guard | `<scope>.live.op` | `202 { op }` or `{ error }` (`stale`, `not_allowed`, `invalid_request`, `unsupported`, `unavailable`) | a refused write changes nothing | the replaced operation | `session_writes_need_the_current_ended_op_and_replace_it`, `global_writes_need_the_current_ended_op_and_replace_it`, `a_list_needs_the_current_ended_catalog_op`, `of_two_writes_with_one_expect_only_one_is_admitted`, `a_refused_write_can_be_submitted_again_on_the_same_expect`, `refusals_follow_the_admission_order`, `without_a_connection_writes_are_unavailable_after_expect_and_arguments`, `malformed_and_foreign_writes_are_refused_with_the_protocol_error_body`, `a_session_id_with_slashes_is_written_through_its_encoded_scope` |
| `session/prompt` | turn path, operation ID as request ID | `session/<id>` `op` (`prompt`), `log` | `op.result { stopReason, usage? }` | alignment from `have` | the ending commit | `a_prompt_answers_202_with_its_op_and_commits_its_result`, `prompt_attachment_references_are_resolved_and_an_unknown_one_is_refused` |
| `session/new`, `session/fork`, `session/list` | held response on the completion channel | `global`/`session/<id>`/`catalog` `op` | `200 { op, result }` or `{ op, error }` | a created session is listed while held | the operation's end; a dropped response withdraws nothing | `create_fork_and_list_answer_with_their_result_or_error_when_they_end`, `a_fork_uses_its_explicit_cwd_and_otherwise_the_source_cwd`, `a_fork_response_survives_its_replacement_by_an_agent_request`, `a_create_response_survives_its_replacement_and_a_late_reader_converges`, `admitted_create_fork_and_list_keep_their_errors_when_the_agent_disconnects`, `a_dropped_create_response_does_not_withdraw_the_create`, `a_list_returns_its_page_and_a_stale_revision_is_catalog_changed`, `create_fork_and_list_responses_outlive_the_json_deadline` |
| `session/set_mode`, `session/set_config_option` | `complete_control_operation` | `session/<id>.live.controls` | folded in the ending event; a failure folds nothing | — | the ending commit | `control_acknowledgements_fold_into_the_ending_event_and_failures_change_nothing` |
| `session/close`, `session/delete` | occupation of `global` and the session | both scopes show the delete | `end: deleted`, or the delete's error | a cold delete holds back the load | the delete's end | `a_held_sessions_prompt_and_its_delete_are_never_both_admitted`, `a_close_interaction_is_answered_through_either_scope_that_shows_it`, `a_failed_held_delete_keeps_its_error_and_the_subscription`, `a_subscription_during_the_first_load_keeps_its_recovery_directory`, `a_new_subscription_after_a_partially_failed_delete_still_needs_cwd`, `a_cold_delete_holds_back_the_load_until_it_ends` |
| `session/request_permission`, `elicitation/create` | joined to the scope's running op, else `agent_request` | `op.interactions` | `answer` checked whole, then delivered | the interaction set of the shown op | the interaction leaves with its answer or its op's end | `interactions_of_create_fork_set_config_and_list_are_answered_in_their_scope`, `a_load_interaction_is_answered_in_the_loading_session`, `an_agent_request_without_a_running_op_starts_one_that_completes_with_its_last_interaction`, `an_answer_is_refused_whole_unless_it_covers_exactly_the_pending_interactions` |
| `session/cancel`; cancel outcomes | `cancel_requested` on the owning slot | `op.cancelRequested` | the op ends `cancelled` when it actually ends | — | the operation's end | `a_prompt_cancel_asks_the_agent_once_and_keeps_the_session_occupied`, `a_cancelled_agent_request_answers_its_interactions_with_their_cancel_outcomes`, `other_kinds_refuse_a_cancel` |
| URL elicitation (`elicitation/complete`) | `bridge::resources` by interaction ID; settle and route primitives | `<scope>.live.urlFlows` | `dismiss` `204`, a repeat `404` | a reused `elicitationId` is a new flow | `elicitation/complete`, dismissal, scope end, connection end | `flows_outlive_their_operation_and_end_by_their_own_lifecycle`, `a_reused_elicitation_id_names_a_new_flow`, `the_connection_end_clears_the_global_flow` |
| Terminal authentication | auth terminal manager; ordered writer outside the guard | `global.live.terminals`, `op` (`terminal_login`) | `op.result { exitCode, signal }`; `input`/`resize` `204`; `dismiss` after exit | the same stream | dismissal; the connection end | `a_login_terminal_takes_input_in_order_and_is_dismissed_after_it_exits`, `a_signalled_login_completes_and_a_dismissal_removes_only_its_terminal`, `a_successful_login_completes_and_a_global_only_stream_sees_the_next_connection`, `review_terminal_io::terminal_input_backpressure_does_not_block_its_output_observer` |
| attachments; workspace context | `Publisher::attachment`; the session's own workspace | `GET /api/v2/sessions/{id}/attachments/…`, `files?query=`, `files/content?path=` | the ACP block; `{ matches }`; `{ attachment }` | reads hold nothing | the session's release | `attachments_are_served_while_their_own_session_references_them`, `reads_hold_nothing_and_a_released_session_serves_no_attachment`, `context_reads_follow_each_sessions_own_workspace` |

## Protocol chain (earlier v2 draft, history)

The earlier draft's request records, owner scopes and replication log were removed.
Its tests are not current coverage. The complete chain and named-test evidence
remain at fixed commit
[`11d04d0`](https://github.com/tf4fun/attyd/blob/11d04d0/docs/acp-coverage.md#protocol-v2-chain)
and in the [historical ledger snapshot](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/acp-coverage.md#protocol-chain-earlier-v2-draft-history).
See [history](history.md) for the corresponding old implementation plans.

## Backend behavior

Native evidence includes CLI/configuration, filesystem/context, terminal/auth,
capability/cwd, validation, structured errors, large HTTP/stdio/WebSocket values,
delayed admission, cyclic pagination and MCP process/request races. The named
lifecycle cases below preserve the evidence previously listed in `testing.md`.
Their existing assertions do not close the new field-level matrix automatically.

The bridge identifies a session by its ID only (migration step 9,
[plan §5](state-sync.md#5-completed-migration-history)). Its session-lifecycle gates are behaviour
tests, not checks of an internal identity:

- updates before a creation response: `preserves_early_update_bursts_until_creation_completes`,
  `a_new_sessions_early_controls_survive_its_creation_response`,
  `an_early_compaction_continues_its_summary_after_creation`,
  `invalid_early_updates_prevent_session_commit_and_clear_cleanly`;
- the response as the cut between replay and live data:
  `replay_response_seals_history_before_following_live_data`;
- a cold placeholder: `a_cold_placeholder_keeps_wire_replay_before_the_creation_response`
  (real ACP transport and coordinator),
  `creation_staging_keeps_cold_resources_separate_and_counts_every_unknown_id`;
- a load committed atomically or rolled back:
  `load_candidate_is_invisible_until_success_then_commits_atomically`,
  `failed_load_discards_candidate_and_resume_never_claims_replayed_history`,
  `invalid_replay_cannot_publish_a_valid_prefix`,
  `invalidated_load_attempt_cannot_commit_a_truncated_replay`,
  `replay_validation_rollback_settles_only_its_own_attempt`;
- close and delete cleanup:
  `close_cleanup_rejects_late_live_resources_without_publication_or_resurrection`,
  `close_then_delete_is_irreversible_and_partial_failure_stays_closed`,
  `delete_close_success_releases_resources_before_delete_response`;
- resource release: `closing_a_session_releases_resources_once`,
  `replacement_retires_old_resources_and_history_removal_preserves_live_ones`,
  `failed_attachment_releases_every_pre_response_live_resource`,
  `releases_only_terminals_owned_by_closed_session`.

A late notification names only a `sessionId`. One for an ID with no running
session is dropped when no creation is pending; while a creation is pending it may
be staged, bounded by the number of pending creations, and is replayed only into
the session whose ID the creation response returns. One for an ID that a new
session has taken applies to that session. No test claims to tell where a
notification came from, such as a session closed earlier under the same ID.

## Cross-cutting invariants

These are **baseline evidence boundaries at `991b053`**, not replacement target
rules. Detailed semantics live in the source matrix. The following conditions are
kept because removing them from the evidence inventory would lose a specific test
obligation or implementation limit.

| Area | Baseline condition to retain in direct coverage |
| --- | --- |
| Failed installation cleanup | A distinct unusable new/fork target is best-effort closed only when close is negotiated, exactly one creation is pending, the ID is addressable, and it names no tracked/attaching/deleting session. Duplicate IDs and a fork's source ID must never be auto-closed. Failed load/resume cleanup targets only its still-exclusive attachment. |
| Load transaction | A malformed, rejected or interrupted replay cannot expose a valid prefix. If its RPC cannot be cancelled, drain/discard before reusing the operation path; completion/rollback belongs to the actual attempt. |
| Notification and error isolation | Malformed Agent notifications are dropped to tracing without ending the prompt. Only its own response or connection end ends a prompt. Preserve structured error code/message/data; no browser disconnect or unrelated request failure may substitute for that result. |
| Admission and release | The revision 1 chain tests `expect`/operation admission and unneeded-session release. Those tests do not waive R1/R2 or authorize deleting active work. Keep `--session-unobserved-timeout` negative/zero/delayed cases and distinguish load/resume availability. |
| Numeric and field validation | Prompt Usage safe-integer and sum checks are implementation restrictions to audit against schema u64, not ACP facts. Session usage/cost is a different update. Base64, resource URI/MIME/size, finite annotation priority and timestamp checks also need the pinned field/domain cases, not only rejection tests. |
| Terminal execution policy | Shell acknowledgement precedes publishing a handle; launch/cwd failure leaks none. Missing standalone commands exit 127. Unix `/bin/sh -c` uses literal positional args when provided; no Agent-specific retry/quoting branch. Natural main-command exit retires process-group ownership without killing background jobs; explicit termination checks that exit first. Final pipe drain is one second and does not set ACP byte-limit `truncated`; uint32-incompatible exit values become null. See [usage](usage.md#terminal-command-semantics). |
| Host I/O | In-place fs writes have no automatic mkdir, retry or rollback; errors carry path/stage/OS cause/possible partial change. Pre-mutation cancellation differs from a started write. Cancel a terminal waiter without killing the process; cancel an unannounced MCP startup without leaking its child. |
| MCP privacy and close race | Provider launch argv/environment stay host-private; the Agent sees `{type,name,serverId}`, browser metadata `{type,name}`. Registration and close are atomic; duplicate/contradictory responses, reverse requests, named/status-bearing exits and cross-connection IDs require distinct cases. |
| Catalog and workspace inputs | Session rows preserve portable absolute cwd (POSIX/drive/UNC), additional roots and metadata; duplicate IDs upsert; unparseable dates do not invalidate rows. Cursors are Agent-owned and independent. Catalog intentionally omits cwd filtering. Context search skips symlinks/generated directories and rechecks selected real paths and UTF-8 content within all configured roots. |
| Capability/identity restrictions | Unknown sessions and unoffered mode/config IDs are refused. Remote host fs/terminal/auth and read-only writes remain unadvertised. Request-scoped IDs currently have a JS-safe restriction; form patterns use a restricted engine. Those are explicit limits, not complete-domain support. |
| URL cancellation evidence | `review_prompt_cancel_preserves_accepted_*_url_flow` covers the accepted-flow cancellation boundary. Revision 1 removes finished flows; outcome retention and actual parent-RPC cleanup remain step 16 work. Unknown completion must leave the connection usable. |
| Payload and retention limits | HTTP bodies, ACP stdout/WS messages, relayed values, held histories, catalog rows, pending work and most resource collections have no cumulative application quota; explicit Agent output limits and the form-pattern restrictions remain separate. This is not proof of retention, field fidelity or leak freedom. |

## Browser evidence (separate layer)

The full revision 1 presentation checklist is retained at the
[fixed implementation snapshot](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/acp-coverage.md#cross-cutting-invariants).
The [historical display audit](history.md#审计与发布记录) preserves field-level findings
and their follow-up evidence; [release parity](release-parity.md) separately records
rc.5 regressions and historical v0.2.8 UI/export behavior. These references are not
new first-layer requirements.

| Evidence family | What remains relevant to that layer |
| --- | --- |
| Production browser harness | Fresh real Rust host and stdio SDK Agent per Chromium case; desktop/mobile bounds, console/page errors, screenshots/traces/report. Native/black-box behavior remains independently tested. |
| Client replica/projection | `tests/replica.test.ts`, `tests/protocol-client.test.ts`, `tests/workspace-projection.test.ts`; convergence is not the ACP-content oracle. |
| Navigation and focus | Project/cwd grouping, paginated discovery, structural back links, session switching, menu Escape/outside-click behavior and focus restoration; permission/form focus and refused-answer draft retention. |
| Composer and queue | Session-bound prompts/attachments; queued edits are local, next prompt sends after settlement, Send now cancels through ACP. Expanded composer preserves draft/menus/controls and contracts for interactions; ↑/↓ recall retains complete ContentBlocks without merging intentional repeats. |
| Content presentation | Ordered thought/tool/message display, expandable tool/terminal/compaction details, read-only diffs, copy/context actions and full Markdown export. Revision 1 completed-message-only assertions encode D1, not fidelity. |
| Search and disclosure | Visibility-only semantic search excludes collapsed/raw duplicate content; keyboard controls, regex errors, 150 ms streamed rescan and manual expansion state remain UI evidence. Usage display's exact values and 80% warning are presentation, not a new ACP capability. |
| Attachment/context input | Picker/drop/clipboard/`@` paths, capability refusal, synthetic clipboard URIs, original ContentBlock order and recall; no prefetch or persistent viewer store. Resource opening remains restricted to supported safe link kinds. |
| Authentication and recovery | Agent-managed and PTY login, ordered input/resize, no replay of lost input, cancellation/signal/next-connection behavior, browser reconnect during a prompt and explicit recovery after Agent exit. |

## Remaining verification work

- Add direct fixtures for the unresolved rows above and record the test names as
  they land; [state-sync §7.3](state-sync.md#73-gates) is the first-layer gate.
  Source fields and unsupported subsets remain tracked in the domain matrices.
- Keep the Rust line-coverage gate at or above 85% and existing native, remote,
  real-HTTP and browser suites; commands/environment belong to [testing.md](testing.md).
  A passing aggregate suite is not proof of each new acceptance requirement.
- Retain current `protocol::convergence_tests`, `protocol::hub_tests`,
  `bridge::protocol_gate_tests` and `server::stream_tests` as regression evidence.
  The later client verification/repair design follows
  [protocol §12](protocol.md#12-synchronization-design-requirements).
- Broaden adversarial fixtures as negotiated experimental surfaces evolve. Keep
  optional-feature omissions explicit. Cross-browser rendering/accessibility,
  export and diagnostic panels remain separately scoped presentation work.
