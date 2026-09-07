import type { ReviewDiffResult, ReviewLineKind } from "../../lib/review-changes";

export function DiffLines({
  path,
  diff,
}: {
  path: string;
  diff: Pick<ReviewDiffResult, "lines" | "approximate" | "truncated">;
}) {
  return (
    <>
      <div className="change-review-lines" role="table" aria-label={`Read-only diff for ${path}`}>
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
        <footer>{diff.approximate ? "Large file: change counts are approximate." : ""}{diff.approximate && diff.truncated ? " " : ""}{diff.truncated ? "Some diff lines are omitted." : ""}</footer>
      ) : null}
    </>
  );
}

function lineMarker(kind: ReviewLineKind): string {
  if (kind === "added") return "+";
  if (kind === "removed") return "−";
  return kind === "hunk" ? "·" : " ";
}
