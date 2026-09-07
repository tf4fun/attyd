import { ChevronRight, FileDiff, Info } from "lucide-react";
import { useId } from "react";
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
  const panelId = useId();
  if (summary.fileCount === 0) return null;
  const qualifier = summary.approximate ? "approximately " : "";
  return (
    <section
      className={`change-review ${open ? "open" : ""}`}
      aria-label="Agent-reported changes"
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
        <strong>Changes</strong>
        <span>{summary.fileCount} file{summary.fileCount === 1 ? "" : "s"}</span>
        <span className="change-review-counts" aria-label={`${qualifier}${summary.addedLines} added and ${summary.removedLines} removed lines`}>
          <i>+{summary.addedLines}</i><b>−{summary.removedLines}</b>{summary.approximate ? <em>approx.</em> : null}
        </span>
        <small>Review</small>
        <ChevronRight className="change-review-chevron" size={14} />
      </button>
      {open ? (
        <div id={panelId} className="change-review-panel">
          <header>
            <span><FileDiff size={15} /><strong>Agent-reported ACP diffs</strong></span>
            <p><Info size={12} /> Read-only. ACP reports display diffs but does not define client rollback.</p>
          </header>
          <div className="change-review-files">
            {summary.files.map((file, fileIndex) => (
              <details className="change-review-file" key={file.path} open={fileIndex === 0}>
                <summary>
                  <ChevronRight size={13} />
                  <code title={file.path}>{file.path}</code>
                  <span><i>+{file.addedLines}</i> <b>−{file.removedLines}</b></span>
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
  return (
    <article className="change-review-diff" aria-label={`Diff from ${diff.title}`}>
      <div className="change-review-diff-heading">
        <span>{diff.title}</span>
        <code>{diff.toolCallId}</code>
        <small className={`status-${diff.status ?? "pending"}`}>{formatStatus(diff.status)}</small>
      </div>
      <DiffLines path={diff.path} diff={diff} />
    </article>
  );
}

function formatStatus(status: ReviewDiff["status"]): string {
  return (status ?? "pending").replace("_", " ");
}
