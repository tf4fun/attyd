import type { SessionUpdate } from "@agentclientprotocol/sdk";
import { describe, expect, it } from "vitest";
import {
  parseGlobalBusinessEvent,
  parseSessionBusinessEvent,
  type GlobalBusinessEvent,
  type SessionBusinessEvent,
} from "../shared/business-api";
import type { Entry, Op, SessionState } from "../shared/protocol";
import { Replica, sessionSubscription } from "../shared/replica";
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

describe("replica convergence (browser half of the convergence gate)", () => {
  const subscription = sessionSubscription("S", "e1", 1);
  const scope = "session/S";

  for (const delivery of ["follow", "reset", "reconnect"] as const) {
    it(`applying a valid entry stream equals loading the final snapshot (${delivery})`, () => {
      const random = prng(delivery === "follow" ? 0x5eed_0001 : delivery === "reset" ? 0x5eed_0002 : 0x5eed_0003);
      for (let round = 0; round < 60; round += 1) {
        const { states, entries } = generatedStream(random, 40);
        const replica = new Replica();
        replica.subscribe([subscription]);
        const last = states.length - 1;
        const send = (entry: Entry) => replica.receive(JSON.stringify(entry), `e1:${entry.at.seq}`);
        send({ type: "snapshot", scope, at: { epoch: "e1", seq: 1 }, state: states[1] });
        if (delivery === "follow") {
          for (const entry of entries) send(entry);
        } else if (delivery === "reset") {
          // A coalesced reset at k, an HTTP snapshot at m overtaking or
          // trailing the buffered entries, then the rest of the stream.
          const k = 2 + Math.floor(random() * (last - 1));
          const j = Math.min(last, k + Math.floor(random() * 8));
          const m = Math.min(last, k + Math.floor(random() * 12));
          for (const entry of entries.filter(({ at }) => at.seq < k)) send(entry);
          send({ type: "reset", scope, at: { epoch: "e1", seq: k } });
          const after = entries.filter(({ at }) => at.seq > k).map((entry) =>
            entry.type === "ops" && entry.at.seq === k + 1 ? { ...entry, prev: k } : entry);
          for (const entry of after.filter(({ at }) => at.seq <= j)) send(entry);
          expect(replica.install(subscription, { at: { epoch: "e1", seq: m }, state: states[m] })).toBe(true);
          for (const entry of after.filter(({ at }) => at.seq > j)) send(entry);
        } else {
          const k = 2 + Math.floor(random() * (last - 1));
          const p = 1 + Math.floor(random() * k);
          for (const entry of entries.filter(({ at }) => at.seq <= k)) send(entry);
          replica.reconnect(`e1:${p}`);
          for (const entry of entries.filter(({ at }) => at.seq > p)) send(entry);
        }
        expect(replica.fetches()).toEqual([]);
        expect(replica.applied()).toBe(last);
        expect(replica.document(scope)).toEqual(states[last]);
      }
    });
  }
});

/** A random sequence of session documents and the `ops` entries between them. */
function generatedStream(random: () => number, length: number): { states: SessionState[]; entries: Entry[] } {
  let current: SessionState = {
    sessionId: "S", incarnation: 1, cwd: "/work",
    controls: { modes: null, configOptions: null, commands: [], info: { title: null, updatedAt: null, meta: null }, usage: null },
    history: { revision: "r0", phase: "ready", error: null, notice: null, timeline: [], collapsedTurns: [] },
    turn: null, outcomes: {}, operation: null, interactions: {}, urlFlows: {}, terminals: {},
  };
  const states: SessionState[] = [current, current];
  const entries: Entry[] = [];
  for (let seq = 2; seq <= length; seq += 1) {
    const next = structuredClone(current);
    const ops: Op[] = [];
    const turn = next.turn;
    switch (Math.floor(random() * 7)) {
      case 0: {
        const update = { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `t${seq}` } } as const;
        next.history.timeline.push(update);
        ops.push({ op: "append", path: "/history/timeline", values: [update] });
        break;
      }
      case 1: {
        const title = random() < 0.3 ? null : `title ${seq}`;
        next.controls.info.title = title;
        ops.push({ op: "set", path: "/controls/info/title", value: title });
        break;
      }
      case 2: {
        const id = `i/${seq % 5}`;
        const interaction = {
          interactionId: id, kind: "permission" as const, operationId: null,
          request: { sessionId: "S", toolCall: { toolCallId: `c${seq}` }, options: [] },
        };
        next.interactions[id] = interaction;
        ops.push({ op: "set", path: `/interactions/${id.replace("/", "~1")}`, value: interaction });
        break;
      }
      case 3: {
        const id = Object.keys(next.interactions)[0];
        if (id == null) break;
        delete next.interactions[id];
        ops.push({ op: "remove", path: `/interactions/${id.replace("/", "~1")}` });
        break;
      }
      case 4: {
        const terminal = next.terminals.T;
        if (terminal == null) {
          next.terminals.T = { terminalId: "T", output: "", truncated: false, exitStatus: null, released: false };
          ops.push({ op: "set", path: "/terminals/T", value: next.terminals.T });
          break;
        }
        terminal.output += `out${seq};`;
        ops.push({ op: "append", path: "/terminals/T/output", text: `out${seq};` });
        if (terminal.output.length > 20) {
          const count = terminal.output.length - 20;
          terminal.output = terminal.output.slice(count);
          terminal.truncated = true;
          ops.push({ op: "trim", path: "/terminals/T/output", count }, { op: "set", path: "/terminals/T/truncated", value: true });
        }
        break;
      }
      case 5: {
        if (turn == null) {
          next.turn = { operationId: `op${seq}`, clientIntentId: `k${seq}`, prompt: [], updates: [], cancelRequested: false };
          ops.push({ op: "set", path: "/turn", value: next.turn });
          break;
        }
        const lastUpdate = turn.updates.at(-1);
        if (lastUpdate?.sessionUpdate === "agent_message_chunk" && lastUpdate.content.type === "text" && random() < 0.6) {
          lastUpdate.content.text += `+${seq}`;
          ops.push({ op: "append", path: `/turn/updates/${turn.updates.length - 1}/content/text`, text: `+${seq}` });
        } else {
          const update = { sessionUpdate: "agent_message_chunk", content: { type: "text", text: `c${seq}` } } as const;
          turn.updates.push(update);
          ops.push({ op: "append", path: "/turn/updates", values: [update] });
        }
        break;
      }
      default: {
        if (turn == null) break;
        next.outcomes[turn.operationId] = {
          operationId: turn.operationId, clientIntentId: turn.clientIntentId, status: "completed",
          response: { stopReason: "end_turn" }, afterUpdate: next.history.timeline.length,
        };
        next.turn = null;
        ops.push(
          { op: "set", path: `/outcomes/${turn.operationId}`, value: next.outcomes[turn.operationId] },
          { op: "set", path: "/turn", value: null },
        );
      }
    }
    entries.push({ type: "ops", scope: "session/S", at: { epoch: "e1", seq }, prev: seq - 1, ops });
    current = next;
    states.push(current);
  }
  return { states, entries };
}

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
