import type {
  Annotations,
  ContentBlock,
  PlanEntry,
  ToolCallContent,
} from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../shared/bridge";
import { assertNever } from "../../shared/exhaustive";
import type { TimelineItem } from "./state";

export interface ThreadMarkdownOptions {
  title: string;
  agentName?: string;
  sessionId?: string;
  cwd?: string;
  terminalSnapshots?: TerminalSnapshot[];
}

export function timelineToMarkdown(
  timeline: TimelineItem[],
  options: ThreadMarkdownOptions,
): string {
  const header = [`# ${headingText(options.title)}`];
  const metadata = [
    options.agentName ? `Agent: ${options.agentName}` : undefined,
    options.sessionId ? `Session: ${options.sessionId}` : undefined,
    options.cwd ? `Workspace: ${options.cwd}` : undefined,
  ].filter((line): line is string => line != null);
  if (metadata.length > 0) header.push("", ...metadata.map((line) => `- ${line}`));

  const entries = timeline.map((item) => timelineItemToMarkdown(
    item,
    options.terminalSnapshots ?? [],
  ));
  return [...header, ...entries].filter((section) => section.length > 0).join("\n\n").trimEnd() + "\n";
}

export function contentBlocksToMarkdown(blocks: ContentBlock[]): string {
  return blocks.map(contentBlockToMarkdown).filter(Boolean).join("\n\n");
}

function timelineItemToMarkdown(
  item: TimelineItem,
  terminalSnapshots: TerminalSnapshot[],
): string {
  switch (item.type) {
    case "message": {
      const heading = "You";
      const identity = item.messageId ? `\n\n_Message ID: ${inlineText(item.messageId)}_` : "";
      return `## ${heading}\n\n${contentBlocksToMarkdown(item.blocks)}${identity}`;
    }
    case "assistant":
      return item.chunks.map((chunk) => {
        const heading = chunk.role === "thought" ? "Thinking" : "Agent";
        const identity = chunk.messageId
          ? `\n\n_Message ID: ${inlineText(chunk.messageId)}_`
          : "";
        return `## ${heading}\n\n${contentBlocksToMarkdown(chunk.blocks)}${identity}`;
      }).join("\n\n");
    case "tool": {
      const call = item.call;
      const details = [call.name ?? call.kind, call.status]
        .filter((value): value is string => value != null)
        .join(" · ");
      const sections = [`## Tool · ${headingText(call.title)}`];
      if (details) sections.push(`_${details}_`);
      if (call.locations?.length) {
        sections.push(call.locations.map(({ path, line }) =>
          `- \`${inlineCode(path)}${line == null ? "" : `:${line}`}\``
        ).join("\n"));
      }
      for (const content of call.content ?? []) {
        sections.push(toolContentToMarkdown(content, terminalSnapshots));
      }
      if (call.rawInput !== undefined) sections.push(`**Input**\n\n${jsonBlock(call.rawInput)}`);
      if (call.rawOutput !== undefined) sections.push(`**Output**\n\n${jsonBlock(call.rawOutput)}`);
      return sections.join("\n\n");
    }
    case "plan":
      return planToMarkdown(item.update);
    case "compaction": {
      const sections = [
        `## Context compaction`,
        `_Status: ${inlineText(item.status)} · ID: ${inlineText(item.compactionId)}_`,
      ];
      if (item.blocks.length > 0) sections.push(contentBlocksToMarkdown(item.blocks));
      if (item.error) sections.push(`> ${item.error.replaceAll("\n", "\n> ")}`);
      return sections.join("\n\n");
    }
    case "protocol":
      return `## ACP event · ${headingText(item.notification.update.sessionUpdate)}\n\n${jsonBlock(item.notification)}`;
    case "stop": {
      const usage = item.response.usage;
      const usageLine = usage
        ? ` · ${usage.totalTokens.toLocaleString()} tokens (${usage.inputTokens.toLocaleString()} input, ${usage.outputTokens.toLocaleString()} output)`
        : "";
      return `---\n\n_Turn complete · ${inlineText(item.response.stopReason)}${usageLine}_`;
    }
    case "error":
      return `> **Error:** ${item.message.replaceAll("\n", "\n> ")}`;
  }
  return assertNever(item, "thread timeline item");
}

