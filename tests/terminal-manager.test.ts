import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { TerminalManager } from "../server/terminal-manager";
import type { TerminalSnapshot } from "../shared/bridge";
import { WorkspaceFileSystem } from "../server/safe-fs";

const cleanups: string[] = [];

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((path) => rm(path, { recursive: true, force: true })));
});

describe("ACP terminal manager", () => {
  it("scopes terminal handles to their owning session", async () => {
    const root = await temp();
    const snapshots: TerminalSnapshot[] = [];
    const terminals = new TerminalManager(
      new WorkspaceFileSystem(root, false),
      (snapshot) => snapshots.push(snapshot),
    );
    try {
      const { terminalId } = await terminals.create({
        sessionId: "owner",
        command: process.execPath,
        args: ["-e", "process.stdout.write('ok')"],
        cwd: root,
      });
      await terminals.waitForExit({ sessionId: "owner", terminalId });
      expect(terminals.output({ sessionId: "owner", terminalId }).output).toBe("ok");
      expect(() => terminals.output({ sessionId: "other", terminalId })).toThrow(
        "does not belong",
      );
      terminals.release({ sessionId: "owner", terminalId });
      expect(snapshots[0]).toMatchObject({
        sessionId: "owner",
        terminalId,
        output: "",
        released: false,
      });
      expect(snapshots.some((snapshot) =>
        snapshot.output === "ok" && snapshot.exitStatus?.exitCode === 0
      )).toBe(true);
      expect(snapshots.at(-1)).toMatchObject({
        terminalId,
        output: "ok",
        released: true,
        exitStatus: { exitCode: 0 },
      });
      expect(() => terminals.assertReference(terminalId, "owner")).toThrow("Unknown terminal");
    } finally {
      terminals.close();
    }
  });

  it("validates output limits and truncates only at UTF-8 boundaries", async () => {
    const root = await temp();
    const terminals = new TerminalManager(new WorkspaceFileSystem(root, false));
    try {
      await expect(
        terminals.create({
          sessionId: "s",
          command: process.execPath,
          cwd: root,
          outputByteLimit: -1,
        }),
      ).rejects.toThrow("non-negative");

      const { terminalId } = await terminals.create({
        sessionId: "s",
        command: process.execPath,
        args: ["-e", "process.stdout.write('🙂🙂🙂')"],
        cwd: root,
        outputByteLimit: 9,
      });
      await terminals.waitForExit({ sessionId: "s", terminalId });
      const output = terminals.output({ sessionId: "s", terminalId });
      expect(output.truncated).toBe(true);
      expect(output.output).toBe("🙂🙂");
      expect(Buffer.byteLength(output.output)).toBeLessThanOrEqual(9);
      terminals.release({ sessionId: "s", terminalId });
    } finally {
      terminals.close();
    }
  });

  it("falls back to shell source only for an ENOENT command string without args", async () => {
    const root = await temp();
    const terminals = new TerminalManager(new WorkspaceFileSystem(root, false));
    try {
      const { terminalId } = await terminals.create({
        sessionId: "goose-compatible",
        command: "printf ATTYD_COMPOUND_COMMAND_OK",
        cwd: root,
      });
      await terminals.waitForExit({ sessionId: "goose-compatible", terminalId });
      expect(terminals.output({ sessionId: "goose-compatible", terminalId })).toMatchObject({
        output: "ATTYD_COMPOUND_COMMAND_OK",
        exitStatus: { exitCode: 0, signal: null },
      });
      terminals.release({ sessionId: "goose-compatible", terminalId });

      await expect(terminals.create({
        sessionId: "strict-argv",
        command: "printf ATTYD_MUST_NOT_RUN",
        args: ["arg-keeps-strict-argv"],
        cwd: root,
      })).rejects.toThrow("Failed to start terminal command");
    } finally {
      terminals.close();
    }
  });

  it("rejects hostile create inputs before spawning and remains usable", async () => {
    const root = await temp();
    const snapshots: TerminalSnapshot[] = [];
    const terminals = new TerminalManager(
      new WorkspaceFileSystem(root, false),
      (snapshot) => snapshots.push(snapshot),
    );
    try {
      await expect(terminals.create({
        sessionId: "s",
        command: "x".repeat(16_385),
        cwd: root,
      })).rejects.toThrow("between 1 and 16384");
      await expect(terminals.create({
        sessionId: "s",
        command: process.execPath,
        cwd: root,
        env: [
          { name: "DUPLICATE", value: "one" },
          { name: "DUPLICATE", value: "two" },
        ],
      })).rejects.toThrow("duplicate variable");
      await expect(terminals.create({
        sessionId: "s",
        command: process.execPath,
        cwd: root,
        _meta: { padding: "x".repeat(4_000_000) },
      })).rejects.toThrow("exceeds 4000000 bytes");
      await expect(terminals.create({
        sessionId: "s",
        command: join(root, "missing-terminal-command"),
        cwd: root,
      })).rejects.toThrow("Failed to start terminal command");
      expect(snapshots).toEqual([]);

      const valid = await terminals.create({
        sessionId: "s",
        command: process.execPath,
        args: ["-e", "process.stdout.write('recovered')"],
        cwd: root,
      });
      await terminals.waitForExit({ sessionId: "s", terminalId: valid.terminalId });
      expect(terminals.output({ sessionId: "s", terminalId: valid.terminalId }).output)
        .toBe("recovered");
      terminals.release({ sessionId: "s", terminalId: valid.terminalId });
    } finally {
      terminals.close();
    }
  });

  it("removes cancelled wait_for_exit requests without killing the terminal", async () => {
    const root = await temp();
    const terminals = new TerminalManager(new WorkspaceFileSystem(root, false));
    try {
      const terminal = await terminals.create({
        sessionId: "s",
        command: process.execPath,
        args: ["-e", "setInterval(() => {}, 1000)"],
        cwd: root,
      });

      for (let index = 0; index < 110; index += 1) {
        const controller = new AbortController();
        const waiting = terminals.waitForExit(
          { sessionId: "s", terminalId: terminal.terminalId },
          controller.signal,
        );
        controller.abort();
        await expect(waiting).rejects.toMatchObject({ code: -32_800 });
      }

      expect(terminals.output({ sessionId: "s", terminalId: terminal.terminalId }).exitStatus)
        .toBeNull();
      const finalWait = terminals.waitForExit({
        sessionId: "s",
        terminalId: terminal.terminalId,
      });
      terminals.kill({ sessionId: "s", terminalId: terminal.terminalId });
      await expect(finalWait).resolves.toMatchObject({ signal: expect.any(String) });
      terminals.release({ sessionId: "s", terminalId: terminal.terminalId });
    } finally {
      terminals.close();
    }
  });

  it("kills and forgets every terminal owned by a closed session", async () => {
    const root = await temp();
    const terminals = new TerminalManager(new WorkspaceFileSystem(root, false));
    try {
      const owner = await terminals.create({
        sessionId: "owner",
        command: process.execPath,
        args: ["-e", "setInterval(() => {}, 1000)"],
        cwd: root,
      });
      const survivor = await terminals.create({
        sessionId: "survivor",
        command: process.execPath,
        args: ["-e", "process.stdout.write('alive')"],
        cwd: root,
      });
      const ownerExit = terminals.waitForExit({
        sessionId: "owner",
        terminalId: owner.terminalId,
      });

      terminals.releaseSession("owner");

      await expect(ownerExit).resolves.toMatchObject({ signal: expect.any(String) });
      expect(() => terminals.output({
        sessionId: "owner",
        terminalId: owner.terminalId,
      })).toThrow("Unknown terminal");
      await terminals.waitForExit({
        sessionId: "survivor",
        terminalId: survivor.terminalId,
      });
      expect(terminals.output({
        sessionId: "survivor",
        terminalId: survivor.terminalId,
      }).output).toBe("alive");
    } finally {
      terminals.close();
    }
  });
});

async function temp(): Promise<string> {
  const path = await mkdtemp(join(tmpdir(), "attyd-terminal-"));
  cleanups.push(path);
  return path;
}
