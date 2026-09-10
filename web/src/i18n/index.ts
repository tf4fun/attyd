import i18next from "i18next";
import LanguageDetector from "i18next-browser-languagedetector";
import { initReactI18next } from "react-i18next";
import enApp from "./locales/en/app.json";
import enWorkspace from "./locales/en/workspace.json";
import enConversation from "./locales/en/conversation.json";
import enCards from "./locales/en/cards.json";
import zhApp from "./locales/zh-CN/app.json";
import zhWorkspace from "./locales/zh-CN/workspace.json";
import zhConversation from "./locales/zh-CN/conversation.json";
import zhCards from "./locales/zh-CN/cards.json";

export { useTranslation } from "react-i18next";

export const LANGUAGE_STORAGE_KEY = "attyd.language";
export const resources = {
  en: { app: enApp, workspace: enWorkspace, conversation: enConversation, cards: enCards },
  "zh-CN": { app: zhApp, workspace: zhWorkspace, conversation: zhConversation, cards: zhCards },
};
export const languageNames: Record<keyof typeof resources, string> = {
  en: "English",
  "zh-CN": "简体中文",
};
export type LanguagePreference = keyof typeof resources | "system";

declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "app";
    resources: typeof resources.en;
  }
}

export function readLanguagePreference(): LanguagePreference {
  try {
    const value = window.localStorage.getItem(LANGUAGE_STORAGE_KEY);
    if (value && Object.hasOwn(resources, value)) return value as keyof typeof resources;
  } catch {
    // A blocked storage API must not prevent the application from starting.
  }
  return "system";
}

const i18n = i18next.createInstance();
i18n.use(LanguageDetector).use(initReactI18next);
i18n.on("languageChanged", () => {
  if (typeof document === "undefined") return;
  document.documentElement.lang = i18n.resolvedLanguage ?? "en";
  document.documentElement.dir = i18n.dir();
  document.title = i18n.t("pageTitle");
});

const preference = readLanguagePreference();
let currentPreference = preference;
export function getLanguagePreference(): LanguagePreference {
  return currentPreference;
}

void i18n.init({
  resources,
  lng: preference === "system" ? undefined : preference,
  supportedLngs: Object.keys(resources),
  fallbackLng: "en",
  defaultNS: "app",
  initAsync: false,
  interpolation: { escapeValue: false }, // React escapes rendered text.
  detection: {
    order: ["navigator"],
    caches: [], // Only an explicit user choice is saved, never automatic detection.
  },
});

export async function setLanguagePreference(preference: LanguagePreference): Promise<void> {
  currentPreference = preference;
  try {
    if (preference === "system") window.localStorage.removeItem(LANGUAGE_STORAGE_KEY);
    else window.localStorage.setItem(LANGUAGE_STORAGE_KEY, preference);
  } catch {
    // Switching still works in memory when browser storage is unavailable.
  }
  await i18n.changeLanguage(preference === "system" ? undefined : preference);
}

export default i18n;
