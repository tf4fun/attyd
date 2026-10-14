import type { SessionUpdate } from "@agentclientprotocol/sdk";
import type { TerminalSnapshot } from "../../../shared/bridge";
import type { BridgeSessionView, BridgeTurnOutcome, CollapsedTurn } from "../../../shared/business-api";
import type { CollapsedTurn as V2CollapsedTurn, SessionState, TerminalState as TerminalStateV2 } from "../../../shared/protocol";

/**
 * Processes of turns this client watched live, keyed by
 * `(historyRevision, turnId)` (protocol §9). The client already holds a
 * watched turn's updates from `turn`, so its collapsed process stays inline
 * without fetching the immutable page, until the UI releases it.
 */
export class ObservedProcesses {
  private entries = new Map<string, { revision: string; turnId: string; operationId: string; updates: SessionUpdate[] }>();
  private revision: string | null = null;
  /**
   * The last state of every terminal this client saw. A watched turn's
   * inline process still shows a terminal the document released.
   */
  readonly terminals = new Map<string, TerminalStateV2>();

  /** Records the transition from `previous` to `next` of one session owner. */
  observe(previous: SessionState | undefined, next: SessionState): void {
    for (const terminal of Object.values(next.terminals)) this.terminals.set(terminal.terminalId, terminal);
    const revision = next.history.revision;
    if (revision !== this.revision) {
      // A later commit keeps every watched turn that is still in the history.
      const carried = new Map<string, { revision: string; turnId: string; operationId: string; updates: SessionUpdate[] }>();
      if (revision != null) {
        for (const entry of this.entries.values()) {
          const turn = next.history.collapsedTurns.find((candidate) =>
            candidate.turnId === entry.turnId && candidate.operationId === entry.operationId);
          if (turn != null) carried.set(key(revision, turn.turnId), { ...entry, revision });
        }
      }
      this.entries = carried;
      this.revision = revision;
    }
    const watched = previous?.turn;
    if (watched == null || revision == null || next.turn?.operationId === watched.operationId) return;
    const turn = next.history.collapsedTurns.find((candidate) => candidate.operationId === watched.operationId);
    if (turn == null || turn.processCount === 0) return;
    this.entries.set(key(revision, turn.turnId), {
      revision, turnId: turn.turnId, operationId: watched.operationId, updates: watched.updates,
    });
  }

  get(revision: string | null, turnId: string): SessionUpdate[] | undefined {
    return revision == null ? undefined : this.entries.get(key(revision, turnId))?.updates;
  }

  release(revision: string, turnId: string): boolean {
    return this.entries.delete(key(revision, turnId));
  }

  clear(): void {
    this.entries.clear();
    this.terminals.clear();
    this.revision = null;
  }
}

function key(revision: string, turnId: string): string {
  return `${revision}\u0000${turnId}`;
}

