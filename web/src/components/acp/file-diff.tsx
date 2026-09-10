import type { ToolCallContent } from "@agentclientprotocol/sdk";
import { ChevronRight, FileDiff as FileDiffIcon } from "lucide-react";
import { useMemo } from "react";
import { useTranslation } from "../../i18n";
import { buildReviewLines } from "../../lib/review-changes";
import { DiffLines } from "./diff-lines";

export function FileDiff({
  content,
}: {
  content: Extract<ToolCallContent, { type: "diff" }>;
}) {
  const { t, i18n } = useTranslation("cards");
  const diff = useMemo(
    () => buildReviewLines(content.oldText, content.newText),
    [content.oldText, content.newText],
  );
  return (
    <details className="diff-card">
      <summary>
        <ChevronRight className="diff-chevron" size={13} aria-hidden="true" />
        <FileDiffIcon size={14} aria-hidden="true" />
        <code title={content.path}>{content.path}</code>
        {content.oldText == null ? <small>{t("diff.newFile")}</small> : null}
        <span className="change-review-counts" aria-label={t(diff.approximate ? "diff.approximateCounts" : "diff.counts", { added: diff.addedLines.toLocaleString(i18n.resolvedLanguage), removed: diff.removedLines.toLocaleString(i18n.resolvedLanguage) })}>
          <i>+{diff.addedLines.toLocaleString(i18n.resolvedLanguage)}</i><b>−{diff.removedLines.toLocaleString(i18n.resolvedLanguage)}</b>{diff.approximate ? <em>{t("diff.approximate")}</em> : null}
        </span>
      </summary>
      <div className="diff-body">
        <DiffLines path={content.path} diff={diff} />
        {diff.lines.length === 0 ? (
          <p className="diff-empty">{content.oldText == null ? t("diff.emptyFile") : t("diff.noChanges")}</p>
        ) : null}
      </div>
    </details>
  );
}
