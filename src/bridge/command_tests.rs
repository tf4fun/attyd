//! Protocol v2 session commands, intents and observation leases, end to end
//! over the real routes, coordinator and ACP transport (issue #23
//! acceptance). Every check also runs the convergence gate, so each record
//! a command commits reaches the replicas.
use axum::http::StatusCode;

use super::convergence_tests::{Documents, Peer, SESSION, create, held, session};
use super::stream_client::{HttpStream, get, owner_scope, send};
use super::*;
use crate::protocol::{GLOBAL, session_scope};

mod review_intent_ownership;
mod review_leases;
mod review_owner;
mod review_process_lifetime;
mod review_replication;
mod review_turns;

async fn epoch(peer: &Peer) -> String {
    let (status, global) = get(&peer.router, "/api/v2/global").await;
    assert_eq!(status, StatusCode::OK);
    global["at"]["epoch"].as_str().unwrap().to_string()
}

/// `"<epoch>:<incarnation>"` of a held session.
async fn tag_of(peer: &mut Peer, session_id: &str) -> String {
    let epoch = epoch(peer).await;
    let documents = peer.check().await;
    let incarnation = documents[&session_scope(session_id)]["incarnation"]
        .as_u64()
        .unwrap();
    format!("\"{epoch}:{incarnation}\"")
}

async fn tag(peer: &mut Peer) -> String {
    tag_of(peer, SESSION).await
}

/// The stream scope of a held session's owner.
async fn owner(peer: &mut Peer, session_id: &str) -> String {
    let epoch = epoch(peer).await;
    let documents = peer.check().await;
    owner_scope(
        session_id,
        &epoch,
        documents[&session_scope(session_id)]["incarnation"]
            .as_u64()
            .unwrap(),
    )
}

async fn command(
    peer: &Peer,
    method: &str,
    path: &str,
    key: Option<&str>,
    tag: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut headers = Vec::new();
    if let Some(key) = key {
        headers.push(("idempotency-key", key.to_string()));
    }
    if let Some(tag) = tag {
        headers.push(("if-match", tag.to_string()));
    }
    send(&peer.router, method, path, &headers, Some(body)).await
}

/// A replica's recovery by intent: the record whose `clientIntentId` it
/// chose (protocol §5.2).
fn find(documents: &Documents, intent: &str) -> Option<Value> {
    documents[GLOBAL]["requests"]
        .as_object()?
        .values()
        .find(|request| request["clientIntentId"] == intent)
        .cloned()
}

async fn finished(peer: &mut Peer, intent: &str) -> Value {
    let documents = peer
        .until(&format!("the record of {intent} finishes"), |documents| {
            find(documents, intent).is_some_and(|request| request["status"] != "running")
        })
        .await;
    find(&documents, intent).unwrap()
}

fn mode_path() -> String {
    format!("/api/v2/sessions/{SESSION}/mode")
}

