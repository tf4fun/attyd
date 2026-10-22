//! When a scope is due a checkpoint (docs/protocol.md §5.5): after a pause in
//! its changes, and at the latest a fixed time after the first change that no
//! checkpoint has covered.

use std::time::Duration;

use tokio::time::Instant;

use super::*;

fn at(base: Instant, millis: u64) -> Instant {
    base + Duration::from_millis(millis)
}

#[test]
fn a_scope_without_changes_is_never_due() {
    let pacer = Pacer::default();
    let now = Instant::now();
    assert_eq!(pacer.deadline(), None);
    assert!(pacer.clone().due(at(now, 10_000)).is_empty());
}

#[test]
fn a_change_is_due_after_a_pause() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    pacer.changed("session/s", base);
    assert_eq!(pacer.deadline(), Some(at(base, 500)));
    assert!(pacer.due(at(base, 499)).is_empty());
    assert_eq!(pacer.due(at(base, 500)), ["session/s"]);
    // It was handed out once: nothing is pending now.
    assert_eq!(pacer.deadline(), None);
    assert!(pacer.due(at(base, 5_000)).is_empty());
}

#[test]
fn every_change_postpones_the_pause() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    pacer.changed("session/s", base);
    pacer.changed("session/s", at(base, 400));
    assert_eq!(pacer.deadline(), Some(at(base, 900)));
    assert!(pacer.due(at(base, 899)).is_empty());
    assert_eq!(pacer.due(at(base, 900)), ["session/s"]);
}

#[test]
fn a_scope_that_keeps_changing_is_due_at_the_latest_after_two_seconds() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    for millis in (0..2_000).step_by(400) {
        pacer.changed("session/s", at(base, millis));
    }
    // The last change was at 1600: the pause alone would wait until 2100.
    assert_eq!(pacer.deadline(), Some(at(base, 2_000)));
    assert!(pacer.due(at(base, 1_999)).is_empty());
    assert_eq!(pacer.due(at(base, 2_000)), ["session/s"]);
}

#[test]
fn the_two_seconds_run_from_the_first_change_after_a_checkpoint() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    pacer.changed("session/s", base);
    assert_eq!(pacer.due(at(base, 500)), ["session/s"]);
    pacer.changed("session/s", at(base, 700));
    pacer.changed("session/s", at(base, 1_100));
    assert_eq!(pacer.deadline(), Some(at(base, 1_600)));
}

#[test]
fn scopes_are_paced_apart_and_the_deadline_is_the_nearest() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    pacer.changed("session/a", base);
    pacer.changed("session/b", at(base, 300));
    assert_eq!(pacer.deadline(), Some(at(base, 500)));
    assert_eq!(pacer.due(at(base, 500)), ["session/a"]);
    assert_eq!(pacer.deadline(), Some(at(base, 800)));
}

#[test]
fn a_snapshot_or_an_end_settles_the_scope() {
    let base = Instant::now();
    let mut pacer = Pacer::default();
    pacer.changed("session/s", base);
    // A snapshot carries a digest of its own, an end has nothing to verify.
    pacer.settled("session/s");
    assert_eq!(pacer.deadline(), None);
    assert!(pacer.due(at(base, 5_000)).is_empty());
}
