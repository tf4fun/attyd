// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n, {
  getLanguagePreference,
  LANGUAGE_STORAGE_KEY,
  readLanguagePreference,
  resources,
  setLanguagePreference,
} from "../web/src/i18n";
import { LanguageSelector } from "../web/src/components/language-selector";
import { translateHistoryNotice } from "../web/src/i18n/history-notice";
import { PromptComposer } from "../web/src/components/acp/prompt-composer";
import { PromptAttachmentError, promptAttachmentErrorMessage } from "../web/src/lib/prompt-attachments";
import { contentBlocksToMarkdown } from "../web/src/lib/thread-markdown";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

beforeEach(() => {
  // Node 26 exposes its own unavailable localStorage getter to happy-dom.
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
    removeItem: (key: string) => { values.delete(key); },
  });
});

afterEach(async () => {
  vi.restoreAllMocks();
  await setLanguagePreference("system");
  vi.unstubAllGlobals();
});

describe("interface languages", () => {
  it("detects browser language families, falls back to English, and does not cache detection", async () => {
    for (const [languages, expected] of [
      [["zh-CN"], "zh-CN"],
      [["zh-Hans-SG"], "zh-CN"],
      [["en-GB"], "en"],
      [["fr-FR", "zh-CN"], "zh-CN"],
      [["fr-FR"], "en"],
    ] as const) {
      vi.spyOn(navigator, "languages", "get").mockReturnValue([...languages]);
      vi.spyOn(navigator, "language", "get").mockReturnValue(languages[0]);
      await setLanguagePreference("system");
      expect(i18n.resolvedLanguage).toBe(expected);
      expect(localStorage.getItem(LANGUAGE_STORAGE_KEY)).toBeNull();
      expect(document.documentElement.lang).toBe(expected);
      expect(document.documentElement.dir).toBe("ltr");
      expect(document.title).toBe(i18n.t("pageTitle"));
    }
  });

  it("remembers an explicit choice and removes it when following the browser again", async () => {
    await setLanguagePreference("zh-CN");
    expect(readLanguagePreference()).toBe("zh-CN");
    expect(localStorage.getItem(LANGUAGE_STORAGE_KEY)).toBe("zh-CN");
    await setLanguagePreference("system");
    expect(readLanguagePreference()).toBe("system");
    expect(localStorage.getItem(LANGUAGE_STORAGE_KEY)).toBeNull();
    localStorage.setItem(LANGUAGE_STORAGE_KEY, "unrecognized-language");
    expect(readLanguagePreference()).toBe("system");
  });

  it("keeps switching usable in memory when storage is blocked", async () => {
    vi.spyOn(window.localStorage, "setItem").mockImplementation(() => { throw new Error("blocked"); });
    vi.spyOn(window.localStorage, "getItem").mockImplementation(() => { throw new Error("blocked"); });
    vi.spyOn(window.localStorage, "removeItem").mockImplementation(() => { throw new Error("blocked"); });
    expect(readLanguagePreference()).toBe("system");
    await expect(setLanguagePreference("zh-CN")).resolves.toBeUndefined();
    expect(getLanguagePreference()).toBe("zh-CN");
    expect(i18n.t("projects")).toBe("项目");
    await expect(setLanguagePreference("system")).resolves.toBeUndefined();
  });

  it("uses locale plural rules and falls back to English for a missing translation", async () => {
    await setLanguagePreference("en");
    expect(i18n.t("mcpServers", { count: 1 })).toBe("1 MCP server");
    expect(i18n.t("mcpServers", { count: 2 })).toBe("2 MCP servers");
    await setLanguagePreference("zh-CN");
    expect(i18n.t("mcpServers", { count: 2 })).toBe("2 个 MCP 服务");
    const original = i18n.getResource("zh-CN", "app", "projects");
    i18n.addResource("zh-CN", "app", "projects", undefined);
    try {
      expect(i18n.t("projects")).toBe("Projects");
    } finally {
      i18n.addResource("zh-CN", "app", "projects", original);
    }
  });

  it("updates controls without remounting the composer or changing user content", async () => {
    await setLanguagePreference("en");
    const container = document.createElement("div");
    document.body.append(container);
    const root = createRoot(container);
    try {
      await act(async () => root.render(<>
        <LanguageSelector />
        <PromptComposer disabled={false} running={false} commands={[]} onSubmit={() => true} onCancel={() => undefined} />
      </>));
      const input = container.querySelector("textarea")!;
      await act(async () => {
        Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input, "保留 draft <code>");
        input.dispatchEvent(new InputEvent("input", { bubbles: true }));
      });
      const originalPlaceholder = input.placeholder;
      const selector = container.querySelector("select")!;
      await act(async () => {
        selector.value = "zh-CN";
        selector.dispatchEvent(new Event("change", { bubbles: true }));
      });
      expect(container.textContent).toContain("界面语言");
      expect(container.querySelector("textarea")).toBe(input);
      expect(input.value).toBe("保留 draft <code>");
      expect(input.placeholder).not.toBe(originalPlaceholder);
      expect(contentBlocksToMarkdown([{ type: "text", text: "Agent原文 **raw**" }])).toBe("Agent原文 **raw**");
    } finally {
      await act(async () => root.unmount());
      container.remove();
    }
  });

  it("translates stored attachment diagnostics without rewriting external errors", async () => {
    await setLanguagePreference("en");
    const error = new PromptAttachmentError("sizeLimit");
    const english = promptAttachmentErrorMessage(error);
    await setLanguagePreference("zh-CN");
    expect(promptAttachmentErrorMessage(error)).not.toBe(english);
    expect(promptAttachmentErrorMessage(new Error("Agent said: denied"))).toBe("Agent said: denied");
  });

  it("translates known host history notices while leaving unknown diagnostics intact", async () => {
    await setLanguagePreference("zh-CN");
    const t = i18n.getFixedT(null, "app");
    expect(translateHistoryNotice(resources.en.app.historyNotices.unavailable, t))
      .toBe(resources["zh-CN"].app.historyNotices.unavailable);
    expect(translateHistoryNotice("An unfamiliar host notice", t)).toBe("An unfamiliar host notice");
    await setLanguagePreference("en");
    expect(translateHistoryNotice(resources.en.app.historyNotices.unavailable, t))
      .toBe(resources.en.app.historyNotices.unavailable);
  });
});

describe("translation resources", () => {
  it("covers the same messages and interpolation variables in every bundled language", () => {
    const english = flatten(resources.en);
    for (const [language, resource] of Object.entries(resources)) {
      const translated = flatten(resource);
      const baseKey = (key: string) => key.replace(/_(?:zero|one|two|few|many|other)$/u, "");
      expect(new Set(Object.keys(translated).map(baseKey)), language)
        .toEqual(new Set(Object.keys(english).map(baseKey)));
      for (const [key, value] of Object.entries(translated)) {
        const reference = english[key] ?? english[`${baseKey(key)}_other`];
        expect(value.trim(), `${language}:${key}`).not.toBe("");
        expect(variables(value), `${language}:${key}`).toEqual(variables(reference));
      }
    }
  });
});

function flatten(value: object, prefix = ""): Record<string, string> {
  return Object.fromEntries(Object.entries(value).flatMap(([key, child]) => {
    const path = prefix ? `${prefix}.${key}` : key;
    return typeof child === "string" ? [[path, child]] : Object.entries(flatten(child, path));
  }));
}

function variables(value: string): string[] {
  return [...value.matchAll(/\{\{\s*([^},\s]+).*?\}\}/gu)].map((match) => match[1]).sort();
}
