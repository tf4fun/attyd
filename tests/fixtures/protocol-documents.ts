// Checks documents and events against the generated types, in the shapes the
// gate in src/protocol/convergence_tests.rs publishes.
import type {
  CatalogLive,
  Event,
  GlobalLive,
  Operation,
  SessionLive,
} from "../../shared/protocol";

const op = {
  id: "AAAAAAAAAAAAAAAAAAAAAA",
  kind: "prompt",
  status: "running",
  input: { prompt: [{ type: "text", text: "hi" }] },
  cancelRequested: false,
  interactions: {
    p1: {
      interactionId: "p1",
      kind: "permission",
      request: {
        sessionId: "s",
        toolCall: { toolCallId: "t" },
        options: [{ optionId: "allow", name: "Allow", kind: "allow_once" }],
      },
    },
  },
  process: [{ sessionUpdate: "agent_message_chunk", content: { type: "text", text: "hel" } }],
} satisfies Operation;

export const session = {
  op,
  urlFlows: {},
  sessionId: "s",
  cwd: "/s",
  controls: {
    modes: { currentModeId: "ask", availableModes: [{ id: "ask", name: "Ask" }] },
    configOptions: null,
    commands: [],
    info: { title: null, updatedAt: null, meta: null },
    usage: null,
    meta: {},
  },
  terminals: {
    t: { terminalId: "t", output: "cdefgh", truncated: false, exitStatus: null },
  },
} satisfies SessionLive;

export const global = {
  op: {
    id: "AAAAAAAAAAAAAAAAAAAAAB",
    kind: "none",
    status: "completed",
    input: null,
    cancelRequested: false,
    interactions: {},
    result: {},
  },
  urlFlows: {},
  agent: {
    phase: "ready",
    error: null,
    launch: {
      transport: "stdio",
      command: ["agent", "acp"],
      cwd: "/",
      readOnly: false,
      additionalDirectories: [],
      mcpServers: [{ name: "files", type: "stdio" }],
    },
    initialize: null,
    authenticatedMethodId: null,
  },
  sessions: {
    s: {
      sessionId: "s",
      cwd: "/s",
      title: null,
      op: { kind: "prompt", status: "running" },
      pending: { interactions: 1, urlFlows: 0 },
    },
  },
  terminals: {},
} satisfies GlobalLive;

export const catalog = {
  op: { ...global.op },
  urlFlows: {},
  revision: "0",
} satisfies CatalogLive;

export const events = [
  { scope: "session/s", live: { replace: session } },
  {
    scope: "session/s",
    log: {
      from: 0,
      prev: "AAAAAAAAAAAAAAAAAAAAAA",
      entries: [
        {
          turn: { prompt: [{ type: "text", text: "hi" }], reply: [{ type: "text", text: "hello" }] },
          hash: "AAAAAAAAAAAAAAAAAAAAAA",
        },
      ],
    },
    live: { patch: [{ op: "remove", path: "/op/process" }] },
  },
  { scope: "session/s", end: { reason: "deleted" } },
  {
    scope: "session/s",
    end: { reason: "load_failed", error: { code: "agent_error", message: "no such session" } },
  },
] satisfies Event[];

// Actual process projections replace binary/embedded content with references.
// These must be accepted without casting the wire data back to ACP types.
export const processWithAttachments = [
  {
    sessionUpdate: "agent_message_chunk",
    content: { type: "attachment", id: "image-id", block: "image", mimeType: "image/png" },
  },
  {
    sessionUpdate: "tool_call",
    toolCallId: "t",
    title: "Read",
    content: [{ type: "content", content: { type: "attachment", id: "resource-id", block: "resource", uri: "file:///s/readme" } }],
  },
  {
    sessionUpdate: "compaction_update",
    compactionId: "c",
    status: "completed",
    summary: [{ type: "attachment", id: "audio-id", block: "audio", mimeType: "audio/wav" }],
  },
] satisfies NonNullable<Operation["process"]>;
