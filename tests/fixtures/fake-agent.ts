import { Readable, Writable } from "node:stream";
import { once } from "node:events";
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import * as acp from "@agentclientprotocol/sdk";

const minimal = process.argv.includes("--minimal");
const cyclicList = process.argv.includes("--cyclic-list");
const duplicateListPage = process.argv.includes("--duplicate-list-page");
const invalidListRefreshOnce = process.argv.includes("--invalid-list-refresh-once");
const slowLoad = process.argv.includes("--slow-load");
const failLoadOnce = process.argv.includes("--fail-load-once");
const failLoadAlways = process.argv.includes("--fail-load-always");
const invalidLoadModeOnce = process.argv.includes("--invalid-load-mode-once");
const slowFork = process.argv.includes("--slow-fork");
const slowClose = process.argv.includes("--slow-close");
const failCloseOnce = process.argv.includes("--fail-close-once");
const slowDelete = process.argv.includes("--slow-delete");
const failDeleteOnce = process.argv.includes("--fail-delete-once");
const requireCloseBeforeDelete = process.argv.includes("--require-close-before-delete");
const slowControl = process.argv.includes("--slow-control");
const failControlOnce = process.argv.includes("--fail-control-once");
const raceNew = process.argv.includes("--race-new");
const earlyNewUpdates = process.argv.includes("--early-new-updates");
const invalidEarlyNewModeOnce = process.argv.includes("--invalid-early-new-mode-once");
const invalidEarlyContentOnce = process.argv.includes("--invalid-early-content-once");
const earlyForkUpdates = process.argv.includes("--early-fork-updates");
const oversizedInitialize = process.argv.includes("--oversized-initialize");
const oversizedInitializeError = process.argv.includes("--oversized-initialize-error");
const oversizedNewResponseOnce = process.argv.includes("--oversized-new-response-once");
const oversizedConfigResponseOnce = process.argv.includes("--oversized-config-response-once");
const oversizedForkResponseOnce = process.argv.includes("--oversized-fork-response-once");
const forkSourceIdOnce = process.argv.includes("--fork-source-id-once");
const invalidNesStartIdOnce = process.argv.includes("--invalid-nes-start-id-once");
const oversizedNesStartResponseOnce = process.argv.includes(
  "--oversized-nes-start-response-once",
);
const invalidNesSuggestionIdOnce = process.argv.includes(
  "--invalid-nes-suggestion-id-once",
);
const invalidNesCursorOnce = process.argv.includes("--invalid-nes-cursor-once");
const authRequiredAtStart = process.argv.includes("--auth-required");
const invalidTerminalAuth = process.argv.includes("--invalid-terminal-auth");
const terminalAuthRequired = process.argv.includes("--terminal-auth-required");
const terminalLogin = process.argv.includes("--terminal-login");
const oversizedStdoutLine = process.argv.includes("--oversized-stdout-line");
const terminalAuthFile = process.env.ATTYD_FAKE_AUTH_FILE;
const disconnectCancelFile = process.env.ATTYD_FAKE_DISCONNECT_CANCEL_FILE;
const processMarkerFile = process.env.ATTYD_FAKE_PROCESS_MARKER_FILE;

if (processMarkerFile) writeFileSync(processMarkerFile, `${process.pid}\n`);

if (oversizedStdoutLine) {
  await new Promise<void>((resolve) => setImmediate(resolve));
  let remaining = 8_000_001;
  const chunk = "x".repeat(64 * 1024);
  while (remaining > 0) {
    const output = chunk.slice(0, Math.min(chunk.length, remaining));
    remaining -= output.length;
    if (!process.stdout.write(output)) await once(process.stdout, "drain");
  }
  await new Promise<never>(() => {});
}

if (terminalLogin) {
  process.stdout.write("\u001b[1;32mFake Agent terminal sign-in\u001b[0m\r\nEnter access code: ");
  process.stdin.setEncoding("utf8");
  let input = "";
  for await (const chunk of process.stdin) {
    input += chunk;
    if (!input.includes("\n") && !input.includes("\r")) continue;
    if (input.trim() === "open-sesame" && terminalAuthFile) {
      writeFileSync(terminalAuthFile, "authenticated\n", { mode: 0o600 });
      process.stdout.write("\r\n\u001b[32mSigned in.\u001b[0m\r\n");
      process.exit(0);
    }
    process.stdout.write("\r\n\u001b[31mInvalid access code.\u001b[0m\r\n");
    process.exit(1);
  }
  process.exit(1);
}
let configuredMcpServers: acp.McpServer[] = [];
const observedNesRejects: acp.RejectNesNotification[] = [];
const observedNesEvents: string[] = [];
const observedSessionCloses: string[] = [];
const deletedSessions = new Set<string>();
const crossWorkspaceSessions = process.argv.includes("--cross-workspace-sessions");
const observedMcpNotifications: acp.MessageMcpNotification[] = [];
let closeAttempts = 0;
let deleteAttempts = 0;
let controlAttempts = 0;
let newAttempts = 0;
let loadAttempts = 0;
let listAttempts = 0;
let nesStartAttempts = 0;
let nesSuggestAttempts = 0;
let nesCloseAttempts = 0;
let forkAttempts = 0;
let authenticated = terminalAuthRequired
  ? terminalAuthFile != null && existsSync(terminalAuthFile)
  : !authRequiredAtStart;
const structuredErrorAttempts = new Map<string, number>();
const sessionHistory = new Map<string, acp.SessionUpdate[]>();
let finishDisconnectPrompt: (() => void) | undefined;

