// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SessionActionDialog } from "../web/src/components/acp/session-action-dialog";
import { ContentBlockView } from "../web/src/components/acp/content-block";
import { Conversation } from "../web/src/components/acp/conversation";
import { SessionControls } from "../web/src/components/acp/session-controls";
import { attachmentHref } from "../web/src/lib/hosted-attachment";
import type { ContentBlock } from "@agentclientprotocol/sdk";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

describe("ACP presentation boundaries", () => {
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
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  function mockPromptHeight(height: number) {
    const getStyle = globalThis.getComputedStyle;
    vi.spyOn(globalThis, "getComputedStyle").mockImplementation((element) => {
      if (!element.classList.contains("prompt-text-body")) return getStyle(element);
      const style = document.createElement("div").style;
      style.lineHeight = "24px";
      style.setProperty("--prompt-preview-lines", "9");
      return style;
    });
    const original = HTMLElement.prototype.getBoundingClientRect;
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
      return this.classList.contains("prompt-text-content")
        ? new DOMRect(0, 0, 800, height) : original.call(this);
    });
  }

  it.each(["close", "delete"] as const)("requires explicit %s confirmation and keeps focus on the safe action", async (action) => {
    const cancel = vi.fn();
    const confirm = vi.fn();
    await act(async () => root.render(<SessionActionDialog action={action} disabled={false} onCancel={cancel} onConfirm={confirm} />));
    const [keep, close] = container.querySelectorAll("button");
    expect(document.activeElement).toBe(keep);
    expect(container.textContent).toContain(action === "close" ? "child processes" : "cannot be undone");
    await act(async () => keep.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(cancel).toHaveBeenCalledOnce();
    expect(confirm).not.toHaveBeenCalled();
    await act(async () => keep.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true })));
    expect(document.activeElement).toBe(close);
    await act(async () => close.click());
    expect(confirm).toHaveBeenCalledOnce();
  });

  it("suppresses legacy modes even for an explicitly empty configOptions list", async () => {
    const props = {
      modes: { currentModeId: "chat", availableModes: [{ id: "chat", name: "Legacy chat" }] },
      disabled: false, onMode: vi.fn(), onConfig: vi.fn(),
    };
    await act(async () => root.render(<SessionControls {...props} options={[]} />));
    expect(container.textContent).toBe("");
    await act(async () => root.render(<SessionControls {...props} options={null} />));
    expect(container.textContent).toContain("Legacy chat");
    await act(async () => root.render(<SessionControls {...props} options={[
      { type: "boolean", id: "opaque", name: "Agent option", currentValue: false },
    ]} />));
    expect(container.textContent).toContain("Agent option");
    expect(container.textContent).not.toContain("Legacy chat");
  });

  it.each(["user", "protocol-user"] as const)("opens %s attachment references as compact native links", async (role) => {
    const attachments = [
      hosted("notes 中文.md", "text/markdown", 300), hosted("note.txt", "text/plain", 6),
      hosted("image.png", "image/png", 100), hosted("voice.wav", "audio/wav", 200),
      hosted("report.pdf", "application/pdf", 400), hosted("empty.txt", "text/plain", 0),
    ];
    await act(async () => root.render(<Conversation timeline={[{
      id: "prompt", type: "message", role, raw: [],
      blocks: [{ type: "text", text: "Review **these files**." }, ...attachments],
    }]} />));
    expect(container.querySelector(".prompt-text-body")?.textContent).toBe("Review these files.");
    expect(container.querySelector(".prompt-text-body strong")?.textContent).toBe("these files");
    const links = [...container.querySelectorAll<HTMLAnchorElement>(".attachment-chip")];
    expect(links).toHaveLength(attachments.length);
    expect(links[0].textContent).toContain("notes 中文.md");
    expect(links[1].textContent).toContain("6 B");
    expect(links[1].textContent).not.toContain("text/plain");
    expect(links[1].title).toContain("text/plain · 6 B");
    expect(links[5].textContent).toContain("0 B");
    for (const [index, link] of links.entries()) {
      expect(link.tagName).toBe("A");
      expect(link.getAttribute("href")).toBe(attachmentHref(attachments[index]));
      expect(link.target).toBe("_blank");
      expect(link.rel).toContain("noopener");
      expect(link.hasAttribute("download")).toBe(false);
      expect(link.querySelector("button, a")).toBeNull();
    }
    expect(container.querySelector(".message-content")?.querySelector("pre, img, audio, iframe, object, button")).toBeNull();
  });

  it.each(["user", "protocol-user"] as const)("renders folded %s Markdown without changing its source or prompt reuse", async (role) => {
    mockPromptHeight(1_000);
    const text = `Review this--- Resource: attyd://attachment/${encodeURIComponent("项目启动文档.md")} ---\n`
      + "# Project notes\n\n**Literal source**, including <script>tags</script>.\n".repeat(100)
      + "Final line of the document.";
    const blocks: ContentBlock[] = [{ type: "text", text }];
    const reuse = vi.fn();
    await act(async () => root.render(<Conversation timeline={[{
      id: "prompt", type: "message", role, raw: [], blocks,
    }]} canReusePrompt onReusePrompt={reuse} />));
    const body = container.querySelector(".prompt-text-body")!;
    const toggle = container.querySelector<HTMLButtonElement>(".prompt-text-toggle")!;
    expect(body.textContent).toContain("--- Resource: attyd://attachment/");
    expect(body.textContent).toContain("Final line of the document.");
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(body.querySelector("h1")?.textContent).toBe("Project notes");
    expect(body.querySelector("strong")?.textContent).toBe("Literal source");
    expect(body.querySelector("script")).toBeNull();
    await act(async () => toggle.click());
    expect(body.querySelectorAll("h1")).toHaveLength(100);
    expect(body.textContent).toContain("Final line of the document.");
    expect(body.textContent).toContain("<script>tags</script>");
    expect(body.querySelector("script")).toBeNull();
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    await act(async () => toggle.click());
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await act(async () => container.querySelector<HTMLButtonElement>('[aria-label="Edit and resend user message"]')!.click());
    expect(reuse).toHaveBeenCalledWith(blocks);
  });

  it.each(["message", "tool"] as const)("uses the same attachment link for %s output", async (presentation) => {
    await act(async () => root.render(<ContentBlockView presentation={presentation} block={hosted("result.md", "text/markdown", 20)} />));
    expect(container.querySelector("a.attachment-chip")?.textContent).toContain("result.md");
    expect(container.querySelector("pre, details")).toBeNull();
  });

  it("renders prompt lists, tables and fenced code when expanded", async () => {
    mockPromptHeight(400);
    const text = "## Review\n\n**Check the changes**\n\n- [x] Read the file\n- [ ] Run tests\n\n"
      + "| File | Result |\n| --- | --- |\n| app.ts | Ready |\n\n```ts\nconst enabled = true;\n```\n\nHidden continuation.";
    await act(async () => root.render(<ContentBlockView presentation="prompt" block={{ type: "text", text }} />));
    expect(container.querySelector("h2")?.textContent).toBe("Review");
    expect(container.querySelector("strong")?.textContent).toBe("Check the changes");
    expect(container.querySelector(".prompt-text-toggle")?.getAttribute("aria-expanded")).toBe("false");
    await act(async () => container.querySelector<HTMLButtonElement>(".prompt-text-toggle")!.click());
    expect(container.querySelectorAll('li input[type="checkbox"]')).toHaveLength(2);
    expect(container.querySelector("table td")?.textContent).toBe("app.ts");
    expect(container.querySelector("pre code")?.textContent).toBe("const enabled = true;\n");
    expect(container.querySelector(".prompt-text-body")?.textContent).toContain("Hidden continuation.");
    await act(async () => container.querySelector<HTMLButtonElement>(".prompt-text-toggle")!.click());
    expect(container.querySelector(".prompt-text-toggle")?.getAttribute("aria-expanded")).toBe("false");
  });

  it("preserves complete inline markup and later reference links in a prompt excerpt", async () => {
    mockPromptHeight(400);
    const emphasis = "完整保留这段加粗文字。".repeat(64);
    const text = `**${emphasis}** [使用说明][guide]\n\n## 后续内容\n\n展开后显示。\n\n[guide]: https://example.com/guide`;
    await act(async () => root.render(<ContentBlockView presentation="prompt" block={{ type: "text", text }} />));
    expect(container.querySelector("strong")?.textContent).toBe(emphasis);
    expect(container.querySelector("a")?.getAttribute("href")).toBe("https://example.com/guide");
    expect(container.querySelector(".prompt-text-toggle")?.getAttribute("aria-expanded")).toBe("false");
    await act(async () => container.querySelector<HTMLButtonElement>(".prompt-text-toggle")!.click());
    expect(container.querySelector("h2")?.textContent).toBe("后续内容");
  });

  it.each([
    { source: `\`\`\`ts\n${"// Keep this code block intact.\n".repeat(12)}const last = true;\n\`\`\``, selector: "pre code", ending: "const last = true;\n" },
    { source: `| File |\n| --- |\n${"| repeated.ts |\n".repeat(12)}| last.ts |`, selector: "tbody", ending: "last.ts" },
  ])("retains a complete $selector block across the prompt preview boundary", async ({ source, selector, ending }) => {
    mockPromptHeight(400);
    const text = `Review:\n\n${source}\n\nHidden continuation.`;
    await act(async () => root.render(<ContentBlockView presentation="prompt" block={{ type: "text", text }} />));
    expect(container.querySelector(selector)?.textContent).toContain(ending);
    expect(container.querySelector(".prompt-text-toggle")?.getAttribute("aria-expanded")).toBe("false");
  });

  it("keeps Markdown with many source lines unfolded when its rendered height fits", async () => {
    mockPromptHeight(48);
    const text = `Introduction.${"\n".repeat(40)}Short continuation.`;
    await act(async () => root.render(<ContentBlockView presentation="prompt" block={{ type: "text", text }} />));
    expect(container.querySelector(".prompt-text-body")?.textContent).toContain("Short continuation.");
    expect(container.querySelector(".prompt-text")?.getAttribute("data-collapsible")).toBe("false");
    expect(container.querySelector(".prompt-text-toggle")).toBeNull();
  });

  it("keeps a submitted attachment compact until the host supplies its reference", async () => {
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/note.txt", mimeType: "text/plain", text: "中文",
    } }} />));
    expect(container.querySelector(".attachment-chip")?.textContent).toBe("note.txt6 B");
    expect(container.querySelector(".attachment-chip")?.getAttribute("aria-disabled")).toBe("true");
    expect(container.querySelector("a, button, pre, iframe")).toBeNull();
    expect(container.textContent).not.toContain("中文");
  });

  it("disables invalid attachment bytes", async () => {
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/bad.bin", blob: "invalid",
    } }} />));
    expect(container.querySelector(".invalid-content")?.getAttribute("aria-disabled")).toBe("true");
  });

  it.each(["prompt", "message", "tool"] as const)("keeps external resource links compact in %s content", async (presentation) => {
    const block: ContentBlock = {
      type: "resource_link", uri: "https://example.com/report", name: "report.pdf",
      title: "Remote report", description: "Complete project report", mimeType: "application/pdf", size: 2048,
    };
    await act(async () => root.render(<ContentBlockView presentation={presentation} block={block} />));
    const link = container.querySelector<HTMLAnchorElement>("a.attachment-chip")!;
    expect(link.querySelector("strong")?.textContent).toBe("Remote report");
    expect(link.querySelector("small")?.textContent).toBe("2 KiB");
    expect(link.href).toBe(block.uri);
    expect(link.target).toBe("_blank");
    expect(link.rel).toContain("noopener");
    expect(link.hasAttribute("download")).toBe(false);
    expect(link.title).toBe("Remote report\nreport.pdf\nhttps://example.com/report\nComplete project report\napplication/pdf · 2 KiB");
    expect(link.getAttribute("aria-description")).toBe(link.title);
    expect(link.getAttribute("aria-label")).toBe("Open Remote report in a new tab");
    expect(container.querySelector(".resource-card, button, iframe, img")).toBeNull();
  });

  it("distinguishes unknown resource sizes from known empty resources", async () => {
    await act(async () => root.render(<ContentBlockView block={{
      type: "resource_link", uri: "https://example.com/report", name: "Remote report",
    }} />));
    expect(container.querySelector(".attachment-chip")?.textContent).toBe("Remote report");
    expect(container.querySelector("small")).toBeNull();
    expect(container.querySelector(".attachment-chip")?.getAttribute("title")).not.toContain("0 B");
    await act(async () => root.render(<ContentBlockView block={{
      type: "resource_link", uri: "https://example.com/empty", name: "Empty file", size: 0,
    }} />));
    expect(container.querySelector(".attachment-chip")?.textContent).toBe("Empty file0 B");
  });

  it.each(["file:///work/note.md", "urn:agent:note", "javascript:alert(1)", "data:text/plain,note"])("keeps unopenable resource URI %s as a compact label", async (uri) => {
    await act(async () => root.render(<ContentBlockView presentation="prompt" block={{
      type: "resource_link", uri, name: "note.md", description: "Agent-owned reference",
    }} />));
    const chip = container.querySelector<HTMLElement>(".attachment-chip")!;
    expect(chip.textContent).toBe("note.md");
    expect(chip.getAttribute("aria-disabled")).toBe("true");
    expect(chip.getAttribute("aria-description")).toBe("note.md\n" + uri + "\nAgent-owned reference");
    expect(container.querySelector("a, button, iframe, .resource-card")).toBeNull();
  });
});

function hosted(name: string, mimeType: string, size: number): ContentBlock {
  return {
    type: "resource_link", name, mimeType, size, uri: `attyd://attachment/${encodeURIComponent(name)}`,
    _meta: { "attyd/attachment": { id: "a".repeat(64), sessionId: "session/中文", bridgeEpoch: "epoch", sessionIncarnation: 1 } },
  };
}
