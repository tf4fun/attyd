// @vitest-environment happy-dom

import type { PlanEntry, SessionUpdate } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { PlanCard } from "../web/src/components/acp/plan";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("plan card disclosure", () => {
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

  async function render(update: SessionUpdate, entryId = "plan:current") {
    await act(async () => root.render(<PlanCard entryId={entryId} update={update} />));
  }

  it("places progress with the left title and collapses task details from the right button", async () => {
    await render(plan());
    const heading = element<HTMLElement>(container, ".plan-heading");
    const title = element<HTMLButtonElement>(heading, ".plan-disclosure");
    expect(title.textContent).toBe("Plan1/3");
    const toggle = element<HTMLButtonElement>(heading, ".component-disclosure-button");
    expect(heading.lastElementChild).toBe(toggle);
    expect(toggle.getAttribute("aria-label")).toBe("Collapse plan details");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const body = element<HTMLElement>(container, ".plan-body");
    expect(toggle.getAttribute("aria-controls")).toBe(body.id);
    expect(body.hidden).toBe(false);
    expect(body.querySelectorAll("li")).toHaveLength(3);

    await act(async () => toggle.click());
    expect(body.hidden).toBe(true);
    expect(toggle.getAttribute("aria-label")).toBe("Expand plan details");
    expect(title.getAttribute("aria-expanded")).toBe("false");
    expect(title.textContent).toBe("Plan1/3");

    await act(async () => title.click());
    expect(body.hidden).toBe(false);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });

  it("keeps a user's collapsed choice while progress and task content update", async () => {
    await render(plan());
    await act(async () => element<HTMLButtonElement>(container, ".component-disclosure-button").click());
    await render(plan([
      { content: "Read workspace", priority: "high", status: "completed" },
      { content: "Implement changes", priority: "high", status: "completed" },
      { content: "Run updated tests", priority: "medium", status: "in_progress" },
      { content: "Review results", priority: "low", status: "pending" },
    ]));
    expect(element<HTMLElement>(container, ".plan-body").hidden).toBe(true);
    expect(element(container, ".plan-progress").textContent).toBe("2/4");
    await act(async () => element<HTMLButtonElement>(container, ".component-disclosure-button").click());
    expect(element<HTMLElement>(container, ".plan-body").hidden).toBe(false);
    expect(container.querySelectorAll("li")).toHaveLength(4);
    expect(container.textContent).toContain("Run updated tests");
  });

  it("starts a different plan expanded instead of inheriting another plan's disclosure state", async () => {
    await render(plan());
    await act(async () => element<HTMLButtonElement>(container, ".component-disclosure-button").click());
    await render(plan(), "plan:next");
    expect(element<HTMLElement>(container, ".plan-body").hidden).toBe(false);
    expect(element(container, ".component-disclosure-button").getAttribute("aria-expanded")).toBe("true");
  });

  it.each([
    { type: "file" as const, planId: "named", uri: "file:///workspace/plan.md" },
    { type: "markdown" as const, planId: "named", content: "## Follow-up tasks" },
    { type: "items" as const, planId: "named", entries: [] },
  ])("collapses $type plans using the same accessible controls", async (update) => {
    await render({ sessionUpdate: "plan_update", plan: update });
    const toggle = element<HTMLButtonElement>(container, ".component-disclosure-button");
    const body = element<HTMLElement>(container, ".plan-body");
    expect(body.hidden).toBe(false);
    await act(async () => toggle.click());
    expect(body.hidden).toBe(true);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
  });

  it("keeps plan removal visible without an empty disclosure", async () => {
    await render({ sessionUpdate: "plan_removed", planId: "named" });
    expect(container.textContent?.trim()).toBe("Plan named removed");
    expect(container.querySelector("button")).toBeNull();
  });
});

function plan(entries: PlanEntry[] = [
  { content: "Read workspace", priority: "high", status: "completed" },
  { content: "Implement changes", priority: "high", status: "in_progress" },
  { content: "Run tests", priority: "medium", status: "pending" },
]): SessionUpdate {
  return { sessionUpdate: "plan", entries };
}

function element<T extends Element = HTMLElement>(container: ParentNode, selector: string): T {
  const result = container.querySelector<T>(selector);
  if (!result) throw new Error(`Missing element: ${selector}`);
  return result;
}
