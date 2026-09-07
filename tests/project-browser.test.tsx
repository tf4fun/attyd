// @vitest-environment happy-dom

import type { SessionInfo } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ProjectBrowser, type ProjectBrowserProps, workspaceName } from "../web/src/components/acp/project-browser";
import { projectPath, sessionPath } from "../web/src/lib/session-route";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const sessions: SessionInfo[] = [
  { sessionId: "older", cwd: "/work/alpha", title: "Refine typography", updatedAt: "2026-09-01T08:00:00Z" },
  { sessionId: "other", cwd: "/archive/alpha", title: "Archived notes", updatedAt: "2026-09-02T08:00:00Z" },
  { sessionId: "latest", cwd: "/work/alpha", title: "Fix scroll tracking", updatedAt: "2026-09-03T08:00:00Z" },
];

describe("project browser", () => {
  let container: HTMLDivElement;
  let root: Root;
  let props: ProjectBrowserProps;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    props = {
      sessions,
      canList: true,
      canAttach: true,
      canDelete: true,
      disabled: false,
      busySessionIds: [],
      attentionSessionIds: [],
      deletingSessionIds: [],
      openSessionIds: [],
      onProject: vi.fn(),
      onAttach: vi.fn(),
      onDelete: vi.fn(),
      onNew: vi.fn(),
      onRefresh: vi.fn(),
      onMore: vi.fn(),
    };
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  async function render(overrides: Partial<ProjectBrowserProps> = {}): Promise<void> {
    props = { ...props, ...overrides };
    await act(async () => root.render(<ProjectBrowser {...props} />));
  }

  function element<T extends HTMLElement>(selector: string): T {
    const result = container.querySelector<T>(selector);
    if (!result) throw new Error(`Missing element: ${selector}`);
    return result;
  }

  async function search(value: string): Promise<void> {
    await act(async () => {
      const input = element<HTMLInputElement>('input[type="search"]');
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
      input.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
    });
  }

  it("keeps identically named projects distinct by full path and orders them by recent activity", async () => {
    await render();
    expect(element("h1").textContent).toBe("Projects");
    expect(element(".project-new").textContent).toBe("New project");
    await click(element(".project-new"));
    expect(props.onNew).toHaveBeenCalledOnce();
    const cards = [...container.querySelectorAll<HTMLAnchorElement>(".project-card")];
    expect(cards).toHaveLength(2);
    expect(cards.map((card) => card.querySelector(".project-card-title")?.textContent)).toEqual(["alpha", "alpha"]);
    expect(cards.map((card) => card.querySelector(".project-card-path")?.textContent)).toEqual(["/work/alpha", "/archive/alpha"]);
    expect(cards[0].textContent).toContain("2 loaded sessions");
    expect(cards[0].getAttribute("href")).toBe(projectPath("/work/alpha"));
    expect(container.querySelectorAll(".project-session-row")).toHaveLength(0);
    expect(element(".project-browser-status").textContent).toBe("2 projects · 3 loaded sessions");
  });

  it("finds projects by title or path without shrinking their loaded session counts", async () => {
    await render({ nextCursor: "next-page" });
    await search("scroll");
    expect(container.querySelectorAll(".project-card")).toHaveLength(1);
    expect(element(".project-card").textContent).toContain("2 loaded sessions");
    expect(element(".project-browser-status").textContent).toContain("1 of 2 projects");
    expect(element(".project-pagination").textContent).toContain("loaded sessions");
    await search("/archive/alpha");
    expect(element(".project-card-path").textContent).toBe("/archive/alpha");
    await click(element('button[aria-label="Clear search"]'));
    expect(container.querySelectorAll(".project-card")).toHaveLength(2);
    await click(element(".project-pagination button"));
    expect(props.onMore).toHaveBeenCalledWith("next-page");
  });

  it("navigates project links locally and leaves modifier clicks shareable", async () => {
    await render({ disabled: true });
    const card = element<HTMLAnchorElement>(".project-card");
    expect((await click(card)).defaultPrevented).toBe(true);
    expect(props.onProject).toHaveBeenCalledWith("/work/alpha");
    expect((await click(card, { ctrlKey: true })).defaultPrevented).toBe(false);
    expect((await click(card, { metaKey: true })).defaultPrevented).toBe(false);
    expect(props.onProject).toHaveBeenCalledOnce();
  });

  it("shows only the chosen project's sessions with canonical links and resets the home search", async () => {
    await render();
    await search("archive");
    await render({ projectCwd: "/work/alpha" });
    expect(element("h1").textContent).toBe("alpha");
    expect(element(".project-browser-path").textContent).toBe("/work/alpha");
    expect(element<HTMLInputElement>('input[type="search"]').value).toBe("");
    const links = [...container.querySelectorAll<HTMLAnchorElement>(".project-session-link")];
    expect(links.map((link) => link.querySelector("strong")?.textContent)).toEqual(["Fix scroll tracking", "Refine typography"]);
    expect(links[0].getAttribute("href")).toBe(sessionPath("latest", "/work/alpha"));
    expect((await click(links[0])).defaultPrevented).toBe(true);
    expect(props.onAttach).toHaveBeenCalledWith(sessions[2]);
    expect((await click(links[0], { shiftKey: true })).defaultPrevented).toBe(false);
    expect(props.onAttach).toHaveBeenCalledOnce();
    await search("typography");
    expect(container.querySelectorAll(".project-session-row")).toHaveLength(1);
    expect(element(".project-session-title").textContent).toBe("Refine typography");
  });

  it("exposes session attention and prevents deletion of a working session", async () => {
    await render({
      projectCwd: "/work/alpha",
      attentionSessionIds: ["latest"],
      busySessionIds: ["latest"],
      openSessionIds: ["older"],
    });
    expect(element(".project-session-status").textContent).toBe("Input needed");
    expect(element<HTMLButtonElement>('button[aria-label="Delete Fix scroll tracking"]').disabled).toBe(true);
    expect(container.querySelectorAll(".project-session-status")).toHaveLength(1);
    expect(container.querySelectorAll(".project-session-row")[1].querySelector(".project-session-status")).toBeNull();
    await click(element('button[aria-label="Delete Refine typography"]'));
    expect(props.onDelete).toHaveBeenCalledWith("older");
    expect(props.onAttach).not.toHaveBeenCalled();
    await click(element(".project-new"));
    expect(props.onNew).toHaveBeenCalledOnce();
    expect(element(".project-new").textContent).toBe("New session in project");
    await click(element(".project-refresh"));
    expect(props.onRefresh).toHaveBeenCalledOnce();
    await render({ attentionSessionIds: [] });
    expect(element(".project-session-status").textContent).toBe("Working");
  });

  it("allows open sessions without load support and blocks unavailable or deleting sessions", async () => {
    await render({ projectCwd: "/work/alpha", canAttach: false, openSessionIds: ["older"] });
    const links = [...container.querySelectorAll<HTMLAnchorElement>(".project-session-link")];
    expect(links[0].getAttribute("aria-disabled")).toBe("true");
    expect((await click(links[0])).defaultPrevented).toBe(true);
    expect(props.onAttach).not.toHaveBeenCalled();
    await click(links[1]);
    expect(props.onAttach).toHaveBeenCalledWith(sessions[0]);
    await render({ deletingSessionIds: ["older"] });
    expect(links[1].getAttribute("aria-disabled")).toBe("true");
    expect(element<HTMLButtonElement>('button[aria-label="Deleting Refine typography"]').disabled).toBe(true);
  });

  it("distinguishes empty, loading, and filtered states without hiding pagination", async () => {
    await render({ sessions: [] });
    expect(element(".project-empty h2").textContent).toBe("Your projects start here");
    expect(element(".project-empty").textContent).toContain("working directory to create a project and start its first session");
    await render({ projectCwd: "/empty/project", nextCursor: "more" });
    expect(element(".project-empty h2").textContent).toBe("No sessions yet");
    expect(element(".project-pagination").textContent).toContain("More sessions may be available");
    await render({ disabled: true });
    expect(element(".project-empty h2").textContent).toBe("Loading your workspace…");
    await render({ disabled: false, sessions });
    await search("missing");
    expect(element(".project-empty h2").textContent).toBe("No matching sessions");
    expect(element(".project-pagination button")).toBeTruthy();
  });

  it("preserves filesystem roots and disambiguates unknown workspaces without navigating them", async () => {
    expect(workspaceName("/")).toBe("/");
    expect(workspaceName("/work/alpha/")).toBe("alpha");
    expect(workspaceName("C:\\work\\alpha")).toBe("alpha");
    await render({ sessions: [{ sessionId: "unknown", cwd: "" }] });
    const card = element<HTMLAnchorElement>(".project-card");
    expect(card.textContent).toContain("Unknown workspace");
    expect(card.getAttribute("aria-disabled")).toBe("true");
    expect((await click(card)).defaultPrevented).toBe(true);
    expect(props.onProject).not.toHaveBeenCalled();
  });
});

async function click(element: Element, init: MouseEventInit = {}): Promise<MouseEvent> {
  const event = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
  await act(async () => { element.dispatchEvent(event); });
  return event;
}