const agent = acp
  .agent({ name: "attyd-test-agent" })
  .onRequest(acp.methods.agent.initialize, ({ params }) => {
    if (
      params.clientCapabilities.nes != null ||
      params.clientCapabilities.positionEncodings != null
    ) {
      throw acp.RequestError.invalidParams(
        undefined,
        "attyd must not advertise editor/NES capabilities",
      );
    }
    if (
      (terminalAuthRequired || invalidTerminalAuth) &&
      params.clientCapabilities.auth?.terminal !== true
    ) {
      throw acp.RequestError.invalidParams(
        undefined,
        "attyd must advertise terminal authentication support",
      );
    }
    if (oversizedInitializeError) {
      throw new acp.RequestError(-32_000, "x".repeat(100_000));
    }
    return {
      protocolVersion: acp.PROTOCOL_VERSION,
      agentCapabilities: minimal
        ? { loadSession: false }
        : {
            loadSession: true,
            promptCapabilities: { image: true, audio: true, embeddedContext: true },
            mcpCapabilities: { http: true, sse: true, acp: true },
            nes: {
              events: {
                document: {
                  didOpen: {},
                  didChange: { syncKind: "incremental" },
                  didClose: {},
                  didSave: {},
                  didFocus: {},
                },
              },
              context: {
                recentFiles: { maxCount: 2 },
                editHistory: { maxCount: 2 },
                userActions: { maxCount: 2 },
                openFiles: {},
              },
            },
            positionEncoding: "utf-16",
            auth: { logout: {} },
            sessionCapabilities: {
              list: {},
              delete: {},
              fork: {},
              additionalDirectories: {},
              resume: {},
              close: {},
            },
          },
      agentInfo: { name: "attyd-test-agent", version: "1.0.0" },
      authMethods: invalidTerminalAuth
        ? [{
            id: "terminal-login",
            name: "Terminal login",
            type: "terminal" as const,
            args: ["--login"],
            env: { "INVALID-NAME": "value" },
          }]
        : terminalAuthRequired
          ? [{
              id: "terminal-login",
              name: "Sign in in terminal",
              description: "Uses the Agent's interactive login command",
              type: "terminal" as const,
              args: ["--terminal-login"],
              ...(terminalAuthFile
                ? { env: { ATTYD_FAKE_AUTH_FILE: terminalAuthFile } }
                : {}),
            }]
        : [{
            id: "agent-login",
            name: "Continue with Fake Agent",
            description: "Uses the Agent-owned test account",
          }],
      ...(oversizedInitialize ? { _meta: { padding: "x".repeat(4_000_000) } } : {}),
    };
  })
  .onRequest(acp.methods.agent.authenticate, ({ params }) => {
    if (params.methodId !== "agent-login") {
      throw acp.RequestError.invalidParams(undefined, "Unknown authentication method");
    }
    authenticated = true;
    return { _meta: { authenticatedBy: params.methodId } };
  })
  .onRequest(acp.methods.agent.logout, () => {
    authenticated = false;
    if (terminalAuthRequired && terminalAuthFile) rmSync(terminalAuthFile, { force: true });
    return { _meta: { signedOut: true } };
  })
  .onRequest(acp.methods.agent.session.list, ({ params }) => {
    requireAuthentication();
    listAttempts += 1;
    if (process.argv.includes("--empty-session-list")) return { sessions: [] };
    if (crossWorkspaceSessions) {
      const sessions = [
        { sessionId: "saved-session", cwd: process.cwd(), title: "Saved ACP session" },
        { sessionId: "earlier-session", cwd: "/other-workspace", title: "Earlier Agent thread" },
      ].filter((session) =>
        !deletedSessions.has(session.sessionId) &&
        (params.cwd == null || session.cwd === params.cwd),
      );
      const offset = params.cursor == null ? 0 : 1;
      return {
        sessions: sessions.slice(offset, offset + 1),
        nextCursor: sessions.length > offset + 1 ? "workspace-page-2" : undefined,
      };
    }
    if (cyclicList) {
      const page = params.cursor === "cursor-a" ? 2 : params.cursor === "cursor-b" ? 3 : 1;
      return {
        sessions: [{
          sessionId: `cyclic-session-${page}`,
          cwd: process.cwd(),
          title: `Cyclic page ${page}`,
        }],
        nextCursor: page === 1 ? "cursor-a" : page === 2 ? "cursor-b" : "cursor-a",
      };
    }
    if (duplicateListPage) {
      return {
        sessions: [{
          sessionId: "repeated-session",
          cwd: process.cwd(),
          title: params.cursor == null ? "First page" : "Second page duplicate",
        }],
        nextCursor: params.cursor == null ? "duplicate-page-2" : undefined,
      };
    }
    if (invalidListRefreshOnce && listAttempts === 2) {
      return {
        sessions: [{
          sessionId: "wrong-workspace",
          cwd: `${process.cwd()}/other-workspace`,
          title: "Must not replace the prior list",
        }],
      };
    }
    return {
      sessions: [
        {
          sessionId: "saved-session",
          cwd: process.cwd(),
          title: "Saved ACP session",
          updatedAt: "2026-08-30T08:00:00.000Z",
        },
        {
          sessionId: "earlier-session",
          cwd: process.cwd(),
          title: "Earlier Agent thread",
          updatedAt: "2026-08-20T08:00:00.000Z",
        },
      ].filter(({ sessionId }) => !deletedSessions.has(sessionId)),
    };
  })
  .onRequest(acp.methods.agent.session.load, async ({ params, client }) => {
    if (crossWorkspaceSessions && params.sessionId === "earlier-session" && params.cwd !== "/other-workspace") {
      throw acp.RequestError.invalidParams(undefined, "Load must retain the session's workspace");
    }
    loadAttempts += 1;
    if (slowLoad) await new Promise((resolve) => setTimeout(resolve, 150));
    if (failLoadOnce && loadAttempts === 1) {
      throw new acp.RequestError(-32603, "Synthetic load failure");
    }
    if (failLoadAlways) {
      throw new acp.RequestError(-32002, "Synthetic deterministic load failure");
    }
    if (invalidLoadModeOnce && loadAttempts === 1) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "current_mode_update",
          currentModeId: "ghost-mode",
        },
      });
    } else {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "current_mode_update",
          currentModeId: "plan",
        },
      });
    }
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "config_option_update",
        configOptions: [
          { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
        ],
      },
    });
    let history = sessionHistory.get(params.sessionId);
    if (history == null || history.length === 0) {
      history = [{
        sessionUpdate: "agent_message_chunk" as const,
        messageId: "loaded-message",
        content: { type: "text" as const, text: "Loaded history." },
      }];
      sessionHistory.set(params.sessionId, history);
    }
    for (const update of history) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update,
      });
    }
    return {
      modes: {
        currentModeId: "plan",
        availableModes: [{ id: "build", name: "Build" }, { id: "plan", name: "Plan" }],
      },
      configOptions: [
        { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
      ],
      _meta: {
        observedSessionCloses: [...observedSessionCloses],
        loadAttempts,
      },
    };
  })
  .onRequest(acp.methods.agent.session.resume, () => ({
    modes: {
      currentModeId: "build",
      availableModes: [{ id: "build", name: "Build" }],
    },
    _meta: { observedSessionCloses: [...observedSessionCloses] },
  }))
  .onRequest(acp.methods.agent.session.fork, async ({ params, client }) => {
    forkAttempts += 1;
    if (slowFork) await new Promise((resolve) => setTimeout(resolve, 150));
    if (earlyForkUpdates) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: "forked-session",
        update: {
          sessionUpdate: "available_commands_update",
          availableCommands: [{
            name: "fork-status",
            description: "Inspect the forked session",
          }],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: "forked-session",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "early-fork-message",
          content: { type: "text", text: "Fork initialized before acknowledgement." },
        },
      });
    }
    return {
      sessionId: forkSourceIdOnce && forkAttempts === 1
        ? params.sessionId
        : "forked-session",
      modes: {
        currentModeId: "build",
        availableModes: [{ id: "build", name: "Build" }],
      },
      _meta: {
        forkedFrom: params.sessionId,
        observedSessionCloses: [...observedSessionCloses],
        receivedSessionSetup: {
          additionalDirectories: params.additionalDirectories,
          mcpServers: params.mcpServers,
        },
        ...(oversizedForkResponseOnce && forkAttempts === 1
          ? { padding: "x".repeat(4_000_000) }
          : {}),
      },
    };
  })
  .onRequest(acp.methods.agent.session.close, async ({ params }) => {
    closeAttempts += 1;
    observedSessionCloses.push(params.sessionId);
    if (slowClose) await new Promise((resolve) => setTimeout(resolve, 150));
    if (failCloseOnce && closeAttempts === 1) {
      throw new acp.RequestError(-32603, "Synthetic close failure");
    }
    return {};
  })
  .onRequest(acp.methods.agent.session.delete, async ({ params }) => {
    deleteAttempts += 1;
    if (slowDelete) await new Promise((resolve) => setTimeout(resolve, 150));
    if (requireCloseBeforeDelete && !observedSessionCloses.includes(params.sessionId)) {
      throw new acp.RequestError(-32600, "Session must be closed before deletion");
    }
    if (failDeleteOnce && deleteAttempts === 1) {
      throw new acp.RequestError(-32603, "Synthetic delete failure");
    }
    deletedSessions.add(params.sessionId);
    sessionHistory.delete(params.sessionId);
    return {};
  })
  .onRequest(acp.methods.agent.session.new, async ({ params, client }) => {
    requireAuthentication();
    newAttempts += 1;
    const attempt = newAttempts;
    if (raceNew) {
      await new Promise((resolve) => setTimeout(resolve, attempt === 1 ? 150 : 10));
    }
    configuredMcpServers = params.mcpServers;
    const sessionId = raceNew ? `test-session-${attempt}` : "test-session";
    sessionHistory.set(sessionId, []);
    if (invalidEarlyContentOnce && attempt === 1) {
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "invalid-early-content",
          content: { type: "image", data: "AA==", mimeType: "text/html" },
        },
      });
    }
    if (earlyNewUpdates || invalidEarlyNewModeOnce) {
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "current_mode_update",
          currentModeId: invalidEarlyNewModeOnce && attempt === 1 ? "ghost" : "plan",
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "config_option_update",
          configOptions: [{
            type: "boolean",
            id: "verbose",
            name: "Verbose",
            currentValue: true,
          }],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "available_commands_update",
          availableCommands: [{
            name: "bootstrap",
            description: "Initialize the workspace",
            input: { hint: "path" },
          }],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "usage_update",
          used: 5,
          size: 100,
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "session_info_update",
          title: "Early ACP session",
          updatedAt: "2026-08-30T08:00:00.000Z",
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "early-session-message",
          content: { type: "text", text: "Session initialized before acknowledgement." },
        },
      });
    }
    return {
      sessionId,
      modes: {
        currentModeId: "build",
        availableModes: [{ id: "build", name: "Build" }, { id: "plan", name: "Plan" }],
      },
      configOptions: [
        {
          type: "boolean",
          id: "verbose",
          name: "Verbose",
          currentValue: false,
        },
      ],
      _meta: {
        observedSessionCloses: [...observedSessionCloses],
        receivedSessionSetup: {
          additionalDirectories: params.additionalDirectories,
          mcpServers: params.mcpServers,
        },
        ...(oversizedNewResponseOnce && attempt === 1
          ? { padding: "x".repeat(4_000_000) }
          : {}),
      },
    };
  })
  .onRequest(acp.methods.agent.session.setMode, async () => {
    await beforeControlResponse();
    return {};
  })
  .onRequest(acp.methods.agent.session.setConfigOption, async ({ params }) => {
    await beforeControlResponse();
    return {
      configOptions: [
        {
          type: "boolean",
          id: "verbose",
          name: "Verbose",
          currentValue: Boolean(params.value),
        },
      ],
      _meta: {
        observedSessionCloses: [...observedSessionCloses],
        ...(oversizedConfigResponseOnce && controlAttempts === 1
          ? { padding: "x".repeat(4_000_000) }
          : {}),
      },
    };
  })
  .onRequest(acp.methods.agent.nes.start, () => {
    nesStartAttempts += 1;
    return {
      sessionId: invalidNesStartIdOnce && nesStartAttempts === 1
        ? "x".repeat(1_025)
        : oversizedNesStartResponseOnce && nesStartAttempts === 1
          ? "nes-rejected-session"
          : "nes-session",
      _meta: {
        observedCloseAttempts: nesCloseAttempts,
        ...(oversizedNesStartResponseOnce && nesStartAttempts === 1
          ? { padding: "x".repeat(4_000_000) }
          : {}),
      },
    };
  })
  .onRequest(acp.methods.agent.nes.suggest, ({ params }) => {
    nesSuggestAttempts += 1;
    return {
      suggestions: [
        {
          kind: "edit" as const,
          id: invalidNesSuggestionIdOnce && nesSuggestAttempts === 1
            ? "x".repeat(1_025)
            : "nes-edit-1",
          uri: params.uri,
          edits: [
            {
              range: { start: params.position, end: params.position },
              newText: "/* NES */",
            },
          ],
          cursorPosition: invalidNesCursorOnce && nesSuggestAttempts === 1
            ? { line: 99, character: 0 }
            : { line: params.position.line, character: params.position.character + 9 },
        },
        {
          kind: "jump" as const,
          id: "nes-jump-1",
          uri: params.uri,
          position: { line: 0, character: 0 },
        },
      ],
      _meta: {
        receivedTriggerKind: params.triggerKind,
        receivedContext: params.context,
        observedRejects: [...observedNesRejects],
        observedNesEvents: [...observedNesEvents],
      },
    };
  })
  .onRequest(acp.methods.agent.nes.close, () => {
    nesCloseAttempts += 1;
    return {};
  })
  .onNotification(acp.methods.agent.document.didOpen, () => {})
  .onNotification(acp.methods.agent.document.didChange, ({ params }) => {
    observedNesEvents.push(`didChange:${params.version}`);
  })
  .onNotification(acp.methods.agent.document.didClose, () => {})
  .onNotification(acp.methods.agent.document.didSave, () => {})
  .onNotification(acp.methods.agent.document.didFocus, () => {})
  .onNotification(acp.methods.agent.nes.accept, ({ params }) => {
    observedNesEvents.push(`accept:${params.id}`);
  })
  .onNotification(acp.methods.agent.nes.reject, ({ params }) => {
    observedNesRejects.push(params);
  })
  .onRequest<acp.MessageMcpRequest, acp.MessageMcpResponse>(
    acp.AGENT_METHODS.mcp_message,
    parseMcpMessage,
    ({ params }) => ({
      roots: [{ uri: "file:///fake-agent-workspace" }],
      receivedMethod: params.method,
      receivedParams: params.params,
    }),
  )
  .onNotification<acp.MessageMcpNotification>(
    acp.AGENT_METHODS.mcp_message,
    parseMcpMessage,
    ({ params }) => {
      observedMcpNotifications.push(params);
    },
  )
  .onRequest(acp.methods.agent.session.prompt, async ({ params, client, requestId }) => {
    const promptText = params.prompt
      .filter((block) => block.type === "text")
      .map((block) => block.text)
      .join("\n");
    const history = sessionHistory.get(params.sessionId) ?? [];
    const upstreamClient = client;
    client = new Proxy(upstreamClient, {
      get(target, property) {
        if (property === "notify") {
          return async (method: unknown, notification: unknown) => {
            if (
              method === acp.methods.client.session.update &&
              typeof notification === "object" &&
              notification !== null &&
              "sessionId" in notification &&
              "update" in notification
            ) {
              const candidate = notification as {
                sessionId: string;
                update: acp.SessionUpdate;
              };
              const update = candidate.update as unknown as Record<string, unknown>;
              const content = update.content as Record<string, unknown> | undefined;
              const persistableMessage =
                (update.sessionUpdate === "agent_message_chunk" ||
                  update.sessionUpdate === "agent_thought_chunk") &&
                content?.type === "text" && typeof content.text === "string";
              const persistableTool =
                (update.sessionUpdate === "tool_call" ||
                  update.sessionUpdate === "tool_call_update") &&
                typeof update.toolCallId === "string" &&
                update.toolCallId.length > 0;
              const persistableSessionState =
                update.sessionUpdate === "available_commands_update" ||
                update.sessionUpdate === "usage_update" ||
                update.sessionUpdate === "plan" ||
                update.sessionUpdate === "compaction_update" ||
                update.sessionUpdate === "compaction_summary_chunk";
              if (
                candidate.sessionId === params.sessionId &&
                (persistableMessage || persistableTool || persistableSessionState)
              ) {
                history.push(candidate.update);
              }
            }
            return Reflect.apply(target.notify, target, [method, notification]);
          };
        }
        const value = Reflect.get(target, property, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    for (const content of params.prompt) {
      history.push({
        sessionUpdate: "user_message_chunk",
        messageId: `prompt-${requestId}`,
        content,
      });
    }
    sessionHistory.set(params.sessionId, history);
    if (promptText.includes("disconnect-flow")) {
      setTimeout(() => process.exit(0), 25);
      await new Promise(() => {});
    }
    if (promptText.includes("disconnect-cancel-flow")) {
      if (disconnectCancelFile) writeFileSync(disconnectCancelFile, "prompt\n");
      await new Promise<void>((resolve) => { finishDisconnectPrompt = resolve; });
      return { stopReason: "cancelled" };
    }
    if (promptText.includes("oversized-notification-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "relay-budget-tool",
          title: "Oversized outer metadata",
          status: "pending",
        },
        _meta: { padding: "x".repeat(4_000_000) },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "relay-budget-tool",
          title: "Accepted after oversized notification",
          status: "completed",
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("request-error-flow")) {
      throw new acp.RequestError(-32000, "Authentication required", {
        hint: "Configure the Agent provider",
        padding: "x".repeat(20_000),
      });
    }
    if (promptText.includes("error-after-output-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "error-after-output",
          content: { type: "text", text: "Output persisted before the Agent error." },
        },
      });
      throw new acp.RequestError(-32603, "Synthetic error after persisted output");
    }
    if (promptText.includes("oversized-error-data-flow")) {
      throw new acp.RequestError(-32603, "Oversized structured failure", {
        hint: "The complete data object is intentionally over the browser relay budget",
        padding: "x".repeat(300_000),
      });
    }
    if (promptText.includes("structured-error-flow")) {
      const attempts = (structuredErrorAttempts.get(params.sessionId) ?? 0) + 1;
      structuredErrorAttempts.set(params.sessionId, attempts);
      if (attempts === 1) {
        throw new acp.RequestError(-32603, "Synthetic structured failure", {
          retryHint: "Retry the same ACP ContentBlocks",
          retryAfterMs: 25,
          nested: { owner: "Agent" },
        });
      }
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "structured-error-recovery",
          content: { type: "text", text: "Recovered after structured ACP error." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-usage-flow")) {
      return {
        stopReason: "end_turn",
        usage: {
          totalTokens: 4,
          inputTokens: 3,
          outputTokens: 2,
        },
      };
    }
    if (promptText.includes("context-window-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "usage_update",
          used: 82_000,
          size: 100_000,
          cost: { amount: 1.2345, currency: "USD" },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "context-window-answer",
          content: { type: "text", text: "Context usage updated." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("usage-flow")) {
      return {
        stopReason: "max_tokens",
        usage: {
          totalTokens: 21,
          inputTokens: 13,
          outputTokens: 8,
          thoughtTokens: 3,
          cachedReadTokens: 5,
          cachedWriteTokens: 2,
        },
      };
    }
    if (promptText.includes("filesystem-cancel-flow")) {
      const fixturePath = `${process.cwd()}/.attyd-filesystem-cancel-${process.pid}.txt`;
      const largeContent = "r".repeat(3_500_000);
      let readCancelled = false;
      let writeCancelled = false;
      let originalPreserved = false;
      try {
        writeFileSync(fixturePath, largeContent);
        const readController = new AbortController();
        const reading = client.request(acp.methods.client.fs.readTextFile, {
          sessionId: params.sessionId,
          path: fixturePath,
        }, { cancellationSignal: readController.signal });
        readController.abort();
        try {
          await reading;
        } catch (error) {
          readCancelled = error instanceof acp.RequestError && error.code === -32_800;
        }

        writeFileSync(fixturePath, "stable-before-cancel\n");
        const writeController = new AbortController();
        const writing = client.request(acp.methods.client.fs.writeTextFile, {
          sessionId: params.sessionId,
          path: fixturePath,
          content: "replacement".repeat(250_000),
        }, { cancellationSignal: writeController.signal });
        writeController.abort();
        try {
          await writing;
        } catch (error) {
          writeCancelled = error instanceof acp.RequestError && error.code === -32_800;
        }
        originalPreserved = readFileSync(fixturePath, "utf8") === "stable-before-cancel\n";
      } finally {
        rmSync(fixturePath, { force: true });
      }
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "filesystem-cancel-result",
          content: {
            type: "text",
            text: JSON.stringify({ readCancelled, writeCancelled, originalPreserved }),
          },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("mcp-cancel-flow")) {
      const server = configuredMcpServers.find(
        (candidate): candidate is acp.McpServer & { type: "acp" } =>
          "type" in candidate && candidate.type === "acp",
      );
      if (!server) throw new Error("No ACP-transport MCP server was configured");

      const connected = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      const messageController = new AbortController();
      const cancelledMessage = client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        { connectionId: connected.connectionId, method: "never" },
        { cancellationSignal: messageController.signal },
      );
      await new Promise((resolve) => setTimeout(resolve, 25));
      messageController.abort();
      let messageCancelled = false;
      try {
        await cancelledMessage;
      } catch (error) {
        messageCancelled = error instanceof acp.RequestError && error.code === -32_800;
      }
      await new Promise((resolve) => setTimeout(resolve, 25));
      const cancellationResult = await client.request<
        acp.MessageMcpResponse,
        acp.MessageMcpRequest
      >(
        acp.CLIENT_METHODS.mcp_message,
        { connectionId: connected.connectionId, method: "cancellationCount" },
      );

      const pending = client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        { connectionId: connected.connectionId, method: "never" },
      );
      await new Promise((resolve) => setTimeout(resolve, 25));
      await client.request<acp.DisconnectMcpResponse, acp.DisconnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_disconnect,
        { connectionId: connected.connectionId },
      );
      let disconnectRejectedPending = false;
      try {
        await pending;
      } catch {
        disconnectRejectedPending = true;
      }

      const recovered = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      const recoveredEcho = await client.request<
        acp.MessageMcpResponse,
        acp.MessageMcpRequest
      >(
        acp.CLIENT_METHODS.mcp_message,
        {
          connectionId: recovered.connectionId,
          method: "echo",
          params: { recovered: true },
        },
      );
      await client.request<acp.DisconnectMcpResponse, acp.DisconnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_disconnect,
        { connectionId: recovered.connectionId },
      );
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "mcp-cancel-result",
          content: {
            type: "text",
            text: JSON.stringify({
              messageCancelled,
              cancellationNotificationObserved:
                typeof cancellationResult === "object" &&
                cancellationResult !== null &&
                "count" in cancellationResult &&
                cancellationResult.count === 1,
              disconnectRejectedPending,
              recoveredEcho,
            }),
          },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("mcp-lifecycle-flow")) {
      const server = configuredMcpServers.find(
        (candidate): candidate is acp.McpServer & { type: "acp" } =>
          "type" in candidate && candidate.type === "acp",
      );
      if (!server) throw new Error("No ACP-transport MCP server was configured");

      const bounded = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      const pending = Array.from({ length: 128 }, () =>
        client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
          acp.CLIENT_METHODS.mcp_message,
          { connectionId: bounded.connectionId, method: "never" },
        ).then(
          () => false,
          () => true,
        )
      );
      await waitForMcpPendingNotifications(bounded.connectionId, 128);
      const overflow = client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        { connectionId: bounded.connectionId, method: "never" },
      ).then(
        () => ({ kind: "resolved" as const, message: "" }),
        (error: unknown) => ({ kind: "rejected" as const, message: errorText(error) }),
      );
      const overflowBeforeDisconnect = await Promise.race([
        overflow,
        new Promise<{ kind: "timeout"; message: string }>((resolve) =>
          setTimeout(() => resolve({ kind: "timeout", message: "" }), 1_000)
        ),
      ]);
      await client.request<acp.DisconnectMcpResponse, acp.DisconnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_disconnect,
        { connectionId: bounded.connectionId },
      );
      const pendingRejected = (await Promise.all(pending)).filter(Boolean).length;
      await overflow;

      const exiting = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      let exitError = "";
      try {
        await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
          acp.CLIENT_METHODS.mcp_message,
          { connectionId: exiting.connectionId, method: "exit" },
        );
      } catch (error) {
        exitError = errorText(error);
      }

      const recovered = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      const recoveredEcho = await client.request<
        acp.MessageMcpResponse,
        acp.MessageMcpRequest
      >(
        acp.CLIENT_METHODS.mcp_message,
        {
          connectionId: recovered.connectionId,
          method: "echo",
          params: { recoveredAfterExit: true },
        },
      );
      await client.request<acp.DisconnectMcpResponse, acp.DisconnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_disconnect,
        { connectionId: recovered.connectionId },
      );
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "mcp-lifecycle-result",
          content: {
            type: "text",
            text: JSON.stringify({
              pendingLimitRejected: overflowBeforeDisconnect.kind === "rejected" &&
                overflowBeforeDisconnect.message.includes("128 MCP requests"),
              pendingRejected,
              exitError,
              recoveredEcho,
            }),
          },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("mcp-flow")) {
      const server = configuredMcpServers.find(
        (candidate): candidate is acp.McpServer & { type: "acp" } =>
          "type" in candidate && candidate.type === "acp",
      );
      if (!server) throw new Error("No ACP-transport MCP server was configured");
      const connected = await client.request<acp.ConnectMcpResponse, acp.ConnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_connect,
        { serverId: server.serverId },
      );
      const initialized = await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        {
          connectionId: connected.connectionId,
          method: "initialize",
          params: {
            protocolVersion: "2025-06-18",
            capabilities: {},
            clientInfo: { name: "fake-agent", version: "1.0.0" },
          },
        },
      );
      const echoed = await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        {
          connectionId: connected.connectionId,
          method: "echo",
          params: { from: "agent" },
        },
      );
      let failed: { code?: number; message: string; data?: unknown } | undefined;
      try {
        await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
          acp.CLIENT_METHODS.mcp_message,
          { connectionId: connected.connectionId, method: "fail" },
        );
      } catch (error) {
        failed = requestErrorDetails(error);
      }
      let resultAndError: { code?: number; message: string; data?: unknown } | undefined;
      try {
        await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
          acp.CLIENT_METHODS.mcp_message,
          { connectionId: connected.connectionId, method: "resultAndError" },
        );
      } catch (error) {
        resultAndError = requestErrorDetails(error);
      }
      const roundTrip = await client.request<acp.MessageMcpResponse, acp.MessageMcpRequest>(
        acp.CLIENT_METHODS.mcp_message,
        { connectionId: connected.connectionId, method: "serverRoundTrip" },
      );
      await client.notify<acp.MessageMcpNotification>(acp.CLIENT_METHODS.mcp_message, {
        connectionId: connected.connectionId,
        method: "notifications/initialized",
      });
      await client.request<acp.DisconnectMcpResponse, acp.DisconnectMcpRequest>(
        acp.CLIENT_METHODS.mcp_disconnect,
        { connectionId: connected.connectionId },
      );
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "mcp-result",
          content: {
            type: "text",
            text: JSON.stringify({ initialized, echoed, failed, resultAndError, roundTrip }),
          },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (
      promptText.includes("request-scoped-form-flow") ||
      promptText.includes("null-request-scope-flow") ||
      promptText.includes("empty-request-scope-flow")
    ) {
      const scopedRequestId = promptText.includes("null-request-scope-flow")
        ? null
        : promptText.includes("empty-request-scope-flow")
          ? ""
          : requestId;
      const resultMessageId = promptText.includes("null-request-scope-flow")
        ? "request-scoped-null-result"
        : promptText.includes("empty-request-scope-flow")
          ? "request-scoped-empty-result"
          : "request-scoped-result";
      const response = await client.request(acp.methods.client.elicitation.create, {
        requestId: scopedRequestId,
        mode: "form",
        message: "Configure the active request",
        requestedSchema: {
          type: "object",
          properties: { value: { type: "string", title: "Value" } },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: resultMessageId,
          content: { type: "text", text: `Request-scoped ${response.action}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("form-flow")) {
      const response = await client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        mode: "form",
        message: "Configure the test run",
        requestedSchema: {
          type: "object",
          required: ["name", "count", "channel", "tags", "startsAt"],
          properties: {
            name: { type: "string", title: "Name", minLength: 2, pattern: "^[A-Za-z ]+$" },
            count: { type: "integer", title: "Count", minimum: 1, maximum: 3 },
            channel: { type: "string", title: "Channel", enum: ["stable", "preview"] },
            tags: {
              type: "array",
              title: "Tags",
              minItems: 1,
              maxItems: 2,
              items: { type: "string", enum: ["fast", "safe", "verbose"] },
            },
            startsAt: { type: "string", title: "Starts at", format: "date-time" },
            confirmed: { type: "boolean", title: "Confirmed", default: true },
          },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "form-result",
          content: { type: "text", text: `Form ${response.action}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-content-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "content-recovery",
          content: { type: "image", data: "AA==", mimeType: "text/html" },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "content-recovery",
          content: { type: "text", text: "Connection survived invalid media." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("attachment-input-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "attachment-input-result",
          content: {
            type: "text",
            text: `Received prompt blocks: ${params.prompt.map(({ type }) => type).join(",")}.`,
          },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("message-actions-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "message-actions-answer",
          content: {
            type: "text",
            text: "**Context menu response.**\n\n- first item\n- second item",
          },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_thought_chunk",
          messageId: "message-actions-thought",
          content: { type: "text", text: "Considering possible follow-up work." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("stream-follow-flow")) {
      for (let index = 1; index <= 20; index += 1) {
        const update = {
          sessionUpdate: "agent_message_chunk" as const,
          messageId: "stream-follow-answer",
          content: {
            type: "text" as const,
            text: `Streamed paragraph ${index}: ${"follow the latest Agent output without competing scroll animations. ".repeat(3)}\n\n`,
          },
        };
        await client.notify(acp.methods.client.session.update, {
          sessionId: params.sessionId,
          update,
        });
        await new Promise((resolve) => setTimeout(resolve, 30));
      }
      const finalUpdate = {
        sessionUpdate: "agent_message_chunk" as const,
        messageId: "stream-follow-answer",
        content: { type: "text" as const, text: "Stream follow complete." },
      };
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: finalUpdate,
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("tool-content-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "formatted-tool-content",
          title: "Inspect formatted tool output",
          kind: "read",
          status: "completed",
          rawInput: { query: "dependencies", path: "/workspace" },
          content: [{
            type: "content",
            content: {
              type: "text",
              text: "**2 matches**\n\n- `package.json`\n- `Cargo.toml`",
            },
          }],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "tool-content-answer",
          content: { type: "text", text: "Formatted tool content complete." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("activity-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_thought_chunk",
          messageId: "activity-thought",
          content: { type: "text", text: "Inspecting the requested task." },
        },
      });
      await new Promise((resolve) => setTimeout(resolve, 650));
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "activity-tool",
          title: "Inspect workspace dependencies and generated configuration files",
          kind: "read",
          status: "in_progress",
          rawInput: {
            path: "/workspace",
            include: ["package.json", "generated configuration"],
          },
        },
      });
      await new Promise((resolve) => setTimeout(resolve, 650));
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call_update",
          toolCallId: "activity-tool",
          status: "completed",
          rawOutput: { dependencies: 12, generatedFiles: 2 },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "activity-answer",
          content: { type: "text", text: "Activity flow complete." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("review-flow")) {
      const sourcePath = `${process.cwd()}/src/config.ts`;
      const newPath = `${process.cwd()}/src/review-note.ts`;
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "review-workspace-files",
          title: "Edit workspace files",
          kind: "edit",
          status: "completed",
          locations: [{ path: sourcePath, line: 0 }],
          content: [
            {
              type: "diff",
              path: sourcePath,
              oldText: "export const theme = \"light\";\nexport const compact = false;\n",
              newText: "export const theme = \"zed\";\nexport const compact = true;\n",
            },
            {
              type: "diff",
              path: newPath,
              oldText: null,
              newText: "export const review = true;\nexport const source = \"acp\";\n",
            },
          ],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "review-flow-result",
          content: { type: "text", text: "Reported two workspace changes." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("content-flow")) {
      const blocks: acp.ContentBlock[] = [
        {
          type: "image",
          data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
          mimeType: "image/png",
          uri: "attyd://fixture/pixel.png",
        },
        { type: "audio", data: "AA==", mimeType: "audio/wav" },
        {
          type: "resource_link",
          name: "Fixture source",
          title: "ACP fixture",
          description: "A non-HTTP resource remains visible without becoming a browser link.",
          uri: "file:///workspace/fixture.ts",
          mimeType: "text/typescript",
          size: 128,
        },
        {
          type: "resource",
          resource: {
            uri: "urn:attyd:fixture:text",
            mimeType: "text/plain;charset=utf-8",
            text: "Embedded fixture text.",
          },
        },
        {
          type: "resource",
          resource: {
            uri: "mcp://fixture/binary",
            mimeType: "application/octet-stream",
            blob: "AQID",
          },
        },
      ];
      for (const content of blocks) {
        await client.notify(acp.methods.client.session.update, {
          sessionId: params.sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            messageId: "multimodal-content",
            content,
          },
        });
      }
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-config-update-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "config_option_update",
          configOptions: [
            { type: "boolean", id: "duplicate", name: "One", currentValue: false },
            { type: "boolean", id: "duplicate", name: "Two", currentValue: true },
          ],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "config_option_update",
          configOptions: [
            { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
          ],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "config-update-recovery",
          content: { type: "text", text: "Connection survived invalid config update." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-terminal-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "terminal-recovery",
          title: "Invalid terminal reference",
          content: [{ type: "terminal", terminalId: "never-created" }],
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "terminal-recovery",
          title: "Recovered tool output",
          status: "completed",
          content: [{
            type: "content",
            content: { type: "text", text: "Connection survived invalid terminal reference." },
          }],
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("terminal-cancel-flow")) {
      let invalidCreateRejected = false;
      try {
        await client.request(acp.methods.client.terminal.create, {
          sessionId: params.sessionId,
          command: "x".repeat(16_385),
        });
      } catch {
        invalidCreateRejected = true;
      }
      const terminal = await client.request(acp.methods.client.terminal.create, {
        sessionId: params.sessionId,
        command: process.execPath,
        args: ["-e", "setInterval(() => {}, 1000)"],
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "terminal-cancel-tool",
          title: "Cancel terminal waiter without killing process",
          kind: "execute",
          status: "in_progress",
          content: [{ type: "terminal", terminalId: terminal.terminalId }],
        },
      });
      const controller = new AbortController();
      const waiting = client.request(acp.methods.client.terminal.waitForExit, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      }, { cancellationSignal: controller.signal });
      await new Promise((resolve) => setTimeout(resolve, 25));
      controller.abort();
      let waitCancelled = false;
      try {
        await waiting;
      } catch (error) {
        waitCancelled = error instanceof acp.RequestError && error.code === -32_800;
      }
      const beforeKill = await client.request(acp.methods.client.terminal.output, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      await client.request(acp.methods.client.terminal.kill, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      const exitStatus = await client.request(acp.methods.client.terminal.waitForExit, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call_update",
          toolCallId: "terminal-cancel-tool",
          status: "completed",
          rawOutput: {
            invalidCreateRejected,
            waitCancelled,
            processSurvivedCancellation: beforeKill.exitStatus == null,
            exitStatus,
          },
        },
      });
      await client.request(acp.methods.client.terminal.release, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("terminal-burst-flow")) {
      const terminal = await client.request(acp.methods.client.terminal.create, {
        sessionId: params.sessionId,
        command: process.execPath,
        args: [
          "-e",
          "let count=0;const timer=setInterval(()=>{process.stdout.write('x');if(++count===60)clearInterval(timer)},2)",
        ],
        outputByteLimit: 16,
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "terminal-burst-tool",
          title: "Run terminal burst fixture",
          kind: "execute",
          status: "in_progress",
          content: [{ type: "terminal", terminalId: terminal.terminalId }],
        },
      });
      await client.request(acp.methods.client.terminal.waitForExit, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call_update",
          toolCallId: "terminal-burst-tool",
          status: "completed",
        },
      });
      await client.request(acp.methods.client.terminal.release, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("terminal-flow")) {
      const terminal = await client.request(acp.methods.client.terminal.create, {
        sessionId: params.sessionId,
        command: process.execPath,
        args: ["-e", "process.stdout.write('TERMINAL_FLOW_OUTPUT')"],
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "terminal-tool",
          title: "Run terminal fixture",
          kind: "execute",
          status: "in_progress",
          content: [{ type: "terminal", terminalId: terminal.terminalId }],
          locations: [{ path: process.cwd(), line: 0 }],
        },
      });
      const exitStatus = await client.request(acp.methods.client.terminal.waitForExit, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      const output = await client.request(acp.methods.client.terminal.output, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call_update",
          toolCallId: "terminal-tool",
          status: "completed",
          rawOutput: { ...output, exitStatus },
        },
      });
      await client.request(acp.methods.client.terminal.release, {
        sessionId: params.sessionId,
        terminalId: terminal.terminalId,
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-compaction-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "compaction_summary_chunk",
          compactionId: "not-started",
          content: { type: "text", text: "must be dropped" },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "after-invalid-update",
          content: { type: "text", text: "Connection survived." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText === "permission-flow") {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "permission-tool",
          title: "Inspect permission fixture",
          kind: "read",
          status: "pending",
        },
      });
      const permission = await client.request(
        acp.methods.client.session.requestPermission,
        {
          sessionId: params.sessionId,
          toolCall: { toolCallId: "permission-tool", title: "Inspect permission fixture" },
          options: [
            { optionId: "yes", name: "Allow once", kind: "allow_once" },
            { optionId: "no", name: "Reject", kind: "reject_once" },
          ],
        },
      );
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call_update",
          toolCallId: "permission-tool",
          status: permission.outcome.outcome === "selected" ? "completed" : "failed",
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "permission-result",
          content: { type: "text", text: `Permission ${permission.outcome.outcome}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-permission-flow")) {
      let rejected = false;
      try {
        await client.request(acp.methods.client.session.requestPermission, {
          sessionId: params.sessionId,
          toolCall: { toolCallId: "never-created", title: "Invented tool" },
          options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }],
        });
      } catch {
        rejected = true;
      }
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "invalid-permission-result",
          content: { type: "text", text: `Invalid permission rejected: ${rejected}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("cancel-pending-interactions-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "cancelled-tool",
          title: "Await user interactions",
          status: "in_progress",
        },
      });
      const permission = client.request(acp.methods.client.session.requestPermission, {
        sessionId: params.sessionId,
        toolCall: { toolCallId: "cancelled-tool", title: "Await permission" },
        options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }],
      });
      const elicitation = client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        toolCallId: "cancelled-tool",
        mode: "form",
        message: "Await input",
        requestedSchema: { type: "object", properties: {} },
      });
      const [permissionResponse, elicitationResponse] = await Promise.all([
        permission,
        elicitation,
      ]);
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "cancelled-interactions-result",
          content: {
            type: "text",
            text: `Cancelled interactions: ${permissionResponse.outcome.outcome}/${elicitationResponse.action}.`,
          },
        },
      });
      return { stopReason: "cancelled" };
    }
    if (promptText.includes("agent-cancel-interactions-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "tool_call",
          toolCallId: "agent-cancelled-tool",
          title: "Agent cancels interactions",
          status: "in_progress",
        },
      });
      const permissionController = new AbortController();
      const permission = client.request(acp.methods.client.session.requestPermission, {
        sessionId: params.sessionId,
        toolCall: { toolCallId: "agent-cancelled-tool", title: "Cancelled permission" },
        options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }],
      }, { cancellationSignal: permissionController.signal });
      await new Promise((resolve) => setTimeout(resolve, 20));
      permissionController.abort();
      try {
        await permission;
      } catch {
        // A peer may surface request cancellation as an RPC error or a cancelled outcome.
      }

      const elicitationController = new AbortController();
      const elicitation = client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        toolCallId: "agent-cancelled-tool",
        mode: "form",
        message: "Cancelled input",
        requestedSchema: { type: "object", properties: {} },
      }, { cancellationSignal: elicitationController.signal });
      await new Promise((resolve) => setTimeout(resolve, 20));
      elicitationController.abort();
      try {
        await elicitation;
      } catch {
        // See the permission cancellation note above.
      }
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "agent-cancelled-interactions-result",
          content: { type: "text", text: "Connection survived Agent cancellations." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-elicitation-tool-flow")) {
      let rejected = false;
      try {
        await client.request(acp.methods.client.elicitation.create, {
          sessionId: params.sessionId,
          toolCallId: "never-created",
          mode: "form",
          message: "Invented tool interaction",
          requestedSchema: { type: "object", properties: {} },
        });
      } catch {
        rejected = true;
      }
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "invalid-elicitation-tool-result",
          content: { type: "text", text: `Invalid elicitation rejected: ${rejected}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-message-role-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "cross-role-message",
          content: { type: "text", text: "Valid message." },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_thought_chunk",
          messageId: "cross-role-message",
          content: { type: "text", text: "Must be dropped." },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "invalid-message-role-result",
          content: { type: "text", text: "Connection survived invalid message reuse." },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("background-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "background-only",
          content: { type: "text", text: "BACKGROUND_ONLY_SENTINEL" },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("compaction-flow")) {
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "compaction_update",
          compactionId: "test-compaction",
          status: "in_progress",
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "compaction_summary_chunk",
          compactionId: "test-compaction",
          content: { type: "text", text: "Compact " },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "compaction_summary_chunk",
          compactionId: "test-compaction",
          content: { type: "text", text: "summary." },
        },
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "compaction_update",
          compactionId: "test-compaction",
          status: "completed",
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("pending-url-flow")) {
      await client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        mode: "url",
        message: "Connect the pending test account",
        elicitationId: "pending-external-flow",
        url: "https://example.test/pending",
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("url-flow") && !promptText.includes("duplicate-url-flow")) {
      const response = await client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        mode: "url",
        message: "Connect the test account",
        elicitationId: "test-external-flow",
        url: "https://example.test/connect",
      });
      if (response.action === "accept") {
        await client.notify(acp.methods.client.elicitation.complete, {
          elicitationId: "test-external-flow",
        });
      }
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("duplicate-url-flow")) {
      const first = client.request(acp.methods.client.elicitation.create, {
        sessionId: params.sessionId,
        mode: "url",
        message: "First external flow",
        elicitationId: "duplicate-external-flow",
        url: "https://example.test/first",
      });
      let duplicateRejected = false;
      try {
        await client.request(acp.methods.client.elicitation.create, {
          sessionId: params.sessionId,
          mode: "url",
          message: "Duplicate external flow",
          elicitationId: "duplicate-external-flow",
          url: "https://example.test/duplicate",
        });
      } catch {
        duplicateRejected = true;
      }
      await first;
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "duplicate-url-result",
          content: { type: "text", text: `Duplicate URL rejected: ${duplicateRejected}.` },
        },
      });
      return { stopReason: "end_turn" };
    }
    if (promptText.includes("invalid-complete-flow")) {
      await client.notify(acp.methods.client.elicitation.complete, {
        elicitationId: "never-accepted",
      });
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "invalid-complete-result",
          content: { type: "text", text: "Connection survived invalid completion." },
        },
      });
      return { stopReason: "end_turn" };
    }
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "available_commands_update",
        availableCommands: [
          { name: "inspect", description: "Inspect the workspace", input: { hint: "path" } },
        ],
      },
    });
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "plan",
        entries: [{ content: "Answer the test", priority: "high", status: "in_progress" }],
      },
    });
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call",
        toolCallId: "tool-1",
        title: "Inspect fixture",
        name: "read_file",
        kind: "read",
        status: "pending",
        locations: [{ path: "/workspace/fixture.ts", line: 0 }],
        rawInput: { path: "/workspace/fixture.ts", lineStart: 1, lineEnd: 40 },
      },
    });
    const permission = await client.request(
      acp.methods.client.session.requestPermission,
      {
        sessionId: params.sessionId,
        toolCall: { toolCallId: "tool-1", title: "Inspect fixture" },
        options: [
          { optionId: "yes", name: "Allow once", kind: "allow_once" },
          { optionId: "no", name: "Reject", kind: "reject_once" },
        ],
      },
    );
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: "tool-1",
        status: permission.outcome.outcome === "selected" ? "completed" : "failed",
      },
    });
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        messageId: "answer-1",
        content: { type: "text", text: "ACP " },
      },
    });
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        messageId: "answer-1",
        content: { type: "text", text: "works." },
      },
    });
    return { stopReason: "end_turn" };
  })
  .onNotification(acp.methods.agent.session.cancel, () => {
    if (disconnectCancelFile) writeFileSync(disconnectCancelFile, "cancel\n");
    finishDisconnectPrompt?.();
    finishDisconnectPrompt = undefined;
  });

const stream = acp.ndJsonStream(
  Writable.toWeb(process.stdout),
  Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>,
);
const connection = agent.connect(stream);
await connection.closed;

function requireAuthentication(): void {
  if (!authenticated) {
    throw acp.RequestError.authRequired({ hint: "Use agent-login" });
  }
}

async function beforeControlResponse(): Promise<void> {
  controlAttempts += 1;
  if (slowControl) await new Promise((resolve) => setTimeout(resolve, 150));
  if (failControlOnce && controlAttempts === 1) {
    throw new acp.RequestError(-32603, "Synthetic control failure");
  }
}

function parseMcpMessage(value: unknown): acp.MessageMcpRequest {
  if (typeof value !== "object" || value === null) throw new Error("Invalid mcp/message params");
  return value as acp.MessageMcpRequest;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function requestErrorDetails(error: unknown): { code?: number; message: string; data?: unknown } {
  return {
    ...(error instanceof acp.RequestError ? { code: error.code, data: error.data } : {}),
    message: errorText(error),
  };
}

async function waitForMcpPendingNotifications(
  connectionId: string,
  expected: number,
): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    const observed = observedMcpNotifications.filter((notification) =>
      notification.connectionId === connectionId &&
      notification.method === "notifications/progress" &&
      typeof notification.params === "object" &&
      notification.params !== null &&
      "progressToken" in notification.params &&
      notification.params.progressToken === "pending"
    ).length;
    if (observed >= expected) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for ${expected} pending MCP requests`);
}
