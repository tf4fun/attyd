import {
  ArrowDownToLine,
  ArrowUpToLine,
  Bot,
  Brain,
  ChevronRight,
  CircleAlert,
  Copy,
  FileText,
  Pencil,
  RotateCcw,
  TextSelect,
} from "lucide-react";
import type { ContentBlock } from "@agentclientprotocol/sdk";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import i18n, { useTranslation } from "../../i18n";
import type {
  AgentActivity,
  AssistantMessageChunk,
  TimelineItem,
} from "../../lib/state";
import type { TerminalSnapshot } from "../../../../shared/bridge";
import { contentBlocksToMarkdown } from "../../lib/thread-markdown";
import { collectTurnReviewChanges, type ReviewSummary } from "../../lib/review-changes";
import { ChangeReview } from "./change-review";
import { ContentBlocks } from "./content-block";
import { CompactionCard } from "./compaction";
import { DebugInfoButton, DebugInfoPanel } from "./debug-info";
import { PlanCard } from "./plan";
import { RawJson } from "./raw-json";
import { ToolCallCard } from "./tool-call";

export function Conversation({
  timeline,
  terminalSnapshots = [],
  canReusePrompt = false,
  onReusePrompt,
  onRetryPrompt,
  agentActivity,
  onNavigateThread,
  onOpenThreadMarkdown,
}: {
  timeline: TimelineItem[];
  terminalSnapshots?: TerminalSnapshot[];
  canReusePrompt?: boolean;
  onReusePrompt?: (blocks: ContentBlock[]) => void;
  onRetryPrompt?: (blocks: ContentBlock[]) => void;
  agentActivity?: AgentActivity;
  onNavigateThread?: (target: "top" | "bottom") => void;
  onOpenThreadMarkdown?: () => void;
}) {
  const { t } = useTranslation("conversation");
  const turns = useMemo(() => collectTurnReviewChanges(timeline), [timeline]);
  if (timeline.length === 0) {
    return (
      <div className="empty-state">
        <div className="empty-mark"><Bot size={25} /></div>
        <h2>{t("empty.title")}</h2>
        <p>{t("empty.description")}</p>
      </div>
    );
  }

  return (
    <div className="conversation">
      {turns.flatMap((turn) => {
        // Prompt UUIDs are regenerated when authoritative history is hydrated.
        // Tool entry IDs retain their turn position, keeping open reviews mounted.
        const reviewId = turn.summary.files[0]?.diffs[0]?.id;
        return [
          ...turn.items.map((item) => (
            <TimelineEntry
              item={item}
              terminalSnapshots={terminalSnapshots}
              canReusePrompt={canReusePrompt}
              onReusePrompt={onReusePrompt}
              onRetryPrompt={onRetryPrompt}
              agentActivity={agentActivity}
              onNavigateThread={onNavigateThread}
              onOpenThreadMarkdown={onOpenThreadMarkdown}
              key={item.id}
            />
          )),
          ...(reviewId ? [
            <TurnChangeReview key={`changes:${reviewId}`} turnId={reviewId} summary={turn.summary} />,
          ] : []),
        ];
      })}
    </div>
  );
}

function TurnChangeReview({ turnId, summary }: { turnId: string; summary: ReviewSummary }) {
  const [open, setOpen] = useState(false);
  return (
    <ChangeReview
      entryId={`changes:${turnId}`}
      summary={summary}
      open={open}
      onToggle={() => setOpen((value) => !value)}
    />
  );
}

