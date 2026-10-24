// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ToolCall } from "@agentclientprotocol/sdk";
import { ContentBlockView } from "../web/src/components/acp/content-block";
import { ToolCallCard } from "../web/src/components/acp/tool-call";

const parsed = vi.hoisted(() => [] as string[]);

vi.mock("react-markdown", async (importOriginal) => {
  const { default: Markdown } = await importOriginal<typeof import("react-markdown")>();
  return {
    default: (props: Parameters<typeof Markdown>[0]) => {
      parsed.push(String(props.children));
      return Markdown(props);
    },
  };
});

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("Markdown rendering", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    parsed.length = 0;
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("parses unchanged message text once across re-renders", async () => {
    for (const presentation of ["message", "tool", "prompt"] as const) {
      parsed.length = 0;
      // A new block with the same text, as each timeline projection makes.
      for (let round = 0; round < 3; round += 1) {
        await act(async () => root.render(
          <ContentBlockView block={{ type: "text", text: `**${presentation}** text` }} presentation={presentation} />,
        ));
      }
      expect(parsed).toEqual([`**${presentation}** text`]);
      expect(container.querySelector("strong")?.textContent).toBe(presentation);
    }

    await act(async () => root.render(<ContentBlockView block={{ type: "text", text: "changed" }} />));
    expect(parsed.at(-1)).toBe("changed");
    expect(container.textContent).toContain("changed");
  });

  it("parses a tool's output only while it is open, once across updates", async () => {
    const render = (call: Partial<ToolCall>) => act(async () => root.render(<ToolCallCard item={{
      id: "tool:fixture",
      type: "tool",
      call: {
        toolCallId: "fixture",
        title: "Search",
        content: [{ type: "content", content: { type: "text", text: "| a |\n| - |\n| 1 |" } }],
        ...call,
      },
      raw: [],
    }} />));
    await render({ status: "in_progress" });
    await render({ status: "completed" });
    const card = container.querySelector<HTMLElement>(".tool-card")!;
    const body = card.querySelector<HTMLElement>(".tool-body")!;
    expect(body.hidden).toBe(true);
    expect(body.childElementCount).toBe(0);
    expect(parsed).toEqual([]);

    await act(async () => card.querySelector<HTMLButtonElement>(".tool-disclosure")!.click());
    expect(body.hidden).toBe(false);
    expect(body.querySelector(".tool-output table")).not.toBeNull();
    await render({ status: "completed", rawOutput: { done: true } });
    expect(parsed).toEqual(["| a |\n| - |\n| 1 |"]);
  });
});
