import type { TFunction } from "i18next";
import { Braces, ChevronRight } from "lucide-react";
import i18n, { useTranslation } from "../../i18n";

export function RawJson({
  value,
  label,
  open = false,
}: {
  value: unknown;
  label?: string;
  open?: boolean;
}) {
  const { t } = useTranslation("cards");
  const payload = formatJson(value, t);
  return (
    <details className="raw-json" data-thread-search-ignore open={open}>
      <summary>
        <Braces size={12} aria-hidden="true" />
        <span>{label ?? t("json.payload")}</span>
        <ChevronRight className="raw-json-chevron" size={11} aria-hidden="true" />
      </summary>
      <pre><code>{payload}</code></pre>
    </details>
  );
}

export function formatJson(value: unknown, t: TFunction<"cards"> = i18n.getFixedT(null, "cards")): string {
  try {
    return JSON.stringify(
      value,
      (_, item: unknown) => typeof item === "bigint" ? `${item}n` : item,
      2,
    ) ?? String(value);
  } catch (error) {
    return t("json.serializeError", { error: error instanceof Error ? error.message : String(error) });
  }
}
