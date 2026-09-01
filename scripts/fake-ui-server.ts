import { join } from "node:path";
import { startHttpServer } from "../server/http-server.js";

const portValue = process.argv[2] ?? "7332";
const port = Number.parseInt(portValue, 10);
if (!Number.isInteger(port) || port < 1 || port > 65_535) {
  throw new Error(`Invalid port: ${portValue}`);
}

const cwd = process.cwd();
await startHttpServer({
  host: "127.0.0.1",
  port,
  cwd,
  readOnly: false,
  dev: false,
  additionalDirectories: [],
  mcpServers: [{
    type: "acp",
    name: "browser-fixture",
    serverId: "browser-fixture",
  }],
  acpMcpProviders: [{
    name: "browser-fixture",
    serverId: "browser-fixture",
    command: process.execPath,
    args: ["--import", "tsx", join(cwd, "tests/fixtures/fake-mcp-server.ts")],
    env: [{ name: "BROWSER_MCP_SECRET", value: "server-side-only" }],
  }],
  transport: "stdio",
  command: [
    process.execPath,
    "--import",
    "tsx",
    join(cwd, "tests/fixtures/fake-agent.ts"),
    "--early-new-updates",
    "--early-fork-updates",
  ],
});
