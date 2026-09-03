import { spawn } from "node:child_process";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";
import { startRustTestServer } from "./rust-test-server.js";

const LOCAL_PROVIDER_KEY = "attyd-local-provider-fixture";
const LOCAL_TOOL_OUTPUT = "ATTYD_GOOSE_TOOL_OK";
const LOCAL_AGENT_OUTPUT = "LOCAL_GOOSE_ACP_OK";

interface RuntimeView {
  connected: boolean;
  initialized?: { response?: { agentInfo?: { name?: string; version?: string } } } | null;
  phase?: { phase?: string } | null;
}

interface SessionView {
  historyRevision: string | null;
  phase: string;
  syncError: string | null;
  timeline: unknown[];
  activeTurn: unknown | null;
  interactions: {
    permissions: Record<string, {
      interactionId: string;
      request: { options?: Array<{ optionId: string; kind: string }> };
    }>;
  };
}

interface CreatedSession {
  sessionId: string;
  view: SessionView;
}

const stateRoot = await mkdtemp(join(tmpdir(), "attyd-goose-smoke-"));
await Promise.all([
  mkdir(join(stateRoot, "config")),
  mkdir(join(stateRoot, "data")),
  mkdir(join(stateRoot, "state")),
]);

const gooseEnv = { ...process.env };
for (const key of [
  "GOOSE_PROVIDER",
  "GOOSE_MODEL",
  "OPENAI_API_KEY",
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_AUTH_TOKEN",
  "OPENROUTER_API_KEY",
  "GOOGLE_API_KEY",
  "GEMINI_API_KEY",
  "DATABRICKS_TOKEN",
  "XAI_API_KEY",
  "HF_TOKEN",
  "HUGGINGFACE_API_KEY",
]) {
  delete gooseEnv[key];
}
const isolatedEnv = {
  ...gooseEnv,
  XDG_CONFIG_HOME: join(stateRoot, "config"),
  XDG_DATA_HOME: join(stateRoot, "data"),
  XDG_STATE_HOME: join(stateRoot, "state"),
};

const goose = spawn(resolve("bin/goose"), ["acp"], {
  cwd: process.cwd(),
  env: isolatedEnv,
  stdio: ["pipe", "pipe", "pipe"],
});

let stderr = "";
goose.stderr.setEncoding("utf8").on("data", (chunk: string) => { stderr += chunk; });

try {
  const stream = acp.ndJsonStream(
    Writable.toWeb(goose.stdin),
    Readable.toWeb(goose.stdout) as ReadableStream<Uint8Array>,
  );
  const connection = acp.client({ name: "attyd-goose-smoke" }).connect(stream);
  const response = await connection.agent.request(acp.methods.agent.initialize, {
    protocolVersion: acp.PROTOCOL_VERSION,
    clientCapabilities: {},
    clientInfo: { name: "attyd-goose-smoke", version: "0.1.0" },
  });
  if (response.protocolVersion !== acp.PROTOCOL_VERSION) {
    throw new Error(`Goose negotiated ACP v${response.protocolVersion}`);
  }
  if (response.agentInfo?.name !== "goose") {
    throw new Error(`Unexpected Goose agent identity: ${response.agentInfo?.name ?? "missing"}`);
  }
  if (response.agentCapabilities?.sessionCapabilities?.list == null) {
    throw new Error("Goose did not advertise session/list");
  }
  let sessionError: unknown;
  try {
    await connection.agent.request(acp.methods.agent.session.new, {
      cwd: process.cwd(),
      mcpServers: [],
    });
  } catch (error) {
    sessionError = error;
  }
  if (!(sessionError instanceof acp.RequestError)) {
    throw new Error(
      sessionError == null
        ? "Goose unexpectedly created a session without a configured provider"
        : `Goose session/new returned a non-protocol error: ${String(sessionError)}`,
    );
  }
  if (
    sessionError.code !== -32603 ||
    typeof sessionError.data !== "string" ||
    !sessionError.data.includes("GOOSE_PROVIDER")
  ) {
    throw new Error(
      `Unexpected Goose session/new error: ${JSON.stringify({
        code: sessionError.code,
        message: sessionError.message,
        data: sessionError.data,
      })}`,
    );
  }
  const sessions = await connection.agent.request(acp.methods.agent.session.list, {
    cwd: process.cwd(),
  });
  if (sessions.sessions.length !== 0 || sessions.nextCursor != null) {
    throw new Error(`Isolated Goose session/list was not empty: ${JSON.stringify(sessions)}`);
  }
  if (goose.exitCode != null) {
    throw new Error(`Goose exited during the ACP recovery probe (${goose.exitCode})`);
  }
  connection.close();
  if (goose.exitCode == null) goose.kill();

  await exerciseBridge(response.agentInfo.version, isolatedEnv);
  const provider = await startLocalOpenAiFixture();
  try {
    const configuredEnv = {
      ...isolatedEnv,
      GOOSE_PROVIDER: "openai",
      GOOSE_MODEL: "gpt-4o-mini",
      GOOSE_PROVIDER_SKIP_BACKOFF: "true",
      OPENAI_API_KEY: LOCAL_PROVIDER_KEY,
      OPENAI_HOST: provider.origin,
      OPENAI_BASE_PATH: "v1/chat/completions",
    };
    await exerciseConfiguredRaw(response.agentInfo.version, configuredEnv);
    await exerciseConfiguredBridge(response.agentInfo.version, configuredEnv);
    provider.assertComplete();
  } finally {
    await provider.close();
  }
  console.log(
    `goose ${response.agentInfo.version}: ACP v${response.protocolVersion} recovery plus local prompt/tool raw and bridge REST/SSE lifecycles passed`,
  );
} catch (error) {
  if (stderr) console.error(stderr);
  throw error;
} finally {
  if (goose.exitCode == null) goose.kill();
  await rm(stateRoot, { recursive: true, force: true });
}

