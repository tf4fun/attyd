import type { TerminalExitStatus } from "@agentclientprotocol/sdk";

export interface TerminalSnapshot {
  sessionId: string;
  terminalId: string;
  output: string;
  truncated: boolean;
  exitStatus?: TerminalExitStatus | null;
  released: boolean;
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
