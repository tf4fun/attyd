import type { BridgeSessionView, SessionDeltaChange } from "./business-api";

export type SessionControlKind = "mode" | "config";

export interface LocalSessionDelta {
  view: BridgeSessionView;
  /** The session control this delta completed, if any. */
  settledControl?: SessionControlKind;
}

/**
 * Apply a live session delta to the current view without refetching it.
 * Interaction and control deltas carry everything their view fields need.
 * Returns `undefined` when the view must be refetched instead: history sync
 * may replace the baseline, an accepted URL elicitation may open a URL flow
 * the delta does not describe, and unknown or malformed changes are never
 * guessed at.
 */
export function applyLocalSessionDelta(
  view: BridgeSessionView,
  change: SessionDeltaChange,
  viewRevision: number,
): LocalSessionDelta | undefined {
  switch (change.kind) {
    case "interaction_upsert":
      return withRevision(upsertInteraction(view, change.interaction), viewRevision);
    case "interaction_remove":
      return withRevision(removeInteraction(view, change.interactionId), viewRevision);
    case "control_update":
      return withRevision(updateControl(view, change), viewRevision);
    default:
      return undefined;
  }
}

function withRevision(
  result: LocalSessionDelta | undefined,
  viewRevision: number,
): LocalSessionDelta | undefined {
  return result == null ? undefined : { ...result, view: { ...result.view, viewRevision } };
}

function upsertInteraction(
  view: BridgeSessionView,
  interaction: Extract<SessionDeltaChange, { kind: "interaction_upsert" }>["interaction"],
): LocalSessionDelta | undefined {
  if (!isRecord(interaction) || typeof interaction.interactionId !== "string" ||
    !isRecord(interaction.request)) return undefined;
  const pending = {
    interactionId: interaction.interactionId,
    operationId: view.activeTurn?.operationId ?? null,
  };
  const { interactions } = view;
  if (interaction.type === "permission") {
    return { view: { ...view, interactions: { ...interactions, permissions: {
      ...interactions.permissions,
      [interaction.interactionId]: { ...pending, request: interaction.request },
    } } } };
  }
  if (interaction.type === "elicitation") {
    return { view: { ...view, interactions: { ...interactions, elicitations: {
      ...interactions.elicitations,
      [interaction.interactionId]: { ...pending, request: interaction.request },
    } } } };
  }
  return undefined;
}

function removeInteraction(view: BridgeSessionView, interactionId: unknown): LocalSessionDelta | undefined {
  if (typeof interactionId !== "string") return undefined;
  const { permissions, elicitations } = view.interactions;
  const elicitation = Object.hasOwn(elicitations, interactionId) ? elicitations[interactionId] : undefined;
  // Accepting a URL elicitation registers a URL flow the delta does not carry.
  if (elicitation?.request.mode === "url") return undefined;
  const { [interactionId]: _permission, ...remainingPermissions } = permissions;
  const { [interactionId]: _elicitation, ...remainingElicitations } = elicitations;
  return { view: { ...view, interactions: {
    ...view.interactions,
    permissions: remainingPermissions,
    elicitations: remainingElicitations,
  } } };
}

function updateControl(
  view: BridgeSessionView,
  change: Extract<SessionDeltaChange, { kind: "control_update" }>,
): LocalSessionDelta | undefined {
  const session = view.workspace.session;
  if (session == null) return undefined;
  let next: NonNullable<BridgeSessionView["workspace"]["session"]>;
  let settledControl: SessionControlKind;
  if (change.control === "mode") {
    if (typeof change.modeId !== "string" || session.modes == null) return undefined;
    next = { ...session, modes: { ...session.modes, currentModeId: change.modeId } };
    settledControl = "mode";
  } else if (change.control === "configuration") {
    if (!Array.isArray(change.configOptions)) return undefined;
    next = { ...session, configOptions: change.configOptions };
    settledControl = "config";
  } else {
    return undefined;
  }
  const operationKind = view.operation?.kind;
  const operation = operationKind === settledControl || operationKind === `set_${settledControl}`
    ? null
    : view.operation;
  return {
    view: { ...view, workspace: { ...view.workspace, session: next }, operation },
    settledControl,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value != null && typeof value === "object" && !Array.isArray(value);
}
