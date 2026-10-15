// @vitest-environment happy-dom

// Interactions asked outside a shown thread stay answerable: a catalog list
// may ask while a cold session waits for its cwd, a global operation while
// the user browses.
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const scenario = vi.hoisted(() => ({
  opening: true,
  respondElicitation: vi.fn(),
  respondPermission: vi.fn(),
}));

vi.mock("../web/src/lib/use-acp", async () => {
  const { initialState } = await import("../web/src/lib/state");
  const noop = () => undefined;
  return {
    useAcp: () => ({
      state: {
        ...initialState,
        phase: "ready",
        defaultCwd: "/workspace",
        cwd: "",
        elicitations: [{
          elicitationId: "E",
          request: { mode: "form", message: "Choose a catalog filter", requestedSchema: { type: "object", properties: {} } },
        }],
        permissions: [{
          permissionId: "P",
          request: {
            sessionId: "s",
            toolCall: { toolCallId: "t", title: "Global check" },
            options: [{ optionId: "allow", name: "Allow once", kind: "allow_once" }],
          },
        }],
      },
      sessionOpening: scenario.opening ? { sessionId: "cold" } : undefined,
      respondElicitation: scenario.respondElicitation,
      respondPermission: scenario.respondPermission,
      retrySessionOpening: noop,
      listSessions: noop,
      goHome: noop,
      openProject: noop,
    }),
  };
});

import App from "../web/src/app";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("interactions outside a shown thread", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    scenario.respondElicitation.mockReset();
    scenario.respondPermission.mockReset();
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  for (const opening of [true, false]) {
    it(`shows and answers them ${opening ? "while a session opens" : "while browsing"}`, async () => {
      scenario.opening = opening;
      await act(async () => root.render(<App />));
      expect(container.querySelector(".session-opening-panel") != null).toBe(opening);
      const form = container.querySelector<HTMLElement>('[role="dialog"][aria-label="Agent input request"]');
      expect(form?.textContent).toContain("Choose a catalog filter");
      const permission = container.querySelector<HTMLElement>('[role="alertdialog"][aria-label="Agent permission request"]');
      expect(permission?.textContent).toContain("Global check");
      const allow = [...permission!.querySelectorAll("button")].find((button) => button.textContent === "Allow once")!;
      await act(async () => allow.click());
      expect(scenario.respondPermission).toHaveBeenCalledWith("P", { outcome: "selected", optionId: "allow" });
      const submit = [...form!.querySelectorAll("button")].find((button) => button.textContent === "Submit")!;
      await act(async () => submit.click());
      expect(scenario.respondElicitation).toHaveBeenCalledWith("E", { action: "accept", content: {} });
    });
  }
});
