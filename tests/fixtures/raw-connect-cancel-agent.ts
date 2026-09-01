import { createInterface } from "node:readline";

type JsonRpcId = string | number | null;

interface JsonRpcMessage {
  jsonrpc: "2.0";
  id?: JsonRpcId;
  method?: string;
  params?: Record<string, unknown>;
  result?: unknown;
  error?: { code?: number; message?: string; data?: unknown };
}

let serverId = "";
let sessionId = "raw-connect-cancel-session";
let promptRequestId: JsonRpcId | undefined;
let recoveredConnectionId = "";
let connectCancelled = false;
let recoveredEcho = false;

const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let message: JsonRpcMessage;
  try {
    message = JSON.parse(line) as JsonRpcMessage;
  } catch {
    return;
  }
  if (message.jsonrpc !== "2.0") return;
  if (typeof message.method === "string" && message.id !== undefined) {
    handleRequest(message);
    return;
  }
  if (message.id !== undefined) handleResponse(message);
});

function handleRequest(message: JsonRpcMessage): void {
  switch (message.method) {
    case "initialize":
      respond(message.id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: false,
          mcpCapabilities: { acp: true },
        },
        agentInfo: { name: "raw-connect-cancel-agent", version: "1.0.0" },
        authMethods: [],
      });
      break;
    case "session/new": {
      const servers = Array.isArray(message.params?.mcpServers)
        ? message.params.mcpServers
        : [];
      const server = servers.find((candidate) =>
        isRecord(candidate) && candidate.type === "acp" && typeof candidate.serverId === "string"
      );
      if (!isRecord(server) || typeof server.serverId !== "string") {
        respondError(message.id, -32_602, "No ACP MCP server was configured");
        return;
      }
      serverId = server.serverId;
      respond(message.id, { sessionId });
      break;
    }
    case "session/prompt":
      promptRequestId = message.id;
      writeTogether(
        request("connect-cancel", "mcp/connect", { serverId }),
        notification("$/cancel_request", { requestId: "connect-cancel" }),
      );
      break;
    default:
      respondError(message.id, -32_601, `Unsupported raw fixture method: ${message.method}`);
  }
}

function handleResponse(message: JsonRpcMessage): void {
  switch (message.id) {
    case "connect-cancel":
      connectCancelled = message.error?.code === -32_800;
      if (isRecord(message.result) && typeof message.result.connectionId === "string") {
        recoveredConnectionId = message.result.connectionId;
        write(request("cleanup-unexpected", "mcp/disconnect", {
          connectionId: recoveredConnectionId,
        }));
      } else {
        startRecovery();
      }
      break;
    case "cleanup-unexpected":
      startRecovery();
      break;
    case "recovery-connect":
      if (!isRecord(message.result) || typeof message.result.connectionId !== "string") {
        finish(false);
        return;
      }
      recoveredConnectionId = message.result.connectionId;
      write(request("recovery-echo", "mcp/message", {
        connectionId: recoveredConnectionId,
        method: "echo",
        params: { recoveredAfterConnectCancellation: true },
      }));
      break;
    case "recovery-echo":
      recoveredEcho = isRecord(message.result) &&
        message.result.recoveredAfterConnectCancellation === true;
      write(request("recovery-disconnect", "mcp/disconnect", {
        connectionId: recoveredConnectionId,
      }));
      break;
    case "recovery-disconnect":
      finish(recoveredEcho);
      break;
  }
}

function startRecovery(): void {
  write(request("recovery-connect", "mcp/connect", { serverId }));
}

function finish(recovered: boolean): void {
  write(notification("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      messageId: "raw-connect-cancel-result",
      content: {
        type: "text",
        text: JSON.stringify({ connectCancelled, recovered }),
      },
    },
  }));
  respond(promptRequestId, { stopReason: "end_turn" });
}

function request(id: JsonRpcId, method: string, params: Record<string, unknown>): JsonRpcMessage {
  return { jsonrpc: "2.0", id, method, params };
}

function notification(method: string, params: Record<string, unknown>): JsonRpcMessage {
  return { jsonrpc: "2.0", method, params };
}

function respond(id: JsonRpcId | undefined, result: unknown): void {
  if (id === undefined) return;
  write({ jsonrpc: "2.0", id, result });
}

function respondError(id: JsonRpcId | undefined, code: number, message: string): void {
  if (id === undefined) return;
  write({ jsonrpc: "2.0", id, error: { code, message } });
}

function write(message: JsonRpcMessage): void {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function writeTogether(...messages: JsonRpcMessage[]): void {
  process.stdout.write(`${messages.map((message) => JSON.stringify(message)).join("\n")}\n`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
