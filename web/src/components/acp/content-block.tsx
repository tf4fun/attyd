import type { Annotations, ContentBlock } from "@agentclientprotocol/sdk";
import {
  Bot,
  CircleAlert,
  Clock3,
  FileText,
  Gauge,
  Link2,
  Music2,
  UserRound,
} from "lucide-react";
import type { ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { safeHttpUrl } from "../../lib/safe-url";
import { safeMediaDataUrl } from "../../../../shared/content-validation";
import { assertNever } from "../../../../shared/exhaustive";

export function ContentBlocks({ blocks }: { blocks: ContentBlock[] }) {
  return (
    <>
      {blocks.map((block, index) => (
        <ContentBlockView key={`${block.type}:${index}`} block={block} />
      ))}
    </>
  );
}

export function ContentBlockView({ block }: { block: ContentBlock }) {
  switch (block.type) {
    case "text":
      return (
        <ContentBlockFrame block={block}>
          <div className="markdown">
            <Markdown remarkPlugins={[remarkGfm]}>{block.text}</Markdown>
          </div>
        </ContentBlockFrame>
      );
    case "image": {
      const source = safeMediaDataUrl(block);
      if (!source) {
        return <ContentBlockFrame block={block}><InvalidMedia kind="image" /></ContentBlockFrame>;
      }
      return (
        <ContentBlockFrame block={block}>
          <figure className="media-block">
            <img src={source} alt="ACP image content" />
            <figcaption>
              <span>{block.mimeType}</span>
              {block.uri ? <code title={block.uri}>{block.uri}</code> : null}
            </figcaption>
          </figure>
        </ContentBlockFrame>
      );
    }
    case "audio": {
      const source = safeMediaDataUrl(block);
      if (!source) {
        return <ContentBlockFrame block={block}><InvalidMedia kind="audio" /></ContentBlockFrame>;
      }
      return (
        <ContentBlockFrame block={block}>
          <div className="resource-card audio-resource">
            <Music2 size={16} />
            <span><strong>Audio</strong><small>{block.mimeType}</small></span>
            <audio controls src={source} />
          </div>
        </ContentBlockFrame>
      );
    }
    case "resource_link": {
      const details = [
        block.mimeType,
        block.size != null ? formatBytes(block.size) : undefined,
      ].filter(Boolean).join(" · ");
      const contents = (
        <>
          <Link2 size={16} />
          <span>
            <strong>{block.title ?? block.name}</strong>
            {block.description ? (
              <small className="resource-description">{block.description}</small>
            ) : null}
            <small title={block.uri}>{block.uri}</small>
            {details ? <em>{details}</em> : null}
          </span>
        </>
      );
      const href = safeHttpUrl(block.uri);
      return (
        <ContentBlockFrame block={block}>
          {href ? (
            <a className="resource-card" href={href} target="_blank" rel="noreferrer">
              {contents}
            </a>
          ) : (
            <div className="resource-card">{contents}</div>
          )}
        </ContentBlockFrame>
      );
    }
    case "resource": {
      const resource = block.resource;
      return (
        <ContentBlockFrame block={block}>
          <div className="embedded-resource">
            <div className="resource-heading">
              <FileText size={15} />
              <span title={resource.uri}>{resource.uri}</span>
              {resource.mimeType ? <code>{resource.mimeType}</code> : null}
            </div>
            {"text" in resource ? (
              <pre>{resource.text}</pre>
            ) : (
              <small>
                Embedded {resource.mimeType ?? "binary resource"} · {resource.blob.length} base64
                characters
              </small>
            )}
          </div>
        </ContentBlockFrame>
      );
    }
  }
  return assertNever(block, "ACP content block");
}

function ContentBlockFrame({
  block,
  children,
}: {
  block: ContentBlock;
  children: ReactNode;
}) {
  return (
    <div className={`content-block content-block-${block.type}`}>
      {children}
      <ContentAnnotations annotations={block.annotations} />
    </div>
  );
}

function ContentAnnotations({
  annotations,
}: {
  annotations?: Annotations | null;
}) {
  if (
    annotations == null ||
    ((annotations.audience?.length ?? 0) === 0 &&
      annotations.priority == null &&
      annotations.lastModified == null)
  ) return null;

  const audience = [...new Set(annotations.audience ?? [])];
  const timestamp = annotations.lastModified == null
    ? undefined
    : readableTimestamp(annotations.lastModified);
  return (
    <div
      className="content-annotations"
      aria-label="ACP content annotations"
    >
      {audience.map((role) => (
        <span key={role} title={`Intended for ${role}`}>
          {role === "user" ? <UserRound size={10} /> : <Bot size={10} />}
          {role}
        </span>
      ))}
      {annotations.priority != null ? (
        <span title="ACP content priority">
          <Gauge size={10} /> priority {formatPriority(annotations.priority)}
        </span>
      ) : null}
      {timestamp ? (
        <time dateTime={annotations.lastModified ?? undefined} title={annotations.lastModified ?? undefined}>
          <Clock3 size={10} /> modified {timestamp}
        </time>
      ) : null}
    </div>
  );
}

function readableTimestamp(value: string): string {
  const milliseconds = Date.parse(value);
  if (!Number.isFinite(milliseconds)) return value;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(milliseconds));
}

function formatPriority(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toLocaleString(undefined, {
    maximumFractionDigits: 3,
  });
}

function formatBytes(value: number): string {
  if (value < 1_024) return `${value} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let amount = value / 1_024;
  let unit = units[0];
  for (let index = 1; index < units.length && amount >= 1_024; index += 1) {
    amount /= 1_024;
    unit = units[index];
  }
  return `${amount.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${unit}`;
}

function InvalidMedia({ kind }: { kind: "image" | "audio" }) {
  return (
    <div className="resource-card invalid-content" role="status">
      <CircleAlert size={16} />
      <span>
        <strong>Invalid ACP {kind} content</strong>
        <small>The media payload was not rendered.</small>
      </span>
    </div>
  );
}
