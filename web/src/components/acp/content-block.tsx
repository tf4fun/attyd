import type { Annotations, ContentBlock } from "@agentclientprotocol/sdk";
import {
  Bot,
  ChevronRight,
  CircleAlert,
  Clock3,
  File,
  Gauge,
  Link2,
  Paperclip,
  UserRound,
} from "lucide-react";
import { useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { useTranslation } from "../../i18n";
import { safeHttpUrl } from "../../lib/safe-url";
import { attachmentHref } from "../../lib/hosted-attachment";
import { validateContentBlockSemantics } from "../../../../shared/content-validation";
import { assertNever } from "../../../../shared/exhaustive";

export function ContentBlocks({ blocks, presentation = "message" }: {
  blocks: ContentBlock[];
  presentation?: "message" | "prompt";
}) {
  return (
    <div className="content-blocks">
      {blocks.map((block, index) => (
        <ContentBlockView key={`${block.type}:${index}`} block={block} presentation={presentation} />
      ))}
    </div>
  );
}

export function ContentBlockView({
  block,
  presentation = "message",
}: {
  block: ContentBlock;
  presentation?: "message" | "tool" | "prompt";
}) {
  switch (block.type) {
    case "text":
      return (
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
          {presentation === "prompt" ? (
            <PromptText text={block.text} />
          ) : presentation === "tool" ? (
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
    case "resource":
    case "resource_link":
      return (
        <ContentBlockFrame block={block} showAnnotations={presentation !== "tool"}>
          <Attachment block={block} tool={presentation === "tool"} />
        </ContentBlockFrame>
      );
  }
  return assertNever(block, "ACP content block");
}

function PromptText({ text }: { text: string }) {
  const { t } = useTranslation("cards");
  const id = useId();
  const bodyRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const [collapsible, setCollapsible] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const markdown = useMemo(() => <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>, [text]);
  useLayoutEffect(() => {
    const body = bodyRef.current;
    const content = contentRef.current;
    if (!body || !content) return;
    const measure = () => {
      const style = getComputedStyle(body);
      // Share the CSS height budget; source length cannot predict Markdown layout.
      const limit = Number.parseFloat(style.lineHeight)
        * Number.parseFloat(style.getPropertyValue("--prompt-preview-lines"));
      const bounds = content.getBoundingClientRect();
      if (!bounds.width || !Number.isFinite(limit)) return;
      const overflow = bounds.height > limit + 1;
      setCollapsible(overflow);
      if (!overflow) setExpanded(false);
      body.dispatchEvent(new Event("toggle", { bubbles: true }));
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    // The inner content keeps its natural height while the outer body is clipped.
    const observer = new ResizeObserver(measure);
    observer.observe(content);
    return () => observer.disconnect();
  }, [text]);
  useLayoutEffect(() => {
    bodyRef.current?.dispatchEvent(new Event("toggle", { bubbles: true }));
  }, [expanded]);
  return (
    <div className="prompt-text" data-collapsible={collapsible} data-expanded={expanded}>
      <div className="prompt-text-body" id={id} ref={bodyRef}
        data-thread-search-clip={collapsible && !expanded || undefined}
        onFocusCapture={(event) => {
          if (collapsible && !expanded
            && event.target.getBoundingClientRect().bottom > event.currentTarget.getBoundingClientRect().bottom) {
            setExpanded(true);
          }
        }}
      >
        <div className="prompt-text-content markdown" ref={contentRef}>{markdown}</div>
      </div>
      {collapsible ? (
        <button
          type="button"
          className="prompt-text-toggle"
          aria-expanded={expanded}
          aria-controls={id}
          data-thread-search-ignore
          onClick={() => setExpanded((value) => !value)}
        >
          {t(expanded ? "content.collapseText" : "content.expandText")}
          <ChevronRight size={13} aria-hidden="true" />
        </button>
      ) : null}
    </div>
  );
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
    <div className={`content-block content-block-${block.type === "resource_link" ? "resource" : block.type}`}>
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
  const { t, i18n } = useTranslation("cards");
  if (
    annotations == null ||
    ((annotations.audience?.length ?? 0) === 0 &&
      annotations.priority == null &&
      annotations.lastModified == null)
  ) return null;

  const audience = [...new Set(annotations.audience ?? [])];
  const timestamp = annotations.lastModified == null
    ? undefined
    : readableTimestamp(annotations.lastModified, i18n.resolvedLanguage);
  return (
    <div
      className="content-annotations"
      aria-label={t("content.annotations")}
    >
      {audience.map((role) => (
        <span key={role} title={t("content.intendedFor", { role: t(`content.audience.${role}`) })}>
          {role === "user" ? <UserRound size={10} /> : <Bot size={10} />}
          {t(`content.audience.${role}`)}
        </span>
      ))}
      {annotations.priority != null ? (
        <span title={t("content.priorityLabel")}>
          <Gauge size={10} /> {t("content.priority", { value: formatPriority(annotations.priority, i18n.resolvedLanguage) })}
        </span>
      ) : null}
      {timestamp ? (
        <time dateTime={annotations.lastModified ?? undefined} title={annotations.lastModified ?? undefined}>
          <Clock3 size={10} /> {t("content.modified", { timestamp })}
        </time>
      ) : null}
    </div>
  );
}

function readableTimestamp(value: string, language: string | undefined): string {
  const milliseconds = Date.parse(value);
  if (!Number.isFinite(milliseconds)) return value;
  return new Intl.DateTimeFormat(language, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(milliseconds));
}

function formatPriority(value: number, language: string | undefined): string {
  return value.toLocaleString(language, {
    maximumFractionDigits: 3,
  });
}

function formatBytes(value: number, language: string | undefined): string {
  if (value < 1_024) return `${value.toLocaleString(language)} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let amount = value / 1_024;
  let unit = units[0];
  for (let index = 1; index < units.length && amount >= 1_024; index += 1) {
    amount /= 1_024;
    unit = units[index];
  }
  return `${amount.toLocaleString(language, { maximumFractionDigits: 1 })} ${unit}`;
}

function Attachment({ block, tool }: {
  block: Extract<ContentBlock, { type: "image" | "audio" | "resource" | "resource_link" }>;
  tool: boolean;
}) {
  const { t, i18n } = useTranslation("cards");
  const hostedHref = attachmentHref(block);
  const href = hostedHref ?? (block.type === "resource_link" ? safeHttpUrl(block.uri) : undefined);
  const resource = block.type === "resource" ? block.resource : block;
  const uri = "uri" in resource ? resource.uri ?? undefined : undefined;
  const text = "text" in resource ? resource.text : undefined;
  const data = "data" in resource ? resource.data : "blob" in resource ? resource.blob : "";
  const mimeType = resource.mimeType ?? (block.type === "resource_link" ? undefined
    : text == null ? "application/octet-stream" : "text/plain");
  const name = block.type === "resource_link" ? block.title ?? block.name
    : attachmentName(uri, mimeType ?? "application/octet-stream");
  const location = tool ? attachmentLocation(uri) : undefined;
  const bytes = useMemo(() => block.type === "resource_link" ? block.size : text != null
    ? new TextEncoder().encode(text).byteLength
    : data.length / 4 * 3 - (data.endsWith("==") ? 2 : data.endsWith("=") ? 1 : 0), [block, text, data]);
  const size = bytes == null ? undefined : formatBytes(bytes, i18n.resolvedLanguage);
  let valid = true;
  try {
    validateContentBlockSemantics(block);
  } catch { valid = false; }
  const Icon = !valid ? CircleAlert
    : block.type === "resource_link" && !hostedHref && !(tool && uri?.startsWith("file:")) ? Link2
    : tool ? File : Paperclip;
  const contents = (
    <>
      <Icon size={tool ? 12 : 14} aria-hidden="true" />
      <strong>{name}</strong>
      {location ? <span className="attachment-location">{location}</span> : null}
      {valid && size != null ? <small>{size}</small> : null}
    </>
  );
  const details = valid ? [mimeType, size].filter(Boolean).join(" · ")
    : t("content.invalidAttachment", { mimeType: mimeType ?? "application/octet-stream" });
  const title = [...new Set([
    name,
    block.type === "resource_link" ? block.name : undefined,
    uri,
    block.type === "resource_link" ? block.description : undefined,
    details,
  ].filter(Boolean))].join("\n");
  const props = {
    className: `attachment-chip${tool ? " attachment-row" : ""}${valid ? "" : " invalid-content"}`,
    title,
    "aria-description": title,
  };
  return href && valid
    ? <a {...props} href={href} target="_blank" rel="noopener noreferrer" aria-label={t("content.openNamed", { name })}>{contents}</a>
    : <span {...props} aria-label={name} aria-disabled="true">{contents}</span>;
}

function attachmentLocation(uri: string | undefined): string | undefined {
  if (!uri) return undefined;
  try {
    const url = new URL(uri);
    const path = url.protocol === "file:"
      ? `${url.host ? `//${url.host}` : ""}${url.pathname.slice(0, url.pathname.lastIndexOf("/") + 1)}`
      : url.protocol === "http:" || url.protocol === "https:" ? `${url.host}${url.pathname}` : undefined;
    if (!path) return undefined;
    try { return decodeURIComponent(path); } catch { return path; }
  } catch { return undefined; }
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
