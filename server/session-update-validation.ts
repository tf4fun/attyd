import type {
  CompactionStatus,
  ContentBlock,
  SessionUpdate,
  ToolCallUpdate,
} from "@agentclientprotocol/sdk";
import { validateContentBlockSemantics } from "../shared/content-validation.js";
import { assertNever } from "../shared/exhaustive.js";
import { isAbsoluteWorkspacePath } from "../shared/bridge.js";
import { validateSessionControls } from "./session-validation.js";

interface TrackedCompaction {
  status: CompactionStatus;
  summaryBytes: number;
}

interface TrackedToolCall {
  status?: Extract<SessionUpdate, { sessionUpdate: "tool_call" }>["status"];
}

type MessageUpdateKind = Extract<
  SessionUpdate,
  { sessionUpdate: "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" }
>["sessionUpdate"];

export interface SessionUpdateValidationState {
  compactions: Map<string, TrackedCompaction>;
  toolCalls: Map<string, TrackedToolCall>;
  messages: Map<string, MessageUpdateKind>;
  plans: Map<string, "active" | "removed">;
  updateCount: number;
  updateBytes: number;
  currentModeId?: string;
  invalidReason?: string;
}

export interface SessionUpdateValidationContext {
  assertTerminalReference?: (terminalId: string) => void;
}

const MAX_UPDATE_BYTES = 4_000_000;
const MAX_IDENTIFIER_LENGTH = 1_024;
const MAX_AVAILABLE_COMMANDS = 1_000;
const MAX_PLAN_ENTRIES = 1_000;
const MAX_TOOL_COLLECTION_ITEMS = 10_000;
const MAX_COMPACTIONS = 1_000;
const MAX_TOOL_CALLS = 10_000;
const MAX_MESSAGES = 10_000;
const MAX_PLANS = 1_000;
const MAX_COMPACTION_SUMMARY_BYTES = 3_000_000;
const MAX_SESSION_UPDATES = 100_000;
const MAX_SESSION_UPDATE_BYTES = 128_000_000;
const MAX_SESSION_TITLE_LENGTH = 16_384;
const MAX_TOOL_PATH_LENGTH = 16_384;
const MAX_TOOL_LABEL_LENGTH = 16_384;
const MAX_TOOL_LOCATION_LINE = 4_294_967_295;

export function createSessionUpdateValidationState(): SessionUpdateValidationState {
  return {
    compactions: new Map(),
    toolCalls: new Map(),
    messages: new Map(),
    plans: new Map(),
    updateCount: 0,
    updateBytes: 0,
  };
}

export function validateAndTrackSessionUpdate(
  state: SessionUpdateValidationState | undefined,
  update: SessionUpdate,
  context: SessionUpdateValidationContext = {},
): void {
  const updateBytes = serializedBytes(update);
  if (updateBytes > MAX_UPDATE_BYTES) {
    throw new Error(`Agent session update exceeds ${MAX_UPDATE_BYTES} bytes`);
  }
  if (state) {
    if (state.updateCount >= MAX_SESSION_UPDATES) {
      throw new Error(`Agent exceeded ${MAX_SESSION_UPDATES} updates in one session`);
    }
    if (state.updateBytes + updateBytes > MAX_SESSION_UPDATE_BYTES) {
      throw new Error(
        `Agent session updates exceed ${MAX_SESSION_UPDATE_BYTES} cumulative bytes`,
      );
    }
  }

  validateAndTrackSessionUpdatePayload(state, update, context);

  // An invalid notification is dropped by the bridge. Charge cumulative
  // budgets only after every semantic/lifecycle check has succeeded so a
  // stream of rejected input cannot consume the session's allowance.
  if (state) {
    state.updateCount += 1;
    state.updateBytes += updateBytes;
  }
}