/// Starts a turn through `POST /turns`; returns the Agent's prompt.
async fn start_turn(peer: &mut Peer, key: &str) -> (Value, Value) {
    let epoch = epoch(peer).await;
    let documents = peer.check().await;
    let tag = format!(
        "\"{epoch}:{}:{}\"",
        session(&documents)["incarnation"],
        session(&documents)["history"]["revision"].as_str().unwrap(),
    );
    let (status, accepted) = command(
        peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/turns"),
        Some(key),
        Some(&tag),
        json!({"prompt": [{"type": "text", "text": "Run it"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["scopes"], json!([session_scope(SESSION)]));
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    (accepted, prompt)
}

/// A permission request from the Agent; returns its interaction ID.
async fn request_permission(peer: &mut Peer, id: u64) -> String {
    let before = session(&peer.check().await)["interactions"]
        .as_object()
        .unwrap()
        .len();
    peer.send(json!({
        "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {
            "sessionId": SESSION, "toolCall": {"toolCallId": format!("tool-{id}")},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
        },
    }))
    .await;
    let documents = peer
        .until("the permission is pending", |documents| {
            session(documents)["interactions"]
                .as_object()
                .unwrap()
                .len()
                > before
        })
        .await;
    session(&documents)["interactions"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, interaction)| {
            interaction["request"]["toolCall"]["toolCallId"] == format!("tool-{id}")
        })
        .map(|(id, _)| id.clone())
        .unwrap()
}

fn allow() -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": "allow"}})
}

#[tokio::test]
async fn two_clients_setting_modes_each_find_their_own_record_by_intent() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    for (key, tag) in [(None, Some(tag.as_str())), (Some("no-tag"), None)] {
        let (status, missing) = command(
            &peer,
            "PUT",
            &mode_path(),
            key,
            tag,
            json!({"modeId": "code"}),
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{missing}");
        assert_eq!(missing["error"]["code"], "precondition_required");
    }

    // Client A asks for `code`; the Agent refuses it.
    let (status, a) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode-a"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["result"]["disposition"], "accepted");
    assert_eq!(a["scopes"], json!([GLOBAL, session_scope(SESSION)]));
    let a_id = a["result"]["requestId"].clone();
    // While A runs, another submission is refused synchronously and
    // records nothing.
    let (status, busy) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode-busy"),
        Some(&tag),
        json!({"modeId": "ask"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{busy}");
    assert_eq!(busy["error"]["code"], "conflict");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/set_mode");
    assert_eq!(request["params"]["modeId"], "code");
    let documents = peer.check().await;
    assert_eq!(
        session(&documents)["operation"]["requestId"],
        a_id,
        "the session operation names its record"
    );
    assert!(
        a["at"]["seq"].as_u64().unwrap() <= documents_seq(&peer).await,
        "the record is committed at the command's position"
    );
    peer.send(json!({
        "jsonrpc": "2.0", "id": request["id"],
        "error": {"code": -32000, "message": "mode refused"},
    }))
    .await;
    let refused = finished(&mut peer, "mode-a").await;
    assert_eq!(refused["requestId"], a_id);
    assert_eq!(refused["operation"], "set_mode");
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["error"]["code"], "agent_error");
    assert_eq!(refused["error"]["data"]["message"], "mode refused");
    assert_eq!(refused["params"], json!({"modeId": "code"}));

    // Client B sets `ask`; its HTTP response is lost.
    let (status, _) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode-b"),
        Some(&tag),
        json!({"modeId": "ask"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = peer.next_request().await;
    assert_eq!(request["params"]["modeId"], "ask");
    peer.reply(&request, json!({})).await;
    let set = finished(&mut peer, "mode-b").await;
    assert_eq!(set["status"], "succeeded");
    assert_eq!(set["result"], json!({"kind": "done"}));
    assert_eq!(set["params"], json!({"modeId": "ask"}));

    let documents = peer.check().await;
    assert!(find(&documents, "mode-busy").is_none());
    assert_eq!(find(&documents, "mode-a").unwrap()["status"], "failed");
    // B's resend answers from its record and runs nothing.
    let (status, again) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode-b"),
        Some(&tag),
        json!({"modeId": "ask"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["result"]["disposition"], "duplicate");
    assert_eq!(again["result"]["requestId"], set["requestId"]);
    assert!(again["result"].get("resultExpired").is_none());
    peer.no_request().await;
    peer.settle().await;
    peer.stop().await;
}

async fn documents_seq(peer: &Peer) -> u64 {
    peer.cut().await.0
}

fn config_options() -> Value {
    json!([
        {"id": "a", "name": "A", "type": "boolean", "currentValue": false},
        {"id": "b", "name": "B", "type": "boolean", "currentValue": false},
    ])
}

async fn offer_config(peer: &mut Peer) {
    peer.update(json!({
        "sessionUpdate": "config_option_update", "configOptions": config_options(),
    }))
    .await;
    peer.until("the config options arrive", |documents| {
        session(documents)["controls"]["configOptions"]
            .as_array()
            .is_some_and(|options| options.len() == 2)
    })
    .await;
}

#[tokio::test]
async fn refused_controls_and_a_held_close_keep_their_result_in_global_requests() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"close": {}}})).await;
    create(&mut peer).await;
    offer_config(&mut peer).await;
    let tag = tag(&mut peer).await;

    let (status, _) = command(
        &peer,
        "PUT",
        &format!("/api/v2/sessions/{SESSION}/config/a"),
        Some("config-a"),
        Some(&tag),
        json!({"value": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/set_config_option");
    assert_eq!(request["params"]["configId"], "a");
    peer.send(json!({
        "jsonrpc": "2.0", "id": request["id"],
        "error": {"code": -32000, "message": "config refused"},
    }))
    .await;
    let refused = finished(&mut peer, "config-a").await;
    assert_eq!(refused["operation"], "set_config");
    assert_eq!(refused["status"], "failed");
    assert_eq!(refused["error"]["data"]["message"], "config refused");
    assert_eq!(refused["params"], json!({"configId": "a", "value": true}));

    // A close whose HTTP response is dropped: the result outlives the scope.
    let (status, close) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/close"),
        Some("close"),
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{close}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/close");
    peer.reply(&request, json!({})).await;
    peer.until("the closed session leaves", |documents| !held(documents))
        .await;
    let closed = finished(&mut peer, "close").await;
    assert_eq!(closed["status"], "succeeded");
    assert_eq!(closed["operation"], "close");
    assert_eq!(closed["target"]["sessionId"], SESSION);
    assert_eq!(
        find(&peer.check().await, "config-a").unwrap()["status"],
        "failed"
    );

    // A command naming the retired owner is refused, never re-run.
    let (status, gone) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("after-close"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    peer.no_request().await;
    peer.stop().await;
}

#[tokio::test]
async fn one_key_on_two_targets_is_an_intent_conflict() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    offer_config(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (status, _) = command(
        &peer,
        "PUT",
        &format!("/api/v2/sessions/{SESSION}/config/a"),
        Some("shared"),
        Some(&tag),
        json!({"value": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = peer.next_request().await;
    let (status, conflict) = command(
        &peer,
        "PUT",
        &format!("/api/v2/sessions/{SESSION}/config/b"),
        Some("shared"),
        Some(&tag),
        json!({"value": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "intent_conflict");
    peer.reply(&request, json!({"configOptions": config_options()}))
        .await;
    assert_eq!(finished(&mut peer, "shared").await["status"], "succeeded");

    // Two interactions given the same answer under one key.
    let (_, prompt) = start_turn(&mut peer, "turn").await;
    let first = request_permission(&mut peer, 901).await;
    let second = request_permission(&mut peer, 902).await;
    let (status, answered) = command(
        &peer,
        "POST",
        &format!("/api/v2/interactions/{first}"),
        Some("answer"),
        Some(&tag),
        allow(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    let delivered = peer.next_request().await;
    assert_eq!(delivered["id"], 901);
    let (status, conflict) = command(
        &peer,
        "POST",
        &format!("/api/v2/interactions/{second}"),
        Some("answer"),
        Some(&tag),
        allow(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "intent_conflict");
    assert!(
        session(&peer.check().await)["interactions"]
            .get(&second)
            .is_some(),
        "the second interaction is still pending"
    );
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.stop().await;
}

#[tokio::test]
async fn a_lost_response_recovers_from_public_state_or_gets_result_expired() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"close": {}}})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;

    // Acknowledged by another page.
    let (_, accepted) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("acknowledged"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    let request = peer.next_request().await;
    peer.reply(&request, json!({})).await;
    let record = finished(&mut peer, "acknowledged").await;
    let request_id = record["requestId"].as_str().unwrap().to_string();
    assert_eq!(accepted["result"]["requestId"], request_id);
    let (status, acknowledged) = send(
        &peer.router,
        "DELETE",
        &format!("/api/v2/requests/{request_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{acknowledged}");
    assert_eq!(acknowledged["scopes"], json!([GLOBAL]));
    let documents = peer.check().await;
    assert!(find(&documents, "acknowledged").is_none());
    assert_eq!(
        documents[GLOBAL]["requests"].as_object().unwrap().len(),
        0,
        "an acknowledgement creates no record"
    );
    let (status, again) = send(
        &peer.router,
        "DELETE",
        &format!("/api/v2/requests/{request_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{again}");
    let (status, resent) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("acknowledged"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        resent["result"],
        json!({"requestId": request_id, "disposition": "duplicate",
               "resultExpired": true, "status": "succeeded"})
    );
    peer.no_request().await;

    // Retained out.
    {
        let state = peer.state.upgrade().unwrap();
        state
            .lock()
            .await
            .sessions
            .commands
            .set_retention(0, Duration::ZERO);
    }
    let (_, accepted) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("retained"),
        Some(&tag),
        json!({"modeId": "ask"}),
    )
    .await;
    let request = peer.next_request().await;
    peer.reply(&request, json!({})).await;
    peer.until("the record is retained out", |documents| {
        session(documents)["operation"].is_null()
            && session(documents)["controls"]["modes"]["currentModeId"] == "ask"
            && find(documents, "retained").is_none()
    })
    .await;
    let (_, resent) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("retained"),
        Some(&tag),
        json!({"modeId": "ask"}),
    )
    .await;
    assert_eq!(
        resent["result"]["requestId"],
        accepted["result"]["requestId"]
    );
    assert_eq!(resent["result"]["resultExpired"], true);
    assert_eq!(resent["result"]["status"], "succeeded");
    peer.no_request().await;

    // A turn that is still running is found by its intent.
    let (turn, prompt) = start_turn(&mut peer, "running-turn").await;
    let operation_id = turn["result"]["operationId"].clone();
    let documents = peer.check().await;
    assert_eq!(
        session(&documents)["turn"]["clientIntentId"],
        "running-turn"
    );
    assert_eq!(session(&documents)["turn"]["operationId"], operation_id);
    let revision = session(&documents)["history"]["revision"].clone();
    let turn_tag = format!(
        "{}:{}\"",
        tag.trim_end_matches('"'),
        revision.as_str().unwrap()
    );
    let turns = format!("/api/v2/sessions/{SESSION}/turns");
    let body = json!({"prompt": [{"type": "text", "text": "Run it"}]});
    let (status, resent) = command(
        &peer,
        "POST",
        &turns,
        Some("running-turn"),
        Some(&turn_tag),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resent}");
    assert_eq!(
        resent["result"],
        json!({"operationId": operation_id, "disposition": "duplicate"})
    );
    peer.no_request().await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.until("the turn leaves an outcome", |documents| {
        session(documents)["turn"].is_null()
            && session(documents)["outcomes"]
                .as_object()
                .is_some_and(|outcomes| outcomes.len() == 1)
    })
    .await;

    // Ended with its scope: the outcome left with the session.
    let (status, _) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/close"),
        Some("close"),
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request = peer.next_request().await;
    peer.reply(&request, json!({})).await;
    peer.until("the closed session leaves", |documents| !held(documents))
        .await;
    let (status, resent) = command(
        &peer,
        "POST",
        &turns,
        Some("running-turn"),
        Some(&turn_tag),
        body,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resent}");
    assert_eq!(
        resent["result"],
        json!({"operationId": operation_id, "disposition": "duplicate",
               "resultExpired": true, "status": "completed"})
    );
    peer.no_request().await;
    peer.stop().await;
}

/// Success, lost response, record removed, table at budget, same-key resend
/// (protocol §5.2): no second dispatch, and new keys are refused.
async fn tiny_budget(
    peer: &mut Peer,
    path: &str,
    tag: Option<&str>,
    body: Value,
    method: &str,
    reply: Value,
) {
    let (status, accepted) = command(peer, "POST", path, Some("first"), tag, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], method);
    peer.reply(&request, reply).await;
    let record = finished(peer, "first").await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["result"]["kind"], "session");
    let request_id = record["requestId"].as_str().unwrap().to_string();
    let (status, _) = send(
        &peer.router,
        "DELETE",
        &format!("/api/v2/requests/{request_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        let used = state.sessions.commands.intent_bytes();
        state.sessions.commands.set_intent_budget(used);
    }
    let (status, resent) = command(peer, "POST", path, Some("first"), tag, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{resent}");
    assert_eq!(
        resent["result"],
        json!({"requestId": request_id, "disposition": "duplicate",
               "resultExpired": true, "status": "succeeded"})
    );
    let (status, refused) = command(peer, "POST", path, Some("second"), tag, body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refused}");
    assert_eq!(refused["error"]["code"], "intent_capacity");
    assert!(find(&peer.check().await, "second").is_none());
    peer.no_request().await;
}

#[tokio::test]
async fn a_tiny_intent_budget_answers_a_fork_from_its_tombstone() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"fork": {}}})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    tiny_budget(
        &mut peer,
        &format!("/api/v2/sessions/{SESSION}/fork"),
        Some(&tag),
        json!({}),
        "session/fork",
        json!({"sessionId": "forked"}),
    )
    .await;
    assert!(peer.check().await.contains_key(&session_scope("forked")));
    peer.stop().await;
}

#[tokio::test]
async fn a_tiny_intent_budget_answers_an_open_from_its_tombstone() {
    let mut peer = Peer::start(-1, json!({"loadSession": true})).await;
    tiny_budget(
        &mut peer,
        "/api/v2/sessions/cold/open",
        None,
        json!({"cwd": "/workspace"}),
        "session/load",
        json!({}),
    )
    .await;
    let documents = peer.check().await;
    assert_eq!(
        documents[&session_scope("cold")]["history"]["phase"],
        "ready"
    );
    peer.stop().await;
}

#[tokio::test]
async fn an_acknowledged_open_of_a_held_session_keeps_its_success_in_the_tombstone() {
    let mut peer = Peer::start(-1, json!({"loadSession": true})).await;
    create(&mut peer).await;
    let path = format!("/api/v2/sessions/{SESSION}/open");
    let body = json!({"cwd": "/workspace"});
    let (status, opened) =
        command(&peer, "POST", &path, Some("held-open"), None, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    let record = finished(&mut peer, "held-open").await;
    assert_eq!(record["status"], "succeeded");
    let request_id = record["requestId"].as_str().unwrap();
    let (status, acknowledged) = send(
        &peer.router,
        "DELETE",
        &format!("/api/v2/requests/{request_id}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{acknowledged}");
    assert!(find(&peer.check().await, "held-open").is_none());

    let (status, retried) = command(&peer, "POST", &path, Some("held-open"), None, body).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(
        retried["result"],
        json!({"requestId": request_id, "disposition": "duplicate",
               "resultExpired": true, "status": "succeeded"}),
        "acknowledgement cannot change a successful held-open result into a failure"
    );
    peer.no_request().await;
    peer.stop().await;
}

#[tokio::test]
async fn open_returns_the_owner_and_a_retry_never_materializes_again() {
    for (capabilities, method) in [
        (json!({"loadSession": true}), "session/load"),
        (
            json!({"sessionCapabilities": {"resume": {}}}),
            "session/resume",
        ),
    ] {
        open_once(capabilities, method).await;
    }
}

async fn open_once(capabilities: Value, method: &str) {
    let mut peer = Peer::start(-1, capabilities).await;
    let path = "/api/v2/sessions/cold/open";
    let (status, opened) = command(
        &peer,
        "POST",
        path,
        Some("open"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    let documents = peer.check().await;
    let record = find(&documents, "open").unwrap();
    assert_eq!(record["status"], "running");
    let incarnation = record["target"]["incarnation"].as_u64().unwrap();
    assert!(incarnation > 0, "open allocates the incarnation at once");
    assert_eq!(
        documents[&session_scope("cold")]["incarnation"],
        incarnation
    );
    let load = peer.next_request().await;
    assert_eq!(load["method"], method);
    let (_, retried) = command(
        &peer,
        "POST",
        path,
        Some("open"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(retried["result"]["disposition"], "duplicate");
    peer.reply(&load, json!({})).await;
    let record = finished(&mut peer, "open").await;
    assert_eq!(
        record["result"],
        json!({"kind": "session", "sessionId": "cold", "incarnation": incarnation})
    );
    // A new key on the held session returns the current owner at once.
    let (status, held_open) = command(
        &peer,
        "POST",
        path,
        Some("again"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let record = find(&peer.check().await, "again").unwrap();
    assert_eq!(record["requestId"], held_open["result"]["requestId"]);
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["result"]["incarnation"], incarnation);
    peer.no_request().await;
    peer.stop().await;
}

#[tokio::test]
async fn interaction_responses_report_delivery() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (_, prompt) = start_turn(&mut peer, "turn").await;

    // A permission response whose HTTP response is lost: delivered.
    let permission = request_permission(&mut peer, 901).await;
    let path = format!("/api/v2/interactions/{permission}");
    let (status, accepted) =
        command(&peer, "POST", &path, Some("allow"), Some(&tag), allow()).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let delivered = peer.next_request().await;
    assert_eq!(delivered["id"], 901);
    assert_eq!(delivered["result"]["outcome"]["optionId"], "allow");
    let record = finished(&mut peer, "allow").await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["operation"], "interaction_response");
    assert_eq!(record["interactionId"], permission);
    assert_eq!(record["params"], json!({"optionId": "allow"}));
    let (_, resent) = command(&peer, "POST", &path, Some("allow"), Some(&tag), allow()).await;
    assert_eq!(resent["result"]["disposition"], "duplicate");
    // Answered by another client first: refused, nothing recorded.
    let (status, late) = command(&peer, "POST", &path, Some("late"), Some(&tag), allow()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{late}");
    assert!(find(&peer.check().await, "late").is_none());

    // A form response: delivered.
    let before = session(&peer.check().await)["interactions"]
        .as_object()
        .unwrap()
        .len();
    peer.send(json!({
        "jsonrpc": "2.0", "id": 902, "method": "elicitation/create",
        "params": {
            "sessionId": SESSION, "mode": "form", "message": "Confirm",
            "requestedSchema": {"type": "object", "properties": {}},
        },
    }))
    .await;
    let documents = peer
        .until("the form is pending", |documents| {
            session(documents)["interactions"]
                .as_object()
                .unwrap()
                .len()
                > before
        })
        .await;
    let form = session(&documents)["interactions"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, interaction)| interaction["kind"] == "elicitation")
        .map(|(id, _)| id.clone())
        .unwrap();
    let answer = json!({"response": {"action": "accept", "content": {}}});
    let (status, accepted) = command(
        &peer,
        "POST",
        &format!("/api/v2/interactions/{form}"),
        Some("form"),
        Some(&tag),
        answer,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let delivered = peer.next_request().await;
    assert_eq!(delivered["id"], 902);
    assert_eq!(delivered["result"]["action"], "accept");
    let record = finished(&mut peer, "form").await;
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["params"], json!({"action": "accept"}));

    // Cancelled by the Agent before delivery: never reported as resolved.
    let cancelled = request_permission(&mut peer, 903).await;
    peer.send(json!({
        "jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": 903},
    }))
    .await;
    peer.until("the cancelled permission leaves", |documents| {
        session(documents)["interactions"].get(&cancelled).is_none()
    })
    .await;
    // A response the bridge already handed over counts as delivered; once
    // the cancellation is processed, the response is refused and recorded
    // nowhere, so it can never be reported as resolved.
    let (status, response) = command(
        &peer,
        "POST",
        &format!("/api/v2/interactions/{cancelled}"),
        Some("cancelled"),
        Some(&tag),
        allow(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response}");
    assert!(find(&peer.check().await, "cancelled").is_none());
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.stop().await;
}

#[tokio::test]
async fn a_fork_target_survives_until_its_delayed_subscription() {
    // An Agent without load or resume could never bring a retired fork back.
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"fork": {}}})).await;
    create(&mut peer).await;
    let epoch = epoch(&peer).await;
    let source_owner = owner(&mut peer, SESSION).await;
    let source = HttpStream::open(&peer.router, &[source_owner]).await;
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        state.observer_timeout = Some(0);
        state.first_leases.grace = Duration::from_millis(800);
        refresh_observer_timers(&mut state);
    }
    let tag = tag(&mut peer).await;
    let (status, forked) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/fork"),
        Some("fork"),
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forked}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/fork");
    peer.reply(&request, json!({"sessionId": "forked"})).await;
    let record = finished(&mut peer, "fork").await;
    assert_eq!(record["result"]["sessionId"], "forked");
    let incarnation = record["result"]["incarnation"].as_u64().unwrap();

    // The creator subscribes after a delay shorter than the grace period.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        peer.check().await.contains_key(&session_scope("forked")),
        "the lease kept the unobserved target"
    );
    let target =
        HttpStream::open(&peer.router, &[owner_scope("forked", &epoch, incarnation)]).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        peer.check().await.contains_key(&session_scope("forked")),
        "the stream took the lease over"
    );
    drop(target);
    peer.until("the unobserved target retires", |documents| {
        !documents.contains_key(&session_scope("forked"))
    })
    .await;
    assert!(peer.check().await.contains_key(&session_scope(SESSION)));
    drop(source);
    peer.stop().await;
}

#[tokio::test]
async fn a_command_never_advances_applied_past_a_pending_reset() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let owner = owner(&mut peer, SESSION).await;
    let mut stream = HttpStream::open(&peer.router, &[owner]).await;
    let (seq, documents) = peer.cut().await;
    assert!(stream.check(&peer.router, seq, &documents).await);

    // Applied 10: the replica applies one more entry, the chunk its paused
    // reader was already waiting for; everything later stays queued.
    stream.pause();
    peer.update(json!({"sessionUpdate": "session_info_update", "title": "Before"}))
        .await;
    let entry = stream.next().await;
    assert_eq!(entry["type"], "ops", "{entry}");
    let applied = stream.replica.applied();
    assert_eq!(applied, entry["at"]["seq"].as_u64().unwrap());
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock_without_commit().await;
        let epoch = state.sessions.epoch().to_string();
        state.replication.set_subscriber_budget(&epoch, 1);
    }

    // Commit 15: a command on the scope; reset@20 after more traffic.
    let tag = tag(&mut peer).await;
    let (status, accepted) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let at = accepted["at"]["seq"].as_u64().unwrap();
    assert!(at > applied);
    let request = peer.next_request().await;
    peer.reply(&request, json!({})).await;
    finished(&mut peer, "mode").await;
    peer.update(json!({"sessionUpdate": "session_info_update", "title": "After"}))
        .await;
    peer.until("the later title is committed", |documents| {
        session(documents)["controls"]["info"]["title"] == "After"
    })
    .await;

    stream.resume();
    while stream.replica.fetches().is_empty() {
        let entry = stream.next().await;
        assert_ne!(
            entry["type"], "ops",
            "the lagging queue was coalesced: {entry}"
        );
    }
    assert!(stream.replica.received >= at);
    assert_eq!(
        stream.replica.applied(),
        applied,
        "a reset never advances applied"
    );
    stream.fetch(&peer.router).await;
    assert!(stream.replica.applied() >= at);
    assert_eq!(
        stream.replica.documents()[&session_scope(SESSION)]["controls"]["modes"]["currentModeId"],
        "code"
    );
    peer.stop().await;
}

