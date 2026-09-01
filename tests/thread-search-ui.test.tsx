// @vitest-environment happy-dom

import { act, createRef, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ThreadSearchBar,
  scanThreadSearchDom,
} from "../web/src/components/acp/thread-search";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const defaults = { caseSensitive: false, wholeWord: false, regex: false };

describe("Zed-style Agent thread search UI", () => {
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

  it("searches visible ACP entry content while ignoring raw payloads and collapsed bodies", () => {
    const thread = document.createElement("div");
    thread.innerHTML = `
      <article data-thread-entry-id="message">
        <div data-thread-searchable>Hello <strong>world</strong></div>
        <details data-thread-search-ignore open><summary>payload</summary><pre>world</pre></details>
      </article>
      <details data-thread-entry-id="closed-tool">
        <summary><span data-thread-searchable>Inspect workspace</span></summary>
        <div data-thread-searchable>hidden needle</div>
      </details>
      <details data-thread-entry-id="open-tool" open>
        <summary><span data-thread-searchable>Run tests</span></summary>
        <div data-thread-searchable>visible needle</div>
      </details>
      <section data-thread-entry-id="custom-collapsed">
        <button data-thread-searchable>Structured tool</button>
        <div data-thread-searchable hidden>custom hidden needle</div>
      </section>
    `;

    const world = scanThreadSearchDom(thread, "world", defaults);
    expect(world.matches).toHaveLength(1);
    expect(world.matches[0]?.entry.dataset.threadEntryId).toBe("message");
    expect(world.matches[0]?.range?.toString()).toBe("world");

    const needle = scanThreadSearchDom(thread, "needle", defaults);
    expect(needle.matches).toHaveLength(1);
    expect(needle.matches[0]?.entry.dataset.threadEntryId).toBe("open-tool");
  });

  it("navigates matches, toggles options, and restores control through close", async () => {
    const threadRef = createRef<HTMLDivElement>();
    const searchRef = createRef<HTMLDivElement>();
    const onClose = vi.fn();
    await render(root, (
      <>
        <ThreadSearchBar
          rootRef={threadRef}
          containerRef={searchRef}
          contentVersion={0}
          terminalVersion={0}
          focusRequest={0}
          onClose={onClose}
        />
        <div ref={threadRef}>
          <article data-thread-entry-id="one"><div data-thread-searchable>Alpha alpha</div></article>
          <article data-thread-entry-id="two"><div data-thread-searchable>alpha</div></article>
        </div>
      </>
    ));

    const input = requireElement<HTMLInputElement>(container.querySelector('input[type="search"]'));
    await replaceInput(input, "alpha");
    expect(container.querySelector("output")?.textContent).toBe("1/3");
    expect(container.querySelector('[data-thread-entry-id="one"]')?.getAttribute("data-thread-search-active"))
      .toBe("true");

    await press(input, "Enter");
    expect(container.querySelector("output")?.textContent).toBe("2/3");
    await click(requireElement(container.querySelector('button[aria-label="Match case"]')));
    expect(container.querySelector("output")?.textContent).toBe("1/2");
    expect(requireElement<HTMLButtonElement>(container.querySelector('button[aria-label="Match case"]'))
      .getAttribute("aria-pressed")).toBe("true");

    await press(input, "Escape");
    expect(onClose).toHaveBeenCalledOnce();
  });
});

async function render(root: Root, node: ReactNode): Promise<void> {
  await act(async () => root.render(node));
}

async function replaceInput(element: HTMLInputElement, value: string): Promise<void> {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(element, value);
    element.dispatchEvent(new InputEvent("input", { bubbles: true, data: value }));
  });
}

async function press(element: HTMLElement, key: string, shiftKey = false): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new KeyboardEvent("keydown", { bubbles: true, key, shiftKey }));
  });
}

async function click(element: Element): Promise<void> {
  await act(async () => {
    element.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

function requireElement<T extends Element>(element: T | null): T {
  if (!element) throw new Error("Expected UI element was not rendered");
  return element;
}