function TimelineEntry({
  item,
  terminalSnapshots,
  canReusePrompt,
  onReusePrompt,
  onRetryPrompt,
  agentActivity,
  onNavigateThread,
  onOpenThreadMarkdown,
}: {
  item: TimelineItem;
  terminalSnapshots: TerminalSnapshot[];
  canReusePrompt: boolean;
  onReusePrompt?: (blocks: ContentBlock[]) => void;
  onRetryPrompt?: (blocks: ContentBlock[]) => void;
  agentActivity?: AgentActivity;
  onNavigateThread?: (target: "top" | "bottom") => void;
  onOpenThreadMarkdown?: () => void;
}) {
  const { t, i18n } = useTranslation("conversation");
  if (item.type === "tool") {
    return <ToolCallCard item={item} terminalSnapshots={terminalSnapshots} />;
  }
  if (item.type === "plan") return <PlanCard entryId={item.id} update={item.update} />;
  if (item.type === "compaction") return <CompactionCard item={item} />;
  if (item.type === "protocol") {
    return (
      <div className="protocol-event" data-thread-entry data-thread-entry-id={item.id}>
        <span>{item.notification.update.sessionUpdate}</span>
        <RawJson label={t("debug.notification")} value={item.notification} />
      </div>
    );
  }
  if (item.type === "stop") {
    const usage = item.response.usage;
    return (
      <div className="turn-stop" data-stop-reason={item.response.stopReason} data-thread-entry data-thread-entry-id={item.id}>
        <span>{stopReasonLabel(item.response.stopReason)}</span>
        <code>{item.response.stopReason}</code>
        {usage ? (
          <span
            className="turn-usage"
            title={[
              t("usage.input", { value: usage.inputTokens.toLocaleString(i18n.language) }),
              t("usage.output", { value: usage.outputTokens.toLocaleString(i18n.language) }),
              usage.thoughtTokens != null
                ? t("usage.reasoning", { value: usage.thoughtTokens.toLocaleString(i18n.language) })
                : undefined,
              usage.cachedReadTokens != null
                ? t("usage.cacheRead", { value: usage.cachedReadTokens.toLocaleString(i18n.language) })
                : undefined,
              usage.cachedWriteTokens != null
                ? t("usage.cacheWrite", { value: usage.cachedWriteTokens.toLocaleString(i18n.language) })
                : undefined,
            ].filter(Boolean).join(" · ")}
          >
            {t("usage.tokens", { count: usage.totalTokens, value: usage.totalTokens.toLocaleString(i18n.language) })}
          </span>
        ) : null}
        <RawJson label={t("debug.turnResponse")} value={item.response} />
      </div>
    );
  }
  if (item.type === "error") {
    return <ErrorCard
      item={item}
      canRetry={canReusePrompt}
      onEdit={onReusePrompt}
      onRetry={onRetryPrompt}
    />;
  }
  if (item.type === "assistant") {
    return (
      <AssistantEntry
        item={item}
        agentActivity={agentActivity}
        onNavigateThread={onNavigateThread}
        onOpenThreadMarkdown={onOpenThreadMarkdown}
      />
    );
  }

  return (
    <MessageEntry
      item={item}
      canReusePrompt={canReusePrompt}
      onReusePrompt={onReusePrompt}
    />
  );
}

function stopReasonLabel(reason: Extract<TimelineItem, { type: "stop" }>["response"]["stopReason"]): string {
  switch (reason) {
    case "end_turn": return i18n.t("stop.end_turn", { ns: "conversation" });
    case "cancelled": return i18n.t("stop.cancelled", { ns: "conversation" });
    case "refusal": return i18n.t("stop.refusal", { ns: "conversation" });
    case "max_tokens": return i18n.t("stop.max_tokens", { ns: "conversation" });
    case "max_turn_requests": return i18n.t("stop.max_turn_requests", { ns: "conversation" });
  }
}

