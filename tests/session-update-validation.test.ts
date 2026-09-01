import { describe, expect, it } from "vitest";
import {
  createSessionUpdateValidationState,
  validateAndTrackSessionUpdate,
} from "../server/session-update-validation";

describe("Agent session/update validation", () => {
  it("tracks the documented compaction streaming lifecycle", () => {
    const state = createSessionUpdateValidationState();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "compact-1",
      status: "in_progress",
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_summary_chunk",
      compactionId: "compact-1",
      content: { type: "text", text: "summary" },
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "compact-1",
      status: "completed",
    });

    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_summary_chunk",
      compactionId: "compact-1",
      content: { type: "text", text: "too late" },
    })).toThrow("in-progress");
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "compact-1",
      status: "failed",
      error: "too late",
    })).toThrow("already terminal");
  });

  it("rejects compaction chunks without a started entity and invalid terminal fields", () => {
    const state = createSessionUpdateValidationState();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_summary_chunk",
      compactionId: "missing",
      content: { type: "text", text: "orphan" },
    })).toThrow("in-progress");
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "bad-summary",
      status: "failed",
      summary: [{ type: "text", text: "not completed" }],
    })).toThrow("only valid with completed");
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "bad-error",
      status: "completed",
      error: "not failed",
    })).toThrow("only valid with failed");
  });

  it("rejects ambiguous command menus while preserving unusual Agent usage values", () => {
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "available_commands_update",
      availableCommands: [
        { name: "inspect", description: "one" },
        { name: "inspect", description: "two" },
      ],
    })).toThrow("duplicate available command");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "usage_update",
      used: 11,
      size: 10,
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "usage_update",
      used: 1,
      size: 10,
      cost: { amount: 0.01, currency: "usd" },
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "usage_update",
      used: -1,
      size: 10,
    })).toThrow("invalid context usage");
  });

  it("charges accepted tool upserts but not rejected content against session budgets", () => {
    const state = createSessionUpdateValidationState();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "agent_message_chunk",
      content: { type: "image", data: "not-base64", mimeType: "image/png" },
    })).toThrow("canonical base64");
    expect(state.updateCount).toBe(0);
    expect(state.updateBytes).toBe(0);

    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool-1",
      title: "Valid",
    });
    const committedBytes = state.updateBytes;
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool-1",
      title: "Duplicate",
    })).not.toThrow();
    expect(state.updateCount).toBe(2);
    expect(state.updateBytes).toBeGreaterThan(committedBytes);

    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "connection remains usable" },
    });
    expect(state.updateCount).toBe(3);
    expect(state.updateBytes).toBeGreaterThan(committedBytes);
  });

  it("accepts tool upserts, missing starts, and late status corrections like Zed", () => {
    const state = createSessionUpdateValidationState();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "missing",
      status: "in_progress",
    })).not.toThrow();

    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool-1",
      title: "Inspect",
      status: "pending",
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tool-1",
      status: "in_progress",
      rawInput: { path: "README.md" },
    });
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tool-1",
      status: "pending",
    })).not.toThrow();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tool-1",
      status: "completed",
    });
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call_update",
      toolCallId: "tool-1",
      rawOutput: "late output",
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool-1",
      title: "Duplicate",
    })).not.toThrow();
  });

  it("accepts server-owned message IDs across content update kinds", () => {
    const state = createSessionUpdateValidationState();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "agent_message_chunk",
      messageId: "message-1",
      content: { type: "text", text: "one" },
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "agent_message_chunk",
      messageId: "message-1",
      content: { type: "text", text: "two" },
    });
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "agent_thought_chunk",
      messageId: "message-1",
      content: { type: "text", text: "shared by this ACP Server" },
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "user_message_chunk",
      messageId: "message-1",
      content: { type: "text", text: "the raw ID remains server-owned" },
    })).not.toThrow();
    expect(state.messages.size).toBe(1);
  });

  it("validates content semantics in messages, tools, and compaction summaries", () => {
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "agent_message_chunk",
      content: { type: "image", data: "AA==", mimeType: "text/html" },
    })).toThrow("image/* family");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "invalid-tool-content",
      title: "Invalid output",
      content: [{
        type: "content",
        content: { type: "audio", data: "malformed", mimeType: "audio/mpeg" },
      }],
    })).toThrow("canonical base64");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "compaction_update",
      compactionId: "invalid-summary",
      status: "completed",
      summary: [{ type: "resource_link", name: "relative", uri: "./relative" }],
    })).toThrow("resource URI is invalid");

    const state = createSessionUpdateValidationState();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_update",
      compactionId: "streaming-summary",
      status: "in_progress",
    });
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "compaction_summary_chunk",
      compactionId: "streaming-summary",
      content: {
        type: "resource",
        resource: { uri: "urn:bad-blob", blob: "%%%" },
      },
    })).toThrow("canonical base64");
  });

  it("validates diff paths, zero-based locations, and live terminal references", () => {
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "relative-diff",
      title: "Edit",
      content: [{ type: "diff", path: "relative.ts", newText: "next" }],
    })).toThrow("diff path must be an absolute path");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "relative-location",
      title: "Read",
      locations: [{ path: "relative.ts", line: 0 }],
    })).toThrow("location path must be an absolute path");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "negative-line",
      title: "Read",
      locations: [{ path: "/workspace/file.ts", line: -1 }],
    })).toThrow("line must be an integer between 0");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "missing-terminal",
      title: "Run",
      content: [{ type: "terminal", terminalId: "missing" }],
    }, {
      assertTerminalReference: () => {
        throw new Error("Unknown terminal: missing");
      },
    })).toThrow("Unknown terminal");

    const referenced: string[] = [];
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "valid-tool-output",
      title: "Edit and run",
      content: [
        { type: "diff", path: "/workspace/file.ts", oldText: "old", newText: "new" },
        { type: "terminal", terminalId: "terminal-1" },
      ],
      locations: [{ path: "/workspace/file.ts", line: 0 }],
    }, {
      assertTerminalReference: (terminalId) => referenced.push(terminalId),
    })).not.toThrow();
    expect(referenced).toEqual(["terminal-1"]);

    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "tool_call",
      toolCallId: "remote-windows-output",
      title: "Edit on a remote Windows Agent",
      content: [{ type: "diff", path: "C:\\workspace\\file.ts", newText: "new" }],
      locations: [{ path: "\\\\server\\share\\file.ts", line: 0 }],
    })).not.toThrow();
  });

  it("treats ID-addressed plan updates and removals as recoverable upserts", () => {
    const state = createSessionUpdateValidationState();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_removed",
      planId: "unknown",
    })).not.toThrow();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_update",
      plan: { type: "markdown", planId: "plan-1", content: "First" },
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_update",
      plan: { type: "markdown", planId: "plan-1", content: "Second" },
    });
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_removed",
      planId: "plan-1",
    });
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_update",
      plan: { type: "markdown", planId: "plan-1", content: "Too late" },
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "plan_removed",
      planId: "plan-1",
    })).not.toThrow();
  });

  it("enforces cumulative per-session update budgets", () => {
    const countLimited = createSessionUpdateValidationState();
    countLimited.updateCount = 100_000;
    expect(() => validateAndTrackSessionUpdate(countLimited, {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "one too many" },
    })).toThrow("exceeded 100000 updates");

    const byteLimited = createSessionUpdateValidationState();
    byteLimited.updateBytes = 127_999_999;
    expect(() => validateAndTrackSessionUpdate(byteLimited, {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "over budget" },
    })).toThrow("128000000 cumulative bytes");
  });

  it("validates bounded session metadata updates while preserving null clears", () => {
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      title: null,
      updatedAt: null,
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      title: "x".repeat(16_385),
    })).toThrow("title exceeds");
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      updatedAt: "not-a-timestamp",
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      updatedAt: "08/30/2026",
    })).not.toThrow();
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      updatedAt: "x".repeat(257),
    })).toThrow("updatedAt exceeds");
    expect(() => validateAndTrackSessionUpdate(createSessionUpdateValidationState(), {
      sessionUpdate: "session_info_update",
      updatedAt: "2026-08-30T16:00:00+08:00",
    })).not.toThrow();
  });

  it("validates dynamic session controls before mutating tracked state", () => {
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "config_option_update",
      configOptions: [
        { type: "boolean", id: "duplicate", name: "One", currentValue: false },
        { type: "boolean", id: "duplicate", name: "Two", currentValue: true },
      ],
    })).toThrow("duplicate config option ID");
    expect(() => validateAndTrackSessionUpdate(undefined, {
      sessionUpdate: "config_option_update",
      configOptions: [{
        type: "select",
        id: "model",
        name: "Model",
        currentValue: "missing",
        options: [{ value: "fast", name: "Fast" }],
      }],
    })).toThrow("current value");

    const state = createSessionUpdateValidationState();
    expect(() => validateAndTrackSessionUpdate(state, {
      sessionUpdate: "current_mode_update",
      currentModeId: "ghost",
    })).not.toThrow();
    expect(state.currentModeId).toBe("ghost");

    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "current_mode_update",
      currentModeId: "plan",
    });
    expect(state.currentModeId).toBe("plan");
  });
});
