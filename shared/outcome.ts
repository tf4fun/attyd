// What became of a write whose answer was lost (docs/protocol-r4-design.md §9.3), the client
// half of src/protocol/outcome.rs. Both replay tests/fixtures/protocol-r4-outcome-vectors.json.
//
// A request `(x, e)` is operation ID `x` written with `expect = e` to a target, while the client
// held instance `i` and the target at path version `t`. The rule reads what the tree publishes of
// the target: its `op`, its `ops` and, for a session, `heldSince`.

export interface Logged {
  id: string;
  after: string | null;
}

export interface TargetState {
  instance: string;
  /** `op.id`. */
  op: string;
  /** The revision at which the bridge started holding the session; null for global and catalog. */
  heldSince: number | null;
  ops: readonly Logged[];
}

export interface Request {
  id: string;
  expect: string;
  instance: string;
  /** The path version of the target the client held. */
  at: number;
}

export type Reason = "restarted" | "released" | "expired";

export type Outcome =
  | { outcome: "admitted"; record: string }
  | { outcome: "superseded" }
  | { outcome: "open" }
  | { outcome: "unknown"; reason: Reason };

/** The outcome of `request` against `target`: the rows of the table in order. */
export function outcome(target: TargetState, request: Request): Outcome {
  if (target.instance !== request.instance) return { outcome: "unknown", reason: "restarted" };
  if (target.heldSince !== null && target.heldSince > request.at) {
    return { outcome: "unknown", reason: "released" };
  }
  const admitted = target.ops.find((record) => record.id === request.id);
  if (admitted !== undefined) return { outcome: "admitted", record: admitted.id };
  // `e` still being the target's `op` means no operation replaced it, whatever the log says: it is
  // checked before `superseded`, which must never be answered for a request that may yet be admitted.
  if (target.op === request.expect) return { outcome: "open" };
  if (target.ops.some((record) => record.after === request.expect)) return { outcome: "superseded" };
  return { outcome: "unknown", reason: "expired" };
}

/** What a client may do next with a request, by its outcome (§9.3). */
export type Next =
  /** It took effect: its result is in the tree. */
  | "done"
  /** Never executed and can never be: the input is still the user's, a new request may carry it. */
  | "resubmit-with-new-id"
  /** Not admitted yet: only the same `(x, e)` may be repeated, unchanged. */
  | "repeat-unchanged"
  /** Left to the user, never resent automatically. */
  | "ask-the-user";

export function next(result: Outcome): Next {
  switch (result.outcome) {
    case "admitted":
      return "done";
    case "superseded":
      return "resubmit-with-new-id";
    case "open":
      return "repeat-unchanged";
    case "unknown":
      return "ask-the-user";
  }
}
