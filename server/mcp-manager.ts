import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { isAbsolute } from "node:path";
import * as acp from "@agentclientprotocol/sdk";
import type { AcpMcpProvider } from "./options.js";

type JsonRpcId = string | number | null;

interface JsonRpcError {
  code: number;
  message: string;
  data?: unknown;
}

interface PendingRequest {
  method: string;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
  removeAbort?: () => void;
}

interface McpConnection {
  connectionId: string;
  provider: AcpMcpProvider;
  child: ChildProcessWithoutNullStreams;
  input: string;
  nextRequestId: number;
  pending: Map<JsonRpcId, PendingRequest>;
  closed: boolean;
  announced: boolean;
}

export interface McpActivity {
  direction: "agent-to-server" | "server-to-agent";
  connectionId: string;
  method: string;
  kind: "request" | "notification" | "response";
  params?: Record<string, unknown> | null;
  result?: unknown;
  error?: JsonRpcError;
}

export interface McpManagerOptions {
  cwd: string;
  providers: AcpMcpProvider[];
  onServerRequest: (request: acp.MessageMcpRequest) => Promise<unknown>;
  onServerNotification: (notification: acp.MessageMcpNotification) => Promise<void> | void;
  onConnection?: (
    action: "connected" | "disconnected",
    value: { serverId: string; connectionId: string; name: string },
  ) => void;
  onActivity?: (activity: McpActivity) => void;
  onStderr?: (chunk: string) => void;
}

const MAX_CONNECTIONS = 16;
const MAX_PENDING_REQUESTS = 128;
const MAX_MESSAGE_BYTES = 3_000_000;
const MAX_INPUT_BYTES = 5_000_000;

export class McpManager {
  private readonly providers = new Map<string, AcpMcpProvider>();
  private readonly connections = new Map<string, McpConnection>();
  private closed = false;

  constructor(private readonly options: McpManagerOptions) {
    for (const provider of options.providers) {
      if (this.providers.has(provider.serverId)) {
        throw new Error(`Duplicate ACP MCP serverId: ${provider.serverId}`);
      }
      if (!isAbsolute(provider.command)) {
        throw new Error(`ACP MCP provider ${provider.name} command must be an absolute path`);
      }
      this.providers.set(provider.serverId, provider);
    }
  }