interface LocalOpenAiFixture {
  origin: string;
  assertComplete(): void;
  close(): Promise<void>;
}

async function startLocalOpenAiFixture(): Promise<LocalOpenAiFixture> {
  let requestCount = 0;
  let toolCalls = 0;
  let toolResults = 0;
  const server = createServer(async (request, response) => {
    try {
      if (request.headers.authorization !== `Bearer ${LOCAL_PROVIDER_KEY}`) {
        sendJson(response, 401, { error: { message: "Invalid local fixture key" } });
        return;
      }
      if (request.method === "GET" && request.url === "/v1/models") {
        sendJson(response, 200, {
          object: "list",
          data: [{ id: "gpt-4o-mini", object: "model", created: 0, owned_by: "attyd" }],
        });
        return;
      }
      if (request.method !== "POST" || request.url !== "/v1/chat/completions") {
        sendJson(response, 404, { error: { message: "Unknown local fixture route" } });
        return;
      }
      requestCount += 1;
      const body = await readJsonBody(request);
      if (body.model !== "gpt-4o-mini" || !Array.isArray(body.messages)) {
        sendJson(response, 400, { error: { message: "Invalid chat completion request" } });
        return;
      }
      const hasToolResult = body.messages.some((message) =>
        isLocalToolResult(message)
      );
      if (hasToolResult) toolResults += 1;
      const shellTool = findShellTool(body.tools);
      if (!hasToolResult && shellTool == null) {
        sendJson(response, 400, { error: { message: "Goose omitted developer shell tool" } });
        return;
      }
      if (!hasToolResult) toolCalls += 1;
      sendChatCompletion(response, body.stream === true, requestCount, shellTool, hasToolResult);
    } catch (error) {
      sendJson(response, 500, {
        error: { message: error instanceof Error ? error.message : String(error) },
      });
    }
  });
  await new Promise<void>((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolvePromise());
  });
  const address = server.address() as AddressInfo;
  return {
    origin: `http://127.0.0.1:${address.port}`,
    assertComplete: () => {
      if (requestCount < 4 || toolCalls < 2 || toolResults < 2) {
        throw new Error(
          `Local Goose provider lifecycle incomplete: ${JSON.stringify({ requestCount, toolCalls, toolResults })}`,
        );
      }
    },
    close: () => new Promise<void>((resolvePromise, reject) => {
      server.close((error) => error ? reject(error) : resolvePromise());
    }),
  };
}

async function readJsonBody(request: IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  let bytes = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    bytes += buffer.length;
    if (bytes > 4_000_000) throw new Error("Local provider request exceeded 4000000 bytes");
    chunks.push(buffer);
  }
  const value = JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Local provider request must be an object");
  }
  return value as Record<string, unknown>;
}

function isLocalToolResult(value: unknown): boolean {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const message = value as { role?: unknown; content?: unknown };
  if (message.role !== "tool" || typeof message.content !== "string") return false;
  if (/ENOENT|Failed to start terminal command|not found/iu.test(message.content)) {
    throw new Error(`Goose developer tool failed locally: ${message.content.slice(0, 1_000)}`);
  }
  return message.content.includes(LOCAL_TOOL_OUTPUT);
}

