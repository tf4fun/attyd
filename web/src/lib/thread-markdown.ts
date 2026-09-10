import type {
  Annotations,
  ContentBlock,
  PlanEntry,
  ToolCallContent,
} from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import i18n from "../i18n";
import { assertNever } from "../../../shared/exhaustive";
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
    options.agentName ? i18n.t("markdown.metadata.agent", { ns: "conversation", name: options.agentName }) : undefined,
    options.sessionId ? i18n.t("markdown.metadata.session", { ns: "conversation", id: options.sessionId }) : undefined,
    options.cwd ? i18n.t("markdown.metadata.workspace", { ns: "conversation", path: options.cwd }) : undefined,
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
      const heading = i18n.t("markdown.you", { ns: "conversation" });
      const identity = item.messageId ? `\n\n_${i18n.t("markdown.messageId", { ns: "conversation", id: inlineText(item.messageId) })}_` : "";
      return `## ${heading}\n\n${contentBlocksToMarkdown(item.blocks)}${identity}`;
    }
    case "assistant":
      return item.chunks.map((chunk) => {
        const heading = chunk.role === "thought" ? i18n.t("markdown.thinking", { ns: "conversation" }) : i18n.t("markdown.agent", { ns: "conversation" });
        const identity = chunk.messageId
          ? `\n\n_${i18n.t("markdown.messageId", { ns: "conversation", id: inlineText(chunk.messageId) })}_`
          : "";
        return `## ${heading}\n\n${contentBlocksToMarkdown(chunk.blocks)}${identity}`;
      }).join("\n\n");
    case "tool": {
      const call = item.call;
      const details = [call.name ?? call.kind, call.status]
        .filter((value): value is string => value != null)
        .join(" · ");
      const sections = [`## ${i18n.t("markdown.toolHeading", { ns: "conversation", title: headingText(call.title) })}`];
      if (details) sections.push(`_${details}_`);
      if (call.locations?.length) {
        sections.push(call.locations.map(({ path, line }) =>
          `- \`${inlineCode(path)}${line == null ? "" : `:${line}`}\``
        ).join("\n"));
      }
      for (const content of call.content ?? []) {
        sections.push(toolContentToMarkdown(content, terminalSnapshots));
      }
      if (call.rawInput !== undefined) sections.push(`**${i18n.t("markdown.input", { ns: "conversation" })}**\n\n${jsonBlock(call.rawInput)}`);
      if (call.rawOutput !== undefined) sections.push(`**${i18n.t("markdown.output", { ns: "conversation" })}**\n\n${jsonBlock(call.rawOutput)}`);
      return sections.join("\n\n");
    }
    case "plan":
      return planToMarkdown(item.update);
    case "compaction": {
      const sections = [
        `## ${i18n.t("markdown.compaction", { ns: "conversation" })}`,
        `_${i18n.t("markdown.compactionDetails", { ns: "conversation", status: inlineText(item.status), id: inlineText(item.compactionId) })}_`,
      ];
      if (item.blocks.length > 0) sections.push(contentBlocksToMarkdown(item.blocks));
      if (item.error) sections.push(`> ${item.error.replaceAll("\n", "\n> ")}`);
      return sections.join("\n\n");
    }
    case "protocol":
      return `## ${i18n.t("markdown.eventHeading", { ns: "conversation", event: headingText(item.notification.update.sessionUpdate) })}\n\n${jsonBlock(item.notification)}`;
    case "stop": {
      const usage = item.response.usage;
      const usageLine = usage
        ? i18n.t("markdown.usage", { ns: "conversation", total: usage.totalTokens.toLocaleString(i18n.language), input: usage.inputTokens.toLocaleString(i18n.language), output: usage.outputTokens.toLocaleString(i18n.language) })
        : "";
      return `---\n\n_${i18n.t("markdown.turnComplete", { ns: "conversation", reason: inlineText(item.response.stopReason), usage: usageLine })}_`;
    }
    case "error":
      return `> **${i18n.t("markdown.error", { ns: "conversation" })}** ${item.message.replaceAll("\n", "\n> ")}`;
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
        ? `![${i18n.t("markdown.image", { ns: "conversation" })}](${block.uri})`
        : `[${i18n.t("markdown.embeddedImage", { ns: "conversation", mimeType: block.mimeType, size: decodedBytes(block.data).toLocaleString(i18n.language) })}]`;
      break;
    case "audio":
      content = `[${i18n.t("markdown.embeddedAudio", { ns: "conversation", mimeType: block.mimeType, size: decodedBytes(block.data).toLocaleString(i18n.language) })}]`;
      break;
    case "resource_link": {
      const label = escapeLinkLabel(block.title ?? block.name);
      const details = [block.description, block.mimeType, block.size == null ? undefined : i18n.t("markdown.bytes", { ns: "conversation", size: block.size.toLocaleString(i18n.language) })]
        .filter((value): value is string => value != null && value.length > 0)
        .join(" · ");
      content = `[${label}](${block.uri})${details ? `\n\n_${details}_` : ""}`;
      break;
    }
    case "resource": {
      const resource = block.resource;
      content = `**${i18n.t("markdown.resource", { ns: "conversation" })}** \`${inlineCode(resource.uri)}\``;
      if ("text" in resource) {
        content += `\n\n${fencedBlock(resource.text, resource.mimeType ?? "text")}`;
      } else {
        content += `\n\n[${i18n.t("markdown.binary", { ns: "conversation", mimeType: resource.mimeType ?? i18n.t("markdown.unknownMimeType", { ns: "conversation" }), size: decodedBytes(resource.blob).toLocaleString(i18n.language) })}]`;
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
      return `**${i18n.t("markdown.diff", { ns: "conversation" })}** \`${inlineCode(content.path)}\`\n\n${fencedBlock([...before, ...after].join("\n"), "diff")}`;
    }
    case "terminal": {
      const snapshot = terminalSnapshots.find(({ terminalId }) => terminalId === content.terminalId);
      if (!snapshot) return `**${i18n.t("markdown.terminal", { ns: "conversation" })}** \`${inlineCode(content.terminalId)}\` (${i18n.t("markdown.outputUnavailable", { ns: "conversation" })})`;
      const status = snapshot.exitStatus?.exitCode != null
        ? i18n.t("markdown.exit", { ns: "conversation", code: snapshot.exitStatus.exitCode.toLocaleString(i18n.language) })
        : snapshot.exitStatus?.signal ?? (snapshot.released ? i18n.t("markdown.released", { ns: "conversation" }) : i18n.t("markdown.running", { ns: "conversation" }));
      return `**${i18n.t("markdown.terminal", { ns: "conversation" })}** \`${inlineCode(content.terminalId)}\` · ${status}\n\n${fencedBlock(snapshot.output, "text")}`;
    }
    default:
      return assertNever(content, "ACP tool content markdown");
  }
}

