export type ThemePreference = "system" | "light" | "dark";
export const THEME_STORAGE_KEY = "attyd.theme";

export function readThemePreference(): ThemePreference {
  try {
    const value = window.localStorage.getItem(THEME_STORAGE_KEY);
    if (value === "light" || value === "dark") return value;
  } catch {
    // Browser preferences remain usable when storage is blocked.
  }
  return "system";
}

let preference = readThemePreference();
let systemTheme: MediaQueryList | undefined;
const listeners = new Set<() => void>();

export function getThemePreference(): ThemePreference {
  return preference;
}

export function subscribeTheme(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function applyTheme() {
  const theme = preference === "system" ? (systemTheme?.matches ? "dark" : "light") : preference;
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
  // The stylesheet is now loaded; it replaces the bootstrap's initial canvas color.
  document.documentElement.style.removeProperty("background-color");
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", theme === "dark" ? "#161617" : "#f5f5f7");
}

export function initializeTheme(): () => void {
  systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
  const updateFromSystem = () => { if (preference === "system") applyTheme(); };
  const updateFromStorage = (event: StorageEvent) => {
    if (event.key !== THEME_STORAGE_KEY && event.key !== null) return;
    preference = readThemePreference();
    applyTheme();
    listeners.forEach((listener) => listener());
  };
  systemTheme.addEventListener("change", updateFromSystem);
  window.addEventListener("storage", updateFromStorage);
  applyTheme();
  return () => {
    systemTheme?.removeEventListener("change", updateFromSystem);
    window.removeEventListener("storage", updateFromStorage);
  };
}

export function setThemePreference(next: ThemePreference): void {
  preference = next;
  try {
    if (next === "system") window.localStorage.removeItem(THEME_STORAGE_KEY);
    else window.localStorage.setItem(THEME_STORAGE_KEY, next);
  } catch {
    // Keep the choice in memory for this page when storage is unavailable.
  }
  applyTheme();
  listeners.forEach((listener) => listener());
}
