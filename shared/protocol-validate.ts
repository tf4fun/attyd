// Structural validation of protocol v2 state documents (docs/protocol.md §3),
// mirroring the generated types in `protocol.ts`. Embedded ACP values are
// checked only for the discriminators the client relies on.

import type { GlobalState, SessionState } from "./protocol.js";

export class ProtocolShapeError extends Error {
  constructor(path: string, expected: string) {
    super(`${path || "/"} must be ${expected}`);
    this.name = "ProtocolShapeError";
  }
}

type Check = (value: unknown, path: string) => void;

function fail(path: string, expected: string): never {
  throw new ProtocolShapeError(path, expected);
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const string: Check = (value, path) => { if (typeof value !== "string") fail(path, "a string"); };
const boolean: Check = (value, path) => { if (typeof value !== "boolean") fail(path, "a boolean"); };
const integer: Check = (value, path) => {
  if (!Number.isSafeInteger(value) || (value as number) < 0) fail(path, "a non-negative integer");
};
const number: Check = (value, path) => { if (typeof value !== "number" || !Number.isFinite(value)) fail(path, "a number"); };
const object: Check = (value, path) => { if (!isObject(value)) fail(path, "an object"); };
const any: Check = () => undefined;

function nullable(check: Check): Check {
  return (value, path) => { if (value !== null) check(value, path); };
}

function optional(check: Check): Check {
  return (value, path) => { if (value !== undefined) check(value, path); };
}

function oneOf(...values: string[]): Check {
  return (value, path) => {
    if (typeof value !== "string" || !values.includes(value)) fail(path, `one of ${values.join(", ")}`);
  };
}

function array(item: Check): Check {
  return (value, path) => {
    if (!Array.isArray(value)) fail(path, "an array");
    value.forEach((entry, index) => item(entry, `${path}/${index}`));
  };
}

function record(item: Check, key?: string): Check {
  return (value, path) => {
    if (!isObject(value)) fail(path, "an object");
    for (const [name, entry] of Object.entries(value)) {
      item(entry, `${path}/${name}`);
      if (key != null && (entry as Record<string, unknown>)[key] !== name) fail(`${path}/${name}/${key}`, `"${name}"`);
    }
  };
}

function shape(fields: Record<string, Check>): Check {
  return (value, path) => {
    if (!isObject(value)) fail(path, "an object");
    for (const [name, check] of Object.entries(fields)) check(value[name], `${path}/${name}`);
  };
}

const protocolError = shape({ code: string, message: string });
const sessionUpdate: Check = (value, path) => {
  if (!isObject(value) || typeof value.sessionUpdate !== "string") fail(path, "a SessionUpdate");
};
const contentBlock: Check = (value, path) => {
  if (!isObject(value) || typeof value.type !== "string") fail(path, "a ContentBlock");
};
const sessionPhase = oneOf("cold", "loading", "ready", "running", "reconciling", "blocked");

const interaction: Check = (value, path) => {
  shape({
    interactionId: string,
    kind: oneOf("permission", "elicitation"),
    request: object,
    operationId: nullable(string),
    requestId: optional(string),
  })(value, path);
};

const urlFlow = shape({
  interactionId: string,
  elicitationId: string,
  requestId: optional(string),
  request: object,
  status: oneOf("waiting", "completed", "cancelled"),
});

const request = shape({
  requestId: string,
  operation: string,
  method: string,
  clientIntentId: nullable(string),
  target: nullable(shape({ sessionId: string, incarnation: nullable(integer) })),
  interactionId: nullable(string),
  methodId: nullable(string),
  params: object,
  status: oneOf("running", "succeeded", "failed", "cancelled"),
  result: optional(object),
  error: optional(protocolError),
  terminal: optional(shape({
    output: string, truncated: boolean, exitCode: nullable(number), signal: nullable(number), message: nullable(string),
  })),
});

const sessionSummary = shape({
  sessionId: string,
  incarnation: integer,
  cwd: string,
  title: nullable(string),
  lifecycle: string,
  phase: sessionPhase,
  work: oneOf("idle", "running", "awaiting_input"),
  pending: shape({ permissions: integer, elicitations: integer, urlFlows: integer }),
});

const globalFields: Record<keyof GlobalState, Check> = {
  agent: shape({
    phase: oneOf("starting", "initializing", "ready", "stopped", "error"),
    error: nullable(protocolError),
    launch: shape({
      transport: oneOf("stdio", "http", "ws"),
      command: array(string),
      cwd: string,
      readOnly: boolean,
      additionalDirectories: array(string),
      mcpServers: array(shape({ name: string, type: string })),
    }),
    initialize: nullable(object),
    authenticatedMethodId: nullable(string),
    predecessor: nullable(shape({ epoch: string, reason: string })),
  }),
  catalog: shape({ revision: string }),
  sessions: record(sessionSummary, "sessionId"),
  requests: record(request, "requestId"),
  interactions: record(interaction, "interactionId"),
  urlFlows: record(urlFlow, "interactionId"),
};

const sessionFields: Record<keyof SessionState, Check> = {
  sessionId: string,
  incarnation: integer,
  cwd: string,
  controls: shape({
    modes: nullable(shape({ currentModeId: string, availableModes: array(object) })),
    configOptions: nullable(array(object)),
    commands: array(object),
    info: shape({ title: nullable(string), updatedAt: nullable(string), meta: any }),
    usage: nullable(shape({ used: number, size: number, cost: any })),
  }),
  history: shape({
    revision: nullable(string),
    phase: sessionPhase,
    error: nullable(string),
    notice: nullable(string),
    timeline: array(sessionUpdate),
    collapsedTurns: array(shape({
      turnId: string,
      operationId: nullable(string),
      beforeUpdate: integer,
      afterUpdate: integer,
      processCount: integer,
      visibleRanges: array(shape({ start: integer, end: integer })),
    })),
  }),
  turn: nullable(shape({
    operationId: string,
    clientIntentId: string,
    prompt: array(contentBlock),
    updates: array(sessionUpdate),
    cancelRequested: boolean,
  })),
  outcomes: record(shape({
    operationId: string,
    clientIntentId: string,
    status: oneOf("completed", "failed"),
    response: optional(object),
    error: optional(protocolError),
    prompt: optional(array(contentBlock)),
    afterUpdate: integer,
  }), "operationId"),
  operation: nullable(shape({ operationId: string, kind: string, stage: string, requestId: nullable(string) })),
  interactions: record(interaction, "interactionId"),
  urlFlows: record(urlFlow, "interactionId"),
  terminals: record(shape({
    terminalId: string,
    output: string,
    truncated: boolean,
    exitStatus: nullable(shape({ exitCode: nullable(integer), signal: nullable(string) })),
    released: boolean,
  }), "terminalId"),
};

function validateFields(
  fields: Record<string, Check>,
  value: unknown,
  touched?: Iterable<string>,
): void {
  if (!isObject(value)) fail("", "an object");
  for (const name of touched ?? Object.keys(fields)) {
    const check = fields[name];
    if (check == null) fail(`/${name}`, "a known field");
    check(value[name], `/${name}`);
  }
}

/**
 * Validates a scope's document. With `touched`, only those top-level fields
 * are checked, which is enough after ops that changed only them.
 */
export function validateScopeState(scope: string, value: unknown, touched?: Iterable<string>): void {
  if (scope === "global") validateFields(globalFields, value, touched);
  else if (scope.startsWith("session/")) {
    validateFields(sessionFields, value, touched);
    if (touched == null && `session/${(value as SessionState).sessionId}` !== scope) {
      fail("/sessionId", `the ID of ${scope}`);
    }
  } else fail("", `a document of a known scope, not ${scope}`);
}
