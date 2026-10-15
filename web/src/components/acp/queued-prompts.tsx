import {
  Clock3,
  FastForward,
  FileText,
  Image,
  Link2,
  Music2,
  Pencil,
  RotateCcw,
  Trash2,
  X,
} from "lucide-react";
import { assertNever } from "../../../../shared/exhaustive";
import { useTranslation } from "../../i18n";
import type { PromptBlock } from "../../../../shared/protocol";


export interface QueuedPrompt {
  id: string;
  sessionId: string;
  blocks: PromptBlock[];
  /**
   * Where the prompt is: waiting, being sent, refused (sent again once the
   * turn ends, C2), or sent without a definite answer, which is never sent
   * again unless the user asks (C3). Absent means waiting.
   */
  state?: "queued" | "paused" | "sending" | "refused" | "uncertain";
  error?: string;
}

export function QueuedPrompts({
  prompts,
  error,
  paused = false,
  canSendNow,
  onEdit,
  onRemove,
  onClear,
  onSendNow,
  onResend,
}: {
  prompts: QueuedPrompt[];
  error?: string;
  paused?: boolean;
  canSendNow: boolean;
  onEdit: (prompt: QueuedPrompt) => void;
  onRemove: (id: string) => void;
  onClear: () => void;
  onSendNow: (id: string) => void;
  onResend?: (id: string) => void;
}) {
  const { t } = useTranslation("workspace");
  if (prompts.length === 0 && !error) return null;
  const uncertain = prompts.filter(({ state }) => state === "uncertain");
  const waiting = prompts.filter(({ state }) => state !== "uncertain");
  return (
    <section className="queued-prompts" aria-label={t("queue.label")}>
      <header>
        <span><Clock3 size={12} /> {paused
          ? t("queue.pausedCount", { count: waiting.length })
          : t("queue.count", { count: waiting.length })}</span>
        {prompts.length > 0 ? (
          <button type="button" aria-label={t("queue.clearLabel")} onClick={onClear}>
            <X size={11} /> {t("queue.clear")}
          </button>
        ) : null}
      </header>
      {error ? <p role="alert">{error}</p> : null}
      {uncertain.map((prompt) => {
        const index = prompts.indexOf(prompt);
        return (
          <article key={prompt.id} className="queued-prompt-uncertain" aria-label={t("queue.messageLabel", { index: index + 1 })}>
            <p role="status">{t("queue.uncertain")}</p>
            <div className="queued-prompt-preview">
              {prompt.blocks.map((block, blockIndex) => (
                <QueuedBlock block={block} key={`${block.type}:${blockIndex}`} />
              ))}
            </div>
            <footer>
              {onResend ? (
                <button type="button" aria-label={t("queue.resendLabel", { index: index + 1 })}
                  onClick={() => onResend(prompt.id)}><RotateCcw size={11} /> {t("queue.resend")}</button>
              ) : null}
              <button type="button" aria-label={t("queue.dismissLabel", { index: index + 1 })}
                onClick={() => onRemove(prompt.id)}><X size={11} /> {t("queue.dismiss")}</button>
            </footer>
          </article>
        );
      })}
      <div className="queued-prompt-list">
        {waiting.map((prompt) => {
          const index = prompts.indexOf(prompt);
          const sending = prompt.state === "sending";
          return (
          <article key={prompt.id} aria-label={t("queue.messageLabel", { index: index + 1 })}>
            <span className="queue-index">{index + 1}</span>
            <div className="queued-prompt-preview">
              {prompt.blocks.map((block, blockIndex) => (
                <QueuedBlock block={block} key={`${block.type}:${blockIndex}`} />
              ))}
            </div>
            <footer>
              <span>{sending ? t("queue.sending")
                : prompt.state === "refused" ? t("queue.refused", { message: prompt.error ?? "" })
                : paused || prompt.state === "paused" ? t("queue.paused") : t("queue.queued")}</span>
              <button
                type="button"
                aria-label={t("queue.editLabel", { index: index + 1 })}
                title={t("queue.editTitle")}
                disabled={sending}
                onClick={() => onEdit(prompt)}
              ><Pencil size={11} /> {t("queue.edit")}</button>
              <button
                type="button"
                aria-label={t("queue.sendNowLabel", { index: index + 1 })}
                title={t("queue.sendNowTitle")}
                disabled={!canSendNow || sending}
                onClick={() => onSendNow(prompt.id)}
              ><FastForward size={11} /> {t("queue.sendNow")}</button>
              <button
                type="button"
                aria-label={t("queue.removeLabel", { index: index + 1 })}
                title={t("queue.removeTitle")}
                disabled={sending}
                onClick={() => onRemove(prompt.id)}
              ><Trash2 size={11} /> {t("queue.remove")}</button>
            </footer>
          </article>
          );
        })}
      </div>
    </section>
  );
}

function QueuedBlock({ block }: { block: PromptBlock }) {
  switch (block.type) {
    case "attachment":
      return <span className="queued-attachment"><FileText size={11} /> {block.uri ?? block.mimeType ?? block.block}</span>;
    case "text":
      return <p>{block.text}</p>;
    case "image":
      return <span className="queued-attachment"><Image size={11} /> {block.uri ?? block.mimeType}</span>;
    case "audio":
      return <span className="queued-attachment"><Music2 size={11} /> {block.mimeType}</span>;
    case "resource_link":
      return <span className="queued-attachment"><Link2 size={11} /> {block.title ?? block.name}</span>;
    case "resource":
      return <span className="queued-attachment"><FileText size={11} /> {block.resource.uri}</span>;
  }
  return assertNever(block, "queued ACP content block");
}