function planToMarkdown(update: Extract<TimelineItem, { type: "plan" }>["update"]): string {
  if (update.sessionUpdate === "plan_removed") {
    return `## ${i18n.t("markdown.planRemoved", { ns: "conversation" })}\n\n_${i18n.t("markdown.id", { ns: "conversation", id: inlineText(update.planId) })}_`;
  }
  if (update.sessionUpdate === "plan") {
    return `## ${i18n.t("markdown.plan", { ns: "conversation" })}\n\n${planEntriesToMarkdown(update.entries)}`;
  }
  if (update.sessionUpdate === "plan_update") {
    if (update.plan.type === "items") {
      return `## ${i18n.t("markdown.plan", { ns: "conversation" })}\n\n${planEntriesToMarkdown(update.plan.entries)}`;
    }
    if (update.plan.type === "file") {
      return `## ${i18n.t("markdown.planFile", { ns: "conversation" })}\n\n${update.plan.uri}`;
    }
    return `## ${i18n.t("markdown.plan", { ns: "conversation" })}\n\n${update.plan.content}`;
  }
  return `## ${i18n.t("markdown.planEvent", { ns: "conversation" })}\n\n${jsonBlock(update)}`;
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
    annotations.audience?.length ? i18n.t("markdown.annotations.audience", { ns: "conversation", audience: [...new Set(annotations.audience)].join(", ") }) : undefined,
    annotations.priority == null ? undefined : i18n.t("markdown.annotations.priority", { ns: "conversation", priority: annotations.priority.toLocaleString(i18n.language) }),
    annotations.lastModified ? i18n.t("markdown.annotations.modified", { ns: "conversation", date: annotations.lastModified }) : undefined,
  ].filter((value): value is string => value != null);
  return details.length > 0 ? `_${i18n.t("markdown.annotations.details", { ns: "conversation", details: details.join(" · ") })}_` : "";
}

function jsonBlock(value: unknown): string {
  try {
    return fencedBlock(JSON.stringify(value, null, 2) ?? "null", "json");
  } catch {
    return fencedBlock(i18n.t("markdown.serializationFailed", { ns: "conversation" }), "text");
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
