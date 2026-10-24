// The shape of the revision 4 tree (docs/protocol-r4-design.md §4.2): the kind of each node by
// its path. A full snapshot carries a subtree as plain JSON, so a client learns what is an
// object, a map, a list or a value from here, to hash what it holds (§5). The bridge's tree is
// checked against tests/fixtures/protocol-r4-shape.json, and so is this.

import type { TextRule } from "./tree-hash";

export type Shape =
  | { readonly kind: "value"; readonly rule: TextRule }
  | { readonly kind: "object"; readonly children: ReadonlySet<string> }
  | { readonly kind: "map" }
  | { readonly kind: "list" };

const NONE: Shape = { kind: "value", rule: "none" };
const MAP: Shape = { kind: "map" };
const LIST: Shape = { kind: "list" };

const GLOBAL: Shape = {
  kind: "object",
  children: new Set(["op", "opDetail", "ops", "sessions", "loadFailures", "terminals", "interactions", "urlFlows"]),
};
const CATALOG: Shape = {
  kind: "object",
  children: new Set(["op", "opDetail", "ops", "interactions", "urlFlows", "pages"]),
};
const SESSION: Shape = {
  kind: "object",
  children: new Set([
    "controls", "op", "opDetail", "ops", "interactions", "urlFlows", "terminals", "entries", "prompts", "turns",
  ]),
};
const TURN: Shape = { kind: "object", children: new Set(["entries"]) };

/** The shape of the node at `tokens` (unescaped), or none if the tree has no node there. */
export function shapeAt(tokens: readonly string[]): Shape | undefined {
  const [top, ...rest] = tokens;
  switch (top) {
    case "global":
      return rest.length === 0 ? GLOBAL : inGlobal(rest);
    case "catalog":
      return rest.length === 0 ? CATALOG : inCatalog(rest);
    case "sessions":
      if (rest.length === 0) return MAP;
      return rest.length === 1 ? SESSION : inSession(rest.slice(1));
    case "views":
      return rest.length === 0 ? MAP : inViews(rest);
    default:
      return undefined;
  }
}

function inGlobal([name, ...rest]: readonly string[]): Shape | undefined {
  switch (name) {
    case "op":
    case "opDetail":
      return rest.length === 0 ? NONE : undefined;
    case "ops":
      return collection(LIST, NONE, rest);
    case "sessions":
    case "loadFailures":
    case "interactions":
    case "urlFlows":
      return collection(MAP, NONE, rest);
    case "terminals":
      return collection(MAP, { kind: "value", rule: "terminal" }, rest);
    default:
      return undefined;
  }
}

function inCatalog([name, ...rest]: readonly string[]): Shape | undefined {
  switch (name) {
    case "op":
    case "opDetail":
      return rest.length === 0 ? NONE : undefined;
    case "ops":
    case "pages":
      return collection(LIST, NONE, rest);
    case "interactions":
    case "urlFlows":
      return collection(MAP, NONE, rest);
    default:
      return undefined;
  }
}

function inSession([name, ...rest]: readonly string[]): Shape | undefined {
  switch (name) {
    case "controls":
    case "op":
    case "opDetail":
      return rest.length === 0 ? NONE : undefined;
    case "ops":
    case "prompts":
    case "turns":
      return collection(LIST, NONE, rest);
    case "interactions":
    case "urlFlows":
      return collection(MAP, NONE, rest);
    case "terminals":
      return collection(MAP, { kind: "value", rule: "terminal" }, rest);
    case "entries":
      return collection(LIST, { kind: "value", rule: "entry" }, rest);
    default:
      return undefined;
  }
}

/** `/views/<id>/…`. */
function inViews([, ...rest]: readonly string[]): Shape | undefined {
  const output: Shape = { kind: "value", rule: "outputView" };
  if (rest.length === 0) return MAP;
  const [first, ...more] = rest;
  if (first === "output") {
    if (more.length === 0) return MAP;
    return more.length === 1 && more[0] === "latest" ? output : undefined;
  }
  if (first !== "history") return undefined;
  // history / <instance> / <historyRev> / (output | turn) / <start> …
  if (more.length <= 2) return MAP;
  const [, , kind, ...below] = more;
  if (kind === "output") return below.length === 0 ? MAP : below.length === 1 ? output : undefined;
  if (kind === "turn") {
    if (below.length === 0) return MAP;
    if (below.length === 1) return TURN;
    return below[1] === "entries" ? collection(LIST, { kind: "value", rule: "entry" }, below.slice(2)) : undefined;
  }
  return undefined;
}

/** The collection itself when `rest` is empty, its item when it has one token. */
function collection(own: Shape, item: Shape, rest: readonly string[]): Shape | undefined {
  if (rest.length === 0) return own;
  return rest.length === 1 ? item : undefined;
}
