import { Info } from "lucide-react";
import type { MouseEventHandler } from "react";
import { formatJson } from "./raw-json";

export function DebugInfoButton({
  expanded,
  label,
  onClick,
}: {
  expanded: boolean;
  label: string;
  onClick: MouseEventHandler<HTMLButtonElement>;
}) {
  return (
    <button
      type="button"
      className="debug-info-button"
      aria-label={label}
      aria-expanded={expanded}
      title={expanded ? "Hide debug information" : "Show debug information"}
      onClick={onClick}
    >
      <Info size={12} aria-hidden="true" />
    </button>
  );
}

export function DebugInfoPanel({
  label,
  entries,
  hidden,
}: {
  label: string;
  entries: Array<{
    label: string;
    value: unknown;
    format?: "json" | "text";
    emptyText?: string;
    count?: number;
  }>;
  hidden: boolean;
}) {
  return (
    <section
      className="debug-info-panel"
      aria-label={label}
      data-thread-search-ignore
      hidden={hidden}
    >
      <header><Info size={13} aria-hidden="true" /><strong>Debug information</strong></header>
      {entries.map((entry) => (
        <div className="debug-info-entry" key={entry.label}>
          <div className="debug-info-label">
            <span>{entry.label}</span>
            {entry.count != null ? <em>{entry.count}</em> : null}
          </div>
          {entry.format === "text" ? (
            <code title={entry.value == null ? undefined : String(entry.value)}>
              {entry.value == null || entry.value === ""
                ? entry.emptyText ?? "Not reported"
                : String(entry.value)}
            </code>
          ) : (
            <pre><code>{formatJson(entry.value)}</code></pre>
          )}
        </div>
      ))}
    </section>
  );
}

export function StructuredData({ value }: { value: unknown }) {
  const entries = structuredEntries(value);
  if (!entries) return <div className="structured-scalar"><StructuredLeaf value={value} /></div>;
  if (entries.length === 0) return <div className="structured-scalar"><code>Empty</code></div>;
  return (
    <dl className="structured-data">
      {entries.map(([key, item]) => (
        <div key={key}>
          <dt>{key}</dt>
          <dd><StructuredLeaf value={item} /></dd>
        </div>
      ))}
    </dl>
  );
}

function StructuredLeaf({ value }: { value: unknown }) {
  if (typeof value === "string") {
    return value.includes("\n")
      ? <pre><code>{value}</code></pre>
      : <code>{value || "Empty string"}</code>;
  }
  if (value == null || typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") {
    return <code>{value == null ? "null" : String(value)}</code>;
  }
  return <pre><code>{formatJson(value)}</code></pre>;
}

function structuredEntries(value: unknown): Array<[string, unknown]> | undefined {
  if (Array.isArray(value)) return value.map((item, index) => [String(index), item]);
  if (value && typeof value === "object") return Object.entries(value as Record<string, unknown>);
  return undefined;
}
