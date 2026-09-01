import assert from "node:assert/strict";
import { join } from "node:path";
import WebSocket from "ws";
import type { ServerEvent } from "../shared/bridge.js";
import { appReducer, initialState, type AppState } from "../src/lib/state.js";
import { startRustTestServer } from "./rust-test-server.js";

const cwd = process.cwd();
const server = await startSmokeServer(cwd);

try {
  const origin = `http://127.0.0.1:${server.port}`;
  const health = await fetch(`${origin}/api/health`);
  assert.equal(health.status, 200);
  assert.deepEqual(await health.json(), {
    ok: true,
    protocol: "acp/v1",
    backend: "rust",
  });

  const page = await fetch(origin);
  assert.equal(page.status, 200);
  assert.match(page.headers.get("content-type") ?? "", /^text\/html/);
  const html = await page.text();
  assert.match(html, /<div id="root"><\/div>/);
  const scriptPath = html.match(/<script[^>]+src="([^"]+)"/)?.[1];
  assert.ok(scriptPath, "production HTML must reference its JavaScript bundle");
  const bundle = await fetch(new URL(scriptPath, origin));
  assert.equal(bundle.status, 200);
  assert.match(bundle.headers.get("content-type") ?? "", /^text\/javascript/);

  await exerciseAgentHardening(cwd);
  if (process.env.ATTYD_SMOKE_SKIP_OVERSIZED_LINE !== "1") {
    await exerciseOversizedAgentLine(cwd);
  }
  await exerciseAgentSemanticHardening(cwd);
  await exerciseEarlySemanticRejection(cwd);
  await exerciseMcpConnectCancellation(cwd);
  const webSocketUrl = `ws://127.0.0.1:${server.port}/ws`;
  const hardeningEvents = await exerciseHardeningWebSocket(webSocketUrl);
  assert.ok(hardeningEvents.some((event) =>
    event.type === "bridge/pong" && event.nonce === "ui-smoke-liveness"
  ));
  assert.ok(hardeningEvents.some((event) =>
    event.type === "bridge/pong" && event.nonce === "ui-smoke-adversarial-recovery"
  ));
  assert.ok(hardeningEvents.some((event) =>
    event.type === "bridge/error" &&
    /unsupported\/command/u.test(`${event.message} ${JSON.stringify(event.data)}`)
  ));
  assert.ok(hardeningEvents.some((event) =>
    event.type === "bridge/error" &&
    event.requestId === "ui-smoke-unknown-session"
  ));
  assert.ok(hardeningEvents.some((event) =>
    event.type === "bridge/error" &&
    /cursor/u.test(`${event.message} ${JSON.stringify(event.data)}`)
  ));
  assert.ok(hardeningEvents.some((event) =>
    event.type === "acp/session_created" &&
    event.requestId === "ui-smoke-recovery"
  ));
  const attachment = await exerciseAttachmentWebSocket(webSocketUrl);
  assert.equal(attachment.state.session?.sessionId, "saved-session");
  assert.equal(attachment.state.cwd, cwd);
  assert.equal(attachment.state.modeId, "plan");
  assert.deepEqual(attachment.state.configOptions, [
    { type: "boolean", id: "verbose", name: "Verbose", currentValue: true },
  ]);
  assert.match(JSON.stringify(attachment.state.timeline), /Loaded history\./);
  assert.ok(attachment.events.some((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "current_mode_update"
  ));
  const resumeEvents = await exerciseResumeWebSocket(webSocketUrl);
  assert.ok(resumeEvents.some((event) =>
    event.type === "acp/session_attached" &&
    event.requestId === "ui-resume-session" &&
    event.method === "resume" &&
    event.response.modes?.currentModeId === "build"
  ));
  const { events, state, beforeClose } = await exerciseWebSocket(webSocketUrl);
  assert.ok(events.some((event) => event.type === "acp/initialized"));
  assert.ok(events.some((event) =>
    event.type === "acp/session_created" &&
    event.earlyUpdates?.some(({ update }) =>
      update.sessionUpdate === "available_commands_update"
    )
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/config_changed" &&
    event.requestId === "ui-smoke-config" &&
    event.response.configOptions.some((option) =>
      option.id === "verbose" && option.currentValue === true
    ),
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/elicitation_request" && event.request.mode === "form",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/elicitation_request" && event.request.mode === "url",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/elicitation_complete" &&
    event.notification.elicitationId === "test-external-flow",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/elicitation_aborted" &&
    event.elicitationId === "pending-external-flow" &&
    event.reason === "session_closed",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_message_chunk" &&
    event.notification.update.content.type === "text" &&
    event.notification.update.content.text === "Form accept.",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/mcp_connection" && event.action === "connected",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/mcp_connection" && event.action === "disconnected",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/mcp_message" &&
    event.direction === "server-to-agent" &&
    event.method === "roots/list",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/mcp_message" &&
    event.direction === "server-to-agent" &&
    event.method === "fail" &&
    event.kind === "response" &&
    event.error?.code === -32_042 &&
    event.error.message === "deliberate MCP failure",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/mcp_message" &&
    event.direction === "server-to-agent" &&
    event.method === "resultAndError" &&
    event.kind === "response" &&
    event.error?.code === -32_603,
  ));
  const mcpCancellationUpdate = events.find((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_message_chunk" &&
    event.notification.update.messageId === "mcp-cancel-result"
  );
  assert.ok(
    mcpCancellationUpdate?.type === "acp/session_update" &&
      mcpCancellationUpdate.notification.update.sessionUpdate === "agent_message_chunk" &&
      mcpCancellationUpdate.notification.update.content.type === "text",
  );
  assert.deepEqual(JSON.parse(mcpCancellationUpdate.notification.update.content.text), {
    messageCancelled: true,
    cancellationNotificationObserved: true,
    disconnectRejectedPending: true,
    recoveredEcho: { recovered: true },
  });
  const mcpLifecycleUpdate = events.find((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_message_chunk" &&
    event.notification.update.messageId === "mcp-lifecycle-result"
  );
  assert.ok(
    mcpLifecycleUpdate?.type === "acp/session_update" &&
      mcpLifecycleUpdate.notification.update.sessionUpdate === "agent_message_chunk" &&
      mcpLifecycleUpdate.notification.update.content.type === "text",
  );
  const mcpLifecycleResult = JSON.parse(
    mcpLifecycleUpdate.notification.update.content.text,
  ) as {
    pendingLimitRejected: boolean;
    pendingRejected: number;
    exitError: string;
    recoveredEcho: unknown;
  };
  assert.equal(mcpLifecycleResult.pendingLimitRejected, true);
  assert.equal(mcpLifecycleResult.pendingRejected, 128);
  assert.match(mcpLifecycleResult.exitError, /MCP server browser-fixture exited \(23\)/u);
  assert.deepEqual(mcpLifecycleResult.recoveredEcho, { recoveredAfterExit: true });
  const filesystemCancellationUpdate = events.find((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_message_chunk" &&
    event.notification.update.messageId === "filesystem-cancel-result"
  );
  assert.ok(
    filesystemCancellationUpdate?.type === "acp/session_update" &&
      filesystemCancellationUpdate.notification.update.sessionUpdate === "agent_message_chunk" &&
      filesystemCancellationUpdate.notification.update.content.type === "text",
  );
  assert.deepEqual(JSON.parse(filesystemCancellationUpdate.notification.update.content.text), {
    readCancelled: true,
    writeCancelled: true,
    originalPreserved: true,
  });
  assert.ok(events.some((event) =>
    event.type === "acp/session_forked" &&
    event.earlyUpdates?.some(({ update }) =>
      update.sessionUpdate === "available_commands_update"
    )
  ));
  assert.ok(events.some((event) =>
    event.type === "bridge/error" &&
    event.requestId === "ui-smoke-structured-error" &&
    event.code === -32_603 &&
    event.data != null,
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/session_closed" && event.requestId === "ui-smoke-close",
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/session_deleted" && event.requestId === "ui-smoke-delete",
  ));
  assert.ok(events.some((event) => event.type === "acp/prompt_complete"));
  assert.ok(events.some((event) =>
    event.type === "acp/prompt_complete" &&
    event.requestId === "ui-smoke-usage" &&
    event.response.stopReason === "max_tokens" &&
    event.response.usage?.totalTokens === 21,
  ));
  assert.equal(events.filter((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_message_chunk" &&
    event.notification.update.messageId === "multimodal-content"
  ).length, 5);
  assert.ok(events.some((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "agent_thought_chunk" &&
    event.notification.update.messageId === "activity-thought"
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "tool_call" &&
    event.notification.update.toolCallId === "review-workspace-files" &&
    event.notification.update.content?.filter(({ type }) => type === "diff").length === 2
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/session_update" &&
    event.notification.update.sessionUpdate === "tool_call" &&
    event.notification.update.toolCallId === "terminal-tool" &&
    event.notification.update.locations?.[0]?.line === 0 &&
    event.notification.update.content?.[0]?.type === "terminal"
  ));
  assert.ok(events.some((event) =>
    event.type === "acp/terminal_state" &&
    event.terminal.output === "TERMINAL_FLOW_OUTPUT" &&
    event.terminal.exitStatus?.exitCode === 0 &&
    event.terminal.released
  ));
  assert.doesNotMatch(JSON.stringify(events), /BROWSER_MCP_SECRET|server-side-only/);

  assert.equal(beforeClose.session?.sessionId, "forked-session");
  assert.deepEqual(beforeClose.availableCommands, [{
    name: "fork-status",
    description: "Inspect the forked session",
  }]);
  assert.equal(beforeClose.title, "Early ACP session");
  assert.ok(
    beforeClose.timeline.some((item) =>
      item.type === "assistant" && item.chunks.some(
        ({ messageId }) => messageId === "early-session-message",
      )
    ),
    `early session message missing; timeline=${JSON.stringify(beforeClose.timeline)}`,
  );
  assert.ok(beforeClose.timeline.some((item) =>
    item.type === "assistant" && item.chunks.some(
      ({ messageId }) => messageId === "early-fork-message",
    )
  ));
  assert.equal(beforeClose.sessionTransition, undefined);
  assert.equal(state.session, undefined);
  assert.equal(state.sessionTransition, undefined);
  assert.equal(state.pendingPrompt, undefined);
  assert.equal(state.pendingSessionControl, undefined);
  assert.equal(state.pendingSessionDeletions.length, 0);
  assert.ok(!state.sessions.some(({ sessionId }) => sessionId === "saved-session"));
  assert.equal(
    state.externalFlows.find(({ elicitationId }) => elicitationId === "test-external-flow")?.status,
    "completed",
  );
  assert.equal(
    state.externalFlows.find(({ elicitationId }) => elicitationId === "pending-external-flow")?.status,
    "cancelled",
  );
  assert.equal(state.mcpActivity.length, 100);
  assert.ok(state.mcpActivity.some(({ method }) => method === "exit"));
  assert.deepEqual(state.timeline, []);
  assert.doesNotMatch(JSON.stringify(beforeClose.timeline), /BACKGROUND_ONLY_SENTINEL/);
  assert.match(
    JSON.stringify(state.cachedSessions.get("test-session")?.timeline),
    /BACKGROUND_ONLY_SENTINEL/,
  );
  const compaction = beforeClose.timeline.find((item) => item.type === "compaction");
  assert.ok(compaction && compaction.type === "compaction");
  assert.equal(compaction.status, "completed");
  assert.deepEqual(compaction.blocks, [{ type: "text", text: "Compact summary." }]);
  const multimodal = beforeClose.timeline
    .filter((item) => item.type === "assistant")
    .flatMap(({ chunks }) => chunks)
    .find(({ messageId }) => messageId === "multimodal-content");
  assert.ok(multimodal);
  assert.deepEqual(multimodal.blocks.map(({ type }) => type), [
    "image",
    "audio",
    "resource_link",
    "resource",
    "resource",
  ]);
  assert.equal(
    multimodal.blocks.find((block) => block.type === "resource_link")?.uri,
    "file:///workspace/fixture.ts",
  );
  assert.ok(multimodal.blocks.some((block) =>
    block.type === "resource" && "text" in block.resource &&
    block.resource.text === "Embedded fixture text."
  ));
  const terminalTool = beforeClose.timeline.find((item) =>
    item.type === "tool" && item.call.toolCallId === "terminal-tool"
  );
  assert.ok(terminalTool && terminalTool.type === "tool");
  assert.equal(terminalTool.call.status, "completed");
  assert.equal(terminalTool.call.locations?.[0]?.line, 0);
  assert.equal(terminalTool.call.content?.[0]?.type, "terminal");
  assert.match(JSON.stringify(terminalTool.call.rawOutput), /TERMINAL_FLOW_OUTPUT/);
  const terminalSnapshot = beforeClose.terminalSnapshots.find(
    ({ terminalId }) =>
      terminalId === (
        terminalTool.call.content?.[0]?.type === "terminal"
          ? terminalTool.call.content[0].terminalId
          : undefined
      ),
  );
  assert.ok(terminalSnapshot);
  assert.equal(terminalSnapshot.output, "TERMINAL_FLOW_OUTPUT");
  assert.equal(terminalSnapshot.exitStatus?.exitCode, 0);
  assert.equal(terminalSnapshot.released, true);
  const reviewTool = beforeClose.timeline.find((item) =>
    item.type === "tool" && item.call.toolCallId === "review-workspace-files"
  );
  assert.ok(reviewTool && reviewTool.type === "tool");
  assert.equal(reviewTool.call.status, "completed");
  assert.equal(reviewTool.call.content?.filter(({ type }) => type === "diff").length, 2);
  assert.doesNotMatch(JSON.stringify(state), /BROWSER_MCP_SECRET|server-side-only/);
  console.log(
    `UI smoke passed (${events.length + attachment.events.length + resumeEvents.length + hardeningEvents.length} bridge events, ${state.backgroundEvents.length} isolated background events)`,
  );
} finally {
  await server.close();
}

interface SmokeServer {
  port: number;
  close(): Promise<void>;
}

async function startSmokeServer(
  cwd: string,
  agentFlags: string[] = ["--early-new-updates", "--early-fork-updates"],
  agentFixture = "fake-agent.ts",
): Promise<SmokeServer> {
  const agentCommand = [
    process.execPath,
    "--import",
    "tsx",
    join(cwd, "tests/fixtures", agentFixture),
    ...agentFlags,
  ];
  const provider = {
    name: "browser-fixture",
    serverId: "browser-fixture",
    command: process.execPath,
    args: ["--import", "tsx", join(cwd, "tests/fixtures/fake-mcp-server.ts")],
    env: [{ name: "BROWSER_MCP_SECRET", value: "server-side-only" }],
  };

  return startRustTestServer({
    cwd,
    command: agentCommand,
    mcpConfig: {
      mcpServers: [{ type: "acp", ...provider }],
    },
  });
}

async function exerciseAgentHardening(
  cwd: string,
): Promise<void> {
  const cyclicServer = await startSmokeServer(cwd, ["--cyclic-list"]);
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${cyclicServer.port}/ws`);
    const events: ServerEvent[] = [];
    socket.on("message", (data) => {
      events.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
    });
    try {
      await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
      socket.send(JSON.stringify({ type: "session/list", requestId: "cycle-list-1" }));
      const first = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/sessions_listed" && event.requestId === "cycle-list-1"
      );
      assert.equal(first.type, "acp/sessions_listed");
      assert.equal(first.response.nextCursor, "cursor-a");
      socket.send(JSON.stringify({
        type: "session/list",
        requestId: "cycle-list-2",
        cursor: "cursor-a",
      }));
      const second = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/sessions_listed" && event.requestId === "cycle-list-2"
      );
      assert.equal(second.type, "acp/sessions_listed");
      assert.equal(second.response.nextCursor, "cursor-b");
      socket.send(JSON.stringify({
        type: "session/list",
        requestId: "cycle-list-3",
        cursor: "cursor-b",
      }));
      const cycleError = await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" && event.requestId === "cycle-list-3"
      );
      assert.equal(cycleError.type, "bridge/error");
      assert.match(
        `${cycleError.message} ${JSON.stringify(cycleError.data)}`,
        /reused session\/list cursor/u,
      );
      socket.send(JSON.stringify({ type: "session/list", requestId: "cycle-list-recovery" }));
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/sessions_listed" && event.requestId === "cycle-list-recovery"
      );
    } finally {
      socket.close();
    }
  } finally {
    await cyclicServer.close();
  }
}

async function exerciseOversizedAgentLine(
  cwd: string,
): Promise<void> {
  const oversizedServer = await startSmokeServer(cwd, ["--oversized-stdout-line"]);
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${oversizedServer.port}/ws`);
    const events: ServerEvent[] = [];
    socket.on("message", (data) => {
      events.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
    });
    try {
      const error = await waitForSmokeEvent(
        events,
        (event) =>
          event.type === "bridge/error" &&
          /Agent NDJSON line exceeds 8000000 bytes/u.test(
            `${event.message} ${JSON.stringify(event.data)}`,
          ),
        30_000,
      );
      assert.equal(error.type, "bridge/error");
      await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/phase" && event.phase === "error"
      );
      assert.equal(events.some((event) => event.type === "acp/initialized"), false);
    } finally {
      socket.close();
    }
  } finally {
    await oversizedServer.close();
  }
}

async function exerciseMcpConnectCancellation(
  cwd: string,
): Promise<void> {
  const cancellationServer = await startSmokeServer(
    cwd,
    [],
    "raw-connect-cancel-agent.ts",
  );
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${cancellationServer.port}/ws`);
    const events: ServerEvent[] = [];
    socket.on("message", (data) => {
      events.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
    });
    try {
      await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
      socket.send(JSON.stringify({
        type: "session/new",
        requestId: "raw-connect-cancel-new",
      }));
      const created = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_created" &&
        event.requestId === "raw-connect-cancel-new"
      );
      assert.equal(created.type, "acp/session_created");
      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId: "raw-connect-cancel-prompt",
        sessionId: created.response.sessionId,
        prompt: [{ type: "text", text: "test ordered MCP connect cancellation" }],
      }));
      const result = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.messageId === "raw-connect-cancel-result"
      );
      assert.ok(
        result.type === "acp/session_update" &&
          result.notification.update.sessionUpdate === "agent_message_chunk" &&
          result.notification.update.content.type === "text",
      );
      assert.deepEqual(JSON.parse(result.notification.update.content.text), {
        connectCancelled: true,
        recovered: true,
      });
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/prompt_complete" &&
        event.requestId === "raw-connect-cancel-prompt"
      );
      assert.deepEqual(
        events
          .filter((event) => event.type === "acp/mcp_connection")
          .map(({ action }) => action),
        ["connected", "disconnected"],
        "the cancelled, unannounced MCP connection must not leak lifecycle events",
      );
    } finally {
      socket.close();
    }
  } finally {
    await cancellationServer.close();
  }
}

async function exerciseAgentSemanticHardening(
  cwd: string,
): Promise<void> {
  const semanticServer = await startSmokeServer(cwd, []);
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${semanticServer.port}/ws`);
    const events: ServerEvent[] = [];
    socket.on("message", (data) => {
      events.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
    });
    try {
      await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
      socket.send(JSON.stringify({ type: "session/new", requestId: "semantic-new" }));
      const created = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_created" && event.requestId === "semantic-new"
      );
      assert.equal(created.type, "acp/session_created");
      const sessionId = created.response.sessionId;

      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId: "semantic-invalid-usage",
        sessionId,
        prompt: [{ type: "text", text: "invalid-usage-flow" }],
      }));
      const usageError = await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" && event.requestId === "semantic-invalid-usage"
      );
      assert.equal(usageError.type, "bridge/error");
      assert.match(
        `${usageError.message} ${JSON.stringify(usageError.data)}`,
        /exceeds totalTokens/u,
      );

      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId: "semantic-invalid-content",
        sessionId,
        prompt: [{ type: "text", text: "invalid-content-flow" }],
      }));
      await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" &&
        /image\/\* family/u.test(`${event.message} ${JSON.stringify(event.data)}`)
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.messageId === "content-recovery"
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/prompt_complete" &&
        event.requestId === "semantic-invalid-content"
      );
      assert.equal(events.some((event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.content.type === "image"
      ), false);

      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId: "semantic-invalid-config",
        sessionId,
        prompt: [{ type: "text", text: "invalid-config-update-flow" }],
      }));
      await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" &&
        /duplicate config option ID/u.test(`${event.message} ${JSON.stringify(event.data)}`)
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "config_option_update" &&
        event.notification.update.configOptions.length === 1 &&
        event.notification.update.configOptions[0]?.id === "verbose"
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.messageId === "config-update-recovery"
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/prompt_complete" &&
        event.requestId === "semantic-invalid-config"
      );

      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId: "semantic-invalid-compaction",
        sessionId,
        prompt: [{ type: "text", text: "invalid-compaction-flow" }],
      }));
      await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" &&
        /in-progress compaction/u.test(`${event.message} ${JSON.stringify(event.data)}`)
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.messageId === "after-invalid-update"
      );
      await waitForSmokeEvent(events, (event) =>
        event.type === "acp/prompt_complete" &&
        event.requestId === "semantic-invalid-compaction"
      );
    } finally {
      socket.close();
    }
  } finally {
    await semanticServer.close();
  }
}

