// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

let dispose: (() => void) | undefined;
let dark = false;
let media: EventTarget;

beforeEach(() => {
  vi.resetModules();
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
    removeItem: (key: string) => { values.delete(key); },
  });
  dark = false;
  media = new EventTarget();
  Object.defineProperty(media, "matches", { get: () => dark });
  vi.stubGlobal("matchMedia", () => media);
  document.head.innerHTML = '<meta name="theme-color" content="#f5f5f7">';
});

afterEach(() => {
  dispose?.();
  dispose = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function setSystemDark(value: boolean) {
  dark = value;
  media.dispatchEvent(new Event("change"));
}

describe("interface theme preferences", () => {
  it("follows live system changes until explicitly overridden, then can resume following", async () => {
    const theme = await import("../web/src/lib/theme");
    dispose = theme.initializeTheme();
    expect(document.documentElement.dataset.theme).toBe("light");
    setSystemDark(true);
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(document.documentElement.style.colorScheme).toBe("dark");
    expect(document.querySelector('meta[name="theme-color"]')?.getAttribute("content")).toBe("#161617");
    expect(localStorage.getItem(theme.THEME_STORAGE_KEY)).toBeNull();

    theme.setThemePreference("light");
    expect(localStorage.getItem(theme.THEME_STORAGE_KEY)).toBe("light");
    setSystemDark(false);
    setSystemDark(true);
    expect(document.documentElement.dataset.theme).toBe("light");
    theme.setThemePreference("system");
    expect(localStorage.getItem(theme.THEME_STORAGE_KEY)).toBeNull();
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  it("restores a saved choice on startup and ignores invalid values", async () => {
    localStorage.setItem("attyd.theme", "dark");
    const theme = await import("../web/src/lib/theme");
    dispose = theme.initializeTheme();
    expect(theme.getThemePreference()).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    localStorage.setItem("attyd.theme", "invalid");
    expect(theme.readThemePreference()).toBe("system");
  });

  it("still starts and switches in memory when browser storage is blocked", async () => {
    for (const name of ["getItem", "setItem", "removeItem"] as const) {
      vi.spyOn(localStorage, name).mockImplementation(() => { throw new Error("blocked"); });
    }
    const theme = await import("../web/src/lib/theme");
    dispose = theme.initializeTheme();
    theme.setThemePreference("dark");
    expect(theme.getThemePreference()).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    theme.setThemePreference("system");
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  it("synchronizes saved preferences across tabs and removes listeners on disposal", async () => {
    const theme = await import("../web/src/lib/theme");
    dispose = theme.initializeTheme();
    const listener = vi.fn();
    const unsubscribe = theme.subscribeTheme(listener);
    localStorage.setItem("attyd.theme", "dark");
    window.dispatchEvent(new StorageEvent("storage", { key: "attyd.theme" }));
    expect(theme.getThemePreference()).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(listener).toHaveBeenCalledOnce();
    localStorage.removeItem("attyd.theme");
    window.dispatchEvent(new StorageEvent("storage", { key: null }));
    expect(theme.getThemePreference()).toBe("system");
    expect(document.documentElement.dataset.theme).toBe("light");
    unsubscribe();
    dispose();
    dispose = undefined;
    setSystemDark(true);
    expect(document.documentElement.dataset.theme).toBe("light");
  });
});