/** The reducer's session view of a v2 session document. */
export function bridgeSessionView(
  state: SessionState,
  epoch: string,
  seq: number,
  processes?: ObservedProcesses,
): BridgeSessionView {
  const { history, controls } = state;
  const outcomes = Object.values(state.outcomes);
  const timeline: SessionUpdate[] = [];
  let expanded = false;
  let cursor = 0;
  const collapsedTurns = history.collapsedTurns.map((turn): CollapsedTurn => {
    for (; cursor < turn.beforeUpdate; cursor += 1) timeline.push(history.timeline[cursor]);
    cursor = turn.afterUpdate;
    const beforeUpdate = timeline.length;
    const compact = history.timeline.slice(turn.beforeUpdate, turn.afterUpdate);
    const observed = processes?.get(history.revision, turn.turnId);
    let visibleRanges = shift(turn, beforeUpdate - turn.beforeUpdate);
    if (observed == null) {
      timeline.push(...compact);
    } else {
      expanded = true;
      const echoed = observed[0]?.sessionUpdate === "user_message_chunk";
      const prompt = echoed ? [] : leadingPrompt(compact);
      const output = compact.slice(leadingPrompt(compact).length);
      // The last overlay this client saw can miss the final answer when it
      // arrived with the commit; history then supplies it.
      const missing = !agentText(observed).endsWith(agentText(output));
      timeline.push(...prompt, ...observed, ...(missing ? output : []));
      const promptLength = echoed ? leadingPrompt(observed).length : prompt.length;
      visibleRanges = [
        ...(promptLength > 0 ? [{ start: beforeUpdate, end: beforeUpdate + promptLength }] : []),
        ...(output.length > 0 ? [{ start: timeline.length - output.length, end: timeline.length }] : []),
      ];
    }
    const afterUpdate = timeline.length;
    return {
      turnId: turn.turnId,
      ...(turn.operationId == null ? {} : { operationId: turn.operationId }),
      visibleRanges,
      beforeUpdate,
      afterUpdate,
      processCount: turn.processCount,
      processIncluded: observed != null,
      historyRevision: history.revision ?? "",
      outcomes: outcomes.flatMap((outcome): BridgeTurnOutcome[] =>
        outcome.operationId === turn.operationId && outcome.status === "completed" && outcome.response != null
          ? [{ operationId: outcome.operationId, afterUpdate, response: outcome.response }]
          : []),
    };
  });
  for (; cursor < history.timeline.length; cursor += 1) timeline.push(history.timeline[cursor]);

  const permissions: BridgeSessionView["interactions"]["permissions"] = {};
  const elicitations: BridgeSessionView["interactions"]["elicitations"] = {};
  for (const interaction of Object.values(state.interactions)) {
    const pending = { interactionId: interaction.interactionId, operationId: interaction.operationId };
    if (interaction.kind === "permission") permissions[interaction.interactionId] = { ...pending, request: interaction.request };
    else elicitations[interaction.interactionId] = { ...pending, request: interaction.request };
  }
  const urlFlows: BridgeSessionView["interactions"]["urlFlows"] = {};
  for (const flow of Object.values(state.urlFlows)) {
    urlFlows[flow.interactionId] = {
      interactionId: flow.interactionId, elicitationId: flow.elicitationId, request: flow.request, status: flow.status,
    };
  }
  const terminals: Record<string, TerminalSnapshot> = {};
  if (expanded) {
    // Released terminals an inline process still references.
    for (const terminal of processes?.terminals.values() ?? []) {
      terminals[terminal.terminalId] = { ...terminal, released: true, sessionId: state.sessionId };
    }
  }
  for (const terminal of Object.values(state.terminals)) {
    terminals[terminal.terminalId] = { ...terminal, sessionId: state.sessionId };
  }

  const controlUpdates: Record<string, SessionUpdate> = {
    available_commands_update: { sessionUpdate: "available_commands_update", availableCommands: controls.commands },
    // `null` means the Agent never named it (protocol §3.3); the catalog's
    // title stays until it does.
    session_info_update: {
      sessionUpdate: "session_info_update",
      ...(controls.info.title == null ? {} : { title: controls.info.title }),
      ...(controls.info.updatedAt == null ? {} : { updatedAt: controls.info.updatedAt }),
      ...(controls.info.meta == null ? {} : { _meta: controls.info.meta as Record<string, unknown> }),
    },
  };
  if (controls.usage != null) {
    controlUpdates.usage_update = {
      sessionUpdate: "usage_update", used: controls.usage.used, size: controls.usage.size,
      ...(controls.usage.cost == null ? {} : { cost: controls.usage.cost }),
    };
  }

  return {
    bridgeEpoch: epoch,
    sessionId: state.sessionId,
    sessionIncarnation: state.incarnation,
    viewRevision: seq,
    historyRevision: history.revision,
    phase: history.phase,
    syncError: history.error,
    historyNotice: history.notice,
    timeline: expanded ? timeline : history.timeline,
    collapsedTurns,
    turnOutcomes: collapsedTurns.flatMap((turn) => turn.outcomes),
    activeTurn: state.turn == null ? null : {
      operationId: state.turn.operationId,
      clientIntentId: state.turn.clientIntentId,
      prompt: state.turn.prompt,
      updates: state.turn.updates,
      terminal: null,
      cancelRequested: state.turn.cancelRequested,
    },
    workspace: {
      cwd: state.cwd,
      session: {
        ...(controls.modes == null ? {} : { modes: controls.modes }),
        ...(controls.configOptions == null ? {} : { configOptions: controls.configOptions }),
      },
    },
    controls: controlUpdates,
    interactions: { permissions, elicitations, urlFlows },
    operation: state.operation == null ? null : {
      operationId: state.operation.operationId, kind: state.operation.kind, stage: state.operation.stage,
    },
    terminals,
  };
}

