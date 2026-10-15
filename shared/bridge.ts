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
