// @vitest-environment happy-dom

import type { SessionInfo } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionHistory } from "../web/src/components/acp/session-history";
import { filterSessions, groupSessionsByRecency } from "../web/src/lib/session-history";

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

  it("sorts Agent-reported sessions into deterministic local-time groups", () => {
    const now = new Date(2026, 7, 31, 12);
    const at = (daysAgo: number, hour = 9) => {
      const value = new Date(2026, 7, 31 - daysAgo, hour);
      return value.toISOString();
    };
    const sessions: SessionInfo[] = [
      session("today-early", at(0, 8)),
      session("unknown"),
      session("month", at(15)),
      session("today-late", at(0, 11)),
      session("week", at(4)),
      session("yesterday", at(1)),
      session("older", at(40)),
    ];

    const groups = groupSessionsByRecency(sessions, now);
    expect(groups.map(({ label }) => label)).toEqual([
      "Today",
      "Yesterday",
      "Previous 7 Days",
      "Previous 30 Days",
      "Older",
      "Saved by Agent",
    ]);
    expect(groups[0]?.sessions.map(({ sessionId }) => sessionId))
      .toEqual(["today-late", "today-early"]);
    expect(groups.at(-1)?.sessions.map(({ sessionId }) => sessionId)).toEqual(["unknown"]);
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
      session("active-session", new Date().toISOString(), "Current task"),
      session("architecture-session", new Date().toISOString(), "Architecture review"),
      session("release-session", undefined, "Release notes"),
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
    expect(container.textContent).toContain("Today");
    expect(container.textContent).toContain("Saved by Agent");

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

    const openDelete = requireButton(container, "Delete Release notes");
    expect(openDelete.disabled).toBe(true);
    expect(openDelete.title).toContain("Close this open thread");
    expect(container.textContent).toContain("open");
    await act(async () => requireButton(container, "Refresh Agent threads").click());
    expect(onRefresh).toHaveBeenCalledOnce();
    await act(async () => requireButtonByText(container, "Load more from Agent").click());
    expect(onMore).toHaveBeenCalledWith("agent-page-2");
  });
});

function session(sessionId: string, updatedAt?: string, title = sessionId): SessionInfo {
  return { sessionId, cwd: "/workspace", title, updatedAt };
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
