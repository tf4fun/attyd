//! The `restart` action on `global` (protocol §7; audit Q8): with no session followed, nothing
//! passive starts the next Agent connection after the Agent exits, and `restart` does.
use super::recovery::initialized;
use super::*;

const RESTART: &str = "/api/v2/targets/global/actions/restart";

/// Events until the Agent is in one of `phases`.
async fn until_phase(stream: &mut Watch, phases: &[&str]) {
    let wanted: Vec<String> = phases.iter().map(|phase| phase.to_string()).collect();
    stream
        .until(&format!("the Agent is {phases:?}"), move |stream| {
            stream.live("global")["agent"]["phase"]
                .as_str()
                .is_some_and(|phase| wanted.iter().any(|wanted| wanted == phase))
        })
        .await;
}

/// After the Agent exits with no session followed, a read, a `global` stream and a write start no
/// connection; `restart` starts one and `global`'s phase follows to `ready`.
#[tokio::test]
async fn restart_starts_the_connection_nothing_passive_would() {
    let (mut server, link) = Bench::start(json!({})).await;
    let mut stream = server.watch("scope=global&scope=catalog").await;
    stream.event().await;
    link.close().await;
    until_phase(&mut stream, &["stopped", "error"]).await;
    server.document("global").await;
    let expect = server.expect_of("global").await;
    let (status, body) = server
        .op("global", &expect, "create", json!({"cwd": CWD}))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(server.agents.accepted(), 1, "nothing passive starts a connection");

    let (status, body) = server.call(Method::POST, RESTART, Some(json!({}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let mut next = server.agents.connect().await;
    let initialize = next.request("initialize").await;
    next.reply(&initialize, initialized(json!({}))).await;
    until_phase(&mut stream, &["ready"]).await;
    assert_eq!(server.agents.accepted(), 2, "exactly one connection was started");
    stream.converged(&server).await;
}

/// While a connection starts or runs, `restart` is `not_allowed` and starts nothing.
#[tokio::test]
async fn restart_while_a_connection_runs_is_not_allowed() {
    let (server, _link) = Bench::start(json!({})).await;
    let mut stream = server.watch("scope=global").await;
    until_phase(&mut stream, &["ready"]).await;
    let before = server.document("global").await;
    let (status, body) = server.call(Method::POST, RESTART, Some(json!({}))).await;
    not_allowed(status, &body, "a restart while the Agent is ready");
    tokio::time::sleep(QUIET).await;
    assert_eq!(server.agents.accepted(), 1);
    assert_eq!(server.document("global").await, before);
}

/// While the connection initializes, `restart` is `not_allowed` too, and starts nothing (the row of
/// protocol §12.4 names starting and initializing as well as ready).
#[tokio::test]
async fn restart_while_the_connection_initializes_is_not_allowed() {
    let mut server = Bench::launch().await;
    let mut link = server.agents.connect().await;
    let initialize = link.request("initialize").await;
    let mut stream = server.watch("scope=global").await;
    stream.event().await;
    assert_eq!(stream.live("global")["agent"]["phase"], "initializing");

    let (status, body) = server.call(Method::POST, RESTART, Some(json!({}))).await;
    not_allowed(status, &body, "a restart while the Agent initializes");
    tokio::time::sleep(QUIET).await;
    assert_eq!(server.agents.accepted(), 1, "nothing was started");

    link.reply(&initialize, initialized(json!({}))).await;
    until_phase(&mut stream, &["ready"]).await;
    stream.converged(&server).await;
}

/// Only `global` has the action, and its body is an object.
#[tokio::test]
async fn restart_belongs_to_global_and_takes_an_object() {
    let (server, _link) = Bench::start(json!({})).await;
    for scope in ["catalog", "sessions/s"] {
        let path = format!("/api/v2/targets/{scope}/actions/restart");
        let (status, body) = server.call(Method::POST, &path, Some(json!({}))).await;
        not_found(status, &body, &format!("restart in {scope}"));
    }
    let (status, body) = server.call(Method::POST, RESTART, Some(json!([]))).await;
    invalid(status, &body, "a restart whose body is not an object");
    assert_eq!(server.agents.accepted(), 1);
}
