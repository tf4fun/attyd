// The client's reading of the tree's shape (design §4.2) against the fixture the bridge's tree is
// checked against (src/protocol/views/tests.rs).
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { shapeAt } from "../shared/tree-shape";

interface Row {
  path: string;
  kind: "value" | "object" | "map" | "list";
  rule?: string;
  children?: string[];
}

const rows = (
  JSON.parse(readFileSync(new URL("./fixtures/protocol-r4-shape.json", import.meta.url), "utf8")) as { nodes: Row[] }
).nodes;

/** A path of the pattern with every `*` a distinct token. */
function instance(pattern: string): string[] {
  return pattern
    .slice(1)
    .split("/")
    .map((token, index) => (token === "*" ? `t${index}` : token));
}

describe("the shape of the tree", () => {
  it.each(rows)("$path is a $kind", (row) => {
    const shape = shapeAt(instance(row.path));
    expect(shape?.kind).toBe(row.kind);
    if (shape?.kind === "value") expect(shape.rule).toBe(row.rule ?? "none");
    if (shape?.kind === "object") expect([...shape.children].sort()).toEqual([...(row.children ?? [])].sort());
  });

  it("knows no node where the tree has none", () => {
    for (const path of [
      ["nowhere"],
      ["global", "agent"],
      ["global", "op", "x"],
      ["sessions", "s", "nothing"],
      ["sessions", "s", "entries", "0", "chunks"],
      ["views", "s", "output", "other"],
      ["views", "s", "history", "I", "1", "other", "0"],
      ["views", "s", "history", "I", "1", "turn", "0", "other"],
    ]) {
      expect(shapeAt(path), path.join("/")).toBeUndefined();
    }
  });

  it("is a view's entries that hold entry rules", () => {
    const entry = shapeAt(["views", "s", "history", "I", "1", "turn", "0", "entries", "3"]);
    expect(entry).toEqual({ kind: "value", rule: "entry" });
  });
});
