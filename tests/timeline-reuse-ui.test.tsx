// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Conversation } from "../web/src/components/acp/conversation";
import { projectEntries } from "../web/src/lib/entry-projection";
import type { Entry } from "../shared/entries";

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

const message = (role: "user" | "agent", text: string): Entry => ({
  kind: "message", role, chunks: [{ content: { type: "text", text } }],
});

describe("a thread update", () => {
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

  it("renders only the entries it changed", async () => {
    const render = (entries: Entry[]) => act(async () => root.render(
      // Fresh handlers, terminals and activity each time, as the page passes them.
      <Conversation
        timeline={projectEntries("S", entries, [{ entry: 0 }]).timeline}
        terminalSnapshots={[]}
        agentActivity={{ kind: "responding" }}
        onReusePrompt={() => {}}
        onRetryPrompt={() => {}}
        onNavigateThread={() => {}}
        onOpenThreadMarkdown={() => {}}
      />,
    ));
    const entries = [message("user", "question"), message("agent", "first answer")];
    await render(entries);
    expect(parsed).toEqual(expect.arrayContaining(["question", "first answer"]));

    parsed.length = 0;
    await render([...entries, { kind: "tool", call: { toolCallId: "t1", title: "Run" } }, message("agent", "second answer")]);
    expect(parsed).toEqual(["second answer"]);
    expect(container.textContent).toContain("first answer");
    expect(container.textContent).toContain("second answer");
  });

  it("calls the handlers the page passes last", async () => {
    const first = vi.fn();
    const latest = vi.fn();
    const entries = [message("user", "question"), message("agent", "answer")];
    const timeline = projectEntries("S", entries, [{ entry: 0, end: { response: { stopReason: "end_turn" } } }]).timeline;
    for (const onReusePrompt of [first, latest]) {
      await act(async () => root.render(
        <Conversation timeline={timeline} canReusePrompt onReusePrompt={onReusePrompt} settled />,
      ));
    }
    const edit = container.querySelector<HTMLButtonElement>('[aria-label="Edit and resend user message"]');
    if (edit == null) throw new Error("no edit button");
    await act(async () => edit.click());
    expect(first).not.toHaveBeenCalled();
    expect(latest).toHaveBeenCalledWith([{ type: "text", text: "question" }]);
  });
});