  async connect(
    request: acp.ConnectMcpRequest,
    signal?: AbortSignal,
  ): Promise<acp.ConnectMcpResponse> {
    if (this.closed) throw new acp.RequestError(-32000, "MCP manager is closed");
    if (signal?.aborted) throw acp.RequestError.requestCancelled();
    const provider = this.providers.get(request.serverId);
    if (!provider) {
      throw new acp.RequestError(-32002, `Unknown ACP MCP server: ${request.serverId}`);
    }
    if (this.connections.size >= MAX_CONNECTIONS) {
      throw new acp.RequestError(-32000, `At most ${MAX_CONNECTIONS} MCP connections are allowed`);
    }

    const child = spawn(provider.command, provider.args, {
      cwd: this.options.cwd,
      env: {
        ...process.env,
        ...Object.fromEntries(provider.env.map(({ name, value }) => [name, value])),
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const connection: McpConnection = {
      connectionId: randomUUID(),
      provider,
      child,
      input: "",
      nextRequestId: 0,
      pending: new Map(),
      closed: false,
      announced: false,
    };
    this.connections.set(connection.connectionId, connection);
    this.observeProcess(connection);

    await new Promise<void>((resolve, reject) => {
      const cleanup = () => {
        child.off("spawn", onSpawn);
        child.off("error", onError);
        signal?.removeEventListener("abort", onAbort);
      };
      const onSpawn = () => {
        cleanup();
        resolve();
      };
      const onError = (error: Error) => {
        cleanup();
        this.terminate(connection, error);
        reject(new acp.RequestError(-32000, `Could not start MCP server ${provider.name}`, {
          message: error.message,
        }));
      };
      const onAbort = () => {
        cleanup();
        const error = acp.RequestError.requestCancelled();
        this.terminate(connection, error);
        reject(error);
      };
      child.once("spawn", onSpawn);
      child.once("error", onError);
      signal?.addEventListener("abort", onAbort, { once: true });
      if (signal?.aborted) onAbort();
    });

    if (signal?.aborted) {
      const error = acp.RequestError.requestCancelled();
      this.terminate(connection, error);
      throw error;
    }
    if (connection.closed) {
      throw new acp.RequestError(-32000, `MCP server ${provider.name} exited during startup`);
    }
    connection.announced = true;
    this.options.onConnection?.("connected", {
      serverId: provider.serverId,
      connectionId: connection.connectionId,
      name: provider.name,
    });
    return { connectionId: connection.connectionId };
  }

  async message(request: acp.MessageMcpRequest, signal?: AbortSignal): Promise<unknown> {
    const connection = this.requireConnection(request.connectionId);
    this.assertMessage(request.method, request.params);
    if (connection.pending.size >= MAX_PENDING_REQUESTS) {
      throw new acp.RequestError(
        -32000,
        `At most ${MAX_PENDING_REQUESTS} MCP requests may be pending on one connection`,
      );
    }
    if (signal?.aborted) throw acp.RequestError.requestCancelled();

    const id = `attyd-${++connection.nextRequestId}`;
    this.options.onActivity?.({
      direction: "agent-to-server",
      connectionId: connection.connectionId,
      method: request.method,
      kind: "request",
      params: request.params,
    });

    return new Promise<unknown>((resolve, reject) => {
      const pending: PendingRequest = { method: request.method, resolve, reject };
      if (signal) {
        const onAbort = () => {
          if (!connection.pending.delete(id)) return;
          void this.write(connection, {
            jsonrpc: "2.0",
            method: "notifications/cancelled",
            params: { requestId: id, reason: "ACP request cancelled" },
          }).catch(() => undefined);
          this.options.onActivity?.({
            direction: "agent-to-server",
            connectionId: connection.connectionId,
            method: "notifications/cancelled",
            kind: "notification",
            params: { requestId: id, reason: "ACP request cancelled" },
          });
          reject(acp.RequestError.requestCancelled());
        };
        signal.addEventListener("abort", onAbort, { once: true });
        pending.removeAbort = () => signal.removeEventListener("abort", onAbort);
      }
      connection.pending.set(id, pending);
      void this.write(connection, {
        jsonrpc: "2.0",
        id,
        method: request.method,
        ...(request.params == null ? {} : { params: request.params }),
      }).catch((error: unknown) => {
        const active = connection.pending.get(id);
        if (!active) return;
        connection.pending.delete(id);
        active.removeAbort?.();
        reject(asError(error));
      });
    });
  }

  async notify(notification: acp.MessageMcpNotification): Promise<void> {
    const connection = this.requireConnection(notification.connectionId);
    this.assertMessage(notification.method, notification.params);
    this.options.onActivity?.({
      direction: "agent-to-server",
      connectionId: connection.connectionId,
      method: notification.method,
      kind: "notification",
      params: notification.params,
    });
    await this.write(connection, {
      jsonrpc: "2.0",
      method: notification.method,
      ...(notification.params == null ? {} : { params: notification.params }),
    });
  }

  disconnect(request: acp.DisconnectMcpRequest): acp.DisconnectMcpResponse {
    const connection = this.requireConnection(request.connectionId);
    this.terminate(connection, new acp.RequestError(-32000, "MCP connection was disconnected"));
    return {};
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    for (const connection of [...this.connections.values()]) {
      this.terminate(connection, new acp.RequestError(-32000, "ACP connection was closed"));
    }
  }

  private observeProcess(connection: McpConnection): void {
    connection.child.stdout.setEncoding("utf8");
    connection.child.stdout.on("data", (chunk: string) => this.receive(connection, chunk));
    connection.child.stderr.setEncoding("utf8");
    connection.child.stderr.on("data", (chunk: string) => {
      this.options.onStderr?.(`[MCP ${connection.provider.name}] ${chunk}`);
    });
    connection.child.once("error", (error) => {
      this.terminate(connection, new acp.RequestError(-32000, error.message));
    });
    connection.child.once("exit", (code, signal) => {
      this.terminate(
        connection,
        new acp.RequestError(
          -32000,
          `MCP server ${connection.provider.name} exited (${code ?? signal ?? "unknown"})`,
        ),
      );
    });
  }

  private receive(connection: McpConnection, chunk: string): void {
    if (connection.closed) return;
    connection.input += chunk;
    while (true) {
      const newline = connection.input.indexOf("\n");
      if (newline < 0) {
        if (Buffer.byteLength(connection.input, "utf8") > MAX_INPUT_BYTES) {
          this.terminate(
            connection,
            new acp.RequestError(-32000, "MCP server output exceeded the input buffer limit"),
          );
        }
        return;
      }
      const line = connection.input.slice(0, newline).replace(/\r$/, "");
      connection.input = connection.input.slice(newline + 1);
      if (line.trim().length === 0) continue;
      if (Buffer.byteLength(line, "utf8") > MAX_MESSAGE_BYTES) {
        this.terminate(
          connection,
          new acp.RequestError(-32000, "MCP server emitted an oversized JSON-RPC message"),
        );
        return;
      }
      let message: unknown;
      try {
        message = JSON.parse(line);
      } catch {
        this.options.onStderr?.(`[MCP ${connection.provider.name}] Invalid JSON-RPC message ignored\n`);
        continue;
      }
      this.route(connection, message);
    }
  }

  private route(connection: McpConnection, value: unknown): void {
    if (!isRecord(value) || value.jsonrpc !== "2.0") {
      this.options.onStderr?.(`[MCP ${connection.provider.name}] Invalid JSON-RPC object ignored\n`);
      return;
    }
    if (typeof value.method === "string") {
      const params = parseInnerParams(value.params);
      if (params === undefined && value.params !== undefined) {
        if (isJsonRpcId(value.id)) {
          void this.respondError(connection, value.id, -32602, "MCP params must be an object or null");
        }
        return;
      }
      if (isJsonRpcId(value.id)) {
        void this.routeServerRequest(connection, value.id, value.method, params).catch(
          (error: unknown) => {
            this.options.onStderr?.(
              `[MCP ${connection.provider.name}] Request forwarding failed: ${asError(error).message}\n`,
            );
          },
        );
      } else if (!("id" in value)) {
        try {
          this.assertMessage(value.method, params);
        } catch (error) {
          this.options.onStderr?.(
            `[MCP ${connection.provider.name}] Invalid notification ignored: ${asError(error).message}\n`,
          );
          return;
        }
        this.options.onActivity?.({
          direction: "server-to-agent",
          connectionId: connection.connectionId,
          method: value.method,
          kind: "notification",
          params,
        });
        void Promise.resolve(this.options.onServerNotification({
          connectionId: connection.connectionId,
          method: value.method,
          params,
        })).catch((error: unknown) => {
          this.options.onStderr?.(
            `[MCP ${connection.provider.name}] Notification forwarding failed: ${asError(error).message}\n`,
          );
        });
      }
      return;
    }
    if (!isJsonRpcId(value.id)) return;
    const pending = connection.pending.get(value.id);
    if (!pending) {
      this.options.onStderr?.(`[MCP ${connection.provider.name}] Unknown response id ignored\n`);
      return;
    }
    connection.pending.delete(value.id);
    pending.removeAbort?.();
    if (isRecord(value.error) && "result" in value) {
      const error = { code: -32603, message: "MCP response contains both result and error" };
      this.options.onActivity?.({
        direction: "server-to-agent",
        connectionId: connection.connectionId,
        method: pending.method,
        kind: "response",
        error,
      });
      pending.reject(new acp.RequestError(error.code, error.message));
    } else if (isRecord(value.error)) {
      const error = parseJsonRpcError(value.error);
      this.options.onActivity?.({
        direction: "server-to-agent",
        connectionId: connection.connectionId,
        method: pending.method,
        kind: "response",
        error,
      });
      pending.reject(new acp.RequestError(error.code, error.message, error.data));
    } else if ("result" in value) {
      this.options.onActivity?.({
        direction: "server-to-agent",
        connectionId: connection.connectionId,
        method: pending.method,
        kind: "response",
        result: value.result,
      });
      pending.resolve(value.result);
    } else {
      const error = { code: -32603, message: "MCP response has neither result nor error" };
      this.options.onActivity?.({
        direction: "server-to-agent",
        connectionId: connection.connectionId,
        method: pending.method,
        kind: "response",
        error,
      });
      pending.reject(new acp.RequestError(error.code, error.message));
    }
  }

  private async routeServerRequest(
    connection: McpConnection,
    id: JsonRpcId,
    method: string,
    params: Record<string, unknown> | null | undefined,
  ): Promise<void> {
    try {
      this.assertMessage(method, params);
      this.options.onActivity?.({
        direction: "server-to-agent",
        connectionId: connection.connectionId,
        method,
        kind: "request",
        params,
      });
      const result = await this.options.onServerRequest({
        connectionId: connection.connectionId,
        method,
        params,
      });
      await this.write(connection, { jsonrpc: "2.0", id, result: result ?? null });
      this.options.onActivity?.({
        direction: "agent-to-server",
        connectionId: connection.connectionId,
        method,
        kind: "response",
        result: result ?? null,
      });
    } catch (error) {
      const rpcError = error instanceof acp.RequestError
        ? error
        : new acp.RequestError(-32603, asError(error).message);
      await this.respondError(connection, id, rpcError.code, rpcError.message, rpcError.data);
      this.options.onActivity?.({
        direction: "agent-to-server",
        connectionId: connection.connectionId,
        method,
        kind: "response",
        error: {
          code: rpcError.code,
          message: rpcError.message,
          ...(rpcError.data === undefined ? {} : { data: rpcError.data }),
        },
      });
    }
  }

  private respondError(
    connection: McpConnection,
    id: JsonRpcId,
    code: number,
    message: string,
    data?: unknown,
  ): Promise<void> {
    return this.write(connection, {
      jsonrpc: "2.0",
      id,
      error: { code, message, ...(data === undefined ? {} : { data }) },
    });
  }

  private async write(connection: McpConnection, message: Record<string, unknown>): Promise<void> {
    if (connection.closed || !this.connections.has(connection.connectionId)) {
      throw new acp.RequestError(-32002, `Unknown MCP connection: ${connection.connectionId}`);
    }
    let serialized: string;
    try {
      serialized = `${JSON.stringify(message)}\n`;
    } catch (error) {
      throw new acp.RequestError(-32603, "MCP message is not JSON serializable", {
        message: asError(error).message,
      });
    }
    if (Buffer.byteLength(serialized, "utf8") > MAX_MESSAGE_BYTES) {
      throw new acp.RequestError(-32602, "MCP message exceeds the size limit");
    }
    await new Promise<void>((resolve, reject) => {
      connection.child.stdin.write(serialized, (error) => error ? reject(error) : resolve());
    });
  }

  private assertMessage(method: string, params: unknown): void {
    if (method.length === 0 || method.length > 1_024) {
      throw new acp.RequestError(-32602, "MCP method must contain between 1 and 1024 characters");
    }
    if (params !== undefined && params !== null && !isRecord(params)) {
      throw new acp.RequestError(-32602, "MCP params must be an object or null");
    }
    try {
      const size = Buffer.byteLength(JSON.stringify(params ?? null), "utf8");
      if (size > MAX_MESSAGE_BYTES) {
        throw new acp.RequestError(-32602, "MCP params exceed the size limit");
      }
    } catch (error) {
      if (error instanceof acp.RequestError) throw error;
      throw new acp.RequestError(-32602, "MCP params are not JSON serializable");
    }
  }

  private requireConnection(connectionId: string): McpConnection {
    const connection = this.connections.get(connectionId);
    if (!connection || connection.closed) {
      throw new acp.RequestError(-32002, `Unknown MCP connection: ${connectionId}`);
    }
    return connection;
  }

  private terminate(connection: McpConnection, reason: Error): void {
    if (connection.closed) return;
    connection.closed = true;
    this.connections.delete(connection.connectionId);
    for (const pending of connection.pending.values()) {
      pending.removeAbort?.();
      pending.reject(reason);
    }
    connection.pending.clear();
    if (connection.child.exitCode == null && connection.child.signalCode == null) {
      connection.child.kill();
      setTimeout(() => {
        if (connection.child.exitCode == null && connection.child.signalCode == null) {
          connection.child.kill("SIGKILL");
        }
      }, 1_000).unref();
    }
    if (connection.announced) {
      this.options.onConnection?.("disconnected", {
        serverId: connection.provider.serverId,
        connectionId: connection.connectionId,
        name: connection.provider.name,
      });
    }
  }
}

export function parseConnectMcpRequest(value: unknown): acp.ConnectMcpRequest {
  const record = requireRecord(value, "mcp/connect params");
  return { serverId: requireIdentifier(record.serverId, "mcp/connect serverId"), ...parseMeta(record) };
}

export function parseDisconnectMcpRequest(value: unknown): acp.DisconnectMcpRequest {
  const record = requireRecord(value, "mcp/disconnect params");
  return {
    connectionId: requireIdentifier(record.connectionId, "mcp/disconnect connectionId"),
    ...parseMeta(record),
  };
}

export function parseMessageMcp(value: unknown): acp.MessageMcpRequest {
  const record = requireRecord(value, "mcp/message params");
  const params = parseInnerParams(record.params);
  if (params === undefined && record.params !== undefined) {
    throw acp.RequestError.invalidParams(undefined, "mcp/message params must be an object or null");
  }
  return {
    connectionId: requireIdentifier(record.connectionId, "mcp/message connectionId"),
    method: requireIdentifier(record.method, "mcp/message method", 1_024),
    params,
    ...parseMeta(record),
  };
}

function parseMeta(value: Record<string, unknown>): Pick<acp.ConnectMcpRequest, "_meta"> {
  if (value._meta === undefined) return {};
  if (value._meta === null || isRecord(value._meta)) return { _meta: value._meta };
  throw acp.RequestError.invalidParams(undefined, "_meta must be an object or null");
}

function parseInnerParams(value: unknown): Record<string, unknown> | null | undefined {
  if (value === undefined || value === null) return value;
  return isRecord(value) ? value : undefined;
}

function requireRecord(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) throw acp.RequestError.invalidParams(undefined, `${label} must be an object`);
  return value;
}

function requireIdentifier(value: unknown, label: string, maximum = 256): string {
  if (typeof value !== "string" || value.length === 0 || value.length > maximum) {
    throw acp.RequestError.invalidParams(
      undefined,
      `${label} must contain between 1 and ${maximum} characters`,
    );
  }
  return value;
}

function parseJsonRpcError(value: Record<string, unknown>): JsonRpcError {
  return {
    code: Number.isSafeInteger(value.code) ? Number(value.code) : -32603,
    message: typeof value.message === "string" ? value.message : "MCP request failed",
    ...(value.data === undefined ? {} : { data: value.data }),
  };
}

function isJsonRpcId(value: unknown): value is JsonRpcId {
  return value === null || typeof value === "string" ||
    (typeof value === "number" && Number.isSafeInteger(value));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asError(value: unknown): Error {
  return value instanceof Error ? value : new Error(String(value));
}
