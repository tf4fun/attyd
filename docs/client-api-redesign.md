# Client API design rationale

This document explains choices behind [简易 Agent 交互协议](protocol.md), the
project-defined FE ↔ bridge protocol. The wire contract and its current design
requirements live only there; implementation and migration live in
[state-sync.md](state-sync.md). This document defines no additional fields,
endpoints or lifecycle rules.

Status: **revision 2 target (#51), reviewed design #52; sequencing clarified
2026-10-20.** Complete the bridge's ACP functionality and authoritative memory
first, then finish client synchronization, verification and repair. Existing
replication mechanisms are a working baseline for that second stage, not a reason
to narrow ACP content. See the [source matrix](acp-bridge-matrix.md) and
[second-stage design requirements](protocol.md#12-synchronization-design-requirements).
The experimental runtime has not yet completed this target; the
[plan](state-sync.md#7-fidelity-migration-revision-2) tracks its status.

## 1. Scope

The authority order is `FE < bridge memory < ACP server`. The bridge is an ordinary
ACP client and has no persistence layer. Clients present the displayable and
actionable memory the bridge actually holds; their drafts, disclosure state and
unconfirmed cached history cannot restore authority the bridge lacks.

A faithful first layer and a converged second layer prove different things.
Comparing a replica with a lossy projection misses an ACP content loss; checking a
cached hash or applying a patch alone does not verify the installed content.
That is why the plan completes and directly checks ACP behavior and memory before
finalizing the FE protocol.

## 2. Why this model

The useful Raft analogy is one authority supplying ordered state and replacing a
replica's conflicting suffix. attyd has one bridge, no election or votes, no term,
and no persistent consensus log. The analogy does not require another distributed
system inside the bridge. A fresh or returning presentation client should follow
the same synchronization rules.

The original simplification removed content when a turn ended. That made the
representation too weak: a tool result, thought, plan, metadata or terminal output
may still be needed after the operation finishes. The correction is to retain the
semantic entities and their results, while releasing execution handles and
superseded patches. Folding a card remains a presentation choice.

## 3. Decisions and tradeoffs

These choices explain the current target and baseline. The linked protocol section
is the contract; an unresolved second-stage choice is not finalized by this table.

| Choice | Reason and tradeoff | Contract or follow-up |
| --- | --- | --- |
| Keep reduced timeline entities, not an unlimited raw event archive | Entity updates preserve current meaning and order without retaining every prior patch. One entity is not split between log and a correction table. | [Reduction](protocol.md#32-reduction) |
| Preserve sparse tool state as ACP `ToolCallUpdate` | A permission or first update can contain only an ID. Required `ToolCall` fields would force invented values; absent information must remain absent. | [Entries](protocol.md#31-entries) |
| Keep legacy plan snapshots per turn; ID-addressed plans have session identity | The legacy type has no ID; treating it like an ID-addressed plan would invent identity. | [Reduction](protocol.md#32-reduction) |
| Settle every sent prompt, including failure or interruption | Received content is not withdrawn by a later error. Unobserved outcomes cannot become successful stop reasons. | [Settling](protocol.md#33-turns-and-settling) |
| Hash the entire published entry, including its observed result | Omitting `end`, metadata or tool results would let different entries appear equal. This does not itself prove that a client verifies the entry it installs. | [Hash chain](protocol.md#34-hash-chain), [verification requirements](protocol.md#12-synchronization-design-requirements) |
| Let a complete Agent replay replace live-observed history | A replay normally lacks the live PromptResponse and may contain different detail or IDs. Equal reduction input gives equal items; live and replay need not give equal entries. Inventing missing detail to keep hashes equal would reverse authority. | [History status](protocol.md#35-history-status) |
| Rewrite a settled entity at its existing position | A second mutable correction store would split one entity's state. Recomputing the subsequent chain costs work; optimize only if measurements justify it. | [Settling](protocol.md#33-turns-and-settling) |
| Keep between-prompt content in the tail until a settle point | Streaming can use the existing mutable-state path without pretending each update is a separate prompt. | [Settling](protocol.md#33-turns-and-settling) |
| Start with head alignment and full replacement on mismatch | One fallback covers reconnect and divergent history. Multiple sample points are only a possible optimization; full replacement is valid if its content is verified. | [Stream baseline](protocol.md#5-stream), [design requirements](protocol.md#12-synchronization-design-requirements) |
| Use current operation admission instead of a retained intent/result system | An admitted new operation replaces the old `expect`; no ABA is introduced by returning to the same history. But `stale` cannot distinguish an admitted lost request from a competing write. | [Admission](protocol.md#64-admission) |
| Keep uncertain writes for the user, without automatic resend | A write with no definite answer may already have run. A user submitting again is new input. At-most-once interaction answers are the stated exception; terminal input is not idempotent. | [Client rules](protocol.md#1-model), [actions](protocol.md#7-actions) |
| Keep current results, with held HTTP responses for create/fork/list | The initiating client receives the needed result even if the visible operation is replaced. Losing that response loses it for that client; there is no historical result lookup guarantee. | [Responses](protocol.md#63-response) |
| Serialize global operations and give listing its own scope | Create/delete/authentication share the global slot; listing need not be blocked by them. | [Operation kinds](protocol.md#62-kinds) |
| Release only sessions the Agent can load or resume | Otherwise a first or returning subscriber could lose an unrecoverable session. A resume-only release trades memory for incomplete history and must return `partial`. | [Holding](protocol.md#1-model), [history status](protocol.md#35-history-status) |
| Run controls and close as actions, including during a prompt | ACP permits these actions without another exclusive operation slot. Their actual unfinished requests keep the session needed until ordered completion is applied. | [Actions](protocol.md#7-actions), [implementation](state-sync.md#71-design) |
| Scope interactions by their real ACP request/session | An operation may outlive one RPC or end before an independent interaction. Unknown parents are not guessed; current request correlation suffices without request history. | [Interactions](protocol.md#44-operations-and-interactions) |
| Keep accepted URL outcomes on the flow until dismissal or scope end | A transient notification would leave a fresh replica unable to distinguish completion from disappearance. Parent response ends unanswered interactions, not an already accepted external flow. | [Resources](protocol.md#46-resources) |
| Separate terminal handles from referenced output | Releasing execution must not erase output embedded in a tool. Frozen output holds no live work and is reclaimed when no retained content references it. | [Resources](protocol.md#46-resources) |
| Keep diagnostics optional and bounded if adopted | Agent stderr and MCP activity can help users, but do not turn every service RPC into session history. Scope and bounds belong to later presentation work. | [Diagnostics proposal](protocol.md#47-diagnostics), plan step 19 |

The representation decisions still open in the first layer—fork history source,
pre-installation resource attribution, and source-level metadata—remain in the
[source matrix](acp-bridge-matrix.md). No rationale above resolves them by implication.
The second stage must also define content equality and verification, mutable-state
divergence detection, resource-read association, ordering and repair before it can
claim a complete client contract.

## 11. Acceptance

The original **cases 1–37** and all their subcases are maintained in
[Protocol acceptance](protocol-acceptance.md), with stable anchors `a01`–`a37`.
The [source matrix](acp-bridge-matrix.md) adds ACP field-level cases; the
[protocol's second-stage requirements](protocol.md#12-synchronization-design-requirements)
add independent content-verification and repair cases.

## History

The prior proposal, duplicated rule tables and migration narrative are available
at the fixed experimental baseline
[`991b053`](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/client-api-redesign.md).
They explain earlier choices but do not override today's contract or plan.
The abandoned draft review remains a [historical review](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/protocol-v2-review.md).
