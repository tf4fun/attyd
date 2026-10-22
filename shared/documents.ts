// The documents of revision 3 as a client reads them (docs/protocol.md §3, §4): a
// session's entries, prompts and `live`, and the `live` of `global` and `catalog`. The
// operation a scope shows names the scope it was written to and the operation it replaced;
// interactions are a member of `live` beside it, not part of it.

import type { SessionInfo } from "@agentclientprotocol/sdk";
import type {
  AgentInfo,
  Interaction,
  LoginTerminal,
  Operation,
  SessionControls,
  SessionSummary,
  Terminal,
  UrlFlow,
} from "./protocol";
import type { Entry, PromptState } from "./entries";

/** The `op` a scope shows (§4.4). */
export type ShownOp = Omit<Operation, "interactions" | "process"> & {
  /** The scope the operation was written to. */
  scope: string;
  /** The operation it replaced in that scope. */
  after: string | null;
};

/** What every scope's `live` has (§4.1). */
export interface ScopeLive {
  op: ShownOp;
  /** Pending, by interaction ID. */
  interactions: Readonly<Record<string, Interaction>>;
  /** Accepted URL flows, by interaction ID. */
  urlFlows: Readonly<Record<string, UrlFlow>>;
}

export interface SessionLive3 extends ScopeLive {
  sessionId: string;
  cwd: string;
  controls: SessionControls;
  /** Agent terminals, with their frozen output once released (§4.6). */
  terminals: Readonly<Record<string, Terminal & { released: boolean }>>;
  closing: boolean;
}

export interface GlobalLive3 extends ScopeLive {
  agent: AgentInfo;
  /** Sessions the bridge holds; `origin` is the create or fork that installed one. */
  sessions: Readonly<Record<string, SessionSummary & { origin?: string }>>;
  terminals: Readonly<Record<string, LoginTerminal>>;
}

export interface CatalogLive3 extends ScopeLive {
  /** Changes when the Agent's list may have changed. */
  revision: string;
  /** The pages listed since `revision` last changed. */
  pages: readonly { readonly sessions: readonly SessionInfo[]; readonly nextCursor: string | null }[];
}

/** A session's document without `epoch` and `rev`. */
export interface SessionBody {
  /** Absent while the history is unknown. */
  history?: "complete" | "partial";
  entries?: readonly Entry[];
  prompts: readonly PromptState[];
  live: SessionLive3;
}
