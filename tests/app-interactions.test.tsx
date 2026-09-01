// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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
      },
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
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    await act(async () => root.render(<App />));
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("moves focus into the mobile drawer and restores it on Escape", async () => {
    const open = requireButton(container, "Open sidebar");
    expect(open.getAttribute("aria-controls")).toBe("app-sidebar");
    expect(open.getAttribute("aria-expanded")).toBe("false");

    await act(async () => open.click());
    const close = requireButton(container, "Close sidebar");
    expect(document.activeElement).toBe(close);
    expect(open.getAttribute("aria-expanded")).toBe("true");
    expect(requireButton(container, "Dismiss sidebar overlay")).toBeTruthy();

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key: "Escape" }));
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    });
    expect(open.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(open);
    expect(container.querySelector('[aria-label="Dismiss sidebar overlay"]')).toBeNull();
  });

  it("exposes one unambiguous new-thread action", () => {
    expect(container.querySelectorAll('button[aria-label="New thread"]')).toHaveLength(1);
    expect(container.querySelector('button[aria-label="New session"]')).toBeNull();
  });

  it("asks for the Agent workspace and pre-fills the stdio default", async () => {
    await act(async () => requireButton(container, "New thread").click());
    expect(container.querySelector('[role="dialog"][aria-labelledby="new-thread-title"]')).toBeTruthy();
    expect(container.querySelector<HTMLInputElement>("#new-thread-cwd")?.value)
      .toBe("/workspace/attyd");
    expect(document.activeElement).toBe(container.querySelector("#new-thread-cwd"));
  });
});

function requireButton(container: ParentNode, label: string): HTMLButtonElement {
  const button = container.querySelector<HTMLButtonElement>(
    `button[aria-label="${label}"]`,
  );
  if (!button) throw new Error(`Missing button: ${label}`);
  return button;
}
