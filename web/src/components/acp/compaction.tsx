import {
  Archive,
  Check,
  ChevronRight,
  CircleAlert,
  CircleMinus,
  LoaderCircle,
} from "lucide-react";
import { useEffect, useState } from "react";
import type { TimelineItem } from "../../lib/state";
import { ContentBlocks } from "./content-block";
import { RawJson } from "./raw-json";

export function CompactionCard({
  item,
}: {
  item: Extract<TimelineItem, { type: "compaction" }>;
}) {
  const live = item.status === "in_progress";
  const completed = item.status === "completed";
  const failed = item.status === "failed";
  const cancelled = item.status === "cancelled";
  const [open, setOpen] = useState(live || failed);

  // Follow the lifecycle while the Agent owns the disclosure, then leave a
  // terminal item under explicit user control. A completed/cancelled summary
  // collapses at the transition; failures stay open so their reason is not
  // hidden. Subsequent timeline renders do not overwrite a manual toggle.
  useEffect(() => {
    setOpen(live || failed);
  }, [failed, live]);

  const label = live
    ? "Compacting context…"
    : completed
      ? "Context compacted"
      : failed
        ? "Context compaction failed"
        : cancelled
          ? "Context compaction cancelled"
          : "Context compaction";
  const statusClass = live || completed || failed || cancelled
    ? item.status
    : "other";

  return (
    <details
      className={`compaction-card status-${statusClass}`}
      data-thread-entry
      data-thread-entry-id={item.id}
      data-compaction-status={item.status}
      data-live={live ? "true" : "false"}
      open={open}
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="compaction-heading" data-thread-searchable>
        <Archive size={15} aria-hidden="true" />
        <span>
          <strong>{label}</strong>
          <small>{compactionStatusLabel(item.status, item.blocks.length)}</small>
        </span>
        <code>{item.compactionId}</code>
        {completed ? (
          <Check className="compaction-state-icon complete" size={14} aria-hidden="true" />
        ) : failed ? (
          <CircleAlert className="compaction-state-icon failed" size={14} aria-hidden="true" />
        ) : cancelled ? (
          <CircleMinus className="compaction-state-icon cancelled" size={14} aria-hidden="true" />
        ) : (
          <LoaderCircle className={`compaction-state-icon${live ? " spin" : ""}`} size={14} aria-hidden="true" />
        )}
        <ChevronRight className="compaction-chevron" size={14} aria-hidden="true" />
      </summary>
      <div className="compaction-body" data-thread-searchable>
        {item.blocks.length > 0 ? (
          <div className="compaction-summary"><ContentBlocks blocks={item.blocks} /></div>
        ) : (
          <p className="compaction-empty">No retained summary was supplied by the Agent.</p>
        )}
        {item.error ? <p className="compaction-error">{item.error}</p> : null}
        <RawJson label="Compaction events" value={item.raw} />
      </div>
    </details>
  );
}

function compactionStatusLabel(status: string, blockCount: number): string {
  const summary = blockCount === 0
    ? "no retained summary"
    : `${blockCount} summary block${blockCount === 1 ? "" : "s"}`;
  if (status === "in_progress") return `streaming · ${summary}`;
  if (status === "completed") return summary;
  if (status === "failed") return `failed · ${summary}`;
  if (status === "cancelled") return `cancelled · ${summary}`;
  return `${status} · ${summary}`;
}
