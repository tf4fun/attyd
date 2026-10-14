//! Protocol v2 global requests, auth flows and request-scoped interactions,
//! end to end over the real routes, coordinator and ACP transport (issue #24
//! acceptance, plan §4.2). Every check also runs the convergence gate, and
//! the critical points rebuild a replica from public state alone.
use std::collections::BTreeMap;

use axum::http::StatusCode;

use super::command_tests::{command, epoch, find, finished};
use super::convergence_tests::{Documents, Peer, SESSION, create, held};
use super::stream_client::{HttpStream, get, owner_scope, send};
use super::*;
use crate::protocol::{GLOBAL, session_scope};

mod review_auth;
mod review_parent_completion;
mod review_parents;

const AUTHENTICATE: &str = "/api/v2/agent/authenticate";
const LOGOUT: &str = "/api/v2/agent/logout";
const AUTH_TERMINALS: &str = "/api/v2/agent/auth-terminals";
const SESSIONS: &str = "/api/v2/sessions";

fn agent_login() -> Value {
    json!([{"id": "login", "name": "Login"}])
}

/// The `If-Match` of a request-scoped interaction (protocol §5.6).
async fn global_tag(peer: &Peer) -> String {
    format!("\"{}\"", epoch(peer).await)
}

fn records(documents: &Documents) -> &serde_json::Map<String, Value> {
    documents[GLOBAL]["requests"].as_object().unwrap()
}

/// The running record of an ACP request the bridge sent on its own.
fn agent_request<'a>(documents: &'a Documents, method: &str) -> Option<&'a Value> {
    records(documents)
        .values()
        .find(|request| request["operation"] == "agent_request" && request["method"] == method)
}

