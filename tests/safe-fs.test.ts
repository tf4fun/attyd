import { mkdtemp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { WorkspaceFileSystem } from "../server/safe-fs";

const cleanups: string[] = [];

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((path) => rm(path, { recursive: true, force: true })));
});

describe("workspace filesystem boundary", () => {
  it("reads line ranges and writes inside the workspace", async () => {
    const root = await temp("workspace");
    const file = join(root, "notes.txt");
    await writeFile(file, "one\ntwo\nthree", "utf8");
    const fs = new WorkspaceFileSystem(root, false);

    await expect(fs.read({ sessionId: "s", path: file, line: 2, limit: 1 })).resolves.toEqual({ content: "two" });
    await fs.write({ sessionId: "s", path: file, content: "updated" });
    await expect(readFile(file, "utf8")).resolves.toBe("updated");
  });

  it("rejects invalid ranges and oversized content before file-service mutation", async () => {
    const root = await temp("workspace");
    const file = join(root, "bounded.txt");
    const oversized = "x".repeat(4_000_001);
    await writeFile(file, "stable", "utf8");
    const fs = new WorkspaceFileSystem(root, false);

    await expect(fs.read({ sessionId: "s", path: file, line: 0 })).rejects.toThrow(
      "1-based uint32",
    );
    await expect(fs.read({ sessionId: "s", path: file, limit: -1 })).rejects.toThrow(
      "limit must be a uint32",
    );
    await expect(fs.write({ sessionId: "s", path: file, content: oversized })).rejects.toThrow(
      "write exceeds 4000000 bytes",
    );
    await expect(readFile(file, "utf8")).resolves.toBe("stable");

    await writeFile(file, `head\n${oversized}`, "utf8");
    await expect(fs.read({ sessionId: "s", path: file })).rejects.toThrow(
      "read exceeds 4000000 bytes",
    );
    await expect(fs.read({ sessionId: "s", path: file, line: 1, limit: 1 })).resolves.toEqual({
      content: "head",
    });
    await writeFile(file, "recovered", "utf8");
    await expect(fs.read({ sessionId: "s", path: file })).resolves.toEqual({
      content: "recovered",
    });
  });

  it("propagates request cancellation before reads or writes mutate state", async () => {
    const root = await temp("workspace");
    const file = join(root, "cancelled.txt");
    await writeFile(file, "stable", "utf8");
    const fs = new WorkspaceFileSystem(root, false);

    const preCancelled = new AbortController();
    preCancelled.abort();
    await expect(fs.read(
      { sessionId: "s", path: file },
      preCancelled.signal,
    )).rejects.toMatchObject({ code: -32_800 });

    const readController = new AbortController();
    const reading = fs.read(
      { sessionId: "s", path: file, line: 1, limit: 1 },
      readController.signal,
    );
    readController.abort();
    await expect(reading).rejects.toMatchObject({ code: -32_800 });

    const writeController = new AbortController();
    const writing = fs.write(
      { sessionId: "s", path: file, content: "must not commit" },
      writeController.signal,
    );
    writeController.abort();
    await expect(writing).rejects.toMatchObject({ code: -32_800 });
    await expect(readFile(file, "utf8")).resolves.toBe("stable");
  });

  it("rejects traversal, symlink escape, and read-only writes", async () => {
    const root = await temp("workspace");
    const outside = await temp("outside");
    await writeFile(join(outside, "secret.txt"), "secret", "utf8");
    await mkdir(join(root, "links"));
    await symlink(outside, join(root, "links", "outside"));

    const fs = new WorkspaceFileSystem(root, false);
    await expect(fs.read({ sessionId: "s", path: "relative.txt" })).rejects.toThrow("absolute");
    await expect(fs.write({ sessionId: "s", path: "relative.txt", content: "x" })).rejects.toThrow("absolute");
    await expect(fs.read({ sessionId: "s", path: join(root, "..", "escape") })).rejects.toThrow("outside");
    await expect(fs.read({ sessionId: "s", path: join(root, "links", "outside", "secret.txt") })).rejects.toThrow("outside");

    const readonly = new WorkspaceFileSystem(root, true);
    await expect(readonly.write({ sessionId: "s", path: join(root, "new.txt"), content: "x" })).rejects.toThrow("read-only");
  });

  it("confines access to the primary and configured additional roots", async () => {
    const root = await temp("workspace");
    const additional = await temp("additional");
    const outside = await temp("outside");
    const extraFile = join(additional, "extra.txt");
    await writeFile(extraFile, "extra", "utf8");
    await writeFile(join(outside, "secret.txt"), "secret", "utf8");
    const fs = new WorkspaceFileSystem(root, false, [additional]);

    await expect(fs.read({ sessionId: "s", path: extraFile })).resolves.toEqual({
      content: "extra",
    });
    await fs.write({ sessionId: "s", path: join(additional, "new.txt"), content: "new" });
    await expect(readFile(join(additional, "new.txt"), "utf8")).resolves.toBe("new");
    await expect(fs.read({ sessionId: "s", path: join(outside, "secret.txt") })).rejects.toThrow("outside");
  });

  it("searches bounded workspace text context and embeds the selected file", async () => {
    const root = await temp("workspace-context");
    const additional = await temp("workspace-context-extra");
    await mkdir(join(root, "src"));
    await mkdir(join(root, "node_modules"));
    await writeFile(join(root, "src", "safe-context.ts"), "export const safe = true;\n", "utf8");
    await writeFile(join(root, "node_modules", "hidden.ts"), "hidden", "utf8");
    await writeFile(join(additional, "extra-context.md"), "# Extra\n", "utf8");
    const fs = new WorkspaceFileSystem(root, false, [additional]);

    await expect(fs.searchContext("safe con")).resolves.toEqual([
      expect.objectContaining({
        name: "safe-context.ts",
        relativePath: join("src", "safe-context.ts"),
        size: 26,
      }),
    ]);
    expect((await fs.searchContext("hidden")).map(({ name }) => name)).not.toContain("hidden.ts");
    expect(await fs.searchContext("extra")).toEqual([
      expect.objectContaining({
        name: "extra-context.md",
        rootName: basename(additional),
      }),
    ]);

    await expect(fs.readContext(join(root, "src", "safe-context.ts"))).resolves.toEqual({
      name: join("src", "safe-context.ts"),
      size: 26,
      block: {
        type: "resource",
        resource: {
          uri: expect.stringMatching(/^file:\/\/\/.+\/src\/safe-context\.ts$/),
          mimeType: "text/typescript",
          text: "export const safe = true;\n",
        },
      },
    });
  });

  it("rejects unsafe, binary, and oversized workspace context reads", async () => {
    const root = await temp("workspace-context-boundary");
    const outside = await temp("workspace-context-outside");
    const large = join(root, "large.txt");
    await writeFile(join(root, "binary.png"), new Uint8Array([0, 1, 2]));
    await writeFile(large, "x".repeat(3 * 1024 * 1024 + 1), "utf8");
    await writeFile(join(outside, "secret.txt"), "secret", "utf8");
    const fs = new WorkspaceFileSystem(root, false);

    await expect(fs.readContext(join(root, "binary.png"))).rejects.toThrow("supported text");
    await expect(fs.readContext(large)).rejects.toThrow("exceeds 3145728 bytes");
    await expect(fs.readContext(join(outside, "secret.txt"))).rejects.toThrow("outside");
    expect(await fs.searchContext("large")).toEqual([]);
  });
});

async function temp(label: string): Promise<string> {
  const path = await mkdtemp(join(tmpdir(), `attyd-${label}-`));
  cleanups.push(path);
  return path;
}
