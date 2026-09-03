import assert from "node:assert/strict";
import { join } from "node:path";
import type { BridgeSessionView, RuntimeView, SessionListResult } from "../web/src/lib/business-api.js";
import { startRustTestServer } from "./rust-test-server.js";

const cwd = process.cwd();
const server = await startRustTestServer({
  cwd,
  command: [
    process.execPath,
    "--import",
    "tsx",
    join(cwd, "tests/fixtures/fake-agent.ts"),
    "--early-new-updates",
    "--early-fork-updates",
  ],
});

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
  assert.match(page.headers.get("content-type") ?? "", /^text\/html/u);
  const html = await page.text();
  const scriptPath = html.match(/<script[^>]+src="([^"]+)"/u)?.[1];
  assert.ok(scriptPath, "production HTML must reference its JavaScript bundle");
  const bundle = await fetch(new URL(scriptPath, origin));
  assert.equal(bundle.status, 200);
  assert.match(bundle.headers.get("content-type") ?? "", /^text\/javascript/u);

  const runtime = await eventually(async () => {
    const candidate = await getJson<RuntimeView>(`${origin}/api/v1/runtime`);
    return candidate.connected && candidate.phase?.phase === "ready" ? candidate : undefined;
  });
  assert.equal(runtime.initialized?.response.agentInfo?.name, "attyd-test-agent");

  const listed = await getJson<SessionListResult>(`${origin}/api/v1/sessions`);
  assert.deepEqual(listed.sessions.map(({ sessionId }) => sessionId), [
    "saved-session",
    "earlier-session",
  ]);

  const initial = await getJson<BridgeSessionView>(
    `${origin}/api/v1/sessions/saved-session`,
  );
  assert.equal(initial.phase, "ready");
  assert.ok(initial.historyRevision);
  assert.match(JSON.stringify(initial.timeline), /Loaded history\./u);

  const reset = await readOneSseEvent(
    `${origin}/api/v1/sessions/saved-session/events`,
  );
  assert.equal(reset.type, "bridge/session_reset");
  assert.equal(reset.sessionId, "saved-session");

  const prompt = await fetch(`${origin}/api/v1/sessions/saved-session/turns`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Idempotency-Key": "ui-smoke-usage",
      "If-Match": `"${initial.historyRevision}"`,
    },
    body: JSON.stringify({
      prompt: [{ type: "text", text: "usage-flow" }],
    }),
  });
  assert.equal(prompt.status, 202);
  assert.equal((await prompt.json() as { disposition: string }).disposition, "accepted");

  const completed = await eventually(async () => {
    const candidate = await getJson<BridgeSessionView>(
      `${origin}/api/v1/sessions/saved-session`,
    );
    return candidate.phase === "ready" &&
        candidate.historyRevision !== initial.historyRevision
      ? candidate
      : undefined;
  });
  assert.match(JSON.stringify(completed.timeline), /usage-flow/u);

  const deletion = await fetch(`${origin}/api/v1/sessions/saved-session`, {
    method: "DELETE",
  });
  assert.equal(deletion.status, 204);
  const afterDelete = await getJson<SessionListResult>(`${origin}/api/v1/sessions`);
  assert.ok(!afterDelete.sessions.some(({ sessionId }) => sessionId === "saved-session"));

  console.log("Rust-hosted REST/SSE UI smoke passed");
} finally {
  await server.close();
}

async function getJson<T>(url: string): Promise<T> {
  const response = await fetch(url);
  assert.equal(response.status, 200, `${url} returned ${response.status}`);
  return response.json() as Promise<T>;
}

async function readOneSseEvent(url: string): Promise<Record<string, unknown>> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5_000);
  try {
    const response = await fetch(url, { signal: controller.signal });
    assert.equal(response.status, 200);
    assert.ok(response.body);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let pending = "";
    while (true) {
      const { done, value } = await reader.read();
      if (done) throw new Error("SSE stream ended before its first event");
      pending += decoder.decode(value, { stream: true });
      const boundary = pending.indexOf("\n\n");
      if (boundary < 0) continue;
      const data = pending.slice(0, boundary).split("\n")
        .find((line) => line.startsWith("data: "))?.slice(6);
      if (data == null) {
        pending = pending.slice(boundary + 2);
        continue;
      }
      await reader.cancel();
      return JSON.parse(data) as Record<string, unknown>;
    }
  } finally {
    clearTimeout(timeout);
    controller.abort();
  }
}

async function eventually<T>(probe: () => Promise<T | undefined>, timeout = 8_000): Promise<T> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    const value = await probe();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error("Timed out waiting for the Rust-hosted ACP state transition");
}
