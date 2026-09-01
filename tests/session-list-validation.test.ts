import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { validateSessionListPage } from "../server/session-list-validation";

const cwd = resolve("/workspace/project");

describe("Agent session/list validation", () => {
  it("accepts bounded pages for the requested cwd and returns their serialized cost", () => {
    const bytes = validateSessionListPage({
      sessions: [{
        sessionId: "saved",
        cwd,
        additionalDirectories: [resolve("/workspace/shared")],
        title: "Saved session",
        updatedAt: "2026-08-30T08:00:00.000Z",
      }],
      nextCursor: "next",
    }, undefined, cwd, {
      listedSessionIds: new Set(),
      listedCursors: new Set(),
      accumulatedBytes: 0,
    });
    expect(bytes).toBeGreaterThan(0);

    expect(() => validateSessionListPage({
      sessions: [{ sessionId: "second", cwd }],
    }, "next", cwd, {
      listedSessionIds: new Set(["saved"]),
      listedCursors: new Set(["next"]),
      accumulatedBytes: bytes,
    })).not.toThrow();
  });

  it("accepts sessions from multiple Agent-host workspaces when no cwd filter was requested", () => {
    expect(() => validateSessionListPage({
      sessions: [
        { sessionId: "one", cwd: "/srv/projects/one" },
        { sessionId: "two", cwd: "/home/agent/two" },
      ],
    }, undefined, undefined, emptyContext())).not.toThrow();
  });

  it("preserves Agent results outside its filter while rejecting malformed workspace roots", () => {
    expect(() => validateSessionListPage({
      sessions: [{ sessionId: "other", cwd: resolve("/workspace/other") }],
    }, undefined, cwd, emptyContext())).not.toThrow();
    expect(() => validateSessionListPage({
      sessions: [{ sessionId: "relative", cwd: "./relative" }],
    }, undefined, cwd, emptyContext())).toThrow("invalid absolute session cwd");
    expect(() => validateSessionListPage({
      sessions: [{
        sessionId: "duplicate-roots",
        cwd,
        additionalDirectories: [
          resolve("/workspace/shared"),
          resolve("/workspace/project/../shared"),
        ],
      }],
    }, undefined, cwd, emptyContext())).not.toThrow();
    expect(() => validateSessionListPage({
      sessions: [{
        sessionId: "main-root-repeated",
        cwd,
        additionalDirectories: [resolve("/workspace/project/.")],
      }],
    }, undefined, cwd, emptyContext())).not.toThrow();
    expect(() => validateSessionListPage({
      sessions: [{
        sessionId: "too-many-roots",
        cwd,
        additionalDirectories: Array.from(
          { length: 257 },
          (_, index) => resolve(`/workspace/root-${index}`),
        ),
      }],
    }, undefined, cwd, emptyContext())).toThrow("more than 256 additional directories");
  });

  it("bounds individual pages and cumulative pagination without poisoning refreshes", () => {
    expect(() => validateSessionListPage({
      sessions: [],
      _meta: { padding: "x".repeat(4_000_000) },
    }, undefined, cwd, emptyContext())).toThrow("page exceeds 4000000 bytes");
    expect(() => validateSessionListPage({
      sessions: [],
    }, "next", cwd, {
      listedSessionIds: new Set(),
      listedCursors: new Set(["next"]),
      accumulatedBytes: 15_999_999,
    })).toThrow("16000000 cumulative bytes");

    expect(() => validateSessionListPage({
      sessions: [{ sessionId: "refreshed", cwd }],
    }, undefined, cwd, {
      listedSessionIds: new Set(["stale"]),
      listedCursors: new Set(["old-cursor"]),
      accumulatedBytes: 16_000_000,
    })).not.toThrow();
  });
});

function emptyContext() {
  return {
    listedSessionIds: new Set<string>(),
    listedCursors: new Set<string>(),
    accumulatedBytes: 0,
  };
}
