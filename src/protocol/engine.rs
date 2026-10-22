//! The publisher of the client protocol (docs/protocol.md §5.2–§5.5): for each
//! scope, a document at an `epoch` and a `rev`, and the events that take a
//! replica from one version to the next.
//!
//! A commit compares the scope's new state with the published one. Nothing
//! changed: no event. A new epoch (the history was replaced, or the scope is
//! new): the document is rebuilt at rev 0 and subscribers are sent a snapshot.
//! Otherwise the changes are ops, the rev rises by one, and each entry that
//! changed remembers that rev, so that a subscriber that reconnects within the
//! epoch is sent only the entries changed after the version it holds.
//!
//! Entries are compared by their `Arc`: an entry the memory did not touch is not
//! published again, and a changed one is diffed against what was published, so
//! streamed text is an `append`.
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::diff::{self, Text};
use super::documents::End;
use super::digest::{self, Hash};
use super::timeline::{Entry, History, Timeline};
use super::timeline_doc::{AttachmentIndex, publish_entry};
use super::{AppendTail, Op};

/// The version of a document a replica holds (`have`, protocol §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Have {
    pub epoch: String,
    pub rev: u64,
}

/// What a scope publishes: a session's history, timeline and live document, or
/// the live document alone (`global`, `catalog`).
#[derive(Debug, Clone)]
pub(crate) struct Input {
    /// A new value replaces the document; it stays while the history only grows.
    pub epoch: String,
    pub history: Option<History>,
    /// `None` while the history is unknown.
    pub timeline: Option<Arc<Timeline>>,
    pub live: Value,
}

/// What a commit did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Commit {
    Unchanged,
    /// The next rev, as ops.
    Ops(StreamEvent),
    /// A first publication or a new epoch: subscribers are sent a snapshot.
    Rebuilt,
}

/// One event of a stream (§5.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum StreamEvent {
    Snapshot {
        scope: String,
        snapshot: Snapshot,
    },
    Ops {
        scope: String,
        rev: u64,
        ops: Vec<Op>,
    },
    Checkpoint {
        scope: String,
        rev: u64,
        digest: String,
    },
    /// The scope is over: the client drops the document (§5.2).
    End {
        scope: String,
        end: End,
    },
}

