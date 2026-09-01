import { describe, expect, it } from "vitest";
import { validatePermissionRequest } from "../server/permission-validation";
import {
  createSessionUpdateValidationState,
  validateAndTrackSessionUpdate,
} from "../server/session-update-validation";

describe("Agent permission request validation", () => {
  it("accepts a permission-carried tool upsert and requires unique non-empty options", () => {
    const state = createSessionUpdateValidationState();
    expect(() => validatePermissionRequest({
      sessionId: "session",
      toolCall: { toolCallId: "missing" },
      options: [{ optionId: "yes", name: "Allow", kind: "allow_once" }],
    }, state)).not.toThrow();
    expect(state.toolCalls.has("missing")).toBe(true);

    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool",
      title: "Tool",
      status: "pending",
    });
    expect(() => validatePermissionRequest({
      sessionId: "session",
      toolCall: { toolCallId: "tool" },
      options: [],
    }, state)).toThrow("between 1 and 100");
    expect(() => validatePermissionRequest({
      sessionId: "session",
      toolCall: { toolCallId: "tool" },
      options: [
        { optionId: "same", name: "Allow", kind: "allow_once" },
        { optionId: "same", name: "Reject", kind: "reject_once" },
      ],
    }, state)).toThrow("duplicate permission option ID");

    expect(() => validatePermissionRequest({
      sessionId: "session",
      toolCall: { toolCallId: "tool" },
      options: [{ optionId: "yes", name: "Allow", kind: "allow_once" }],
    }, state)).not.toThrow();
  });

  it("keeps a late permission request recoverable even after a terminal update", () => {
    const state = createSessionUpdateValidationState();
    validateAndTrackSessionUpdate(state, {
      sessionUpdate: "tool_call",
      toolCallId: "tool",
      title: "Tool",
      status: "completed",
    });
    expect(() => validatePermissionRequest({
      sessionId: "session",
      toolCall: { toolCallId: "tool" },
      options: [{ optionId: "yes", name: "Allow", kind: "allow_once" }],
    }, state)).not.toThrow();
  });
});
