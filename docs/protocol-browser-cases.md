# Browser cases of the revision 2 protocol, to be ported

`tests/browser/protocol-ui.pw.ts` and its harness `tests/browser/protocol-harness.ts` drove the real server
through a barrier proxy that held, lost and injected events of the revision 2 stream. The app now runs on
revision 3 (`/api/v2/stream`), whose events are snapshots, ops and checkpoints, so those files were removed in the
change that switched the app; they are in git history at `bcf9bce`. Each case below has to come back for revision 3,
with the harness reading `StreamEvent`s through the verifying replica set instead of the revision 2 replica. Cases that
assert the revision 2 model (a single write answering every pending interaction, a create or fork result taken from a
held response) become their revision 3 counterparts: one answer per interaction, the result read from the document
or from the session's `origin`.

### protocol harness
- forwards a v2 stream, records operation writes and ends a cancelled turn as cancelled
- serves the current UI unchanged through the proxy

### replica and reconnection
- opens a session by subscribing with its cwd and shows the load's history
- answers an interaction the load asks for while it runs
- subscribes again with the head it holds after the stream breaks, without reading the scope
- follows only the session it switches to and ignores the stream it replaced

### operations and answers
- lets one of two racing pages start the prompt and resubmits the refused input after the operation ends
- answers every pending interaction in one write
- keeps a filled form when its answer is refused because another interaction joined
- keeps a surviving form when the pending set shrinks and the earlier full answer is refused
- settles a prompt whose stream events arrive before its HTTP response
- settles a prompt whose HTTP response arrives before its stream events
- navigates to a created session from its own create result after a successor operation replaced it
- navigates to a fork from its own fork result after a successor operation on the source
- does not resend a prompt whose response was lost
- does not resend an answer whose response was lost
- shows a failed prompt's error without a history entry and retries the exact prompt on request
- stops a running turn with a cancel write and keeps the queue paused until the turn ends
- sends a queued message now by cancelling and submitting it once the turn actually ends

### controls and summaries
- shows mode, usage, commands and session info the Agent changes without a refresh (#16)
- changes config with a set_config write and shows the Agent's own config updates (#16)
- disables new control writes while a prompt runs but shows the Agent's mode change at once
- removes an accepted URL flow when the Agent completes it or the user dismisses it
- shows Input needed for a background session from the global summary without following it (#17)

### resources and media
- keeps a session terminal after its operation ends and drops its output once released
- renders an attachment from the log through the v2 attachment read
