import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import type { BridgeSessionView, CreatedSessionResult } from "../web/src/lib/business-api.js";

// Real HTTP and process-lifecycle checks; no model, account, or user workspace.
const cwd = process.cwd();
const binary = process.env.ATTYD_RUST_BINARY ?? join(cwd, "target/debug/attyd");
const temporary = await mkdtemp(join(tmpdir(), "attyd-server-boundary-"));
const marker = join(temporary, "agent.pid");
const descendantMarker = join(temporary, "descendant.pid");
const wrapper = join(temporary, "agent.mjs");
await writeFile(wrapper, `
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
if (process.platform !== "win32") {
  const child = spawn(process.execPath, ["-e", "process.on('SIGTERM', () => {}); setInterval(() => {}, 1000)"], { stdio: "ignore" });
  writeFileSync(${JSON.stringify(descendantMarker)}, String(child.pid));
}
await import(${JSON.stringify(pathToFileURL(join(cwd, "tests/fixtures/fake-agent.ts")).href)});
`);
const host = spawn(binary, [
  "--host", "127.0.0.1", "--port", "0", "--allowed-origin", "https://agent.example",
  "--", process.execPath, "--import", "tsx", wrapper,
], {
  cwd,
  env: { ...process.env, ATTYD_FAKE_PROCESS_MARKER_FILE: marker },
  stdio: ["ignore", "pipe", "pipe"],
});
const exited = new Promise<{ code: number | null; signal: string | null }>((resolve, reject) => {
  host.once("exit", (code, signal) => resolve({ code, signal }));
  host.once("error", reject);
});
let stdout = "";
let stderr = "";
host.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString(); });
host.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString(); });
const observer = new AbortController();

