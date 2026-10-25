// Resolving a write whose answer was lost, from the log of the target it was written to
// (docs/protocol-r4-design.md §9.3). The rows of the table themselves are checked in
// protocol-r4-outcome.test.ts against the independent vectors.
import { describe, expect, it } from "vitest";
import type { TargetState } from "../shared/outcome";
import {
  isOperationId,
  newOperationId,
  originOf,
  resolveWrite,
  type Write,
} from "../shared/write-resolution";

const ID = "AAAAAAAAAAAAAAAAAAAAAA";
const EXPECT = "BBBBBBBBBBBBBBBBBBBBBB";
const OTHER = "CCCCCCCCCCCCCCCCCCCCCC";
const write: Write = { id: ID, expect: EXPECT, scope: "global", instance: "I", at: 10 };

function target(partial: Partial<TargetState>): TargetState {
  return { instance: "I", op: OTHER, heldSince: null, ops: [], ...partial };
}

describe("operation IDs", () => {
  it("are 128 random bits in 22 characters of unpadded base64url", () => {
    const id = newOperationId();
    expect(id).toMatch(/^[A-Za-z0-9_-]{22}$/u);
    expect(isOperationId(id)).toBe(true);
    expect(newOperationId()).not.toBe(id);
  });

  it("encode the bytes they were made from", () => {
    expect(newOperationId(() => new Uint8Array(16))).toBe("AAAAAAAAAAAAAAAAAAAAAA");
    expect(newOperationId(() => new Uint8Array(16).fill(255))).toBe("_____________________w");
  });

  it("are recognised by their form", () => {
    expect(isOperationId("short")).toBe(false);
    expect(isOperationId("A".repeat(23))).toBe(false);
    expect(isOperationId("A".repeat(21) + "+")).toBe(false);
    expect(isOperationId("")).toBe(false);
  });
});

describe("a write whose answer was lost", () => {
  it("was admitted when the target's log holds it, whatever followed", () => {
    expect(resolveWrite(write, target({ op: ID, ops: [{ id: ID, after: EXPECT }] }))).toBe("admitted");
    expect(resolveWrite(write, target({ ops: [{ id: ID, after: EXPECT }, { id: OTHER, after: ID }] }))).toBe("admitted");
  });

  it("was definitely not admitted when another operation replaced the one it expected", () => {
    expect(resolveWrite(write, target({ ops: [{ id: OTHER, after: EXPECT }] }))).toBe("not_admitted");
  });

  it("is left to the user while the operation it expected is still the target's", () => {
    expect(resolveWrite(write, target({ op: EXPECT }))).toBe("unknown");
  });

  it("is unknown when the bridge restarted, the session was held again, or the log no longer reaches back", () => {
    expect(resolveWrite(write, target({ instance: "J", ops: [{ id: ID, after: EXPECT }] }))).toBe("unknown");
    expect(resolveWrite(write, target({ heldSince: 11, ops: [{ id: ID, after: EXPECT }] }))).toBe("unknown");
    expect(resolveWrite(write, target({ ops: [{ id: OTHER, after: "OLD" }] }))).toBe("unknown");
    expect(resolveWrite(write, undefined)).toBe("unknown");
  });

  it("is judged with the instance and version the client held, or the target's own when it does not say", () => {
    const bare: Write = { id: ID, expect: EXPECT, scope: "global" };
    expect(resolveWrite(bare, target({ heldSince: 3, ops: [{ id: ID, after: EXPECT }] }))).toBe("unknown");
    expect(resolveWrite(bare, target({ ops: [{ id: ID, after: EXPECT }] }))).toBe("admitted");
  });
});

describe("a create or fork whose answer was lost", () => {
  const sessions = {
    a: { sessionId: "a", origin: OTHER },
    b: { sessionId: "b", origin: ID },
    c: { sessionId: "c" },
  };

  it("is found by the session whose origin is the id", () => {
    expect(originOf(sessions, ID)).toBe("b");
  });

  it("is not found while no held session has that origin", () => {
    expect(originOf(sessions, "DDDDDDDDDDDDDDDDDDDDDD")).toBeUndefined();
    expect(originOf({}, ID)).toBeUndefined();
  });
});
