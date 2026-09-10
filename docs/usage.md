# Usage

`attyd` is a web client for ACP agents. Install and configure the Agent you want
to use separately, then give attyd its ACP command or endpoint. Available session
controls depend on the capabilities that Agent advertises.

## Build prerequisites

To build from source, install:

- **Rust 1.88+ and Cargo** through [rustup](https://rust-lang.org/tools/install/).
  Cargo compiles the Rust host.
- **Node.js with npm** from [Node.js downloads](https://nodejs.org/en/download).
  Node.js 22 LTS (22.12+) or 24 LTS is recommended; the full supported ranges are in
  [`package.json`](../package.json). Node builds the web interface.
- **Git**, to clone the repository, and a **C compiler/linker** for native dependencies.
  On Debian/Ubuntu, install `build-essential`; on macOS, run `xcode-select --install`.
  See the [Rust installation guide](https://doc.rust-lang.org/book/ch01-01-installation.html)
  for platform details.

After installation, open a new terminal and check that the tools are on your PATH:

```bash
rustc --version
cargo --version
node --version
npm --version
```

These are build dependencies. The compiled attyd executable requires neither the
Rust toolchain nor Node.js; your chosen Agent may have separate runtime requirements.

## Run from source

With the [build prerequisites](#build-prerequisites) installed:

```bash
git clone https://github.com/tf4fun/attyd.git
cd attyd
npm ci
npm run dev -- -- your-agent acp
```

Replace `your-agent acp` with your installed Agent's command. This builds the
frontend and Rust host, then serves the workspace at `http://127.0.0.1:7331`.
Pass the Agent command explicitly; attyd does not choose or install one for you.

## Release binaries

When a release is available, download an archive from
[GitHub Releases](https://github.com/tf4fun/attyd/releases) that matches your Linux
architecture: `x86_64` or `aarch64`. The release workflow produces GNU and musl
variants; musl builds avoid a dependency on the host's glibc version.

For example, after downloading the Linux x86_64 musl archive:

```bash
tar -xzf attyd-linux-x86_64-musl.tar.gz
./attyd --version
./attyd -- your-agent acp
```

Rust, Cargo, Node.js, and npm are not needed to run the attyd binary. Install your
Agent and its dependencies separately, or use a remote Agent endpoint. For macOS,
build from source; the current release workflow packages Linux binaries only.

## Standalone build

After cloning the repository and running `npm ci`, build and run the release executable:

```bash
npm run build
./target/release/attyd -- your-agent acp
```

`target/release/attyd` embeds the frontend assets. Keep the Agent and any tools it
needs available on the machine where you run it.

## Remote agents

attyd supports Streamable HTTP/SSE and WebSocket endpoints through the pinned
SDK. For protocol compatibility, stdio is ACP's standard transport; Streamable
HTTP is a draft transport and WebSocket is a custom transport.

Supply exactly one endpoint URL for Streamable HTTP/SSE or WebSocket:

```bash
./target/release/attyd -t http -- http://127.0.0.1:3284/acp
./target/release/attyd -t ws -- ws://127.0.0.1:3284/acp
```

HTTPS and WSS URLs are also supported. The CLI currently has no option for an
upstream authorization header. Agent account sign-in through ACP is separate
from authentication required by an HTTP or WebSocket endpoint.

## CLI options

Place attyd options before `--` and the Agent command or endpoint after it.

| Option | Purpose |
| --- | --- |
| `-H, --host <address>` | Bind address; defaults to `127.0.0.1`. |
| `-p, --port <port>` | HTTP port; defaults to `7331`. |
| `-c, --cwd <path>` | Default stdio session directory and configured local filesystem root; defaults to the launch directory. |
| `-t, --transport <transport>` | `stdio` (default), `http` (Streamable HTTP/SSE), or `ws`. |
| `--add-dir <path>` | Additional stdio workspace root; repeatable and unavailable with remote transports. |
| `--mcp-config <file>` | Static MCP configuration file; repeatable. |
| `--session-unobserved-timeout <seconds>` | Close an unobserved session after this interval; defaults to `1800` (30 minutes). Any negative value disables recycling; `0` closes immediately. Requires Agent close support. |
| `--read-only` | Disable attyd's ACP `fs/write_text_file` capability and handler. |
| `--allowed-origin <origin>` | Allow a browser origin and its hostname for a reverse proxy or custom domain; repeatable. |
| `--help`, `--version` | Show CLI help or the executable version. |

Relative CLI paths resolve against the directory where attyd was launched.

## Projects and sessions

The homepage groups sessions by the working directory reported by the Agent.
**New project** asks for an absolute working directory and creates its first
session. A project is a group of sessions, not a separate stored object.

Open a project to browse its sessions. **New session in project** pre-fills that
project's directory; the title dropdown switches between its sessions. The back
button takes a conversation to its project and a project to the homepage.

- `/projects/<encoded-cwd>` opens a project.
- `/projects/<encoded-cwd>/sessions/<sessionId>` opens a conversation.
- Legacy `/sessions/<sessionId>` links resolve to the Agent's actual project.
  A session that no longer exists returns to the homepage.

The homepage's new-project dialog starts with `--cwd` for stdio and an empty
directory for remote Agents. Remote paths refer to the Agent's machine.
Existing sessions keep their Agent-owned directory when loaded or forked;
discovery is not filtered by attyd's startup directory. A known project/session
link can load history even when the Agent does not provide a session list.

Opening a cold session uses `session/load` when available, otherwise negotiated
`session/resume`. History is best effort: Agent replay takes priority, followed by
available memory context, then a visible missing-history notice. A successful
empty replay remains authoritative. Resume and fork do not require load support;
a failed history lookup after attachment does not undo a successful resume or fork.
A branch may display a labelled source snapshot without changing the Agent's context.

Projects and sessions are ordered by recent activity. Homepage search and counts
cover loaded metadata; use **Load more** to discover additional projects.
Project and conversation routes fetch further metadata pages as needed.

**Close thread** asks for confirmation, including for idle sessions: closing may
stop Agent tasks and managed terminal processes, including development servers.
Only a successful Agent close clears local messages and terminal output. Detached
services can outlive the session. Settings may be sent while a prompt runs; the
Agent decides when a changed mode or configuration takes effect.

Leaving a conversation starts the unobserved-session timer. Only that session's
SSE observers count; project/home pages do not keep it open. Returning cancels the
timer, and leaving again starts the full interval. Output does not reset it, and
expiry may close a running task. Newly materialized sessions with no observer also
count; mere list entries do not. Failed or unsupported close retains the projection.

```bash
attyd --session-unobserved-timeout 600 -- your-agent acp # ten minutes
attyd --session-unobserved-timeout -1 -- your-agent acp # manual close only
```

Browser reconnection uses the host's in-memory session state. Restarting attyd
loses that state; durable history and restoration support belong to the Agent.

attyd does not provide a persistent conversation database, an editor with unsaved
buffers, NES/document synchronization, model-provider credential management, or
vendor-specific tool/history interpretation. Missing historical terminal output
is shown as unavailable; old commands are never re-executed to reconstruct it.

## Files and attachments

ACP file writes replace the file's contents in place. Missing files can be created,
but parent directories must already exist. attyd does not format text, add newlines,
retry failed writes or create directories automatically. Errors identify the path,
stage and OS cause. A failure after mutation starts can leave partial content;
the Agent decides how to recover. Cancellation is checked before mutation, then
an ongoing write finishes and reports its actual result.

Images and audio have previews, including embedded resources. Binary attachments
show their name, MIME type and decoded size, with a download action for available
bytes. Links without bytes open only on a user click; attyd does not fetch them to
make previews. Downloads are exports, not a persistent conversation cache.

## Terminal command semantics

For stdio Agents on Unix, ACP `terminal/create` runs through `/bin/sh` in the
requested working directory with the supplied environment overrides. Execution
uses pipes for output and `/dev/null` for standard input:

- Omitted or empty `args`: `command` is a shell script, so pipelines, redirects,
  and shell builtins work. For example, `{"command": "printf 'ok\\n'; uname -a"}`.
- Nonempty `args`: `command` names an executable or builtin, and each argument
  stays literal. `{"command": "printf", "args": ["%s\\n", "$(pwd)"]}` prints
  `$(pwd)` without evaluating it.

These examples show command fields; requests also require `sessionId`. To use a
different shell or shell-specific syntax, request it explicitly, for example
`{"command": "bash", "args": ["-c", "printf '%s\\n' \"$BASH_VERSION\""]}`.
attyd does not load the user's interactive shell configuration or retry commands
after failure. Command failures report shell output and the shell's exit status
(`127` for a standalone missing command). Failure to start the shell or enter the
working directory fails terminal creation.
This is attyd's execution contract; ACP does not prescribe a quoting algorithm.

When the main command exits, including with a nonzero status, attyd leaves
surviving background processes running. `terminal/kill` and `terminal/release`
check for completion before terminating a running command's process group;
session close and bridge shutdown also clean up groups whose main command is
still running. Release invalidates the terminal handle and preserves its output
for tool presentation. Cleanup does not cover separate process groups.

Keep services in the foreground when their lifetime should follow the terminal.
For an independent background service, redirect all three standard streams:

```sh
nohup python3 -m http.server 8000 --bind 127.0.0.1 </dev/null >server.log 2>&1 &
```

Save the PID from `$!` and stop the service yourself. Once its launching command
has ended, the terminal handle no longer manages that service. Output collection
stops after a one-second drain if inherited pipes remain open; later writes to
those pipes may fail. ACP `truncated` reports only byte-limit truncation, not a
drain timeout.

These process and pipe rules are attyd policy, not ACP requirements or a promise
to reproduce Zed's PTY behavior. See [client reference rules](testing.md#zed-derived-cases).

## MCP configuration

MCP configuration is supplied at launch, not edited in the browser:

```bash
./target/release/attyd \
  --cwd /absolute/path/to/project \
  --add-dir /absolute/path/to/shared \
  --mcp-config ./mcp.json \
  -- your-agent acp
```

`mcp.json` accepts an array or an object containing an `mcpServers` array:

```json
{
  "mcpServers": [
    {
      "name": "local-tools",
      "type": "stdio",
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"],
      "env": [{ "name": "TOKEN", "value": "replace-me" }]
    },
    {
      "name": "remote-tools",
      "type": "http",
      "url": "https://example.test/mcp",
      "headers": [{ "name": "Authorization", "value": "Bearer replace-me" }]
    },
    {
      "name": "client-tools",
      "type": "acp",
      "serverId": "client-tools-v1",
      "command": "/absolute/path/to/mcp-server",
      "args": ["--stdio"]
    }
  ]
}
```

Replace the example paths, URLs, and credential values. Commands must be
absolute paths; `http` and `sse` entries require HTTP(S) URLs. Keep configuration
files containing credentials out of version control.

Stdio, HTTP, and SSE definitions are passed to the Agent during session setup.
For experimental `acp` entries, attyd launches the MCP provider; the Agent
receives only its type, name, and server ID. Its launch command and environment
stay on the attyd host. Browser configuration metadata contains names and
transport types only.

Configuration requiring unsupported Agent capabilities, including additional
directories or HTTP/SSE/ACP MCP support, fails before opening a session.

## Deployment and trust boundaries

The default listener is loopback-only. attyd has no application login,
authorization, or tenant isolation. Anyone with access to the service can
interact with the configured Agent. Protect network access with your deployment
infrastructure, and use operating-system or container isolation where needed.
Agent account authentication does not protect access to the attyd application.

For a custom domain or TLS reverse proxy, configure the browser's exact origin:

```bash
./target/release/attyd \
  --allowed-origin https://agent.example \
  -- your-agent acp
```

An origin is an HTTP(S) scheme, hostname, and optional port, without a path,
query, or credentials. By default, Host checks accept localhost and IP literals,
and browser requests must be same-origin. Explicit origins allow a public HTTPS
origin to reach an internal HTTP listener; forwarded headers are not trusted.
These checks reduce cross-origin and DNS-rebinding exposure, not network access
by unauthenticated clients. Keep authentication and access restrictions at the
proxy or network boundary.

Configured workspace roots apply to attyd-provided local filesystem and context
operations. They do not sandbox the Agent's own tools or terminal commands.
Likewise, `--read-only` disables ACP file writes through attyd; it does not make
the Agent process or shell read-only.

Remote transports do not expose attyd-host filesystem operations, terminal
execution, or terminal authentication. Agent-handled ACP sign-in remains
available when supported.

For implementation details, see [ACP support](acp-coverage.md),
[session recovery](active-turn-runtime.md), [tool card presentation](tool-card-presentation.md),
and [testing](testing.md).

## Optional example: Goose

Goose is one possible ACP backend, not an attyd dependency. If you have already
installed and configured Goose separately, start it explicitly:

```bash
./target/release/attyd -- goose acp
```

For an already configured Goose installation's local serve mode, run these in
separate terminals:

```bash
goose serve --dangerously-unauthenticated
./target/release/attyd -t ws -- ws://127.0.0.1:3284/acp
```

Keep the unauthenticated endpoint local. A Goose endpoint requiring an upstream
authorization header cannot currently receive that header from attyd's CLI;
this is separate from Agent account sign-in and attyd application access.

## Release versions

For GitHub releases, push a `v<SemVer>` tag: `v1.2.0` produces an executable whose
`--version` reports `attyd 1.2.0`; `v1.2.0-rc.1` produces a prerelease reporting
`attyd 1.2.0-rc.1`. CI validates the tag and injects its version during compilation,
without changing package manifests or lockfiles. You do not need to bump the
Cargo/npm versions for each tag. The ACP client identification uses the same version.
Local and branch builds use the development version in `Cargo.toml` by default;
`ATTYD_BUILD_VERSION` can override it at build time, not when running the executable.
