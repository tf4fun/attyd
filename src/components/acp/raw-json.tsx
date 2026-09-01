import { Braces, ChevronRight } from "lucide-react";

export function RawJson({
  value,
  label = "ACP payload",
  open = false,
}: {
  value: unknown;
  label?: string;
  open?: boolean;
}) {
  const payload = formatJson(value);
  return (
    <details className="raw-json" data-thread-search-ignore open={open}>
      <summary>
        <Braces size={12} aria-hidden="true" />
        <span>{label}</span>
        <ChevronRight className="raw-json-chevron" size={11} aria-hidden="true" />
      </summary>
      <pre><code>{payload}</code></pre>
    </details>
  );
}

export function formatJson(value: unknown): string {
  try {
    return JSON.stringify(
      value,
      (_, item: unknown) => typeof item === "bigint" ? `${item}n` : item,
      2,
    ) ?? String(value);
  } catch (error) {
    return `Unable to serialize payload: ${error instanceof Error ? error.message : String(error)}`;
  }
}
