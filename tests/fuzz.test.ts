import type { SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  type GlobalBusinessEvent,
  type SessionBusinessEvent,
} from "../shared/business-api";
import { appReducer, initialState, type AppAction } from "../web/src/lib/state";

const eventTypes = new Set<GlobalBusinessEvent["type"] | SessionBusinessEvent["type"]>([
  "bridge/catalog_changed",
  "bridge/connection",
  "bridge/connection_error",
  "acp/authenticated",
  "acp/logged_out",
  "bridge/auth_terminal_started",
  "bridge/auth_terminal_output",
  "bridge/auth_terminal_exited",
  "bridge/session_retired",
  "bridge/session_reset",
  "bridge/session_delta",
  "bridge/session_turn_complete",
  "bridge/session_turn_failed",
]);

describe("deterministic adversarial generation", () => {
  it("never accepts an unknown business event or throws a non-Error for arbitrary JSON", () => {
    const random = prng(0xaced_0001);
    let accepted = 0;
    for (let index = 0; index < 5_000; index += 1) {
      const source = JSON.stringify(randomJson(random, 0));
      for (const parse of [parseGlobalBusinessEvent, parseSessionBusinessEvent]) {
        try {
          const event = parse(source);
          accepted += 1;
          expect(eventTypes.has(event.type)).toBe(true);
        } catch (error) {
          expect(error).toBeInstanceOf(Error);
        }
      }
    }
    // The corpus is not useful if it only exercises rejection paths.
    expect(accepted).toBeGreaterThan(0);
  });

  it("preserves reducer invariants under shuffled and cross-session updates", () => {
    const random = prng(0xaced_0002);
    let state = {
      ...initialState,
      session: { sessionId: "current" },
    };

    for (let index = 0; index < 2_500; index += 1) {
      state = appReducer(state, generatedSessionAction(random, index));

      expect(uniqueEntityIds(state.timeline, "tool")).toBe(true);
      expect(uniqueEntityIds(state.timeline, "plan")).toBe(true);
      expect(uniqueEntityIds(state.timeline, "compaction")).toBe(true);
      const visibleText = JSON.stringify(state.timeline);
      expect(visibleText).not.toContain("CROSS_SESSION_SENTINEL");
      expect(JSON.stringify(state.terminalSnapshots)).not.toContain("CROSS_SESSION_SENTINEL");
    }

    expect(state.backgroundEvents.length).toBeGreaterThan(100);
    expect(state.timeline.length).toBeGreaterThan(0);
  }, 30_000);
});

function generatedSessionAction(random: () => number, index: number): AppAction {
  const crossSession = random() < 0.22;
  const sessionId = crossSession ? "other" : "current";
  const marker = crossSession ? "CROSS_SESSION_SENTINEL" : `visible-${index}`;
  const choice = Math.floor(random() * 9);
  const notification = (update: SessionUpdate): AppAction => ({
    type: "session/turn_update",
    notification: { sessionId, update },
  });

  switch (choice) {
    case 0:
      return notification({
        sessionUpdate: "agent_message_chunk",
        messageId: `m-${index}`,
        content: { type: "text", text: marker },
      });
    case 1:
      return notification({
        sessionUpdate: "tool_call_update",
        toolCallId: `tool-${index % 7}`,
        title: marker,
        status: random() < 0.5 ? "in_progress" : "completed",
      });
    case 2:
      return notification({
        sessionUpdate: "tool_call",
        toolCallId: `tool-${index % 7}`,
        title: marker,
        kind: "read",
        status: "pending",
      });
    case 3:
      return notification({
        sessionUpdate: "plan_update",
        plan: {
          type: "markdown",
          planId: `plan-${index % 3}`,
          content: marker,
        },
      });
    case 4:
      return notification({
        sessionUpdate: "plan_removed",
        planId: `plan-${index % 3}`,
      });
    case 5:
      return notification({
        sessionUpdate: "compaction_update",
        compactionId: `compact-${index % 4}`,
        status: random() < 0.5 ? "in_progress" : "completed",
        summary: [{ type: "text", text: marker }],
      });
    case 6:
      return notification({
        sessionUpdate: "compaction_summary_chunk",
        compactionId: `compact-${index % 4}`,
        content: { type: "text", text: marker },
      });
    case 7:
      return notification({
        sessionUpdate: "available_commands_update",
        availableCommands: [{ name: `command-${index % 5}`, description: marker }],
      });
    default:
      return {
        type: "session/terminal_update",
        terminal: {
          sessionId,
          terminalId: `terminal-${index % 70}`,
          output: marker,
          truncated: random() < 0.2,
          released: random() < 0.5,
        },
      };
  }
}

function uniqueEntityIds(
  timeline: ReturnType<typeof appReducer>["timeline"],
  type: "tool" | "plan" | "compaction",
): boolean {
  const ids = timeline.filter((item) => item.type === type).map(({ id }) => id);
  return new Set(ids).size === ids.length;
}

function randomJson(random: () => number, depth: number): unknown {
  if (depth >= 3) return randomPrimitive(random);
  switch (Math.floor(random() * 7)) {
    case 0:
      return null;
    case 1:
    case 2:
      return randomPrimitive(random);
    case 3:
      return Array.from({ length: Math.floor(random() * 5) }, () => randomJson(random, depth + 1));
    default: {
      const result: Record<string, unknown> = {};
      const keys = ["type", "requestId", "sessionId", "bridgeEpoch", "viewRevision", "change", "phase", "response"];
      for (let index = 0; index < Math.floor(random() * 7); index += 1) {
        result[keys[Math.floor(random() * keys.length)]] = randomJson(random, depth + 1);
      }
      if (random() < 0.12) result.type = [...eventTypes][Math.floor(random() * eventTypes.size)];
      if (random() < 0.06) result.type = "bridge/connection";
      return result;
    }
  }
}

function randomPrimitive(random: () => number): string | number | boolean {
  const choice = Math.floor(random() * 3);
  if (choice === 0) return `s-${Math.floor(random() * 1000)}`;
  if (choice === 1) return Math.floor(random() * 2000) - 1000;
  return random() < 0.5;
}

function prng(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
    return state / 0x1_0000_0000;
  };
}
