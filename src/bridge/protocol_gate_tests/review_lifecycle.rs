//! Regression constraints from the independent PR #42 lifecycle review.
use super::*;

#[tokio::test]
async fn review_request_scoped_session_flow_is_removed_when_its_scope_is_deleted() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"delete": {}}})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
    peer.agent_request(
        "sign-in",
        "elicitation/create",
        url_elicitation(json!({"requestId": request["id"]}), "flow"),
    )
    .await;
    peer.until("the prompt receives the interaction", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live(&scope)).remove(0);
    peer.answer(
        &scope,
        &interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
    assert_eq!(peer.response("sign-in").await["result"]["action"], "accept");
    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the prompt completes and leaves its flow", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
            && peer.live(&scope)["urlFlows"].get(&interaction).is_some()
    })
    .await;

    peer.start_op("global", "delete", json!({"sessionId": CREATED}))
        .await;
    let request = peer.request("session/delete").await;
    peer.reply(&request, json!({})).await;
    peer.until("the delete completes", |peer| {
        peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    peer.settled().await;

    {
        let lock = peer.lock();
        let state = lock.lock().await;
        assert!(
            !state.sessions.request_url_flows.contains_key("flow"),
            "a deleted scope must release its request-scoped URL flow"
        );
        assert!(
            !state.sessions.url_owners.contains_key("flow"),
            "a deleted scope must release the Agent's elicitationId"
        );
    }

    // The Agent can now reuse the elicitationId for an unrelated operation.
    // This is externally visible: the new request must be answerable instead
    // of being refused as an already-outstanding URL elicitation.
    let (_, creating) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
    let new_request = peer.request("session/new").await;
    peer.agent_request(
        "new-sign-in",
        "elicitation/create",
        url_elicitation(json!({"requestId": new_request["id"]}), "flow"),
    )
    .await;
    peer.until(
        "the reused URL elicitationId is answerable on its new operation",
        |peer| interaction_ids(&peer.live("global")).len() == 1,
    )
    .await;
    let replacement = interaction_ids(&peer.live("global")).remove(0);
    peer.answer(
        "global",
        &replacement,
        json!({"response": {"action": "accept"}}),
    )
    .await;
    assert_eq!(
        peer.response("new-sign-in").await["result"]["action"],
        "accept"
    );
    peer.reply(&new_request, json!({"sessionId": "other"}))
        .await;
    peer.ended(creating).await.unwrap();
    peer.until("the replacement flow belongs to global", |peer| {
        peer.live("global")["urlFlows"].get(&replacement).is_some()
            && peer.live("global")["op"]["status"] == "completed"
    })
    .await;
    let lock = peer.lock();
    let state = lock.lock().await;
    assert_eq!(
        state.sessions.request_url_flows["flow"].registration_id,
        replacement
    );
    assert_ne!(
        state.sessions.url_owners["flow"].registration_id,
        interaction
    );
    assert!(!state.sessions.scopes.flow_scopes.contains_key(&interaction));
}

/// A permission request's `toolCall` is a `ToolCallUpdate` reduced into the session's entries
/// like one (memory model §3.2), also with no tool notification before it: the tool card exists
/// while the permission waits, a later notification patches the same entry, and the answer
/// leaves it.
#[tokio::test]
async fn a_permission_requests_tool_call_is_an_entry_before_any_tool_notification() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    assert!(peer.entries(&scope).is_empty());

    peer.agent_request("p", "session/request_permission", permission(CREATED))
        .await;
    peer.until("the permission waits and its tool call is an entry", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1 && peer.entries(&scope).len() == 1
    })
    .await;
    let entry = peer.entries(&scope).remove(0);
    assert_eq!(entry["call"]["toolCallId"], "tool", "{entry}");
    assert_eq!(entry["call"]["title"], "Confirm", "{entry}");

    peer.update(
        CREATED,
        json!({"sessionUpdate": "tool_call_update", "toolCallId": "tool", "status": "in_progress"}),
    )
    .await;
    peer.until("the notification patches the same entry", |peer| {
        peer.entries(&scope).first().is_some_and(|entry| entry["call"]["status"] == "in_progress")
    })
    .await;
    assert_eq!(peer.entries(&scope).len(), 1);

    let interaction = interaction_ids(&peer.live(&scope)).remove(0);
    peer.answer(
        &scope,
        &interaction,
        json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
    )
    .await;
    peer.response("p").await;
    peer.settled().await;
    assert_eq!(peer.entries(&scope).len(), 1, "the answer leaves the entry");
}

