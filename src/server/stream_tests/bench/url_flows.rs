//! URL flows (protocol S7, §4.6, §7; acceptance 11): accepting a URL
//! elicitation moves it to its scope's `urlFlows`, where it lives by its own
//! lifecycle. An operation's end or replacement ends no flow; the Agent's
//! `elicitation/complete`, a client's dismissal, the scope's end and the
//! Agent connection's end do.
use super::*;

/// The flow `interaction` of `scope`'s `live`.
fn flow(live: &Value, interaction: &str) -> Value {
    live["urlFlows"][interaction].clone()
}

/// The Agent asks `params` (a URL elicitation) as Agent request `id`; once
/// `scope` shows it, it is accepted through `scope`'s `answer` on `op`, and
/// the accepted flow's interaction ID is returned. The interaction leaves
/// `interactions` in the step that adds it to `urlFlows`.
async fn accept_flow(
    bench: &Bench,
    link: &mut Link,
    watch: &mut Watch,
    scope: &str,
    id: &str,
    params: Value,
) -> String {
    let elicitation_id = params["elicitationId"].clone();
    link.agent_request(id, "elicitation/create", params.clone())
        .await;
    watch
        .until_live(scope, &format!("{scope} shows {id}"), |live| {
            live["interactions"]
                .as_object()
                .is_some_and(|interactions| {
                    interactions.values().any(|interaction| {
                        interaction["request"]["elicitationId"] == elicitation_id
                    })
                })
        })
        .await;
    let live = watch.live(scope);
    let op = live["op"]["id"].clone();
    let interaction = interaction_ids(&live)
        .into_iter()
        .find(|interaction| {
            live["interactions"][interaction]["request"]["elicitationId"] == elicitation_id
        })
        .unwrap();
    let request = live["interactions"][&interaction]["request"].clone();
    assert_eq!(
        live["interactions"][&interaction]["kind"],
        "elicitation"
    );
    let (status, body) = bench
        .answer(scope, &op, answers(&[(interaction.as_str(), accept_url())]))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{scope}: {body}");
    assert_eq!(body["op"], op);
    assert_eq!(link.response(id).await["result"]["action"], "accept");
    // Every published step shows the ID in exactly one place: pending, or
    // the flow. Taking the events one by one, a step with both or neither
    // (an interaction removed in one commit, the flow added in a later one)
    // fails.
    loop {
        let live = watch.live(scope);
        let pending = interaction_ids(&live).contains(&interaction);
        let flowing = live["urlFlows"].get(&interaction).is_some();
        assert!(
            pending != flowing,
            "{scope}: {interaction} is pending and a flow, or neither: {live}"
        );
        if flowing {
            break;
        }
        watch.event().await;
    }
    let live = watch.live(scope);
    assert!(
        !interaction_ids(&live).contains(&interaction),
        "the accepted interaction leaves `interactions` in the step that adds its flow"
    );
    assert_eq!(
        flow(&live, &interaction),
        json!({
            "interactionId": interaction,
            "elicitationId": params["elicitationId"],
            "request": request,
            "status": "waiting",
        })
    );
    interaction
}

