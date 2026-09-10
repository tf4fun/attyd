import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmod, copyFile, mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const binaryName = process.platform === "win32" ? "attyd.exe" : "attyd";
const source = process.env.ATTYD_RUST_BINARY ?? join(process.cwd(), "target", "release", binaryName);
const directory = await mkdtemp(join(tmpdir(), "attyd-binary-smoke-"));
const executable = join(directory, binaryName);
await copyFile(source, executable);
if (process.platform !== "win32") await chmod(executable, 0o755);

const child = spawn(executable, [
  "--host",
  "127.0.0.1",
  "--port",
  "0",
  "--transport",
  "ws",
  "--",
  "ws://127.0.0.1:1/acp",
], {
  cwd: directory,
  env: process.env,
  stdio: ["ignore", "pipe", "pipe"],
});
const closed = new Promise<void>((resolve) => child.once("close", () => resolve()));

try {
  const port = await listeningPort();
  const origin = `http://127.0.0.1:${port}`;
  const health = await fetch(`${origin}/api/health`);
  assert.deepEqual(await health.json(), {
    ok: true,
    protocol: "acp/v1",
    backend: "rust",
  });
  const page = await fetch(origin);
  assert.match(page.headers.get("content-type") ?? "", /^text\/html/u);
  const html = await page.text();
  assert.match(html, /<div id="root"><\/div>/u);
  const scriptPath = html.match(/<script[^>]+src="([^"]+)"/u)?.[1];
  assert.ok(scriptPath, "embedded HTML must reference its JavaScript bundle");
  const bundle = await fetch(new URL(scriptPath, origin));
  assert.equal(bundle.status, 200);
  assert.match(bundle.headers.get("content-type") ?? "", /^text\/javascript/u);
  const { size } = await stat(executable);
  console.log(`standalone Rust binary passed (${(size / 1_048_576).toFixed(1)} MiB)`);
} finally {
  if (child.exitCode == null && child.signalCode == null) child.kill("SIGTERM");
  const timeout = setTimeout(() => child.kill("SIGKILL"), 2_000);
  await closed;
  clearTimeout(timeout);
  await rm(directory, { recursive: true, force: true });
}

function listeningPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    const timeout = setTimeout(() => {
      reject(new Error(`standalone binary did not start; stdout=${stdout} stderr=${stderr}`));
    }, 10_000);
    const finish = (error?: Error, port?: number) => {
      clearTimeout(timeout);
      child.stdout.removeAllListeners("data");
      child.stderr.removeAllListeners("data");
      child.removeAllListeners("exit");
      child.removeListener("error", onError);
      if (error) reject(error);
      else resolve(port!);
    };
    const onError = (error: Error) => finish(error);
    child.once("error", onError);
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
      const match = stdout.match(/attyd listening on http:\/\/127\.0\.0\.1:(\d+)/u);
      if (match) finish(undefined, Number(match[1]));
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });
    child.once("exit", (code, signal) => {
      finish(new Error(
        `standalone binary exited before listening (code=${String(code)}, ` +
        `signal=${String(signal)}); stdout=${stdout} stderr=${stderr}`,
      ));
    });
  });
}
