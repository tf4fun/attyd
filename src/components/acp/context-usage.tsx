import { useEffect, useId, useRef, useState } from "react";

export interface ContextUsageValue {
  used: number;
  size: number;
  cost?: { amount: number; currency: string } | null;
}

export function ContextUsage({ usage }: { usage?: ContextUsageValue }) {
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
  const exactUsage = `${formatInteger(usage.used)} of ${formatInteger(usage.size)} context tokens`;
  const label = `Context usage: ${percentage}% · ${exactUsage}`;

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
          aria-label="ACP context usage"
        >
          <header>
            <strong>Context window</strong>
            <span>{percentage}% used</span>
          </header>
          <div
            className="context-usage-progress"
            role="progressbar"
            aria-label="Context window used"
            aria-valuemin={0}
            aria-valuemax={usage.size}
            aria-valuenow={boundedUsed}
          >
            <i style={{ width: `${boundedPercentage}%` }} />
          </div>
          <dl>
            <div><dt>In context</dt><dd>{formatInteger(usage.used)} tokens</dd></div>
            <div><dt>Window size</dt><dd>{formatInteger(usage.size)} tokens</dd></div>
            <div><dt>Remaining</dt><dd>{formatInteger(remaining)} tokens</dd></div>
            {usage.cost ? (
              <div><dt>Session cost</dt><dd>{formatCost(usage.cost)}</dd></div>
            ) : null}
          </dl>
          <p>Reported by the Agent via ACP <code>usage_update</code>.</p>
        </section>
      ) : null}
    </div>
  );
}

function formatInteger(value: number): string {
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value);
}

function formatCost(cost: NonNullable<ContextUsageValue["cost"]>): string {
  const amount = new Intl.NumberFormat("en-US", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 6,
  }).format(cost.amount);
  return `${amount} ${cost.currency}`;
}
