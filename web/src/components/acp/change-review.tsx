import { ChevronRight, FileDiff, Info } from "lucide-react";
import { useId } from "react";
import { useTranslation } from "../../i18n";
import type { ReviewDiff, ReviewSummary } from "../../lib/review-changes";
import { DiffLines } from "./diff-lines";

export function ChangeReview({
  summary,
  open,
  onToggle,
  entryId,
}: {
  summary: ReviewSummary;
  open: boolean;
  onToggle: () => void;
  entryId?: string;
}) {
  const { t, i18n } = useTranslation("cards");
  const panelId = useId();
  if (summary.fileCount === 0) return null;
  return (
    <section
      className={`change-review ${open ? "open" : ""}`}
      aria-label={t("review.label")}
      data-thread-entry={entryId != null ? true : undefined}
      data-thread-entry-id={entryId}
    >
      <button
        type="button"
        className="change-review-trigger"
        aria-expanded={open}
        aria-controls={panelId}
        onClick={onToggle}
      >
        <FileDiff size={15} />
        <strong>{t("review.title")}</strong>
        <span>{t("review.fileCount", { count: summary.fileCount })}</span>
        <span className="change-review-counts" aria-label={t(summary.approximate ? "diff.approximateCounts" : "diff.counts", { added: summary.addedLines.toLocaleString(i18n.resolvedLanguage), removed: summary.removedLines.toLocaleString(i18n.resolvedLanguage) })}>
          <i>+{summary.addedLines.toLocaleString(i18n.resolvedLanguage)}</i><b>−{summary.removedLines.toLocaleString(i18n.resolvedLanguage)}</b>{summary.approximate ? <em>{t("diff.approximate")}</em> : null}
        </span>
        <small>{t("review.action")}</small>
        <ChevronRight className="change-review-chevron" size={14} />
      </button>
      {open ? (
        <div id={panelId} className="change-review-panel">
          <header>
            <span><FileDiff size={15} /><strong>{t("review.diffs")}</strong></span>
            <p><Info size={12} /> {t("review.readOnly")}</p>
          </header>
          <div className="change-review-files">
            {summary.files.map((file, fileIndex) => (
              <details className="change-review-file" key={file.path} open={fileIndex === 0}>
                <summary>
                  <ChevronRight size={13} />
                  <code title={file.path}>{file.path}</code>
                  <span><i>+{file.addedLines.toLocaleString(i18n.resolvedLanguage)}</i> <b>−{file.removedLines.toLocaleString(i18n.resolvedLanguage)}</b></span>
                </summary>
                <div className="change-review-file-body">
                  {file.diffs.map((diff) => <ReviewDiffView key={diff.id} diff={diff} />)}
                </div>
              </details>
            ))}
          </div>
        </div>
      ) : null}
    </section>
  );
}

function ReviewDiffView({ diff }: { diff: ReviewDiff }) {
  const { t } = useTranslation("cards");
  return (
    <article className="change-review-diff" aria-label={t("review.diffFrom", { title: diff.title })}>
      <div className="change-review-diff-heading">
        <span>{diff.title}</span>
        <code>{diff.toolCallId}</code>
        <small className={`status-${diff.status ?? "pending"}`}>{t(`review.status.${diff.status ?? "pending"}`)}</small>
      </div>
      <DiffLines path={diff.path} diff={diff} />
    </article>
  );
}
