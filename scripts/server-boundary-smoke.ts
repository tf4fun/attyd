import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { request } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import * as acp from "@agentclientprotocol/sdk";
import { startRustTestServer } from "./rust-test-server.js";
import type { OpArgs } from "../shared/protocol.js";
import {
  agentMessages, globalLive, replyText, sessionLive, sessionScope, SmokeClient,
} from "./protocol-smoke-client.js";

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
let client: SmokeClient | undefined;

try {
  const origin = await eventually(() => /attyd listening on (http:\/\/[^\s]+)/u.exec(stdout)?.[1], "HTTP listener");
  client = new SmokeClient(origin);
  client.follow([{ scope: "global" }, { scope: "catalog" }]);
  await client.until("global", (replica) => (globalLive(replica)?.agent.phase === "ready" ? true : undefined),
    "fake Agent initialization");
  const live = client;
  // The Agent lists sessions only while signed in: `list` succeeds, or ends
  // with the Agent's own auth-required error, carried verbatim.
  const authRequired = acp.RequestError.authRequired({ hint: "Use agent-login" });
  const listed = async () => {
    const op = await live.run("catalog", { kind: "list", args: {} });
    if (op.status === "completed") return true;
    assert.equal(op.status, "failed", JSON.stringify(op));
    assert.equal(op.error?.code, "agent_error", JSON.stringify(op));
    assert.deepEqual(op.error?.data, {
      code: authRequired.code, message: authRequired.message, data: authRequired.data,
    });
    return false;
  };
  assert.equal(await listed(), true);
  const globalOp = async () => (await live.ended("global")).id;
  /** A global write with `headers`: admitted, then shown completed. */
  const writeGlobal = async (write: OpArgs, headers: Record<string, string>) => {
    const response = await fetch(`${origin}/api/v2/targets/global/ops`, {
      method: "POST",
      headers: { "Content-Type": "application/json", ...headers },
      body: JSON.stringify({ expect: await globalOp(), ...write }),
    });
    const text = await response.text();
    assert.equal(response.status, 202, `${write.kind} with ${JSON.stringify(headers)}: ${text}`);
    const ended = await live.ended("global", (JSON.parse(text) as { op: string }).op);
    assert.equal(ended.status, "completed", `${write.kind}: ${JSON.stringify(ended.error)}`);
  };

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
    // A real write and real reads, refused for their origin alone.
    const attack = await fetch(`${origin}/api/v2/targets/global/ops`, {
      method: "POST",
      headers: { Origin: attackOrigin, "Content-Type": "application/x-www-form-urlencoded" },
      body: "unused=1",
    });
    assert.equal(attack.status, 403, `unexpected acceptance of ${attackOrigin}`);
    const forged = await fetch(`${origin}/api/v2/targets/global/ops`, {
      method: "POST",
      headers: { Origin: attackOrigin, "Content-Type": "application/json" },
      body: JSON.stringify({ expect: await globalOp(), kind: "logout", args: {} }),
    });
    assert.equal(forged.status, 403, `unexpected acceptance of a JSON write from ${attackOrigin}`);
    assert.equal((await fetch(`${origin}/api/health`, { headers: { Origin: attackOrigin } })).status, 403, "cross-origin reads must also be denied");
    assert.equal((await fetch(`${origin}/api/v2/stream?follow=%2Fglobal`, { headers: { Origin: attackOrigin } })).status, 403, "cross-origin streams must also be denied");
    assert.equal(await listed(), true, "a rejected logout must not change Agent authentication");
  }

  const logout = { kind: "logout", args: {} } as const;
  const login = { kind: "authenticate", args: { methodId: "agent-login" } } as const;
  await writeGlobal(logout, { Origin: origin });
  assert.equal(await listed(), false, "same-origin logout must reach the Agent");
  await writeGlobal(login, {});
  assert.equal(await listed(), true, "CLI without Origin must work");
  await writeGlobal(logout, {});
  assert.equal(await listed(), false, "CLI logout must reach the Agent");
  await writeGlobal(login, { Origin: "https://agent.example" });
  assert.equal(await listed(), true, "explicit TLS proxy origin with rewritten upstream Host must work");

  assert.equal(await statusWithHost(origin, { Host: "evil.example" }), 403, "DNS rebinding Host must be rejected");
  assert.equal(await statusWithHost(origin, { Host: "evil.example", "X-Forwarded-Host": new URL(origin).host }), 403);
  assert.equal(await statusWithHost(origin, { Host: "agent.example", Origin: "https://agent.example" }), 200);
  assert.equal(await statusWithHost(origin, { Host: "agent.example", Origin: "http://agent.example", "X-Forwarded-Proto": "https" }), 403);

  await verifyLargeRequestBody(origin, live);

  live.close();
  const events = await fetch(`${origin}/api/v2/stream?follow=%2Fglobal`, { signal: observer.signal });
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
  await verifyWildcardOrigin();
  console.log("server boundary smoke passed");
} catch (error) {
  console.error({ stdout, stderr });
  throw error;
} finally {
  client?.close();
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

async function verifyWildcardOrigin(): Promise<void> {
  const server = await startRustTestServer({
    command: [process.execPath, "--import", "tsx", "tests/fixtures/fake-agent.ts"],
    args: ["--allowed-origin", "*"],
  });
  try {
    const origin = `http://127.0.0.1:${server.port}`;
    for (const headers of [
      { Host: "custom.example:8443" },
      { Host: "custom.example:8443", Origin: "https://custom.example:8443" },
      { Host: "custom.example:8443", Origin: "https://other.example" },
      { Host: `127.0.0.1:${server.port}`, Origin: "http://other.example:8000" },
    ]) {
      assert.equal(await statusWithHost(origin, headers), 200, JSON.stringify(headers));
    }
    for (const invalidOrigin of ["null", "*", "https://custom.example/path"]) {
      assert.equal(await statusWithHost(origin, { Host: "custom.example", Origin: invalidOrigin }), 403);
    }
  } finally {
    await server.close();
  }
}

function statusWithHost(origin: string, headers: Record<string, string | undefined>): Promise<number> {
  return new Promise((resolve, reject) => {
    const req = request(`${origin}/api/health`, { headers }, (response) => {
      response.resume();
      response.once("end", () => resolve(response.statusCode!));
    });
    req.once("error", reject);
    req.end();
  });
}

async function verifyLargeRequestBody(origin: string, client: SmokeClient): Promise<void> {
  const created = await client.run("global", { kind: "create", args: { cwd } });
  assert.equal(created.status, "completed", JSON.stringify(created));
  const sessionId = (created.result as { sessionId: string }).sessionId;
  const scope = sessionScope(sessionId);
  // The stream holds and follows the session until its result is read.
  client.follow([{ scope: "global" }, { scope: "catalog" }, { scope }]);
  const opened = await client.ended(scope);
  // Valid PCM WAV beyond the former 3 MiB attachment and 5 MiB HTTP limits.
  // Verify the Agent receives every decoded byte and all prompt blocks.
  const wav = Buffer.alloc(4 * 1024 * 1024);
  wav.write("RIFF", 0); wav.writeUInt32LE(wav.length - 8, 4); wav.write("WAVEfmt ", 8);
  wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(8_000, 24); wav.writeUInt32LE(16_000, 28);
  wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34);
  wav.write("data", 36); wav.writeUInt32LE(wav.length - 44, 40);
  const body = JSON.stringify({ expect: opened.id, kind: "prompt", args: { prompt: [
    ...Array.from({ length: 65 }, () => ({ type: "text", text: "large-attachment-input-flow" })),
    { type: "audio", mimeType: "audio/wav", data: wav.toString("base64") },
  ] } });
  assert.ok(Buffer.byteLength(body) > 5 * 1024 * 1024);
  const accepted = await postJson(`${origin}/api/v2/targets/sessions/${encodeURIComponent(sessionId)}/ops`, { "Content-Type": "application/json" }, body);
  assert.equal(accepted.status, 202, "a supported attachment must reach the prompt");
  const op = (JSON.parse(accepted.body) as { op: string }).op;
  const completed = await client.ended(scope, op);
  assert.equal(completed.status, "completed", JSON.stringify(completed.error));
  const replies = agentMessages(client.replicas.get(scope));
  assert.equal(replies.length, 1);
  assert.deepEqual(JSON.parse(replyText(replies[0])), [{ mimeType: "audio/wav", bytes: wav.length }]);
  assert.equal(sessionLive(client.replicas.get(scope))?.sessionId, sessionId);
  assert.deepEqual(client.streamErrors, []);
}

function postJson(url: string, headers: Record<string, string>, body: string): Promise<{ status: number; body: string }> {
  return new Promise((resolve, reject) => {
    const req = request(url, {
      method: "POST",
      headers: { ...headers, "Content-Length": Buffer.byteLength(body), Expect: "100-continue" },
    }, (response) => {
      let text = "";
      response.setEncoding("utf8");
      response.on("data", (chunk: string) => { text += chunk; });
      response.once("end", () => {
        resolve({ status: response.statusCode!, body: text });
        req.destroy();
      });
      response.once("error", reject);
    });
    // Exercise the HTTP 100-continue handshake with the complete large body.
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
