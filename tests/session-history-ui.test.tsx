// @vitest-environment happy-dom

import type { SessionInfo } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionHistory } from "../web/src/components/acp/session-history";
import { filterSessions, groupSessionsByWorkspace } from "../web/src/lib/session-history";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("Agent thread history presentation", () => {
  it("filters loaded ACP metadata and fuzzy-matches thread titles", () => {
    const sessions: SessionInfo[] = [
      {
        sessionId: "session-alpha",
        cwd: "/workspace/attyd",
        additionalDirectories: ["/shared/protocol"],
        title: "Architecture refactor",
      },
      { sessionId: "session-beta", cwd: "/workspace/docs", title: "Write release notes" },
    ];

    expect(filterSessions(sessions, "  arc ref  ")).toEqual([sessions[0]]);
    expect(filterSessions(sessions, "ssion-beta")).toEqual([sessions[1]]);
    expect(filterSessions(sessions, "shared/protocol")).toEqual([sessions[0]]);
    expect(filterSessions(sessions, "missing")).toEqual([]);
  });

  it("keeps exact Agent workspaces distinct, including paths with the same basename", () => {
    const sessions: SessionInfo[] = [
      session("alice", undefined, "Alice task", "/alice/app"),
      session("bob", undefined, "Bob task", "/bob/app"),
      session("windows-alice", undefined, "Windows Alice task", "C:\\Users\\Alice\\app"),
      session("windows-bob", undefined, "Windows Bob task", "C:\\Users\\Bob\\app"),
      session("trailing-slash", undefined, "Agent path", "/alice/app/"),
      session("empty", undefined, "Empty workspace", ""),
      { sessionId: "missing", title: "Missing workspace" } as SessionInfo,
    ];

    const groups = groupSessionsByWorkspace(sessions);
    expect(groups.map(({ cwd, label }) => [cwd, label])).toEqual([
      ["/alice/app", "/alice/app"],
      ["/bob/app", "/bob/app"],
      ["C:\\Users\\Alice\\app", "C:\\Users\\Alice\\app"],
      ["C:\\Users\\Bob\\app", "C:\\Users\\Bob\\app"],
      ["/alice/app/", "/alice/app/"],
      ["", "Unknown workspace"],
    ]);
    expect(groups.at(-1)?.sessions.map(({ sessionId }) => sessionId)).toEqual(["empty", "missing"]);
  });

  it("orders workspaces and rows by latest valid activity with stable ties and missing dates", () => {
    const sessions: SessionInfo[] = [
      session("unknown-first", undefined, undefined, "/unknown-first"),
      session("a-missing-first", undefined, undefined, "/a"),
      session("b-latest", "2026-09-06T10:00:00Z", undefined, "/b"),
      session("a-older", "2026-09-01T10:00:00Z", undefined, "/a"),
      session("a-latest", "2026-09-06T10:00:00Z", undefined, "/a"),
      session("a-tied", "2026-09-06T10:00:00Z", undefined, "/a"),
      session("a-invalid", "invalid date", undefined, "/a"),
      session("a-missing-last", undefined, undefined, "/a"),
      session("c-latest", "2026-09-07T10:00:00Z", undefined, "/c"),
      session("unknown-last", "invalid date", undefined, "/unknown-last"),
    ];

    const groups = groupSessionsByWorkspace(sessions);
    expect(groups.map(({ cwd }) => cwd)).toEqual(["/c", "/a", "/b", "/unknown-first", "/unknown-last"]);
    expect(groups[1]?.sessions.map(({ sessionId }) => sessionId)).toEqual([
      "a-latest", "a-tied", "a-older", "a-missing-first", "a-invalid", "a-missing-last",
    ]);
  });
});

