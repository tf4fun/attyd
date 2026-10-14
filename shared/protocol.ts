// Generated from src/protocol by `npm run protocol:types`. Do not edit.
// Protocol v2: docs/protocol.md.

import type {
  AvailableCommand,
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
