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
});
