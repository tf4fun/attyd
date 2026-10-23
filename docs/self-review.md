# Self-review of the revision 3 implementation (2026-10-23)

Written before the owner's review, after every part of [the plan](state-sync.md#7-fidelity-migration-revision-2)
that could be implemented was merged. It says what was checked and how, what the check found and
what was done about it, and what it did **not** check. Nothing here was verified on a local
machine: every statement about behaviour rests on the tests CI runs, and every change below went
through a draft run (fast checks) and a full run (all suites, coverage, browser, seven binaries)
before it was merged.

## 1. How it was checked

1. **Hygiene scan.** `TODO`/`FIXME`, skipped or ignored tests, blanket lint allowances.
2. **Dead code.** The module-wide `#![allow(dead_code)]` of `src/protocol` was lifted so that the
   compiler lists what nothing calls, twice: before and after the fixes.
3. **Plan against code.** Every row of the plan's step table (11–20), every normative rule of
   `protocol.md` §4–§9 that has a visible implementation, and every row of the independent
   acceptance table (§12.4) was matched to the code that implements it and to a test that would
   fail without it. A rule with a function but no caller, or with no test, was a finding.
4. **Routes against the documented endpoints**, error codes against HTTP statuses (§9), operation
   kinds against the admission (§6.2), live document members against the types generated for
   the client (§4.1–§4.4).
5. **Production `unwrap`/`expect`** in the protocol, server and bridge modules: the ones left
   assert invariants of the bridge's own data (a serialization of its own types, a lock that
   would be poisoned) and none depends on input from a client or the Agent.
6. **Exported TypeScript symbols** nothing imports: only types and helpers used inside their
   own file; no dead function.

## 2. What it found, and what was done

| Finding | Kind | Resolution |
| --- | --- | --- |
| A permission request's `toolCall` was never reduced into the session's entries: the reducer and its unit test existed, nothing called it (memory model §3.2; plan step 16) | functional omission | #165, with a gate test; one earlier test had encoded the missing behaviour and was corrected |
| A request-scoped interaction ended with the operation it had joined, not with its parent request's response: the interaction a control request raised was cancelled the moment it arrived (§8.2) | functional defect | #162, with a gate test |
| `set_mode` and `set_config` were still accepted as session *operations* beside the actions the protocol defines, with the semantics the actions exist to remove (refused while a prompt runs, R1) | spec–code inconsistency | #169: removed as operation kinds; the tests that used them as a convenient operation use a prompt or a fork, the ones about controls use the actions |
| The generated `SessionLive`/`GlobalLive`/`CatalogLive`/`Operation`/`Terminal`/`UrlFlow` lacked members the wire has, and the client kept a second hand-written description | type drift | #143, #147, #168: the live documents are serialized from the types the TypeScript comes from; #170: the client uses the generated types, so the client's typecheck is the alignment check |
| Attachment identity had two definitions (prompt references hashed doubles, the timeline hashed the exact form), the read answered the JSON encoder's bytes, and the client showed whatever it read | functional defect | #152, #153, #156; digests of integers beyond 2^53 pinned by vectors in #155 |
| A URL flow vanished when completed and when the connection ended | spec gap | #148 (`completed` and `ended` are kept until dismissed) |
| No way back once the Agent exited with no session followed | design gap | audit Q8–Q10, decided by comparison with Zed; the `restart` action (#159), the page's *Reconnect* sends it (#160), #161 |
| Audit Q5 (unknown Agent notifications) was left "to be confirmed by a test"; two acceptance rows (§12.4: concurrent answers, large integers in digests) and the initializing case of `restart` had no test | missing tests | #166, #154, #155, #161 |
| Unused code the cutover left: the revision 2 projection, the per-session turn log and `canon`'s, the `attachments` module, `held`/`pending_interactions`/`Pending`/`install_prompt`, two responder fields, a gate step's event copy | dead code | #144, #145, #146, #164, #167, #171; the test build now has no dead-code warning |

**Withdrawn.** I first listed the unbounded read of a workspace context file as a defect. It is
a decision, not an oversight: `docs/acp-coverage.md` states that payload limits are not imposed
and a filesystem test asserts that a large valid context is preserved.

## 3. Out of scope or by design

- **Diagnostics (plan step 19, protocol §4.7).** The protocol calls it "a later presentation
  proposal, not part of the first milestone". Not implemented.
- **A flow `ended` by the connection's end** stays in the final documents; nothing can dismiss it
  until the next connection, which starts without it (audit Q10). Zed keeps no such record.
- **No separate button to close a session** (audit Q9): releasing an unneeded session already
  closes it at the Agent; `close` is an action for an API client and for R2.
- **Reads and a `global`/`catalog` stream start no connection** (S6); only a followed session, a
  terminal login's next connection and `restart` do.
- **PR #50** (the web UI's visual language) was left untouched, as asked.

## 4. What this review did not cover

- **Visual and interaction quality of the page.** The Playwright suite passes in CI; nobody has
  looked at the pages in a browser, in particular the *Reconnect* button that now starts an Agent.
- **Performance.** Every response of a request of the bridge now takes the state lock once more
  (to note it as completed for #162) and may cause a commit; the memory of completed requests is
  bounded (256). Not measured.
- **Concurrency beyond the tests.** Races were reasoned about only where a test exercises them
  (concurrent answers, a restart racing a recovery, an interaction arriving after its parent's
  response). There was no stress or soak run.
- **Security.** Origin and host checks, path handling of the file reads and the attachment ID
  validation were read, not attacked; no dedicated security review was run.
- **Documents other than the ones the changes touched.** The older audit and matrix documents
  were not re-read in full and may still describe revision 2 in places.
- **Local verification.** Nothing was built or run locally, by instruction. CI is the only
  evidence, and it showed two kinds of mistake of mine more than once: test code that did not
  compile (helper visibility, mutability) and generated TypeScript copied from a CI log with
  interleaved log lines. Both were caught by CI before merging and the procedure was changed.

## 4a. The integrated tip

Pushes to a branch like `experiment/protocol-v2` run no CI (only pull requests, `main` and tags do), and
each pull request was validated against the tip it was based on. So the tip that results from
merging them one after another was never run as such. The pull request that adds this section is
based on the tip after the last merge (`ef9d834`) and changes nothing else, so its full run is the
run of the integrated tip: all suites, coverage, the browser suite and the seven binaries.

## 5. Where to start reading

`docs/protocol.md` (the wire), `docs/bridge-memory-model.md` (the bridge's memory), then
`src/protocol/{project,live_doc,registry,admission}.rs`, `src/bridge/{agent_dispatch,operations,controls,coordinator}.rs`
and `src/server.rs`; the acceptance rows of §12.4 map to `src/server/stream_tests/` and
`src/bridge/protocol_gate_tests*`.