/// A request-scoped interaction lives until the response of the request it names (memory model
/// §8.2), whether or not an operation runs: a control action installs none, and the interaction
/// its `session/set_mode` request raises is not retired for lack of a running operation.
#[tokio::test]
async fn an_interaction_of_a_control_request_lives_until_that_requests_response() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(
        CREATED,
        json!({"modes": {"currentModeId": "ask", "availableModes": [
            {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
        ]}}),
    )
    .await;
    let scope = format!("session/{CREATED}");
    let answer = peer
        .control(
            CREATED,
            crate::bridge::controls::Control::SetMode { mode_id: "plan".into() },
        )
        .await;
    let request = peer.request("session/set_mode").await;
    peer.agent_request(
        "q",
        "elicitation/create",
        form(json!({"requestId": request["id"]})),
    )
    .await;
    peer.until("the control's interaction is pending", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    peer.settled().await;
    assert_eq!(
        interaction_ids(&peer.live(&scope)).len(),
        1,
        "no running operation ends it: only the control's response does"
    );

    peer.reply(&request, json!({})).await;
    answer.await.unwrap().unwrap();
    peer.until("the response ends the interaction", |peer| {
        interaction_ids(&peer.live(&scope)).is_empty()
    })
    .await;
    assert_eq!(peer.response("q").await["result"]["action"], "cancel");
    peer.settled().await;
}

/// A request-scoped elicitation names an outstanding request of the bridge (memory model §8.2,
/// Q6): one that names none, or one that is no longer outstanding, is refused with
/// `invalid_params` and never becomes an interaction.
#[tokio::test]
async fn an_elicitation_that_names_no_outstanding_request_is_refused() {
    let mut peer = Peer::start(json!({})).await;
    let (op, created) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
    let request = peer.request("session/new").await;
    let before = peer.live("global");
    for (id, scope) in [
        ("null", json!({"requestId": null})),
        ("unknown", json!({"requestId": "no-such-request"})),
        ("number", json!({"requestId": 99999})),
    ] {
        peer.agent_request(id, "elicitation/create", form(scope)).await;
        let response = peer.response(id).await;
        assert_eq!(response["error"]["code"], -32602, "{id}: {response}");
    }
    peer.settled().await;
    assert_eq!(peer.live("global"), before, "a refused request changes nothing");
    assert!(interaction_ids(&peer.live("global")).is_empty());

    // The one that names the pending request is an interaction.
    peer.agent_request(
        "named",
        "elicitation/create",
        form(json!({"requestId": request["id"]})),
    )
    .await;
    peer.until("the named elicitation joins the create", |peer| {
        peer.live("global")["op"]["id"] == op && interaction_ids(&peer.live("global")).len() == 1
    })
    .await;
    peer.reply(&request, json!({"sessionId": CREATED})).await;
    peer.ended(created).await.unwrap();
    peer.settled().await;
    assert_eq!(peer.live("global")["op"]["status"], "completed");
    assert_eq!(
        peer.response("named").await["result"]["action"],
        "cancel",
        "the parent's end answers it"
    );
}

// A request's end ends no session-scoped interaction (memory model §8.2, D11): one the Agent asked
// of the session while a control request ran is still the session's when the control completed,
// and is answered on its own.
#[tokio::test]
async fn a_session_scoped_interaction_outlives_the_control_request_it_arrived_in() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(
        CREATED,
        json!({"modes": {
            "currentModeId": "ask", "availableModes": [
                {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
            ],
        }}),
    )
    .await;
    let scope = format!("session/{CREATED}");
    let shown = peer.live(&scope)["op"].clone();
    let answer = peer
        .control(
            CREATED,
            crate::bridge::controls::Control::SetMode { mode_id: "plan".into() },
        )
        .await;
    let request = peer.request("session/set_mode").await;
    peer.agent_request(
        "question",
        "elicitation/create",
        form(json!({"sessionId": CREATED})),
    )
    .await;
    peer.until("the session receives the interaction", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    peer.reply(&request, json!({})).await;
    answer.await.unwrap().unwrap();
    peer.until("the control is folded", |peer| {
        peer.live(&scope)["controls"]["modes"]["currentModeId"] == "plan"
    })
    .await;
    peer.settled().await;
    let live = peer.live(&scope);
    assert_eq!(live["op"], shown, "a control installs no operation");
    let interaction = interaction_ids(&live);
    assert_eq!(interaction.len(), 1, "the interaction outlives the control: {live}");
    peer.answer_resource(&scope, &interaction[0], json!({"response": {"action": "accept", "content": {}}}))
        .await
        .unwrap();
    assert_eq!(
        peer.response("question").await["result"]["action"],
        "accept"
    );
    peer.until("the answered interaction leaves", |peer| {
        interaction_ids(&peer.live(&scope)).is_empty()
    })
    .await;
    peer.settled().await;
}

#[tokio::test]
async fn a_permission_asked_during_a_prompt_is_still_the_sessions_when_the_prompt_ended() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(
        CREATED,
        json!({"modes": {"currentModeId": "ask", "availableModes": [
            {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
        ]}}),
    )
    .await;
    let scope = format!("session/{CREATED}");
    let (op, request) = peer.prompt(CREATED, vec![text("hi")]).await;
    peer.agent_request("permission", "session/request_permission", permission(CREATED))
        .await;
    peer.until("the permission is pending", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    peer.reply(&request, json!({"stopReason": "end_turn"})).await;
    peer.until("the prompt completes", |peer| {
        peer.live(&scope)["op"]["id"] == op && peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    peer.settled().await;
    let interaction = interaction_ids(&peer.live(&scope));
    assert_eq!(interaction.len(), 1, "the permission outlives the prompt");
    // A new operation of the session does not take it either.
    let (next, next_request) = peer.prompt(CREATED, vec![text("next")]).await;
    peer.until("the next operation runs", |peer| peer.live(&scope)["op"]["id"] == next)
        .await;
    assert_eq!(interaction_ids(&peer.live(&scope)), interaction);
    peer.answer_resource(
        &scope,
        &interaction[0],
        json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
    )
    .await
    .unwrap();
    let response = peer.response("permission").await;
    assert_eq!(response["result"]["outcome"]["optionId"], "allow");
    peer.reply(&next_request, json!({"stopReason": "end_turn"})).await;
    peer.settled().await;
}

// Uses the existing Peer and real coordinator/ACP transport. No cancellation
// token is fired: the Agent only closes its incoming stream before answering.
#[tokio::test]
async fn review_natural_eof_fails_global_and_catalog_with_unavailable() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"list": {}}}),
        json!([{"id": "agent-login", "name": "Login"}]),
        |_| {},
    )
    .await;
    peer.start_op("global", "authenticate", json!({"methodId": "agent-login"}))
        .await;
    peer.request("authenticate").await;
    let (_, _list) = peer.start_op("catalog", "list", json!({})).await;
    peer.request("session/list").await;
    peer.settled().await;

    peer.responses.close().await.unwrap();
    peer.finish().await.unwrap();
    assert_eq!(peer.live("global")["agent"]["phase"], "stopped");
    for scope in ["global", "catalog"] {
        let live = peer.live(scope);
        assert_eq!(live["op"]["status"], "failed", "{scope}: {live}");
        assert_eq!(
            live["op"]["error"]["code"], "unavailable",
            "{scope}: {live}"
        );
    }
}

