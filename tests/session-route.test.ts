import { describe, expect, it } from "vitest";
import { projectPath, readProjectCwdFromPath, readSessionIdFromPath, sessionPath } from "../web/src/lib/session-route";

describe("session routes", () => {
  it("uses the homepage when the URL contains no session route", () => {
    for (const pathname of ["/", "", "/sessions", "/sessions/", "/other/id", "sessions/id"]) {
      expect(readSessionIdFromPath(pathname)).toBeUndefined();
    }
    expect(sessionPath()).toBe("/");
    expect(sessionPath("")).toBe("/");
  });

  it("round-trips opaque session IDs through a shareable path", () => {
    for (const sessionId of ["saved-session", "folder/session", "a b?c#d%+e", "线程 🪿", "a\\b", "%2F"]) {
      const path = sessionPath(sessionId);
      const url = new URL(path, "http://localhost:7331/old?query=old#fragment");
      expect(path).toBe(`/sessions/${encodeURIComponent(sessionId)}`);
      expect(readSessionIdFromPath(url.pathname)).toBe(sessionId);
      expect(url.search).toBe("");
      expect(url.hash).toBe("");
    }
    expect(readSessionIdFromPath("/sessions/folder%2fsession")).toBe("folder/session");
  });

  it("accepts only one path segment and does not parse query strings or fragments", () => {
    for (const pathname of [
      "/sessions/id/",
      "/sessions/id/extra",
      "/sessions//id",
      "/sessions/id?sessionId=other",
      "/sessions/id#fragment",
      "/?sessionId=id",
      "/#sessions/id",
    ]) {
      expect(readSessionIdFromPath(pathname)).toBeUndefined();
    }
  });

  it("rejects malformed encodings without throwing", () => {
    for (const encoded of ["%", "%2", "%GG", "%FF", "%E0%A4%A", "%ED%A0%80"]) {
      expect(readSessionIdFromPath(`/sessions/${encoded}`)).toBeUndefined();
    }
    expect(sessionPath("\uD800")).toBe("/");
  });

  it("bounds decoded IDs to the backend limit of 1024 UTF-16 units", () => {
    for (const sessionId of ["a".repeat(1_024), "🪿".repeat(512)]) {
      expect(readSessionIdFromPath(sessionPath(sessionId))).toBe(sessionId);
    }
    for (const sessionId of ["a".repeat(1_025), "🪿".repeat(513)]) {
      expect(sessionPath(sessionId)).toBe("/");
      expect(readSessionIdFromPath(`/sessions/${encodeURIComponent(sessionId)}`)).toBeUndefined();
    }
  });

  it("rejects dot-only IDs that browsers remove during path normalization", () => {
    for (const sessionId of [".", ".."]) {
      expect(sessionPath(sessionId)).toBe("/");
    }
    for (const encoded of [".", "..", "%2E", "%2e%2e", ".%2e"]) {
      expect(readSessionIdFromPath(`/sessions/${encoded}`)).toBeUndefined();
    }
  });
});

describe("project routes", () => {
  it("round-trips portable absolute workspaces and their session routes", () => {
    for (const cwd of ["/", "/work/attyd", "/工作路径/a b?c#d%+e", "C:\\Users\\dev\\project", "D:/work/project", "\\\\server\\share\\project"]) {
      const project = projectPath(cwd);
      expect(project).toBe(`/projects/${encodeURIComponent(cwd)}`);
      expect(readProjectCwdFromPath(new URL(project, "http://localhost").pathname)).toBe(cwd);
      expect(readSessionIdFromPath(project)).toBeUndefined();
      const session = sessionPath("folder/线程", cwd);
      expect(session).toBe(`${project}/sessions/folder%2F%E7%BA%BF%E7%A8%8B`);
      expect(readProjectCwdFromPath(session)).toBe(cwd);
      expect(readSessionIdFromPath(session)).toBe("folder/线程");
    }
  });

  it("preserves case, separators, dot components, and trailing slashes exactly", () => {
    const paths = ["/work/project", "/work/project/", "/work//project", "/work/./project", "/work/other/../project", "/work/Project", "C:\\work\\project", "C:/work/project"];
    expect(new Set(paths.map(projectPath)).size).toBe(paths.length);
    for (const cwd of paths) expect(readProjectCwdFromPath(projectPath(cwd))).toBe(cwd);
  });

  it("rejects relative, oversized, null-containing, and unencodable paths", () => {
    for (const cwd of ["", ".", "..", "work/project", "~/project", "C:project", "\\work", "/work\0project", "/\uD800", `/${"a".repeat(16_384)}`]) {
      expect(projectPath(cwd)).toBe("/");
      expect(sessionPath("saved", cwd)).toBe("/");
      try {
        expect(readProjectCwdFromPath(`/projects/${encodeURIComponent(cwd)}`)).toBeUndefined();
      } catch (error) {
        expect(error).toBeInstanceOf(URIError);
      }
    }
    const longest = `/${"a".repeat(16_383)}`;
    expect(readProjectCwdFromPath(projectPath(longest))).toBe(longest);
  });

  it("rejects malformed project and nested session routes as a whole", () => {
    for (const route of [
      "/projects", "/projects/", "/projects/%", "/projects/%2Fwork/", "/projects//work",
      "/projects/%2Fwork?extra=1", "/projects/%2Fwork#fragment", "/projects/%2Fwork/other",
      "/projects/%2Fwork/sessions", "/projects/%2Fwork/sessions/", "/projects/%2Fwork/sessions/%",
      "/projects/%2Fwork/sessions/%2E%2E", "/projects/%2Fwork/sessions/id/extra",
      "/projects/relative/sessions/id", "/projects/%FF/sessions/id",
      `/projects/%2Fwork/sessions/${"a".repeat(1_025)}`,
    ]) {
      expect(readProjectCwdFromPath(route)).toBeUndefined();
      expect(readSessionIdFromPath(route)).toBeUndefined();
    }
    expect(readProjectCwdFromPath("/sessions/saved-session")).toBeUndefined();
  });
});