impl StreamEvent {
    /// The scope the event belongs to.
    pub(crate) fn scope(&self) -> &str {
        match self {
            Self::Snapshot { scope, .. }
            | Self::Ops { scope, .. }
            | Self::Checkpoint { scope, .. }
            | Self::End { scope, .. } => scope,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    pub epoch: String,
    pub rev: u64,
    pub digest: String,
    /// The document but its entries, `epoch` and `rev`.
    pub doc: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<SnapshotEntries>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SnapshotEntries {
    pub base: EntriesBase,
    pub length: usize,
    pub changed: Vec<(usize, Value)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntriesBase {
    /// The subscriber holds nothing of this epoch: every entry is listed.
    Replace,
    /// The subscriber keeps its entries of this epoch; those listed changed.
    Update,
}

/// An entry as published, and the rev at which it last changed.
struct PublishedEntry {
    source: Arc<Entry>,
    value: Value,
    changed_rev: u64,
    hash: OnceLock<Hash>,
}

impl PublishedEntry {
    fn hash(&self) -> Hash {
        *self.hash.get_or_init(|| digest::entry_hash(&self.value))
    }
}

struct ScopeDoc {
    session: bool,
    epoch: String,
    rev: u64,
    history: Option<History>,
    /// `None` while the history is unknown.
    entries: Option<Vec<Arc<PublishedEntry>>>,
    prompts: Value,
    live: Value,
    attachments: AttachmentIndex,
}

/// Terminal output is a bounded tail; every other string grows or is set.
fn text_mode(path: &str) -> Text {
    if path.starts_with("/live/terminals/") && path.ends_with("/output") {
        Text::Tail
    } else {
        Text::Grow
    }
}

fn history_value(history: Option<History>) -> Value {
    serde_json::to_value(history).expect("a history serializes")
}

impl ScopeDoc {
    fn build(scope: &str, input: &Input) -> Self {
        let session = scope.starts_with("session/");
        let mut attachments = AttachmentIndex::default();
        let entries: Option<Vec<Arc<PublishedEntry>>> =
            input.timeline.as_ref().filter(|_| session).map(|timeline| {
            timeline
                .entries()
                .iter()
                .map(|entry| Arc::new(publish(entry, 0, &mut attachments)))
                .collect()
        });
        let prompts = match input.timeline.as_ref().filter(|_| session) {
            Some(timeline) => serde_json::to_value(timeline.prompts()).expect("prompts serialize"),
            None => json!([]),
        };
        Self {
            session,
            epoch: input.epoch.clone(),
            rev: 0,
            history: input.history.filter(|_| entries.is_some()),
            entries,
            prompts,
            live: input.live.clone(),
            attachments,
        }
    }

    /// The document but its entries, `epoch` and `rev`.
    fn body(&self) -> Value {
        let mut body = Map::new();
        if self.session {
            if let Some(history) = self.history {
                body.insert("history".into(), history_value(Some(history)));
            }
            body.insert("prompts".into(), self.prompts.clone());
        }
        body.insert("live".into(), self.live.clone());
        Value::Object(body)
    }

    fn digest(&self) -> String {
        let mut head = self.body();
        head["epoch"] = Value::String(self.epoch.clone());
        head["rev"] = json!(self.rev);
        if !self.session {
            return digest::scope_digest(&head);
        }
        let hashes: Option<Vec<Hash>> = self
            .entries
            .as_ref()
            .map(|entries| entries.iter().map(|entry| entry.hash()).collect());
        digest::session_digest(&head, hashes.as_deref())
    }
}

fn publish(entry: &Arc<Entry>, rev: u64, attachments: &mut AttachmentIndex) -> PublishedEntry {
    PublishedEntry {
        source: Arc::clone(entry),
        value: publish_entry(entry, attachments),
        changed_rev: rev,
        hash: OnceLock::new(),
    }
}

#[derive(Default)]
pub(crate) struct Engine {
    docs: BTreeMap<String, ScopeDoc>,
}

impl Engine {
    /// Whether committing `input` to `scope` now would change what is published.
    #[cfg(test)]
    pub(crate) fn diverges(&self, scope: &str, input: &Input) -> bool {
        let Some(doc) = self.docs.get(scope) else {
            return true;
        };
        let fresh = ScopeDoc::build(scope, input);
        let values = |doc: &ScopeDoc| {
            doc.entries
                .as_ref()
                .map(|entries| entries.iter().map(|entry| entry.value.clone()).collect::<Vec<_>>())
        };
        doc.epoch != fresh.epoch || doc.body() != fresh.body() || values(doc) != values(&fresh)
    }

    /// The published `live` of `scope`.
    pub(crate) fn live(&self, scope: &str) -> Option<&Value> {
        self.docs.get(scope).map(|doc| &doc.live)
    }

    /// The version `scope` is published at, if it is.
    pub(crate) fn version(&self, scope: &str) -> Option<(&str, u64)> {
        self.docs.get(scope).map(|doc| (doc.epoch.as_str(), doc.rev))
    }

    pub(crate) fn remove(&mut self, scope: &str) {
        self.docs.remove(scope);
    }

    /// The scopes that are published.
    pub(crate) fn scopes(&self) -> impl Iterator<Item = &str> {
        self.docs.keys().map(String::as_str)
    }

    /// Publishes the state of `scope` and returns what changed.
    pub(crate) fn commit(&mut self, scope: &str, input: &Input) -> Commit {
        let Some(doc) = self.docs.get_mut(scope) else {
            self.docs
                .insert(scope.to_string(), ScopeDoc::build(scope, input));
            return Commit::Rebuilt;
        };
        // Entries only grow within an epoch, and a history does not become known
        // or unknown in one; anything else is a new document.
        let shrinks = matches!(
            (&doc.entries, input.timeline.as_ref()),
            (Some(entries), Some(timeline)) if timeline.entries().len() < entries.len()
        );
        let known_changes = doc.session && input.timeline.is_some() != doc.entries.is_some();
        if doc.epoch != input.epoch || (doc.session && shrinks) || known_changes {
            *doc = ScopeDoc::build(scope, input);
            return Commit::Rebuilt;
        }
        let rev = doc.rev + 1;
        let mut ops = Vec::new();

        if doc.session {
            let history = input.history.filter(|_| doc.entries.is_some());
            if history != doc.history {
                ops.push(match history {
                    Some(_) => Op::Set {
                        path: "/history".into(),
                        value: history_value(history),
                    },
                    None => Op::Remove {
                        path: "/history".into(),
                    },
                });
                doc.history = history;
            }
            if let Some(timeline) = input.timeline.as_ref() {
                let prompts = serde_json::to_value(timeline.prompts()).expect("prompts serialize");
                diff::diff("/prompts", &doc.prompts, &prompts, &text_mode, &mut ops);
                doc.prompts = prompts;

                let entries = doc.entries.as_mut().expect("known entries");
                for (index, entry) in timeline.entries().iter().enumerate() {
                    if let Some(published) = entries.get(index) {
                        if Arc::ptr_eq(&published.source, entry) {
                            continue;
                        }
                        let before = ops.len();
                        let value = publish_entry(entry, &mut doc.attachments);
                        diff::diff(
                            &diff::child("/entries", &index.to_string()),
                            &published.value,
                            &value,
                            &text_mode,
                            &mut ops,
                        );
                        let changed_rev = if ops.len() > before {
                            rev
                        } else {
                            published.changed_rev
                        };
                        entries[index] = Arc::new(PublishedEntry {
                            source: Arc::clone(entry),
                            value,
                            changed_rev,
                            hash: OnceLock::new(),
                        });
                    }
                }
                if timeline.entries().len() > entries.len() {
                    let added: Vec<Arc<PublishedEntry>> = timeline.entries()[entries.len()..]
                        .iter()
                        .map(|entry| Arc::new(publish(entry, rev, &mut doc.attachments)))
                        .collect();
                    ops.push(Op::Append {
                        path: "/entries".into(),
                        tail: AppendTail::Values {
                            values: added.iter().map(|entry| entry.value.clone()).collect(),
                        },
                    });
                    entries.extend(added);
                }
            }
        }

        diff::diff("/live", &doc.live, &input.live, &text_mode, &mut ops);
        doc.live = input.live.clone();

        if ops.is_empty() {
            return Commit::Unchanged;
        }
        doc.rev = rev;
        Commit::Ops(StreamEvent::Ops {
            scope: scope.to_string(),
            rev,
            ops,
        })
    }

    /// The snapshot a subscriber holding `have` is sent: only the entries
    /// changed after its version when it holds this epoch, else everything.
    pub(crate) fn snapshot(&self, scope: &str, have: Option<&Have>) -> Option<StreamEvent> {
        let doc = self.docs.get(scope)?;
        let entries = doc.entries.as_ref().map(|entries| {
            let held = have.filter(|have| have.epoch == doc.epoch && have.rev <= doc.rev);
            let (base, changed) = match held {
                Some(have) => (
                    EntriesBase::Update,
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| entry.changed_rev > have.rev)
                        .map(|(index, entry)| (index, entry.value.clone()))
                        .collect(),
                ),
                None => (
                    EntriesBase::Replace,
                    entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| (index, entry.value.clone()))
                        .collect(),
                ),
            };
            SnapshotEntries {
                base,
                length: entries.len(),
                changed,
            }
        });
        Some(StreamEvent::Snapshot {
            scope: scope.to_string(),
            snapshot: Snapshot {
                epoch: doc.epoch.clone(),
                rev: doc.rev,
                digest: doc.digest(),
                doc: doc.body(),
                entries,
            },
        })
    }

    /// The digest of `scope` at its current rev (§5.5).
    pub(crate) fn checkpoint(&self, scope: &str) -> Option<StreamEvent> {
        let doc = self.docs.get(scope)?;
        Some(StreamEvent::Checkpoint {
            scope: scope.to_string(),
            rev: doc.rev,
            digest: doc.digest(),
        })
    }

    /// The block an attachment ID names, while a published entry references it.
    pub(crate) fn attachment(&self, scope: &str, id: &str) -> Option<&Value> {
        self.docs.get(scope)?.attachments.get(id)
    }
}

#[cfg(test)]
mod tests;
