import { useTranslation } from "../../i18n";
import type { ReviewDiffResult, ReviewLineKind } from "../../lib/review-changes";

export function DiffLines({
  path,
  diff,
}: {
  path: string;
  diff: Pick<ReviewDiffResult, "lines" | "approximate" | "truncated">;
}) {
  const { t } = useTranslation("cards");
  return (
    <>
      <div className="change-review-lines" role="table" aria-label={t("diff.readOnlyFor", { path })}>
        {diff.lines.map((line, index) => (
          <div className={`change-review-line line-${line.kind}`} role="row" key={`${line.kind}:${line.oldLine ?? ""}:${line.newLine ?? ""}:${index}`}>
            <span role="cell">{line.oldLine ?? ""}</span>
            <span role="cell">{line.newLine ?? ""}</span>
            <span role="cell" aria-hidden="true">{lineMarker(line.kind)}</span>
            <code role="cell">{line.text || " "}</code>
          </div>
        ))}
      </div>
      {diff.approximate || diff.truncated ? (
        <footer>{t(diff.approximate && diff.truncated ? "diff.approximateAndTruncated" : diff.approximate ? "diff.approximateNotice" : "diff.truncatedNotice")}</footer>
      ) : null}
    </>
  );
}

function lineMarker(kind: ReviewLineKind): string {
  if (kind === "added") return "+";
  if (kind === "removed") return "−";
  return kind === "hunk" ? "·" : " ";
}