#[tokio::test]
async fn applied_waits_for_every_scope_of_a_new_stream() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let scopes = [GLOBAL.to_string(), owner(&mut peer, SESSION).await];
    let mut stream = HttpStream::open(&peer.router, &scopes).await;
    let first = stream.next().await;
    assert_eq!(
        (first["type"].as_str(), first["scope"].as_str()),
        (Some("snapshot"), Some(GLOBAL))
    );
    assert_eq!(stream.replica.applied(), 0);

    let tag = tag(&mut peer).await;
    let (_, accepted) = command(
        &peer,
        "PUT",
        &mode_path(),
        Some("mode"),
        Some(&tag),
        json!({"modeId": "code"}),
    )
    .await;
    let at = accepted["at"]["seq"].as_u64().unwrap();
    assert!(stream.pump(at).await);
    assert_eq!(
        stream.replica.applied(),
        0,
        "global's snapshot and the record do not advance applied while S is pending"
    );
    stream.fetch(&peer.router).await;
    assert!(stream.replica.applied() >= at);
    let documents = stream.replica.documents();
    assert!(
        find(&documents, "mode").is_some(),
        "the record reached the replica"
    );
    let request = peer.next_request().await;
    peer.reply(&request, json!({})).await;
    peer.stop().await;
}

#[tokio::test]
async fn a_turn_cancel_is_a_control_on_the_running_turn() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (turn, prompt) = start_turn(&mut peer, "turn").await;
    let operation_id = turn["result"]["operationId"].as_str().unwrap().to_string();
    let (status, stale) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/turns/other/cancel"),
        None,
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    let (status, cancelled) = command(
        &peer,
        "POST",
        &format!("/api/v2/sessions/{SESSION}/turns/{operation_id}/cancel"),
        None,
        Some(&tag),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["scopes"], json!([session_scope(SESSION)]));
    let cancel = peer.next_request().await;
    assert_eq!(cancel["method"], "session/cancel");
    let documents = peer
        .until("the turn shows the cancel request", |documents| {
            session(documents)["turn"]["cancelRequested"] == true
        })
        .await;
    assert_eq!(
        documents[GLOBAL]["requests"],
        json!({}),
        "a control action creates no record"
    );
    peer.reply(&prompt, json!({"stopReason": "cancelled"}))
        .await;
    peer.until("the cancelled turn ends", |documents| {
        session(documents)["turn"].is_null()
    })
    .await;
    peer.stop().await;
}

