# Client API design rationale

This document explains the choices behind [简易 Agent 交互协议](protocol.md), the
project-defined FE ↔ bridge protocol. The wire contract lives only there; layer 1 (ACP →
bridge memory) lives in the [bridge memory model](bridge-memory-model.md); implementation
and migration live in [state-sync.md](state-sync.md). This document defines no fields,
endpoints or lifecycle rules.

Status: **revision 3 proposal, 2026-10-21.**

## 1. Scope

The authority order is `FE < bridge memory < ACP server`. The bridge is an ordinary,
memory-only ACP client; `session/load` is the only history recovery path. Clients
present the memory the bridge holds; their drafts, folding and cached content cannot
restore authority the bridge lacks.

The two layers prove different things. Layer 1 shows that bridge memory faithfully
reduces what ACP gave. Layer 2 shows that every client's replica equals that memory,
can tell when it does not, and recovers. A replica that equals a lossy projection
proves nothing about ACP fidelity, which is why layer 1 is specified and tested first.

## 2. Goals

1. **Reliable, verifiable, recoverable synchronization** of bridge memory.
2. **Push to every subscriber** when memory changes.
3. **Correct concurrent and repeated requests** from several clients.

The analogy is one leader replicating ordered state to read-only replicas that replace
any conflicting state from it. There is one bridge, no election and no persistent log;
the analogy does not require a distributed system inside the bridge.

## 3. Decisions and tradeoffs

| Choice | Reason and tradeoff | Contract |
| --- | --- | --- |
| Replicate the memory model's flat entries, derive turns in the client | Load replay and live prompts build one structure (memory model §1); a turn-shaped wire log would reintroduce a structure only one path produces. The client needs one shared derivation function, tested against the bridge's fixtures. | [Session history](protocol.md#3-session-history) |
| `epoch` + `rev` instead of a hash chain | Live and replayed turns never hashed equal (replays lack `PromptResponse` and the sent prompt's shape), so the chain only helped same-process reconnects, where a version is enough. Late updates no longer rehash every later entry. A new epoch never reuses content, so a reload always replaces. | [Synchronization](protocol.md#54-synchronization) |
| Per-entry `rev` and differential snapshots | A reconnect within an epoch resends only entries changed since the client's `rev`, including a late update to an early entry. Costs one integer per entry in the bridge. | [Events](protocol.md#52-events) |
| Client-computed digest at snapshots and checkpoints | Comparing hashes the client was handed proves nothing about what it installed; recomputing over its own replica does, for entries and mutable `live` state alike. Costs JCS and SHA-256 on both sides, with per-entry caching; checkpoints are debounced. | [Verification](protocol.md#55-verification) |
| Full resubscription after a digest mismatch | Once content is known to differ, no part of the replica can be trusted cheaply; a full snapshot is the one simple repair. Partial repair would need per-part digests without a demonstrated benefit. | [Verification](protocol.md#55-verification) |
| Per-scope atomicity only | Events are per scope; promising cross-scope atomicity (for example a session's installation across `global` and its own scope) would be untrue. Clients tolerate the intermediate state. | [Several clients](protocol.md#56-several-clients) |
| Operation admission by `expect`, IDs chosen by the client | `expect` serializes competing operations with no lock or request table. A client-chosen `id` makes a repeat harmless and, with the recorded `after`, lets a lost answer be resolved from the replica in the common cases. Two or more operations later it is unknown, which is left to the user. | [Admission](protocol.md#64-admission), [repeats](protocol.md#66-repeats-races-and-lost-answers) |
| Every operation answers `202`; results live in the document | One result path for every client, including ones that did not write. `origin` identifies a created session; `catalog.pages` keeps listing results. No HTTP response is held open for minutes. | [Response](protocol.md#63-response) |
| `fork` also occupies `global` | One installation at a time makes a not-yet-installed session's resources and notifications attributable without a provisional owner (memory model §9). Forks wait for a create or login. | [Kinds](protocol.md#62-kinds) |
| Controls and close are actions, also during a prompt | ACP allows them while the Agent generates; they need no exclusive slot. Their responses apply to the session generation they were sent for. | [Actions](protocol.md#7-actions) |
| Interaction answers at most once, independently | Each Agent request has its own response; two clients may answer two interactions, and of two answers to one, exactly one is delivered. | [Actions](protocol.md#7-actions) |
| Content-addressed attachments | A separately read block can be verified against its reference; prompts can reuse a block by reference. | [Reads](protocol.md#8-reads) |
| Diagnostics optional and bounded | Useful, but not session history and not part of basic ACP conformance. | [Diagnostics](protocol.md#47-diagnostics) |

## 4. Acceptance

The original cases 1–37 are maintained in [Protocol acceptance](protocol-acceptance.md)
(`a01`–`a37`); layer-1 cases are in [audit §9.4](acp-fidelity-audit.md#94-direct-acceptance)
and [memory model §10](bridge-memory-model.md#10-acceptance); layer-2 verification and
repair cases are in [protocol §12.4](protocol.md#124-independent-acceptance).

## History

Revision 2's rationale (turn log, hash chain, held responses) is at
[`7ceca79`](https://github.com/tf4fun/attyd/blob/7ceca793d0733b0c3ac62d3bcfba4b31e2c119f5/docs/client-api-redesign.md);
earlier proposals are at
[`991b053`](https://github.com/tf4fun/attyd/blob/991b0532f8f2cc321f2955a1f250e283beac1ddb/docs/client-api-redesign.md).
They explain earlier choices but do not override today's contract.
