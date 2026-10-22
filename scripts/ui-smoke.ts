import assert from "node:assert/strict";
import { join } from "node:path";
import type { PromptResult } from "../shared/protocol.js";
import {
  catalogLive, entriesOf, globalLive, promptsOf, sessionLive, sessionScope, SmokeClient,
} from "./protocol-smoke-client.js";
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

  const client = new SmokeClient(origin);
  try {
    client.follow([{ scope: "global" }]);
    const agent = await client.until("global", (replica) => {
      const live = globalLive(replica);
      return live?.agent.phase === "ready" ? live.agent : undefined;
    }, "Agent initialization");
    assert.equal(agent.initialize?.agentInfo?.name, "attyd-test-agent");

    client.follow([{ scope: "global" }, { scope: "catalog" }]);
    const catalog = await client.run("catalog", { kind: "list", args: {} });
    assert.equal(catalog.status, "completed", JSON.stringify(catalog));
    // The page is in the catalog's document, not in a held response.
    const sessions = catalogLive(client.replicas.get("catalog"))!.pages.flatMap((page) => [...page.sessions]);
    assert.deepEqual(sessions.map(({ sessionId }) => sessionId), ["saved-session", "earlier-session"]);

    // Subscribing loads the session; the stream stays open to the end.
    const scope = sessionScope("saved-session");
    client.follow([{ scope: "global" }, { scope: "catalog" }, { scope, cwd: sessions[0].cwd }]);
    const loaded = await client.until(scope, (replica) => {
      const live = sessionLive(replica);
      return replica?.doc.history === "complete" && live?.op.status !== "running"
        ? { entries: entriesOf(replica), op: live!.op } : undefined;
    }, "history load");
    assert.equal(loaded.op.kind, "load");
    assert.equal(loaded.op.status, "completed");
    assert.match(JSON.stringify(loaded.entries), /Loaded history\./u);

    const prompt = await client.ops.write(scope, {
      kind: "prompt",
      args: { prompt: [{ type: "text", text: "usage-flow" }] },
    });
    assert.ok(prompt.kind === "accepted", JSON.stringify(prompt));
    const completed = await client.ended(scope, prompt.op);
    assert.equal(completed.status, "completed");
    assert.equal((completed.result as PromptResult).stopReason, "max_tokens");
    const entries = entriesOf(client.replicas.get(scope));
    assert.ok(entries.length > loaded.entries.length);
    assert.match(JSON.stringify(entries.slice(loaded.entries.length)), /usage-flow/u);
    // The prompt keeps how it ended.
    assert.deepEqual(promptsOf(client.replicas.get(scope)).at(-1)?.end, { response: { stopReason: "max_tokens" } });

    const deletion = await client.ops.write("global", { kind: "delete", args: { sessionId: "saved-session" } });
    assert.ok(deletion.kind === "accepted", JSON.stringify(deletion));
    await client.until(scope, (replica) => (replica === undefined ? true : undefined), "the deleted session's end");
    assert.equal((await client.ended("global", deletion.op)).status, "completed");
    assert.ok(!Object.hasOwn(globalLive(client.replicas.get("global"))!.sessions, "saved-session"));
    const afterDelete = await client.run("catalog", { kind: "list", args: {} });
    assert.equal(afterDelete.status, "completed", JSON.stringify(afterDelete));
    const remaining = catalogLive(client.replicas.get("catalog"))!.pages.flatMap((page) => [...page.sessions]);
    assert.ok(!remaining.some(({ sessionId }) => sessionId === "saved-session"));
    assert.deepEqual(client.streamErrors, []);
  } finally {
    client.close();
  }

  console.log("Rust-hosted protocol revision 3 UI smoke passed");
} finally {
  await server.close();
}