function validateAndTrackSessionUpdatePayload(
  state: SessionUpdateValidationState | undefined,
  update: SessionUpdate,
  context: SessionUpdateValidationContext,
): void {
  switch (update.sessionUpdate) {
    case "user_message_chunk":
    case "agent_message_chunk":
    case "agent_thought_chunk":
      validateContentBlockSemantics(update.content, "Agent message content");
      if (update.messageId != null) {
        assertIdentifier(update.messageId, "message ID");
        trackMessage(state, update.messageId, update.sessionUpdate);
      }
      return;
    case "tool_call":
      assertIdentifier(update.toolCallId, "tool call ID");
      validateToolCallLabels(update.title, update.name);
      validateToolCollections(update.content, update.locations, context);
      trackToolCall(state, update);
      return;
    case "tool_call_update":
      validateAndTrackToolCallPatch(state, update, context);
      return;
    case "plan":
      validatePlanEntries(update.entries);
      return;
    case "plan_update":
      assertIdentifier(update.plan.planId, "plan ID");
      if (update.plan.type === "items") validatePlanEntries(update.plan.entries);
      trackPlanUpdate(state, update.plan.planId);
      return;
    case "plan_removed":
      assertIdentifier(update.planId, "plan ID");
      trackPlanRemoval(state, update.planId);
      return;
    case "available_commands_update":
      validateAvailableCommands(update.availableCommands);
      return;
    case "current_mode_update":
      assertIdentifier(update.currentModeId, "mode ID");
      if (state) state.currentModeId = update.currentModeId;
      return;
    case "config_option_update":
      validateSessionControls(undefined, update.configOptions);
      return;
    case "session_info_update":
      validateSessionInfoUpdate(update);
      return;
    case "usage_update":
      validateUsage(update);
      return;
    case "compaction_update":
      validateCompactionUpdate(state, update);
      return;
    case "compaction_summary_chunk":
      validateCompactionChunk(state, update.compactionId, update.content);
      return;
  }

  assertNever(update, "ACP session update validation");
}

function trackMessage(
  state: SessionUpdateValidationState | undefined,
  messageId: string,
  kind: MessageUpdateKind,
): void {
  if (!state) return;
  const previous = state.messages.get(messageId);
  if (previous == null && state.messages.size >= MAX_MESSAGES) {
    throw new Error(`Agent exceeded ${MAX_MESSAGES} message IDs in one session`);
  }
  state.messages.set(messageId, kind);
}

function trackPlanUpdate(
  state: SessionUpdateValidationState | undefined,
  planId: string,
): void {
  if (!state) return;
  if (!state.plans.has(planId) && state.plans.size >= MAX_PLANS) {
    throw new Error(`Agent exceeded ${MAX_PLANS} plans in one session`);
  }
  state.plans.set(planId, "active");
}

function trackPlanRemoval(
  state: SessionUpdateValidationState | undefined,
  planId: string,
): void {
  if (!state) return;
  if (!state.plans.has(planId) && state.plans.size >= MAX_PLANS) {
    throw new Error(`Agent exceeded ${MAX_PLANS} plans in one session`);
  }
  state.plans.set(planId, "removed");
}

export function validateAndTrackToolCallPatch(
  state: SessionUpdateValidationState | undefined,
  update: ToolCallUpdate,
  context: SessionUpdateValidationContext = {},
): void {
  assertIdentifier(update.toolCallId, "tool call ID");
  validateToolCallLabels(update.title, update.name);
  validateToolCollections(update.content, update.locations, context);
  trackToolCallUpdate(state, update);
}

function validateAvailableCommands(
  commands: Extract<SessionUpdate, { sessionUpdate: "available_commands_update" }>["availableCommands"],
): void {
  if (commands.length > MAX_AVAILABLE_COMMANDS) {
    throw new Error(`Agent returned more than ${MAX_AVAILABLE_COMMANDS} available commands`);
  }
  const names = new Set<string>();
  for (const command of commands) {
    assertIdentifier(command.name, "available command name");
    if (names.has(command.name)) {
      throw new Error(`Agent returned duplicate available command: ${command.name}`);
    }
    names.add(command.name);
    if (command.description.length > 16_384) {
      throw new Error(`Available command description is too long: ${command.name}`);
    }
    if (command.input?.hint != null && command.input.hint.length > 4_096) {
      throw new Error(`Available command input hint is too long: ${command.name}`);
    }
  }
}

