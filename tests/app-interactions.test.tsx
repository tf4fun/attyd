// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const authScenario = vi.hoisted(() => ({ logoutOnly: false, logout: vi.fn() }));

vi.mock("../web/src/lib/use-acp", async () => {
  const { initialState } = await import("../web/src/lib/state");
  const noop = () => undefined;
  return {
    useAcp: () => ({
      state: {
        ...initialState,
        phase: "ready",
        socketOpen: true,
        defaultCwd: "/workspace/attyd",
        cwd: "/workspace/attyd",
        ...(authScenario.logoutOnly ? {
          initialized: { protocolVersion: 1, authMethods: [], agentCapabilities: { auth: { logout: {} } } },
          authStatus: "available",
        } : {}),
      },
      logout: authScenario.logout,
      prompt: noop,
      cancel: noop,
      setMode: noop,
      setConfig: noop,
      respondPermission: noop,
      respondElicitation: noop,
      dismissExternalFlow: noop,
      newSession: noop,
      listSessions: noop,
      attachSession: noop,
      goHome: noop,
      forkSession: noop,
      closeSession: noop,
      deleteSession: noop,
      startNes: noop,
      openNesDocument: noop,
      changeNesDocument: noop,
      saveNesDocument: noop,
      focusNesDocument: noop,
      suggestNes: noop,
      rejectNesSuggestion: noop,
      acceptNesSuggestion: noop,
      closeNesDocument: noop,
      closeNes: noop,
      selectNesDocument: noop,
    }),
  };
});

import App from "../web/src/app";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("application shell interaction", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(async () => {
    authScenario.logoutOnly = false;
    authScenario.logout.mockReset();
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    await act(async () => root.render(<App />));
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("offers negotiated logout even when sign-in is managed outside ACP", async () => {
    authScenario.logoutOnly = true;
    await act(async () => root.render(<App />));
    const logout = container.querySelector<HTMLButtonElement>(".agent-auth-logout");
    expect(logout).not.toBeNull();
    expect(logout?.disabled).toBe(false);
    expect(container.querySelector('[aria-label^="Authenticate with"]')).toBeNull();
    vi.stubGlobal("confirm", vi.fn(() => true));
    await act(async () => logout?.click());
    expect(authScenario.logout).toHaveBeenCalledOnce();
  });

  it("renders the project browser without a sidebar or drawer controls", () => {
    expect(container.querySelectorAll("main")).toHaveLength(1);
    expect(container.querySelector("aside, .sidebar, .mobile-menu, .sidebar-overlay, .topbar")).toBeNull();
    expect(container.querySelector('[aria-label="Open sidebar"]')).toBeNull();
    expect(container.querySelector('[aria-label="Close sidebar"]')).toBeNull();
    expect(container.querySelector('[aria-label="Dismiss sidebar overlay"]')).toBeNull();
    expect(container.querySelector("h1")?.textContent).toBe("Projects");
  });

  it("closes Agent settings on Escape and restores focus to its trigger", async () => {
    const trigger = container.querySelector<HTMLElement>('summary[aria-label="Agent settings"]');
    if (!trigger) throw new Error("Missing Agent settings trigger");
    await act(async () => trigger.click());
    const settings = container.querySelector<HTMLDetailsElement>(".agent-details");
    expect(settings?.open).toBe(true);

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key: "Escape" }));
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    });
    expect(settings?.open).toBe(false);
    expect(document.activeElement).toBe(trigger);
  });

  it("exposes one primary project action on the homepage", () => {
    expect(container.querySelectorAll("button.project-new")).toHaveLength(1);
    expect(container.querySelector("button.project-new")?.textContent).toBe("New project");
    expect(container.querySelector('button[aria-label="New thread"]')).toBeNull();
    expect(container.querySelector(".page-back")).toBeNull();
  });

  it("opens project creation with the default workspace and restores its action on cancellation", async () => {
    const trigger = container.querySelector<HTMLButtonElement>("button.project-new");
    if (!trigger) throw new Error("Missing session action");
    trigger.focus();
    await act(async () => trigger.click());
    expect(container.querySelector('[role="dialog"][aria-labelledby="new-thread-title"]')).toBeTruthy();
    expect(container.querySelector("#new-thread-title")?.textContent).toBe("New project");
    expect(container.querySelector('button[type="submit"]')?.textContent).toBe("Create project");
    expect(container.querySelector<HTMLInputElement>("#new-thread-cwd")?.value)
      .toBe("/workspace/attyd");
    expect(document.activeElement).toBe(container.querySelector("#new-thread-cwd"));
    await act(async () => {
      requireButton(container, "Cancel new project").click();
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    });
    expect(container.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });
});

function requireButton(container: ParentNode, label: string): HTMLButtonElement {
  const button = container.querySelector<HTMLButtonElement>(
    `button[aria-label="${label}"]`,
  );
  if (!button) throw new Error(`Missing button: ${label}`);
  return button;
}
