//! A subscriber's queue (docs/protocol.md §5.1, §5.4): alignment at subscription,
//! events in rev order, a rebuild, and what a slow subscriber is sent instead
//! of the events it could not take.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::protocol::documents::{End, EndReason};
use crate::protocol::engine::{Commit, Engine, EntriesBase, Input};
use crate::protocol::timeline::{History, Timeline};

const SCOPE: &str = "session/S";

fn input(timeline: &Timeline, epoch: &str, live: Value) -> Input {
    Input {
        epoch: epoch.into(),
        history: Some(History::Complete),
        timeline: Some(Arc::new(timeline.clone())),
        live,
    }
}

fn timeline(texts: &[&str]) -> Timeline {
    let mut timeline = Timeline::default();
    for text in texts {
        timeline.begin_prompt(&[json!({ "type": "text", "text": text })]);
    }
    timeline
}

fn subscribed(have: Option<Have>) -> Vec<Subscribed> {
    vec![Subscribed {
        scope: Scope::Session("S".into()),
        have,
        cwd: None,
    }]
}

/// The kinds of the events in a queue, drained.
fn kinds(subscriber: &mut Subscriber) -> Vec<String> {
    std::iter::from_fn(|| subscriber.next())
        .map(|event| match event {
            StreamEvent::Snapshot { snapshot, .. } => format!(
                "snapshot {} rev {} {}",
                snapshot.epoch,
                snapshot.rev,
                snapshot
                    .entries
                    .map_or(String::new(), |entries| format!(
                        "{:?} {:?}",
                        entries.base,
                        entries.changed.iter().map(|(index, _)| *index).collect::<Vec<_>>()
                    ))
            ),
            StreamEvent::Ops { rev, .. } => format!("ops rev {rev}"),
            StreamEvent::Checkpoint { rev, .. } => format!("checkpoint rev {rev}"),
            StreamEvent::End { end, .. } => format!("end {:?}", end.reason),
        })
        .collect()
}

fn published(engine: &mut Engine, epoch: &str, texts: &[&str]) -> Commit {
    engine.commit(SCOPE, &input(&timeline(texts), epoch, json!({ "n": texts.len() })))
}

#[test]
fn a_subscriber_is_aligned_with_a_snapshot_of_every_scope_published() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    assert_eq!(kinds(&mut subscriber), ["snapshot E1 rev 0 Replace [0]"]);
}

#[test]
fn a_subscriber_holding_the_epoch_is_sent_only_what_changed_after_its_version() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    published(&mut engine, "E1", &["a", "b"]);
    published(&mut engine, "E1", &["a", "b", "c"]);
    let mut subscriber = Subscriber::new(&subscribed(Some(Have { epoch: "E1".into(), rev: 1 })));
    subscriber.align(&engine);
    assert_eq!(kinds(&mut subscriber), ["snapshot E1 rev 2 Update [2]"]);
}

#[test]
fn a_scope_not_published_yet_is_aligned_when_it_is() {
    let mut engine = Engine::default();
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    assert_eq!(kinds(&mut subscriber), Vec::<String>::new());
    let commit = published(&mut engine, "E1", &["a"]);
    assert_eq!(commit, Commit::Rebuilt);
    subscriber.rebuilt(SCOPE, &engine);
    assert_eq!(kinds(&mut subscriber), ["snapshot E1 rev 0 Replace [0]"]);
}

#[test]
fn events_follow_in_rev_order_and_only_to_the_scopes_a_subscriber_follows() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    for texts in [["a", "b"].as_slice(), ["a", "b", "c"].as_slice()] {
        let Commit::Ops(event) = published(&mut engine, "E1", texts) else {
            panic!("ops expected");
        };
        subscriber.publish(&event);
    }
    // An event of a scope it does not follow is not queued.
    subscriber.publish(&StreamEvent::Checkpoint {
        scope: "global".into(),
        rev: 9,
        digest: "x".into(),
    });
    assert_eq!(
        kinds(&mut subscriber),
        ["snapshot E1 rev 0 Replace [0]", "ops rev 1", "ops rev 2"]
    );
}