function validatePlanEntries(
  entries: Extract<SessionUpdate, { sessionUpdate: "plan" }>["entries"],
): void {
  if (entries.length > MAX_PLAN_ENTRIES) {
    throw new Error(`Agent plan exceeds ${MAX_PLAN_ENTRIES} entries`);
  }
}

function validateToolCollections(
  content: Extract<SessionUpdate, { sessionUpdate: "tool_call" }>["content"] | null | undefined,
  locations: Extract<SessionUpdate, { sessionUpdate: "tool_call" }>["locations"] | null | undefined,
  context: SessionUpdateValidationContext,
): void {
  if ((content?.length ?? 0) > MAX_TOOL_COLLECTION_ITEMS) {
    throw new Error(`Agent tool content exceeds ${MAX_TOOL_COLLECTION_ITEMS} items`);
  }
  for (const item of content ?? []) {
    switch (item.type) {
      case "content":
        validateContentBlockSemantics(item.content, "Agent tool content");
        break;
      case "diff":
        validateAbsoluteToolPath(item.path, "Agent tool diff path");
        break;
      case "terminal":
        assertIdentifier(item.terminalId, "terminal reference ID");
        context.assertTerminalReference?.(item.terminalId);
        break;
    }
  }
  if ((locations?.length ?? 0) > MAX_TOOL_COLLECTION_ITEMS) {
    throw new Error(`Agent tool locations exceed ${MAX_TOOL_COLLECTION_ITEMS} items`);
  }
  for (const location of locations ?? []) {
    validateAbsoluteToolPath(location.path, "Agent tool location path");
    if (
      location.line != null &&
      (!Number.isSafeInteger(location.line) ||
        location.line < 0 ||
        location.line > MAX_TOOL_LOCATION_LINE)
    ) {
      throw new Error(
        `Agent tool location line must be an integer between 0 and ${MAX_TOOL_LOCATION_LINE}`,
      );
    }
  }
}

function validateToolCallLabels(
  title: string | null | undefined,
  name: string | null | undefined,
): void {
  if (title != null && title.length > MAX_TOOL_LABEL_LENGTH) {
    throw new Error(`Agent tool title exceeds ${MAX_TOOL_LABEL_LENGTH} characters`);
  }
  if (name != null && name.length > MAX_TOOL_LABEL_LENGTH) {
    throw new Error(`Agent tool name exceeds ${MAX_TOOL_LABEL_LENGTH} characters`);
  }
}

function validateAbsoluteToolPath(path: string, subject: string): void {
  if (
    path.length === 0 ||
    path.length > MAX_TOOL_PATH_LENGTH ||
    path.includes("\0") ||
    !isAbsoluteWorkspacePath(path)
  ) {
    throw new Error(`${subject} must be an absolute path of at most ${MAX_TOOL_PATH_LENGTH} characters`);
  }
}

function trackToolCall(
  state: SessionUpdateValidationState | undefined,
  update: Extract<SessionUpdate, { sessionUpdate: "tool_call" }>,
): void {
  if (!state) return;
  if (!state.toolCalls.has(update.toolCallId) && state.toolCalls.size >= MAX_TOOL_CALLS) {
    throw new Error(`Agent exceeded ${MAX_TOOL_CALLS} tool calls in one session`);
  }
  const previous = state.toolCalls.get(update.toolCallId);
  state.toolCalls.set(update.toolCallId, { status: update.status ?? previous?.status });
}

function trackToolCallUpdate(
  state: SessionUpdateValidationState | undefined,
  update: ToolCallUpdate,
): void {
  if (!state) return;
  const toolCall = state.toolCalls.get(update.toolCallId);
  if (!toolCall) {
    if (state.toolCalls.size >= MAX_TOOL_CALLS) {
      throw new Error(`Agent exceeded ${MAX_TOOL_CALLS} tool calls in one session`);
    }
    state.toolCalls.set(update.toolCallId, { status: update.status ?? undefined });
    return;
  }
  if (update.status != null) toolCall.status = update.status;
}

