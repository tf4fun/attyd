// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it, vi } from "vitest";
import { NewSessionDialog } from "../web/src/components/acp/new-session-dialog";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("new Agent thread workspace", () => {
  it("leaves remote workspaces blank and submits an absolute Agent-host path", async () => {
    const container = document.createElement("div");
    document.body.append(container);
    const root = createRoot(container);
    const onCreate = vi.fn(() => true);

    await act(async () => root.render(
      <NewSessionDialog
        transport="ws"
        defaultCwd="/local/attyd/path"
        onCancel={() => undefined}
        onCreate={onCreate}
      />,
    ));

    const input = container.querySelector<HTMLInputElement>("#new-thread-cwd");
    if (!input) throw new Error("Missing Agent workspace input");
    expect(container.querySelector("h2")?.textContent).toBe("New thread");
    expect(container.querySelector("label")?.textContent).toBe("Agent workspace");
    expect(container.querySelector('button[type="submit"]')?.textContent).toBe("Create thread");
    expect(input.value).toBe("");
    expect(container.textContent).toContain("remote Agent host");

    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setter?.call(input, "/home/xxl/gugugaga");
      input.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        data: "/home/xxl/gugugaga",
      }));
    });
    const form = container.querySelector<HTMLFormElement>("form");
    if (!form) throw new Error("Missing new-thread form");
    await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    expect(onCreate).toHaveBeenCalledWith("/home/xxl/gugugaga");

    await act(async () => root.unmount());
    container.remove();
  });

  it("creates a project's first session from a validated working directory", async () => {
    const container = document.createElement("div");
    document.body.append(container);
    const root = createRoot(container);
    const onCreate = vi.fn(() => true);
    try {
      await act(async () => root.render(
        <NewSessionDialog
          transport="stdio"
          defaultCwd="/workspace/default"
          purpose="project"
          onCancel={() => undefined}
          onCreate={onCreate}
        />,
      ));
      const input = container.querySelector<HTMLInputElement>("#new-thread-cwd");
      const form = container.querySelector<HTMLFormElement>("form");
      if (!input || !form) throw new Error("Missing project form");
      expect(container.querySelector("h2")?.textContent).toBe("New project");
      expect(container.querySelector("label")?.textContent).toBe("Project working directory");
      expect(container.querySelector("header p")?.textContent).toContain("first session");
      expect(container.querySelector('button[type="submit"]')?.textContent).toBe("Create project");
      expect(input.value).toBe("/workspace/default");
      expect(document.activeElement).toBe(input);

      await replaceInput(input, "relative/project");
      await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
      expect(onCreate).not.toHaveBeenCalled();
      expect(input.getAttribute("aria-invalid")).toBe("true");
      expect(container.textContent).toContain("Enter an absolute working directory.");

      await replaceInput(input, "  /workspace/new project  ");
      await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
      expect(onCreate).toHaveBeenCalledWith("/workspace/new project");
      expect(input.hasAttribute("aria-invalid")).toBe(false);
    } finally {
      await act(async () => root.unmount());
      container.remove();
    }
  });

  it("keeps disabled project creation blocked and exposes a project cancel action", async () => {
    const container = document.createElement("div");
    document.body.append(container);
    const root = createRoot(container);
    const onCreate = vi.fn(() => true);
    const onCancel = vi.fn();
    try {
      await act(async () => root.render(
        <NewSessionDialog
          transport="stdio"
          defaultCwd="/workspace/project"
          purpose="project"
          disabled
          onCancel={onCancel}
          onCreate={onCreate}
        />,
      ));
      expect(container.querySelector<HTMLButtonElement>('button[type="submit"]')?.disabled).toBe(true);
      const form = container.querySelector<HTMLFormElement>("form");
      const cancel = container.querySelector<HTMLButtonElement>('button[aria-label="Cancel new project"]');
      if (!form || !cancel) throw new Error("Missing project controls");
      await act(async () => form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
      expect(onCreate).not.toHaveBeenCalled();
      await act(async () => cancel.click());
      expect(onCancel).toHaveBeenCalledOnce();
    } finally {
      await act(async () => root.unmount());
      container.remove();
    }
  });
});

async function replaceInput(input: HTMLInputElement, value: string): Promise<void> {
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(input, value);
    input.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
  });
}
