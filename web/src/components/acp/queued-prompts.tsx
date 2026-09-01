import type { ContentBlock } from "@agentclientprotocol/sdk";
import {
  Clock3,
  FastForward,
  FileText,
  Image,
  Link2,
  Music2,
  Pencil,
  Trash2,
  X,
} from "lucide-react";
import { assertNever } from "../../../../shared/exhaustive";

export const MAX_QUEUED_PROMPTS = 8;

export interface QueuedPrompt {
  id: string;
  sessionId: string;
  blocks: ContentBlock[];
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
}: {
  prompts: QueuedPrompt[];
  error?: string;
  paused?: boolean;
  canSendNow: boolean;
  onEdit: (prompt: QueuedPrompt) => void;
  onRemove: (id: string) => void;
  onClear: () => void;
  onSendNow: (id: string) => void;
}) {
  if (prompts.length === 0 && !error) return null;
  return (
    <section className="queued-prompts" aria-label="Queued messages">
      <header>
        <span><Clock3 size={12} /> {prompts.length} queued{paused ? " · paused" : ""}</span>
        {prompts.length > 0 ? (
          <button type="button" aria-label="Clear queued messages" onClick={onClear}>
            <X size={11} /> Clear
          </button>
        ) : null}
      </header>
      {error ? <p role="alert">{error}</p> : null}
      <div className="queued-prompt-list">
        {prompts.map((prompt, index) => (
          <article key={prompt.id} aria-label={`Queued message ${index + 1}`}>
            <span className="queue-index">{index + 1}</span>
            <div className="queued-prompt-preview">
              {prompt.blocks.map((block, blockIndex) => (
                <QueuedBlock block={block} key={`${block.type}:${blockIndex}`} />
              ))}
            </div>
            <footer>
              <span>{paused ? "Paused" : "Queued"}</span>
              <button
                type="button"
                aria-label={`Edit queued message ${index + 1}`}
                title="Edit queued message"
                onClick={() => onEdit(prompt)}
              ><Pencil size={11} /> Edit</button>
              <button
                type="button"
                aria-label={`Send queued message ${index + 1} now`}
                title="Cancel the current ACP turn, then send this message"
                disabled={!canSendNow}
                onClick={() => onSendNow(prompt.id)}
              ><FastForward size={11} /> Send now</button>
              <button
                type="button"
                aria-label={`Remove queued message ${index + 1}`}
                title="Remove queued message"
                onClick={() => onRemove(prompt.id)}
              ><Trash2 size={11} /> Remove</button>
            </footer>
          </article>
        ))}
      </div>
    </section>
  );
}

function QueuedBlock({ block }: { block: ContentBlock }) {
  switch (block.type) {
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
