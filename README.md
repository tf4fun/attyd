# attyd

**Your ACP agent, in the browser.**

attyd is a lightweight web client for the [Agent Client Protocol (ACP)](https://agentclientprotocol.com/).
Connect a local or remote agent, organize conversations by project, and follow its work from one workspace.
Your agent handles models, sign-in, and saved conversations; attyd gives you the interface.

![An attyd demo session showing tool activity and file changes](docs/assets/session.png)

## What you can do

- **Move between projects and conversations.** Sessions are grouped by working directory, with direct links to each conversation.
- **Follow the work.** Read responses, inspect tool activity, and review file changes alongside the turn that produced them.
- **Stay in control.** Approve permissions, answer the agent's questions, stop a task, or queue a follow-up while it works.
- **Bring context and find it again.** Attach files and images, mention workspace files with `@`, search a conversation, and export it as Markdown.

Available features, including attachments and restoring saved sessions, depend on your agent's capabilities.
See the [compatibility guide](docs/acp-coverage.md) for supported behavior and known limits.

## Get started

Install and configure an ACP-compatible agent of your choice. To run attyd from this repository,
use Rust **1.88+** and Node.js **22.12+**:

```bash
npm ci
npm run dev -- -- your-agent acp
```

Replace `your-agent acp` with your agent's ACP launch command and arguments.
Open **http://127.0.0.1:7331**, choose **New project**, and enter a working directory to start a conversation.
Open an existing project to return to its sessions.

You can also [connect to a remote agent](docs/usage.md#remote-agents) over HTTP/SSE or WebSocket,
or [build a standalone executable](docs/usage.md#standalone-build) with the web interface included.
The built executable does not need Node.js at runtime.

## Running it safely

attyd listens on localhost by default and has no application access login.
The connected agent can read files and run commands; provide access control through your infrastructure
before making the service available remotely. See [deployment and trust boundaries](docs/usage.md#deployment-and-trust-boundaries).

## Documentation

- [Usage and configuration](docs/usage.md) — local and remote agents, CLI options, MCP, and optional backend examples.
- [ACP compatibility](docs/acp-coverage.md) — supported capabilities, experimental features, and remaining verification work.
- [Contributing](AGENTS.md) — repository structure, coding conventions, and development commands.
- [Testing](docs/testing.md) — unit, integration, browser, and real-agent checks.
- [Runtime architecture](docs/active-turn-runtime.md) — session ownership, history, and reconnect behavior.

Licensed under [Apache-2.0](LICENSE).
