# Verification

This guide owns commands, environments and fixtures. The [TDD guide](bridge-state-machine-tdd.md)
defines the testing method; the [ACP memory contract](acp-fidelity-audit.md#9-acp-to-bridge-memory-contract)
and [简易 Agent 交互协议](protocol.md) define the behavior. Existing test evidence
is recorded in [the coverage ledger](acp-coverage.md), and pending work in
[the implementation plan](state-sync.md#7-fidelity-migration-revision-2).

Verify ACP behavior and retained memory before client replication. The current
experimental code still has the [audited fidelity gaps](acp-fidelity-audit.md#2-defects);
existing assertions of lost content must change with the repair, not be treated
as target behavior. Earlier API and test designs are in [the history index](history.md).

The production host and every host-level integration harness run the Rust backend. TypeScript is
used only for the browser application, shared browser protocol types, test Agents, and test
drivers; there is no second backend implementation.

For a real Agent and browser walkthrough, use the
[feature matrix and manual acceptance guide](manual-acceptance.md). It maps stable case IDs
to actions, expected observations and related automated tests, and provides a run template.
Record manual observations separately from CI results and from capabilities the Agent
does not advertise.

## Test layers

| Layer | Command | Purpose |
| --- | --- | --- |
| Frontend and browser contract | `npm test` | Replica alignment and patches, operation writes, protocol validation and coverage, the page projection, content, prompt history, search, interaction, and accessibility cases |
| Rust backend | `npm run test:rust` | Native cases for active-turn folding/retirement, 10,000-turn zero-history regression, same-session reload success/rollback, multi-subscriber isolation, lossless event queues and burst/slow-subscriber delivery, semantic validation, CLI/MCP configuration, filesystem confinement, terminal/auth lifecycle, Agent process I/O, and MCP boundaries |
| Remote ACP transports | `npm run test:remote` | Connects the Rust binary to SDK HTTP/SSE and WebSocket ACP servers and drives it over protocol v2; verifies remote capability boundaries, cancellation, and Agent-owned absolute cwd |
| Rust-hosted black box | `npm run test:ui` | Exercises the production binary's embedded bundle and the protocol v2 stream and writes with a real stdio ACP fixture: prompts bound to their operation, interactions, history reload, and deletion |
| MCP-over-ACP protocol | `node --import tsx scripts/acp-protocol-smoke.ts` | Through protocol v2 prompts, verifies session filesystem and terminal services, elicitation, and MCP-over-ACP requests, notifications, errors, cancellation, and reconnect using SDK fixtures |
| Reference client of the tree protocol | `node --import tsx scripts/reference-client-smoke.ts` | A device that follows one output as a text window and one operation head, on a stream and by polling, beside the page's client; scenarios S1–S5 of the revision 4 design with the figures of the largest event and of the text held while one is applied (below `2(N + L)` bytes for a 1 MiB output) |
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

`npm run typecheck`, also run by `npm run check`, checks the web/shared code, TypeScript
scripts and Agent fixtures, Playwright browser tests, and ordinary unit tests matching
`tests/**/*.test.ts` and `tests/**/*.test.tsx`. The unit-test configuration inherits
the application's strict checks. Passing Vitest execution does not replace this
static type check.

The `attyd` Rust crate uses `#![deny(warnings)]`: a compiler warning fails the build,
including the native test build, the `dev` feature check and all seven release targets
in CI. This gate applies to this crate; dependency advisories are checked separately
by the audit job. Draft CI checks the test build, while ready PRs also exercise the
production configurations. CI does not currently run Clippy or a Rustfmt check;
those tools have no enforced repository baseline.

Helpers used only by Rust test fixtures are compiled with `#[cfg(test)]`. Production
protocol modules have no blanket `dead_code` allowance. Public request DTOs retained
for TypeScript generation have individual allowances explaining why the server does
not construct them; the other local exceptions retain request-lifetime RAII ownership
or completion/error provenance asserted by tests. New warnings must be removed or
given a narrow, documented exception instead of disabling a module's checks.

**Draft pull requests run the fast checks only**: release metadata, the npm and Rust dependency
audits, TypeScript, the frontend tests, and the Rust tests (`npm run test:rust`). Marking the pull
request ready for review runs the whole validation, and so does every later push to it: the
remote-transport and host smokes, the `dev` feature check, the instrumented coverage, the browser
suite and the binaries of every target. Pushes to `main` and release tags always run everything.
Work on a pull request as a draft, and mark it ready (`gh pr ready`) once the fast checks pass;
merge it when the full run is green.

Rust dependencies are audited with `cargo audit --file Cargo.lock --deny unsound` in
both draft and full CI. Unsoundness advisories must fail the check even when the audit
tool classifies them as warnings. Update the affected dependency; any necessary exception
must identify the advisory and document why it applies, rather than disabling this category.
Tokio's production and test requirements start at 1.53.3, the compatible patch for
[RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386.html).

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
[tool-card guide](tool-card-presentation.md) defines presentation behavior.
Fixture rendering is not evidence of ACP lifecycle correctness.

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

Browser assertions follow the target content contract as each migration step lands.
The existing completed-turn collapse and released-terminal removal assertions
encode defects D1 and D8; they must become retention cases when those paths are
repaired. Check retained content directly in bridge tests before testing its
collapsed and expanded presentation.

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

Run the native Rust suite for direct state, semantic validation and service
lifecycles; use real coordinator/ACP and HTTP harnesses when the failure crosses
those boundaries. The [coverage ledger](acp-coverage.md#backend-behavior) owns the
named lifecycle cases and their evidence limits; its
[current protocol chains](acp-coverage.md#protocol-chain-current-protocol) identify
the coordinator and route-level gates.

The replica engine and page projection remain TypeScript because the browser runs
them. Their tests and the independent test-Agent fixtures do not constitute a
second backend. Client agreement cannot replace the direct source-memory tests.

## Protocol publication benchmark

The opt-in CI job `protocol-benchmark` runs only for non-draft pull requests carrying
the `protocol-benchmark` label. Add the label before marking ready or pushing the next
commit. It runs `scripts/protocol-benchmark.py` on one runner, comparing `ab4f0c5` (before
the internal R3 publisher was removed in #222) with the tested head; `--baseline` selects
another revision for a future performance review.
The same ignored Rust fixture in `src/protocol/feed/perf_tests.rs` runs on both revisions;
only its `Feed::new` adapter changes for the earlier constructor. The application is cleaned
and rebuilt between revisions, while dependency artifacts are reused. Logs and a comparison table are uploaded as the
`protocol-benchmark` artifact and written to the job summary. There is no timing gate.

Each case starts with 2,000 entries and 500 turns. It measures the production
`Feed::commit_ending` after a small incoming text chunk, with 5 warm-ups and 30 samples.
This includes all publishers the Feed runs and subscriber queueing, so the baseline
includes the still-active R3 publisher. The reducer, fixture construction, assertions,
subscriber consumption and network I/O are outside measurement. The two modes are no
subscribers and one subscriber following a 4 KiB output window; its queue is consumed
between commits. Current output sizes are 4 KiB and 1 MiB; the latter also runs with
256 metadata-separated blocks, for six cases in all. Checkpoints and end-to-end Agent
latency are not measured. The earlier `tree_doc::perf_tests` fixture remains available
for separate projection/tree measurements; it is not the before/after CI comparison.
The report includes median and p95 time and requested allocation bytes/counts. Allocation
traffic is not peak memory or RSS. The older projection fixture's forced checkpoints
isolate hashing cost and do not model the normal checkpoint timer.

The optimization preserves the complete tree and its ordinary reconciliation: it still
walks the session's entries and turn index. Output views retain immutable JSON strings,
so updating one still copies its existing projected text; the append path avoids building
that text again from all blocks and allocating a growing concatenation buffer. Text-chain
caches reuse already hashed prefixes, retaining segment boundaries/states rather than
old bodies. Replacements rebuild the affected chains. Rust also reuses those chains for
window and segment reads; the browser carries only the last stable prefix of a text
through immutable append updates. Independent hash vectors and negative checkpoint tests
remain the correctness checks for both implementations.

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
