import type { TFunction } from "i18next";
import {
  Archive,
  Check,
  ChevronRight,
  CircleAlert,
  CircleMinus,
  LoaderCircle,
} from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "../../i18n";
import type { TimelineItem } from "../../lib/state";
import { ContentBlocks } from "./content-block";
import { RawJson } from "./raw-json";

export function CompactionCard({
  item,
}: {
  item: Extract<TimelineItem, { type: "compaction" }>;
}) {
  const { t } = useTranslation("cards");
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
    ? t("compaction.running")
    : completed
      ? t("compaction.completed")
      : failed
        ? t("compaction.failed")
        : cancelled
          ? t("compaction.cancelled")
          : t("compaction.title");
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
          <small>{compactionStatusLabel(item.status, item.blocks.length, t)}</small>
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
          <p className="compaction-empty">{t("compaction.noSummarySupplied")}</p>
        )}
        {item.error ? <p className="compaction-error">{item.error}</p> : null}
        <RawJson label={t("compaction.events")} value={item.raw} />
      </div>
    </details>
  );
}

function compactionStatusLabel(status: string, blockCount: number, t: TFunction<"cards">): string {
  const summary = t("compaction.summary", { count: blockCount });
  if (status === "completed") return summary;
  if (status === "in_progress") return t("compaction.streamingSummary", { summary });
  if (status === "failed") return t("compaction.failedSummary", { summary });
  if (status === "cancelled") return t("compaction.cancelledSummary", { summary });
  return t("compaction.otherSummary", { status, summary });
}
