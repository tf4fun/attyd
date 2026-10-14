// Generated from src/protocol by `npm run protocol:types`. Do not edit.
// Protocol v2: docs/protocol.md.

import type {
  AvailableCommand,
  ContentBlock,
  Cost,
  SessionConfigOption,
  SessionMode,
} from "@agentclientprotocol/sdk";

export type SessionControls = { modes: SessionModes | null, 
/**
 * `null`: never provided; `[]`: provided empty. Preferred over `modes`
 * whenever non-null.
 */
configOptions: SessionConfigOption[] | null, commands: AvailableCommand[], info: SessionInfo, usage: SessionUsage | null, };

export type SessionModes = { currentModeId: string, availableModes: SessionMode[], };

export type SessionInfo = { title: string | null, updatedAt: string | null, meta: unknown | null, };

export type SessionUsage = { used: number, size: number, cost: Cost | null, };

export type Op = { "op": "set", path: string, value: unknown, } | { "op": "remove", path: string, } | { "op": "append", path: string, } & ({ values: unknown[], } | { text: string, }) | { "op": "trim", path: string, count: number, };

export type AppendTail = { values: unknown[], } | { text: string, };

export type Turn = { 
/**
 * The user's message, normalized.
 */
prompt: Array<Block>, 
/**
 * The Agent's message content, in order, normalized.
 */
reply: Array<Block>, };

export type Block = AttachmentRef | Extract<ContentBlock, { type: "text" | "resource_link" }>;

export type PromptBlock = AttachmentRef | ContentBlock;

export type AttachmentRef = { type: "attachment", 
/**
 * Lowercase hex SHA-256 of the block's canonical JSON.
 */
id: string, block: AttachmentKind, mimeType?: string, 
/**
 * A resource's URI.
 */
uri?: string, };

export type AttachmentKind = "image" | "audio" | "resource";

export type LogEntry = { turn: Turn, 
/**
 * `h_i` in its wire form (protocol §3.3).
 */
hash: string, };