/// The one pending global interaction.
async fn global_interaction(peer: &mut Peer) -> (String, Value) {
    let documents = peer
        .until("a global interaction is pending", |documents| {
            !documents[GLOBAL]["interactions"]
                .as_object()
                .unwrap()
                .is_empty()
        })
        .await;
    let (id, interaction) = documents[GLOBAL]["interactions"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    (id.clone(), interaction.clone())
}

/// A request-scoped elicitation naming the bridge's JSON-RPC request `parent`.
fn scoped_elicitation(id: u64, parent: &Value, mode: Value) -> Value {
    let mut params = json!({"requestId": parent});
    params
        .as_object_mut()
        .unwrap()
        .extend(mode.as_object().unwrap().clone());
    json!({"jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": params})
}

fn url_mode(elicitation_id: &str) -> Value {
    json!({
        "mode": "url", "elicitationId": elicitation_id,
        "url": "https://example.test/sign-in", "message": "Sign in",
    })
}

fn form_mode() -> Value {
    json!({
        "mode": "form", "message": "Pick a value",
        "requestedSchema": {"type": "object", "properties": {"value": {"type": "string"}}},
    })
}

/// Answers an interaction through `POST /interactions` and returns what the
/// Agent received.
async fn answer(peer: &mut Peer, interaction: &str, key: &str, response: Value) -> Value {
    let tag = global_tag(peer).await;
    let (status, answered) = command(
        peer,
        "POST",
        &format!("/api/v2/interactions/{interaction}"),
        Some(key),
        Some(&tag),
        json!({"response": response}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answered}");
    peer.next_request().await
}

/// Starts `POST /agent/authenticate` and returns the record ID and the
/// Agent's `authenticate` request.
async fn authenticate(peer: &mut Peer, key: &str) -> (Value, Value) {
    let (status, accepted) = command(
        peer,
        "POST",
        AUTHENTICATE,
        Some(key),
        None,
        json!({"methodId": "login"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["result"]["disposition"], "accepted");
    assert_eq!(accepted["scopes"], json!([GLOBAL]));
    let request = peer.next_request().await;
    assert_eq!(request["method"], "authenticate");
    assert_eq!(request["params"]["methodId"], "login");
    (accepted["result"]["requestId"].clone(), request)
}

#[tokio::test]
async fn global_url_authentication_survives_a_refresh_then_completes() {
    let mut peer = Peer::start_with(-1, json!({}), agent_login(), |_| {}).await;
    let (request_id, authenticate) = authenticate(&mut peer, "sign-in").await;
    let record = find(&peer.check().await, "sign-in").unwrap();
    assert_eq!(record["requestId"], request_id);
    assert_eq!(record["operation"], "authenticate");
    assert_eq!(record["method"], "authenticate");
    assert_eq!(record["methodId"], "login");
    assert_eq!(record["status"], "running");
    assert_eq!(record["target"], Value::Null);

    peer.send(scoped_elicitation(
        700,
        &authenticate["id"],
        url_mode("sign-in-flow"),
    ))
    .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], request_id, "{pending}");
    let delivered = answer(
        &mut peer,
        &interaction,
        "accept-url",
        json!({"action": "accept"}),
    )
    .await;
    assert_eq!(delivered["id"], 700);
    assert_eq!(delivered["result"]["action"], "accept");

    // A page that refreshes after accepting rebuilds the flow and its parent
    // from public state alone.
    peer.streams
        .push(HttpStream::open(&peer.router, &[GLOBAL.to_string()]).await);
    let (status, snapshot) = get(&peer.router, "/api/v2/global").await;
    assert_eq!(status, StatusCode::OK);
    let flow = &snapshot["state"]["urlFlows"][&interaction];
    assert_eq!(flow["status"], "waiting", "{snapshot}");
    assert_eq!(flow["requestId"], request_id);
    assert_eq!(flow["elicitationId"], "sign-in-flow");
    assert_eq!(
        snapshot["state"]["requests"][request_id.as_str().unwrap()]["status"],
        "running"
    );
    assert!(
        snapshot["state"]["interactions"]
            .as_object()
            .unwrap()
            .is_empty()
    );

    peer.send(json!({
        "jsonrpc": "2.0", "method": "elicitation/complete",
        "params": {"elicitationId": "sign-in-flow"},
    }))
    .await;
    peer.until("the flow completes", |documents| {
        documents[GLOBAL]["urlFlows"][&interaction]["status"] == "completed"
    })
    .await;
    peer.reply(&authenticate, json!({})).await;
    let record = finished(&mut peer, "sign-in").await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["result"], json!({"kind": "done"}));
    assert_eq!(record["methodId"], "login");
    let documents = peer
        .until("the method is authenticated", |documents| {
            documents[GLOBAL]["agent"]["authenticatedMethodId"] == "login"
        })
        .await;
    // The flow outlives its parent request until it is acknowledged.
    assert_eq!(
        documents[GLOBAL]["urlFlows"][&interaction]["status"],
        "completed"
    );
    let (status, acknowledged) = send(
        &peer.router,
        "DELETE",
        &format!("/api/v2/url-flows/{interaction}"),
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{acknowledged}");
    assert_eq!(acknowledged["scopes"], json!([GLOBAL]));
    assert!(
        peer.check().await[GLOBAL]["urlFlows"]
            .get(&interaction)
            .is_none()
    );
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn a_cancelled_global_url_authentication_ends_its_interaction_and_fails_its_parent() {
    let mut peer = Peer::start_with(-1, json!({}), agent_login(), |_| {}).await;
    let (request_id, authenticate) = authenticate(&mut peer, "cancelled-sign-in").await;
    peer.send(scoped_elicitation(
        701,
        &authenticate["id"],
        url_mode("cancelled-flow"),
    ))
    .await;
    let (_, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], request_id);
    peer.send(json!({
        "jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": 701},
    }))
    .await;
    peer.until("the cancelled interaction leaves", |documents| {
        documents[GLOBAL]["interactions"]
            .as_object()
            .unwrap()
            .is_empty()
    })
    .await;
    peer.send(json!({
        "jsonrpc": "2.0", "id": authenticate["id"],
        "error": {"code": -32000, "message": "sign-in cancelled"},
    }))
    .await;
    let record = finished(&mut peer, "cancelled-sign-in").await;
    assert_eq!(record["status"], "failed", "{record}");
    assert_eq!(record["error"]["code"], "agent_error");
    assert_eq!(record["error"]["data"]["message"], "sign-in cancelled");
    let documents = peer.check().await;
    assert!(
        documents[GLOBAL]["urlFlows"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        documents[GLOBAL]["agent"]["authenticatedMethodId"],
        Value::Null
    );
    peer.settle().await;
    peer.stop().await;
}

/// Plan §4.2: global flow A completes, the Agent reuses its `elicitationId`
/// for flow B, and a late acknowledgement of A must not touch B.
#[tokio::test]
async fn a_late_global_acknowledgement_never_touches_a_flow_reusing_its_elicitation_id() {
    let mut peer = Peer::start_with(-1, json!({}), agent_login(), |_| {}).await;
    let (request_id, authenticate) = authenticate(&mut peer, "reused-sign-in").await;
    let mut flows = Vec::new();
    for (id, key) in [(710, "accept-a"), (711, "accept-b")] {
        peer.send(scoped_elicitation(
            id,
            &authenticate["id"],
            url_mode("reused"),
        ))
        .await;
        let (interaction, _) = global_interaction(&mut peer).await;
        let delivered = answer(&mut peer, &interaction, key, json!({"action": "accept"})).await;
        assert_eq!(delivered["id"], id);
        peer.until("the flow waits", |documents| {
            documents[GLOBAL]["urlFlows"][&interaction]["status"] == "waiting"
        })
        .await;
        flows.push(interaction);
        if flows.len() == 1 {
            let path = format!("/api/v2/url-flows/{}", flows[0]);
            let (status, waiting) = send(&peer.router, "DELETE", &path, &[], None).await;
            assert_eq!(status, StatusCode::CONFLICT, "{waiting}");
            peer.send(json!({
                "jsonrpc": "2.0", "method": "elicitation/complete",
                "params": {"elicitationId": "reused"},
            }))
            .await;
            peer.until("flow A completes", |documents| {
                documents[GLOBAL]["urlFlows"][&flows[0]]["status"] == "completed"
            })
            .await;
        }
    }
    let [a, b] = <[_; 2]>::try_from(flows).unwrap();
    let acknowledge_a = format!("/api/v2/url-flows/{a}");
    let (status, _) = send(&peer.router, "DELETE", &acknowledge_a, &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let documents = peer.check().await;
    assert!(documents[GLOBAL]["urlFlows"].get(&a).is_none());
    assert_eq!(documents[GLOBAL]["urlFlows"][&b]["status"], "waiting");
    assert_eq!(documents[GLOBAL]["urlFlows"][&b]["requestId"], request_id);
    let (status, late) = send(&peer.router, "DELETE", &acknowledge_a, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{late}");
    assert_eq!(late["error"]["code"], "not_found");
    // B is still the one waiting flow for the reused ID.
    peer.send(json!({
        "jsonrpc": "2.0", "method": "elicitation/complete",
        "params": {"elicitationId": "reused"},
    }))
    .await;
    peer.until("flow B completes", |documents| {
        documents[GLOBAL]["urlFlows"][&b]["status"] == "completed"
    })
    .await;
    peer.reply(&authenticate, json!({})).await;
    finished(&mut peer, "reused-sign-in").await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn request_scoped_elicitations_during_create_and_a_catalog_read_have_visible_parents() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"list": {}}})).await;
    let (status, accepted) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("create-parent"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["scopes"], json!([GLOBAL]));
    let new = peer.next_request().await;
    assert_eq!(new["method"], "session/new");
    assert_eq!(new["params"]["cwd"], "/workspace");
    let record = find(&peer.check().await, "create-parent").unwrap();
    assert_eq!(record["operation"], "create");
    assert_eq!(record["method"], "session/new");
    assert_eq!(record["params"], json!({"cwd": "/workspace"}));
    assert_eq!(record["status"], "running");

    peer.send(scoped_elicitation(720, &new["id"], form_mode()))
        .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], accepted["result"]["requestId"]);
    // A fresh replica finds the elicitation and its running parent.
    peer.streams
        .push(HttpStream::open(&peer.router, &[GLOBAL.to_string()]).await);
    let delivered = answer(
        &mut peer,
        &interaction,
        "create-form",
        json!({"action": "accept", "content": {"value": "chosen"}}),
    )
    .await;
    assert_eq!(delivered["id"], 720);
    peer.reply(&new, json!({"sessionId": SESSION})).await;
    let record = finished(&mut peer, "create-parent").await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["result"]["kind"], "session");
    assert_eq!(record["result"]["sessionId"], SESSION);
    assert!(record["result"]["incarnation"].as_u64().unwrap() > 0);
    assert!(held(&peer.check().await));

    let router = peer.router.clone();
    let read = tokio::spawn(async move { get(&router, "/api/v2/catalog").await });
    let list = peer.next_request().await;
    assert_eq!(list["method"], "session/list");
    let documents = peer
        .until("the catalog read has a record", |documents| {
            agent_request(documents, "session/list").is_some()
        })
        .await;
    let parent = agent_request(&documents, "session/list").unwrap().clone();
    assert_eq!(parent["clientIntentId"], Value::Null);
    assert_eq!(parent["status"], "running");
    peer.send(scoped_elicitation(721, &list["id"], form_mode()))
        .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], parent["requestId"]);
    let delivered = answer(
        &mut peer,
        &interaction,
        "catalog-form",
        json!({"action": "decline"}),
    )
    .await;
    assert_eq!(delivered["id"], 721);
    peer.reply(
        &list,
        json!({"sessions": [{"sessionId": "listed", "cwd": "/repo"}]}),
    )
    .await;
    let (status, page) = read.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["sessions"][0]["sessionId"], "listed");
    let documents = peer
        .until("the read leaves no record behind", |documents| {
            agent_request(documents, "session/list").is_none()
        })
        .await;
    assert_eq!(page["revision"], documents[GLOBAL]["catalog"]["revision"]);
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn a_stale_catalog_revision_is_refused_without_listing() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"list": {}}})).await;
    let revision = peer.check().await[GLOBAL]["catalog"]["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, stale) = get(&peer.router, "/api/v2/catalog?revision=stale").await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["error"]["code"], "catalog_changed");
    peer.no_request().await;
    assert!(records(&peer.check().await).is_empty());

    let router = peer.router.clone();
    let path = format!("/api/v2/catalog?revision={revision}&cursor=next-page");
    let read = tokio::spawn(async move { get(&router, &path).await });
    let list = peer.next_request().await;
    assert_eq!(list["method"], "session/list");
    assert_eq!(list["params"]["cursor"], "next-page");
    peer.reply(&list, json!({"sessions": [], "nextCursor": "after"}))
        .await;
    let (status, page) = read.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["revision"], revision);
    assert_eq!(page["nextCursor"], "after");

    let mut unlisted = Peer::start(-1, json!({})).await;
    let (status, unsupported) = get(&unlisted.router, "/api/v2/catalog").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{unsupported}");
    assert_eq!(unsupported["error"]["code"], "unsupported");
    unlisted.no_request().await;
    unlisted.stop().await;
    peer.stop().await;
}

