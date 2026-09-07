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

const agent = acp.agent({ name: "session-capabilities-fixture" })
  .onRequest(acp.methods.agent.initialize, () => ({
    protocolVersion: acp.PROTOCOL_VERSION,
    agentCapabilities: {
      loadSession: !noLoad,
      auth: { logout: {} },
      sessionCapabilities: { ...(!noList ? { list: {} } : {}), fork: {}, resume: {} },
    },
    authMethods: [],
  }))
  .onRequest(acp.methods.agent.session.new, () => ({ sessionId: "created" }))
  .onRequest(acp.methods.agent.session.list, async ({ params }) => {
    if (noList) throw acp.RequestError.methodNotFound("session/list");
    await new Promise((resolve) => setTimeout(resolve, 15));
    return {
      sessions: [{ sessionId: splitList && params.cursor == null ? "first" : "saved", cwd: process.cwd() }],
      ...(params.cursor == null ? { nextCursor: `page-${++page}` } : {}),
      _meta: { forks, resumes, loads, requestedCursor: params.cursor ?? null, listDiagnostics },
    };
  })
  .onRequest(acp.methods.agent.session.load, async ({ params, client }) => {
    loads += 1;
    if (noLoad) throw acp.RequestError.methodNotFound("session/load");
    if (params.sessionId !== "saved") throw new acp.RequestError(-32002, "Session not found");
    await client.notify(acp.methods.client.session.update, {
      sessionId: params.sessionId,
      update: { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `Loaded ${params.cwd}` } },
    });
    return {};
  })
  .onRequest(acp.methods.agent.session.fork, () => { forks += 1; return { sessionId: "forked" }; })
  .onRequest(acp.methods.agent.session.resume, () => { resumes += 1; return {}; })
  .onRequest(acp.methods.agent.logout, () => ({}));

await agent.connect(acp.ndJsonStream(
  Writable.toWeb(process.stdout) as WritableStream<Uint8Array>,
  Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>,
)).closed;
