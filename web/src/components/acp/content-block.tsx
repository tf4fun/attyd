import type { Annotations, ContentBlock } from "@agentclientprotocol/sdk";
import {
  Bot,
  CircleAlert,
  Clock3,
  Download,
  FileText,
  Gauge,
  Link2,
  UserRound,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { safeHttpUrl } from "../../lib/safe-url";
import { safeMediaDataUrl, validateContentBlockSemantics } from "../../../../shared/content-validation";
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

export function ContentBlockView({
  block,
  presentation = "message",
}: {
  block: ContentBlock;
  presentation?: "message" | "tool";
}) {
  switch (block.type) {
    case "text":
      return (
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
          {presentation === "tool" ? (
            <div className="structured-scalar structured-markdown">
              <div className="markdown">
                <Markdown remarkPlugins={[remarkGfm]}>{block.text}</Markdown>
              </div>
            </div>
          ) : (
            <div className="markdown">
              <Markdown remarkPlugins={[remarkGfm]}>{block.text}</Markdown>
            </div>
          )}
        </ContentBlockFrame>
      );
    case "image":
    case "audio":
      return (
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
          <BinaryAttachment data={block.data} mimeType={block.mimeType} mediaKind={block.type}
            uri={block.type === "image" ? block.uri ?? undefined : undefined} />
        </ContentBlockFrame>
      );
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
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
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
      if ("blob" in resource) {
        return (
          <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
            <BinaryAttachment data={resource.blob} mimeType={resource.mimeType ?? "application/octet-stream"} uri={resource.uri} />
          </ContentBlockFrame>
        );
      }
      return (
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
          <div className="embedded-resource">
            <div className="resource-heading">
              <FileText size={15} />
              <span title={resource.uri}>{resource.uri}</span>
              {resource.mimeType ? <code>{resource.mimeType}</code> : null}
            </div>
            <pre>{resource.text}</pre>
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
  showAnnotations = true,
}: {
  block: ContentBlock;
  children: ReactNode;
  showAnnotations?: boolean;
}) {
  return (
    <div className={`content-block content-block-${block.type}`}>
      {children}
      {showAnnotations ? <ContentAnnotations annotations={block.annotations} /> : null}
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

function BinaryAttachment({ data, mimeType, uri, mediaKind }: {
  data: string;
  mimeType: string;
  uri?: string;
  mediaKind?: "image" | "audio";
}) {
  const [failedPreview, setFailedPreview] = useState<string>();
  const [downloadError, setDownloadError] = useState(false);
  let valid = true;
  try {
    validateContentBlockSemantics(mediaKind
      ? { type: mediaKind, mimeType, data }
      : { type: "resource", resource: { uri: uri ?? "attachment:content", mimeType, blob: data } });
  } catch { valid = false; }
  const kind = mimeType.toLowerCase().startsWith("image/") ? "image"
    : mimeType.toLowerCase().startsWith("audio/") ? "audio" : undefined;
  const source = valid && kind ? safeMediaDataUrl({ type: kind, mimeType, data }) : undefined;
  const name = attachmentName(uri, mimeType);
  const bytes = data.length / 4 * 3 - (data.endsWith("==") ? 2 : data.endsWith("=") ? 1 : 0);
  const download = () => {
    if (!valid) return;
    setDownloadError(false);
    try {
      const decoded = Uint8Array.from(atob(data), (character) => character.charCodeAt(0));
      const url = URL.createObjectURL(new Blob([decoded], { type: mimeType }));
      const link = document.createElement("a");
      link.href = url;
      link.download = name;
      document.body.append(link);
      try { link.click(); } finally {
        link.remove();
        // Let the browser consume the click before releasing the temporary export.
        setTimeout(() => URL.revokeObjectURL(url), 0);
      }
    } catch { setDownloadError(true); }
  };
  return (
    <div className="binary-attachment">
      {source && source !== failedPreview ? (
        kind === "image"
          ? <figure className="media-block"><img src={source} alt={uri ? name : "ACP image content"} onError={() => setFailedPreview(source)} /></figure>
          : <audio controls preload="metadata" src={source} onError={() => setFailedPreview(source)} />
      ) : null}
      <div className={`resource-card${valid ? "" : " invalid-content"}`}>
        {valid ? <FileText size={16} /> : <CircleAlert size={16} />}
        <span>
          <strong title={uri}>{name}</strong>
          <small>{mimeType}{valid ? ` · ${formatBytes(bytes)}` : " · Invalid attachment data"}</small>
          {source && source === failedPreview ? <small>Preview unavailable. You can still download this attachment.</small> : null}
          {downloadError ? <small role="status">Download failed. Try again.</small> : null}
        </span>
        <button type="button" className="attachment-download" disabled={!valid} onClick={download} aria-label={`Download ${name}`}>
          <Download size={15} /> Download
        </button>
      </div>
    </div>
  );
}

function attachmentName(uri: string | undefined, mimeType: string): string {
  if (uri) {
    const segment = uri.split(/[?#]/, 1)[0].split("/").filter(Boolean).at(-1);
    if (segment) {
      let name = segment;
      try { name = decodeURIComponent(segment); } catch { /* Retain a malformed URI label. */ }
      name = name.replace(/[\/\\\u0000-\u001f\u007f]/g, "_");
      if (name !== "." && name !== "..") return name;
    }
  }
  const extension = mimeType.split(";", 1)[0].split("/")[1];
  return `attachment${extension && /^[a-z0-9]+$/i.test(extension) && extension !== "octet-stream" ? `.${extension}` : ""}`;
}
