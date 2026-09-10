import { Trans } from "react-i18next";
import { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "../../i18n";

export interface ContextUsageValue {
  used: number;
  size: number;
  cost?: { amount: number; currency: string } | null;
}

export function ContextUsage({ usage }: { usage?: ContextUsageValue }) {
  const { t, i18n } = useTranslation("cards");
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const popoverId = useId();

  useEffect(() => {
    if (!open) return;
    const closeOutside = (event: PointerEvent) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setOpen(false);
      trigger.current?.focus();
    };
    window.addEventListener("pointerdown", closeOutside, true);
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      window.removeEventListener("pointerdown", closeOutside, true);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [open]);

  useEffect(() => {
    if (!usage) setOpen(false);
  }, [usage]);

  if (!usage) return null;
  const ratio = usage.size > 0 ? usage.used / usage.size : 0;
  const percentage = Math.round(ratio * 100);
  const boundedPercentage = Math.max(0, Math.min(percentage, 100));
  const boundedUsed = Math.max(0, Math.min(usage.used, usage.size));
  const level = ratio >= 1 ? "critical" : ratio >= 0.8 ? "warning" : "normal";
  const remaining = Math.max(usage.size - usage.used, 0);
  const exactUsage = t("context.exactUsage", { used: formatInteger(usage.used, i18n.resolvedLanguage), size: formatInteger(usage.size, i18n.resolvedLanguage) });
  const label = t("context.usageLabel", { percentage: percentage.toLocaleString(i18n.resolvedLanguage), usage: exactUsage });

  return (
    <div className="context-usage" ref={root}>
      <button
        ref={trigger}
        type="button"
        className={`context-usage-trigger ${level}`}
        aria-label={label}
        aria-expanded={open}
        aria-controls={open ? popoverId : undefined}
        title={label}
        onClick={() => setOpen((current) => !current)}
      >
        <svg aria-hidden="true" viewBox="0 0 20 20">
          <circle className="context-ring-track" cx="10" cy="10" r="7" pathLength="100" />
          <circle
            className="context-ring-value"
            cx="10"
            cy="10"
            r="7"
            pathLength="100"
            strokeDasharray={`${boundedPercentage} ${100 - boundedPercentage}`}
          />
        </svg>
      </button>
      {open ? (
        <section
          id={popoverId}
          className="context-usage-popover"
          role="region"
          aria-label={t("context.label")}
        >
          <header>
            <strong>{t("context.title")}</strong>
            <span>{t("context.percentUsed", { percentage: percentage.toLocaleString(i18n.resolvedLanguage) })}</span>
          </header>
          <div
            className="context-usage-progress"
            role="progressbar"
            aria-label={t("context.usedLabel")}
            aria-valuemin={0}
            aria-valuemax={usage.size}
            aria-valuenow={boundedUsed}
          >
            <i style={{ width: `${boundedPercentage}%` }} />
          </div>
          <dl>
            <div><dt>{t("context.inContext")}</dt><dd>{t("context.tokens", { count: usage.used, value: formatInteger(usage.used, i18n.resolvedLanguage) })}</dd></div>
            <div><dt>{t("context.windowSize")}</dt><dd>{t("context.tokens", { count: usage.size, value: formatInteger(usage.size, i18n.resolvedLanguage) })}</dd></div>
            <div><dt>{t("context.remaining")}</dt><dd>{t("context.tokens", { count: remaining, value: formatInteger(remaining, i18n.resolvedLanguage) })}</dd></div>
            {usage.cost ? (
              <div><dt>{t("context.sessionCost")}</dt><dd>{formatCost(usage.cost, i18n.resolvedLanguage)}</dd></div>
            ) : null}
          </dl>
          <p><Trans t={t} i18nKey="context.reportedBy" components={{ code: <code /> }} /></p>
        </section>
      ) : null}
    </div>
  );
}

function formatInteger(value: number, language: string | undefined): string {
  return new Intl.NumberFormat(language, { maximumFractionDigits: 0 }).format(value);
}

function formatCost(cost: NonNullable<ContextUsageValue["cost"]>, language: string | undefined): string {
  const amount = new Intl.NumberFormat(language, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 6,
  }).format(cost.amount);
  return `${amount} ${cost.currency}`;
}