function shift(turn: V2CollapsedTurn, offset: number): Array<{ start: number; end: number }> {
  return turn.visibleRanges.map(({ start, end }) => ({ start: start + offset, end: end + offset }));
}

function agentText(updates: SessionUpdate[]): string {
  return updates.map((update) => update.sessionUpdate === "agent_message_chunk" && update.content.type === "text"
    ? update.content.text : "").join("");
}

function leadingPrompt(updates: SessionUpdate[]): SessionUpdate[] {
  let end = 0;
  while (end < updates.length && updates[end].sessionUpdate === "user_message_chunk") end += 1;
  return updates.slice(0, end);
}

/**
 * The raw updates that take the reducer from the folded overlay `previous` to
 * the folded overlay `next` of the same turn, or `undefined` when the change
 * cannot be expressed that way and the session must be rebuilt.
 */
export function turnUpdateDelta(previous: readonly SessionUpdate[], next: readonly SessionUpdate[]): SessionUpdate[] | undefined {
  const raw: SessionUpdate[] = [];
  let index = 0;
  let removed: SessionUpdate | undefined;
  for (; index < previous.length; index += 1) {
    const before = previous[index];
    const after = next[index - (removed == null ? 0 : 1)];
    if (before === after) continue;
    if (removed == null && before.sessionUpdate === "plan_update" && next.length === previous.length - 1 &&
      previous.slice(index + 1).every((update, offset) => update === next[index + offset])) {
      removed = before;
      raw.push({ sessionUpdate: "plan_removed", planId: before.plan.planId });
      break;
    }
    if (after == null) return undefined;
    const folded = foldedDelta(before, after, index === previous.length - 1);
    if (folded == null) return undefined;
    raw.push(folded);
  }
  if (removed != null) return raw;
  return [...raw, ...next.slice(previous.length)];
}

function foldedDelta(before: SessionUpdate, after: SessionUpdate, last: boolean): SessionUpdate | undefined {
  if (before.sessionUpdate !== after.sessionUpdate &&
    !(before.sessionUpdate === "tool_call_update" && after.sessionUpdate === "tool_call")) return undefined;
  switch (after.sessionUpdate) {
    case "user_message_chunk":
    case "agent_message_chunk":
    case "agent_thought_chunk": {
      if (!last || before.sessionUpdate !== after.sessionUpdate || !("content" in before)) return undefined;
      const previous = before.content;
      if (previous.type !== "text" || after.content.type !== "text" || !after.content.text.startsWith(previous.text)) return undefined;
      return { ...after, content: { ...after.content, text: after.content.text.slice(previous.text.length) } };
    }
    case "tool_call":
    case "tool_call_update":
      return "toolCallId" in before && before.toolCallId === after.toolCallId ? after : undefined;
    case "plan":
      return after;
    case "plan_update":
      return before.sessionUpdate === "plan_update" && before.plan.planId === after.plan.planId ? after : undefined;
    default:
      return undefined;
  }
}