#[tokio::test]
async fn review_filesystem_setup_failure_publishes_final_agent_error() {
    let missing = std::env::temp_dir().join(format!("attyd-missing-root-{}", Uuid::new_v4()));
    assert!(!missing.exists());
    let configured = options(|options| {
        options.transport = Transport::Stdio;
        options.command = vec!["/bin/sh".into()];
        options.cwd = missing.clone();
    });
    let (outgoing, _requests) = futures::channel::mpsc::channel::<String>(64);
    let (_responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
    let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
    let (_commands, command_rx) = mpsc::unbounded_channel();
    let (events, _event_rx) = mpsc::unbounded_channel();
    let probe = super::memory_retention_tests::probe_retained_coordinator_state();
    let error = tokio::time::timeout(
        WAIT,
        run_connection(
            transport,
            Arc::new(configured),
            command_rx,
            EventSink { tx: events },
            CancellationToken::new(),
        ),
    )
    .await
    .expect("setup must finish")
    .unwrap_err();
    assert!(
        error
            .data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("workspace root cannot be resolved")),
        "{error:?}"
    );

    let retained = probe.await.expect("setup creates bridge state");
    let state = retained.lock().await;
    let global = state
        .protocol
        .publication
        .lock()
        .feed
        .with_tree(|tree| tree.get("/global").expect("initial global published").to_value());
    let agent = &global["agent"];
    assert_eq!(
        agent["phase"], "error",
        "setup failure left the published agent unfinished: {agent}"
    );
    assert!(
        agent["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("workspace root cannot be resolved")),
        "{agent}"
    );
}

async fn assert_prompt_cancel_preserves_accepted_url_flow(request_scoped: bool) {
    let label = if request_scoped {
        "request scope"
    } else {
        "session scope"
    };
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = format!("session/{CREATED}");
    let prompt = vec![text("hi")];
    let (op, request) = peer.prompt(CREATED, prompt).await;
    let elicitation_scope = if request_scoped {
        json!({"requestId": request["id"]})
    } else {
        json!({"sessionId": CREATED})
    };
    peer.agent_request(
        "sign-in",
        "elicitation/create",
        url_elicitation(elicitation_scope, "flow"),
    )
    .await;
    peer.until("the URL elicitation joins the prompt", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live(&scope)).remove(0);
    peer.answer(
        &scope,
        &interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
    assert_eq!(
        peer.response("sign-in").await["result"]["action"],
        "accept",
        "{label}"
    );
    peer.until("the accepted flow is published", |peer| {
        peer.live(&scope)["urlFlows"].get(&interaction).is_some()
            && interaction_ids(&peer.live(&scope)).is_empty()
    })
    .await;
    let accepted = peer.live(&scope)["urlFlows"][&interaction].clone();

    peer.write(&scope, "cancel", json!({}))
        .await
        .expect("the running prompt takes a cancel");
    let cancellation = peer.request("session/cancel").await;
    assert_eq!(cancellation["params"]["sessionId"], CREATED);
    peer.until("the cancel intent is published", |peer| {
        peer.live(&scope)["op"]["cancelRequested"] == true
    })
    .await;
    let live = peer.live(&scope);
    assert_eq!(live["op"]["id"], op, "{label}");
    assert_eq!(live["op"]["status"], "running", "{label}");
    assert_eq!(
        live["urlFlows"][&interaction], accepted,
        "{label}: cancelling an operation must not terminate an accepted resource"
    );

    peer.reply(&request, json!({"stopReason": "cancelled"}))
        .await;
    peer.until("the cancelled prompt completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live(&scope)["urlFlows"][&interaction],
        accepted,
        "{label}: the operation ending must not terminate an accepted resource"
    );
    assert_eq!(
        peer.live("global")["sessions"][CREATED]["pending"]["urlFlows"],
        1
    );
    peer.settled().await;

    peer.send(json!({
        "jsonrpc": "2.0", "method": "elicitation/complete",
        "params": {"elicitationId": "flow"},
    }))
    .await;
    peer.until("the Agent completes its flow", |peer| {
        peer.live(&scope)["urlFlows"][&interaction]["status"] == "completed"
    })
    .await;
    assert_eq!(
        peer.live("global")["sessions"][CREATED]["pending"]["urlFlows"],
        0
    );
    peer.settled().await;
}

#[tokio::test]
async fn review_prompt_cancel_preserves_accepted_request_scoped_url_flow() {
    assert_prompt_cancel_preserves_accepted_url_flow(true).await;
}

#[tokio::test]
async fn review_prompt_cancel_preserves_accepted_session_scoped_url_flow() {
    assert_prompt_cancel_preserves_accepted_url_flow(false).await;
}