async function exerciseEarlySemanticRejection(
  cwd: string,
): Promise<void> {
  const semanticServer = await startSmokeServer(
    cwd,
    ["--invalid-early-content-once"],
  );
  try {
    const socket = new WebSocket(`ws://127.0.0.1:${semanticServer.port}/ws`);
    const events: ServerEvent[] = [];
    socket.on("message", (data) => {
      events.push(JSON.parse(data.toString()) as ServerEvent);
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("open", resolve);
      socket.once("error", reject);
    });
    try {
      await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
      socket.send(JSON.stringify({ type: "session/new", requestId: "invalid-early-new" }));
      const rejected = await waitForSmokeEvent(events, (event) =>
        event.type === "bridge/error" && event.requestId === "invalid-early-new"
      );
      assert.equal(rejected.type, "bridge/error");
      assert.match(
        `${rejected.message} ${JSON.stringify(rejected.data)}`,
        /session replay was invalid/u,
      );
      assert.equal(events.some((event) =>
        event.type === "acp/session_created" && event.requestId === "invalid-early-new"
      ), false);

      socket.send(JSON.stringify({ type: "session/new", requestId: "early-recovery-new" }));
      const recovered = await waitForSmokeEvent(events, (event) =>
        event.type === "acp/session_created" && event.requestId === "early-recovery-new"
      );
      assert.equal(recovered.type, "acp/session_created");
      assert.deepEqual(recovered.response._meta?.observedSessionCloses, ["test-session"]);
    } finally {
      socket.close();
    }
  } finally {
    await semanticServer.close();
  }
}

