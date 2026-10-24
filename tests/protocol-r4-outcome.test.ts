// The client's outcome rule (design §9.3) against the vectors an independent statement of the
// table computed (tests/fixtures/protocol_r4_outcome_vectors.py); src/protocol/outcome.rs
// replays the same file.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { type Outcome, type Request, type TargetState, next, outcome } from "../shared/outcome";

interface Case {
  name: string;
  target: TargetState;
  request: Request;
  expected: Outcome;
}

const cases = (
  JSON.parse(readFileSync(new URL("./fixtures/protocol-r4-outcome-vectors.json", import.meta.url), "utf8")) as {
    cases: Case[];
  }
).cases;

describe("revision 4 write outcomes", () => {
  it("has the vectors", () => {
    expect(cases.length).toBeGreaterThanOrEqual(16);
  });

  it.each(cases)("$name", ({ target, request, expected }) => {
    expect(outcome(target, request)).toEqual(expected);
  });

  it("allows a new request identity only for a superseded one", () => {
    expect(next({ outcome: "admitted", record: "x" })).toBe("done");
    expect(next({ outcome: "superseded" })).toBe("resubmit-with-new-id");
    expect(next({ outcome: "open" })).toBe("repeat-unchanged");
    for (const reason of ["restarted", "released", "expired"] as const) {
      expect(next({ outcome: "unknown", reason })).toBe("ask-the-user");
    }
  });
});
