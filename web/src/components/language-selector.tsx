import { useState } from "react";
import { Languages } from "lucide-react";
import {
  languageNames,
  getLanguagePreference,
  setLanguagePreference,
  useTranslation,
  type LanguagePreference,
} from "../i18n";

export function LanguageSelector() {
  const { t } = useTranslation("app");
  const [preference, setPreference] = useState(getLanguagePreference);
  return (
    <label className="language-setting">
      <span><Languages size={14} aria-hidden="true" />{t("language.label")}</span>
      <select value={preference} onChange={(event) => {
        const next = event.target.value as LanguagePreference;
        setPreference(next);
        void setLanguagePreference(next);
      }}>
        <option value="system">{t("language.system")}</option>
        {Object.entries(languageNames).map(([code, name]) => (
          <option key={code} value={code} lang={code}>{name}</option>
        ))}
      </select>
    </label>
  );
}
