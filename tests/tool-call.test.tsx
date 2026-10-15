// @vitest-environment happy-dom

import type { ToolCall } from "@agentclientprotocol/sdk";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { TerminalSnapshot } from "../shared/bridge";
import { ToolCallCard } from "../web/src/components/acp/tool-call";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("ACP tool output presentation", () => {
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

  async function render(call: Partial<ToolCall>, terminalSnapshots: TerminalSnapshot[] = []) {
    await act(async () => root.render(<ToolCallCard item={{
      id: "tool:fixture",
      type: "tool",
      call: { toolCallId: "fixture", title: "Inspect workspace", ...call },
      raw: [],
    }} terminalSnapshots={terminalSnapshots} />));
    return element<HTMLElement>(container, ".tool-card");
  }

  it("renders mixed content in Agent order without replacing it with raw output", async () => {
    const card = await render({
      status: "completed",
      content: [
        { type: "content", content: { type: "text", text: "First explanation" } },
        { type: "diff", path: "/workspace/app.ts", oldText: "before", newText: "after" },
        { type: "terminal", terminalId: "private-terminal-id" },
        { type: "content", content: { type: "text", text: "Final explanation" } },
      ],
      rawOutput: { extra: "structured-result" },
    }, [snapshot({ output: "terminal-result", exitStatus: { exitCode: 0 }, released: true })]);

    await click(element(card, ".tool-disclosure"));
    const output = element(card, ".tool-output");
    const text = output.textContent ?? "";
    const positions = ["First explanation", "/workspace/app.ts", "terminal-result", "Final explanation"]
      .map((value) => text.indexOf(value));
    expect(positions.every((position) => position >= 0)).toBe(true);
    expect(positions).toEqual([...positions].sort((left, right) => left - right));
    expect(output.querySelector(".terminal-embed")?.textContent).toContain("terminal-result");
    expect(output.querySelector(".diff-card")).not.toBeNull();

    const additional = additionalOutput(output);
    expect(additional.open).toBe(false);
    expect(additional.textContent).toContain("structured-result");
    await click(element(additional, "summary"));
    expect(additional.open).toBe(true);
    expect(output.querySelector(".terminal-embed")?.textContent).toContain("terminal-result");
  });

  it.each([
    { rawOutput: 0, expected: "0" },
    { rawOutput: false, expected: "false" },
    { rawOutput: "", expected: "Empty string" },
    { rawOutput: null, expected: "null" },
  ])("preserves raw output $rawOutput with and without content", async ({ rawOutput, expected }) => {
    const card = await render({ status: "completed", rawOutput });
    await click(element(card, ".tool-disclosure"));
    expect(element(card, ".tool-output .structured-scalar").textContent).toBe(expected);
    expect(card.querySelector(".tool-output details")).toBeNull();

    await render({
      status: "completed",
      rawOutput,
      content: [{ type: "content", content: { type: "text", text: "Agent result" } }],
    });
    const additional = additionalOutput(element(card, ".tool-output"));
    expect(additional.open).toBe(false);
    expect(element(additional, ".structured-scalar").textContent).toBe(expected);
    expect(element(card, ".tool-output .structured-markdown").textContent).toBe("Agent result");
  });

  it("retains the user's disclosure choices across tool and terminal updates", async () => {
    const content: ToolCall["content"] = [{ type: "terminal", terminalId: "private-terminal-id" }];
    const card = await render({ status: "in_progress", content, rawOutput: { progress: 1 } }, [snapshot()]);
    await click(element(card, ".tool-disclosure"));
    const additional = additionalOutput(card);
    await click(element(additional, "summary"));
    await render({ status: "completed", content, rawOutput: { progress: 2 } }, [
      snapshot({ output: "finished", exitStatus: { exitCode: 0 }, released: true }),
    ]);
    expect(card.dataset.open).toBe("true");
    expect(additionalOutput(card)).toBe(additional);
    expect(additional.open).toBe(true);
    expect(element(card, ".terminal-embed").textContent).toContain("finished");

    await click(element(card, ".tool-disclosure"));
    await render({ status: "completed", content, rawOutput: { progress: 3 } }, [
      snapshot({ output: "finished", exitStatus: { exitCode: 0 }, released: true }),
    ]);
    expect(card.dataset.open).toBe("false");
  });

  it("keeps content annotations, terminal identifiers, and locations in Tool info", async () => {
    const annotations = { audience: ["assistant" as const], priority: 0.75, lastModified: "2026-09-08T01:00:00Z" };
    const card = await render({
      status: "completed",
      locations: [{ path: "/workspace/metadata-only.ts", line: 0 }],
      content: [
        { type: "content", content: { type: "text", text: "Visible result", annotations } },
        { type: "terminal", terminalId: "private-terminal-id" },
      ],
    }, [snapshot({ output: "Visible terminal output", exitStatus: { exitCode: 0 }, released: true })]);
    await click(element(card, ".tool-disclosure"));
    const output = element(card, ".tool-output");
    expect(output.textContent).toContain("Visible result");
    expect(output.textContent).toContain("Visible terminal output");
    expect(output.textContent).not.toContain("private-terminal-id");
    expect(output.textContent).not.toContain("metadata-only.ts");
    expect(output.querySelector(".content-annotations")).toBeNull();
    expect(card.querySelector(".locations")).toBeNull();

    const info = element<HTMLElement>(card, '[aria-label="Tool debug information"]');
    expect(info.hidden).toBe(true);
    await click(element(card, 'button[aria-label="Tool info"]'));
    expect(info.hidden).toBe(false);
    expect(info.textContent).toContain("private-terminal-id");
    expect(info.textContent).toContain("/workspace/metadata-only.ts");
    expect(info.textContent).toContain('"line": 0');
    expect(info.textContent).toContain('"priority": 0.75');
    expect(info.textContent).toContain(annotations.lastModified);
  });

  it.each([
    { status: undefined, expected: "Waiting for the tool…" },
    { status: "pending" as const, expected: "Waiting for the tool…" },
    { status: "in_progress" as const, expected: "Waiting for output…" },
    { status: "completed" as const, expected: "Completed without output." },
    { status: "failed" as const, expected: "No error details were provided." },
  ])("explains absent output for $status tools", async ({ status, expected }) => {
    const card = await render({ status, content: [] });
    await click(element(card, ".tool-disclosure"));
    const output = element(card, ".tool-output");
    expect(output.querySelector("header")?.textContent).toBe("Output");
    expect(output.textContent).toContain(expected);
  });

  it.each(["", " \n\t "])("treats blank text %j as absent output while preserving metadata", async (text) => {
    const content: ToolCall["content"] = [{
      type: "content",
      content: { type: "text", text, annotations: { audience: ["assistant"], priority: 0.5 } },
    }];
    const card = await render({ status: "failed", content });
    await click(element(card, ".tool-disclosure"));
    const output = element(card, ".tool-output");
    expect(output.querySelector(".content-block-text")).toBeNull();
    expect(output.textContent).toContain("No error details were provided.");
    await click(element(card, 'button[aria-label="Tool info"]'));
    const info = element<HTMLElement>(card, '[aria-label="Tool debug information"]');
    expect(info.hidden).toBe(false);
    expect(info.textContent).toContain("Content annotations");
    expect(info.textContent).toContain('"priority": 0.5');

    await render({ status: "failed", content, rawOutput: 0 });
    expect(element(output, ":scope > .structured-scalar").textContent).toBe("0");
    expect(output.querySelector(".content-block-text")).toBeNull();
    expect(output.querySelector("details")).toBeNull();
    expect(output.textContent).not.toContain("No error details were provided.");
  });

  it.each([
    { terminal: snapshot({ exitStatus: { exitCode: 0 } }), state: "Completed", text: "No output" },
    { terminal: snapshot({ exitStatus: { exitCode: 7 } }), state: "Failed (exit 7)", text: "No output" },
    { terminal: snapshot({ exitStatus: { signal: "SIGTERM" } }), state: "Stopped (SIGTERM)", text: "No output" },
    { terminal: snapshot({ exitStatus: {} }), state: "Ended", text: "No output" },
    { terminal: snapshot({ released: true }), state: "Stopped", text: "No output" },
    { terminal: snapshot(), state: "Running", text: "Waiting for terminal output…" },
    { terminal: undefined, state: "Unavailable", text: "Terminal output is unavailable." },
  ])("reports terminal $state without a misleading waiting message", async ({ terminal, state, text }) => {
    const card = await render({
      content: [{ type: "terminal", terminalId: "private-terminal-id" }],
    }, terminal ? [terminal] : []);
    await click(element(card, ".tool-disclosure"));
    const output = element(card, ".terminal-embed");
    expect(output.querySelector(".terminal-heading")?.textContent).toBe(`Terminal${state}`);
    expect(output.textContent).toContain(text);
    expect(output.textContent).not.toContain("private-terminal-id");
  });

  it("preserves released terminal output and reports truncation", async () => {
    const card = await render({
      status: "completed",
      content: [{ type: "terminal", terminalId: "private-terminal-id" }],
    }, [snapshot({ output: "retained output", exitStatus: { exitCode: 0 }, released: true, truncated: true })]);
    await click(element(card, ".tool-disclosure"));
    const terminal = element(card, ".terminal-embed");
    expect(terminal.querySelector("pre")?.textContent).toBe("retained output");
    expect(terminal.textContent).toContain("Earlier output was truncated.");
    expect(terminal.textContent).not.toContain("Waiting");
  });

  it("marks only changed diff lines and preserves context", async () => {
    const card = await render({
      status: "completed",
      content: [{
        type: "diff",
        path: "/workspace/app.ts",
        oldText: "unchanged\nbefore\nshared tail\n",
        newText: "unchanged\nafter\nshared tail\n",
      }],
    });
    await click(element(card, ".tool-disclosure"));
    const diff = element<HTMLDetailsElement>(card, ".diff-card");
    await click(element(diff, "summary"));
    expect([...diff.querySelectorAll(".line-context code")].map((line) => line.textContent))
      .toEqual(["unchanged", "shared tail"]);
    expect([...diff.querySelectorAll(".line-removed code")].map((line) => line.textContent)).toEqual(["before"]);
    expect([...diff.querySelectorAll(".line-added code")].map((line) => line.textContent)).toEqual(["after"]);
    expect(diff.querySelector(".line-removed > span:nth-child(3)")?.textContent).toBe("−");
    expect(diff.querySelector(".line-added > span:nth-child(3)")?.textContent).toBe("+");
  });

  it("keeps new empty files and empty replacement content inspectable", async () => {
    const card = await render({
      status: "completed",
      content: [
        { type: "diff", path: "/workspace/new.txt", oldText: null, newText: "" },
        { type: "diff", path: "/workspace/cleared.txt", oldText: "removed\n", newText: "" },
      ],
    });
    await click(element(card, ".tool-disclosure"));
    const diffs = [...card.querySelectorAll<HTMLDetailsElement>(".diff-card")];
    expect(diffs).toHaveLength(2);
    expect(diffs[0].querySelector("summary")?.textContent).toContain("New file");
    expect(diffs[0].textContent).toContain("Empty file.");
    expect(diffs[1].querySelector(".line-removed code")?.textContent).toBe("removed");
    expect(diffs[1].querySelector(".line-added")).toBeNull();
  });
});

function snapshot(overrides: Partial<TerminalSnapshot> = {}): TerminalSnapshot {
  return {
    sessionId: "session",
    terminalId: "private-terminal-id",
    output: "",
    truncated: false,
    released: false,
    ...overrides,
  };
}

function element<T extends Element = HTMLElement>(container: ParentNode, selector: string): T {
  const result = container.querySelector<T>(selector);
  if (!result) throw new Error(`Missing tool element: ${selector}`);
  return result;
}

function additionalOutput(container: ParentNode): HTMLDetailsElement {
  const details = [...container.querySelectorAll<HTMLDetailsElement>("details")]
    .find((candidate) => candidate.querySelector("summary")?.textContent?.trim() === "Additional output");
  if (!details) throw new Error("Missing Additional output disclosure");
  return details;
}

async function click(target: HTMLElement) {
  await act(async () => target.click());
}
