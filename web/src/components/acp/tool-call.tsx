import type { TFunction } from "i18next";
import type { ToolCall, ToolCallContent, ToolKind } from "@agentclientprotocol/sdk";
import {
  Check,
  ChevronRight,
  CircleAlert,
  CircleMinus,
  FilePenLine,
  FileSearch,
  Globe2,
  Lightbulb,
  LoaderCircle,
  Move,
  Play,
  TerminalSquare,
  Trash2,
  Wrench,
} from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "../../i18n";
import type { TimelineItem } from "../../lib/state";
import type { TerminalSnapshot } from "../../../../shared/bridge";
import { ContentBlockView } from "./content-block";
import { DebugInfoButton, DebugInfoPanel, StructuredData } from "./debug-info";
import { FileDiff } from "./file-diff";

const kindIcons: Record<ToolKind, typeof Wrench> = {
  read: FileSearch,
  edit: FilePenLine,
  delete: Trash2,
  move: Move,
  search: FileSearch,
  execute: Play,
  think: Lightbulb,
  fetch: Globe2,
  switch_mode: Wrench,
  other: Wrench,
};

export function ToolCallCard({
  item,
  terminalSnapshots = [],
}: {
  item: Extract<TimelineItem, { type: "tool" }>;
  terminalSnapshots?: TerminalSnapshot[];
}) {
  const { t } = useTranslation("cards");
  const { call } = item;
  const Icon = kindIcons[call.kind ?? "other"] ?? Wrench;
  const status = item.cancelled ? "cancelled" : call.status;
  const live = status == null || status === "pending" || status === "in_progress";
  const [open, setOpen] = useState(false);
  const [debugOpen, setDebugOpen] = useState(false);
  const bodyId = useId();
  const disclosure = useRef<HTMLElement>(null);
  const disclosureMounted = useRef(false);
  const kind = toolKindLabel(call.kind, t);
  const title = call.title.trim() || call.name?.trim() || kind || t("tool.fallbackTitle");
  const content = call.content ?? [];
  const visibleContent = content.flatMap((item, index) =>
    item.type === "content" && item.content.type === "text" && item.content.text.trim().length === 0
      ? []
      : [{ item, index }]
  );
  const annotations = content.flatMap((item, index) =>
    item.type === "content" && item.content.annotations
      ? [{ index, ...item.content.annotations }]
      : []
  );
  const terminals = content.flatMap((item) => {
    if (item.type !== "terminal") return [];
    const snapshot = terminalSnapshots.find(({ terminalId }) => terminalId === item.terminalId);
    return [{
      terminalId: item.terminalId,
      ...(snapshot ? {
        exitStatus: snapshot.exitStatus,
        released: snapshot.released,
        truncated: snapshot.truncated,
      } : {}),
    }];
  });
  const toggleOpen = () => {
    setOpen((value) => !value);
    if (open) setDebugOpen(false);
  };
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
      className={`tool-card status-${status ?? "pending"}`}
      data-thread-entry
      data-thread-entry-id={item.id}
      data-tool-status={status ?? "pending"}
      data-live={live ? "true" : "false"}
      data-open={open ? "true" : "false"}
    >
      <header className="tool-card-header">
        <button
          type="button"
          className="tool-disclosure"
          aria-expanded={open}
          aria-controls={bodyId}
          onClick={toggleOpen}
        >
          <span className="tool-icon">
            <Icon size={15} />
          </span>
          <span className="tool-title" data-thread-searchable>
            <strong title={title}>{title}</strong>
            {kind && kind !== title ? <small>{kind}</small> : null}
          </span>
        </button>
        <span className="tool-actions">
          <ToolStatus status={status} />
          <button
            type="button"
            className="component-disclosure-button"
            aria-label={open ? t("tool.collapse") : t("tool.expand")}
            aria-expanded={open}
            aria-controls={bodyId}
            onClick={toggleOpen}
          ><ChevronRight className="chevron" size={15} aria-hidden="true" /></button>
        </span>
      </header>
      <div id={bodyId} className="tool-body" data-thread-searchable hidden={!open}>
        {call.rawInput !== undefined ? (
          <section className="tool-data-section tool-input">
            <header>{t("tool.input")}</header>
            <StructuredData value={call.rawInput} />
          </section>
        ) : null}
        <section className="tool-data-section tool-output">
          <header>{t("tool.output")}</header>
          {visibleContent.length > 0 ? (
            <>
              <div className="tool-output-content">
                {visibleContent.map(({ item, index }) => (
                  <ToolContentView key={index} content={item} terminalSnapshots={terminalSnapshots} />
                ))}
              </div>
              {call.rawOutput !== undefined ? (
                <details className="tool-additional-output">
                  <summary><ChevronRight size={12} />{t("tool.additionalOutput")}</summary>
                  <StructuredData value={call.rawOutput} />
                </details>
              ) : null}
            </>
          ) : call.rawOutput !== undefined ? (
            <StructuredData value={call.rawOutput} />
          ) : (
            <p className="tool-output-empty">{emptyOutputMessage(status, t)}</p>
          )}
        </section>
        <footer className={debugOpen ? "message-meta message-meta-open component-debug-meta" : "message-meta component-debug-meta"}>
          <div className="message-meta-actions">
            <DebugInfoButton
              expanded={debugOpen}
              label={t("tool.info")}
              onClick={() => setDebugOpen((value) => !value)}
            />
          </div>
          <DebugInfoPanel
            label={t("tool.debugInfo")}
            hidden={!debugOpen}
            entries={[
              { label: t("tool.callId"), value: call.toolCallId, format: "text" },
              ...(call.name ? [{ label: t("tool.name"), value: call.name, format: "text" as const }] : []),
              ...(call.locations?.length ? [{ label: t("tool.locations"), value: call.locations }] : []),
              ...(annotations.length ? [{ label: t("tool.annotations"), value: annotations }] : []),
              ...(terminals.length ? [{ label: t("tool.terminals"), value: terminals }] : []),
              { label: t("tool.events"), value: item.raw, count: item.raw.length },
            ]}
          />
        </footer>
      </div>
    </section>
  );
}