async function exerciseHardeningWebSocket(url: string): Promise<ServerEvent[]> {
  const socket = new WebSocket(url);
  const events: ServerEvent[] = [];
  socket.on("message", (data) => {
    events.push(JSON.parse(data.toString()) as ServerEvent);
  });
  await new Promise<void>((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
  });

  try {
    await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
    socket.send(JSON.stringify({
      type: "auth/authenticate",
      requestId: "ui-smoke-authenticate",
      methodId: "agent-login",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "acp/authenticated" && event.requestId === "ui-smoke-authenticate"
    );
    socket.send(JSON.stringify({ type: "auth/logout", requestId: "ui-smoke-logout" }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "acp/logged_out" && event.requestId === "ui-smoke-logout"
    );
    socket.send(JSON.stringify({
      type: "session/new",
      requestId: "ui-smoke-unauthenticated-new",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/error" && event.requestId === "ui-smoke-unauthenticated-new"
    );
    socket.send(JSON.stringify({
      type: "auth/authenticate",
      requestId: "ui-smoke-reauthenticate",
      methodId: "agent-login",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "acp/authenticated" && event.requestId === "ui-smoke-reauthenticate"
    );
    socket.send(JSON.stringify({ type: "bridge/ping", nonce: "ui-smoke-liveness" }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/pong" && event.nonce === "ui-smoke-liveness"
    );

    const errorsBeforeMalformed = events.filter((event) => event.type === "bridge/error").length;
    socket.send("{");
    await waitForSmokeEvent(events, (_event, current) =>
      current.filter((event) => event.type === "bridge/error").length > errorsBeforeMalformed
    );

    const errorsBeforeArray = events.filter((event) => event.type === "bridge/error").length;
    socket.send(JSON.stringify(["not", "an", "object"]));
    await waitForSmokeEvent(events, (_event, current) =>
      current.filter((event) => event.type === "bridge/error").length > errorsBeforeArray
    );

    const errorsBeforeBinary = events.filter((event) => event.type === "bridge/error").length;
    socket.send(Buffer.from([0xff, 0xfe, 0xfd]));
    await waitForSmokeEvent(events, (_event, current) =>
      current.filter((event) => event.type === "bridge/error").length > errorsBeforeBinary
    );

    const adversarialFrames = rawWebSocketAdversarialCorpus();
    const errorsBeforeCorpus = events.filter((event) => event.type === "bridge/error").length;
    for (const frame of adversarialFrames) socket.send(frame);
    await waitForSmokeEvent(
      events,
      (_event, current) =>
        current.filter((event) => event.type === "bridge/error").length >=
          errorsBeforeCorpus + adversarialFrames.length,
      30_000,
    );
    socket.send(JSON.stringify({
      type: "bridge/ping",
      nonce: "ui-smoke-adversarial-recovery",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/pong" && event.nonce === "ui-smoke-adversarial-recovery"
    );

    socket.send(JSON.stringify({
      type: "unsupported/command",
      requestId: "ui-smoke-unknown-command",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/error" &&
      /unsupported\/command/u.test(`${event.message} ${JSON.stringify(event.data)}`)
    );

    socket.send(JSON.stringify({
      type: "session/prompt",
      requestId: "ui-smoke-unknown-session",
      sessionId: "missing-session",
      prompt: [{ type: "text", text: "must not reach the Agent" }],
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/error" && event.requestId === "ui-smoke-unknown-session"
    );

    socket.send(JSON.stringify({
      type: "session/list",
      requestId: "ui-smoke-invalid-cursor",
      cursor: 42,
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "bridge/error" &&
      /cursor/u.test(`${event.message} ${JSON.stringify(event.data)}`)
    );

    socket.send(JSON.stringify({
      type: "session/new",
      requestId: "ui-smoke-recovery",
    }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "acp/session_created" && event.requestId === "ui-smoke-recovery"
    );
    return events;
  } finally {
    socket.close();
  }
}

function rawWebSocketAdversarialCorpus(): string[] {
  const frames = [
    "",
    "null",
    "true",
    "0",
    '"string"',
    "[]",
    "{}",
    "{",
  ];
  for (let index = 0; index < 320; index += 1) {
    switch (index % 10) {
      case 0:
        frames.push(`{"type":`);
        break;
      case 1:
        frames.push(JSON.stringify([index, { type: "session/list" }]));
        break;
      case 2:
        frames.push(JSON.stringify({
          type: `unsupported/adversarial-${index}`,
          requestId: `adversarial-${index}`,
        }));
        break;
      case 3:
        frames.push(JSON.stringify({ type: "bridge/ping", nonce: index }));
        break;
      case 4:
        frames.push(JSON.stringify({
          type: "session/list",
          requestId: { invalid: index },
          cursor: null,
        }));
        break;
      case 5:
        frames.push(JSON.stringify({
          type: "context/search",
          requestId: `adversarial-${index}`,
          query: ["invalid"],
        }));
        break;
      case 6:
        frames.push(JSON.stringify({
          type: "session/prompt",
          requestId: `adversarial-${index}`,
          sessionId: null,
          prompt: "not-blocks",
        }));
        break;
      case 7:
        frames.push(JSON.stringify({
          type: "session/set_config_option",
          requestId: `adversarial-${index}`,
          sessionId: "missing-session",
          configId: [],
          value: { invalid: true },
        }));
        break;
      case 8:
        frames.push(JSON.stringify({
          type: "permission/respond",
          requestId: `adversarial-${index}`,
          permissionId: `missing-${index}`,
          outcome: { outcome: "selected", optionId: index },
        }));
        break;
      default:
        frames.push(JSON.stringify({
          type: "auth/terminal_resize",
          requestId: `adversarial-${index}`,
          cols: -1,
          rows: 65_536,
        }));
        break;
    }
  }
  return frames;
}

async function waitForSmokeEvent(
  events: ServerEvent[],
  predicate: (event: ServerEvent, events: ServerEvent[]) => boolean,
  timeoutMs = 10_000,
): Promise<ServerEvent> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const event = events.find((event) => predicate(event, events));
    if (event) return event;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`Timed out waiting for hardening event: ${JSON.stringify(events)}`);
}

function exerciseAttachmentWebSocket(url: string): Promise<{
  events: ServerEvent[];
  state: AppState;
}> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    const events: ServerEvent[] = [];
    let state = initialState;
    const timeout = setTimeout(() => {
      socket.terminate();
      reject(new Error(`Timed out waiting for attachment flow: ${JSON.stringify(events)}`));
    }, 10_000);

    socket.on("message", (data) => {
      const event = JSON.parse(data.toString()) as ServerEvent;
      events.push(event);
      state = appReducer(state, { type: "server/event", event });
      if (event.type === "bridge/error") {
        clearTimeout(timeout);
        socket.terminate();
        reject(new Error(event.message));
        return;
      }
      if (event.type === "acp/initialized") {
        socket.send(JSON.stringify({ type: "session/list", requestId: "ui-attach-list" }));
        return;
      }
      if (event.type === "acp/sessions_listed" && event.requestId === "ui-attach-list") {
        state = appReducer(state, {
          type: "session/transition_start",
          kind: "attach",
          requestId: "ui-attach-load",
          sessionId: "saved-session",
          title: "Saved ACP session",
        });
        socket.send(JSON.stringify({
          type: "session/load",
          requestId: "ui-attach-load",
          sessionId: "saved-session",
        }));
        return;
      }
      if (event.type === "acp/session_attached" && event.requestId === "ui-attach-load") {
        clearTimeout(timeout);
        socket.close();
        resolve({ events, state });
      }
    });
    socket.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
  });
}

async function exerciseResumeWebSocket(url: string): Promise<ServerEvent[]> {
  const socket = new WebSocket(url);
  const events: ServerEvent[] = [];
  socket.on("message", (data) => {
    events.push(JSON.parse(data.toString()) as ServerEvent);
  });
  await new Promise<void>((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
  });

  try {
    await waitForSmokeEvent(events, (event) => event.type === "acp/initialized");
    socket.send(JSON.stringify({ type: "session/list", requestId: "ui-resume-list" }));
    await waitForSmokeEvent(events, (event) =>
      event.type === "acp/sessions_listed" && event.requestId === "ui-resume-list"
    );
    socket.send(JSON.stringify({
      type: "session/resume",
      requestId: "ui-resume-session",
      sessionId: "saved-session",
    }));
    const attached = await waitForSmokeEvent(events, (event) =>
      event.type === "acp/session_attached" && event.requestId === "ui-resume-session"
    );
    assert.equal(attached.type, "acp/session_attached");
    assert.equal(attached.method, "resume");
    assert.equal(attached.response.modes?.currentModeId, "build");
    return events;
  } finally {
    socket.close();
  }
}

function exerciseWebSocket(url: string): Promise<{
  events: ServerEvent[];
  state: AppState;
  beforeClose: AppState;
}> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    const events: ServerEvent[] = [];
    let state = initialState;
    let beforeClose: AppState | undefined;
    let sourceSessionId: string | undefined;
    let forkedSessionId: string | undefined;
    const timeout = setTimeout(() => {
      socket.terminate();
      reject(new Error(`Timed out waiting for UI WebSocket flow: ${JSON.stringify(events)}`));
    }, 30_000);
    const sendVisiblePrompt = (requestId: string, sessionId: string, text: string) => {
      const blocks = [{ type: "text" as const, text }];
      state = appReducer(state, {
        type: "user/prompt",
        requestId,
        sessionId,
        blocks,
      });
      assert.deepEqual(state.pendingPrompt, { requestId, sessionId, blocks });
      socket.send(JSON.stringify({
        type: "session/prompt",
        requestId,
        sessionId,
        prompt: blocks,
      }));
    };

    socket.on("message", (data) => {
      const event = JSON.parse(data.toString()) as ServerEvent;
      events.push(event);
      state = appReducer(state, { type: "server/event", event });
      if (
        event.type === "bridge/error" &&
        event.requestId !== "ui-smoke-structured-error"
      ) {
        clearTimeout(timeout);
        socket.terminate();
        reject(new Error(event.message));
        return;
      }
      if (event.type === "acp/initialized") {
        state = appReducer(state, {
          type: "session/transition_start",
          kind: "new",
          requestId: "ui-smoke-new",
        });
        assert.equal(state.sessionTransition?.requestId, "ui-smoke-new");
        socket.send(JSON.stringify({
          type: "session/new",
          requestId: "ui-smoke-new",
        }));
        return;
      }
      if (event.type === "acp/session_created") {
        sourceSessionId = event.response.sessionId;
        state = appReducer(state, {
          type: "session/control_start",
          kind: "mode",
          requestId: "ui-smoke-mode",
          sessionId: sourceSessionId,
        });
        socket.send(JSON.stringify({
          type: "session/set_mode",
          requestId: "ui-smoke-mode",
          sessionId: sourceSessionId,
          modeId: "plan",
        }));
        return;
      }
      if (event.type === "acp/mode_changed" && event.requestId === "ui-smoke-mode") {
        assert.equal(event.modeId, "plan");
        assert.equal(state.pendingSessionControl, undefined);
        assert.ok(sourceSessionId);
        state = appReducer(state, {
          type: "session/control_start",
          kind: "config",
          requestId: "ui-smoke-config",
          sessionId: sourceSessionId,
        });
        assert.deepEqual(state.pendingSessionControl, {
          kind: "config",
          requestId: "ui-smoke-config",
          sessionId: sourceSessionId,
        });
        socket.send(JSON.stringify({
          type: "session/set_config_option",
          requestId: "ui-smoke-config",
          sessionId: sourceSessionId,
          configId: "verbose",
          value: true,
        }));
        return;
      }
      if (event.type === "acp/config_changed" && event.requestId === "ui-smoke-config") {
        assert.equal(state.pendingSessionControl, undefined);
        assert.ok(event.response.configOptions.some((option) =>
          option.id === "verbose" && option.currentValue === true
        ));
        assert.ok(sourceSessionId);
        sendVisiblePrompt("ui-smoke-form", sourceSessionId, "form-flow");
        return;
      }
      if (event.type === "acp/elicitation_request") {
        if (event.request.mode === "form") {
          state = appReducer(state, {
            type: "elicitation/respond_start",
            elicitationId: event.elicitationId,
            requestId: "ui-smoke-form-response",
          });
          socket.send(JSON.stringify({
            type: "elicitation/respond",
            requestId: "ui-smoke-form-response",
            elicitationId: event.elicitationId,
            response: {
              action: "accept",
              content: {
                name: "Browser fixture",
                count: 2,
                channel: "stable",
                tags: ["fast"],
                startsAt: "2026-08-30T12:30:00Z",
                confirmed: true,
              },
            },
          }));
          return;
        }
        if (event.request.mode === "url") {
          state = appReducer(state, {
            type: "elicitation/respond_start",
            elicitationId: event.elicitationId,
            requestId: "ui-smoke-url-response",
          });
          socket.send(JSON.stringify({
            type: "elicitation/respond",
            requestId: "ui-smoke-url-response",
            elicitationId: event.elicitationId,
            response: { action: "accept" },
          }));
          return;
        }
      }
      if (event.type === "acp/permission_request") {
        socket.send(JSON.stringify({
          type: "permission/respond",
          requestId: "ui-smoke-permission-response",
          permissionId: event.permissionId,
          outcome: { outcome: "selected", optionId: "yes" },
        }));
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-form") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(sourceSessionId);
        sendVisiblePrompt("ui-smoke-url", sourceSessionId, "url-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-url") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(sourceSessionId);
        const visibleTimeline = state.timeline;
        state = appReducer(state, {
          type: "session/transition_start",
          kind: "fork",
          requestId: "ui-smoke-fork",
          sessionId: sourceSessionId,
        });
        assert.equal(state.session?.sessionId, sourceSessionId);
        assert.equal(state.sessionTransition?.kind, "fork");
        assert.equal(state.timeline, visibleTimeline);
        socket.send(JSON.stringify({
          type: "session/fork",
          requestId: "ui-smoke-fork",
          sessionId: sourceSessionId,
        }));
        return;
      }
      if (event.type === "acp/session_forked" && event.requestId === "ui-smoke-fork") {
        assert.equal(event.sourceSessionId, sourceSessionId);
        forkedSessionId = event.response.sessionId;
        socket.send(JSON.stringify({
          type: "session/prompt",
          requestId: "ui-smoke-background",
          sessionId: sourceSessionId,
          prompt: [{ type: "text", text: "background-flow" }],
        }));
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-background") {
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-compaction", forkedSessionId, "compaction-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-compaction") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-content", forkedSessionId, "content-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-content") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-activity", forkedSessionId, "activity-flow");
        return;
      }
      if (
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_thought_chunk" &&
        event.notification.update.messageId === "activity-thought"
      ) {
        assert.equal(state.agentActivity?.kind, "thinking");
      }
      if (
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "tool_call" &&
        event.notification.update.toolCallId === "activity-tool"
      ) {
        assert.deepEqual(state.agentActivity, {
          kind: "tool",
          toolCallId: "activity-tool",
          title: "Inspect workspace dependencies and generated configuration files",
        });
      }
      if (
        event.type === "acp/session_update" &&
        event.notification.update.sessionUpdate === "agent_message_chunk" &&
        event.notification.update.messageId === "activity-answer"
      ) {
        assert.equal(state.agentActivity?.kind, "responding");
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-activity") {
        assert.equal(state.pendingPrompt, undefined);
        assert.equal(state.agentActivity, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-review", forkedSessionId, "review-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-review") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        const reviewTool = state.timeline.find((item) =>
          item.type === "tool" && item.call.toolCallId === "review-workspace-files"
        );
        assert.ok(reviewTool?.type === "tool");
        assert.equal(reviewTool.call.content?.filter(({ type }) => type === "diff").length, 2);
        sendVisiblePrompt("ui-smoke-terminal", forkedSessionId, "terminal-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-terminal") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt(
          "ui-smoke-terminal-cancel",
          forkedSessionId,
          "terminal-cancel-flow",
        );
        return;
      }
      if (
        event.type === "acp/prompt_complete" &&
        event.requestId === "ui-smoke-terminal-cancel"
      ) {
        assert.equal(state.pendingPrompt, undefined);
        const tool = state.timeline.find((item) =>
          item.type === "tool" && item.call.toolCallId === "terminal-cancel-tool"
        );
        assert.ok(tool?.type === "tool");
        assert.equal(tool.call.status, "completed");
        assert.ok(typeof tool.call.rawOutput === "object" && tool.call.rawOutput !== null);
        const terminalResult = tool.call.rawOutput as {
          invalidCreateRejected?: boolean;
          waitCancelled?: boolean;
          processSurvivedCancellation?: boolean;
          exitStatus?: { exitCode?: number | null; signal?: string | null };
        };
        assert.equal(terminalResult.invalidCreateRejected, true);
        assert.equal(terminalResult.waitCancelled, true);
        assert.equal(terminalResult.processSurvivedCancellation, true);
        assert.ok(terminalResult.exitStatus?.exitCode == null);
        assert.match(terminalResult.exitStatus?.signal ?? "", /^SIG(?:TERM|9)$/u);
        assert.ok(state.terminalSnapshots.some(({ released }) => released));
        assert.ok(forkedSessionId);
        sendVisiblePrompt(
          "ui-smoke-structured-error",
          forkedSessionId,
          "structured-error-flow",
        );
        return;
      }
      if (
        event.type === "bridge/error" &&
        event.requestId === "ui-smoke-structured-error"
      ) {
        assert.equal(event.code, -32_603);
        assert.deepEqual(event.data, {
          retryHint: "Retry the same ACP ContentBlocks",
          retryAfterMs: 25,
          nested: { owner: "Agent" },
        });
        const error = state.timeline.at(-1);
        assert.ok(error?.type === "error");
        assert.deepEqual(error.retryBlocks, [{ type: "text", text: "structured-error-flow" }]);
        assert.ok(forkedSessionId);
        sendVisiblePrompt(
          "ui-smoke-structured-error-retry",
          forkedSessionId,
          "structured-error-flow",
        );
        return;
      }
      if (
        event.type === "acp/prompt_complete" &&
        event.requestId === "ui-smoke-structured-error-retry"
      ) {
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-mcp", forkedSessionId, "mcp-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-mcp") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-mcp-cancel", forkedSessionId, "mcp-cancel-flow");
        return;
      }
      if (
        event.type === "acp/prompt_complete" &&
        event.requestId === "ui-smoke-mcp-cancel"
      ) {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt(
          "ui-smoke-mcp-lifecycle",
          forkedSessionId,
          "mcp-lifecycle-flow",
        );
        return;
      }
      if (
        event.type === "acp/prompt_complete" &&
        event.requestId === "ui-smoke-mcp-lifecycle"
      ) {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt(
          "ui-smoke-filesystem-cancel",
          forkedSessionId,
          "filesystem-cancel-flow",
        );
        return;
      }
      if (
        event.type === "acp/prompt_complete" &&
        event.requestId === "ui-smoke-filesystem-cancel"
      ) {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-usage", forkedSessionId, "usage-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-usage") {
        assert.equal(state.pendingPrompt, undefined);
        assert.equal(event.response.usage?.totalTokens, 21);
        assert.ok(forkedSessionId);
        sendVisiblePrompt("ui-smoke-permission", forkedSessionId, "permission-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-permission") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        assert.ok(state.timeline.some((item) =>
          item.type === "tool" &&
          item.call.toolCallId === "permission-tool" &&
          item.call.status === "completed"
        ));
        sendVisiblePrompt("ui-smoke-pending-url", forkedSessionId, "pending-url-flow");
        return;
      }
      if (event.type === "acp/prompt_complete" && event.requestId === "ui-smoke-pending-url") {
        assert.equal(state.pendingPrompt, undefined);
        assert.ok(forkedSessionId);
        assert.equal(
          state.externalFlows.find(
            ({ elicitationId }) => elicitationId === "pending-external-flow",
          )?.status,
          "waiting",
        );
        beforeClose = state;
        const visibleTimeline = state.timeline;
        state = appReducer(state, {
          type: "session/transition_start",
          kind: "close",
          requestId: "ui-smoke-close",
          sessionId: forkedSessionId,
        });
        assert.equal(state.session?.sessionId, forkedSessionId);
        assert.equal(state.sessionTransition?.kind, "close");
        assert.equal(state.timeline, visibleTimeline);
        socket.send(JSON.stringify({
          type: "session/close",
          requestId: "ui-smoke-close",
          sessionId: forkedSessionId,
        }));
        return;
      }
      if (event.type === "acp/session_closed" && event.requestId === "ui-smoke-close") {
        assert.ok(beforeClose);
        socket.send(JSON.stringify({
          type: "session/list",
          requestId: "ui-smoke-delete-list",
        }));
        return;
      }
      if (event.type === "acp/sessions_listed" && event.requestId === "ui-smoke-delete-list") {
        assert.ok(event.response.sessions.some(({ sessionId }) => sessionId === "saved-session"));
        state = appReducer(state, {
          type: "session/delete_start",
          requestId: "ui-smoke-delete",
          sessionId: "saved-session",
        });
        assert.deepEqual(state.pendingSessionDeletions, [{
          requestId: "ui-smoke-delete",
          sessionId: "saved-session",
        }]);
        socket.send(JSON.stringify({
          type: "session/delete",
          requestId: "ui-smoke-delete",
          sessionId: "saved-session",
        }));
        return;
      }
      if (event.type === "acp/session_deleted" && event.requestId === "ui-smoke-delete") {
        assert.ok(beforeClose);
        clearTimeout(timeout);
        socket.close();
        resolve({ events, state, beforeClose });
      }
    });
    socket.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
  });
}
