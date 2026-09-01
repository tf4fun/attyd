# Backend verification

The production host is Rust, but migration keeps the mature TypeScript backend tests as a
differential contract oracle until every hardening case has a native Rust equivalent. The Node
host is not part of the release binary.

## Test layers

| Layer | Command | Purpose |
| --- | --- | --- |
| TypeScript contract oracle | `npm test` | 258 protocol, reducer, validation, filesystem, terminal, MCP, transport, race, and UI-state cases accumulated by the original backend |
| Rust unit tests | `npm run test:rust` | 69 native cases for semantic validation, CLI/MCP configuration, capability negotiation, response bounds, early/late update routing, session mutation locks, cyclic pagination, filesystem confinement/context, terminal/auth-terminal lifecycle, bounded Agent process I/O, final transport-error relay, and MCP message boundaries |
| Rust remote transports | `npm run test:rust:remote` | Connects the Rust binary to real SDK HTTP/SSE and WebSocket ACP servers; verifies remote capability boundaries and Agent-owned absolute cwd |
| Shared black-box bridge | `npm run test:ui:rust` and `npm run test:ui:node` | Runs both hosts against the same fake ACP Agents and MCP provider; the current flow observes 601 Node/603 Rust bridge events across initialize, authentication recovery, 328 targeted malformed/binary WebSocket frames with recovery, liveness, list/load/resume/new/fork, pre-response updates, mode/config, prompt/usage/error, permission, form/URL elicitation, terminal, cancellable filesystem RPC, ordered MCP connect cancellation, message cancellation, disconnect/process-exit recovery, the 128-request pending bound, close, and delete, plus isolated semantic/cyclic-pagination and oversized-Agent-line checks |
| Backend coverage parity | `npm run test:coverage:backends` | Starts from clean profiles, runs the 258-test Node oracle and 69 Rust unit tests, drives the instrumented Rust binary through the shared and remote black boxes, and fails when Rust line coverage is below that run's Node `server/shared` line coverage. The 8 MB shared hostile-line flow is skipped only under whole-binary coverage instrumentation because the same instrumented run already executes three native process-level line-limit cases. Requires `cargo-llvm-cov`. |
| Standalone artifact | `npm run test:binary` | Copies only the release executable to an empty temporary directory and verifies embedded HTML, JavaScript, health metadata, and static MIME types |
| Browser interaction | `npm run test:browser` | Zed-style Agent interaction and mobile behavior in Chromium |
| Real Agent | `npm run test:goose` | Goose ACP v1 recovery and a deterministic local prompt/tool lifecycle without a real provider key |

`npm run check` runs type checking, the TypeScript oracle, Rust unit tests, HTTP/SSE and WebSocket
remote transport smoke tests, the Rust black-box bridge, the release build, and the standalone
binary test. CI additionally runs the Chromium browser suite.

The slower coverage gate is intentionally separate from `npm run check`. The current clean result
is 83.38% lines for Node `server/shared` and 87.22% for the complete Rust backend. The comparison is
dynamic, so increasing the Node oracle's coverage raises the Rust gate automatically.

## Current Node-to-Rust parity

Native Rust tests now cover the original backend's CLI and MCP configuration boundaries,
filesystem roots/read/write/context behavior, terminal creation/scoping/output/release behavior,
interactive terminal authentication, MCP message validation, optional capability gates, Agent-owned
cwd selection, early/late update isolation, bounded Agent relay values and structured errors,
cyclic `session/list` cursors, and same-session prompt/lifecycle/control/delete races. Rust also now
validates the full ContentBlock family, prompt stop/usage coherence, mode/config definitions and
references, transactional message/tool/plan/compaction updates, permission options and tool
upserts, and form/URL elicitation request/response semantics. These validators use JavaScript
UTF-16 length semantics where the Node oracle uses string length, preventing non-BMP boundary
drift. Invalid updates do not mutate identity or consume cumulative budgets, so a valid replacement
can recover on the same ACP connection.

The shared black-box fixture runs both hosts and verifies authentication/logout recovery, malformed
input recovery, rejected early semantic replay followed by clean creation, invalid active media,
config and compaction updates followed by valid replacements, prompt usage rejection, permission
completion, load/resume, and cyclic list recovery. Rejected new/fork allocations are closed only
when the returned ID is not already active; in particular, an invalid fork response cannot close
its source session.

The deterministic cross-session reducer corpus remains in TypeScript because the browser state
machine is still TypeScript and is shared by both hosts; it is not a Rust-backend migration gap.
Filesystem RPC cancellation, MCP connect/message cancellation, disconnect/process-exit recovery,
the 128-request MCP pending bound, oversized stdio Agent lines, binary WebSocket rejection, and a
targeted raw WebSocket adversarial corpus now run against both hosts. The ordered connect-cancel
fixture writes `mcp/connect` and `$/cancel_request` in one Agent stdout batch, avoiding a false test
race against a provider that starts before a separately scheduled cancellation. No known backend
behavior case remains Node-only. Browser and Goose harnesses still use the Node reference bridge;
migrating those harnesses is the next verification-infrastructure step, not a production-runtime gap.

## Zed-derived cases

We select behavior, not Zed implementation code:

- Zed's `test_load_session_replays_notifications_sent_before_response` maps to the attachment
  half of `scripts/ui-smoke.ts`: history updates sent before `session/load` returns must appear in
  the restored thread.
- Zed's `client_capabilities_include_elicitation_without_acp_beta` maps to the Rust capability
  test: form and URL elicitation are advertised as Agent-interaction capabilities without
  advertising editor/NES support.
- Zed's missing-thread handling maps to the Rust early/late routing test: updates are buffered only
  while a matching session can still be allocated, and late updates for unknown or closed sessions
  never leak into the visible thread.
- Zed's session-list work-directory tests map to the shared smoke assertion that an Agent-owned cwd
  returned by `session/list` is retained when the history session is loaded.

The source reference is Zed's
[`crates/agent_servers/src/acp.rs`](https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs).
Editor workbench and NES cases are intentionally excluded from attyd's scope.
