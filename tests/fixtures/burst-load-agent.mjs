import { createInterface } from "node:readline";

const messages = createInterface({ input: process.stdin });
const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
let finishLoad;
let listedDuringLoad = false;

for await (const line of messages) {
  const request = JSON.parse(line);
  const response = (result) => ({ jsonrpc: "2.0", id: request.id, result });
  if (request.method === "initialize") {
    write(response({
      protocolVersion: 1,
      agentCapabilities: { loadSession: true, sessionCapabilities: { list: {} } },
      authMethods: [],
    }));
  } else if (request.method === "session/load") {
    const updates = Array.from({ length: 10_050 }, (_, index) => ({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: request.params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId: `history-${index}`,
          content: { type: "text", text: `Historical message ${index}. ${"x".repeat(128)}` },
        },
      },
    }));
    // A saved conversation may replay hundreds of updates without waiting for
    // another client request. Keep the response after the entire replay.
    process.stdout.write(updates.slice(0, 360).map((message) => `${JSON.stringify(message)}\n`).join(""));
    finishLoad = () => process.stdout.write([...updates.slice(360), response({})].map((message) => `${JSON.stringify(message)}\n`).join(""));
  } else if (request.method === "session/list") {
    write(response({ sessions: finishLoad ? [{ sessionId: "burst-history", cwd: process.cwd(), title: "Replay paused" }] : [] }));
    if (finishLoad && listedDuringLoad) {
      finishLoad();
      finishLoad = undefined;
    } else if (finishLoad) {
      listedDuringLoad = true;
    }
  } else if (request.id !== undefined) {
    write({ jsonrpc: "2.0", id: request.id, error: { code: -32601, message: "Method not found" } });
  }
}
