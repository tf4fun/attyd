import { describe, expect, it } from "vitest";
import {
  AuthTerminalManager,
  type AuthTerminalExit,
  validateTerminalAuthMethod,
} from "../server/auth-terminal";

describe("Agent terminal authentication PTY", () => {
  it("reproduces the base invocation and appends Agent-provided args and env", async () => {
    let output = "";
    let resolveExit!: (exit: AuthTerminalExit) => void;
    const exited = new Promise<AuthTerminalExit>((resolve) => {
      resolveExit = resolve;
    });
    const manager = new AuthTerminalManager(
      (_requestId, data) => {
        output += data;
      },
      resolveExit,
    );
    const script = [
      "process.stdout.write(process.argv.slice(1).join('|') + ':' + process.env.ATTYD_AUTH_TEST + '> ')",
      "process.stdin.setEncoding('utf8')",
      "process.stdin.on('data', value => process.exit(value.includes('ok') ? 0 : 2))",
    ].join(";");
    try {
      manager.start({
        requestId: "auth",
        method: {
          id: "terminal-login",
          name: "Terminal login",
          type: "terminal",
          args: ["terminal-arg"],
          env: { ATTYD_AUTH_TEST: "method-env" },
        },
        command: [process.execPath, "-e", script, "base-arg"],
        cwd: process.cwd(),
        env: { ...process.env, ATTYD_AUTH_TEST: "base-env" },
        cols: 80,
        rows: 24,
      });
      await waitFor(() => output.includes("> "));
      expect(output).toContain("base-arg|terminal-arg:method-env>");
      manager.resize("auth", 100, 30);
      manager.write("auth", "ok\r");
      await expect(exited).resolves.toMatchObject({
        requestId: "auth",
        methodId: "terminal-login",
        status: "succeeded",
        exitCode: 0,
      });
    } finally {
      manager.close();
    }
  }, 10_000);

  it("reports cancellation and rejects stale terminal input", async () => {
    let resolveExit!: (exit: AuthTerminalExit) => void;
    const exited = new Promise<AuthTerminalExit>((resolve) => {
      resolveExit = resolve;
    });
    const manager = new AuthTerminalManager(() => undefined, resolveExit);
    try {
      manager.start({
        requestId: "cancel-auth",
        method: { id: "terminal-login", name: "Terminal login", type: "terminal" },
        command: [process.execPath, "-e", "setInterval(() => {}, 1000)"],
        cwd: process.cwd(),
        cols: 80,
        rows: 24,
      });
      manager.cancel("cancel-auth");
      await expect(exited).resolves.toMatchObject({ status: "cancelled" });
      expect(() => manager.write("cancel-auth", "late")).toThrow("no longer active");
    } finally {
      manager.close();
    }
  }, 10_000);

  it("bounds Agent-provided arguments and environment", () => {
    expect(() => validateTerminalAuthMethod({
      id: "terminal-login",
      name: "Terminal login",
      type: "terminal",
      env: { "INVALID-NAME": "value" },
    })).toThrow("invalid environment variable name");
    expect(() => validateTerminalAuthMethod({
      id: "terminal-login",
      name: "Terminal login",
      type: "terminal",
      args: ["x".repeat(16_385)],
    })).toThrow("at most 16384 characters");
  });
});

async function waitFor(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("Timed out waiting for terminal output");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
