//! Regression constraints for protocol v2 terminal authentication results.
use super::*;

#[cfg(unix)]
#[tokio::test]
async fn a_signal_terminated_auth_terminal_preserves_the_signal_in_its_record() {
    let methods = json!([{
        "id": "signalled", "name": "Signalled", "type": "terminal",
        "args": ["-c", "kill -TERM $$"],
    }]);
    let mut peer = Peer::start_with(-1, json!({}), methods, |options| {
        options.transport = crate::options::Transport::Stdio;
        options.command = vec!["/bin/sh".to_string()];
    })
    .await;
    let (status, accepted) = command(
        &peer,
        "POST",
        AUTH_TERMINALS,
        Some("signalled-login"),
        None,
        json!({"methodId": "signalled", "cols": 80, "rows": 24}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let record = finished(&mut peer, "signalled-login").await;
    peer.settle().await;
    peer.stop().await;

    assert_eq!(record["status"], "failed", "{record}");
    assert_eq!(record["methodId"], "signalled", "{record}");
    assert_eq!(
        record["terminal"]["signal"], 15,
        "SIGTERM must remain available as structured terminal state, not be lost on exit: {record}"
    );
}