function findShellTool(value: unknown): string | undefined {
  if (!Array.isArray(value)) return undefined;
  for (const item of value) {
    if (typeof item !== "object" || item === null || Array.isArray(item)) continue;
    const candidate = (item as { function?: unknown }).function;
    if (typeof candidate !== "object" || candidate === null || Array.isArray(candidate)) continue;
    const name = (candidate as { name?: unknown }).name;
    if (typeof name === "string" && (name === "shell" || name.endsWith("__shell"))) {
      return name;
    }
  }
  return undefined;
}

function sendChatCompletion(
  response: ServerResponse,
  stream: boolean,
  sequence: number,
  shellTool: string | undefined,
  hasToolResult: boolean,
): void {
  const id = `chatcmpl-attyd-${sequence}`;
  const message = hasToolResult
    ? { role: "assistant", content: LOCAL_AGENT_OUTPUT }
    : {
        role: "assistant",
        content: null,
        tool_calls: [{
          id: `call-attyd-${sequence}`,
          type: "function",
          function: {
            name: shellTool,
            arguments: JSON.stringify({ command: `printf ${LOCAL_TOOL_OUTPUT}` }),
          },
        }],
      };
  const finishReason = hasToolResult ? "stop" : "tool_calls";
  const usage = { prompt_tokens: 11, completion_tokens: 3, total_tokens: 14 };
  if (!stream) {
    sendJson(response, 200, {
      id,
      object: "chat.completion",
      created: 0,
      model: "gpt-4o-mini",
      choices: [{ index: 0, message, finish_reason: finishReason }],
      usage,
    });
    return;
  }
  response.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  const delta = hasToolResult
    ? { role: "assistant", content: LOCAL_AGENT_OUTPUT }
    : {
        role: "assistant",
        tool_calls: [{
          index: 0,
          id: `call-attyd-${sequence}`,
          type: "function",
          function: {
            name: shellTool,
            arguments: JSON.stringify({ command: `printf ${LOCAL_TOOL_OUTPUT}` }),
          },
        }],
      };
  writeSse(response, {
    id,
    object: "chat.completion.chunk",
    created: 0,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta, finish_reason: null }],
  });
  writeSse(response, {
    id,
    object: "chat.completion.chunk",
    created: 0,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta: {}, finish_reason: finishReason }],
    usage,
  });
  response.end("data: [DONE]\n\n");
}

function writeSse(response: ServerResponse, value: unknown): void {
  response.write(`data: ${JSON.stringify(value)}\n\n`);
}

function sendJson(response: ServerResponse, status: number, value: unknown): void {
  response.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  response.end(JSON.stringify(value));
}

