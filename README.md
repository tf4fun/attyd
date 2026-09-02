# attyd

`attyd` turns an [Agent Client Protocol (ACP)](https://agentclientprotocol.com/) agent into a web workspace, in the same spirit that `ttyd` turns a terminal command into a web service.

The product is **agent-first**, not model-first. The browser does not receive a flattened stream of chat tokens. It receives the agent's session, plans, tool calls, permissions, reasoning, configuration, usage, content blocks, and stop reasons as first-class ACP concepts.

## What we are building

The useful mental model is a lightweight, browser-based Zed agent panel—not a browser clone of Zed's editor or workbench:

- launch a local ACP v1 agent over stdio or connect to one over Streamable HTTP/SSE or WebSocket;
- negotiate capabilities with the official Rust SDK;
- create and control ACP sessions from the browser;
- render the protocol faithfully, with an inspector for every raw payload;
- implement the client side of ACP permissions, filesystem, terminals, and elicitation;
- keep the host small enough to run like a CLI utility.

The Zed reference is deliberately limited to Agent interaction: threads, message presentation, composer behavior, plans, tools, permissions, elicitation, changes, and protocol inspection. Project trees, editor tabs, text-editor surfaces, panes, and the rest of Zed's workbench are outside attyd's UI scope.

The interface borrows the composable, neutral visual language of [Vercel AI Elements](https://github.com/vercel/ai-elements), but none of its AI SDK message or streaming state. The components in `web/src/components/acp` consume ACP objects directly.

The current method-by-method acceptance ledger is maintained in [`docs/acp-coverage.md`](docs/acp-coverage.md).

Like `ttyd`, this is deliberately a single-purpose bridge. `attyd` has no users, access-control login, tenant model, provider registry, or session database. It starts the configured Agent and exposes that Agent's negotiated ACP surface to the browser. Account authentication, model/provider setup, and durable session storage remain the Agent's responsibility. attyd supports both Agent-handled ACP sign-in/logout and Agent-provided terminal authentication; the latter runs the Agent's own login command in a real PTY and keeps its scrollback ephemeral.

## Current vertical slice

The first implementation includes:

- one Bridge-owned ACP upstream connection per attyd process; browser WebSocket connections are disposable subscribers, while stdio mode launches one Agent process for the Bridge lifetime;
- ACP v1 `initialize`, `session/new`, `session/prompt`, `session/cancel`, `session/set_mode`, and `session/set_config_option`, with request-scoped prompt/control acknowledgements, serialized Agent updates, and bounded transactional replay of creation notifications that arrive before `session/new` returns;
- ACP v1 Agent-owned `authenticate` plus capability-gated `logout`, with bounded advertised methods, request-scoped acknowledgements, Zed External Agent-style sign-in UI, raw response inspection, and automatic retry of the original session restoration path after `auth_required`; stable terminal authentication is also negotiated and rendered as an embedded xterm PTY that reproduces the configured Agent invocation, appends only its advertised bounded args/environment overrides, supports input/resize/cancel, and reconnects the Agent after a zero exit;
- capability-gated `session/list`, `session/load`, `session/resume`, `session/close`, and `session/delete`, with bounded pagination, exact last-thread restoration on browser reconnect (falling back to the most recently updated session), transactional replay/switching/closing, and the Agent remaining the source of truth; the cwd sent with stdio discovery is a request filter rather than a client-enforced invariant, while every Agent-returned session retains its own portable absolute cwd; already-open threads switch locally like Zed instead of issuing a duplicate `session/load`; mobile resume performs a bounded browser-bridge ping/pong probe and a terminal connection state exposes an explicit reconnect action over the disabled composer;
- capability-gated experimental `session/fork`, preserving inherited visible context during a transactional switch while locking source-session mutations until the matching Agent response;
- a New thread workspace prompt that pre-fills attyd's startup cwd for stdio and intentionally starts blank for remote transports; remote absolute paths are sent only as ACP session cwd, while load/resume/fork keep the cwd owned by that Agent session;
- static, capability-negotiated stdio `additionalDirectories` and MCP server definitions on every created, loaded, or resumed session;
- experimental client-provided MCP-over-ACP, including bidirectional MCP requests/notifications and visible transport activity;
- streamed user, agent, reasoning, and compaction content blocks, with Zed-compatible assistant entries containing ordered thought/answer chunks and tool events forming real entry boundaries, visible ACP annotations and resource metadata, loaded and live user prompts sharing the same editable/resendable presentation, and copyable Agent responses; reasoning uses Zed Agent Panel-style automatic disclosure driven by the latest valid ACP activity—open while thought chunks stream, closed when tools/answers take over, and still manually inspectable afterward; experimental ACP compaction uses the same Agent-first lifecycle treatment—streaming summaries open live, completed/cancelled summaries collapse into a `Context compacted` disclosure, failures remain visible, and every terminal item can be manually reinspected;
- a session-bound, eight-message follow-up queue while the Agent is working, with Zed-style edit/remove/clear controls, automatic FIFO dispatch after each normal ACP turn, manual Stop pausing the queue, and Send now resuming it through standard `session/cancel` rather than an invented external-Agent steer primitive;
- Agent-advertised slash commands plus a Zed-style `@` workspace-file menu with keyboard/ARIA interaction; selected text files become capability-gated ACP embedded resources, while picker, filesystem drag-and-drop, and clipboard paste share the same bounded attachment pipeline; the same message composer can expand from auto-height to the Agent panel bounds with `Shift+Alt+Escape`, and its empty-state ↑/↓ history navigation recalls up to 100 complete ACP prompts from the active Agent thread—including media/resources replayed by load/resume—without replacing the Agent-owned session store;
- tool creation and sparse tool updates, including content, locations, diffs, terminal references, raw input, and raw output; every compact card exposes the exact ACP pending/running/completed/failed state in text as well as iconography, remains collapsed by default even for live, failed, diff, or terminal output, and preserves the user's disclosure choice across sparse updates; ACP diff content is also grouped into a Zed Agent Panel-style, read-only Changes disclosure above the composer, with bounded line rendering and no invented editor, accept/reject, or rollback operation;
- legacy plans shown as current Agent activity beside the composer and snapshotted into the thread when completed, plus capability-gated plan update/removal events with recoverable ID upserts;
- self-contained permission requests that upsert the carried tool patch even when it is the tool's first appearance, with merged tool name/kind, locations, and default-visible raw input before the exact options supplied by the Agent;
- bounded form and HTTP(S)-only URL elicitation, including full representable JSON-RPC request IDs, session/request scope separation, tool-call association, schema-valid defaults, focus entry/restoration, and RPC cancellation handling;
- URL elicitation completion notifications, kept distinct from the user's consent to open the external flow;
- session metadata, Zed-style composer-adjacent ACP context-ring/details with cumulative cost, configuration, modes, stop reasons, and validated per-turn token usage;
- stdio-only, workspace-confined, cancellable, path/content-bounded `fs/read_text_file` and `fs/write_text_file` handlers with validated 1-based read ranges; remote transports do not advertise or service attyd-host filesystem operations;
- stdio-only bounded-input/output ACP terminal lifecycle handlers with spawn acknowledgement, cancellable waiters, uint32-safe exit state, coalesced read-only UI snapshots, and a strict-first compatibility fallback for Agents such as Goose that send shell source in `command` instead of ACP's `command` + `args` argv form;
- no project editor or Next Edit Suggestion surface: the unstable ACP NES/document capabilities are deliberately not advertised because they require the editor UI that is outside attyd's Agent-interaction scope;
- Zed-style long-thread navigation from the thread pane or composer keyboard shortcuts, including deterministic top/bottom boundary buttons that cannot be interrupted by streaming layout changes, plus an accessible Agent-response context menu for selected/full-response copy, top/bottom navigation, and complete Markdown export containing semantic messages, tool/diff/terminal activity, plans, compactions, stop usage, message IDs, and ACP annotations;
- Zed Agent Panel-style thread search via `Ctrl/⌘F`, with case-sensitive, whole-word, and regular-expression modes, wrapping previous/next navigation, live match counts, and visible-range highlighting. Search follows the semantic Agent conversation: collapsed thought/tool bodies stay out of results until opened, while duplicate raw ACP inspectors are always excluded;
- an expandable raw ACP payload beside every semantic presentation.

`attyd` intentionally does not provide its own durable session storage, application access authentication, authorization, or multi-tenancy. ACP Agent authentication is different: the Agent advertises the method and owns the complete account flow. For terminal methods in stdio mode, attyd necessarily transports live terminal input to that Agent process but does not persist it or turn it into application credentials. Remote transports do not advertise terminal authentication because attyd cannot reproduce a remote server's launch command; Agent-handled authentication still works. MCP servers cannot be configured from the UI. Optional controls only appear when the Agent advertises the corresponding ACP capability. Workspace cwd is chosen per new thread; other session setup remains static CLI configuration. Stdio/HTTP/SSE MCP definitions are handed to the Agent; only a configured ACP-transport provider is launched by attyd, because that transport explicitly makes the ACP client the provider.

## Run it

Build requirements: Rust 1.88 or newer, Node.js 20 or newer, and an ACP v1 agent. Node is used to
compile the TypeScript frontend; the resulting production executable does not require Node.

```bash
npm install
npm run dev
```

The development command builds the TypeScript frontend, starts the Rust host with
`./bin/goose acp` by default, and serves the UI at `http://127.0.0.1:7331`.

To run another ACP agent:

```bash
npm run dev -- -- your-agent acp
```

To connect to an ACP Streamable HTTP/SSE or WebSocket endpoint:

```bash
npm run dev -- -t http -- http://127.0.0.1:3284/acp
npm run dev -- -t ws -- ws://127.0.0.1:3284/acp
```

For Goose's unauthenticated local serve mode:

```bash
./bin/goose serve --dangerously-unauthenticated
npm run dev -- -t ws -- ws://127.0.0.1:3284/acp
```

Goose's default authenticated serve mode requires an upstream authorization header, which this intentionally unauthenticated attyd CLI does not configure yet.

Build and run the production server:

```bash
npm run build
./target/release/attyd -- your-agent acp
```

`target/release/attyd` embeds the frontend assets and is the only runtime artifact.

Every CI run also publishes self-contained Linux archives for four targets. Each
archive contains the `attyd` executable and its `attyd.sha256` checksum:

- `attyd-linux-x86_64-gnu.tar.gz`
- `attyd-linux-aarch64-gnu.tar.gz`
- `attyd-linux-x86_64-musl.tar.gz`
- `attyd-linux-aarch64-musl.tar.gz`

CLI options:

```text
attyd [options] -- <agent-command> [args...]
attyd -t http [options] -- <http(s)://acp-endpoint>
attyd -t ws [options] -- <ws(s)://acp-endpoint>

-H, --host <host>   Bind address (default: 127.0.0.1)
-p, --port <port>   HTTP port (default: 7331)
-c, --cwd <path>    stdio workspace default and local filesystem boundary
-t, --transport <transport>
                     Agent transport: stdio, http (Streamable HTTP/SSE), or ws
                     (default: stdio)
    --add-dir <path> Additional stdio workspace root (repeatable)
    --mcp-config <file>
                     Static JSON array of MCP server definitions (repeatable)
    --read-only     Do not advertise or permit fs/write_text_file
```

For example:

```bash
./target/release/attyd \
  --add-dir /absolute/path/to/shared \
  --mcp-config ./mcp.json \
  -- your-agent acp
```

New thread asks for an absolute Agent workspace. In stdio mode it starts with `--cwd` (or attyd's startup directory); with HTTP/WS it starts blank because the path belongs to the remote Agent host. Remote `session/list` results retain their own cwd, so load/resume/fork do not substitute the attyd host path.

`mcp.json` may be an array, or an object with an `mcpServers` array. Commands must be absolute; remote transports must use HTTP(S). For stdio/HTTP/SSE, configuration is sent only to the Agent as ACP session setup. For ACP transport, the Agent receives only `type`, `name`, and `serverId`; the launch command and environment stay private in the attyd host. Browser metadata contains only names and transport types.

```json
{
  "mcpServers": [
    {
      "name": "local-tools",
      "type": "stdio",
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "env": [{ "name": "TOKEN", "value": "secret" }]
    },
    {
      "name": "remote-tools",
      "type": "http",
      "url": "https://example.test/mcp",
      "headers": [{ "name": "Authorization", "value": "Bearer secret" }]
    },
    {
      "name": "client-tools",
      "type": "acp",
      "serverId": "client-tools-v1",
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "env": [{ "name": "TOKEN", "value": "host-private" }]
    }
  ]
}
```

Startup fails before opening a session when the Agent does not advertise the capability required by `--add-dir`, HTTP MCP, SSE MCP, or ACP-transport MCP. This keeps unsupported configuration explicit instead of silently dropping it.

## Architecture

```text
Browser / React + TypeScript
  │  typed attyd WebSocket events
  ▼
Rust host (single executable)
  ├─ ACP client and capability negotiation
  │    ├─ NDJSON/stdin+stdout ── local ACP v1 Agent
  │    ├─ POST + SSE ────────── remote ACP v1 Agent
  │    └─ WebSocket ─────────── remote ACP v1 Agent
  ├─ bounded in-memory ACP v1 runtime replay, keyed by sessionId
  ├─ stdio-only Agent terminal-auth PTY ── configured Agent command + advertised args/env
  ├─ permission / elicitation rendezvous
  ├─ configured-root filesystem implementation
  ├─ managed terminal processes
  └─ ACP-transport adapter ── MCP JSON-RPC/stdin+stdout ── configured MCP server
```

There are two deliberately separate protocols:

1. ACP between the attyd host and the agent. The official `agent-client-protocol` Rust SDK owns its wire schema and transport.
2. A small typed WebSocket bridge between the host and browser. Its events carry original ACP values rather than inventing a second agent abstraction.

This separation keeps process execution, files, and terminal handles on the trusted host while allowing the UI to reconnect or evolve independently.

The Agent remains the only durable authority. The Rust Bridge keeps a bounded, non-persistent
runtime journal only for sessions currently open in that ACP connection, including active turns,
streamed updates, terminal snapshots, permissions, and elicitations. Losing or reloading a browser
does not cancel those turns: a new subscriber atomically receives the runtime projection before live
events. Exiting attyd discards the journal; when the Agent advertises replay, the next process
reconstructs history through its `session/list` plus `session/load`/`session/resume` capabilities.
An Agent without those methods cannot be recovered across a Bridge restart without adding durable
client storage, which attyd deliberately does not do. Different `sessionId`s may run concurrently;
prompt and mutation exclusion remains per session, not global.

The repository keeps the executable as a conventional Rust binary crate while separating the
build-time web application clearly:

```text
src/           Rust binary source (`src/main.rs`)
web/           Vite entry point and React source
shared/        browser protocol types shared with test drivers
scripts/       integration and coverage drivers
tests/         TypeScript, browser, and fixture tests
```

## Verification

```bash
npm run typecheck
npm test
npm run test:rust
npm run test:remote
npm run test:coverage
npm run test:goose
npm run test:ui
npm run test:browser
npm run build
```

The complete test-layer map, including the selected Zed regression behaviors, is in
[`docs/testing.md`](docs/testing.md).

`npm test` covers the TypeScript frontend and shared browser contract: ACP event parsing and reduction, Zed-compatible message/tool/plan presentation, interaction focus, queued prompts, retry state, attachments and prompt history, thread search, session isolation, runtime event envelopes, and deterministic adversarial reducer inputs. Backend semantics live in Rust unit tests and the real-binary black-box suites rather than a second implementation. `npm run test:goose` launches the bundled real Goose with isolated temporary config/data/state directories. It verifies ACP v1 initialization, the expected unconfigured-provider `session/new` error, subsequent `session/list` recovery, and a deterministic loopback-only prompt/tool lifecycle through both the raw SDK and the Rust host/event-parser/browser-reducer path without an external model endpoint or real API key.

`npm run test:ui` starts the Rust binary on an ephemeral loopback port, serves its embedded production bundle, and replays every real bridge event through the production browser reducer. It verifies pre-response session creation/fork replay, Agent-owned history cwd and configuration, semantic prompt stop/usage, structured request failures, all ACP content-block variants, live thought/tool/answer activity, read-only aggregation of Agent-reported diffs, live terminal-backed tool calls with output/exit/release presentation and cancelled-wait recovery, cancellable filesystem reads/pre-mutation writes, form elicitation, URL consent/completion/session-close abortion, transactional session fork/close/delete, cached background-session isolation, context compaction, bounded incomplete Agent stdout with final error delivery, 328 malformed/binary WebSocket frames followed by same-connection recovery, and complete browser-bridge → fake Agent → ACP-transport MCP round trips including ordered connect cancellation, message cancellation, the 128-request pending bound, provider exit/disconnect recovery, contradictory error/result handling, and secret non-disclosure. For the interactive Rust-hosted fixture, run `npm run test:browser:serve -- 7332`, open `http://127.0.0.1:7332`, and submit `structured-error-flow`, `usage-flow`, `context-window-flow`, `content-flow`, `activity-flow`, `message-actions-flow`, `review-flow`, `terminal-flow`, `terminal-cancel-flow`, `terminal-burst-flow`, `filesystem-cancel-flow`, `form-flow`, `url-flow`, `pending-url-flow`, `background-flow`, `compaction-flow`, `mcp-flow`, `mcp-cancel-flow`, or `mcp-lifecycle-flow`; type `/` for Agent commands or `@` to search workspace text files. The sidebar's MCP-over-ACP inspector shows the initialize/echo/notification/reverse-request lifecycle; provider command and environment values must not appear in browser state.

`npm run test:browser` launches a fresh production Rust Bridge/fake-Agent fixture for each real-Chromium Playwright case, so the product's process-lifetime runtime sessions cannot leak across tests. It drives permission and form ACP interactions, verifies the permission subject exposes the live tool's name, location, and default-visible raw input before approval, checks first-action focus plus composer restoration, exercises queued follow-ups including edit and Send now cancellation followed by FIFO dispatch, verifies `auth_required` first-action focus followed by Agent-handled authentication and real-PTY terminal authentication/reconnect, restored session history, close-before-delete ordering for both current and switched-away sessions, capability-gated logout, and reauthentication without client-defined credential fields, sends clipboard-image, dropped-file, and `@`-selected workspace context through the real Agent prompt, recalls the complete attachment prompt through Zed-style ↑/↓ history, expands the read-only Agent diff review without editor actions, verifies live thinking plus textual tool lifecycle/default rich-content disclosure, inspects structured ACP error data and retries the exact failed prompt without exposing an editor surface, exercises the mouse/keyboard Agent-response context menu plus Markdown export, searches visible semantic Agent-thread content with match navigation/highlighting and regex error handling, verifies completed ACP compaction disclosure/search behavior, renders live Agent-reported context usage/cost beside the composer with keyboard dismissal, toggles the same ACP composer into a Zed-style expanded message composer on desktop/mobile and confirms a pending permission remains visible, validates concurrent session turns, long-thread navigation, and preservation of the user's scroll position during streamed Agent output, exercises mobile sidebar Escape/focus behavior, rejects browser console/page errors, and detects desktop/mobile horizontal layout overflow. Local runs use an installed Chrome; CI installs Playwright Chromium and retains traces, screenshots, and an HTML report on failure. The GitHub Actions workflow runs type/unit/build, bridge UI smoke, and this rendering-engine layer from a clean `npm ci` install.

## Security boundary

The default bind address is loopback. `attyd` intentionally has no authentication layer. `--host 0.0.0.0` exposes an Agent capable of reading files and running commands, so only use it behind infrastructure you trust or inside a trusted network.

In stdio mode, filesystem calls and `@`-mentioned workspace context are limited to `--cwd` and any explicit `--add-dir` roots, including real-path checks that reject traversal and symlink escapes. Context search scans at most 50,000 entries, skips common generated/dependency directories, returns at most 24 supported text files, and embeds at most 3 MiB. Terminal working directories have the same boundary. Remote modes leave filesystem and terminal execution on the Agent host and never map remote session paths onto the attyd host. `--read-only` disables ACP file writes, but it does not make the agent's own tools or terminal commands read-only; stronger isolation belongs in a container or OS sandbox around the launched agent.

Terminal authentication uses a separate real PTY and never invokes a shell: attyd reuses the exact configured Agent executable/base argv, appends at most 256 bounded Agent-advertised arguments, and applies only bounded, valid-name environment overrides. One request may run per Bridge connection; input chunks, dimensions, and total output are bounded, and the PTY is killed on explicit cancel or Bridge shutdown. Input is not logged or durably stored by attyd, but the Agent controls terminal echo, so anything it echoes may appear in the ephemeral browser scrollback. A zero exit causes a fresh WebSocket/Agent initialization as required by ACP.

ACP text-file reads and writes are capped at 4,000,000 UTF-8 bytes, and filesystem paths are capped before resolution or error rendering. Invalid ranges and oversized writes fail before touching the target file.

Prompt and Agent-output binary content blocks are limited to 3 MiB decoded data and require canonical base64 plus a valid media MIME type. Resource URIs remain protocol-faithful (`file:`, `urn:`, and custom absolute schemes are preserved), but the UI only turns HTTP(S) resource links into clickable navigation. Invalid Agent content is reported and dropped without ending the active prompt.

The browser bridge has a 5 MiB hard message limit in both directions. Complete Agent values relayed to the browser are checked at 4,000,000 bytes—including opaque `_meta`—before session or control state is committed; browser-supplied identifiers and paths are length-bounded before they can be echoed in errors or acknowledgements.
