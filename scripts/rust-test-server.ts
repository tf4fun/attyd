import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface RustTestServerOptions {
  command: string[];
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  host?: string;
  port?: number;
  args?: string[];
  mcpConfig?: unknown;
}

export interface RustTestServer {
  port: number;
  close(): Promise<void>;
}

export async function startRustTestServer(
  options: RustTestServerOptions,
): Promise<RustTestServer> {
  const cwd = options.cwd ?? process.cwd();
  const temporaryDirectory = options.mcpConfig == null
    ? undefined
    : await mkdtemp(join(tmpdir(), "attyd-rust-test-"));
  const mcpConfig = temporaryDirectory == null
    ? undefined
    : join(temporaryDirectory, "mcp.json");
  if (mcpConfig != null) {
    await writeFile(mcpConfig, JSON.stringify(options.mcpConfig), "utf8");
  }

  const binary = process.env.ATTYD_RUST_BINARY ?? join(cwd, "target/debug/attyd");
  const args = [
    "--host",
    options.host ?? "127.0.0.1",
    "--port",
    String(options.port ?? 0),
    "--cwd",
    cwd,
    ...(mcpConfig == null ? [] : ["--mcp-config", mcpConfig]),
    ...(options.args ?? []),
    "--",
    ...options.command,
  ];
  const child = spawn(binary, args, {
    cwd,
    env: options.env ?? process.env,
    stdio: ["pipe", "pipe", "pipe"],
  });

  try {
    const port = await waitForRustServer(child);
    return {
      port,
      async close() {
        child.kill("SIGTERM");
        await Promise.race([
          new Promise<void>((resolve) => child.once("exit", () => resolve())),
          new Promise<void>((resolve) => setTimeout(resolve, 2_000)),
        ]);
        if (child.exitCode == null && child.signalCode == null) child.kill("SIGKILL");
        if (temporaryDirectory != null) {
          await rm(temporaryDirectory, { recursive: true, force: true });
        }
      },
    };
  } catch (error) {
    child.kill("SIGKILL");
    if (temporaryDirectory != null) {
      await rm(temporaryDirectory, { recursive: true, force: true });
    }
    throw error;
  }
}

function waitForRustServer(child: ChildProcessWithoutNullStreams): Promise<number> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    const timeout = setTimeout(() => {
      finish(new Error(`Rust test server did not start; stdout=${stdout} stderr=${stderr}`));
    }, 10_000);
    const finish = (error?: Error, port?: number) => {
      clearTimeout(timeout);
      child.stdout.removeAllListeners("data");
      child.stderr.removeAllListeners("data");
      child.removeAllListeners("exit");
      child.removeAllListeners("error");
      if (error) reject(error);
      else resolve(port!);
    };
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
      const match = stdout.match(/attyd listening on http:\/\/[^:]+:(\d+)/u);
      if (match) finish(undefined, Number(match[1]));
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });
    child.once("error", (error) => finish(error));
    child.once("exit", (code, signal) => {
      finish(new Error(
        `Rust test server exited before listening (code=${String(code)}, ` +
        `signal=${String(signal)}); stdout=${stdout} stderr=${stderr}`,
      ));
    });
  });
}
