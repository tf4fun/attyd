import { createInterface } from "node:readline";

type JsonRpcId = string | number | null;

interface PendingRoundTrip {
  requestId: JsonRpcId;
}

const pendingRoundTrips = new Map<JsonRpcId, PendingRoundTrip>();
let nextServerRequest = 0;
let cancellationCount = 0;
let pendingCount = 0;

const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let value: unknown;
  try {
    value = JSON.parse(line);
  } catch {
    return;
  }
  if (!isRecord(value) || value.jsonrpc !== "2.0") return;

  if (typeof value.method !== "string") {
    const roundTrip = pendingRoundTrips.get(value.id as JsonRpcId);
    if (!roundTrip) return;
    pendingRoundTrips.delete(value.id as JsonRpcId);
    write({
      jsonrpc: "2.0",
      id: roundTrip.requestId,
      result: {
        clientResult: value.result,
        clientError: value.error,
      },
    });
    return;
  }

  if (!("id" in value)) {
    if (value.method === "notifications/cancelled") cancellationCount += 1;
    return;
  }
  switch (value.method) {
    case "initialize":
      write({
        jsonrpc: "2.0",
        id: value.id,
        result: {
          protocolVersion: "2025-06-18",
          capabilities: { tools: {} },
          serverInfo: { name: "fake-mcp", version: "1.0.0" },
        },
      });
      break;
    case "echo":
      write({ jsonrpc: "2.0", id: value.id, result: value.params ?? null });
      break;
    case "cancellationCount":
      write({ jsonrpc: "2.0", id: value.id, result: { count: cancellationCount } });
      break;
    case "delayed": {
      const delay = isRecord(value.params) && typeof value.params.delay === "number"
        ? value.params.delay
        : 0;
      setTimeout(() => write({
        jsonrpc: "2.0",
        id: value.id,
        result: isRecord(value.params) ? value.params.value : null,
      }), delay);
      break;
    }
    case "fail":
      write({
        jsonrpc: "2.0",
        id: value.id,
        error: { code: -32042, message: "deliberate MCP failure", data: { fixture: true } },
      });
      break;
    case "serverRoundTrip": {
      const serverRequestId = `server-${++nextServerRequest}`;
      pendingRoundTrips.set(serverRequestId, { requestId: value.id as JsonRpcId });
      write({
        jsonrpc: "2.0",
        method: "notifications/progress",
        params: { progressToken: "fixture", progress: 0.5 },
      });
      write({
        jsonrpc: "2.0",
        id: serverRequestId,
        method: "roots/list",
        params: { requestedBy: "fake-mcp" },
      });
      break;
    }
    case "never":
      pendingCount += 1;
      write({
        jsonrpc: "2.0",
        method: "notifications/progress",
        params: { progressToken: "pending", progress: pendingCount },
      });
      break;
    case "exit":
      setTimeout(() => process.exit(23), 0);
      break;
    case "resultAndError":
      write({
        jsonrpc: "2.0",
        id: value.id,
        result: { invalid: true },
        error: { code: -32099, message: "must not win" },
      });
      break;
    case "duplicateResponse":
      write({ jsonrpc: "2.0", id: value.id, result: "first" });
      write({ jsonrpc: "2.0", id: value.id, result: "second" });
      break;
    default:
      write({
        jsonrpc: "2.0",
        id: value.id,
        error: { code: -32601, message: `Unknown fixture method: ${value.method}` },
      });
  }
});

function write(value: Record<string, unknown>): void {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