try {
  const origin = await eventually(() => /attyd listening on (http:\/\/[^\s]+)/u.exec(stdout)?.[1], "HTTP listener");
  await eventually(async () => {
    const runtime = await (await fetch(`${origin}/api/v1/runtime`)).json() as { phase?: { phase?: string } };
    return runtime.phase?.phase === "ready";
  }, "fake Agent initialization");
  const list = () => fetch(`${origin}/api/v1/sessions`);
  assert.equal((await list()).status, 200);

  // Iframe navigations can omit Origin, so request-origin checks alone cannot
  // prevent another site from presenting the Agent controls inside a frame.
  const documentPaths = ["/", "/index.html", "/projects/%2Fworkspace/sessions/frame-regression"];
  const framingHeaders = [];
  for (const path of documentPaths) {
    const response = await fetch(`${origin}${path}`);
    assert.equal(response.status, 200, `HTML navigation must work at ${path}`);
    assert.match(response.headers.get("content-type") ?? "", /^text\/html\b/u);
    await response.text();
    framingHeaders.push({
      path,
      csp: response.headers.get("content-security-policy"),
      xFrameOptions: response.headers.get("x-frame-options"),
    });
  }
  assert.deepEqual(framingHeaders, documentPaths.map((path) => ({
    path, csp: "frame-ancestors 'none'", xFrameOptions: "DENY",
  })), "entry HTML and SPA routes must deny framing even without Origin");

  for (const attackOrigin of ["http://127.0.0.1:1", "https://evil.example", "null"]) {
    const attack = await fetch(`${origin}/api/v1/auth/logout`, {
      method: "POST",
      headers: { Origin: attackOrigin, "Content-Type": "application/x-www-form-urlencoded" },
      body: "unused=1",
    });
    assert.equal(attack.status, 403, `unexpected acceptance of ${attackOrigin}`);
    assert.equal((await fetch(`${origin}/api/v1/runtime`, { headers: { Origin: attackOrigin } })).status, 403, "cross-origin reads must also be denied");
    assert.equal((await list()).status, 200, "rejected logout must not change Agent authentication");
  }

  const sameOriginLogout = await fetch(`${origin}/api/v1/auth/logout`, { method: "POST", headers: { Origin: origin } });
  assert.equal(sameOriginLogout.status, 200);
  assert.equal((await list()).status, 409, "same-origin logout must reach the Agent");
  assert.equal((await fetch(`${origin}/api/v1/auth/agent-login`, { method: "POST" })).status, 200, "CLI without Origin must work");
  assert.equal((await list()).status, 200);
  assert.equal((await fetch(`${origin}/api/v1/auth/logout`, { method: "POST" })).status, 200);
  assert.equal((await list()).status, 409, "CLI logout must reach the Agent");
  assert.equal((await fetch(`${origin}/api/v1/auth/agent-login`, { method: "POST", headers: { Origin: "https://agent.example" } })).status, 200, "explicit TLS proxy origin with rewritten upstream Host must work");
  assert.equal((await list()).status, 200);

  assert.equal(await statusWithHost(origin, { Host: "evil.example" }), 403, "DNS rebinding Host must be rejected");
  assert.equal(await statusWithHost(origin, { Host: "evil.example", "X-Forwarded-Host": new URL(origin).host }), 403);
  assert.equal(await statusWithHost(origin, { Host: "agent.example", Origin: "https://agent.example" }), 200);
  assert.equal(await statusWithHost(origin, { Host: "agent.example", Origin: "http://agent.example", "X-Forwarded-Proto": "https" }), 403);

  await verifyBodyLimits(origin);

  const events = await fetch(`${origin}/api/v1/events`, { signal: observer.signal });
  assert.equal(events.status, 200);
  const reader = events.body!.getReader();
  assert.equal((await reader.read()).done, false);
  const streamEnded = (async () => {
    while (!(await reader.read()).done) { /* keep SSE open through shutdown */ }
  })();
  const agentPid = Number(await readFile(marker, "utf8"));
  assert.ok(isAlive(agentPid));
  const descendantPid = process.platform === "win32" ? undefined : Number(await readFile(descendantMarker, "utf8"));
  if (descendantPid) assert.ok(isAlive(descendantPid));
  const started = Date.now();
  host.kill("SIGTERM");
  const exit = await deadline(exited, 8_000, "SIGTERM while SSE remains open");
  assert.equal(exit.code, 0, JSON.stringify(exit));
  await deadline(streamEnded, 1_000, "SSE end");
  await eventually(() => !isAlive(agentPid) && (!descendantPid || !isAlive(descendantPid)), "Agent process tree cleanup");
  console.log(JSON.stringify({ origin, deniedSimplePosts: 3, proxyOrigin: "https://agent.example", shutdownMs: Date.now() - started, agentPid, descendantPid, agentTreeExited: true }));

  const noCommand = spawn(binary, ["--port", "0"], { cwd, stdio: ["ignore", "pipe", "pipe"] });
  let noCommandOutput = "";
  noCommand.stdout.on("data", (chunk: Buffer) => { noCommandOutput += chunk.toString(); });
  noCommand.stderr.on("data", (chunk: Buffer) => { noCommandOutput += chunk.toString(); });
  const noCommandExit = await deadline(new Promise<number | null>((resolve) => noCommand.once("exit", resolve)), 5_000, "missing Agent command rejection");
  assert.notEqual(noCommandExit, 0);
  assert.match(noCommandOutput, /-- <agent-command> \[args\.\.\.\]/u);
  assert.doesNotMatch(noCommandOutput, /attyd listening/u);
  console.log("server boundary smoke passed");
} catch (error) {
  console.error({ stdout, stderr });
  throw error;
} finally {
  observer.abort();
  if (host.exitCode == null && host.signalCode == null) {
    host.kill("SIGTERM");
    try { await deadline(exited, 8_000, "cleanup"); } catch { host.kill("SIGKILL"); }
  }
  for (const file of [marker, descendantMarker]) {
    try {
      const pid = Number(await readFile(file, "utf8"));
      if (isAlive(pid)) process.kill(pid, "SIGKILL");
    } catch { /* fixture may not have started */ }
  }
  await rm(temporary, { recursive: true, force: true });
}

function statusWithHost(origin: string, headers: Record<string, string>): Promise<number> {
  return new Promise((resolve, reject) => {
    const req = request(`${origin}/api/v1/runtime`, { headers }, (response) => {
      response.resume();
      response.once("end", () => resolve(response.statusCode!));
    });
    req.once("error", reject);
    req.end();
  });
}