function ErrorCard({
  item,
  canRetry,
  onEdit,
  onRetry,
}: {
  item: Extract<TimelineItem, { type: "error" }>;
  canRetry: boolean;
  onEdit?: (blocks: ContentBlock[]) => void;
  onRetry?: (blocks: ContentBlock[]) => void;
}) {
  const { t } = useTranslation("conversation");
  const retryable = item.retryBlocks != null && item.retryBlocks.length > 0;
  const details = item.code != null || item.data !== undefined || item.dataTruncated
    ? {
        ...(item.code != null ? { code: item.code } : {}),
        message: item.message,
        ...(item.data !== undefined ? { data: item.data } : {}),
        ...(item.dataBytes != null ? { dataBytes: item.dataBytes } : {}),
        ...(item.dataTruncated ? { dataTruncated: true } : {}),
      }
    : undefined;
  return (
    <section
      className="error-card"
      role="alert"
      data-thread-entry
      data-thread-entry-id={item.id}
    >
      <CircleAlert size={16} aria-hidden="true" />
      <div className="error-card-body">
        <header>
          <strong>{item.operation === "session/prompt" ? t("errors.agentTurnFailed") : t("errors.requestFailed")}</strong>
          {item.code != null ? <code>{item.code}</code> : null}
        </header>
        <p data-thread-searchable>{item.message}</p>
        {retryable ? (
          <div className="error-card-actions">
            {onRetry ? (
              <button
                type="button"
                disabled={!canRetry}
                onClick={() => onRetry(item.retryBlocks!)}
              ><RotateCcw size={12} /> {t("errors.retry")}</button>
            ) : null}
            {onEdit ? (
              <button
                type="button"
                disabled={!canRetry}
                onClick={() => onEdit(item.retryBlocks!)}
              ><Pencil size={12} /> {t("errors.editPrompt")}</button>
            ) : null}
          </div>
        ) : null}
        {details ? <RawJson label={t("debug.errorDetails")} value={details} /> : null}
      </div>
    </section>
  );
}

function MessageEntry({
  item,
  canReusePrompt,
  onReusePrompt,
}: {
  item: Extract<TimelineItem, { type: "message" }>;
  canReusePrompt: boolean;
  onReusePrompt?: (blocks: ContentBlock[]) => void;
}) {
  const { t } = useTranslation("conversation");
  const isProtocolUser = item.role === "protocol-user";
  const canReuse = (item.role === "user" || item.role === "protocol-user") && onReusePrompt != null;

  return (
      <article
        className={`message message-${item.role}${isProtocolUser ? " message-user" : ""}`}
        data-thread-entry
        data-thread-entry-id={item.id}
        data-thread-role="user"
      >
        <div className="message-shell">
          <div className="message-content" data-thread-searchable>
            <ContentBlocks blocks={item.blocks} />
          </div>
          <MessageMeta messageId={item.messageId} events={item.raw}>
            {canReuse ? (
              <button
                type="button"
                className="message-action message-icon-action"
                aria-label={t("message.editAndResendLabel")}
                title={t("message.editAndResend")}
                disabled={!canReusePrompt}
                onClick={() => onReusePrompt(item.blocks)}
              ><Pencil size={12} aria-hidden="true" /></button>
            ) : null}
          </MessageMeta>
        </div>
      </article>
  );
}