describe("Agent thread history interactions", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("filters only loaded rows, clears on Escape, and preserves Agent-owned actions", async () => {
    const onAttach = vi.fn();
    const onDelete = vi.fn();
    const onRefresh = vi.fn();
    const onMore = vi.fn();
    const sessions: SessionInfo[] = [
      session("active-session", new Date().toISOString(), "Current task", "/workspace/attyd"),
      session("architecture-session", new Date().toISOString(), "Architecture review", "/workspace/attyd"),
      session("release-session", undefined, "Release notes", "/workspace/docs"),
    ];
    await act(async () => root.render(
      <SessionHistory
        sessions={sessions}
        activeSessionId="active-session"
        activeTitle="Current Agent task"
        canList
        nextCursor="agent-page-2"
        canAttach
        canDelete
        deletingSessionIds={[]}
        openSessionIds={["release-session"]}
        disabled={false}
        onAttach={onAttach}
        onDelete={onDelete}
        onRefresh={onRefresh}
        onMore={onMore}
      />,
    ));

    expect(container.querySelector('[aria-current="page"]')?.textContent)
      .toContain("Current Agent task");
    expect([...container.querySelectorAll(".session-group-label")].map((element) => element.textContent))
      .toEqual(["/workspace/attyd", "/workspace/docs"]);
    const activeGroup = container.querySelector('[aria-current="page"]')?.closest(".session-group");
    expect(activeGroup?.querySelector(".session-group-label")?.textContent).toBe("/workspace/attyd");
    expect(activeGroup?.querySelectorAll(".session-row")).toHaveLength(2);
    expect(container.querySelectorAll('[aria-current="page"]')).toHaveLength(1);

    const filter = requireElement<HTMLInputElement>(
      container.querySelector('input[aria-label="Filter loaded Agent threads"]'),
    );
    await replaceInput(filter, "arch rev");
    expect(container.textContent).toContain("Architecture review");
    expect(container.textContent).not.toContain("Release notes");
    expect(container.textContent).not.toContain("Current Agent task");
    expect(container.querySelector(".session-filter-status")?.textContent).toBe("1 of 3 loaded threads");
    expect(container.textContent).toContain("Filter covers loaded threads only.");

    const row = requireElement<HTMLButtonElement>(
      [...container.querySelectorAll<HTMLButtonElement>(".session-open")]
        .find((button) => button.textContent?.includes("Architecture review")),
    );
    await act(async () => row.click());
    expect(onAttach).toHaveBeenCalledWith(sessions[1]);

    await act(async () => {
      filter.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(filter.value).toBe("");
    expect(container.textContent).toContain("Release notes");

    const currentDelete = requireButton(container, "Delete Current Agent task");
    expect(currentDelete.disabled).toBe(false);
    expect(currentDelete.title).toContain("Close and delete this thread");
    await act(async () => currentDelete.click());
    expect(onDelete).toHaveBeenCalledWith("active-session");

    const openDelete = requireButton(container, "Delete Release notes");
    expect(openDelete.disabled).toBe(false);
    expect(openDelete.title).toContain("Close and delete this thread");
    await act(async () => openDelete.click());
    expect(onDelete).toHaveBeenCalledWith("release-session");
    expect(container.textContent).not.toContain(" · open");
    await act(async () => requireButton(container, "Refresh Agent threads").click());
    expect(onRefresh).toHaveBeenCalledOnce();
    await act(async () => requireButtonByText(container, "Load more from Agent").click());
    expect(onMore).toHaveBeenCalledWith("agent-page-2");
  });

  it("keeps cached threads attachable without presenting an idle open status", async () => {
    const onAttach = vi.fn();
    const sessions = [
      session("cached", undefined, "Previously viewed"),
      session("saved", undefined, "Agent listed"),
      session("busy", undefined, "Working thread"),
    ];
    await act(async () => root.render(
      <SessionHistory
        sessions={sessions}
        activeSessionId="active"
        activeTitle="Current thread"
        activeCwd="/workspace"
        canList
        canAttach={false}
        canDelete
        deletingSessionIds={[]}
        openSessionIds={["cached", "busy"]}
        busySessionIds={["busy"]}
        attentionSessionIds={["busy"]}
        disabled={false}
        onAttach={onAttach}
        onDelete={vi.fn()}
        onRefresh={vi.fn()}
        onMore={vi.fn()}
      />,
    ));

    const cached = requireElement(container.querySelector<HTMLButtonElement>('.session-open[title="cached"]'));
    expect(cached.disabled).toBe(false);
    expect(cached.querySelector("small")?.textContent).toBe("Saved by Agent");
    await act(async () => cached.click());
    expect(onAttach).toHaveBeenCalledWith(sessions[0]);
    const listed = [...container.querySelectorAll<HTMLButtonElement>(".session-open")]
      .find((button) => button.textContent?.includes("Agent listed"));
    expect(listed?.disabled).toBe(true);
    expect(container.querySelector('[aria-current="page"]')?.textContent).toContain(" · active");
    expect(container.querySelector('[aria-label="Agent input required"]')).not.toBeNull();
    expect(requireButton(container, "Delete Working thread").disabled).toBe(true);
    expect(container.textContent).not.toContain(" · open");
  });

  it("groups an unlisted active thread under its runtime workspace and filters by that path", async () => {
    await act(async () => root.render(
      <SessionHistory
        sessions={[session("saved-session", undefined, "Saved task", "/workspace/docs")]}
        activeSessionId="new-session"
        activeTitle="New task"
        activeCwd="/workspace/current"
        canList
        canAttach
        canDelete={false}
        deletingSessionIds={[]}
        disabled={false}
        onAttach={vi.fn()}
        onDelete={vi.fn()}
        onRefresh={vi.fn()}
        onMore={vi.fn()}
      />,
    ));

    const activeGroup = container.querySelector('[aria-current="page"]')?.closest(".session-group");
    expect(activeGroup?.querySelector(".session-group-label")?.textContent).toBe("/workspace/current");
    expect(container.querySelector(".session-filter-status")?.textContent).toBe("2 loaded threads");

    const filter = requireElement<HTMLInputElement>(
      container.querySelector('input[aria-label="Filter loaded Agent threads"]'),
    );
    await replaceInput(filter, "/workspace/current");
    expect(container.querySelector(".session-filter-status")?.textContent).toBe("1 of 2 loaded threads");
    expect(container.textContent).toContain("New task");
    expect(container.textContent).not.toContain("Saved task");
    expect(container.querySelectorAll(".session-group-label")).toHaveLength(1);

    await replaceInput(filter, "/workspace/docs");
    expect(container.textContent).toContain("Saved task");
    expect(container.textContent).not.toContain("New task");
    expect(container.querySelector(".session-group-label")?.textContent).toBe("/workspace/docs");
  });
});

function session(sessionId: string, updatedAt?: string, title = sessionId, cwd = "/workspace"): SessionInfo {
  return { sessionId, cwd, title, updatedAt };
}

function requireElement<T extends Element>(element: T | undefined | null): T {
  if (!element) throw new Error("Missing expected element");
  return element;
}

function requireButton(container: ParentNode, label: string): HTMLButtonElement {
  return requireElement(container.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`));
}

function requireButtonByText(container: ParentNode, text: string): HTMLButtonElement {
  return requireElement(
    [...container.querySelectorAll<HTMLButtonElement>("button")]
      .find((button) => button.textContent?.trim() === text),
  );
}

async function replaceInput(element: HTMLInputElement, value: string): Promise<void> {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(element, value);
    element.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
  });
}
