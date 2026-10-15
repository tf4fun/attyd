// @vitest-environment happy-dom

import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  createAttachmentReader,
  inertType,
  useBlockUrl,
  type AttachedBlock,
} from "../web/src/lib/attachments";
import type { AttachmentRef } from "../shared/protocol";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const png = (data: string): AttachedBlock => ({ type: "image", mimeType: "image/png", data });

describe("local attachment URLs", () => {
  let container: HTMLDivElement;
  let root: Root;
  let live: Set<string>;
  let created: string[];

  beforeEach(() => {
    live = new Set();
    created = [];
    let next = 0;
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => {
      const url = `blob:test/${next += 1}`;
      live.add(url);
      created.push(url);
      return url;
    });
    vi.spyOn(URL, "revokeObjectURL").mockImplementation((url) => {
      live.delete(url);
    });
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(() => {
    container.remove();
    vi.restoreAllMocks();
  });

  function Shown({ block }: { block: AttachedBlock }) {
    const local = useBlockUrl(block);
    return <a data-download={local?.download ? "true" : "false"} href={local?.url}>link</a>;
  }

  const shown = () => container.querySelector("a")!.getAttribute("href");

  it("keeps the shown URL usable under StrictMode and releases every URL it created", async () => {
    const first = png("AQID");
    await act(async () => root.render(<StrictMode><Shown block={first} /></StrictMode>));
    expect(shown()).not.toBeNull();
    expect(live.has(shown()!)).toBe(true);

    const second = png("BAUG");
    await act(async () => root.render(<StrictMode><Shown block={second} /></StrictMode>));
    expect(live.has(shown()!)).toBe(true);
    expect([...live]).toEqual([shown()]);

    await act(async () => root.unmount());
    expect(created.length).toBeGreaterThan(0);
    expect([...live]).toEqual([]);
  });

  it("offers markup and unknown types only as downloads", async () => {
    const html: AttachedBlock = {
      type: "resource",
      resource: { uri: "attyd://attachment/page.html", mimeType: "text/html", text: "<script>1</script>" },
    };
    await act(async () => root.render(<Shown block={html} />));
    expect(container.querySelector("a")!.dataset.download).toBe("true");
    await act(async () => root.unmount());
    expect([...live]).toEqual([]);
  });
});

describe("attachment types a local URL may show", () => {
  it("shows inert media and plain text, and nothing that runs as a document", () => {
    expect(inertType("image/png")).toBe("image/png");
    expect(inertType("audio/wav")).toBe("audio/wav");
    expect(inertType("text/markdown")).toBe("text/plain;charset=utf-8");
    expect(inertType('text/plain; charset="utf-8"')).toBe("text/plain;charset=utf-8");
    for (const type of ["text/html", "image/svg+xml", "application/xhtml+xml", "text/xml", "application/pdf", "application/octet-stream"]) {
      expect(inertType(type)).toBeUndefined();
    }
  });
});

describe("attachment reader", () => {
  const reference: AttachmentRef = { type: "attachment", id: "a".repeat(64), block: "image", mimeType: "image/png" };
  let fetches: number;
  let pending: Array<() => void>;

  beforeEach(() => {
    fetches = 0;
    pending = [];
    vi.stubGlobal("fetch", vi.fn(async () => {
      fetches += 1;
      return new Response(JSON.stringify(png("AQID")), { status: 200 });
    }));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  const reader = () => createAttachmentReader("session", (release) => pending.push(release));
  const flush = () => pending.splice(0).forEach((release) => release());

  it("reads a reference once while views hold it", async () => {
    const attachments = reader();
    const first = attachments.acquire(reference);
    const second = attachments.acquire(reference);
    expect(await first.read).toEqual(png("AQID"));
    expect(await second.read).toEqual(png("AQID"));
    expect(fetches).toBe(1);
    first.release();
    flush();
    attachments.acquire(reference);
    expect(fetches).toBe(1);
  });

  it("drops a block once no view holds it, but not across a remount in one commit", async () => {
    const attachments = reader();
    const view = attachments.acquire(reference);
    await view.read;
    view.release();
    const remounted = attachments.acquire(reference);
    flush();
    await remounted.read;
    expect(fetches).toBe(1);
    remounted.release();
    flush();
    await attachments.acquire(reference).read;
    expect(fetches).toBe(2);
  });
});