/// Dismisses `interaction` in `scope`: `204` and the flow is gone; a repeat
/// is `404` and changes nothing.
async fn dismiss(
    bench: &Bench,
    link: &mut Link,
    watch: &mut Watch,
    scope: &str,
    interaction: &str,
) {
    let (status, body) = bench.act(scope, interaction, "dismiss", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{scope}: {body}");
    watch
        .until_live(scope, "the dismissed flow is gone", |live| {
            live["urlFlows"].get(interaction).is_none()
        })
        .await;
    let before = bench.document(scope).await;
    let (status, body) = bench.act(scope, interaction, "dismiss", json!({})).await;
    not_found(status, &body, "a repeated dismissal");
    link.quiet(QUIET).await;
    assert_eq!(
        bench.document(scope).await,
        before,
        "a repeat changes nothing"
    );
}

/// Flows accepted in `global` (an authenticate), `catalog` (a list) and a
/// session (a prompt, and none) outlive their operation and its
/// replacement, and each ends only by its own lifecycle.
#[tokio::test]
async fn flows_outlive_their_operation_and_end_by_their_own_lifecycle() {
    let (bench, mut link) = Bench::ws_initialized(json!({
        "protocolVersion": 1,
        "agentCapabilities": {"sessionCapabilities": {"list": {}}},
        "authMethods": [{"id": "agent-login", "name": "Login"}],
    }))
    .await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench
        .watch(&scopes(&["global", "catalog", "session/s"]))
        .await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog") && watch.has("session/s")
        })
        .await;

    // global: an authenticate's request-scoped flow outlives it and the
    // authenticate that replaces it.
    let expect = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "global",
            &expect,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let auth = assert_accepted(status, &body, "authenticate");
    let request = link.request("authenticate").await;
    let global_flow = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "global",
        "global-sign-in",
        url_elicitation(json!({"requestId": request["id"]}), "global-flow"),
    )
    .await;
    let kept = flow(&watch.live("global"), &global_flow);
    link.reply(&request, json!({})).await;
    watch.until_op("global", &auth, "completed").await;
    assert_eq!(flow(&watch.live("global"), &global_flow), kept);
    let (status, body) = bench
        .op(
            "global",
            &auth,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let again = assert_accepted(status, &body, "the next authenticate");
    let request = link.request("authenticate").await;
    link.reply(&request, json!({})).await;
    watch.until_op("global", &again, "completed").await;
    assert_eq!(
        flow(&watch.live("global"), &global_flow),
        kept,
        "a replaced operation ends no flow"
    );

    // catalog: a list's flow outlives the list and the next list.
    let expect = watch.live("catalog")["op"]["id"].clone();
    let listed = bench.spawn_op("catalog", &expect, "list", json!({}));
    let request = link.request("session/list").await;
    let catalog_flow = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "catalog",
        "catalog-sign-in",
        url_elicitation(json!({"requestId": request["id"]}), "catalog-flow"),
    )
    .await;
    let catalog_kept = flow(&watch.live("catalog"), &catalog_flow);
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = body["op"].clone();
    watch.until_op("catalog", &list, "completed").await;
    let listed = bench.spawn_op("catalog", &list, "list", json!({}));
    let request = link.request("session/list").await;
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    watch.until_op("catalog", &body["op"], "completed").await;
    assert_eq!(flow(&watch.live("catalog"), &catalog_flow), catalog_kept);

    // session: a prompt's flow outlives the prompt and the next prompt; the
    // session summary counts it.
    let expect = watch.live("session/s")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "session/s",
            &expect,
            "prompt",
            json!({"prompt": [text("sign in")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "the prompt");
    let turn = link.request("session/prompt").await;
    let prompt_flow = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "session/s",
        "prompt-sign-in",
        url_elicitation(json!({"sessionId": "s"}), "prompt-flow"),
    )
    .await;
    let prompt_kept = flow(&watch.live("session/s"), &prompt_flow);
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &prompt, "completed").await;
    let (status, body) = bench
        .op(
            "session/s",
            &prompt,
            "prompt",
            json!({"prompt": [text("next")]}),
        )
        .await;
    let next = assert_accepted(status, &body, "the next prompt");
    let turn = link.request("session/prompt").await;
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &next, "completed").await;
    assert_eq!(flow(&watch.live("session/s"), &prompt_flow), prompt_kept);
    watch
        .until_live("global", "the summary counts the flow", |live| {
            live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
                && live["sessions"]["s"]["pending"]["urlFlows"] == 1
        })
        .await;

    // A new subscription aligns with the three kept flows.
    let mut second = bench
        .watch(&scopes(&["global", "catalog", "session/s"]))
        .await;
    second
        .until("the second stream's alignments", |watch| {
            watch.has("global") && watch.has("catalog") && watch.has("session/s")
        })
        .await;
    for (scope, interaction, kept) in [
        ("global", &global_flow, &kept),
        ("catalog", &catalog_flow, &catalog_kept),
        ("session/s", &prompt_flow, &prompt_kept),
    ] {
        assert_eq!(&flow(&second.live(scope), interaction), kept, "{scope}");
    }
    drop(second);

    // session: an Agent request with no running operation starts no operation; its accepted
    // flow outlives the next prompt.
    // The Agent's `elicitation/complete` ends that one flow.
    let asked = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "session/s",
        "asked-sign-in",
        url_elicitation(json!({"sessionId": "s"}), "asked-flow"),
    )
    .await;
    let asking = watch.live("session/s")["op"].clone();
    assert_ne!(asking["status"], "running", "no operation started");
    let asked_kept = flow(&watch.live("session/s"), &asked);
    assert!(!asked_kept.is_null());
    watch
        .until_live("global", "the summary counts both flows", |live| {
            live["sessions"]["s"]["pending"]["urlFlows"] == 2
        })
        .await;

    let (status, body) = bench
        .op(
            "session/s",
            &asking["id"],
            "prompt",
            json!({"prompt": [text("after the agent request")]}),
        )
        .await;
    let successor = assert_accepted(status, &body, "the agent request's successor");
    let turn = link.request("session/prompt").await;
    watch.until_op("session/s", &successor, "running").await;
    assert_ne!(successor, asking["id"]);
    assert_eq!(flow(&watch.live("session/s"), &asked), asked_kept);
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &successor, "completed").await;
    assert_eq!(
        flow(&watch.live("session/s"), &asked),
        asked_kept,
        "replacing the agent request and completing its successor end no flow"
    );
    watch
        .until_live("global", "the successor keeps both flows", |live| {
            live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
                && live["sessions"]["s"]["pending"]["urlFlows"] == 2
        })
        .await;

    link.notify(
        "elicitation/complete",
        json!({"elicitationId": "asked-flow"}),
    )
    .await;
    watch
        .until_live("session/s", "the flow shows completed", |live| {
            live["urlFlows"][&asked]["status"] == "completed"
        })
        .await;
    assert_eq!(
        flow(&watch.live("session/s"), &prompt_flow),
        prompt_kept,
        "the completion finishes the one flow with its elicitationId"
    );
    assert_eq!(watch.live("session/s")["op"]["id"], successor);
    watch
        .until_live("global", "the summary drops the completed flow", |live| {
            live["sessions"]["s"]["pending"]["urlFlows"] == 1
        })
        .await;

    // Dismissals: each through the scope that shows the flow; a repeat, or
    // the wrong scope, is `404` and changes nothing.
    let before = bench.document("catalog").await;
    let (status, body) = bench
        .act("global", &catalog_flow, "dismiss", json!({}))
        .await;
    not_found(status, &body, "a catalog flow dismissed through global");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("catalog").await, before);
    dismiss(&bench, &mut link, &mut watch, "global", &global_flow).await;
    dismiss(&bench, &mut link, &mut watch, "catalog", &catalog_flow).await;
    dismiss(&bench, &mut link, &mut watch, "session/s", &prompt_flow).await;
    // A completed flow stays until it is dismissed.
    dismiss(&bench, &mut link, &mut watch, "session/s", &asked).await;
    watch
        .until_live("global", "the summary has no flow", |live| {
            live["sessions"]["s"]["pending"]["urlFlows"] == 0
        })
        .await;
    for scope in ["global", "catalog", "session/s"] {
        assert!(flow_ids(&watch.live(scope)).is_empty(), "{scope}");
    }
    // A dismissal is a resource action: it replaces no operation.
    assert_eq!(watch.live("global")["op"]["id"], again);
    assert_eq!(watch.live("session/s")["op"]["id"], successor);
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// An elicitationId may be reused once its flow ended: the new flow has a new
/// interaction ID. The ended flow's ID, or the new one in the wrong scope, is
/// `404` and leaves the new flow; a delete ends its scope's flows and
/// releases the elicitationId again.
#[tokio::test]
async fn a_reused_elicitation_id_names_a_new_flow() {
    let (bench, mut link) = Bench::ws(json!({
        "sessionCapabilities": {"delete": {}},
    }))
    .await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["global", "session/s"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("session/s")
        })
        .await;

    // A: a request-scoped flow of a prompt, ended by the Agent.
    let expect = watch.live("session/s")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "session/s",
            &expect,
            "prompt",
            json!({"prompt": [text("a")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "the first prompt");
    let turn = link.request("session/prompt").await;
    let a = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "session/s",
        "sign-in-a",
        url_elicitation(json!({"requestId": turn["id"]}), "reused"),
    )
    .await;
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &prompt, "completed").await;
    link.notify("elicitation/complete", json!({"elicitationId": "reused"}))
        .await;
    watch
        .until_live("session/s", "A completes", |live| {
            live["urlFlows"][&a]["status"] == "completed"
        })
        .await;

    // B: the same elicitationId on the next prompt is a new flow.
    let (status, body) = bench
        .op(
            "session/s",
            &prompt,
            "prompt",
            json!({"prompt": [text("b")]}),
        )
        .await;
    let next = assert_accepted(status, &body, "the next prompt");
    let turn = link.request("session/prompt").await;
    let b = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "session/s",
        "sign-in-b",
        url_elicitation(json!({"requestId": turn["id"]}), "reused"),
    )
    .await;
    assert_ne!(a, b, "a reused elicitationId gets a new interaction ID");
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &next, "completed").await;
    watch
        .until_live("global", "the summary of B's prompt", |live| {
            live["sessions"]["s"]["op"] == json!({"kind": "prompt", "status": "completed"})
                && live["sessions"]["s"]["pending"]["urlFlows"] == 1
        })
        .await;

    // A's ID, and B's in the wrong scope, are not found and leave B.
    let session = bench.document("session/s").await;
    let global = bench.document("global").await;
    let (status, body) = bench.act("session/s", &a, "dismiss", json!({})).await;
    not_found(status, &body, "the ended flow A");
    let (status, body) = bench.act("global", &b, "dismiss", json!({})).await;
    not_found(status, &body, "B through global");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, session);
    assert_eq!(bench.document("global").await, global);
    assert_eq!(flow_ids(&watch.live("session/s")), vec![b.clone()]);

    // B's dismissal releases the elicitationId: an Agent request reuses it.
    dismiss(&bench, &mut link, &mut watch, "session/s", &b).await;
    let c = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "session/s",
        "sign-in-c",
        url_elicitation(json!({"sessionId": "s"}), "reused"),
    )
    .await;
    assert!(c != a && c != b);
    let asking = watch.live("session/s")["op"]["id"].clone();
    watch.until_op("session/s", &asking, "completed").await;

    // A delete ends the scope and its flow C; the elicitationId is free for
    // a global operation, whose flow is global's.
    let expect = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op("global", &expect, "delete", json!({"sessionId": "s"}))
        .await;
    let delete = assert_accepted(status, &body, "the delete");
    let request = link.request("session/delete").await;
    link.reply(&request, json!({})).await;
    watch.until_op("global", &delete, "completed").await;
    watch
        .until("the deleted scope ends", |watch| {
            watch.ended("session/s").is_some()
        })
        .await;
    assert_eq!(
        watch.ended("session/s").unwrap(),
        json!({"reason": "deleted"})
    );
    let (status, body) = bench.act("session/s", &c, "dismiss", json!({})).await;
    not_found(status, &body, "a flow of a deleted scope");
    let created = bench.spawn_op("global", &delete, "create", json!({"cwd": CWD}));
    let request = link.request("session/new").await;
    let d = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "global",
        "sign-in-d",
        url_elicitation(json!({"requestId": request["id"]}), "reused"),
    )
    .await;
    assert!(![&a, &b, &c].contains(&&d));
    link.reply(&request, json!({"sessionId": "other"})).await;
    let (status, body) = created.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    watch.until_op("global", &body["op"], "completed").await;
    assert!(watch.live("global")["urlFlows"].get(&d).is_some());
    watch
        .until_live("global", "the created session's summary", |live| {
            live["sessions"]["other"]["op"] == json!({"kind": "open", "status": "completed"})
        })
        .await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// The Agent connection's end leaves global's waiting flows in the final documents, `ended`.
