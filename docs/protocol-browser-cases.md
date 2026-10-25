# Browser cases of the revision 2 protocol, and what covers them on revisions 3 and 4

`tests/browser/protocol-ui.pw.ts` and its harness `tests/browser/protocol-harness.ts` drove the real server
through a barrier proxy that held, lost and injected events of the revision 2 stream. The app runs on revision 3
(`/api/v2/stream`) now, whose events are snapshots, ops and checkpoints, so those files were removed in the change
that switched the app; they are in git history at `bcf9bce`.

A barrier proxy is a way to produce a client's failure conditions, not a behavior: what each retired case asserts is
now asserted one layer down, against the store (`useAcp`, `tests/use-acp.test.tsx`), the write queue and client
(`tests/write-queue.test.ts`, `tests/write-client.test.ts`, `tests/write-resolution.test.ts`) and the verifying
replica (`tests/tree-feed.test.ts`, `tests/tree-replica.test.ts`, and `tests/scope-feed.test.ts` for the documents the page reads), where the lost, late and reordered
events and responses are exact, not timing-dependent; the browser suite (`tests/browser/*.pw.ts`) keeps the flows
the user sees against the real server. `[~]` marks a case about the revision 2 model that has no counterpart.

### protocol harness
- [~] forwards a v2 stream, records operation writes and ends a cancelled turn as cancelled: `api-requests.ts` records
  the page's requests in every browser case
- [~] serves the current UI unchanged through the proxy: there is no proxy

### replica and reconnection
- [x] opens a session by subscribing with its cwd and shows the load's history: use-acp "loads an unheld session the
  catalog lists, with the cwd the catalog gives"; browser `session-loading.pw.ts`
- [x] answers an interaction the load asks for while it runs: bridge `reconnecting_before_during_and_after_a_load…`
  (an interaction answered during a load) and `interactions_of_create_fork_set_config_and_list…`; use-acp "shows each
  pending interaction once, whichever scope shows it"
- [x] subscribes again with the head it holds after the stream breaks, without reading the scope: tree-feed
  "subscribes again later with the instance and the version it holds"; tree-replica "asks for what it follows, from what it holds, and for a session's load with its directory"
- [x] follows only the session it switches to and ignores the stream it replaced: tree-feed "closes the stream it
  replaces and lets nothing of it through"; tree-replica "lets nothing of a replaced stream or of a path not followed through"

### operations and answers
- [x] lets one of two racing pages start the prompt and resubmits the refused input after the operation ends:
  use-acp "keeps a prompt the bridge refused, with the reason, and sends it again only against a later op";
  write-queue "sends a definitely refused write again only once a later op has ended"; bridge admission
  `of_two_writes_with_one_expect_only_one_is_admitted`
- [~] answers every pending interaction in one write: a revision 3 answer is per interaction (use-acp "answers one
  interaction alone, at its resource, whatever else is pending")
- [~] keeps a filled form when its answer is refused because another interaction joined: no write carries another
  interaction; use-acp "keeps the input and shows why when the bridge refused an answer"
- [~] keeps a surviving form when the pending set shrinks and the earlier full answer is refused: as above
- [x] settles a prompt whose stream events arrive before its HTTP response: write-queue "sends the next write when the
  HTTP answer comes after the stream already showed a later op ended"; write-client "resolves a lost write from the
  document of the scope it was written to"
- [x] settles a prompt whose HTTP response arrives before its stream events: write-queue "sends the next write against
  the op the replica shows, even one past the admitted one"
- [x] navigates to a created session from its own create result after a successor operation replaced it: use-acp "finds
  the created session by its origin when the operation was replaced before the page saw it end"
- [x] navigates to a fork from its own fork result after a successor operation on the source: use-acp "forks the shown
  session and goes to the fork in the source's project"; browser "keeps the user's later navigation when an earlier
  fork response arrives late"
- [x] does not resend a prompt whose response was lost: use-acp "never sends again a prompt whose answer was lost";
  write-queue "never sends a write without a definite answer again, whatever is refused later"
- [x] does not resend an answer whose response was lost: use-acp "says an answer that may have been delivered is not
  known, and never sends it again"; write-client "never resends an answer whose response was lost"
- [x] shows a failed prompt's error without a history entry and retries the exact prompt on request: browser "renders
  structured ACP errors and retries the exact failed prompt"
- [x] stops a running turn with a cancel write and keeps the queue paused until the turn ends: use-acp "cancels the
  running turn against the op it shows"; browser "keeps queued ACP work paused after Stop and resumes it after a new
  message"
- [x] sends a queued message now by cancelling and submitting it once the turn actually ends: browser "queues ACP
  follow-ups and uses session cancel for Send now"; ui-interactions "offers bounded queued-message editing, removal,
  clearing, and Send now actions"

### controls and summaries
- [x] shows mode, usage, commands and session info the Agent changes without a refresh (#16): use-acp "shows the mode,
  usage, commands and title the Agent changes without a refresh, also while a prompt runs"; browser "renders live ACP
  context usage at the Zed-style composer edge"
- [x] changes config with a set_config write and shows the Agent's own config updates (#16): use-acp "sets an option,
  and shows the error the Agent answered with" and "shows the Agent's own config updates"
- [x] disables new control writes while a prompt runs but shows the Agent's mode change at once: browser "leaves a
  running session without cancelling it and allows configuration only once the prompt ends"; use-acp (the mode change
  while a prompt runs)
- [x] removes an accepted URL flow when the Agent completes it or the user dismisses it: use-acp "removes a URL flow
  the Agent completed or ended from the page" and "dismisses a URL flow in the scope that shows it"
- [x] shows Input needed for a background session from the global summary without following it (#17): use-acp "lists a
  background session as needing input from the global summary without following it"; `project-browser.test.tsx`

### resources and media
- [x] keeps a session terminal after its operation ends and drops its output once released: use-acp "keeps the frozen
  output of a released terminal and says so"; browser "keeps live terminal output across reconnect and drops it for
  every observer once released"
- [x] renders an attachment from the entries through the attachment read: browser "opens attachment chips with native
  browser viewing and automatic downloads"