/// Plan §4.2: flow A completes, the Agent reuses its `elicitationId` for
/// flow B, and a late acknowledgement of A must not touch B.
#[tokio::test]
async fn url_flows_are_acknowledged_by_interaction_never_by_elicitation_id() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (_, prompt) = start_turn(&mut peer, "turn").await;
    let mut flows = Vec::new();
    for (id, key) in [(910, "accept-a"), (911, "accept-b")] {
        peer.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "elicitation/create",
            "params": {
                "sessionId": SESSION, "mode": "url", "elicitationId": "reused",
                "url": "https://example.test/sign-in", "message": "Sign in",
            },
        }))
        .await;
        let documents = peer
            .until("the URL elicitation is pending", |documents| {
                !session(documents)["interactions"]
                    .as_object()
                    .unwrap()
                    .is_empty()
            })
            .await;
        let interaction = session(&documents)["interactions"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let (status, accepted) = command(
            &peer,
            "POST",
            &format!("/api/v2/interactions/{interaction}"),
            Some(key),
            Some(&tag),
            json!({"response": {"action": "accept"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        let delivered = peer.next_request().await;
        assert_eq!(delivered["id"], id);
        peer.until("the flow waits", |documents| {
            session(documents)["urlFlows"][&interaction]["status"] == "waiting"
        })
        .await;
        let acknowledge = format!("/api/v2/sessions/{SESSION}/url-flows/{interaction}");
        let (status, waiting) = send(
            &peer.router,
            "DELETE",
            &acknowledge,
            &[("if-match", tag.clone())],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{waiting}");
        peer.send(json!({
            "jsonrpc": "2.0", "method": "elicitation/complete",
            "params": {"elicitationId": "reused"},
        }))
        .await;
        peer.until("the flow completes", |documents| {
            session(documents)["urlFlows"][&interaction]["status"] == "completed"
        })
        .await;
        flows.push((interaction, acknowledge));
    }
    let [(a, acknowledge_a), (b, acknowledge_b)] = <[_; 2]>::try_from(flows).unwrap();
    let (status, _) = send(
        &peer.router,
        "DELETE",
        &acknowledge_a,
        &[("if-match", tag.clone())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let documents = peer.check().await;
    assert!(session(&documents)["urlFlows"].get(&a).is_none());
    assert_eq!(session(&documents)["urlFlows"][&b]["status"], "completed");
    // A late second acknowledgement of A leaves B alone.
    let (status, late) = send(
        &peer.router,
        "DELETE",
        &acknowledge_a,
        &[("if-match", tag.clone())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{late}");
    assert_eq!(
        session(&peer.check().await)["urlFlows"][&b]["status"],
        "completed"
    );
    let (status, _) = send(
        &peer.router,
        "DELETE",
        &acknowledge_b,
        &[("if-match", tag.clone())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    peer.stop().await;
}

#[tokio::test]
async fn reads_answer_from_the_held_owner_and_never_load() {
    let mut peer = Peer::start(-1, json!({"loadSession": true})).await;
    let router = peer.router.clone();
    let get_status = |path: String| {
        let router = router.clone();
        async move { get(&router, &path).await }
    };
    // Nothing is held yet: every read is refused without a load.
    let (status, _) = get_status(format!(
        "/api/v2/sessions/{SESSION}/history/rev/turns/turn/process"
    ))
    .await;
    assert_eq!(status, StatusCode::GONE);
    peer.no_request().await;

    create(&mut peer).await;
    let tag = tag(&mut peer).await;
    let (_, prompt) = start_turn(&mut peer, "turn").await;
    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let documents = peer
        .until("the turn is committed", |documents| {
            session(documents)["turn"].is_null()
                && session(documents)["history"]["phase"] == "ready"
        })
        .await;
    let revision = session(&documents)["history"]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let process = |revision: &str, turn: &str| {
        format!("/api/v2/sessions/{SESSION}/history/{revision}/turns/{turn}/process")
    };
    let (status, missing) = get_status(process(&revision, "no-such-turn")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let (status, stale) = get_status(process("stale-revision", "no-such-turn")).await;
    assert_eq!(status, StatusCode::GONE, "{stale}");
    assert_eq!(stale["error"]["code"], "gone");
    if let Some(turn) = session(&documents)["history"]["collapsedTurns"]
        .as_array()
        .and_then(|turns| turns.first())
    {
        let (status, page) = get_status(process(&revision, turn["turnId"].as_str().unwrap())).await;
        assert_eq!(status, StatusCode::OK, "{page}");
    }

    let attachment = format!("/api/v2/sessions/{SESSION}/attachments/{}", "0".repeat(64));
    let (status, _) = send(&peer.router, "GET", &attachment, &[], None).await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _) = send(
        &peer.router,
        "GET",
        &attachment,
        &[("if-match", tag.clone())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &peer.router,
        "GET",
        &format!("/api/v2/sessions/{SESSION}/attachments/short"),
        &[("if-match", tag.clone())],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Workspace files need the host filesystem, which remote transports lack.
    let (status, files) = get_status(format!("/api/v2/sessions/{SESSION}/files?query=src")).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{files}");
    assert_eq!(files["error"]["code"], "unsupported");
    let (status, _) = get_status(format!("/api/v2/sessions/{SESSION}/files/content")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    peer.no_request().await;
    peer.stop().await;
}