function toolKindLabel(kind: ToolKind | undefined, t: TFunction<"cards">): string | undefined {
  if (!kind || kind === "other") return undefined;
  return t(`tool.kind.${kind}`);
}

function emptyOutputMessage(status: ToolCall["status"] | "cancelled", t: TFunction<"cards">): string {
  if (status === "cancelled") return t("tool.empty.cancelled");
  if (status === "completed") return t("tool.empty.completed");
  if (status === "failed") return t("tool.empty.failed");
  return status === "in_progress" ? t("tool.empty.waitingOutput") : t("tool.empty.waitingTool");
}

function ToolStatus({ status }: { status: ToolCall["status"] | "cancelled" }) {
  const { t } = useTranslation("cards");
  const state = status ?? "pending";
  const label = t(`tool.status.${state}`);
  return (
    <span className={`tool-status tool-status-${state}`} aria-label={t("tool.statusLabel", { status: label })}>
      {state === "cancelled" ? (
        <CircleMinus className="status-icon" size={14} aria-hidden="true" />
      ) : state === "completed" ? (
        <Check className="status-icon complete" size={14} aria-hidden="true" />
      ) : state === "failed" ? (
        <CircleAlert className="status-icon failed" size={14} aria-hidden="true" />
      ) : state === "in_progress" ? (
        <LoaderCircle className="status-icon spin" size={14} aria-hidden="true" />
      ) : (
        <span className="status-dot" aria-hidden="true" />
      )}
      <small>{label}</small>
    </span>
  );
}

function ToolContentView({
  content,
  terminalSnapshots,
}: {
  content: ToolCallContent;
  terminalSnapshots: TerminalSnapshot[];
}) {
  const { t } = useTranslation("cards");
  if (content.type === "content") {
    return <ContentBlockView block={content.content} presentation="tool" />;
  }
  if (content.type === "terminal") {
    const snapshot = terminalSnapshots.find(
      ({ terminalId }) => terminalId === content.terminalId,
    );
    return (
      <div className="terminal-embed">
        <div className="terminal-heading">
          <TerminalSquare size={14} />
          <strong>{t("terminal.title")}</strong>
          <span>{terminalStatus(snapshot, t)}</span>
        </div>
        {snapshot ? (
          <>
            <pre>{snapshot.output || (snapshot.exitStatus != null || snapshot.released
              ? t("terminal.noOutput")
              : t("terminal.waitingOutput"))}</pre>
            {snapshot.truncated ? <small>{t("terminal.truncated")}</small> : null}
          </>
        ) : (
          <small>{t("terminal.outputUnavailable")}</small>
        )}
      </div>
    );
  }
  return <FileDiff content={content} />;
}

function terminalStatus(snapshot: TerminalSnapshot | undefined, t: TFunction<"cards">): string {
  if (!snapshot) return t("terminal.unavailable");
  if (snapshot.exitStatus?.exitCode != null) {
    return snapshot.exitStatus.exitCode === 0 ? t("terminal.completed") : t("terminal.failedExit", { code: snapshot.exitStatus.exitCode });
  }
  if (snapshot.exitStatus?.signal) return t("terminal.stoppedSignal", { signal: snapshot.exitStatus.signal });
  if (snapshot.exitStatus != null) return t("terminal.ended");
  if (snapshot.released) return t("terminal.stopped");
  return t("terminal.running");
}
