import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";

const sessionId = "lazy-process-session";
const history: acp.SessionUpdate[] = [
  {
    sessionUpdate: "user_message_chunk",
    messageId: "history-prompt",
    content: { type: "text", text: "Inspect the recorded execution process." },
  },
  ...Array.from({ length: 25 }, (_, index): acp.SessionUpdate => ({
    sessionUpdate: "tool_call",
    toolCallId: `history-tool-${index + 1}`,
    title: `Inspect process item ${String(index + 1).padStart(2, "0")}`,
    kind: "read",
    status: "completed",
    content: [{ type: "content", content: {
      type: "text",
      text: `HIDDEN_PROCESS_PAYLOAD_${index + 1}: ${"Detailed execution output. ".repeat(150)}`,
    } }],
  })),
  {
    sessionUpdate: "agent_message_chunk",
    messageId: "history-final",
    content: { type: "text", text: "The final answer is available without loading execution details." },
  },
];
const prompts = new Map<string, () => void>();

const agent = acp.agent({ name: "lazy-process-fixture" })
  .onRequest(acp.methods.agent.initialize, () => ({
    protocolVersion: acp.PROTOCOL_VERSION,
    agentCapabilities: { loadSession: true, sessionCapabilities: { list: {}, close: {} } },
    agentInfo: { name: "lazy-process-fixture", version: "1.0.0" },
    authMethods: [],
  }))
  .onRequest(acp.methods.agent.session.list, () => ({
    sessions: [{ sessionId, cwd: process.cwd(), title: "Lazy process fixture" }],
  }))
  .onRequest(acp.methods.agent.session.new, () => ({ sessionId }))
  .onRequest(acp.methods.agent.session.load, async ({ params, client }) => {
    if (params.sessionId !== sessionId) throw new acp.RequestError(-32002, "Session not found");
    for (const update of history) {
      await client.notify(acp.methods.client.session.update, { sessionId, update });
    }
    return {};
  })
  .onRequest(acp.methods.agent.session.prompt, async ({ params, client }) => {
    const finished = new Promise<void>((resolve) => prompts.set(params.sessionId, resolve));
    for (const content of params.prompt) {
      const update: acp.SessionUpdate = { sessionUpdate: "user_message_chunk", messageId: "live-prompt", content };
      history.push(update);
      await client.notify(acp.methods.client.session.update, { sessionId, update });
    }
    const plan: acp.SessionUpdate = {
      sessionUpdate: "plan",
      entries: [
        { content: "Inspect the current implementation", priority: "high", status: "completed" },
        { content: "Update transport and task presentation", priority: "high", status: "in_progress" },
        { content: "Verify pagination boundaries", priority: "medium", status: "pending" },
        { content: "Check mobile layout", priority: "medium", status: "pending" },
        { content: "Review the final changes", priority: "low", status: "pending" },
      ],
    };
    history.push(plan);
    await client.notify(acp.methods.client.session.update, { sessionId, update: plan });
    await finished;
    prompts.delete(params.sessionId);
    return { stopReason: "cancelled" };
  })
  .onNotification(acp.methods.agent.session.cancel, ({ params }) => {
    prompts.get(params.sessionId)?.();
  })
  .onRequest(acp.methods.agent.session.close, ({ params }) => {
    prompts.get(params.sessionId)?.();
    return {};
  });

await agent.connect(acp.ndJsonStream(
  Writable.toWeb(process.stdout) as WritableStream<Uint8Array>,
  Readable.toWeb(process.stdin) as ReadableStream<Uint8Array>,
)).closed;
