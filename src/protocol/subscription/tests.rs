//! The stream query (docs/protocol.md §5.1): which scopes a stream follows, the
//! version of the replica a client holds of each (`have`) and the directory to
//! load an unheld session with (`cwd`).

use super::*;
use crate::protocol::documents::{ErrorCode, Scope};

fn query(text: &str) -> Vec<Subscribed> {
    parse_stream_query(text).expect("a valid query")
}

fn refused(text: &str) -> String {
    let error = parse_stream_query(text).expect_err("an invalid query");
    assert_eq!(error.code, ErrorCode::InvalidRequest, "{text}");
    error.message
}

#[test]
fn a_stream_follows_the_scopes_it_names_in_order() {
    let scopes = query("scope=global&scope=session%2FS&scope=catalog");
    assert_eq!(
        scopes.iter().map(|followed| followed.scope.to_string()).collect::<Vec<_>>(),
        ["global", "session/S", "catalog"]
    );
    assert!(scopes.iter().all(|followed| followed.have.is_none() && followed.cwd.is_none()));
}

#[test]
fn have_is_the_epoch_and_rev_of_the_replica_held_of_any_followed_scope() {
    let scopes = query(
        "scope=global&scope=session/S&have=session/S:Zm9vYmFy:12&have=global:G1:0",
    );
    assert_eq!(
        scopes[0].have,
        Some(Have { epoch: "G1".into(), rev: 0 }),
        "global may be held too"
    );
    assert_eq!(scopes[1].have, Some(Have { epoch: "Zm9vYmFy".into(), rev: 12 }));
}

#[test]
fn a_session_id_with_colons_is_still_the_scope_of_a_have() {
    let scopes = query("scope=session/a:b&have=session/a:b:EPOCH_1-x:7");
    assert_eq!(scopes[0].scope, Scope::Session("a:b".into()));
    assert_eq!(scopes[0].have, Some(Have { epoch: "EPOCH_1-x".into(), rev: 7 }));
}

#[test]
fn cwd_names_the_directory_to_load_an_unheld_session_with() {
    let scopes = query("scope=session/S&scope=session/S2&cwd=session/S2:/work/a:b");
    assert_eq!(scopes[0].cwd, None);
    assert_eq!(scopes[1].cwd.as_deref(), Some("/work/a:b"), "the longest followed scope wins");
}

#[test]
fn an_invalid_query_is_refused_whole() {
    for (text, why) in [
        ("", "no scope"),
        ("scope=nowhere", "not a scope"),
        ("scope=global&scope=global", "a scope twice"),
        ("scope=global&flag=1", "an unknown parameter"),
        ("scope=global&have=session/S:E:1", "a have of a scope not followed"),
        ("scope=session/S&have=session/S:E", "a have without a rev"),
        ("scope=session/S&have=session/S:E:x", "a rev that is not a number"),
        ("scope=session/S&have=session/S::3", "a have without an epoch"),
        ("scope=session/S&have=session/S:E:1&have=session/S:E:2", "a have twice"),
        ("scope=session/S&cwd=session/T:/w", "a cwd of a scope not followed"),
        ("scope=session/S&cwd=session/S:", "a cwd without a directory"),
        ("scope=session/S&cwd=session/S:/a&cwd=session/S:/b", "a cwd twice"),
        ("scope=global&cwd=global:/w", "a cwd of a scope that is not a session"),
    ] {
        let message = refused(text);
        assert!(!message.is_empty(), "{why}");
    }
}
