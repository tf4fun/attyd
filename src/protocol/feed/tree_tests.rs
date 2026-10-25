//! The feed commits the revision 4 tree (docs/protocol-r4-design.md §4.1):
//! one revision per commit that changed anything, under an instance of the process.
use serde_json::json;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::session_registry::SessionRegistry;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn registry() -> SessionRegistry {
    let mut registry = SessionRegistry::new("epoch");
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    registry
}

#[test]
fn the_first_commit_is_revision_one_and_a_commit_without_a_change_is_no_revision() {
    let mut feed = Feed::new();
    let registry = registry();
    assert_eq!(feed.tree().rev(), 0);
    feed.commit(&registry, &agent(), 1);
    assert_eq!(feed.tree().rev(), 1);
    feed.commit(&registry, &agent(), 1);
    assert_eq!(feed.tree().rev(), 1);
}

#[test]
fn one_commit_installing_a_session_is_one_revision_for_all_its_paths() {
    let mut feed = Feed::new();
    let mut registry = SessionRegistry::new("epoch");
    feed.commit(&registry, &agent(), 1);
    registry
        .open_new("epoch", "s", "/work", json!({ "sessionId": "s" }))
        .unwrap();
    feed.commit(&registry, &agent(), 1);
    let tree = feed.tree();
    assert_eq!(tree.rev(), 2);
    assert_eq!(tree.get("/global/sessions/s").unwrap().rev(), 2);
    assert_eq!(tree.get("/sessions/s").unwrap().srev(), 2);
    assert_eq!(tree.get("/catalog").unwrap().srev(), 1);
}

#[test]
fn each_feed_has_a_fresh_instance() {
    let a = Feed::new();
    let b = Feed::new();
    assert_ne!(a.tree().instance(), b.tree().instance());
    assert_eq!(a.tree().instance().len(), 22, "128 bits in unpadded base64url");
}

#[test]
fn a_released_session_leaves_the_tree() {
    let mut feed = Feed::new();
    let mut registry = registry();
    feed.commit(&registry, &agent(), 1);
    registry
        .sessions
        .get_mut("s")
        .unwrap()
        .state
        .live
        .as_mut()
        .unwrap()
        .lifecycle = crate::runtime_state::SessionLifecycle::Closed;
    feed.commit(&registry, &agent(), 1);
    assert!(feed.tree().get("/sessions/s").is_none());
    assert!(feed.tree().get("/global/sessions/s").is_none());
    assert_eq!(feed.tree().rev(), 2);
}
