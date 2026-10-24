#!/usr/bin/env python3
"""Generates the protocol fixtures that Rust and TypeScript both replay.

Run from the repository root:  python3 tests/fixtures/protocol_vectors.py

The digests and the documents after each event are computed here, by an
implementation that shares no code with either runtime (docs/protocol.md
§5.3, §5.5): canonical JSON for the value domain used (strings, integers,
booleans, null, arrays, objects with BMP keys), SHA-256 from hashlib, and a
small JSON Pointer patch engine.
"""
import base64
import copy
import hashlib
import json
import pathlib

HERE = pathlib.Path(__file__).parent


def jcs(value):
    """RFC 8785 for the value domain used. JSON numbers are IEEE doubles there (§3.1), so an
    integer beyond +-2^53 is the integer of its nearest double."""
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value) if abs(value) <= 2**53 else str(int(float(value)))
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, list):
        return "[" + ",".join(jcs(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{" + ",".join(json.dumps(key, ensure_ascii=False) + ":" + jcs(item) for key, item in sorted(value.items())) + "}"
    raise TypeError(f"outside the value domain: {value!r}")


def canon(value):
    return jcs(value).encode("utf-8")


def H(*parts):
    h = hashlib.sha256()
    for part in parts:
        h.update(part)
    return h.digest()


def wire(digest):
    return base64.urlsafe_b64encode(digest[:16]).decode().rstrip("=")


def session_digest(doc):
    entries = doc.get("entries")
    head = {k: v for k, v in doc.items() if k != "entries"}
    if entries is None:
        hashes, entries_digest = None, H(b"unknown")
    else:
        hashes = [H(canon(entry)) for entry in entries]
        entries_digest = H(*hashes)
    return wire(H(canon(head), entries_digest)), (None if hashes is None else [h.hex() for h in hashes]), entries_digest.hex()


def scope_digest(doc):
    return wire(H(canon(doc)))


# --- JSON Pointer ops (docs/protocol.md §5.3) -------------------------------------

def tokens(path):
    return [] if path == "" else [t.replace("~1", "/").replace("~0", "~") for t in path[1:].split("/")]


def apply_op(doc, op):
    doc = copy.deepcopy(doc)
    parts = tokens(op["path"])
    if not parts:
        raise ValueError("root ops are not used here")
    parent = doc
    for token in parts[:-1]:
        parent = parent[int(token)] if isinstance(parent, list) else parent[token]
    last = parts[-1]
    key = int(last) if isinstance(parent, list) else last
    kind = op["op"]
    if kind == "set":
        parent[key] = op["value"]
    elif kind == "remove":
        del parent[key]
    elif kind == "append":
        target = parent[key]
        if "values" in op:
            target.extend(op["values"])
        else:
            parent[key] = target + op["text"]
    elif kind == "trim":
        parent[key] = parent[key][op["count"]:]
    else:
        raise ValueError(kind)
    return doc


def text(value):
    return {"type": "text", "text": value}


def message(role, value, message_id=None):
    entry = {"kind": "message", "role": role, "chunks": [{"content": text(value)}]}
    if message_id:
        entry["messageId"] = message_id
    return entry


def live(op_id="op-1", status="completed", kind="open"):
    return {
        "sessionId": "S", "cwd": "/w", "closing": False,
        "op": {"id": op_id, "kind": kind, "status": status, "input": None, "cancelRequested": False},
        "interactions": {}, "urlFlows": {}, "terminals": {},
    }


def snapshot_event(scope, doc, base="replace", changed=None):
    digest, _, _ = session_digest(doc)
    event_doc = {k: v for k, v in doc.items() if k not in ("entries", "epoch", "rev")}
    snapshot = {"epoch": doc["epoch"], "rev": doc["rev"], "digest": digest, "doc": event_doc}
    if "entries" in doc:
        entries = doc["entries"]
        snapshot["entries"] = {
            "base": base, "length": len(entries),
            "changed": [[i, entries[i]] for i in range(len(entries))] if changed is None else [[i, entries[i]] for i in changed],
        }
    return {"scope": scope, "snapshot": snapshot}


def session_scenario():
    scope = "session/S"
    steps = []
    # 1. A full snapshot: one user message with a running prompt.
    d0 = {"epoch": "E1", "rev": 10, "history": "complete",
          "entries": [message("user", "hi")], "prompts": [{"entry": 0}],
          "live": live("op-2", "running", "prompt")}
    steps.append({"name": "a full snapshot installs the document", "event": snapshot_event(scope, d0), "doc": d0})
    # 2-4. Ops: a new entry, streamed text, then the prompt ends.
    doc = d0
    for rev, ops in [
        (11, [{"op": "append", "path": "/entries", "values": [message("agent", "", "m1")]}]),
        (12, [{"op": "append", "path": "/entries/1/chunks/0/content/text", "text": "Hel"}]),
        (13, [
            {"op": "append", "path": "/entries/1/chunks/0/content/text", "text": "lo"},
            {"op": "set", "path": "/prompts/0/end", "value": {"response": {"stopReason": "end_turn"}}},
            {"op": "set", "path": "/live/op/status", "value": "completed"},
        ]),
    ]:
        for op in ops:
            doc = apply_op(doc, op)
        doc = dict(doc, rev=rev)
        steps.append({"name": f"ops take the document to rev {rev}", "event": {"scope": scope, "rev": rev, "ops": ops}, "doc": doc})
    # 5. A checkpoint states the digest at rev 13.
    digest, _, _ = session_digest(doc)
    steps.append({"name": "a checkpoint at the current rev verifies the document",
                  "event": {"scope": scope, "rev": 13, "digest": digest}, "doc": doc})
    # 6. A reconnect within the epoch: only the changed entries come, with the rest of the document.
    d1 = copy.deepcopy(doc)
    d1["rev"] = 15
    d1["entries"][1] = message("agent", "Hello there", "m1")
    d1["entries"].append(message("user", "again"))
    d1["prompts"].append({"entry": 2})
    steps.append({"name": "a differential snapshot replaces the changed entries and appends the new ones",
                  "event": snapshot_event(scope, d1, base="update", changed=[1, 2]), "doc": d1})
    # 7. A new epoch replaces everything, history partial.
    d2 = {"epoch": "E2", "rev": 0, "history": "partial", "entries": [message("agent", "resumed")],
          "prompts": [], "live": live("op-9")}
    steps.append({"name": "a new epoch replaces the document", "event": snapshot_event(scope, d2), "doc": d2})
    # 8. A history that is unknown: no entries.
    d3 = {"epoch": "E3", "rev": 0, "prompts": [], "live": live("op-10", "running", "load")}
    steps.append({"name": "an unknown history has no entries", "event": snapshot_event(scope, d3), "doc": d3})
    return steps


def global_scenario():
    scope = "global"
    steps = []
    g0 = {"epoch": "G1", "rev": 0, "live": {"op": {"id": "n", "kind": "none", "status": "completed"}, "sessions": {}}}
    steps.append({"name": "a global snapshot", "event": {"scope": scope, "snapshot": {
        "epoch": "G1", "rev": 0, "digest": scope_digest(g0), "doc": {"live": g0["live"]}}}, "doc": g0})
    ops = [{"op": "set", "path": "/live/sessions/S", "value": {"sessionId": "S", "title": None}}]
    g1 = apply_op(g0, ops[0])
    g1["rev"] = 1
    steps.append({"name": "an op adds a session summary", "event": {"scope": scope, "rev": 1, "ops": ops}, "doc": g1})
    steps.append({"name": "a global checkpoint", "event": {"scope": scope, "rev": 1, "digest": scope_digest(g1)}, "doc": g1})
    return steps


def digest_vectors():
    text_ = text
    cases = []

    def session(name, doc):
        d, hashes, e = session_digest(doc)
        cases.append({"name": name, "kind": "session", "doc": doc, "entryHashes": hashes, "entriesDigest": e, "digest": d})

    def scope(name, doc):
        cases.append({"name": name, "kind": "scope", "doc": doc, "digest": scope_digest(doc)})

    session("a session whose history is unknown has no entries", {
        "epoch": "e1", "rev": 0, "prompts": [], "live": {"sessionId": "s", "op": {"id": "o", "kind": "load", "status": "running"}}})
    session("a known empty history is not the unknown one", {
        "epoch": "e1", "rev": 0, "history": "complete", "entries": [], "prompts": [], "live": {"sessionId": "s"}})
    session("one message with a prompt outcome", {
        "epoch": "e1", "rev": 3, "history": "complete",
        "entries": [{"kind": "message", "role": "user", "chunks": [{"content": text_("hi")}]},
                    {"kind": "message", "role": "agent", "messageId": "m1", "chunks": [{"content": text_("hello")}]}],
        "prompts": [{"entry": 0, "end": {"response": {"stopReason": "end_turn"}}}],
        "live": {"sessionId": "s", "cwd": "/w", "closing": False}})
    session("tools plans compactions metadata and escapes", {
        "epoch": "e2", "rev": 41, "history": "partial",
        "entries": [
            {"kind": "tool", "call": {"toolCallId": "t", "title": "Run \"x\"\n\\ é 日本", "status": "completed",
                                       "content": [{"type": "terminal", "terminalId": "term"}], "locations": []},
             "notificationMeta": {"trace": "t1"}},
            {"kind": "plan", "update": {"plan": {"planId": "p", "entries": ["a", "b"]}}},
            {"kind": "compaction", "compactionId": "c", "status": "completed",
             "summary": [{"content": text_("short"), "_meta": {"k": 1}}]}],
        "prompts": [],
        "live": {"sessionId": "s", "terminals": {"term": {"terminalId": "term", "output": "ok\n", "truncated": False, "exitStatus": None}}}})
    session("the same entries in another order differ", {
        "epoch": "e1", "rev": 3, "history": "complete",
        "entries": [{"kind": "message", "role": "agent", "messageId": "m1", "chunks": [{"content": text_("hello")}]},
                    {"kind": "message", "role": "user", "chunks": [{"content": text_("hi")}]}],
        "prompts": [{"entry": 1, "end": {"response": {"stopReason": "end_turn"}}}],
        "live": {"sessionId": "s", "cwd": "/w", "closing": False}})
    session("integers up to two to the fifty three are exact", {
        "epoch": "e1", "rev": 9007199254740991, "history": "complete", "entries": [], "prompts": [],
        "live": {"usedBytes": 9007199254740991, "negative": -7, "zero": 0}})
    # The document digest works in the double domain: an integer beyond 2^53 hashes as its
    # nearest double, in the bridge and in a browser alike (§5.5, §12.4).
    session("an integer beyond two to the fifty three hashes as its double", {
        "epoch": "e1", "rev": 1, "history": "complete", "entries": [], "prompts": [],
        "live": {"bytes": 9007199254740993, "also": -9007199254740995}})
    session("the same document written with the doubles", {
        "epoch": "e1", "rev": 1, "history": "complete", "entries": [], "prompts": [],
        "live": {"bytes": 9007199254740992, "also": -9007199254740996}})
    scope("global with its first operation", {
        "epoch": "g1", "rev": 0, "live": {"op": {"id": "n", "kind": "none", "status": "completed"}, "sessions": {}, "terminals": {}}})
    scope("catalog with a listed page", {
        "epoch": "c1", "rev": 2, "live": {"revision": "r1", "pages": [{"sessions": [{"sessionId": "s", "cwd": "/w"}], "nextCursor": None}]}})
    return {"comment": "Digest vectors of docs/protocol.md §5.5, computed by an independent implementation (Python hashlib and its own canonical JSON). Both the Rust and the TypeScript digest must reproduce them.",
            "cases": cases}


def write(name, value):
    (HERE / name).write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def engine_scenario():
    """What the bridge's publisher does for one session, step by step.

    Each step mutates the timeline the way the bridge does (`prompt`, `updates`,
    `end`) and commits it with `live`. `doc` is the published document after the
    step (`null` when nothing changed), `digest` its digest, `changedRevs` the rev
    at which each entry last changed, and `snapshots` what a subscriber that holds
    `have` is sent.
    """
    scope = "session/S"
    epoch = "E1"
    run, done = live("op-2", "running", "prompt"), live("op-2", "completed", "prompt")
    t1 = {"kind": "tool", "call": {"toolCallId": "t1", "title": "Run", "kind": "other", "status": "pending",
                                    "content": [], "locations": []}}
    plan = [
        ({"prompt": [text("hi")]}, run, lambda e, p: (e + [message("user", "hi")], p + [{"entry": 0}])),
        ({"updates": [{"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": text("Hel")}]}, run,
         lambda e, p: (e + [message("agent", "Hel", "m1")], p)),
        ({"updates": [{"sessionUpdate": "agent_message_chunk", "messageId": "m1", "content": text("lo")}]}, run,
         lambda e, p: (e[:1] + [message("agent", "Hello", "m1")], p)),
        ({"updates": [{"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run"}]}, run,
         lambda e, p: (e + [copy.deepcopy(t1)], p)),
        ({"updates": [{"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed"}]}, run,
         lambda e, p: (e[:2] + [dict(t1, call=dict(t1["call"], status="completed"))], p)),
        ({"end": {"stopReason": "end_turn"}}, done,
         lambda e, p: (e, [dict(p[0], end={"response": {"stopReason": "end_turn"}})])),
        ({"updates": []}, done, lambda e, p: (e, p)),
    ]
    steps, entries, prompts, changed, rev, doc = [], [], [], [], -1, None
    for mutation, live_doc, evolve in plan:
        new_entries, new_prompts = evolve(copy.deepcopy(entries), copy.deepcopy(prompts))
        if doc is None or new_entries != entries or new_prompts != prompts or live_doc != doc["live"]:
            rev += 1
            for i, entry in enumerate(new_entries):
                if i >= len(entries) or entry != entries[i]:
                    if i < len(changed):
                        changed[i] = rev
                    else:
                        changed.append(rev)
            entries, prompts = new_entries, new_prompts
            doc = {"epoch": epoch, "rev": rev, "history": "complete", "entries": copy.deepcopy(entries),
                   "prompts": copy.deepcopy(prompts), "live": copy.deepcopy(live_doc)}
            step_doc = doc
        else:
            step_doc = None
        steps.append({"mutation": mutation, "live": live_doc, "doc": step_doc,
                      "digest": session_digest(doc)[0], "changedRevs": list(changed)})
    final = doc
    snapshots = [{"have": None, "base": "replace", "length": len(final["entries"]),
                  "changed": list(range(len(final["entries"])))}]
    for have_rev in (0, 2, 4, final["rev"]):
        snapshots.append({"have": {"epoch": epoch, "rev": have_rev}, "base": "update", "length": len(final["entries"]),
                          "changed": [i for i, r in enumerate(changed) if r > have_rev]})
    snapshots.append({"have": {"epoch": "E0", "rev": 3}, "base": "replace", "length": len(final["entries"]),
                      "changed": list(range(len(final["entries"])))})
    return {"scope": scope, "epoch": epoch, "steps": steps, "final": final,
            "finalDigest": session_digest(final)[0], "snapshots": snapshots}


def global_engine_scenario():
    sessions_a = {"S": {"sessionId": "S", "title": None}}
    sessions_b = {"S": {"sessionId": "S", "title": "T"}}
    terminal = {"login": {"terminalId": "login", "output": "abcd", "truncated": False}}
    op = {"id": "n", "kind": "none", "status": "completed"}
    lives = [
        {"op": op, "sessions": {}, "terminals": {}},
        {"op": op, "sessions": sessions_a, "terminals": {}},
        {"op": op, "sessions": sessions_b, "terminals": terminal},
        {"op": op, "sessions": sessions_b, "terminals": terminal},
    ]
    steps, rev, previous, doc = [], -1, None, None
    for live_doc in lives:
        if previous is None or live_doc != previous:
            rev += 1
            doc = {"epoch": "G1", "rev": rev, "live": copy.deepcopy(live_doc)}
            steps.append({"live": live_doc, "doc": doc, "digest": scope_digest(doc)})
        else:
            steps.append({"live": live_doc, "doc": None, "digest": scope_digest(doc)})
        previous = live_doc
    return {"scope": "global", "epoch": "G1", "steps": steps}


def with_digests(steps, digest_of):
    """Adds the digest of the document after each step."""
    return [dict(step, digest=digest_of(step["doc"])) for step in steps]


if __name__ == "__main__":
    write("protocol-digest-vectors.json", digest_vectors())
    write("protocol-engine-scenario.json", {
        "comment": "What the bridge's publisher emits for a session and for global, step by step, computed independently (docs/protocol.md §5.2–§5.5). Replayed by the Rust engine.",
        "session": engine_scenario(),
        "global": global_engine_scenario(),
    })