function validateUsage(
  update: Extract<SessionUpdate, { sessionUpdate: "usage_update" }>,
): void {
  if (
    !Number.isFinite(update.used) ||
    !Number.isFinite(update.size) ||
    update.used < 0 ||
    update.size < 0
  ) {
    throw new Error("Agent returned invalid context usage values");
  }
  if (update.cost != null) {
    if (!Number.isFinite(update.cost.amount) || update.cost.amount < 0) {
      throw new Error("Agent returned an invalid cumulative cost");
    }
    if (update.cost.currency.length === 0 || update.cost.currency.length > 32) {
      throw new Error("Agent returned an invalid cost currency label");
    }
  }
}

function validateSessionInfoUpdate(
  update: Extract<SessionUpdate, { sessionUpdate: "session_info_update" }>,
): void {
  validateSessionMetadata(update.title, update.updatedAt, "Agent session");
}

export function validateSessionMetadata(
  title: string | null | undefined,
  updatedAt: string | null | undefined,
  subject: string,
): void {
  if (title != null && title.length > MAX_SESSION_TITLE_LENGTH) {
    throw new Error(`${subject} title exceeds ${MAX_SESSION_TITLE_LENGTH} characters`);
  }
  if (updatedAt != null && updatedAt.length > 256) {
    throw new Error(`${subject} updatedAt exceeds 256 characters`);
  }
}

function validateCompactionUpdate(
  state: SessionUpdateValidationState | undefined,
  update: Extract<SessionUpdate, { sessionUpdate: "compaction_update" }>,
): void {
  assertIdentifier(update.compactionId, "compaction ID");
  if (update.summary != null && update.summary.length > 0 && update.status !== "completed") {
    throw new Error("A non-empty compaction summary is only valid with completed status");
  }
  if (update.error != null && update.status !== "failed") {
    throw new Error("A compaction error is only valid with failed status");
  }
  if (update.summary != null) {
    assertSerializedLimit(
      update.summary,
      MAX_COMPACTION_SUMMARY_BYTES,
      "compaction summary",
    );
    for (const block of update.summary) {
      validateContentBlockSemantics(block, "Agent compaction summary content");
    }
  }
  if (!state) return;

  const previous = state.compactions.get(update.compactionId);
  if (previous && isTerminalCompactionStatus(previous.status)) {
    throw new Error(`Compaction is already terminal: ${update.compactionId}`);
  }
  if (!previous && state.compactions.size >= MAX_COMPACTIONS) {
    throw new Error(`Agent exceeded ${MAX_COMPACTIONS} compactions in one session`);
  }
  const summaryBytes = update.summary === undefined
    ? previous?.summaryBytes ?? 0
    : update.summary === null
      ? 0
      : serializedBytes(update.summary);
  state.compactions.set(update.compactionId, {
    status: update.status,
    summaryBytes,
  });
}

function validateCompactionChunk(
  state: SessionUpdateValidationState | undefined,
  compactionId: string,
  content: ContentBlock,
): void {
  assertIdentifier(compactionId, "compaction ID");
  validateContentBlockSemantics(content, "Agent compaction summary content");
  if (!state) return;
  const compaction = state.compactions.get(compactionId);
  if (!compaction || compaction.status !== "in_progress") {
    throw new Error(
      `Compaction summary chunks require an in-progress compaction: ${compactionId}`,
    );
  }
  const summaryBytes = compaction.summaryBytes + serializedBytes(content);
  if (summaryBytes > MAX_COMPACTION_SUMMARY_BYTES) {
    throw new Error(`Compaction summary exceeds ${MAX_COMPACTION_SUMMARY_BYTES} bytes`);
  }
  compaction.summaryBytes = summaryBytes;
}

function isTerminalCompactionStatus(status: CompactionStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled";
}

function assertIdentifier(value: string, label: string): void {
  if (value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH) {
    throw new Error(`Agent ${label} must contain between 1 and ${MAX_IDENTIFIER_LENGTH} characters`);
  }
}

function assertSerializedLimit(value: unknown, maximum: number, label: string): void {
  if (serializedBytes(value) > maximum) {
    throw new Error(`Agent ${label} exceeds ${maximum} bytes`);
  }
}

function serializedBytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value), "utf8");
}
