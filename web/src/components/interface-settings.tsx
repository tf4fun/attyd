import { Monitor, Settings } from "lucide-react";
import { useSyncExternalStore, type RefObject } from "react";
import { useTranslation } from "../i18n";
import { getThemePreference, setThemePreference, subscribeTheme, type ThemePreference } from "../lib/theme";
import { LanguageSelector } from "./language-selector";
import "./interface-settings.css";

export function InterfaceSettings({ menuRef }: { menuRef: RefObject<HTMLDetailsElement | null> }) {
  const { t } = useTranslation("app");
  const theme = useSyncExternalStore(subscribeTheme, getThemePreference);
  return (
    <details className="interface-settings" ref={menuRef}>
      <summary role="button" aria-label={t("interfaceSettings.title")} title={t("interfaceSettings.title")}>
        <Settings size={17} aria-hidden="true" />
      </summary>
      <div className="interface-settings-body">
        <h2>{t("interfaceSettings.title")}</h2>
        <LanguageSelector />
        <label className="language-setting">
          <span><Monitor size={14} aria-hidden="true" />{t("interfaceSettings.appearance")}</span>
          <select value={theme} onChange={(event) => setThemePreference(event.target.value as ThemePreference)}>
            <option value="system">{t("interfaceSettings.system")}</option>
            <option value="light">{t("interfaceSettings.light")}</option>
            <option value="dark">{t("interfaceSettings.dark")}</option>
          </select>
        </label>
        <p>{t("interfaceSettings.localNotice")}</p>
      </div>
    </details>
  );
}
