// Resolving a write whose answer was lost, from the replica of the scope it was written to
// (docs/protocol.md §6.5, §6.6). One case per row of the table.
import { describe, expect, it } from "vitest";
import {
  isOperationId,
  newOperationId,
  originOf,
  resolveWrite,
  type ShownOperation,
  type Write,
} from "../shared/write-resolution";

const ID = "AAAAAAAAAAAAAAAAAAAAAA";
const EXPECT = "BBBBBBBBBBBBBBBBBBBBBB";
const OTHER = "CCCCCCCCCCCCCCCCCCCCCC";
const write: Write = { id: ID, expect: EXPECT, scope: "global" };

function op(partial: Partial<ShownOperation>): ShownOperation {
  return { id: OTHER, scope: "global", after: null, status: "running", ...partial };
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
  it("was admitted when the scope shows its operation", () => {
    expect(resolveWrite(write, op({ id: ID, after: EXPECT }))).toBe("admitted");
    expect(resolveWrite(write, op({ id: ID, status: "completed" }))).toBe("admitted");
  });

  it("was not admitted when the scope still shows the operation it expected, ended", () => {
    expect(resolveWrite(write, op({ id: EXPECT, status: "completed" }))).toBe("not_admitted");
    expect(resolveWrite(write, op({ id: EXPECT, status: "failed" }))).toBe("not_admitted");
  });

  it("is unknown when the expected operation is still running", () => {
    expect(resolveWrite(write, op({ id: EXPECT, status: "running" }))).toBe("unknown");
  });

  it("lost the race when another operation of the scope followed the same expectation", () => {
    expect(resolveWrite(write, op({ id: OTHER, scope: "global", after: EXPECT }))).toBe("not_admitted");
  });

  it("was admitted and followed by another operation when that one's `after` is its id", () => {
    expect(resolveWrite(write, op({ id: OTHER, scope: "global", after: ID }))).toBe("admitted");
  });

  it("is unknown for an operation written to another scope, whatever its `after`", () => {
    // A fork from a session also occupies `global`: its `after` says nothing about `global`.
    expect(resolveWrite(write, op({ id: OTHER, scope: "session/S", after: EXPECT }))).toBe("unknown");
    expect(resolveWrite(write, op({ id: OTHER, scope: "session/S", after: ID }))).toBe("unknown");
  });

  it("is unknown for anything else, and when nothing is shown", () => {
    expect(resolveWrite(write, op({ id: OTHER, after: OTHER }))).toBe("unknown");
    expect(resolveWrite(write, undefined)).toBe("unknown");
  });

  it("is judged on the scope it was written to", () => {
    const inSession: Write = { id: ID, expect: EXPECT, scope: "session/S" };
    expect(resolveWrite(inSession, op({ id: OTHER, scope: "global", after: EXPECT }))).toBe("unknown");
    expect(resolveWrite(inSession, op({ id: OTHER, scope: "session/S", after: EXPECT }))).toBe("not_admitted");
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
