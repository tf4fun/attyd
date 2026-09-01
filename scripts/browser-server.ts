import { join } from "node:path";
import { startRustTestServer } from "./rust-test-server.js";

const portValue = process.argv[2] ?? "7332";
const port = Number.parseInt(portValue, 10);
if (!Number.isInteger(port) || port < 1 || port > 65_535) {
  throw new Error(`Invalid port: ${portValue}`);
}

const cwd = process.cwd();
const provider = {
  type: "acp",
  name: "browser-fixture",
  serverId: "browser-fixture",
  command: process.execPath,
  args: ["--import", "tsx", join(cwd, "tests/fixtures/fake-mcp-server.ts")],
  env: [{ name: "BROWSER_MCP_SECRET", value: "server-side-only" }],
};
const server = await startRustTestServer({
  cwd,
  port,
  command: [
    process.execPath,
    "--import",
    "tsx",
    join(cwd, "tests/fixtures/fake-agent.ts"),
    "--early-new-updates",
    "--early-fork-updates",
  ],
  mcpConfig: { mcpServers: [provider] },
});

let closing = false;
const close = async () => {
  if (closing) return;
  closing = true;
  await server.close();
};
process.once("SIGINT", () => void close());
process.once("SIGTERM", () => void close());
process.once("beforeExit", () => void close());

await new Promise<void>(() => undefined);
