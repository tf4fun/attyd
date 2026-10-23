//! A client's copy of one scope's document, built from stream events alone (docs/protocol.md
//! §5.3): what the bridge tests compare with the document a commit published.
use serde_json::Value;

use super::Op;
use super::diff;

#[derive(Debug, Clone, Default)]
pub(crate) struct Replica {
    pub epoch: String,
    pub rev: u64,
    /// `{ history?, prompts, live, entries? }`: the paths the ops address.
    pub doc: Value,
}

impl Replica {
    /// The replica a snapshot event installs.
    pub(crate) fn from_snapshot(event: &Value) -> Self {
        let snapshot = &event["snapshot"];
        let mut doc = snapshot["doc"].clone();
        if let Some(entries) = snapshot.get("entries") {
            let mut list = vec![Value::Null; entries["length"].as_u64().unwrap() as usize];
            for pair in entries["changed"].as_array().unwrap() {
                list[pair[0].as_u64().unwrap() as usize] = pair[1].clone();
            }
            doc["entries"] = Value::Array(list);
        }
        Self {
            epoch: snapshot["epoch"].as_str().unwrap().to_string(),
            rev: snapshot["rev"].as_u64().unwrap(),
            doc,
        }
    }

    pub(crate) fn apply(&mut self, event: &Value) -> Result<(), String> {
        if let Some(snapshot) = event.get("snapshot") {
            let mut installed = Self::from_snapshot(event);
            // `update` keeps the entries held in the same epoch; those listed changed.
            if snapshot["entries"]["base"] == "update" && snapshot["epoch"] == self.epoch.as_str() {
                let mut held = self.entries();
                held.resize(installed.entries().len(), Value::Null);
                for pair in snapshot["entries"]["changed"].as_array().into_iter().flatten() {
                    held[pair[0].as_u64().unwrap() as usize] = pair[1].clone();
                }
                installed.doc["entries"] = Value::Array(held);
            }
            *self = installed;
            return Ok(());
        }
        if event.get("digest").is_some() {
            return Ok(());
        }
        let rev = event["rev"].as_u64().ok_or("an ops event names its rev")?;
        if rev != self.rev + 1 {
            return Err(format!("rev {rev} does not follow {}: {event}", self.rev));
        }
        let ops: Vec<Op> = serde_json::from_value(event["ops"].clone())
            .map_err(|error| format!("ops: {error}"))?;
        for op in &ops {
            diff::apply(&mut self.doc, op)
                .map_err(|error| format!("{op:?} does not apply: {error:?}"))?;
        }
        self.rev = rev;
        Ok(())
    }

    pub(crate) fn live(&self) -> Value {
        self.doc["live"].clone()
    }

    pub(crate) fn entries(&self) -> Vec<Value> {
        self.doc["entries"].as_array().cloned().unwrap_or_default()
    }

    /// How many prompts of the document have ended.
    pub(crate) fn ended_prompts(&self) -> usize {
        self.doc["prompts"].as_array().map_or(0, |prompts| {
            prompts
                .iter()
                .filter(|prompt| prompt.get("end").is_some())
                .count()
        })
    }
}
