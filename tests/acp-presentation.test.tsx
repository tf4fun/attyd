// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CloseSessionDialog } from "../web/src/components/acp/close-session-dialog";
import { ContentBlockView } from "../web/src/components/acp/content-block";
import { SessionControls } from "../web/src/components/acp/session-controls";

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

  it("requires explicit close confirmation and keeps focus on the safe action", async () => {
    const cancel = vi.fn();
    const confirm = vi.fn();
    await act(async () => root.render(<CloseSessionDialog disabled={false} onCancel={cancel} onConfirm={confirm} />));
    const [keep, close] = container.querySelectorAll("button");
    expect(document.activeElement).toBe(keep);
    expect(container.textContent).toContain("child processes");
    expect(container.textContent).toContain("Detached background services");
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

  it("downloads the decoded attachment bytes only after a user click", async () => {
    vi.useFakeTimers();
    const create = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:test");
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => undefined);
    const clicks: { name: string; href: string }[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function () {
      clicks.push({ name: this.download, href: this.href });
    });
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/report%20one.pdf", mimeType: "application/pdf", blob: "YWJj",
    } }} />));
    expect(container.textContent).toContain("report one.pdf");
    expect(container.textContent).toContain("3 B");
    expect(container.textContent).not.toContain("YWJj");
    expect(container.querySelector("iframe, object, embed")).toBeNull();
    expect(create).not.toHaveBeenCalled();
    await act(async () => container.querySelector("button")!.click());
    expect(create.mock.calls[0][0].size).toBe(3);
    expect(create.mock.calls[0][0].type).toBe("application/pdf");
    expect(clicks).toEqual([{ name: "report one.pdf", href: "blob:test" }]);
    await act(async () => vi.runAllTimers());
    expect(revoke).toHaveBeenCalledWith("blob:test");
  });

  it("shares embedded media previews and retains download when a preview fails", async () => {
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/image.png", mimeType: "image/png", blob: "iVBORw==",
    } }} />));
    const image = container.querySelector("img")!;
    expect(image.src).toBe("data:image/png;base64,iVBORw==");
    await act(async () => image.dispatchEvent(new Event("error")));
    expect(container.textContent).toContain("Preview unavailable");
    expect(container.querySelector("button")!.disabled).toBe(false);
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/audio.mp3", mimeType: "audio/mpeg", blob: "AA==",
    } }} />));
    expect(container.querySelector("audio")?.src).toBe("data:audio/mpeg;base64,AA==");
  });

  it("disables downloads for invalid bytes and leaves resource links as links", async () => {
    await act(async () => root.render(<ContentBlockView block={{ type: "resource", resource: {
      uri: "file:///work/bad.bin", blob: "invalid",
    } }} />));
    expect(container.querySelector("button")!.disabled).toBe(true);
    await act(async () => root.render(<ContentBlockView block={{
      type: "resource_link", uri: "https://example.com/report", name: "Remote report",
    }} />));
    expect(container.querySelector("a")?.href).toBe("https://example.com/report");
    expect(container.querySelector("a")?.hasAttribute("download")).toBe(false);
    expect(container.querySelector("button, iframe, img")).toBeNull();
  });
});