/// A v1 client's out-of-session request is still an ACP request the bridge
/// sends, so it gets an `agent_request` record that can parent an
/// elicitation.
#[tokio::test]
async fn out_of_session_v1_requests_get_agent_request_records() {
    let mut peer = Peer::start_with(-1, json!({}), agent_login(), |_| {}).await;
    let authenticated = peer.submit(json!({
        "type": "auth/authenticate", "requestId": "v1-sign-in", "methodId": "login",
    }));
    let request = peer.next_request().await;
    assert_eq!(request["method"], "authenticate");
    let documents = peer
        .until("the v1 request has a record", |documents| {
            agent_request(documents, "authenticate").is_some()
        })
        .await;
    let record = agent_request(&documents, "authenticate").unwrap().clone();
    assert_eq!(record["clientIntentId"], Value::Null);
    assert_eq!(record["status"], "running");
    peer.send(scoped_elicitation(730, &request["id"], form_mode()))
        .await;
    let (interaction, pending) = global_interaction(&mut peer).await;
    assert_eq!(pending["requestId"], record["requestId"]);
    // The parent ends before the user answers: its elicitation goes with it.
    peer.reply(&request, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), authenticated)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let cancelled = peer.next_request().await;
    assert_eq!(cancelled["id"], 730);
    assert_eq!(cancelled["result"]["action"], "cancel");
    let documents = peer
        .until("the parent finishes", |documents| {
            records(documents)[record["requestId"].as_str().unwrap()]["status"] == "succeeded"
        })
        .await;
    assert!(
        documents[GLOBAL]["interactions"]
            .get(&interaction)
            .is_none()
    );
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn acknowledging_a_finished_request_creates_no_record() {
    let mut peer =
        Peer::start_with(-1, json!({"auth": {"logout": {}}}), agent_login(), |_| {}).await;
    let (request_id, authenticate) = authenticate(&mut peer, "acknowledged-sign-in").await;
    let path = format!("/api/v2/requests/{}", request_id.as_str().unwrap());
    let (status, running) = send(&peer.router, "DELETE", &path, &[], None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{running}");
    peer.reply(&authenticate, json!({})).await;
    finished(&mut peer, "acknowledged-sign-in").await;
    let (status, acknowledged) = send(&peer.router, "DELETE", &path, &[], None).await;
    assert_eq!(status, StatusCode::OK, "{acknowledged}");
    assert!(records(&peer.check().await).is_empty());
    let (status, again) = send(&peer.router, "DELETE", &path, &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{again}");
    assert!(
        records(&peer.check().await).is_empty(),
        "an acknowledgement creates no record"
    );

    // Logout is an operation command with its own record.
    let (status, logout) = command(&peer, "POST", LOGOUT, Some("logout"), None, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{logout}");
    let request = peer.next_request().await;
    assert_eq!(request["method"], "logout");
    peer.reply(&request, json!({})).await;
    let record = finished(&mut peer, "logout").await;
    assert_eq!(record["operation"], "logout");
    assert_eq!(record["status"], "succeeded");
    peer.until("the method is no longer authenticated", |documents| {
        documents[GLOBAL]["agent"]["authenticatedMethodId"].is_null()
    })
    .await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn authentication_admission_refuses_unknown_and_terminal_methods() {
    let terminal = json!([
        {"id": "login", "name": "Login"},
        {"id": "terminal", "name": "Terminal", "type": "terminal", "args": []},
    ]);
    let mut peer = Peer::start_with(-1, json!({}), terminal, |options| {
        options.transport = crate::options::Transport::Stdio;
    })
    .await;
    let (status, missing) = command(
        &peer,
        "POST",
        AUTHENTICATE,
        None,
        None,
        json!({"methodId": "login"}),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{missing}");
    for (key, method) in [("unknown", "unknown"), ("terminal", "terminal")] {
        let (status, refused) = command(
            &peer,
            "POST",
            AUTHENTICATE,
            Some(key),
            None,
            json!({"methodId": method}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method}: {refused}");
        assert_eq!(refused["error"]["code"], "invalid_request");
    }
    let (status, refused) = command(
        &peer,
        "POST",
        AUTH_TERMINALS,
        Some("not-terminal"),
        None,
        json!({"methodId": "login", "cols": 80, "rows": 24}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    // Logout needs the Agent's capability.
    let (status, unsupported) =
        command(&peer, "POST", LOGOUT, Some("logout"), None, json!({})).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{unsupported}");
    peer.no_request().await;
    assert!(records(&peer.check().await).is_empty());
    peer.stop().await;
}

/// The records two pages created in one directory, keyed by intent.
async fn created(peer: &mut Peer, key: &str) -> Value {
    let record = finished(peer, key).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["result"]["kind"], "session");
    record
}

#[tokio::test]
async fn two_pages_creating_in_one_directory_each_find_their_own_session() {
    let mut peer = Peer::start(-1, json!({})).await;
    let mut accepted = BTreeMap::new();
    for key in ["page-a", "page-b"] {
        let (status, response) = command(
            &peer,
            "POST",
            SESSIONS,
            Some(key),
            None,
            json!({"cwd": "/repo"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        accepted.insert(key, response["result"]["requestId"].clone());
    }
    for session in ["created-a", "created-b"] {
        let new = peer.next_request().await;
        assert_eq!(new["method"], "session/new");
        peer.reply(&new, json!({"sessionId": session})).await;
    }
    let a = created(&mut peer, "page-a").await;
    let b = created(&mut peer, "page-b").await;
    assert_eq!(a["requestId"], accepted["page-a"]);
    assert_eq!(b["requestId"], accepted["page-b"]);
    assert_ne!(a["result"]["sessionId"], b["result"]["sessionId"]);
    let documents = peer.check().await;
    for record in [&a, &b] {
        let session_id = record["result"]["sessionId"].as_str().unwrap();
        assert_eq!(
            documents[&session_scope(session_id)]["incarnation"],
            record["result"]["incarnation"]
        );
    }

    // Page A lost its response: the same key answers from its record and
    // creates nothing.
    let (status, retried) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("page-a"),
        None,
        json!({"cwd": "/repo"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(retried["result"]["requestId"], accepted["page-a"]);
    let (status, conflict) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("page-a"),
        None,
        json!({"cwd": "/other"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "intent_conflict");
    peer.no_request().await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn a_full_intent_table_never_dispatches_a_second_session_new() {
    let mut peer = Peer::start(-1, json!({})).await;
    let (status, accepted) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("budget-create"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let new = peer.next_request().await;
    peer.reply(&new, json!({"sessionId": SESSION})).await;
    let record = created(&mut peer, "budget-create").await;
    let path = format!("/api/v2/requests/{}", record["requestId"].as_str().unwrap());
    let (status, _) = send(&peer.router, "DELETE", &path, &[], None).await;
    assert_eq!(status, StatusCode::OK);
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock().await;
        let used = state.sessions.commands.intent_bytes();
        state.sessions.commands.set_intent_budget(used);
    }

    // The lost response is answered from the tombstone; nothing runs again.
    let (status, retried) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("budget-create"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    assert_eq!(retried["result"]["disposition"], "duplicate");
    assert_eq!(retried["result"]["resultExpired"], true);
    assert_eq!(retried["result"]["status"], "succeeded");
    assert_eq!(retried["result"]["requestId"], record["requestId"]);
    let (status, full) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("budget-second"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{full}");
    assert_eq!(full["error"]["code"], "intent_capacity");
    peer.no_request().await;
    assert!(find(&peer.check().await, "budget-second").is_none());
    peer.stop().await;
}

#[tokio::test]
async fn a_created_session_waits_for_a_delayed_first_subscription_at_timeout_zero() {
    // An Agent without load or resume: a retired session cannot come back.
    let mut peer = Peer::start(0, json!({})).await;
    let epoch = epoch(&peer).await;
    let (status, accepted) = command(
        &peer,
        "POST",
        SESSIONS,
        Some("unobserved-create"),
        None,
        json!({"cwd": "/workspace"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    let new = peer.next_request().await;
    peer.reply(&new, json!({"sessionId": SESSION})).await;
    let record = created(&mut peer, "unobserved-create").await;
    let incarnation = record["result"]["incarnation"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        held(&peer.check().await),
        "the first-subscription lease keeps the created owner"
    );
    let stream = HttpStream::open(&peer.router, &[owner_scope(SESSION, &epoch, incarnation)]).await;
    peer.no_request().await;
    assert!(held(&peer.check().await));
    // The stream took the lease over: once it leaves, retirement proceeds.
    drop(stream);
    peer.until("the unobserved session retires", |documents| {
        !held(documents)
    })
    .await;
    peer.stop().await;
}

#[tokio::test]
async fn cold_delete_never_loads_and_refuses_a_held_session() {
    let mut peer = Peer::start(-1, json!({"sessionCapabilities": {"delete": {}}})).await;
    let epoch = epoch(&peer).await;
    let cold = format!("\"{epoch}:0\"");
    let path = format!("{SESSIONS}/cold-session");
    let (status, missing) = command(&peer, "DELETE", &path, None, Some(&cold), json!({})).await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED, "{missing}");
    let (status, accepted) = command(
        &peer,
        "DELETE",
        &path,
        Some("cold-delete"),
        Some(&cold),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["scopes"], json!([GLOBAL]));
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/delete");
    assert_eq!(request["params"]["sessionId"], "cold-session");
    let record = find(&peer.check().await, "cold-delete").unwrap();
    assert_eq!(record["operation"], "delete");
    assert_eq!(
        record["target"],
        json!({"sessionId": "cold-session", "incarnation": null})
    );
    peer.reply(&request, json!({})).await;
    let record = finished(&mut peer, "cold-delete").await;
    assert_eq!(record["status"], "succeeded", "{record}");
    peer.no_request().await;

    // `"<epoch>:0"` says the bridge holds no incarnation: a held session
    // fails the tag and nothing reaches the Agent.
    create(&mut peer).await;
    let (status, held_session) = command(
        &peer,
        "DELETE",
        &format!("{SESSIONS}/{SESSION}"),
        Some("cold-held"),
        Some(&cold),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{held_session}");
    assert_eq!(held_session["error"]["code"], "owner_mismatch");
    peer.no_request().await;
    assert!(find(&peer.check().await, "cold-held").is_none());
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn cold_delete_needs_the_agent_capability() {
    let mut peer = Peer::start(-1, json!({})).await;
    let cold = format!("\"{}:0\"", epoch(&peer).await);
    let (status, unsupported) = command(
        &peer,
        "DELETE",
        &format!("{SESSIONS}/cold-session"),
        Some("cold-unsupported"),
        Some(&cold),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{unsupported}");
    peer.no_request().await;
    peer.stop().await;
}

#[cfg(unix)]
mod auth_terminal {
    use super::*;

    /// A bridge whose Agent command is `/bin/sh`, so a terminal method's
    /// arguments are a shell script.
    async fn terminal_peer() -> Peer {
        let methods = json!([
            {"id": "passing", "name": "Passing", "type": "terminal", "args": ["-c", "exit 0"]},
            {
                "id": "failing", "name": "Failing", "type": "terminal",
                "args": ["-c", "printf 'login> '; read answer; exit 3"],
            },
        ]);
        Peer::start_with(-1, json!({}), methods, |options| {
            options.transport = crate::options::Transport::Stdio;
            options.command = vec!["/bin/sh".to_string()];
        })
        .await
    }

    fn controls(request_id: &str, control: &str) -> String {
        format!("{AUTH_TERMINALS}/{request_id}/{control}")
    }

    async fn start(peer: &Peer, key: &str, method_id: &str) -> String {
        let (status, accepted) = command(
            peer,
            "POST",
            AUTH_TERMINALS,
            Some(key),
            None,
            json!({"methodId": method_id, "cols": 80, "rows": 24}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        assert_eq!(accepted["scopes"], json!([GLOBAL]));
        accepted["result"]["requestId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn an_auth_terminal_keeps_its_method_from_running_to_failed() {
        let mut peer = terminal_peer().await;
        let request_id = start(&peer, "failing-terminal", "failing").await;
        let documents = peer
            .until("the terminal prompts", |documents| {
                records(documents)[&request_id]["terminal"]["output"]
                    .as_str()
                    .is_some_and(|output| output.contains("login> "))
            })
            .await;
        let record = &records(&documents)[&request_id];
        assert_eq!(record["operation"], "auth_terminal");
        assert_eq!(record["method"], "attyd/auth_terminal");
        assert_eq!(record["methodId"], "failing");
        assert_eq!(record["status"], "running");
        assert_eq!(record["terminal"]["exitCode"], Value::Null);

        // A page rebuilt from public state knows which of the two methods
        // is running.
        peer.streams
            .push(HttpStream::open(&peer.router, &[GLOBAL.to_string()]).await);
        let (_, snapshot) = get(&peer.router, "/api/v2/global").await;
        assert_eq!(
            snapshot["state"]["requests"][&request_id]["methodId"],
            "failing"
        );
        let before = records(&peer.check().await).len();
        for (control, body) in [
            ("resize", json!({"cols": 100, "rows": 30})),
            ("input", json!({"data": "wrong\n"})),
        ] {
            let (status, answered) = command(
                &peer,
                "POST",
                &controls(&request_id, control),
                None,
                None,
                body,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{control}: {answered}");
            assert_eq!(answered["result"], json!({}));
        }
        let record = finished(&mut peer, "failing-terminal").await;
        assert_eq!(record["status"], "failed", "{record}");
        assert_eq!(record["methodId"], "failing");
        assert_eq!(record["terminal"]["exitCode"], 3);
        assert!(record["error"]["message"].as_str().unwrap().contains('3'));
        assert_eq!(
            records(&peer.check().await).len(),
            before,
            "control actions create no records"
        );
        let (status, ended) = command(
            &peer,
            "POST",
            &controls(&request_id, "input"),
            None,
            None,
            json!({"data": "late\n"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{ended}");
        peer.settle().await;
        peer.stop().await;
    }

    #[tokio::test]
    async fn cancelling_an_auth_terminal_ends_its_record_cancelled() {
        let mut peer = terminal_peer().await;
        let (status, unknown) = command(
            &peer,
            "POST",
            &controls("unknown", "cancel"),
            None,
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{unknown}");
        let request_id = start(&peer, "cancelled-terminal", "failing").await;
        let (status, busy) = command(
            &peer,
            "POST",
            AUTH_TERMINALS,
            Some("second-terminal"),
            None,
            json!({"methodId": "passing", "cols": 80, "rows": 24}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{busy}");
        let (status, cancelled) = command(
            &peer,
            "POST",
            &controls(&request_id, "cancel"),
            None,
            None,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        let record = finished(&mut peer, "cancelled-terminal").await;
        assert_eq!(record["status"], "cancelled", "{record}");
        assert_eq!(record["methodId"], "failing");
        assert!(find(&peer.check().await, "second-terminal").is_none());
        peer.settle().await;
        peer.stop().await;
    }
}
