// The entries of a session document as the bridge publishes them (docs/protocol.md §3,
// bridge memory model §3). Entries are never removed within an epoch; their order is the
// order they were received.

/** One received chunk, its own fields kept. */
export interface Chunk {
  readonly content: unknown;
  readonly _meta?: unknown;
  readonly notificationMeta?: unknown;
}

export interface MessageEntry {
  readonly kind: "message";
  readonly role: "user" | "agent" | "thought";
  /** The first `messageId` one of its chunks carried. */
  readonly messageId?: string;
  readonly chunks: readonly Chunk[];
}

export interface ToolEntry {
  readonly kind: "tool";
  /** ACP's sparse `ToolCallUpdate`, reduced: a field is absent until the Agent provides it. */
  readonly call: Readonly<Record<string, unknown>> & { readonly toolCallId: string };
  readonly notificationMeta?: unknown;
}

export interface PlanEntry {
  readonly kind: "plan";
  /** A plan without an ID: a turn's snapshot. */
  readonly legacy?: { readonly entries: readonly { readonly status?: string }[] };
  /** By `planId`: the plan's state, or its removal. */
  readonly update?: {
    readonly planId?: string;
    readonly plan?: { readonly planId: string; readonly entries: readonly { readonly status?: string }[] };
  };
  readonly notificationMeta?: unknown;
}

export interface CompactionEntry {
  readonly kind: "compaction";
  readonly compactionId: string;
  readonly status: string;
  readonly summary: readonly Chunk[];
  readonly error?: string;
  readonly _meta?: unknown;
  readonly notificationMeta?: unknown;
}

export type Entry = MessageEntry | ToolEntry | PlanEntry | CompactionEntry;

/** A prompt the bridge sent: the user entry it sent, and how it ended (absent while it runs). */
export interface PromptState {
  readonly entry: number;
  readonly end?:
    | { readonly response: { readonly stopReason: string } & Record<string, unknown> }
    | { readonly error: { readonly code?: number; readonly message?: string; readonly data?: unknown } }
    | { readonly interrupted: true };
}
