import { readFileSync } from "node:fs";
import { isAbsolute, resolve } from "node:path";
import type { McpServer } from "@agentclientprotocol/sdk";
import type { AgentTransport } from "../shared/bridge.js";

export interface ServerOptions {
  host: string;
  port: number;
  cwd: string;
  readOnly: boolean;
  dev: boolean;
  additionalDirectories: string[];
  mcpServers: McpServer[];
  acpMcpProviders: AcpMcpProvider[];
  transport: AgentTransport;
  /** Process command for stdio; a single endpoint URL for HTTP or WebSocket. */
  command: [string, ...string[]];
  /** Explicit Agent launch environment for embedders/tests; the CLI inherits process.env. */
  env?: NodeJS.ProcessEnv;
}

export interface AcpMcpProvider {
  name: string;
  serverId: string;
  command: string;
  args: string[];
  env: Array<{ name: string; value: string }>;
}

interface McpConfigBundle {
  servers: McpServer[];
  acpProviders: AcpMcpProvider[];
}

export function parseOptions(argv: string[]): ServerOptions {
  let host = "127.0.0.1";
  let port = 7331;
  let cwd = process.cwd();
  let readOnly = false;
  let dev = false;
  let transport: AgentTransport = "stdio";
  const additionalDirectories: string[] = [];
  const mcpServers: McpServer[] = [];
  const acpMcpProviders: AcpMcpProvider[] = [];
  let command: string[] = [];

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];

    if (arg === "--") {
      command = argv.slice(index + 1);
      break;
    }
    if (!arg.startsWith("-")) {
      command = argv.slice(index);
      break;
    }
    if (arg === "--host" || arg === "-H") {
      host = requiredValue(argv, ++index, arg);
    } else if (arg === "--port" || arg === "-p") {
      port = Number.parseInt(requiredValue(argv, ++index, arg), 10);
      if (!Number.isInteger(port) || port < 1 || port > 65_535) {
        throw new Error(`Invalid port: ${String(port)}`);
      }
    } else if (arg === "--cwd" || arg === "-c") {
      cwd = resolve(requiredValue(argv, ++index, arg));
    } else if (arg === "--transport" || arg === "-t") {
      transport = parseTransport(requiredValue(argv, ++index, arg));
    } else if (arg === "--read-only") {
      readOnly = true;
    } else if (arg === "--add-dir") {
      additionalDirectories.push(resolve(requiredValue(argv, ++index, arg)));
    } else if (arg === "--mcp-config") {
      const configPath = resolve(requiredValue(argv, ++index, arg));
      const bundle = parseMcpConfigBundle(readFileSync(configPath, "utf8"), configPath);
      mcpServers.push(...bundle.servers);
      acpMcpProviders.push(...bundle.acpProviders);
    } else if (arg === "--dev") {
      dev = true;
    } else if (arg === "--help" || arg === "-h") {
      printHelp();
      process.exit(0);
    } else {
      throw new Error(`Unknown option: ${arg}`);
    }
  }

  if (command.length === 0 && transport === "stdio") {
    command = [resolve("bin/goose"), "acp"];
  }
  if (transport !== "stdio") validateRemoteTarget(transport, command);
  if (transport !== "stdio" && additionalDirectories.length > 0) {
    throw new Error("--add-dir is only available with the stdio transport");
  }
  if (mcpServers.length > 32) throw new Error("MCP configs contain more than 32 MCP servers");
  assertUniqueMcpNames(mcpServers, "MCP configs");
  assertUniqueAcpServerIds(acpMcpProviders, "MCP configs");

  return {
    host,
    port,
    cwd,
    readOnly,
    dev,
    additionalDirectories: [...new Set(additionalDirectories)],
    mcpServers,
    acpMcpProviders,
    transport,
    command: command as [string, ...string[]],
  };
}

export function parseMcpConfig(source: string, label = "MCP config"): McpServer[] {
  return parseMcpConfigBundle(source, label).servers;
}

export function parseMcpConfigBundle(
  source: string,
  label = "MCP config",
): McpConfigBundle {
  let value: unknown;
  try {
    value = JSON.parse(source);
  } catch (error) {
    throw new Error(`${label} is not valid JSON: ${errorMessage(error)}`);
  }
  const entries = Array.isArray(value)
    ? value
    : isRecord(value) && Array.isArray(value.mcpServers)
      ? value.mcpServers
      : undefined;
  if (!entries) throw new Error(`${label} must be an array or contain mcpServers`);
  if (entries.length > 32) throw new Error(`${label} contains more than 32 MCP servers`);

  const parsed = entries.map((entry, index) => parseMcpServer(entry, `${label}[${index}]`));
  const servers = parsed.map(({ server }) => server);
  const acpProviders = parsed.flatMap(({ provider }) => provider ? [provider] : []);
  assertUniqueMcpNames(servers, label);
  assertUniqueAcpServerIds(acpProviders, label);
  return { servers, acpProviders };
}

