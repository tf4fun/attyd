import { createInterface } from "node:readline";

const messages = createInterface({ input: process.stdin });
const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
const update = (sessionId, messageId, text) => ({
  jsonrpc: "2.0",
  method: "session/update",
  params: {
    sessionId,
    update: { sessionUpdate: "agent_message_chunk", messageId, content: { type: "text", text } },
  },
});

for await (const line of messages) {
  const request = JSON.parse(line);
  const response = (result) => ({ jsonrpc: "2.0", id: request.id, result });
  if (request.method === "initialize") {
    write(response({
      protocolVersion: 1,
      agentCapabilities: { sessionCapabilities: { fork: {} } },
      authMethods: [],
    }));
  } else if (request.method === "session/new") {
    write([
      response({ sessionId: "source" }),
      update("source", "source-before-fork", "Source context before the fork."),
    ]);
  } else if (request.method === "session/fork") {
    // The target message is after the response in one physical JSON-RPC batch.
    // It must append to the target baseline before its local continuation runs.
    write([
      response({ sessionId: "target" }),
      update("target", "target-after-response", "Target context after the fork response."),
    ]);
  } else if (request.id !== undefined) {
    write({ jsonrpc: "2.0", id: request.id, error: { code: -32601, message: "Method not found" } });
  }
}
