import type { TFunction } from "i18next";
import { resources } from "./index";

// The host currently sends its history notices as text. Translate only this
// explicit set of attyd-owned notices; unknown diagnostics remain unchanged.
export function translateHistoryNotice(notice: string, t: TFunction<"app">): string {
  for (const [key, text] of Object.entries(resources.en.app.historyNotices)) {
    if (notice === text) return t(`historyNotices.${key as keyof typeof resources.en.app.historyNotices}`);
  }
  return notice;
}
