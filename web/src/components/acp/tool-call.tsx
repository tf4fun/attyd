import type { ToolCall, ToolCallContent, ToolKind } from "@agentclientprotocol/sdk";
import {
  Check,
  ChevronRight,
  CircleAlert,
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
  const { call } = item;
  const Icon = kindIcons[call.kind ?? "other"] ?? Wrench;
  const live = call.status == null || call.status === "pending" || call.status === "in_progress";
  const [open, setOpen] = useState(false);
  const [debugOpen, setDebugOpen] = useState(false);
  const bodyId = useId();
  const disclosure = useRef<HTMLElement>(null);
  const disclosureMounted = useRef(false);
  const kind = toolKindLabel(call.kind);
  const title = call.title.trim() || call.name?.trim() || kind || "Tool";
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
      className={`tool-card status-${call.status ?? "pending"}`}
      data-thread-entry
      data-thread-entry-id={item.id}
      data-tool-status={call.status ?? "pending"}
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
          <ToolStatus status={call.status} />
          <button
            type="button"
            className="component-disclosure-button"
            aria-label={open ? "Collapse tool details" : "Expand tool details"}
            aria-expanded={open}
            aria-controls={bodyId}
            onClick={toggleOpen}
          ><ChevronRight className="chevron" size={15} aria-hidden="true" /></button>
        </span>
      </header>
      <div id={bodyId} className="tool-body" data-thread-searchable hidden={!open}>
        {call.rawInput !== undefined ? (
          <section className="tool-data-section tool-input">
            <header>Input</header>
            <StructuredData value={call.rawInput} />
          </section>
        ) : null}
        <section className="tool-data-section tool-output">
          <header>Output</header>
          {visibleContent.length > 0 ? (
            <>
              <div className="tool-output-content">
                {visibleContent.map(({ item, index }) => (
                  <ToolContentView key={index} content={item} terminalSnapshots={terminalSnapshots} />
                ))}
              </div>
              {call.rawOutput !== undefined ? (
                <details className="tool-additional-output">
                  <summary><ChevronRight size={12} />Additional output</summary>
                  <StructuredData value={call.rawOutput} />
                </details>
              ) : null}
            </>
          ) : call.rawOutput !== undefined ? (
            <StructuredData value={call.rawOutput} />
          ) : (
            <p className="tool-output-empty">{emptyOutputMessage(call.status)}</p>
          )}
        </section>
        <footer className={debugOpen ? "message-meta message-meta-open component-debug-meta" : "message-meta component-debug-meta"}>
          <div className="message-meta-actions">
            <DebugInfoButton
              expanded={debugOpen}
              label="Tool info"
              onClick={() => setDebugOpen((value) => !value)}
            />
          </div>
          <DebugInfoPanel
            label="Tool debug information"
            hidden={!debugOpen}
            entries={[
              { label: "Tool call ID", value: call.toolCallId, format: "text" },
              ...(call.name ? [{ label: "Tool name", value: call.name, format: "text" as const }] : []),
              ...(call.locations?.length ? [{ label: "Locations", value: call.locations }] : []),
              ...(annotations.length ? [{ label: "Content annotations", value: annotations }] : []),
              ...(terminals.length ? [{ label: "Terminals", value: terminals }] : []),
              { label: "Message events", value: item.raw, count: item.raw.length },
            ]}
          />
        </footer>
      </div>
    </section>
  );
}

function toolKindLabel(kind: ToolKind | undefined): string | undefined {
  if (!kind || kind === "other") return undefined;
  return kind.split("_").map((part) => part[0]?.toUpperCase() + part.slice(1)).join(" ");
}

function emptyOutputMessage(status: ToolCall["status"]): string {
  if (status === "completed") return "Completed without output.";
  if (status === "failed") return "No error details were provided.";
  return status === "in_progress" ? "Waiting for output…" : "Waiting for the tool…";
}

function ToolStatus({ status }: { status: ToolCall["status"] }) {
  const state = status ?? "pending";
  const label = state === "in_progress"
    ? "Running"
    : state === "completed"
      ? "Completed"
      : state === "failed"
        ? "Failed"
        : "Pending";
  return (
    <span className={`tool-status tool-status-${state}`} aria-label={`Tool status: ${label}`}>
      {state === "completed" ? (
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
          <strong>Terminal</strong>
          <span>{terminalStatus(snapshot)}</span>
        </div>
        {snapshot ? (
          <>
            <pre>{snapshot.output || (snapshot.exitStatus != null || snapshot.released
              ? "No output."
              : "Waiting for terminal output…")}</pre>
            {snapshot.truncated ? <small>Earlier output was truncated.</small> : null}
          </>
        ) : (
          <small>Terminal output is unavailable.</small>
        )}
      </div>
    );
  }
  return <FileDiff content={content} />;
}

function terminalStatus(snapshot: TerminalSnapshot | undefined): string {
  if (!snapshot) return "Unavailable";
  if (snapshot.exitStatus?.exitCode != null) {
    return snapshot.exitStatus.exitCode === 0 ? "Completed" : `Failed (exit ${snapshot.exitStatus.exitCode})`;
  }
  if (snapshot.exitStatus?.signal) return `Stopped (${snapshot.exitStatus.signal})`;
  if (snapshot.exitStatus != null) return "Ended";
  if (snapshot.released) return "Stopped";
  return "Running";
}