function AssistantEntry({
  item,
  agentActivity,
  onNavigateThread,
  onOpenThreadMarkdown,
}: {
  item: Extract<TimelineItem, { type: "assistant" }>;
  agentActivity?: AgentActivity;
  onNavigateThread?: (target: "top" | "bottom") => void;
  onOpenThreadMarkdown?: () => void;
}) {
  const { t } = useTranslation("conversation");
  const article = useRef<HTMLElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    selection?: string;
  }>();
  const answerBlocks = item.chunks.flatMap((chunk) =>
    chunk.role === "agent" ? chunk.blocks : []
  );
  const lastAnswerChunk = [...item.chunks].reverse().find((chunk) => chunk.role === "agent");

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(undefined);
    const closeOutside = (event: PointerEvent) => {
      if (!menu.current?.contains(event.target as Node)) close();
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        close();
        article.current?.focus();
        return;
      }
      if (!menu.current || !["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
      const items = [...menu.current.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')];
      if (items.length === 0) return;
      event.preventDefault();
      const current = items.indexOf(document.activeElement as HTMLButtonElement);
      const next = event.key === "Home"
        ? 0
        : event.key === "End"
          ? items.length - 1
          : (current + (event.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
      items[next]?.focus();
    };
    const frame = requestAnimationFrame(() =>
      menu.current?.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus()
    );
    window.addEventListener("pointerdown", closeOutside);
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("resize", close);
    window.addEventListener("scroll", close, true);
    return () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("pointerdown", closeOutside);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("resize", close);
      window.removeEventListener("scroll", close, true);
    };
  }, [contextMenu]);

  const openContextMenu = (x: number, y: number, selectionRoot: HTMLElement | null) => {
    setContextMenu({
      x: Math.max(6, Math.min(x, window.innerWidth - 220)),
      y: Math.max(6, Math.min(y, window.innerHeight - 210)),
      selection: selectedMessageText(selectionRoot),
    });
  };
  const runMenuAction = (action: () => void | Promise<void>) => {
    setContextMenu(undefined);
    void action();
  };

  return (
    <>
      <article
        ref={article}
        className="assistant-entry"
        data-thread-entry
        data-thread-entry-id={item.id}
        data-thread-role="agent"
        tabIndex={answerBlocks.length > 0 ? 0 : undefined}
        onContextMenu={(event) => {
          const response = (event.target as Element).closest<HTMLElement>(".assistant-chunk");
          if (!response || !event.currentTarget.contains(response)) return;
          event.preventDefault();
          openContextMenu(event.clientX, event.clientY, response);
        }}
        onKeyDown={(event) => {
          if (event.key !== "ContextMenu" && !(event.shiftKey && event.key === "F10")) return;
          event.preventDefault();
          const bounds = event.currentTarget.getBoundingClientRect();
          openContextMenu(
            bounds.left + 18,
            bounds.top + 18,
            lastAssistantResponse(event.currentTarget),
          );
        }}
      >
        {item.chunks.map((chunk) => chunk.role === "thought" ? (
          <ThinkingBlock
            key={chunk.id}
            item={chunk}
            live={agentActivity?.kind === "thinking" && agentActivity.timelineId === chunk.id}
          />
        ) : (
          <AssistantChunk
            key={chunk.id}
            item={chunk}
            copyableBlocks={chunk.id === lastAnswerChunk?.id ? answerBlocks : undefined}
          />
        ))}
      </article>
      {contextMenu && typeof document !== "undefined" ? createPortal(
        <div
          ref={menu}
          className="message-context-menu"
          role="menu"
          aria-label={t("message.responseActions")}
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {contextMenu.selection ? (
            <button type="button" role="menuitem" onClick={() => runMenuAction(() => copyText(contextMenu.selection!))}>
              <TextSelect size={13} /> {t("message.copySelection")}
            </button>
          ) : null}
          {answerBlocks.length > 0 ? (
            <button type="button" role="menuitem" onClick={() => runMenuAction(() => copyBlocks(answerBlocks))}>
              <Copy size={13} /> {t("message.copyThisResponse")}
            </button>
          ) : null}
          {onNavigateThread ? (
            <>
              <div role="separator" />
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onNavigateThread("top"))}>
                <ArrowUpToLine size={13} /> {t("message.scrollTop")}
              </button>
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onNavigateThread("bottom"))}>
                <ArrowDownToLine size={13} /> {t("message.scrollBottom")}
              </button>
            </>
          ) : null}
          {onOpenThreadMarkdown ? (
            <>
              <div role="separator" />
              <button type="button" role="menuitem" onClick={() => runMenuAction(onOpenThreadMarkdown)}>
                <FileText size={13} /> {t("message.openMarkdown")}
              </button>
            </>
          ) : null}
        </div>,
        document.body,
      ) : null}
    </>
  );
}

function AssistantChunk({
  item,
  copyableBlocks,
}: {
  item: AssistantMessageChunk;
  copyableBlocks?: ContentBlock[];
}) {
  const { t } = useTranslation("conversation");
  return (
    <div className="message message-agent assistant-chunk">
      <div className="message-shell">
        <div className="message-content" data-thread-searchable>
          <ContentBlocks blocks={item.blocks} />
        </div>
        <MessageMeta messageId={item.messageId} events={item.raw}>
          {copyableBlocks ? (
            <button
              type="button"
              className="message-action message-icon-action"
              aria-label={t("message.copyResponseLabel")}
              title={t("message.copyResponse")}
              onClick={() => void copyBlocks(copyableBlocks)}
            ><Copy size={12} aria-hidden="true" /></button>
          ) : null}
        </MessageMeta>
      </div>
    </div>
  );
}