#[tokio::test]
async fn the_connection_end_leaves_the_global_flow_ended_in_the_final_document() {
    let (bench, mut link) = Bench::ws_initialized(json!({
        "protocolVersion": 1,
        "agentCapabilities": {},
        "authMethods": [{"id": "agent-login", "name": "Login"}],
    }))
    .await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until("the alignment", |watch| watch.has("global"))
        .await;
    let expect = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "global",
            &expect,
            "authenticate",
            json!({"methodId": "agent-login"}),
        )
        .await;
    let auth = assert_accepted(status, &body, "authenticate");
    let request = link.request("authenticate").await;
    let flow = accept_flow(
        &bench,
        &mut link,
        &mut watch,
        "global",
        "sign-in",
        url_elicitation(json!({"requestId": request["id"]}), "flow"),
    )
    .await;
    link.reply(&request, json!({})).await;
    watch.until_op("global", &auth, "completed").await;
    assert!(watch.live("global")["urlFlows"].get(&flow).is_some());

    link.close().await;
    watch
        .until_live("global", "the connection ends with its flow", |live| {
            live["agent"]["phase"] != "ready" && live["urlFlows"][&flow]["status"] == "ended"
        })
        .await;
    // The ended connection's bridge applies nothing: the flow stays in the final document as
    // it was published, and the next connection starts without it.
    let before = bench.document("global").await;
    let (status, body) = bench.act("global", &flow, "dismiss", json!({})).await;
    not_found(status, &body, "a dismissal after the connection ended");
    assert_eq!(bench.document("global").await, before);
    watch.converged(&bench).await;
}
