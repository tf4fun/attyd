//! The documents of the scopes `global`, `catalog` and `session/<id>` read back from the tree, for
//! the tests that were written against them (the same assembly as `shared/scope-feed.ts`): the
//! operation is its head and detail again, a session's entries, prompts and history are the
//! lists and fields the bridge publishes.
use serde_json::{Map, Value, json};

use super::super::tree::escape;

/// A read-only assertion view of a scope assembled from the R4 client's held node. It does
/// not apply events or maintain a second replication protocol.
#[derive(Debug, Clone, Default)]
pub(crate) struct Document {
    pub doc: Value,
    pub rev: u64,
}

impl Document {
    pub(crate) fn live(&self) -> Value {
        self.doc["live"].clone()
    }

    pub(crate) fn entries(&self) -> Vec<Value> {
        self.doc["entries"].as_array().cloned().unwrap_or_default()
    }

    pub(crate) fn ended_prompts(&self) -> usize {
        self.doc["prompts"].as_array().map_or(0, |prompts| {
            prompts.iter().filter(|prompt| prompt.get("end").is_some()).count()
        })
    }
}

/// The tree path of a scope.
pub(crate) fn path_of(scope: &str) -> String {
    match scope.strip_prefix("session/") {
        Some(id) => format!("/sessions/{}", escape(id)),
        None => format!("/{scope}"),
    }
}

/// The scope of a path that is a scope's root.
pub(crate) fn scope_of(path: &str) -> Option<String> {
    match path {
        "/global" | "/catalog" => Some(path[1..].to_string()),
        _ => {
            let id = path.strip_prefix("/sessions/")?;
            (!id.contains('/')).then(|| format!("session/{}", id.replace("~1", "/").replace("~0", "~")))
        }
    }
}

/// An operation's head and detail as the operation a scope showed.
fn operation(head: &Value, detail: Option<&Value>) -> Value {
    let mut op = head.as_object().cloned().unwrap_or_default();
    let detail = detail.and_then(Value::as_object);
    op.insert("input".into(), detail.and_then(|d| d.get("input")).cloned().unwrap_or(Value::Null));
    if let Some(result) = detail.and_then(|d| d.get("result")) {
        op.insert("result".into(), result.clone());
    }
    if let Some(error) = detail.and_then(|d| d.get("error")) {
        op.insert("error".into(), error.clone());
    } else if let Some(summary) = op.get("error").cloned() {
        let mut error = Map::new();
        error.insert("code".into(), summary["code"].clone());
        error.insert("message".into(), json!(""));
        if let Some(agent) = summary.get("agentCode") {
            error.insert("data".into(), json!({ "code": agent }));
        }
        op.insert("error".into(), Value::Object(error));
    }
    Value::Object(op)
}

/// The document of `scope` for its tree `value`: `{ history?, entries?, prompts, live }` for a
/// session, `{ live }` for the others.
pub(crate) fn document(scope: &str, value: &Value) -> Option<Value> {
    let members = value.as_object()?;
    let op = operation(members.get("op")?, members.get("opDetail"));
    let mut live = Map::new();
    let session = scope.starts_with("session/");
    for (key, member) in members {
        let own = matches!(key.as_str(), "op" | "opDetail" | "ops" | "loadFailures");
        let document_level = session
            && matches!(key.as_str(), "history" | "entries" | "prompts" | "turns" | "historyRev" | "heldSince");
        if !own && !document_level {
            live.insert(key.clone(), member.clone());
        }
    }
    live.insert("op".into(), op);
    let mut doc = Map::new();
    if session {
        for key in ["history", "entries", "turns"] {
            if let Some(member) = members.get(key) {
                doc.insert(key.into(), member.clone());
            }
        }
        doc.insert("prompts".into(), members.get("prompts").cloned().unwrap_or_else(|| json!([])));
    }
    doc.insert("live".into(), Value::Object(live));
    Some(Value::Object(doc))
}

/// The body a read of the scope's document answered when scopes were documents:
/// `{ scope, snapshot: { epoch, rev, digest, doc, entries? } }`, `doc` without its entries.
pub(crate) fn snapshot_body(scope: &str, instance: &str, at: u64, value: &Value) -> Option<Value> {
    let mut doc = document(scope, value)?;
    let entries = doc.as_object_mut()?.remove("entries");
    let mut snapshot = json!({ "epoch": instance, "rev": at, "digest": "", "doc": doc });
    if let Some(entries) = entries.as_ref().and_then(Value::as_array) {
        snapshot["entries"] = json!({
            "base": "replace",
            "length": entries.len(),
            "changed": entries.iter().enumerate().map(|(i, e)| json!([i, e])).collect::<Vec<_>>(),
        });
    }
    Some(json!({ "scope": scope, "snapshot": snapshot }))
}
