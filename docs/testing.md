# Verification

The bridge-first state-machine migration and its executable acceptance ledger are documented in
[`bridge-state-machine-tdd.md`](bridge-state-machine-tdd.md). That ledger is authoritative for ACP
lifecycle, reconnect, concurrency, and subscriber correctness; browser tests validate only the
projection after the bridge gates pass.

Completed-turn transfer, on-demand process pagination, and the collapsible task bar have a
separate [design and acceptance ledger](lazy-turn-process.md).
Ordinary API deadlines, timeout messages, and explicit retry behavior are covered by the
[HTTP timeout ledger](http-request-timeouts.md).

The production host and every host-level integration harness run the Rust backend. TypeScript is
used only for the browser application, shared browser protocol types, test Agents, and test
drivers; there is no second backend implementation.

## Test layers

| Layer | Command | Purpose |
| --- | --- | --- |
| Frontend and browser contract | `npm test` | Reducer, event-envelope, content, prompt-history, search, interaction, accessibility, load-replacement, and adversarial-state cases |
| Rust backend | `npm run test:rust` | Native cases for active-turn folding/retirement, 10,000-turn zero-history regression, same-session reload success/rollback, multi-subscriber isolation, lossless event queues and burst/slow-subscriber delivery, semantic validation, CLI/MCP configuration, filesystem confinement, terminal/auth lifecycle, Agent process I/O, and MCP boundaries |
| Idle-retirement TDD gate | `npm run test:idle-retirement` | Paused-clock ACP regressions for refused-close retries, resume/fork idle intervals, history workflow backoff/cleanup/handoff, cross-session progress, late retired-session updates and recovery guards; see the [Red/Green contract](bridge-state-machine-tdd.md#idle-retirement-regression-gate) and [workflow Red baseline](bridge-state-machine-tdd.md#history-workflow-red-baseline) |
| Remote ACP transports | `npm run test:remote` | Connects the Rust binary to SDK HTTP/SSE and WebSocket ACP servers; verifies remote capability boundaries and Agent-owned absolute cwd |
| Rust-hosted black box | `npm run test:ui` | Exercises the production binary's embedded bundle and REST/session-SSE API with a real stdio ACP fixture, revision-guarded prompt completion, history reload, and deletion |
| MCP-over-ACP protocol | `node --import tsx scripts/acp-protocol-smoke.ts` | Verifies bidirectional requests and notifications, error preservation, cancellation, cleanup, and reconnect using SDK fixtures |
| HTTP and process boundaries | `node --import tsx scripts/server-boundary-smoke.ts` | Rejects cross-origin and untrusted Host requests; verifies graceful shutdown with an open SSE subscription and Agent process cleanup |
| Rust coverage gate | `npm run test:coverage` | Combines native tests, instrumented API/protocol suites, and browser interactions; requires at least 85% Rust line coverage and `cargo-llvm-cov` |
| Standalone artifact | `npm run test:binary` | Copies only the release executable to an empty directory and verifies embedded HTML, JavaScript, health metadata, and static MIME types |
| Release metadata | `python3 tests/release-metadata.test.py` | Validates tag-derived versions, prerelease classification, branch fallback, and rejection of invalid or injected workflow values; requires Python 3.11+ and runs in CI before artifact builds |
| Release packaging | `python3 tests/package-release.test.py` | Verifies all seven target filenames, tar/zip contents, executable permissions, checksums, and failure on missing release inputs; runs on every binary runner |
| Browser interaction | `npm run test:browser` | Runs the production Rust host with the fake Agent and verifies Zed-style Agent interaction and mobile behavior in Chromium |
| Frontend development proxy | `npx playwright test dev-server.pw.ts` | Uses the built Rust host with Vite; verifies same-origin HMR, React/CSS updates, preserved drafts and sessions, API/SSE routing, and upstream failure handling |
| Optional backend example | `npm run test:goose` | Diagnostic smoke for a separately installed Goose executable at `bin/goose`; not a protocol conformance gate |

`npm run check` runs type checking, frontend/shared tests, and the production client build. CI
runs the frontend, Rust backend, and Chromium browser suites as independent parallel jobs. Rust
unit, remote-transport, REST/SSE host-smoke, release-build, and standalone-binary checks remain
separate steps so failures identify their layer directly. See the workflow for CI gates.
Conformance follows the [ACP compatibility contract](acp-coverage.md#compatibility-contract);
backend-specific smoke tests remain optional.

The remote-transport smoke uses a local SDK mock Agent. Its first prompt deliberately
waits for cancellation; the second session's prompt returns immediately. On failure,
the harness prints the mock HTTP/Agent timeline, runtime/session snapshots, and Rust
process logs before teardown. It inherits the normal logging configuration; set
`RUST_LOG=attyd=info,agent_client_protocol=debug,agent_client_protocol_http=trace`
when extra SDK diagnostics are needed. The 10-second prompt deadline detects
stalled delivery, not model inference latency.

The smoke starts the normal Rust executable and uses the official TypeScript SDK's
HTTP/SSE server over real local TCP connections. Only Agent business responses are
mocked. Like the browser, it continuously consumes global and active-session SSE,
switches the session subscription when opening another thread, and re-subscribes
when returning to the running thread. Holding one prompt while another session
progresses represents a running Agent turn; the smoke does not inject scheduler
yields or modify Hyper. Failure timelines include connection IDs and peer ports
to distinguish requests on different sockets. Loopback
latency, the mock's immediate replies, and debug builds differ from typical remote
deployments, so failures demonstrate a supported flow rather than a production
failure rate.

ACP HTTP currently disables idle connection pooling to avoid observed stalls before
prompt delivery. RPCs use fresh connections, while SSE streams stay open. This
workaround adds a TCP handshake per RPC, plus TLS for HTTPS. With browser subscriptions
aligned as above, a Linux run using unmodified dependencies and default pooling
completed 92 passes and one prompt-delivery timeout; the workaround completed 100
passes without a timeout. These are bounded checks, not production failure rates.

A separate diagnostic run with Hyper 1.11.1 and hyper-util 0.1.20 captured an HTTP/1
dispatch handoff race under the default Tokio executor: after the sender consumed
the readiness signal, an overlapping empty-queue poll reasserted it, and receiving
the request did not clear it. Connection and socket IDs showed the active SSE
connection being stored idle and reused by a later POST, which remained queued
until the smoke timed out. The triggering poll had cooperative budget remaining;
budget exhaustion is not the explanation for this trace. Instrumentation affects
timing, and neither this trace nor the passing workaround establishes the exact
internal sequence of the original CI failure. Retain the normal concurrent-session
smoke as the regression gate; revisit pooling when a narrower mitigation or an
upstream fix has been validated against that flow.

The binary matrix uses native Linux x86_64/ARM64, macOS Intel/Apple silicon, and
Windows x86_64 runners. Every target checks `--version`, runs the standalone
HTTP/assets smoke, and packages license materials before release. The complete
backend and browser suites run on Linux; binary smoke coverage does not imply
identical OS-specific terminal behavior. See [terminal semantics](usage.md#terminal-command-semantics).
CI also checks the `dev` Cargo feature, which compiles without embedded frontend assets.
Some test commands above use POSIX shell syntax and Unix-specific fixtures;
run those suites on Linux or macOS. Windows CI validates the native build and
standalone executable.

## Interactive fixture

To explore the interface without installing an Agent or configuring a model provider:

```bash
npm ci
npm run dev -- -- node --import tsx tests/fixtures/fake-agent.ts
```

Open `http://127.0.0.1:7331`. Frontend edits update through Vite without rebuilding
the Rust host; see [local development](usage.md#local-development). For testing the
embedded production bundle and configured MCP fixture instead:

```bash
npm run build:client
ATTYD_SKIP_WEB_BUILD=1 cargo build --locked
npm run test:browser:serve -- 7332
```

Open `http://127.0.0.1:7332` for the embedded fixture. In either instance, create a
project and send one of these fixture prompts:

| Prompts | Behavior |
| --- | --- |
| `review-flow`, `activity-flow` | File changes and tool activity |
| `content-flow`, `message-actions-flow` | Rich content, message actions, and export |
| `form-flow`, `url-flow`, `pending-url-flow` | Agent questions and external flows |
| `terminal-flow`, `terminal-cancel-flow`, `terminal-burst-flow` | Terminal output, cancellation, and streaming |
| `structured-error-flow`, `background-flow` | Error recovery and background work |
| `usage-flow`, `context-window-flow`, `compaction-flow` | Usage, context, and compaction |
| `mcp-flow`, `mcp-cancel-flow`, `mcp-lifecycle-flow` | MCP-over-ACP transport behavior |

Type `/` for advertised commands or `@` for workspace context. Browser tests use
isolated instances of this SDK fixture. Local Playwright runs require installed
Chrome; CI installs Chromium. The coverage command uses the same browser setup.

## Backend behavior

The Rust suite covers CLI and MCP configuration boundaries,
filesystem roots/read/write/context behavior, terminal and terminal-auth lifecycle, optional
capability gates, Agent-owned cwd selection, early/late update isolation, large lossless relay values,
complete structured errors, large HTTP/stdio/WebSocket payloads, delayed admission, cyclic pagination, session races, complete ContentBlock validation,
prompt/usage coherence, configuration and mode references, transactional session updates,
permission and elicitation semantics, and MCP process/request races.

The deterministic cross-session reducer corpus remains TypeScript because the browser state
machine is TypeScript. Test Agents and HTTP/WebSocket ACP fixtures also remain TypeScript test
infrastructure; they are upstream peers or drivers, not an attyd backend.

## Zed-derived cases

The ACP specification and negotiated capabilities remain the acceptance standard.
Zed's external ACP client is the preferred design reference where the protocol
leaves client behavior open. Record the source revision and test the resulting
attyd behavior; Zed-specific choices do not become protocol requirements.

For terminal lifecycle, use Zed's
[`acp_thread` terminal implementation](https://github.com/zed-industries/zed/blob/ad51f6825c362d930c78a6579eecd21a06a7055d/crates/acp_thread/src/terminal.rs)
at revision `ad51f6825c362d930c78a6579eecd21a06a7055d` as a reference for command
completion and retained output. attyd keeps its pipe-based execution and
[documented background-process policy](usage.md#terminal-command-semantics);
tests must cover natural exit, explicit termination, release, and bounded output
draining independently.

Selected behavior cases:

- Zed's `test_load_session_replays_notifications_sent_before_response` maps to the Rust bridge
  attachment tests: history updates sent before `session/load` returns must appear in the restored
  thread.
- Zed's `client_capabilities_include_elicitation_without_acp_beta` maps to the Rust capability
  test: form and URL elicitation are advertised without editor/NES support.
- Zed's missing-thread handling maps to the Rust early/late routing test: updates are buffered only
  while a matching session can still be allocated, and late unknown/closed-session updates never
  leak into the visible thread.
- Zed's session-list work-directory tests map to the REST/SSE black-box assertion that an
  Agent-owned cwd returned by `session/list` is retained when history is loaded.

The source reference is Zed's
[`crates/agent_servers/src/acp.rs`](https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs).
Editor workbench and NES cases are intentionally excluded from attyd's scope.
