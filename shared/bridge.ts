import type { ContentBlock, TerminalExitStatus } from "@agentclientprotocol/sdk";

export interface TerminalSnapshot {
  sessionId: string;
  terminalId: string;
  output: string;
  outputBytes?: string;
  truncated: boolean;
  exitStatus?: TerminalExitStatus | null;
  released: boolean;
  outputAppend?: boolean;
  retainedBytes?: number;
}

export interface WorkspaceContextMatch {
  path: string;
  name: string;
  relativePath: string;
  rootName: string;
  size: number;
}

export interface WorkspaceContextAttachment {
  name: string;
  size: number;
  block: Extract<ContentBlock, { type: "resource" }>;
}

export type ConnectionPhase =
  | "starting"
  | "initializing"
  | "ready"
  | "stopped"
  | "error";

export type ElicitationAbortReason =
  | "session_cancelled"
  | "session_closed";

export type SessionRuntimeOperation =
  | "fork"
  | "close"
  | "delete"
  | "mode"
  | "config";

export type AgentTransport = "stdio" | "http" | "ws";

export function isAbsoluteWorkspacePath(value: string): boolean {
  return value.startsWith("/") || /^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\");
}

/** Validate a terminal snapshot carried by a `terminal_update` session delta. */
export function parseTerminalSnapshot(value: unknown): TerminalSnapshot {
  const terminal = requireRecord(value, "terminal");
  requireNonemptyString(terminal.sessionId, "terminal sessionId");
  requireNonemptyString(terminal.terminalId, "terminal terminalId");
  if (typeof terminal.output !== "string") throw new Error("terminal output must be a string");
  if (terminal.outputBytes != null) {
    const encoded = terminal.outputBytes;
    if (
      typeof encoded !== "string" ||
      encoded.length % 4 !== 0 ||
      !/^[A-Za-z0-9+/]*={0,2}$/u.test(encoded)
    ) {
      throw new Error("terminal outputBytes must be base64");
    }
  }
  if (typeof terminal.truncated !== "boolean") throw new Error("terminal requires truncated");
  if (typeof terminal.released !== "boolean") throw new Error("terminal requires released");
  if (terminal.outputAppend != null && typeof terminal.outputAppend !== "boolean") {
    throw new Error("terminal outputAppend must be boolean");
  }
  if (
    terminal.retainedBytes != null &&
    (!Number.isSafeInteger(terminal.retainedBytes) || Number(terminal.retainedBytes) < 0)
  ) {
    throw new Error("terminal retainedBytes must be a non-negative safe integer");
  }
  if (terminal.exitStatus != null) {
    const status = requireRecord(terminal.exitStatus, "terminal exitStatus");
    if (
      status.exitCode != null &&
      (
        !Number.isSafeInteger(status.exitCode) ||
        Number(status.exitCode) < 0 ||
        Number(status.exitCode) > 0xffff_ffff
      )
    ) {
      throw new Error("terminal exitCode must be a uint32 integer or null");
    }
    if (status.signal != null && typeof status.signal !== "string") {
      throw new Error("terminal signal must be a string when provided");
    }
  }
  return terminal as unknown as TerminalSnapshot;
}

function requireRecord(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value as Record<string, unknown>;
}

function requireNonemptyString(value: unknown, label: string): void {
  if (typeof value !== "string" || value.length === 0) throw new Error(`${label} is required`);
}
