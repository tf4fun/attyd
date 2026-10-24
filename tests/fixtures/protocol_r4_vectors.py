#!/usr/bin/env python3
"""Generates the revision 4 hash vectors that Rust and TypeScript both replay.

Run from the repository root:  python3 tests/fixtures/protocol_r4_vectors.py

Computed by an implementation that shares no code with either runtime
(docs/protocol-r4-design.md §4.6, §5): UTF-8 segmentation, the text chain, window
continuation and the value, object, map and list hashes, with SHA-256 from hashlib and
canonical JSON for the value domain used (strings, integers within 2^53, booleans, null,
arrays, objects).
"""
import base64
import hashlib
import json
import pathlib
import struct

HERE = pathlib.Path(__file__).parent
L = 4096
ZERO = bytes(32)


def jcs(value):
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        assert abs(value) <= 2**53
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ",".join(jcs(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{" + ",".join(json.dumps(k, ensure_ascii=False) + ":" + jcs(value[k]) for k in utf16_sorted(value)) + "}"
    raise TypeError(value)


def utf16_sorted(keys):
    return sorted(keys, key=lambda key: key.encode("utf-16-be"))


def canon(value):
    return jcs(value).encode("utf-8")


def H(*parts):
    h = hashlib.sha256()
    for part in parts:
        h.update(part)
    return h.digest()


def u64(n):
    return struct.pack(">Q", n)


def b64(digest):
    return base64.urlsafe_b64encode(digest).decode().rstrip("=")


def segments(text):
    """Greedy UTF-8 segments of at most L bytes, each ending on a code point boundary."""
    out, current, size = [], bytearray(), 0
    for char in text:
        encoded = char.encode("utf-8")
        if size + len(encoded) > L:
            out.append(bytes(current))
            current, size = bytearray(), 0
        current += encoded
        size += len(encoded)
    if current:
        out.append(bytes(current))
    return out


def chain(segs, before=ZERO):
    states, state = [], before
    for seg in segs:
        state = H(state, seg)
        states.append(state)
    return states


def text_hash(text):
    segs = segments(text)
    states = chain(segs)
    return H(b"t", u64(len(text.encode("utf-8"))), states[-1] if states else ZERO)


# --- text positions (§4.6) -------------------------------------------------------------

def is_text(block):
    return isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str)


def positions(rule, value):
    """JSON Pointers of the text positions of a value of the given rule."""
    found = []
    if rule == "terminal":
        if isinstance(value.get("output"), str):
            found.append(["output"])
    elif rule == "outputView":
        if isinstance(value.get("text"), str):
            found.append(["text"])
    elif rule == "entry":
        if value.get("kind") == "message":
            for i, chunk in enumerate(value.get("chunks", [])):
                if is_text(chunk.get("content")):
                    found.append(["chunks", i, "content", "text"])
        elif value.get("kind") == "compaction":
            for i, chunk in enumerate(value.get("summary", [])):
                if is_text(chunk.get("content")):
                    found.append(["summary", i, "content", "text"])
        elif value.get("kind") == "tool":
            for i, item in enumerate(value.get("call", {}).get("content", [])):
                if item.get("type") == "content" and is_text(item.get("content")):
                    found.append(["call", "content", i, "content", "text"])
    return found


def deep_copy(value):
    return json.loads(json.dumps(value))


def value_hash(rule, value):
    replaced = deep_copy(value)
    for path in positions(rule, value):
        node = replaced
        for token in path[:-1]:
            node = node[token]
        node[path[-1]] = "#" + b64(text_hash(node[path[-1]]))
    return H(b"v", canon(replaced))


# --- nodes (§5) --------------------------------------------------------------------------

def node_hash(node):
    kind = node["kind"]
    if kind == "value":
        return value_hash(node["rule"], node["value"])
    if kind == "object":
        parts = [b"o", canon(node["fields"])]
        for name in utf16_sorted(node["children"]):
            parts += [canon(name), node_hash(node["children"][name])]
        return H(*parts)
    if kind == "map":
        parts = [b"m"]
        for key in utf16_sorted(node["children"]):
            parts += [canon(key), node_hash(node["children"][key])]
        return H(*parts)
    if kind == "list":
        return H(b"l", u64(len(node["items"])), *[node_hash(item) for item in node["items"]])
    raise ValueError(kind)


# --- cases -------------------------------------------------------------------------------

def texts():
    return [
        ("empty", ""),
        ("one byte", "a"),
        ("exactly one segment", "x" * 4096),
        ("one byte over a segment", "x" * 4097),
        ("two full segments", "y" * 8192),
        ("a three-byte character does not straddle the boundary", "aa" + "日" * 1500),
        ("four-byte characters", "😀" * 2500),
        ("two-byte characters on a boundary", "é" * 3000),
        ("mixed", "héllo, 世界 😀 " * 700),
    ]


def text_cases():
    cases = []
    for name, text in texts():
        segs = segments(text)
        states = chain(segs)
        cases.append({
            "name": name,
            "text": text,
            "length": len(text.encode("utf-8")),
            "segments": [len(seg) for seg in segs],
            "chain": [state.hex() for state in states],
            "hash": text_hash(text).hex(),
        })
    return cases


def window_cases():
    cases = []
    for name, text in texts():
        segs = segments(text)
        states = chain(segs)
        for k in sorted({0, 1, len(segs)} & set(range(len(segs) + 1))):
            before = ZERO if k == 0 else states[k - 1]
            tail = b"".join(segs[k:]).decode("utf-8")
            offset = sum(len(seg) for seg in segs[:k])
            cases.append({
                "name": f"{name}, from segment {k}",
                "length": len(text.encode("utf-8")),
                "offset": offset,
                "before": before.hex(),
                "text": tail,
                "hash": text_hash(text).hex(),
            })
    return cases


def value_cases():
    long_text = "word " * 2000
    entries = [
        ("plain value", "none", {"a": 1, "b": [True, None, "x"], "é": "ü"}),
        ("a message entry hashes its text through the chain", "entry", {
            "kind": "message", "role": "agent", "messageId": "m",
            "chunks": [
                {"content": {"type": "text", "text": long_text}},
                {"content": {"type": "image", "data": "AAAA"}},
                {"content": {"type": "text", "text": "tail"}},
            ]}),
        ("a tool entry's content text", "entry", {
            "kind": "tool", "call": {"toolCallId": "t", "content": [
                {"type": "content", "content": {"type": "text", "text": "out"}},
                {"type": "diff", "path": "/p", "newText": "not a text position"},
            ]}}),
        ("a compaction summary", "entry", {
            "kind": "compaction", "compactionId": "c", "status": {}, "summary": [{"content": {"type": "text", "text": "s"}}]}),
        ("text outside a text position is plain JSON", "entry", {
            "kind": "plan", "planId": "p", "text": "not a position"}),
        ("a terminal's output", "terminal", {"terminalId": "t", "output": long_text, "truncated": False}),
        ("an output view's text", "outputView", {"turn": None, "entry": None, "text": "", "other": 0}),
        ("an output view with text", "outputView", {"turn": {"historyRev": 1}, "entry": 3, "text": "héllo", "other": 1}),
    ]
    return [{"name": n, "rule": r, "value": v, "hash": value_hash(r, v).hex()} for n, r, v in entries]


def value(rule, v):
    return {"kind": "value", "rule": rule, "value": v}


def node_cases():
    leaf = value("none", {"x": 1})
    trees = [
        ("an empty map", {"kind": "map", "children": {}}),
        ("an empty list", {"kind": "list", "items": []}),
        ("a list is ordered and counted", {"kind": "list", "items": [leaf, value("none", 2), value("none", None)]}),
        ("a map is keyed", {"kind": "map", "children": {"b": leaf, "a": value("none", "s")}}),
        ("keys sort by UTF-16 code units, not code points", {"kind": "map", "children": {
            "\U0001F600": value("none", 1), "￿": value("none", 2), "a": value("none", 3)}}),
        ("an object has its own fields and named children", {"kind": "object", "fields": {"title": "t", "n": 1}, "children": {
            "controls": leaf, "entries": {"kind": "list", "items": [value("entry", {
                "kind": "message", "role": "user", "chunks": [{"content": {"type": "text", "text": "hi"}}]})]},
            "terminals": {"kind": "map", "children": {"t": value("terminal", {"terminalId": "t", "output": "ok"})}}}}),
    ]
    cases = [{"name": n, "node": t, "hash": node_hash(t).hex()} for n, t in trees]
    # An empty object, an empty map and an empty list differ.
    cases.append({"name": "an empty object", "node": {"kind": "object", "fields": {}, "children": {}},
                  "hash": node_hash({"kind": "object", "fields": {}, "children": {}}).hex()})
    return cases


if __name__ == "__main__":
    (HERE / "protocol-r4-hash-vectors.json").write_text(json.dumps({
        "comment": "Revision 4 hash vectors (docs/protocol-r4-design.md §4.6, §5), computed by an independent implementation (Python hashlib and its own canonical JSON). Both the Rust and the TypeScript hashes must reproduce them.",
        "L": L,
        "text": text_cases(),
        "windows": window_cases(),
        "values": value_cases(),
        "nodes": node_cases(),
    }, indent=1, ensure_ascii=False) + "\n")