async function verifyBodyLimits(origin: string): Promise<void> {
  const createdResponse = await fetch(`${origin}/api/v1/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  assert.equal(createdResponse.status, 201);
  const created = await createdResponse.json() as CreatedSessionResult;
  const sessionUrl = `${origin}/api/v1/sessions/${encodeURIComponent(created.sessionId)}`;
  const view = async () => {
    const response = await fetch(sessionUrl);
    assert.equal(response.status, 200);
    return response.json() as Promise<BridgeSessionView>;
  };
  const before = await view();
  assert.ok(before.historyRevision);
  const headers = {
    "Content-Type": "application/json",
    "If-Match": `"${before.historyRevision}"`,
    "Idempotency-Key": "body-limit-regression",
  };
  const rejected = await postJson(
    `${sessionUrl}/turns`, headers,
    JSON.stringify({ prompt: [{ type: "text", text: "x".repeat(5 * 1024 * 1024) }] }),
  );
  assert.equal(rejected.status, 413, "requests over 5 MiB must be rejected before a turn starts");
  assert.equal((await view()).historyRevision, before.historyRevision);

  // Valid PCM WAV at the supported 3 MiB decoded attachment limit. Its base64
  // JSON body exceeds Axum's 2 MiB default and must still reach the ACP Agent.
  const wav = Buffer.alloc(3 * 1024 * 1024);
  wav.write("RIFF", 0); wav.writeUInt32LE(wav.length - 8, 4); wav.write("WAVEfmt ", 8);
  wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(8_000, 24); wav.writeUInt32LE(16_000, 28);
  wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34);
  wav.write("data", 36); wav.writeUInt32LE(wav.length - 44, 40);
  const body = JSON.stringify({ prompt: [
    { type: "text", text: "large-attachment-input-flow" },
    { type: "audio", mimeType: "audio/wav", data: wav.toString("base64") },
  ] });
  assert.ok(Buffer.byteLength(body) > 2 * 1024 * 1024);
  assert.ok(Buffer.byteLength(body) < 5 * 1024 * 1024);
  const controller = new AbortController();
  const events = await fetch(`${sessionUrl}/events`, { signal: controller.signal });
  assert.equal(events.status, 200);
  const reader = events.body!.getReader();
  const drain = (async () => {
    while (!(await reader.read()).done) { /* retain and consume the live session */ }
  })().catch((error: unknown) => { if (!controller.signal.aborted) throw error; });
  try {
    const accepted = await postJson(`${sessionUrl}/turns`, headers, body);
    assert.equal(accepted.status, 202, "a supported attachment must reach the turn handler");
    const completed = await eventually(async () => {
      const current = await view();
      return current.phase === "ready" && current.historyRevision !== before.historyRevision ? current : undefined;
    }, "large attachment completion");
    const result = completed.timeline.find((update) =>
      update.sessionUpdate === "agent_message_chunk" && update.messageId === "large-attachment-result"
    );
    assert.ok(result?.sessionUpdate === "agent_message_chunk" && result.content.type === "text");
    assert.deepEqual(JSON.parse(result.content.text), [{ mimeType: "audio/wav", bytes: wav.length }]);
  } finally {
    controller.abort();
    await drain;
  }
}

function postJson(url: string, headers: Record<string, string>, body: string): Promise<{ status: number }> {
  return new Promise((resolve, reject) => {
    const req = request(url, {
      method: "POST",
      headers: { ...headers, "Content-Length": Buffer.byteLength(body), Expect: "100-continue" },
    }, (response) => {
      response.resume();
      response.once("end", () => {
        resolve({ status: response.statusCode! });
        req.destroy();
      });
      response.once("error", reject);
    });
    // Let the server reject an oversized Content-Length before sending bytes;
    // otherwise an early 413 can race the upload and appear as a client EPIPE.
    req.once("continue", () => req.end(body));
    req.once("error", reject);
    req.setTimeout(10_000, () => req.destroy(new Error("JSON upload timed out")));
    req.flushHeaders();
  });
}

function isAlive(pid: number): boolean {
  assert.ok(Number.isInteger(pid) && pid > 1, "expected a fixture process PID");
  try { process.kill(pid, 0); return true; } catch { return false; }
}

async function eventually<T>(read: () => T | Promise<T>, label: string): Promise<NonNullable<T>> {
  const started = Date.now();
  while (Date.now() - started < 10_000) {
    const result = await read();
    if (result) return result;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`Timed out waiting for ${label}`);
}

async function deadline<T>(promise: Promise<T>, milliseconds: number, label: string): Promise<T> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timeout = setTimeout(() => reject(new Error(`Timed out waiting for ${label}`)), milliseconds);
    })]);
  } finally {
    clearTimeout(timeout);
  }
}