function contentBlockToMarkdown(block: ContentBlock): string {
  let content: string;
  switch (block.type) {
    case "text":
      content = block.text;
      break;
    case "image":
      content = block.uri
        ? `![ACP image](${block.uri})`
        : `[Embedded image · ${block.mimeType} · ${decodedBytes(block.data)} bytes]`;
      break;
    case "audio":
      content = `[Embedded audio · ${block.mimeType} · ${decodedBytes(block.data)} bytes]`;
      break;
    case "resource_link": {
      const label = escapeLinkLabel(block.title ?? block.name);
      const details = [block.description, block.mimeType, block.size == null ? undefined : `${block.size} bytes`]
        .filter((value): value is string => value != null && value.length > 0)
        .join(" · ");
      content = `[${label}](${block.uri})${details ? `\n\n_${details}_` : ""}`;
      break;
    }
    case "resource": {
      const resource = block.resource;
      content = `**Embedded resource:** \`${inlineCode(resource.uri)}\``;
      if ("text" in resource) {
        content += `\n\n${fencedBlock(resource.text, resource.mimeType ?? "text")}`;
      } else {
        content += `\n\n[Binary · ${resource.mimeType ?? "unknown MIME type"} · ${decodedBytes(resource.blob)} bytes]`;
      }
      break;
    }
    default:
      return assertNever(block, "ACP content block markdown");
  }
  const annotations = annotationsToMarkdown(block.annotations);
  return annotations ? `${content}\n\n${annotations}` : content;
}

function toolContentToMarkdown(
  content: ToolCallContent,
  terminalSnapshots: TerminalSnapshot[],
): string {
  switch (content.type) {
    case "content":
      return contentBlockToMarkdown(content.content);
    case "diff": {
      const before = content.oldText == null
        ? []
        : content.oldText.split("\n").map((line) => `-${line}`);
      const after = content.newText.split("\n").map((line) => `+${line}`);
      return `**Diff:** \`${inlineCode(content.path)}\`\n\n${fencedBlock([...before, ...after].join("\n"), "diff")}`;
    }
    case "terminal": {
      const snapshot = terminalSnapshots.find(({ terminalId }) => terminalId === content.terminalId);
      if (!snapshot) return `**Terminal:** \`${inlineCode(content.terminalId)}\` (output unavailable)`;
      const status = snapshot.exitStatus?.exitCode != null
        ? `exit ${snapshot.exitStatus.exitCode}`
        : snapshot.exitStatus?.signal ?? (snapshot.released ? "released" : "running");
      return `**Terminal:** \`${inlineCode(content.terminalId)}\` · ${status}\n\n${fencedBlock(snapshot.output, "text")}`;
    }
    default:
      return assertNever(content, "ACP tool content markdown");
  }
}

function planToMarkdown(update: Extract<TimelineItem, { type: "plan" }>["update"]): string {
  if (update.sessionUpdate === "plan_removed") {
    return `## Plan removed\n\n_ID: ${inlineText(update.planId)}_`;
  }
  if (update.sessionUpdate === "plan") {
    return `## Plan\n\n${planEntriesToMarkdown(update.entries)}`;
  }
  if (update.sessionUpdate === "plan_update") {
    if (update.plan.type === "items") {
      return `## Plan\n\n${planEntriesToMarkdown(update.plan.entries)}`;
    }
    if (update.plan.type === "file") {
      return `## Plan file\n\n${update.plan.uri}`;
    }
    return `## Plan\n\n${update.plan.content}`;
  }
  return `## ACP plan event\n\n${jsonBlock(update)}`;
}

function planEntriesToMarkdown(entries: PlanEntry[]): string {
  return entries.map((entry) => {
    const check = entry.status === "completed" ? "x" : " ";
    return `- [${check}] ${entry.content} _(${entry.status}, ${entry.priority})_`;
  }).join("\n");
}

function annotationsToMarkdown(annotations: Annotations | null | undefined): string {
  if (annotations == null) return "";
  const details = [
    annotations.audience?.length ? `audience: ${[...new Set(annotations.audience)].join(", ")}` : undefined,
    annotations.priority == null ? undefined : `priority: ${annotations.priority}`,
    annotations.lastModified ? `modified: ${annotations.lastModified}` : undefined,
  ].filter((value): value is string => value != null);
  return details.length > 0 ? `_ACP annotations · ${details.join(" · ")}_` : "";
}

function jsonBlock(value: unknown): string {
  try {
    return fencedBlock(JSON.stringify(value, null, 2) ?? "null", "json");
  } catch {
    return fencedBlock("Unable to serialize ACP value", "text");
  }
}

function fencedBlock(value: string, language: string): string {
  const longestFence = Math.max(2, ...[...value.matchAll(/`+/g)].map(([ticks]) => ticks.length));
  const fence = "`".repeat(longestFence + 1);
  return `${fence}${language}\n${value}\n${fence}`;
}

function decodedBytes(base64: string): number {
  const padding = base64.endsWith("==") ? 2 : base64.endsWith("=") ? 1 : 0;
  return Math.max(0, (base64.length / 4) * 3 - padding);
}

function headingText(value: string): string {
  return value.replaceAll("\n", " ").replaceAll("#", "\\#");
}

function inlineText(value: string): string {
  return value.replaceAll("\n", " ");
}

function inlineCode(value: string): string {
  return value.replaceAll("`", "\\`").replaceAll("\n", " ");
}

function escapeLinkLabel(value: string): string {
  return value.replaceAll("\\", "\\\\").replaceAll("[", "\\[").replaceAll("]", "\\]");
}
