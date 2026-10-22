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
    Peer::ended(creating).await.unwrap();
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

#[tokio::test]
async fn review_a_null_request_scope_interaction_ends_with_the_global_operation_it_joined() {
    let mut peer = Peer::start(json!({})).await;
    let (op, created) = peer
        .start_op("global", "create", json!({"cwd": "/workspace"}))
        .await;
    let request = peer.request("session/new").await;
    peer.agent_request(
        "question",
        "elicitation/create",
        form(json!({"requestId": null})),
    )
    .await;
    peer.until("the global operation receives the interaction", |peer| {
        peer.live("global")["op"]["id"] == op && interaction_ids(&peer.live("global")).len() == 1
    })
    .await;
    let interaction = interaction_ids(&peer.live("global")).remove(0);
    peer.reply(&request, json!({"sessionId": CREATED})).await;
    Peer::ended(created).await.unwrap();
    peer.settled().await;
    let live = peer.live("global");
    assert_eq!(
        live["op"]["id"], op,
        "the pending interaction must not become a new operation"
    );
    assert_eq!(live["op"]["status"], "completed");
    {
        let lock = peer.lock();
        let state = lock.lock().await;
        assert!(
            !state
                .sessions
                .request_elicitations
                .contains_key(&interaction),
            "ending the operation must clear its pending interaction, not just hide it"
        );
        assert!(!state.request_elicitations.contains_key(&interaction));
    }
    assert_eq!(
        peer.response("question").await["result"]["action"],
        "cancel"
    );
}

// Supplementary existing-state-machine case: the same S7 cleanup gap also
// exists for a directly session-scoped interaction. Not a distinct new 4b
// implementation finding; it guards against a fix limited to requestId:null.
#[tokio::test]
async fn review_a_session_scoped_interaction_ends_with_its_control_operation() {
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
    let (op, _) = peer
        .start_op(&scope, "set_mode", json!({"modeId": "plan"}))
        .await;
    let request = peer.request("session/set_mode").await;
    peer.agent_request(
        "question",
        "elicitation/create",
        form(json!({"sessionId": CREATED})),
    )
    .await;
    peer.until("the control operation receives the interaction", |peer| {
        peer.live(&scope)["op"]["id"] == op && interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    peer.reply(&request, json!({})).await;
    peer.until("the control operation completes", |peer| {
        peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    peer.settled().await;
    let live = peer.live(&scope);
    assert_eq!(
        live["op"]["id"], op,
        "completion must not re-home its old interaction in a new agent_request"
    );
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["controls"]["modes"]["currentModeId"], "plan");
    assert!(interaction_ids(&live).is_empty());
    assert_eq!(
        peer.response("question").await["result"]["action"],
        "cancel"
    );
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
        .live(&crate::protocol::documents::Scope::Global)
        .expect("initial global published");
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
        peer.live(&scope)["urlFlows"].get(&interaction).is_none()
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
