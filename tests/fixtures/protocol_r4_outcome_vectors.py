#!/usr/bin/env python3
"""Generates the outcome vectors of revision 4 writes (docs/protocol-r4-design.md §9.3).

Run from the repository root:  python3 tests/fixtures/protocol_r4_outcome_vectors.py

An independent statement of the table: given a target's state (instance, `op.id`, `heldSince`,
`ops`) and a request `(x, e)` made while the client held instance `i` and the target at path
version `t`, what the outcome is. The bridge (Rust) and `shared/` (TypeScript) both replay it.
"""
import json
import pathlib

HERE = pathlib.Path(__file__).parent


def outcome(target, request):
    x, e = request["id"], request["expect"]
    if target["instance"] != request["instance"]:
        return {"outcome": "unknown", "reason": "restarted"}
    held_since = target.get("heldSince")
    if held_since is not None and held_since > request["at"]:
        return {"outcome": "unknown", "reason": "released"}
    for record in target["ops"]:
        if record["id"] == x:
            return {"outcome": "admitted", "record": x}
    for record in target["ops"]:
        if record["after"] == e and record["id"] != x:
            return {"outcome": "superseded"}
    if target["op"] == e:
        return {"outcome": "open"}
    return {"outcome": "unknown", "reason": "expired"}


def target(op, ops, instance="I", held_since=None):
    return {"instance": instance, "op": op, "heldSince": held_since, "ops": [{"id": i, "after": a} for i, a in ops]}


def request(x, e, instance="I", at=10):
    return {"id": x, "expect": e, "instance": instance, "at": at}


CASES = [
    ("admitted: its record is in the log",
     target("x", [("x", "e")]), request("x", "e")),
    ("admitted even when later operations followed",
     target("z", [("x", "e"), ("y", "x"), ("z", "y")]), request("x", "e")),
    ("admitted wins over a record that also replaced e",
     target("x", [("y", "e"), ("x", "y")]), request("x", "e")),
    ("superseded: another operation replaced e",
     target("y", [("y", "e")]), request("x", "e")),
    ("superseded, though the replacer is no longer the current op",
     target("z", [("y", "e"), ("z", "y")]), request("x", "e")),
    ("open: e is still the current op, not admitted yet",
     target("e", []), request("x", "e")),
    ("open: e is the current op and the log holds older records",
     target("e", [("e", "d"), ("d", "c")]), request("x", "e")),
    ("expired: e is older than the window and nothing replaced it in it",
     target("z", [("z", "y"), ("y", "w")]), request("x", "e")),
    ("expired: an empty log whose op is not e",
     target("z", []), request("x", "e")),
    ("restarted: another instance, whatever the log says",
     target("x", [("x", "e")], instance="J"), request("x", "e")),
    ("restarted wins over released",
     target("e", [], instance="J", held_since=50), request("x", "e", at=10)),
    ("released: the target was installed after the version held",
     target("e", [], held_since=11), request("x", "e", at=10)),
    ("released wins over an admitted-looking record of the new installation",
     target("x", [("x", "e")], held_since=11), request("x", "e", at=10)),
    ("not released: installed at or before the version held",
     target("e", [], held_since=10), request("x", "e", at=10)),
    ("global has no heldSince",
     target("e", []), request("x", "e", at=0)),
    ("a lost answer: global's op was replaced while the request was lost",
     target("authenticate-2", [("authenticate-2", "e")]), request("create-1", "e")),
]

if __name__ == "__main__":
    (HERE / "protocol-r4-outcome-vectors.json").write_text(json.dumps({
        "comment": "Outcomes of lost write answers (docs/protocol-r4-design.md §9.3), computed by an independent statement of the table. Both the bridge and shared/ must reproduce them.",
        "cases": [
            dict(name=name, target=t, request=r, expected=outcome(t, r)) for name, t, r in CASES
        ],
    }, indent=1) + "\n")