async function exerciseConfiguredRaw(
  expectedVersion: string,
  env: NodeJS.ProcessEnv,
): Promise<void> {
  const child = spawn(resolve("bin/goose"), ["acp", "--with-builtin", "developer"], {
    cwd: process.cwd(),
    env,
    stdio: ["pipe", "pipe", "pipe"],
  });
  let childStderr = "";
  child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
    childStderr = `${childStderr}${chunk}`.slice(-20_000);
  });
  const timeout = setTimeout(() => child.kill(), 20_000);
  try {
    const updates: acp.SessionUpdate[] = [];
    const client = acp
      .client({ name: "attyd-goose-configured-smoke" })
      .onNotification(acp.methods.client.session.update, ({ params }) => {
        updates.push(params.update);
      })
      .onRequest(acp.methods.client.session.requestPermission, ({ params }) => {
        const option = params.options.find(({ kind }) => kind.startsWith("allow"));
        if (!option) return { outcome: { outcome: "cancelled" as const } };
        return {
          outcome: { outcome: "selected" as const, optionId: option.optionId },
        };
      });
    const stream = acp.ndJsonStream(
      Writable.toWeb(child.stdin),
      Readable.toWeb(child.stdout) as ReadableStream<Uint8Array>,
    );
    const connection = client.connect(stream);
    const initialized = await connection.agent.request(acp.methods.agent.initialize, {
      protocolVersion: acp.PROTOCOL_VERSION,
      clientCapabilities: {},
      clientInfo: { name: "attyd-goose-configured-smoke", version: "0.1.0" },
    });
    if (
      initialized.agentInfo?.name !== "goose" ||
      initialized.agentInfo.version !== expectedVersion
    ) {
      throw new Error(`Configured Goose raw identity mismatch: ${JSON.stringify(initialized.agentInfo)}`);
    }
    const session = await connection.agent.request(acp.methods.agent.session.new, {
      cwd: process.cwd(),
      mcpServers: [],
    });
    const prompt = await connection.agent.request(acp.methods.agent.session.prompt, {
      sessionId: session.sessionId,
      prompt: [{
        type: "text",
        text: `Use the shell tool to print ${LOCAL_TOOL_OUTPUT}, then report completion.`,
      }],
    });
    if (!updates.some(({ sessionUpdate }) => sessionUpdate === "tool_call")) {
      throw new Error(`Configured Goose raw prompt emitted no tool call: ${JSON.stringify(updates)}`);
    }
    if (!updates.some((update) => {
      if (update.sessionUpdate !== "tool_call_update" || update.status !== "completed") return false;
      const rawOutput = JSON.stringify(update.rawOutput) ?? "";
      return rawOutput.includes(LOCAL_TOOL_OUTPUT) && rawOutput.includes('"exit_code":0');
    })) {
      throw new Error(`Configured Goose raw tool lifecycle was incomplete: ${JSON.stringify(updates)}`);
    }
    if (!updates.some((update) =>
      update.sessionUpdate === "agent_message_chunk" &&
      update.content.type === "text" &&
      update.content.text.includes(LOCAL_AGENT_OUTPUT)
    )) {
      throw new Error(`Configured Goose raw prompt lost Agent output: ${JSON.stringify(updates)}`);
    }
    if (prompt.stopReason !== "end_turn") {
      throw new Error(`Configured Goose raw prompt stopped unexpectedly: ${JSON.stringify(prompt)}`);
    }
    connection.close();
  } catch (error) {
    throw new Error(
      `Configured Goose raw lifecycle failed: ${error instanceof Error ? error.message : String(error)}` +
        (childStderr ? `\n${childStderr}` : ""),
    );
  } finally {
    clearTimeout(timeout);
    if (child.exitCode == null) child.kill();
  }
}

async function exerciseConfiguredBridge(
  expectedVersion: string,
  env: NodeJS.ProcessEnv,
): Promise<void> {
  const server = await startRustTestServer({
    command: [resolve("bin/goose"), "acp", "--with-builtin", "developer"],
    cwd: process.cwd(),
    env,
  });
  const origin = `http://127.0.0.1:${server.port}`;
  let observer: AbortController | undefined;
  let observerReader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const runtime = await waitForValue(
      () => getJson<RuntimeView>(`${origin}/api/v1/runtime`),
      (value) => value.connected && value.phase?.phase === "ready",
      "configured Goose bridge initialization",
    );
    if (
      runtime.initialized?.response?.agentInfo?.name !== "goose" ||
      runtime.initialized.response.agentInfo.version !== expectedVersion
    ) {
      throw new Error(`Configured Goose bridge identity mismatch: ${JSON.stringify(runtime.initialized)}`);
    }

    const created = await postJson<CreatedSession>(
      `${origin}/api/v1/sessions`,
      { cwd: process.cwd() },
      201,
    );
    const sessionId = created.sessionId;
    if (!created.view.historyRevision) throw new Error("Configured Goose session has no revision");

    observer = new AbortController();
    const observed = await fetch(
      `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}/events`,
      { signal: observer.signal },
    );
    if (observed.status !== 200) {
      throw new Error(`Configured Goose session observer returned ${observed.status}`);
    }
    observerReader = observed.body?.getReader();
    await observerReader?.read();

    await postTurn(
      origin,
      sessionId,
      created.view.historyRevision,
      "goose-configured-prompt",
      `Use the shell tool to print ${LOCAL_TOOL_OUTPUT}, then report completion.`,
    );
    const completed = await waitForConfiguredBridgeTurn(origin, sessionId);
    const timeline = JSON.stringify(completed.timeline);
    if (
      completed.syncError != null ||
      !timeline.includes('"sessionUpdate":"tool_call"') ||
      !timeline.includes('"rawOutput"') ||
      !timeline.includes(LOCAL_TOOL_OUTPUT) ||
      !timeline.includes(LOCAL_AGENT_OUTPUT)
    ) {
      throw new Error(`Configured Goose bridge lost its tool turn: ${JSON.stringify(completed)}`);
    }
  } finally {
    observer?.abort();
    await observerReader?.cancel().catch(() => undefined);
    await server.close();
  }
}

