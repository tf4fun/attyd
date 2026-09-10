import { spawn, type ChildProcess } from "node:child_process";
import { constants } from "node:os";
import { fileURLToPath } from "node:url";
import { createServer, type ViteDevServer } from "vite";
import { hasDevServer } from "./dev-options.js";

const root = fileURLToPath(new URL("../", import.meta.url));
const args = process.argv.slice(2);
let vite: ViteDevServer | undefined;
let active: ManagedChild | undefined;
let stopping = false;
let signalExitCode: number | undefined;

function shutdown(signal: "SIGINT" | "SIGTERM") {
  stopping = true;
  signalExitCode = signal === "SIGINT" ? 130 : 143;
  void active?.stop();
}
const onInterrupt = () => shutdown("SIGINT");
const onTerminate = () => shutdown("SIGTERM");
process.on("SIGINT", onInterrupt);
process.on("SIGTERM", onTerminate);

try {
  let hostArgs = args;
  if (!hasDevServer(args)) {
    vite = await createServer({ configFile: fileURLToPath(new URL("../vite.config.ts", import.meta.url)) });
    if (!stopping) await vite.listen();
    if (!stopping) {
      const address = vite.httpServer?.address();
      if (address == null || typeof address === "string") {
        throw new Error("Vite did not open a TCP listener");
      }
      const origin = `http://127.0.0.1:${address.port}`;
      hostArgs = ["--dev-server", origin, ...args];
      console.log(`Frontend development server: ${origin}`);
      console.log("Open the attyd URL printed below; frontend changes update without rebuilding Rust.");
    }
  }

  if (!stopping) {
    // Cargo reports the real executable path, including CARGO_TARGET_DIR and Cargo configuration.
    let executable: string | undefined;
    active = startChild("cargo", ["build", "--features", "dev", "--message-format=json-render-diagnostics"], true, (line) => {
      try {
        const message = JSON.parse(line) as {
          reason?: string;
          executable?: string | null;
          target?: { name?: string; kind?: string[] };
        };
        if (message.reason === "compiler-artifact" && message.target?.name === "attyd"
          && message.target.kind?.includes("bin") && message.executable != null) {
          executable = message.executable;
        }
      } catch {
        console.log(line);
      }
    });
    const buildCode = await active.done;
    if (!stopping && buildCode !== 0) process.exitCode = buildCode;
    else if (!stopping) {
      if (executable == null) throw new Error("Cargo succeeded without reporting the attyd executable");
      // Own the Rust process directly so stopping this runner never leaves cargo run's child behind.
      active = startChild(executable, hostArgs, false);
      process.exitCode = await active.done;
    }
  }
} catch (error) {
  if (!stopping) console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
} finally {
  await Promise.all([active?.stop(), vite?.close()]);
  process.removeListener("SIGINT", onInterrupt);
  process.removeListener("SIGTERM", onTerminate);
  if (signalExitCode != null) process.exitCode = signalExitCode;
}

interface ManagedChild {
  done: Promise<number>;
  stop(): Promise<void>;
}

function startChild(
  command: string,
  commandArgs: string[],
  building: boolean,
  onLine?: (line: string) => void,
): ManagedChild {
  const child = spawn(command, commandArgs, {
    cwd: root,
    stdio: ["inherit", onLine == null ? "inherit" : "pipe", "inherit"],
    detached: process.platform !== "win32",
  });
  let finished = false;
  let stopPromise: Promise<void> | undefined;
  let buffered = "";
  child.stdout?.setEncoding("utf8");
  child.stdout?.on("data", (chunk: string) => {
    buffered += chunk;
    let newline: number;
    while ((newline = buffered.indexOf("\n")) !== -1) {
      onLine?.(buffered.slice(0, newline));
      buffered = buffered.slice(newline + 1);
    }
  });
  const done = new Promise<number>((resolve, reject) => {
    child.once("error", (error) => {
      finished = true;
      reject(error);
    });
    child.once("close", (code, signal) => {
      finished = true;
      if (buffered.length > 0) onLine?.(buffered);
      resolve(code ?? (signal == null ? 1 : 128 + (constants.signals[signal] ?? 1)));
    });
  });
  return {
    done,
    stop() {
      stopPromise ??= (async () => {
        if (finished) return;
        terminateChild(child, building, false);
        let timer: ReturnType<typeof setTimeout> | undefined;
        await Promise.race([
          done.catch(() => undefined),
          new Promise<void>((resolve) => { timer = setTimeout(resolve, 10_000); }),
        ]);
        clearTimeout(timer);
        if (!finished) {
          terminateChild(child, building, true);
          await done.catch(() => undefined);
        }
      })();
      return stopPromise;
    },
  };
}

function terminateChild(child: ChildProcess, building: boolean, force: boolean) {
  if (child.pid == null || child.exitCode != null || child.signalCode != null) return;
  if (process.platform === "win32") {
    // Ctrl+C also reaches the attached Rust console process. Allow its shutdown handler
    // to finish before falling back to forced tree cleanup after the grace period.
    if (!building && !force) return;
    // Windows does not have Unix process groups; limit tree cleanup to this owned child.
    const killer = spawn("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
    killer.on("error", () => { child.kill(); });
    killer.on("close", (code) => { if (code !== 0) child.kill(); });
    return;
  }
  try {
    // During compilation also stop rustc/linker children. Runtime gets graceful ACP cleanup first.
    if (building || force) process.kill(-child.pid, force ? "SIGKILL" : "SIGTERM");
    else child.kill("SIGTERM");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
  }
}
