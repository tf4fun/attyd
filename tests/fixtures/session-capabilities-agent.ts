import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";

const noList = process.argv.includes("--no-list");
const noLoad = process.argv.includes("--no-load");
const splitList = process.argv.includes("--split-list");
const listDiagnostics = process.argv.includes("--large-list-meta") ? "x".repeat(2_000_000) : undefined;
let page = 0;
let forks = 0;
let resumes = 0;
let loads = 0;
let closes = 0;
let prompts = 0;
let controls = 0;
const waitingPrompts = new Map<string, () => void>();

const agent = acp.agent({ name: "session-capabilities-fixture" })
  .onRequest(acp.methods.agent.initialize, () => ({
    protocolVersion: acp.PROTOCOL_VERSION,
    agentCapabilities: {
      loadSession: !noLoad,
      auth: { logout: {} },
      sessionCapabilities: { ...(!noList ? { list: {} } : {}), fork: {}, resume: {}, ...(process.argv.includes("--close") ? { close: {} } : {}) },
    },
    authMethods: [],
  }))
  .onRequest(acp.methods.agent.session.new, () => ({ sessionId: "created",
    modes: { currentModeId: "chat", availableModes: [{ id: "chat", name: "Chat" }, { id: "plan", name: "Plan" }] },
    configOptions: [{ type: "boolean", id: "verbose", name: "Verbose", currentValue: false }],
  }))
  .onRequest(acp.methods.agent.session.list, async ({ params }) => {
    if (noList) throw acp.RequestError.methodNotFound("session/list");
    await new Promise((resolve) => setTimeout(resolve, 15));
    return {
      sessions: [{ sessionId: splitList && params.cursor == null ? "first" : "saved", cwd: process.cwd() }],
      ...(params.cursor == null ? { nextCursor: `page-${++page}` } : {}),
      _meta: { forks, resumes, loads, closes, prompts, controls, requestedCursor: params.cursor ?? null, listDiagnostics },
    };
  })
  .onRequest(acp.methods.agent.session.load, async ({ params, client }) => {
    loads += 1;
    if (noLoad) throw acp.RequestError.methodNotFound("session/load");
    if (params.sessionId === "forked" && process.argv.includes("--empty-fork-history")) return {};
    if (params.sessionId !== "saved") throw new acp.RequestError(-32002, "Session not found");
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `Loaded ${params.cwd}` } },
    });
    return {};
  })
  .onRequest(acp.methods.agent.session.fork, () => { forks += 1; return { sessionId: "forked" }; })
  .onRequest(acp.methods.agent.session.resume, () => { resumes += 1; return {}; })
  .onRequest(acp.methods.agent.session.prompt, async ({ params, client }) => {
    prompts += 1;
    if (params.prompt.some((block) => block.type === "text" && block.text === "wait-close")) {
      const wait = new Promise<void>((resolve) => waitingPrompts.set(params.sessionId, resolve));
      await client.notify(acp.methods.client.session.update, {
        sessionId: params.sessionId,
        update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Waiting for close." } },
      });
      await wait;
      return { stopReason: "cancelled" };
    }
    return { stopReason: "end_turn" };
  })
  .onRequest(acp.methods.agent.session.setMode, () => { controls += 1; return {}; })
  .onRequest(acp.methods.agent.session.setConfigOption, ({ params }) => {
    controls += 1;
    return { configOptions: [{ type: "boolean", id: "verbose", name: "Verbose", currentValue: params.value === true }] };
  })
  .onNotification(acp.methods.agent.session.cancel, ({ params }) => {
    waitingPrompts.get(params.sessionId)?.();
    waitingPrompts.delete(params.sessionId);
  })
  .onRequest(acp.methods.agent.session.close, ({ params }) => {
    closes += 1;
    if (process.argv.includes("--fail-close")) throw new acp.RequestError(-32600, "Close refused");
    const finish = waitingPrompts.get(params.sessionId);
    waitingPrompts.delete(params.sessionId);
    // The response to a cancelled prompt can arrive after close has acknowledged.
    if (finish) setTimeout(finish, 200);
    return {};
  })
  .onRequest(acp.methods.agent.logout, () => ({}));

await agent.connect(acp.ndJsonStream(
  Writable.toWeb(process.stdout) as WritableStream<Uint8Array>,
  Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>,
)).closed;
