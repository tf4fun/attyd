# Verification

The bridge-first state-machine migration and its executable acceptance ledger are documented in
[`bridge-state-machine-tdd.md`](bridge-state-machine-tdd.md). That ledger is authoritative for ACP
lifecycle, reconnect, concurrency, and subscriber correctness; browser tests validate only the
projection after the bridge gates pass.

The production host and every host-level integration harness run the Rust backend. TypeScript is
used only for the browser application, shared browser protocol types, test Agents, and test
drivers; there is no second backend implementation.

## Test layers

| Layer | Command | Purpose |
| --- | --- | --- |
| Frontend and browser contract | `npm test` | 135 reducer, event-envelope, content, prompt-history, search, interaction, accessibility, and adversarial-state cases |
| Rust backend | `npm run test:rust` | 69 native cases for semantic validation, CLI/MCP configuration, capability negotiation, response bounds, session mutation locks, pagination, filesystem confinement/context, terminal/auth-terminal lifecycle, Agent process I/O, final error relay, and MCP boundaries |
| Remote ACP transports | `npm run test:remote` | Connects the Rust binary to SDK HTTP/SSE and WebSocket ACP servers; verifies remote capability boundaries and Agent-owned absolute cwd |
| Rust-hosted black box | `npm run test:ui` | Drives real browser WebSockets, fake ACP Agents, and an MCP provider through the Rust binary; covers 733 bridge events and 328 malformed/binary frames with same-connection recovery |
| Rust coverage gate | `npm run test:coverage` | Runs native and instrumented real-binary suites and requires at least 85% Rust backend line coverage; requires `cargo-llvm-cov` |
| Standalone artifact | `npm run test:binary` | Copies only the release executable to an empty directory and verifies embedded HTML, JavaScript, health metadata, and static MIME types |
| Browser interaction | `npm run test:browser` | Runs the production Rust host with the fake Agent and verifies Zed-style Agent interaction and mobile behavior in Chromium |
| Real Agent | `npm run test:goose` | Runs Goose ACP v1 recovery and a deterministic local prompt/tool lifecycle through the Rust host without a real provider key |

`npm run check` runs type checking, frontend/shared tests, and the production client build. CI
runs the frontend, Rust backend, and Chromium browser suites as independent parallel jobs. Each
Rust unit, remote-transport, Rust-hosted black-box, release-build, and standalone-binary layer is
also a separate backend step, so failures identify their layer directly. The slower coverage and
Goose suites remain separate.

## Backend migration status

The former TypeScript bridge has been removed. Its mature behavior cases were first ported into
native Rust tests or the shared real-binary black box, then the Playwright and Goose harnesses were
moved onto the Rust executable. The Rust suite covers CLI and MCP configuration boundaries,
filesystem roots/read/write/context behavior, terminal and terminal-auth lifecycle, optional
capability gates, Agent-owned cwd selection, early/late update isolation, bounded relay values,
structured errors, cyclic pagination, session races, complete ContentBlock validation,
prompt/usage coherence, configuration and mode references, transactional session updates,
permission and elicitation semantics, and MCP process/request races.

The deterministic cross-session reducer corpus remains TypeScript because the browser state
machine is TypeScript. Test Agents and HTTP/WebSocket ACP fixtures also remain TypeScript test
infrastructure; they are upstream peers or drivers, not an attyd backend.

## Zed-derived cases

We select behavior, not Zed implementation code:

- Zed's `test_load_session_replays_notifications_sent_before_response` maps to the attachment
  half of `scripts/ui-smoke.ts`: history updates sent before `session/load` returns must appear in
  the restored thread.
- Zed's `client_capabilities_include_elicitation_without_acp_beta` maps to the Rust capability
  test: form and URL elicitation are advertised without editor/NES support.
- Zed's missing-thread handling maps to the Rust early/late routing test: updates are buffered only
  while a matching session can still be allocated, and late unknown/closed-session updates never
  leak into the visible thread.
- Zed's session-list work-directory tests map to the black-box assertion that an Agent-owned cwd
  returned by `session/list` is retained when history is loaded.

The source reference is Zed's
[`crates/agent_servers/src/acp.rs`](https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs).
Editor workbench and NES cases are intentionally excluded from attyd's scope.
