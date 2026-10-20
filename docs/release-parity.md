# Release parity

Status: baseline corrected, 2026-10-16 (#51). This compares the stable v1
implementation with the experimental protocol implementation. The immediate goal
is a complete ACP client and displayable authoritative state in bridge memory.
Client synchronization, presentation, export and optional diagnostics are separate
follow-up work; historical UI behavior is not automatically a requirement of this
phase.

The [source-level feature matrix](acp-bridge-matrix.md) cross-checks this baseline
against every pinned Rust protocol surface and a fixed Zed revision. It also lists
shared limitations that release parity alone cannot detect.

| Point | Version | Source |
| --- | --- | --- |
| stable v1 | **v0.3.0-rc.5**, `60f8606` (`main`) | `488ccd9` plus only the rustls lockfile upgrade; source references below use `60f8606` |
| experimental | `experiment/protocol-v2`, `991b053` | protocol revision 1 implementation; [fidelity audit](acp-fidelity-audit.md) |
| historical release | v0.2.8, `a5b07ff`, 2026-09-18 | historical UI/export evidence, not the current stable baseline |
| target | protocol revision 2 (specification, not implemented) | [protocol.md](protocol.md); [state-sync.md §7](state-sync.md#7-fidelity-migration-revision-2) |

Paths and line numbers in the stable column refer to `git show 60f8606:<path>`;
those in the experimental column refer to `991b053`. The v0.2.8 references retained
below explain historical presentation, not what rc.5 retains in bridge memory.

**Classes.**

- **Kept**: stable v1 and the experiment retain the capability.
- **Replaced**: the same capability has a different protocol or representation.
  This does not require restoring v1 endpoints or internal bookkeeping.
- **Regression**: stable v1 provides the stated capability under the stated
  conditions and the experiment loses it.
- **Gap**: the stable baseline also lacks the stated guarantee. The target
  completes it; it is not a migration regression.
- **Later**: client presentation, export or optional diagnostics. A reference here
  does not make it a prerequisite of the ACP-to-bridge phase.
- **Decision**: a documented limit or product choice, distinguished from an ACP
  requirement.

**Acceptance.** Use the [direct ACP-to-memory gate](acp-fidelity-audit.md#94-direct-acceptance),
then the separate client acceptance. Step numbers below retain migration links;
they do not make every historical presentation row mandatory in the first phase.

The distinction matters here: stable v1 retains a `HistorySnapshot` of reduced ACP
updates (`src/history_cache.rs:28–32`). In the experiment, completed history lives
**only** in `TurnLog`; `HistoryCache` holds in-flight load candidates
(`src/history_cache.rs:20–24`). Content discarded by canon at settlement or load is
therefore lost from authoritative bridge memory, not merely hidden by the FE.

## Matrix

### History, tool details and folding

| Capability | Stable v1 rc.5 | Experimental `991b053` | Target | Class |
| --- | --- | --- | --- | --- |
| Completed turns retain thoughts, tool calls (status, content, diffs, terminals, locations, raw input/output), plans and compactions | all reduced turn updates appended to the baseline (`src/session_mirror.rs:528–552`) | only prompt and Agent message content committed (D1; `src/protocol/canon.rs:493–505`) | items, with ACP reduction (§3.1–§3.2) | Regression → step 11 |
| Loaded history retains those kinds | replay builder retains reduced updates (`src/history_replay.rs:28–77`) | messages only (D1; `canon.rs:134–169`) | same item reducer for replay | Regression → step 11 |
| An ordinary failed prompt retains output already received | non-transport errors still commit the overlay (`src/bridge.rs:5828–5845`) | only `TurnEnd::Completed` commits; failed overlay is then cleared (D4) | content retained with `end.error` | Regression for content → step 12 |
| Failed outcomes remain in the history | error emitted as an event; only responses with `stopReason` enter `turn_outcomes` (`src/bridge.rs:5860–5877`; `src/session_state.rs:461–471`) | error is on the operation, not a log entry | `Turn.end.error` | Gap → step 12 |
| Interrupted output survives an Agent connection replacement while the session stays held | blocked overlay remains during shutdown (`src/bridge.rs:5817–5825`); next runtime creates new bridge state, and the old generation's projection is cleared (`src/bridge.rs:2206`; `src/server.rs:1394–1407`) | blocked overlay is not in the transferred log | settle interrupted content before connection teardown (§3.3) | Gap; no stable cross-connection guarantee → step 12 |
| Full PromptResponse, including stop reason, usage and metadata, per locally completed turn | retained in `turn_outcomes` while held; replacing load clears those outcomes (`src/session_state.rs:457–471`) | current `op` keeps stop reason/usage only, until replaced | `Turn.end.response`; replay does not invent missing responses | Regression → step 11 |
| Updates between prompts | Ready updates append to the baseline (`src/session_mirror.rs:393–425`) | Ready branch returns success without retaining the update (D5; `src/session_mirror.rs:241–256`) | session tail, settled by §3.3 | Regression → step 13 |
| Late tool updates | `tool_call_update` finds the tool's original slot across the session history (`src/history_cache.rs:305–332`) | dropped while idle; settled tool content already absent (D5) | rewrite the entity's settled entry | Regression → step 13 |
| Session-wide plan/compaction reduction and plan removal state | compaction updates remain in the timeline; plan matching is turn-local and `plan_removed` deletes the local item (`src/history_replay.rs:37–51`; `src/runtime_state.rs:2831–2844`) | also loses settled content | explicit identity/patch rules, including removal state (§3.2) | Gap in full entity semantics, in addition to the content regression |
| Tool/compaction disclosure, thread search, change review and plan placement | historical client features; not separate bridge storage guarantees | limited by missing settled content | clients consume the restored items | Later; content is covered by the rows above |

### Messages and attachments

| Capability | Stable v1 rc.5 | Experimental `991b053` | Target | Class |
| --- | --- | --- | --- | --- |
| Message boundaries, IDs and interleaving | merges adjacent text only when the remaining update shape matches (`src/runtime_state.rs:2746–2770,2849–2885`) | IDs and intervening kinds lost; Agent messages flattened into reply (D2) | contiguous message items, preserving identity/order (§3.2) | Regression → step 11 |
| User message chunks sent by the Agent during a live turn | retained with all turn updates; matching prompt echo is preferred (`src/session_mirror.rs:528–549`) | dropped at commit (D3) | retained items | Regression → step 11 |
| Chunk/block metadata and annotations | retained in the reduced updates and original content blocks | chunk fields lost; `_meta` recursively stripped, including stored attachments (D6, D7) | fields retained at their original level | Regression → step 11 |
| Attachment blocks can be read and re-sent unchanged | reference projection does not replace the source block; `find` returns it (`src/attachments.rs:125–150,245–252`) | attachment table stores the block after removing `_meta` | references with the whole original block retained | Replaced; metadata regression → step 11 |
| Attachments in `compaction_update.summary[]` | source bytes retained, but the traversal misses this position (`src/attachments.rs:165–179`); not a complete reference/read path | current traversal includes the position, but canon still discards completed compaction content | all ACP content positions covered; retention follows references | Existing v1 gap plus the experimental content regression |
| Image/audio/resource prompts and file context | supported | supported | retained | Kept |
| Picker, drop/paste, `@` files and response copying | client features | client features | evaluated after source content is complete | Later |

### Interactions

| Capability | Stable v1 rc.5 | Experimental `991b053` | Target | Class |
| --- | --- | --- | --- | --- |
| Resolve each permission or elicitation independently | individual interaction response route (`src/server.rs:1730`) | all pending interactions of an operation answered together (D10) | one resource answer per interaction (§7) | Regression → step 16; bridge lifecycle first |
| Preserve ACP session/request scope and lifetime | distinct runtime stores and responder ownership; this is not a guarantee that every lifetime case is correct | interactions joined to the running operation (D11) | scope/lifetime follow ACP (§4.4) | Regression in scope model; remaining lifecycle cases need direct ACP tests |
| URL elicitation consent and completion handling | supported | supported | supported with the ACP scope | Kept |
| Retain a finished URL flow's outcome in authoritative memory | settlement removes the flow, retaining only its ID for duplicate handling (`src/runtime_state.rs:1576–1626`; test `:6764`) | also removes it; only waiting flows remain | outcome retained while its document exists (§4.6, P4/P5) | **Gap R4**, not an rc.5 regression → step 16 |
| Permission-only tool content survives the answer | `toolCall` stored only in the pending permission, not the history (`src/bridge/agent_dispatch.rs:398–431`) | same limitation | reduce the request's sparse `toolCall` into timeline state (§3.2) | Gap; reducer support in step 11, ingress wiring requires its own test |

v0.2.8 showed URL completion/abort events as transient client results
(`web/src/lib/state.ts:2967–3003`; `tests/state.test.ts:2502–2538,2570–2622`).
That is historical UI evidence, not proof that rc.5 retains those outcomes or that
either version restores them after a bridge/Agent restart.

### Session operations and settings

| Capability | Stable v1 rc.5 | Experimental `991b053` | Target | Class |
| --- | --- | --- | --- | --- |
| Change mode/configuration while a prompt runs | Control admission allowed during Running (`src/session_state.rs:181–194`) | refused until the prompt ends | control actions (§7) | **Regression R1** → step 18; confirmation/UI is separate |
| Explicit close, including during a prompt | Close admission allowed during Running; explicit close route (`src/session_state.rs:181–194`; `src/server.rs:1723`) | no explicit close action | `close` action and `end: closed` | **Regression R2** → step 18; browser confirmation is later |
| Unobserved timeout does not interrupt work | only Idle arms the countdown; work cancels it (`src/session_observers.rs:12–40,159–174`) | only releases when unneeded, and load/resume is available | keep the no-interruption rule | Kept; P3 |
| Resume without full replay | retains received context with an explicit missing-history notice (`src/bridge.rs:4257–4259`; `src/session_mirror.rs:275–306`) | unknown log becomes known replay/empty log with no completeness marker (D9) | `history: "partial"` for cold resume | Regression in completeness information → step 14 |
| Fork without authoritative load | captures source baseline at admission; received target context or fallback is labelled potentially incomplete (`src/bridge.rs:4271–4282,4420–4425,4481–4486`) | copies source log or received target messages | must preserve the full source cut, including idle tail, and distinguish partial target notifications from full replay | Needs specification clarification; neither baseline guarantees arbitrary fork notifications are complete history |
| Create, list, delete, cancel, commands, session info and usage | supported | supported | retained | Kept; detailed operation coverage remains in the ACP ledger |
| Project grouping, prompt queue and "Send now" | client behavior | client behavior | evaluated separately from ACP admission/state | Later |

### Terminals and authentication

| Capability | Stable v1 rc.5 | Experimental `991b053` | Target | Class |
| --- | --- | --- | --- | --- |
| Live terminal output | retained in session live state | retained | retained | Kept |
| Referenced output survives `terminal/release` | release freezes output/truncation/exit status if the overlay or history already embeds the terminal; retained for that incarnation, pruned when replay removes the reference (`src/session_mirror.rs:430–485,747–759`) | live terminal removed without a retained output store (D8) | frozen output while embedded (§4.6) | Regression → step 15 |
| Frozen output survives Agent connection replacement while the session stays held | no such guarantee; generation teardown clears bridge projection, new runtime has new state | removed | retain referenced output when the held documents survive (§4.6) | Gap, not a demonstrated rc.5 regression → step 15 |
| Authentication/logout and terminal-login lifecycle | supported | supported | retained | Kept; input/resize/cancel rendering is later |

Neither baseline restores terminal output after the owning session's in-memory
documents are gone. Stable v1 does not copy the source's retained terminal map merely
by copying a fork baseline. Terminal references and snapshots therefore need an
explicit fork test; a matching tool ID alone is not evidence that output is present.

### Markdown export

This is a **later client-layer checklist**, retained from v0.2.8 as historical
evidence. It is not an additional ACP-to-bridge requirement or proof that rc.5
retains every exported field. Source-memory guarantees are the rows above.

| Section | Historical v0.2.8 (`web/src/lib/thread-markdown.ts`) | Experimental `991b053` | Source for a later client |
| --- | --- | --- | --- |
| user message, with `messageId` | `:48–52` | settled IDs lost | user message items |
| Agent message and thought, with `messageId` | `:53–60` | merged reply, no settled thoughts | agent/thought items |
| tool title, name/kind, status and locations | `:61–72` | running turn only | tool items |
| tool blocks, diffs and terminals | `:73–75,148–170` | running turn only; released output lost | items and retained terminal snapshots |
| raw tool input/output | `:76–77` | running turn only | tool items |
| legacy and ID plans, including removal | `:80–81` and helpers | running turn only | plan items |
| compaction status, ID, summary and error | `:82–90` | running turn only | compaction items |
| text/image/audio/resources and annotations | `:107–142` | attachment references; metadata loss | items and attachment reads |
| stop reason and usage | `:93–99` | not in the log | locally observed `Turn.end.response` |
| error | `:100–101` | not in the log | `Turn.end.error`; retained while the session's content exists |

When export is implemented (the existing step 20 / redesign case 33), test it
against the retained ACP content, including released terminal output and failure
content. Do not reconstruct missing history or historical terminal output from IDs.
Passing this client test does not replace the independent bridge fidelity tests.

### Diagnostics

Diagnostics and their panels are optional follow-up work. Correct ACP transport and
MCP relay behavior remain core client capabilities; displaying traffic or stderr is
not required to prove them.

| Capability | Stable v1 rc.5 | Experimental `991b053` | Status |
| --- | --- | --- | --- |
| Agent stderr panel | stderr goes to tracing (`src/bridge.rs:2112–2113`); v0.2.8's panel is historical | bridge log only | **R3: historical diagnostics**, not an rc.5 regression or a core-phase gate |
| MCP-over-ACP activity inspector | relay exists; no requirement to retain its messages as display history | relay exists without an activity projection | **R3: optional diagnostics**; no fixed inspector/history model required here |
| Raw JSON of tool payloads | source tool content retained while its history exists | settled content lost | source fidelity covered above; raw JSON panel is Later |
| Other sessions' raw-event panel | historical v0.2.8 UI | no such panel | Decision P1 |

## Regressions

The existing R identifiers are kept for links. **R3 and R4 are reclassified**;
they must not be counted as proven rc.5-to-experiment regressions.

| ID | Capability | Stable-baseline evidence | Classification / target |
| --- | --- | --- | --- |
| R1 | `set_mode` / `set_config` during a prompt | `60f8606:src/session_state.rs:181–194`; v0.2.8 browser case is secondary historical evidence | Regression; actions (§7). Bridge admission/acknowledged state first, browser case later |
| R2 | explicit close during a prompt | same admission rule; `60f8606:src/server.rs:1723` | Regression; close and resource cleanup first, confirmation UI later |
| R3 | stderr and MCP activity panels | v0.2.8 UI, not a guarantee of the current stable release | Historical/optional diagnostics, outside this phase |
| R4 | finished URL-flow outcome in memory | rc.5 and experiment both remove it on settlement | Shared Gap; retained outcome is a target completion, not a stable regression |

The old redesign references remain: cases 28, 29 and 37 for R1/R2, case 32 for R3,
and case 11 for R4. Their layer and priority follow the classifications above.
Likewise, D1–D12 identify target defects, not a claim that every aspect was already
implemented by stable v1: failed-turn outcomes, permission-only tool history,
some entity reductions and cross-connection retention complete existing gaps.

## Decisions

| ID | Question | Decision |
| --- | --- | --- |
| P1 | Must v0.2.8's panel of raw events from other sessions return? | No. It is historical presentation, not an ACP requirement. Retain each session's authoritative content; a client can inspect that session. No raw-event journal is required. |
| P2 | What survives session release, replacement and cold load? | No persistence is added. Once documents are gone, local PromptResponses and terminal output cannot be reconstructed unless supplied again. A replacing load supplies history but not historical PromptResponses. A same-incarnation v1 load could keep an already retained terminal snapshot if replay still referenced it (`src/session_mirror.rs:271,747–759`); do not confuse that case with reopening after memory was released. |
| P4 | How should a finished URL outcome be represented? | The target retains status in the flow's document until dismissal or that document's end. This is a new memory guarantee relative to rc.5, not restoration of a stable record. A client reconnect can read it while the document exists. Agent connection end changes a waiting flow to ended and does not overwrite a completed outcome; the outcome remains if its owning document remains, including global/catalog documents. Bridge process restart loses this memory. No finished-flow journal or request record is required. |
| P3 | Does unobserved timeout interrupt active work? | No. Stable rc.5 already arms its countdown only while Idle (`src/session_observers.rs:159–174`). Preserve that rule; v0.2.8's older policy is not the current baseline. |
| P5 | Does the parent RPC's response end an accepted request-scoped URL flow? | The target keeps accepted flows until their own completion/end/dismissal, while ending unanswered child interactions with the parent RPC. The pinned schema scopes the elicitation to a request but gives no equivalent parent-completion rule for an accepted external flow. This remains an explicit target lifecycle choice, not a released-capability claim or a new request-history system. |

For P5, Zed's ACP v1 client at [28d0a35](https://github.com/zed-industries/zed/commit/28d0a3547f16b9609b301b48ab363bfca649c864)
remains an implementation reference only:
[authenticate](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/agent_servers/src/acp/v1.rs#L1545-L1563)
does not end accepted flows when its request returns, and
[request finish/cancel handling](https://github.com/zed-industries/zed/blob/28d0a3547f16b9609b301b48ab363bfca649c864/crates/acp_thread/src/acp_thread.rs#L774-L798)
distinguishes the two. Its display choices and session-cancel policy are not adopted
as ACP requirements. Shared finish/cancel helpers alone do not prove that the v1
adapter invokes parent-RPC cleanup. The [source-level matrix](acp-bridge-matrix.md#5-zed-参考的适用范围)
checks the actual v1/v2 call paths separately.
