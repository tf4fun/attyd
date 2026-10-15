# Verification

The bridge-first state-machine migration and its executable acceptance ledger are documented in
[`bridge-state-machine-tdd.md`](bridge-state-machine-tdd.md). That ledger is authoritative for ACP
lifecycle, reconnect, concurrency, and subscriber correctness; browser tests validate only the
projection after the bridge gates pass.

The browser talks to the host only through [protocol v2](protocol.md) under `/api/v2`; its
invariants and gates are listed in the [implementation plan](state-sync.md#4-invariants-and-gates).
A completed turn arrives as a log entry without its process, and writes wait for their HTTP
answer without a short deadline. The earlier
[process pagination](lazy-turn-process.md) and [HTTP timeout](http-request-timeouts.md) ledgers
describe the removed `/api/v1` API and are kept as history.

The production host and every host-level integration harness run the Rust backend. TypeScript is
used only for the browser application, shared browser protocol types, test Agents, and test
drivers; there is no second backend implementation.

## Test layers

| Layer | Command | Purpose |
| --- | --- | --- |
| Frontend and browser contract | `npm test` | Replica alignment and patches, operation writes, protocol validation and coverage, the page projection, content, prompt history, search, interaction, and accessibility cases |
| Rust backend | `npm run test:rust` | Native cases for active-turn folding/retirement, 10,000-turn zero-history regression, same-session reload success/rollback, multi-subscriber isolation, lossless event queues and burst/slow-subscriber delivery, semantic validation, CLI/MCP configuration, filesystem confinement, terminal/auth lifecycle, Agent process I/O, and MCP boundaries |
| Remote ACP transports | `npm run test:remote` | Connects the Rust binary to SDK HTTP/SSE and WebSocket ACP servers and drives it over protocol v2; verifies remote capability boundaries, cancellation, and Agent-owned absolute cwd |
| Rust-hosted black box | `npm run test:ui` | Exercises the production binary's embedded bundle and the protocol v2 stream and writes with a real stdio ACP fixture: prompts bound to their operation, interactions, history reload, and deletion |
| MCP-over-ACP protocol | `node --import tsx scripts/acp-protocol-smoke.ts` | Through protocol v2 prompts, verifies session filesystem and terminal services, elicitation, and MCP-over-ACP requests, notifications, errors, cancellation, and reconnect using SDK fixtures |
| HTTP and process boundaries | `node --import tsx scripts/server-boundary-smoke.ts` | Rejects cross-origin and untrusted Host requests on the v2 routes while same-origin, CLI and proxy writes succeed; verifies graceful shutdown with an open stream and Agent process cleanup |
| Rust coverage gate | `npm run test:coverage` | Combines native tests, the four backend smokes above on an instrumented host, and the browser suite; requires at least 85% Rust line coverage and `cargo-llvm-cov` |
| Rust memory gates | `npm run test:memory:rust`, `npm run test:memory:efficiency:rust` | Subsets of `npm run test:rust`: retained turn versions, and allocations independent of unrelated retained content |
| Standalone artifact | `npm run test:binary` | Copies only the release executable to an empty directory and verifies embedded HTML, JavaScript, health metadata, and static MIME types |
| Release metadata | `python3 tests/release-metadata.test.py` | Validates tag-derived versions, prerelease classification, branch fallback, and rejection of invalid or injected workflow values; requires Python 3.11+ and runs in CI before artifact builds |
| Release packaging | `python3 tests/package-release.test.py` | Verifies all seven target filenames, tar/zip contents, executable permissions, checksums, and failure on missing release inputs; runs on every binary runner |
| Browser interaction | `npm run test:browser` | Runs the production Rust host with the fake Agent and verifies Zed-style Agent interaction and mobile behavior in Chromium |
| Frontend development proxy | `npx playwright test dev-server.pw.ts` | Uses the built Rust host with Vite; verifies same-origin HMR, React/CSS updates, preserved drafts and sessions, API/SSE routing, and upstream failure handling |

`npm run check` runs type checking, frontend/shared tests, and the production client build. CI
runs the frontend, Rust backend, and Chromium browser suites as independent parallel jobs. Rust
unit, remote-transport, protocol v2 host-smoke, release-build, and standalone-binary checks remain
separate steps so failures identify their layer directly. See the workflow for CI gates.
Linux CI jobs pin `ubuntu-26.04`, with `ubuntu-26.04-arm` for native ARM64 binaries.
This adopts Ubuntu 26.04 explicitly ahead of the
[`ubuntu-latest` migration to Ubuntu 26.04](https://github.com/actions/runner-images/issues/14748)
starting October 19, 2026. Backend, browser, coverage, and release jobs validate the new
image; future image upgrades also require an explicit workflow change.
GNU binary smoke tests verify the build image, not compatibility with older glibc versions;
the musl archives avoid a host glibc dependency.
Conformance follows the [ACP compatibility contract](acp-coverage.md#compatibility-contract);
no test depends on a particular Agent's behavior.

The remote-transport smoke uses a local SDK mock Agent. Its first prompt deliberately
waits for cancellation; the second session's prompt returns immediately. On failure,
the harness prints the mock HTTP/Agent timeline, runtime/session snapshots, and Rust
process logs before teardown. It inherits the normal logging configuration; set
`RUST_LOG=attyd=info,agent_client_protocol=debug,agent_client_protocol_http=trace`
when extra SDK diagnostics are needed. The 10-second prompt deadline detects
stalled delivery, not model inference latency.

The smoke starts the normal Rust executable and uses the official TypeScript SDK's
HTTP/SSE server over real local TCP connections. Only Agent business responses are
mocked. Like the browser, it keeps one protocol v2 stream following `global` and the
open session, follows another session when opening another thread, and follows the
running thread again when returning to it. Holding one prompt while another session
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

For a static style review, open `/style-showcase.html` on the existing Vite development server
(normally `http://127.0.0.1:5173/style-showcase.html`). The page fetches
`web/dev/style-session.json` and reuses the normal conversation renderer, including terminal
snapshots and all ten ACP tool kinds. Edit the JSON and reload to change the samples; expand execution processes and tools
to review their output. The page needs no ACP Agent or backend requests and is not part of the
production build.

Its **审批、设置与状态** tab also offers permission content/input, form choice descriptions, URL
elicitation, grouped/boolean controls, legacy modes, context usage, and running/cancelling/late-result
examples. These use the real components with local state and no backend calls. The dedicated
`style-showcase.pw.ts` case checks those interactions at desktop/mobile widths. The
[display audit](acp-display-audit-2026-09-26.md#10-补齐实现) distinguishes these samples from full
protocol/lifecycle coverage.

`prompt-folding.pw.ts` fetches a separate set of static samples to verify the nine-line
rendered-height boundary, short labels with long Markdown URLs, headings/tables, responsive
wrapping, delayed image sizing, and keyboard access to folded links. The production
`acp-ui.pw.ts` prompt case also checks expand/collapse, search visibility, and source reuse.

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
Set `CI=1` when running Playwright locally to use its installed Chromium headless shell
and reproduce CI browser behavior. The attachment case opens text, Markdown and audio
attachments in a new tab, and checks that HTML, PDF and Word files are downloads
(`download="report.pdf"` and so on) whose bytes equal the original, with no script run
and no popup.
CI retains one diagnostic retry but fails on flaky tests, so a retry cannot turn a
first-attempt failure into a successful browser or coverage gate.

Browser assertions follow the current projection contract. A completed turn shows its prompt and
final messages; its process ended with the turn and is not shown again after a reload. Check a tool
call while its turn runs. A session terminal is a resource, not process (protocol S7): it stays in
`live.terminals` after its operation ends until the Agent releases it, as
`keeps a session terminal after its operation ends and drops its output once released` checks.

`tests/browser/browser-errors.ts` fails a case on any console error or page error. A case may
allow only an HTTP status it causes deliberately, on one same-origin path: the session list
failure case allows its injected 503 on `catalog/ops`, and the terminal-login case allows a
stale catalog refresh's 409 there. `tests/browser-errors.test.ts` guards the status, origin and
path boundaries. The ordinary browser job and instrumented coverage job use these same checks.

Terminal authentication input is sent in keystroke order, waiting for each PTY write's
HTTP response before sending the next input. The browser authentication case holds the
first keystroke in transit while typing the remaining access code and Enter, then checks
that authentication completes after releasing it. `tests/protocol-client.test.ts` covers one
input at a time per terminal, in order, independent terminals, and input whose response was lost:
it is never resent.
The native `auth_terminal::tests` gate verifies PTY input and lifecycle behavior first.

## Backend behavior

The Rust suite covers CLI and MCP configuration boundaries,
filesystem roots/read/write/context behavior, terminal and terminal-auth lifecycle, optional
capability gates, Agent-owned cwd selection, early/late update routing, large lossless relay values,
complete structured errors, large HTTP/stdio/WebSocket payloads, delayed admission, cyclic pagination, session races, complete ContentBlock validation,
prompt/usage coherence, configuration and mode references, transactional session updates,
permission and elicitation semantics, and MCP process/request races.
Protocol v2 has its own gates: `protocol::convergence_tests` compares replicas with the
published documents after every event, `protocol::hub_tests` covers cuts, queues and coalescing,
`bridge::protocol_gate_tests` runs the coordinator over a real ACP transport, and
`server::stream_tests` adds real HTTP for the stream, writes, resource actions and reads.

The bridge identifies a session by its ID only (migration step 9,
[plan §5](state-sync.md#5-migration)). Its session-lifecycle gates are behaviour
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

The replica engine and the page projection remain TypeScript because the browser runs them;
their tests are `tests/replica.test.ts`, `tests/protocol-client.test.ts` and
`tests/workspace-projection.test.ts`. Test Agents and HTTP/WebSocket ACP fixtures also remain TypeScript test
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
- Zed's missing-thread handling maps to the Rust early/late routing test
  (`buffers_pre_response_updates_and_drops_late_unknown_sessions`): updates are buffered only
  while a creation is pending (bounded by the number of pending creations) and replayed only into
  the session whose ID the creation response returns; with no creation pending, updates for an ID
  with no running session are dropped and never reach the visible thread.
- Zed's session-list work-directory tests map to the protocol v2 black-box assertion that an
  Agent-owned cwd returned by `session/list` is retained when history is loaded.

The source reference is Zed's
[`crates/agent_servers/src/acp.rs`](https://github.com/zed-industries/zed/blob/main/crates/agent_servers/src/acp.rs).
Editor workbench and NES cases are intentionally excluded from attyd's scope.