async function waitForConfiguredBridgeTurn(
  origin: string,
  sessionId: string,
): Promise<SessionView> {
  const responded = new Set<string>();
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const view = await getSession(origin, sessionId);
    for (const permission of Object.values(view.interactions.permissions)) {
      if (responded.has(permission.interactionId)) continue;
      responded.add(permission.interactionId);
      const option = permission.request.options?.find(({ kind }) => kind.startsWith("allow"));
      await postJson(
        `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}` +
          `/interactions/${encodeURIComponent(permission.interactionId)}/response`,
        {
          kind: "permission",
          outcome: option
            ? { outcome: "selected", optionId: option.optionId }
            : { outcome: "cancelled" },
        },
      );
    }
    if (view.phase === "blocked") {
      throw new Error(`Configured Goose bridge turn blocked: ${JSON.stringify(view)}`);
    }
    if (view.phase === "ready" && view.activeTurn == null && view.timeline.length > 0) return view;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
  }
  throw new Error("Timed out waiting for configured Goose bridge turn");
}

async function exerciseBridge(
  expectedVersion: string,
  env: NodeJS.ProcessEnv,
): Promise<void> {
  const server = await startRustTestServer({
    command: [resolve("bin/goose"), "acp"],
    cwd: process.cwd(),
    env,
  });
  const origin = `http://127.0.0.1:${server.port}`;
  try {
    const runtime = await waitForValue(
      () => getJson<RuntimeView>(`${origin}/api/v1/runtime`),
      (value) => value.connected && value.phase?.phase === "ready",
      "isolated Goose bridge initialization",
    );
    if (
      runtime.initialized?.response?.agentInfo?.name !== "goose" ||
      runtime.initialized.response.agentInfo.version !== expectedVersion
    ) {
      throw new Error(`Goose bridge did not initialize correctly: ${JSON.stringify(runtime.initialized)}`);
    }

    const create = await fetch(`${origin}/api/v1/sessions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ cwd: process.cwd() }),
    });
    const error = await create.json() as {
      code?: number;
      message?: string;
      data?: unknown;
    };
    if (
      create.status !== 409 ||
      error.code !== -32_603 ||
      !JSON.stringify(error.data).includes("GOOSE_PROVIDER")
    ) {
      throw new Error(`Goose bridge lost ACP error details: ${JSON.stringify(error)}`);
    }

    const listed = await getJson<{ sessions: unknown[]; nextCursor?: unknown }>(
      `${origin}/api/v1/sessions`,
    );
    if (listed.sessions.length !== 0 || listed.nextCursor != null) {
      throw new Error(`Goose bridge recovery list was invalid: ${JSON.stringify(listed)}`);
    }
    const recovered = await getJson<RuntimeView>(`${origin}/api/v1/runtime`);
    if (!recovered.connected || recovered.phase?.phase !== "ready") {
      throw new Error("Goose bridge did not remain usable after session/new failed");
    }
  } finally {
    await server.close();
  }
}

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (response.status !== 200) {
    throw new Error(`${url} returned ${response.status}: ${await response.text()}`);
  }
  return await response.json() as T;
}

async function postJson<T = unknown>(
  url: string,
  body: unknown,
  expectedStatus = 200,
  headers: Record<string, string> = {},
): Promise<T> {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  if (response.status !== expectedStatus) {
    throw new Error(`${url} returned ${response.status}: ${text}`);
  }
  return (text === "" ? undefined : JSON.parse(text)) as T;
}

async function getSession(origin: string, sessionId: string): Promise<SessionView> {
  return await getJson<SessionView>(
    `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}`,
  );
}

async function postTurn(
  origin: string,
  sessionId: string,
  historyRevision: string,
  intentId: string,
  text: string,
): Promise<void> {
  await postJson(
    `${origin}/api/v1/sessions/${encodeURIComponent(sessionId)}/turns`,
    { prompt: [{ type: "text", text }] },
    202,
    {
      "If-Match": `"${historyRevision}"`,
      "Idempotency-Key": intentId,
    },
  );
}

async function waitForValue<T>(
  read: () => Promise<T>,
  predicate: (value: T) => boolean,
  label: string,
): Promise<T> {
  const deadline = Date.now() + 10_000;
  let latest: T | undefined;
  while (Date.now() < deadline) {
    latest = await read();
    if (predicate(latest)) return latest;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 10));
  }
  throw new Error(`Timed out waiting for ${label}: ${JSON.stringify(latest)}`);
}