#[test]
fn a_new_epoch_replaces_what_was_queued_with_a_whole_snapshot() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    let Commit::Ops(event) = published(&mut engine, "E1", &["a", "b"]) else {
        panic!("ops expected");
    };
    subscriber.publish(&event);
    assert_eq!(published(&mut engine, "E2", &["z"]), Commit::Rebuilt);
    subscriber.rebuilt(SCOPE, &engine);
    assert_eq!(kinds(&mut subscriber), ["snapshot E2 rev 0 Replace [0]"]);
}

#[test]
fn a_queue_past_its_budget_is_replaced_by_a_snapshot_from_the_last_rev_written() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    // The alignment is written out: the subscriber holds rev 0.
    assert!(subscriber.next().is_some());
    for texts in [["a", "b"].as_slice(), ["a", "b", "c"].as_slice(), ["a", "b", "c", "d"].as_slice()] {
        let Commit::Ops(event) = published(&mut engine, "E1", texts) else {
            panic!("ops expected");
        };
        subscriber.publish(&event);
    }
    assert_eq!(subscriber.queued(), 3);
    subscriber.coalesce(&engine);
    // The events it could not take are replaced by what changed after rev 0.
    assert_eq!(kinds(&mut subscriber), ["snapshot E1 rev 3 Update [1, 2, 3]"]);
}

#[test]
fn an_event_still_queued_never_counts_as_delivered() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    let Commit::Ops(event) = published(&mut engine, "E1", &["a", "b"]) else {
        panic!("ops expected");
    };
    subscriber.publish(&event);
    // Nothing was written out yet: a coalesce starts from nothing held.
    subscriber.coalesce(&engine);
    assert_eq!(kinds(&mut subscriber), ["snapshot E1 rev 1 Replace [0, 1]"]);
}

#[test]
fn an_end_replaces_everything_queued_of_its_scope_and_is_never_coalesced_away() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    let Commit::Ops(event) = published(&mut engine, "E1", &["a", "b"]) else {
        panic!("ops expected");
    };
    subscriber.publish(&event);
    subscriber.end(SCOPE, End { reason: EndReason::Closed, error: None });
    engine.remove(SCOPE);
    subscriber.coalesce(&engine);
    // The scope has no document left to take a snapshot of: only the end stays.
    assert_eq!(kinds(&mut subscriber), ["end Closed"]);
    // Nothing follows an end for that scope.
    subscriber.publish(&StreamEvent::Checkpoint { scope: SCOPE.into(), rev: 5, digest: "x".into() });
    assert_eq!(kinds(&mut subscriber), Vec::<String>::new());
}

#[test]
fn the_budget_is_counted_in_bytes_of_queued_events() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(None));
    subscriber.align(&engine);
    assert!(subscriber.queued_bytes() > 0);
    assert!(!subscriber.over_budget());
    let big = "x".repeat(QUEUE_BUDGET);
    let Commit::Ops(event) = published(&mut engine, "E1", &["a", &big]) else {
        panic!("ops expected");
    };
    subscriber.publish(&event);
    assert!(subscriber.over_budget());
    subscriber.coalesce(&engine);
    assert!(!subscriber.over_budget() || subscriber.queued() == 1);
}

#[test]
fn entries_base_of_a_snapshot_is_replace_without_a_version_held() {
    let mut engine = Engine::default();
    published(&mut engine, "E1", &["a"]);
    let mut subscriber = Subscriber::new(&subscribed(Some(Have { epoch: "E0".into(), rev: 4 })));
    subscriber.align(&engine);
    let Some(StreamEvent::Snapshot { snapshot, .. }) = subscriber.next() else {
        panic!("a snapshot");
    };
    assert_eq!(snapshot.entries.unwrap().base, EntriesBase::Replace);
}
