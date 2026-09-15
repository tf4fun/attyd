import { createInterface } from "node:readline";

const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
for await (const line of createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  const response = (result) => ({ jsonrpc: "2.0", id: request.id, result });
  if (request.method === "initialize") {
    write(response({ protocolVersion: 1, agentCapabilities: { loadSession: true }, authMethods: [] }));
  } else if (request.method === "session/load") {
    write(response({}));
  } else if (request.method === "session/prompt") {
    const large = request.params.prompt?.[0]?.text === "answer-large";
    const messages = Array.from({ length: large ? 1 : 2_048 }, (_, index) => ({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: request.params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: "burst-answer",
          content: { type: "text", text: large ? "完整🙂".repeat(900_000) : `片段${index}🙂\n` },
        },
      },
    }));
    // One transport burst, with the terminal response immediately after its last
    // fragment. No sleeps or per-fragment acknowledgement from the client.
    process.stdout.write([...messages, response({ stopReason: "end_turn" })]
      .map((message) => `${JSON.stringify(message)}\n`).join(""));
  } else if (request.id !== undefined) {
    write({ jsonrpc: "2.0", id: request.id, error: { code: -32601, message: "Method not found" } });
  }
}
