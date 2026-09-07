import type { ToolCallContent } from "@agentclientprotocol/sdk";
import { ChevronRight, FileDiff as FileDiffIcon } from "lucide-react";
import { useMemo } from "react";
import { buildReviewLines } from "../../lib/review-changes";
import { DiffLines } from "./diff-lines";

export function FileDiff({
  content,
}: {
  content: Extract<ToolCallContent, { type: "diff" }>;
}) {
  const diff = useMemo(
    () => buildReviewLines(content.oldText, content.newText),
    [content.oldText, content.newText],
  );
  const qualifier = diff.approximate ? "approximately " : "";
  return (
    <details className="diff-card">
      <summary>
        <ChevronRight className="diff-chevron" size={13} aria-hidden="true" />
        <FileDiffIcon size={14} aria-hidden="true" />
        <code title={content.path}>{content.path}</code>
        {content.oldText == null ? <small>New file</small> : null}
        <span className="change-review-counts" aria-label={`${qualifier}${diff.addedLines} added and ${diff.removedLines} removed lines`}>
          <i>+{diff.addedLines}</i><b>−{diff.removedLines}</b>{diff.approximate ? <em>approx.</em> : null}
        </span>
      </summary>
      <div className="diff-body">
        <DiffLines path={content.path} diff={diff} />
        {diff.lines.length === 0 ? (
          <p className="diff-empty">{content.oldText == null ? "Empty file." : "No content changes."}</p>
        ) : null}
      </div>
    </details>
  );
}