function parseMcpServer(
  value: unknown,
  label: string,
): { server: McpServer; provider?: AcpMcpProvider } {
  if (!isRecord(value)) throw new Error(`${label} must be an object`);
  const name = requiredString(value, "name", label);
  const type = value.type ?? "stdio";
  if (type === "stdio") {
    const command = requiredString(value, "command", label);
    if (!isAbsolute(command)) throw new Error(`${label}.command must be an absolute path`);
    return {
      server: {
        name,
        command,
        args: stringArray(value.args, `${label}.args`),
        env: keyValueArray(value.env, `${label}.env`),
      },
    };
  }
  if (type === "http" || type === "sse") {
    const url = requiredString(value, "url", label);
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      throw new Error(`${label}.url must be an absolute HTTP(S) URL`);
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new Error(`${label}.url must be an absolute HTTP(S) URL`);
    }
    return {
      server: {
        type,
        name,
        url: parsed.href,
        headers: keyValueArray(value.headers, `${label}.headers`),
      },
    };
  }
  if (type === "acp") {
    const serverId = requiredString(value, "serverId", label);
    const command = requiredString(value, "command", label);
    if (!isAbsolute(command)) throw new Error(`${label}.command must be an absolute path`);
    const provider = {
      name,
      serverId,
      command,
      args: stringArray(value.args, `${label}.args`),
      env: keyValueArray(value.env, `${label}.env`),
    };
    return {
      server: { type: "acp", name, serverId },
      provider,
    };
  }
  throw new Error(`${label}.type is unsupported: ${String(type)}`);
}

function assertUniqueAcpServerIds(providers: AcpMcpProvider[], label: string): void {
  const ids = new Set<string>();
  for (const provider of providers) {
    if (ids.has(provider.serverId)) {
      throw new Error(`${label} contains duplicate ACP MCP serverId: ${provider.serverId}`);
    }
    ids.add(provider.serverId);
  }
}

function assertUniqueMcpNames(servers: McpServer[], label: string): void {
  const names = new Set<string>();
  for (const server of servers) {
    if (names.has(server.name)) {
      throw new Error(`${label} contains duplicate MCP server name: ${server.name}`);
    }
    names.add(server.name);
  }
}

function stringArray(value: unknown, label: string): string[] {
  if (value == null) return [];
  if (!Array.isArray(value) || !value.every((item) => typeof item === "string")) {
    throw new Error(`${label} must be an array of strings`);
  }
  return value;
}

function keyValueArray(
  value: unknown,
  label: string,
): Array<{ name: string; value: string }> {
  if (value == null) return [];
  if (!Array.isArray(value)) throw new Error(`${label} must be an array`);
  return value.map((item, index) => {
    if (!isRecord(item)) throw new Error(`${label}[${index}] must be an object`);
    return {
      name: requiredString(item, "name", `${label}[${index}]`),
      value: requiredString(item, "value", `${label}[${index}]`),
    };
  });
}

function requiredString(
  value: Record<string, unknown>,
  key: string,
  label: string,
): string {
  const candidate = value[key];
  if (typeof candidate !== "string" || candidate.length === 0) {
    throw new Error(`${label}.${key} must be a non-empty string`);
  }
  return candidate;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function requiredValue(argv: string[], index: number, option: string): string {
  const value = argv[index];
  if (!value) throw new Error(`${option} requires a value`);
  return value;
}

function parseTransport(value: string): AgentTransport {
  if (value === "stdio" || value === "ws") return value;
  if (value === "http" || value === "sse" || value === "streamable-http") return "http";
  throw new Error(
    `Invalid transport: ${value} (expected stdio, http, or ws)`,
  );
}

function validateRemoteTarget(transport: Exclude<AgentTransport, "stdio">, target: string[]): void {
  if (target.length === 0) {
    throw new Error(`--transport ${transport} requires an ACP endpoint URL`);
  }
  if (target.length !== 1) {
    throw new Error(`--transport ${transport} accepts exactly one ACP endpoint URL`);
  }
  let endpoint: URL;
  try {
    endpoint = new URL(target[0]);
  } catch {
    throw new Error(`Invalid ${transport} ACP endpoint URL: ${target[0]}`);
  }
  const allowed = transport === "http" ? ["http:", "https:"] : ["ws:", "wss:"];
  if (!allowed.includes(endpoint.protocol)) {
    throw new Error(
      `Invalid ${transport} ACP endpoint URL protocol: ${endpoint.protocol || "unknown"}`,
    );
  }
  target[0] = endpoint.href;
}

export function printHelp(): void {
  console.log(`attyd — expose an ACP agent as a web workspace

Usage:
  attyd [options] -- <agent-command> [args...]
  attyd -t http [options] -- <http(s)://acp-endpoint>
  attyd -t ws [options] -- <ws(s)://acp-endpoint>

Options:
  -H, --host <host>   Bind address (default: 127.0.0.1)
  -p, --port <port>   HTTP port (default: 7331)
  -c, --cwd <path>    stdio workspace default and local filesystem boundary
  -t, --transport <transport>
                       Agent transport: stdio, http (Streamable HTTP/SSE), or ws
                       (default: stdio)
      --add-dir <path> Additional stdio workspace root (repeatable)
      --mcp-config <file>
                       Static JSON array of MCP definitions (repeatable; ACP transport
                       entries keep their command and environment private in attyd)
      --read-only     Do not advertise or permit fs/write_text_file
  -h, --help          Show this help

With stdio, no command runs ./bin/goose acp. Remote transports require one URL.
New remote sessions choose an absolute Agent-host path in the web UI.`);
}
