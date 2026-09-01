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
import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type {
  AgentActivity,
  AssistantMessageChunk,
  TimelineItem,
} from "../../lib/state";
import type { TerminalSnapshot } from "../../../../shared/bridge";
import { contentBlocksToMarkdown } from "../../lib/thread-markdown";
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
  if (timeline.length === 0) {
    return (
      <div className="empty-state">
        <div className="empty-mark"><Bot size={25} /></div>
        <h2>Agent, not just a model.</h2>
        <p>Send a task. attyd will render the ACP session exactly as the agent reports it.</p>
      </div>
    );
  }

  return (
    <div className="conversation">
      {timeline.map((item) => (
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
      ))}
    </div>
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
  if (item.type === "tool") {
    return <ToolCallCard item={item} terminalSnapshots={terminalSnapshots} />;
  }
  if (item.type === "plan") return <PlanCard entryId={item.id} update={item.update} />;
  if (item.type === "compaction") return <CompactionCard item={item} />;
  if (item.type === "protocol") {
    return (
      <div className="protocol-event" data-thread-entry data-thread-entry-id={item.id}>
        <span>{item.notification.update.sessionUpdate}</span>
        <RawJson label="Notification" value={item.notification} />
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
              `${usage.inputTokens.toLocaleString()} input`,
              `${usage.outputTokens.toLocaleString()} output`,
              usage.thoughtTokens != null
                ? `${usage.thoughtTokens.toLocaleString()} reasoning`
                : undefined,
              usage.cachedReadTokens != null
                ? `${usage.cachedReadTokens.toLocaleString()} cache read`
                : undefined,
              usage.cachedWriteTokens != null
                ? `${usage.cachedWriteTokens.toLocaleString()} cache write`
                : undefined,
            ].filter(Boolean).join(" · ")}
          >
            {usage.totalTokens.toLocaleString()} tokens
          </span>
        ) : null}
        <RawJson label="Turn response" value={item.response} />
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
    case "end_turn": return "turn complete";
    case "cancelled": return "turn cancelled";
    case "refusal": return "request refused";
    case "max_tokens": return "token limit reached";
    case "max_turn_requests": return "turn request limit reached";
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
          <strong>{item.operation === "session/prompt" ? "Agent turn failed" : "ACP request failed"}</strong>
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
              ><RotateCcw size={12} /> Retry</button>
            ) : null}
            {onEdit ? (
              <button
                type="button"
                disabled={!canRetry}
                onClick={() => onEdit(item.retryBlocks!)}
              ><Pencil size={12} /> Edit prompt</button>
            ) : null}
          </div>
        ) : null}
        {details ? <RawJson label="ACP error details" value={details} /> : null}
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
                aria-label="Edit and resend user message"
                title="Edit and resend"
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
          aria-label="Agent response actions"
          style={{ left: contextMenu.x, top: contextMenu.y }}
        >
          {contextMenu.selection ? (
            <button type="button" role="menuitem" onClick={() => runMenuAction(() => copyText(contextMenu.selection!))}>
              <TextSelect size={13} /> Copy Selection
            </button>
          ) : null}
          {answerBlocks.length > 0 ? (
            <button type="button" role="menuitem" onClick={() => runMenuAction(() => copyBlocks(answerBlocks))}>
              <Copy size={13} /> Copy This Agent Response
            </button>
          ) : null}
          {onNavigateThread ? (
            <>
              <div role="separator" />
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onNavigateThread("top"))}>
                <ArrowUpToLine size={13} /> Scroll to Top
              </button>
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onNavigateThread("bottom"))}>
                <ArrowDownToLine size={13} /> Scroll to Bottom
              </button>
            </>
          ) : null}
          {onOpenThreadMarkdown ? (
            <>
              <div role="separator" />
              <button type="button" role="menuitem" onClick={() => runMenuAction(onOpenThreadMarkdown)}>
                <FileText size={13} /> Open Thread as Markdown
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
              aria-label="Copy agent response"
              title="Copy response"
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
          <span className="thinking-title">{live ? "Thinking…" : "Thinking"}</span>
        </button>
        <span className="thinking-actions">
          {live ? <i>Live</i> : null}
          <button
            type="button"
            className="component-disclosure-button"
            aria-label={open ? "Collapse thinking" : "Expand thinking"}
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
              label="Thinking info"
              onClick={() => setDebugOpen((value) => !value)}
            />
          </div>
          <DebugInfoPanel
            label="Thinking debug information"
            hidden={!debugOpen}
            entries={[
              { label: "Message ID", value: item.messageId, format: "text" },
              { label: "Message events", value: item.raw, count: item.raw.length },
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
  const [open, setOpen] = useState(false);

  return (
    <footer className={open ? "message-meta message-meta-open" : "message-meta"}>
      <div className="message-meta-actions">
        <DebugInfoButton
          expanded={open}
          label="Message info"
          onClick={() => setOpen((value) => !value)}
        />
        {children}
      </div>
      <DebugInfoPanel
        label="Message debug information"
        hidden={!open}
        entries={[
          { label: "Message ID", value: messageId, format: "text" },
          { label: "Message events", value: events, count: events.length },
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
