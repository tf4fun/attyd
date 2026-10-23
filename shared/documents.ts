// The documents of revision 3 as a client reads them (docs/protocol.md §3, §4): a
// session's entries, prompts and `live`, and the `live` of `global` and `catalog`. The
// operation a scope shows names the scope it was written to and the operation it replaced;
// interactions are a member of `live` beside it, not part of it.

import type { CatalogLive, GlobalLive, Operation, SessionLive } from "./protocol";
import type { Entry, PromptState } from "./entries";

// The `live` of each scope is the type generated from the bridge's own (src/protocol), so a client
// cannot read a document differently from how the bridge writes it.

/** The `op` a scope shows (§4.4): the scope it was written to and the operation it replaced
 * are members of it; the interactions are a sibling in `live`. */
export type ShownOp = Operation;

export type SessionLive3 = SessionLive;

export type GlobalLive3 = GlobalLive;

export type CatalogLive3 = CatalogLive;

/** A session's document without `epoch` and `rev`. */
export interface SessionBody {
  /** Absent while the history is unknown. */
  history?: "complete" | "partial";
  entries?: readonly Entry[];
  prompts: readonly PromptState[];
  live: SessionLive3;
}
