import type { ToolCall, ToolCallContent, ToolKind } from "@agentclientprotocol/sdk";
import {
  Braces,
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
import { useEffect, useRef, useState } from "react";
import type { TimelineItem } from "../../lib/state";
import type { TerminalSnapshot } from "../../../shared/bridge";
import { ContentBlockView } from "./content-block";
import { DebugInfoButton, DebugInfoPanel, StructuredData } from "./debug-info";

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
  const Icon = kindIcons[call.kind ?? "other"];
  const live = call.status == null || call.status === "pending" || call.status === "in_progress";
  const [open, setOpen] = useState(false);
  const [debugOpen, setDebugOpen] = useState(false);
  const disclosure = useRef<HTMLElement>(null);
  const disclosureMounted = useRef(false);
  const presentation = toolPresentation(call);
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
          onClick={() => {
            setOpen((value) => {
              if (value) setDebugOpen(false);
              return !value;
            });
          }}
        >
          <span className="tool-icon">
            <Icon size={15} />
          </span>
          <span className="tool-title" data-thread-searchable>
            <strong>{presentation.name}</strong>
            {presentation.kind ? <small>{presentation.kind}</small> : null}
          </span>
        </button>
        <span className="tool-actions">
          <ToolStatus status={call.status} />
          <button
            type="button"
            className="component-disclosure-button"
            aria-label={open ? "Collapse tool details" : "Expand tool details"}
            aria-expanded={open}
            onClick={() => {
              setOpen((value) => {
                if (value) setDebugOpen(false);
                return !value;
              });
            }}
          ><ChevronRight className="chevron" size={15} aria-hidden="true" /></button>
        </span>
      </header>
      <div className="tool-body" data-thread-searchable hidden={!open}>
        {presentation.description ? (
          <section className="tool-data-section tool-description">
            <header>Description</header>
            <p>{presentation.description}</p>
          </section>
        ) : null}
        {call.rawInput !== undefined ? (
          <section className="tool-data-section tool-input">
            <header>Input</header>
            <StructuredData value={call.rawInput} />
          </section>
        ) : null}
        {call.locations && call.locations.length > 0 ? (
          <div className="locations">
            {call.locations.map((location, index) => (
              <code key={`${location.path}:${location.line ?? ""}:${index}`}>
                {location.path}
                {location.line != null ? `:${location.line}` : ""}
              </code>
            ))}
          </div>
        ) : null}
        {call.content?.length || call.rawOutput !== undefined ? (
          <section className="tool-data-section tool-output">
            <header>Output</header>
            {call.rawOutput !== undefined ? (
              <StructuredData value={call.rawOutput} />
            ) : call.content?.map((content, index) => (
              <ToolContentView
                key={index}
                content={content}
                terminalSnapshots={terminalSnapshots}
              />
            ))}
          </section>
        ) : live ? (
          <p className="tool-output-pending">Waiting for output…</p>
        ) : null}
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
              { label: "Message events", value: item.raw, count: item.raw.length },
            ]}
          />
        </footer>
      </div>
    </section>
  );
}

function toolPresentation(call: ToolCall): { name: string; kind?: string; description?: string } {
  const title = call.title.trim();
  const titleParts = title.match(/^([^·:]{1,32})\s*[·:]\s*(.+)$/s);
  const name = call.name?.trim() || titleParts?.[1]?.trim() || toolKindLabel(call.kind) || title || "Tool";
  const kind = toolKindLabel(call.kind);
  const description = titleParts?.[2]?.trim() || (title && title !== name ? title : undefined);
  return { name, kind: kind && kind !== name ? kind : undefined, description };
}

function toolKindLabel(kind: ToolKind | undefined): string | undefined {
  if (!kind) return undefined;
  return kind.split("_").map((part) => part[0]?.toUpperCase() + part.slice(1)).join(" ");
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
  if (content.type === "content") return <ContentBlockView block={content.content} />;
  if (content.type === "terminal") {
    const snapshot = terminalSnapshots.find(
      ({ terminalId }) => terminalId === content.terminalId,
    );
    return (
      <div className="terminal-embed">
        <div className="terminal-heading">
          <TerminalSquare size={14} />
          <code>{content.terminalId}</code>
          <span>{terminalStatus(snapshot)}</span>
        </div>
        {snapshot ? (
          <>
            <pre>{snapshot.output || "Waiting for terminal output…"}</pre>
            {snapshot.truncated ? <small>Earlier output was truncated.</small> : null}
          </>
        ) : (
          <small>Terminal output snapshot is not available.</small>
        )}
      </div>
    );
  }
  return (
    <details className="diff-card">
      <summary>
        <Braces size={14} /> {content.path}
      </summary>
      <div className="diff-grid">
        {content.oldText != null ? (
          <pre className="diff-old">{prefixLines(content.oldText, "−")}</pre>
        ) : null}
        <pre className="diff-new">{prefixLines(content.newText, "+")}</pre>
      </div>
    </details>
  );
}

function terminalStatus(snapshot: TerminalSnapshot | undefined): string {
  if (!snapshot) return "unavailable";
  if (snapshot.exitStatus?.exitCode != null) {
    return `exit ${snapshot.exitStatus.exitCode}`;
  }
  if (snapshot.exitStatus?.signal) return snapshot.exitStatus.signal;
  if (snapshot.released) return "released";
  return "running";
}

function prefixLines(text: string, marker: string): string {
  return text
    .split("\n")
    .map((line) => `${marker} ${line}`)
    .join("\n");
}