function ThinkingBlock({
  item,
  live,
}: {
  item: AssistantMessageChunk;
  live: boolean;
}) {
  const { t } = useTranslation("conversation");
  const [open, setOpen] = useState(live);
  const [debugOpen, setDebugOpen] = useState(false);
  const disclosure = useRef<HTMLElement>(null);
  const disclosureMounted = useRef(false);
  useEffect(() => {
    setOpen(live);
    if (!live) setDebugOpen(false);
  }, [live]);
  useEffect(() => {
    if (!disclosureMounted.current) {
      disclosureMounted.current = true;
      return;
    }
    disclosure.current?.dispatchEvent(new Event("toggle"));
  }, [open]);
  return (
    <section
      ref={disclosure}
      className={live ? "thinking-block thinking-live" : "thinking-block"}
      data-thread-entry
      data-thread-entry-id={item.id}
      data-thread-role="thought"
      data-live={live ? "true" : "false"}
      data-open={open ? "true" : "false"}
    >
      <header className="thinking-header">
        <button
          type="button"
          className="thinking-disclosure"
          aria-expanded={open}
          onClick={() => {
            setOpen((value) => {
              if (value) setDebugOpen(false);
              return !value;
            });
          }}
        >
          <span className="thinking-icon"><Brain size={14} aria-hidden="true" /></span>
          <span className="thinking-title">{live ? t("thinking.liveTitle") : t("thinking.title")}</span>
        </button>
        <span className="thinking-actions">
          {live ? <i>{t("thinking.live")}</i> : null}
          <button
            type="button"
            className="component-disclosure-button"
            aria-label={open ? t("thinking.collapse") : t("thinking.expand")}
            aria-expanded={open}
            onClick={() => {
              setOpen((value) => {
                if (value) setDebugOpen(false);
                return !value;
              });
            }}
          ><ChevronRight size={13} aria-hidden="true" /></button>
        </span>
      </header>
      <div className="thinking-body" hidden={!open}>
        <div className="thinking-content" data-thread-searchable>
          <ContentBlocks blocks={item.blocks} />
        </div>
        <footer className={debugOpen ? "message-meta message-meta-open component-debug-meta" : "message-meta component-debug-meta"}>
          <div className="message-meta-actions">
            <DebugInfoButton
              expanded={debugOpen}
              label={t("thinking.info")}
              onClick={() => setDebugOpen((value) => !value)}
            />
          </div>
          <DebugInfoPanel
            label={t("thinking.debug")}
            hidden={!debugOpen}
            entries={[
              { label: t("debug.messageId"), value: item.messageId, format: "text" },
              { label: t("debug.messageEvents"), value: item.raw, count: item.raw.length },
            ]}
          />
        </footer>
      </div>
    </section>
  );
}

function MessageMeta({
  messageId,
  events,
  children,
}: {
  messageId?: string | null;
  events: unknown[];
  children?: ReactNode;
}) {
  const { t } = useTranslation("conversation");
  const [open, setOpen] = useState(false);

  return (
    <footer className={open ? "message-meta message-meta-open" : "message-meta"}>
      <div className="message-meta-actions">
        <DebugInfoButton
          expanded={open}
          label={t("message.info")}
          onClick={() => setOpen((value) => !value)}
        />
        {children}
      </div>
      <DebugInfoPanel
        label={t("message.debug")}
        hidden={!open}
        entries={[
          { label: t("debug.messageId"), value: messageId, format: "text" },
          { label: t("debug.messageEvents"), value: events, count: events.length },
        ]}
      />
    </footer>
  );
}

async function copyBlocks(blocks: ContentBlock[]): Promise<void> {
  return copyText(contentBlocksToMarkdown(blocks));
}

async function copyText(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // Fall through to the local selection-based copy path.
    }
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  } catch {
    // Copy is a convenience action; a denied browser permission must not break the thread.
  }
}

function selectedMessageText(article: HTMLElement | null): string | undefined {
  const selection = window.getSelection();
  if (!article || !selection || selection.isCollapsed || !selection.anchorNode || !selection.focusNode) {
    return undefined;
  }
  if (!article.contains(selection.anchorNode) || !article.contains(selection.focusNode)) {
    return undefined;
  }
  const text = selection.toString().trim();
  return text || undefined;
}

function lastAssistantResponse(article: HTMLElement | null): HTMLElement | null {
  const chunks = article?.querySelectorAll<HTMLElement>(".assistant-chunk");
  if (!chunks?.length) return null;
  return chunks.item(chunks.length - 1);
}
