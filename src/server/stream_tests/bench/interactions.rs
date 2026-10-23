//! Interactions, answers and cancels (protocol §4.4, §6.2, §6.4; acceptance
//! 7, 8): every Agent request joins the running operation of its scope and is
//! answered through that scope's `answer`, whole or not at all; `cancel`
//! applies to `prompt`, `terminal_login` and `agent_request` and ends nothing
//! by itself.
use super::*;

/// An admitted answer or cancel names the running operation.
#[track_caller]
fn assert_joined(status: StatusCode, body: &Value, op: &Value, what: &str) {
    assert_eq!(status, StatusCode::ACCEPTED, "{what}: {body}");
    assert_eq!(&body["op"], op, "{what}: 202 names the running op: {body}");
}

fn selected(option: &str) -> Value {
    json!({"outcome": {"outcome": "selected", "optionId": option}})
}

#[tokio::test]
async fn interactions_of_create_fork_set_config_and_list_are_answered_in_their_scope() {
    let (bench, mut link) = Bench::ws(json!({
        "sessionCapabilities": {"fork": {}, "list": {}},
    }))
    .await;
    let mut watch = bench.watch(&scopes(&["global", "catalog"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog")
        })
        .await;

    // A create asks through an elicitation that names its `session/new`.
    let none = watch.live("global")["op"]["id"].clone();
    let created = bench.spawn_op("global", &none, "create", json!({"cwd": CWD}));
    let new = link.request("session/new").await;
    link.agent_request(
        "create-question",
        "elicitation/create",
        form(json!({"requestId": new["id"]}), "create?"),
    )
    .await;
    watch
        .until_live("global", "the create's interaction", |live| {
            !interaction_ids(live).is_empty()
        })
        .await;
    let live = watch.live("global");
    let create = live["op"]["id"].clone();
    assert_eq!(live["op"]["kind"], "create");
    let interaction = interaction_for(&live, "create?");
    assert_eq!(
        live["interactions"][&interaction]["kind"],
        "elicitation"
    );
    let (status, body) = bench
        .answer(
            "global",
            &create,
            answers(&[(interaction.as_str(), accept())]),
        )
        .await;
    assert_joined(status, &body, &create, "the create's answer");
    let response = link.response("create-question").await;
    assert_eq!(response["result"]["action"], "accept", "{response}");
    watch
        .until_live("global", "the answered create runs on", |live| {
            live["op"]["id"] == create && interaction_ids(live).is_empty()
        })
        .await;
    assert_eq!(watch.live("global")["op"]["status"], "running");
    // The delivered set, submitted again while its operation still runs.
    let (status, body) = bench
        .answer(
            "global",
            &create,
            answers(&[(interaction.as_str(), accept())]),
        )
        .await;
    not_found(status, &body, "a delivered answer, submitted again");
    link.quiet(QUIET).await;
    let mut response = controls();
    response["sessionId"] = json!("s");
    link.reply(&new, response).await;
    let (status, body) = created.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"], create);
    assert_eq!(body["result"], json!({"sessionId": "s"}));
    watch.until_op("global", &create, "completed").await;

    // A fork asks through a permission in its source.
    let mut session = bench.watch(&scopes(&["session/s"])).await;
    session
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = session.live("session/s")["op"]["id"].clone();
    let forked = bench.spawn_op("session/s", &open, "fork", json!({}));
    let request = link.request("session/fork").await;
    link.agent_request(
        "fork-question",
        "session/request_permission",
        permission("s", "fork-tool"),
    )
    .await;
    session
        .until_live("session/s", "the fork's interaction", |live| {
            live["op"]["kind"] == "fork" && !interaction_ids(live).is_empty()
        })
        .await;
    let live = session.live("session/s");
    let fork = live["op"]["id"].clone();
    let interaction = interaction_for(&live, "fork-tool");
    assert_eq!(
        live["interactions"][&interaction]["kind"],
        "permission"
    );
    let (status, body) = bench
        .answer(
            "session/s",
            &fork,
            answers(&[(interaction.as_str(), allow())]),
        )
        .await;
    assert_joined(status, &body, &fork, "the fork's answer");
    let response = link.response("fork-question").await;
    assert_eq!(response["result"], allow(), "{response}");
    session
        .until_live("session/s", "the answered fork runs on", |live| {
            live["op"]["id"] == fork && interaction_ids(live).is_empty()
        })
        .await;
    assert_eq!(session.live("session/s")["op"]["status"], "running");
    link.reply(&request, json!({"sessionId": "forked"})).await;
    let (status, body) = forked.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!({"sessionId": "forked"}));
    session.until_op("session/s", &fork, "completed").await;

    // A set_config asks through an elicitation that names its
    // `session/set_config_option`: it belongs to the session, not `global`.
    let (status, body) = bench
        .op(
            "session/s",
            &fork,
            "set_config",
            json!({"configId": "verbose", "value": true}),
        )
        .await;
    let config = assert_accepted(status, &body, "set_config");
    let request = link.request("session/set_config_option").await;
    link.agent_request(
        "config-question",
        "elicitation/create",
        form(json!({"requestId": request["id"]}), "config?"),
    )
    .await;
    session
        .until_live("session/s", "the set_config's interaction", |live| {
            live["op"]["id"] == config && !interaction_ids(live).is_empty()
        })
        .await;
    let interaction = interaction_for(&session.live("session/s"), "config?");
    assert!(
        interaction_ids(&bench.document("global").await["live"]).is_empty(),
        "the interaction is the session's"
    );
    let (status, body) = bench
        .answer(
            "session/s",
            &config,
            answers(&[(interaction.as_str(), accept())]),
        )
        .await;
    assert_joined(status, &body, &config, "the set_config's answer");
    assert_eq!(
        link.response("config-question").await["result"]["action"],
        "accept"
    );
    session
        .until_live("session/s", "the answered set_config runs on", |live| {
            live["op"]["id"] == config && interaction_ids(live).is_empty()
        })
        .await;
    assert_eq!(session.live("session/s")["op"]["status"], "running");
    link.reply(&request, json!({"configOptions": verbose(true)}))
        .await;
    session.until_op("session/s", &config, "completed").await;

    // A list asks through an elicitation that names its `session/list`.
    let catalog = watch.live("catalog")["op"]["id"].clone();
    let listed = bench.spawn_op("catalog", &catalog, "list", json!({}));
    let request = link.request("session/list").await;
    link.agent_request(
        "list-question",
        "elicitation/create",
        form(json!({"requestId": request["id"]}), "list?"),
    )
    .await;
    watch
        .until_live("catalog", "the list's interaction", |live| {
            live["op"]["kind"] == "list" && !interaction_ids(live).is_empty()
        })
        .await;
    let live = watch.live("catalog");
    let list = live["op"]["id"].clone();
    let interaction = interaction_for(&live, "list?");
    let (status, body) = bench
        .answer(
            "catalog",
            &list,
            answers(&[(interaction.as_str(), accept())]),
        )
        .await;
    assert_joined(status, &body, &list, "the list's answer");
    assert_eq!(
        link.response("list-question").await["result"]["action"],
        "accept"
    );
    watch
        .until_live("catalog", "the answered list runs on", |live| {
            live["op"]["id"] == list && interaction_ids(live).is_empty()
        })
        .await;
    assert_eq!(watch.live("catalog")["op"]["status"], "running");
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;
    let (status, body) = listed.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"], list);
    watch.until_op("catalog", &list, "completed").await;
    watch.converged(&bench).await;
    session.converged(&bench).await;
}

#[tokio::test]
async fn a_load_interaction_is_answered_in_the_loading_session() {
    let (bench, mut link) = Bench::ws(json!({"loadSession": true})).await;
    let mut watch = bench.watch(&cold("cold")).await;
    let load = link.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "cold");
    link.agent_request(
        "load-question",
        "session/request_permission",
        permission("cold", "load-tool"),
    )
    .await;
    watch
        .until_live("session/cold", "the load's interaction", |live| {
            live["op"]["kind"] == "load" && !interaction_ids(live).is_empty()
        })
        .await;
    let live = watch.live("session/cold");
    let op = live["op"]["id"].clone();
    assert_eq!(live["op"]["status"], "running");
    let interaction = interaction_for(&live, "load-tool");
    // An answer names the interaction, not an operation: it is refused on the form of its body
    // and consumes nothing.
    let before = bench.document("session/cold").await;
    let (status, body) = bench
        .act("session/cold", &interaction, "answer", json!({"outcome": {"outcome": "selected"}}))
        .await;
    invalid(status, &body, "an answer that does not fit the request");
    link.quiet(QUIET).await;
    assert_eq!(bench.document("session/cold").await, before);

    let (status, body) = bench
        .answer(
            "session/cold",
            &op,
            answers(&[(interaction.as_str(), allow())]),
        )
        .await;
    assert_joined(status, &body, &op, "the load's answer");
    assert_eq!(link.response("load-question").await["result"], allow());
    watch
        .until_live("session/cold", "the answered load runs on", |live| {
            live["op"]["id"] == op && interaction_ids(live).is_empty()
        })
        .await;
    assert_eq!(watch.live("session/cold")["op"]["status"], "running");
    link.reply(&load, json!({})).await;
    watch.until_op("session/cold", &op, "completed").await;
    watch.converged(&bench).await;
}

/// With no operation running, an Agent request starts an `agent_request`,
/// which later requests join; it completes when its last interaction goes.
#[tokio::test]
async fn an_agent_request_without_a_running_op_starts_one_that_completes_with_its_last_interaction()
{
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = watch.live("session/s")["op"]["id"].clone();
    link.agent_request("one", "session/request_permission", permission("s", "one"))
        .await;
    link.agent_request("two", "session/request_permission", permission("s", "two"))
        .await;
    watch
        .until_live("session/s", "both interactions", |live| {
            interaction_ids(live).len() == 2
        })
        .await;
    let live = watch.live("session/s");
    let request = live["op"]["id"].clone();
    assert_eq!(live["op"]["kind"], "agent_request");
    assert_eq!(live["op"]["status"], "running");
    assert_ne!(request, open);
    let (one, two) = (interaction_for(&live, "one"), interaction_for(&live, "two"));

    // It occupies its session like any operation.
    let (status, body) = bench
        .op("session/s", &open, "prompt", json!({"prompt": [text("x")]}))
        .await;
    stale(status, &body, "a prompt on the replaced open");
    let (status, body) = bench
        .op(
            "session/s",
            &request,
            "prompt",
            json!({"prompt": [text("x")]}),
        )
        .await;
    not_allowed(status, &body, "a prompt while the agent request runs");

    let (status, body) = bench
        .answer(
            "session/s",
            &request,
            answers(&[(one.as_str(), allow()), (two.as_str(), selected("deny"))]),
        )
        .await;
    assert_joined(status, &body, &request, "the answers");
    let responses = link.responses(&["one", "two"]).await;
    assert_eq!(responses["one"]["result"], allow());
    assert_eq!(responses["two"]["result"], selected("deny"));
    watch.until_op("session/s", &request, "completed").await;
    assert!(interaction_ids(&watch.live("session/s")).is_empty());

    let (status, body) = bench
        .op(
            "session/s",
            &request,
            "prompt",
            json!({"prompt": [text("next")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "a prompt on the completed agent request");
    let turn = link.request("session/prompt").await;
    link.reply(&turn, json!({"stopReason": "end_turn"})).await;
    watch.until_op("session/s", &prompt, "completed").await;
    watch.converged(&bench).await;
}

/// Each interaction is answered on its own, at its resource, and at most once (D10): an answer
/// that does not fit its request is refused and consumes nothing, the others stay pending, and one
/// the Agent cancelled is gone.
#[tokio::test]
async fn interactions_are_answered_one_at_a_time_and_one_the_agent_cancelled_is_gone() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    for (id, tool) in [("p1", "one"), ("p2", "two"), ("p3", "three")] {
        link.agent_request(id, "session/request_permission", permission("s", tool))
            .await;
    }
    watch
        .until_live("session/s", "three interactions", |live| {
            interaction_ids(live).len() == 3
        })
        .await;
    let live = watch.live("session/s");
    let (one, two, three) = (
        interaction_for(&live, "one"),
        interaction_for(&live, "two"),
        interaction_for(&live, "three"),
    );

    // An answer that does not fit its request is refused and consumes nothing.
    let before = bench.document("session/s").await;
    for (bad, what) in [
        (accept(), "an elicitation response to a permission"),
        (
            selected("no-such-option"),
            "an option the permission does not offer",
        ),
        (json!({}), "an empty answer"),
    ] {
        let (status, body) = bench.act("session/s", &one, "answer", bad).await;
        invalid(status, &body, what);
    }
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, before);

    // One interaction is answered; the others stay pending, and it cannot be answered again.
    let (status, _) = bench.act("session/s", &one, "answer", allow()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(link.response("p1").await["result"], allow());
    watch
        .until_live("session/s", "the answered one leaves", |live| {
            interaction_ids(live).len() == 2
        })
        .await;
    let (status, body) = bench.act("session/s", &one, "answer", allow()).await;
    not_found(status, &body, "an interaction answered already");

    // One the Agent cancelled is gone, and answering it finds nothing.
    link.notify("$/cancel_request", json!({"requestId": "p2"})).await;
    // The bridge answers the cancelled request itself.
    assert_eq!(link.response("p2").await["error"]["code"], -32800);
    watch
        .until_live("session/s", "the cancelled interaction leaves", |live| {
            interaction_ids(live) == [three.clone()]
        })
        .await;
    let (status, body) = bench.act("session/s", &two, "answer", allow()).await;
    not_found(status, &body, "an interaction the Agent cancelled");

    let (status, _) = bench.act("session/s", &three, "answer", selected("deny")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(link.response("p3").await["result"], selected("deny"));
    watch
        .until_live("session/s", "no interaction is left", |live| {
            interaction_ids(live).is_empty()
        })
        .await;
    link.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A prompt's cancel sends `session/cancel` once and sets `cancelRequested`;
/// the prompt keeps its session until the Agent answers it.
#[tokio::test]
async fn a_prompt_cancel_asks_the_agent_once_and_keeps_the_session_occupied() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", controls()).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let open = watch.live("session/s")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("long")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "a prompt");
    let request = link.request("session/prompt").await;
    watch.until_op("session/s", &prompt, "running").await;
    assert_eq!(watch.live("session/s")["op"]["cancelRequested"], false);

    let (status, body) = bench.cancel("session/s", &open).await;
    stale(status, &body, "a cancel on the replaced open");
    let (status, body) = bench.cancel("session/s", &prompt).await;
    assert_joined(status, &body, &prompt, "the cancel");
    let cancel = link.request("session/cancel").await;
    assert_eq!(cancel["params"]["sessionId"], "s");
    assert!(
        cancel.get("id").is_none(),
        "session/cancel is a notification"
    );
    watch
        .until_live("session/s", "the cancel is requested", |live| {
            live["op"]["id"] == prompt && live["op"]["cancelRequested"] == true
        })
        .await;
    assert_eq!(watch.live("session/s")["op"]["status"], "running");

    // A repeat is admitted and changes nothing; the prompt still occupies.
    let before = bench.document("session/s").await;
    let (status, body) = bench.cancel("session/s", &prompt).await;
    assert_joined(status, &body, &prompt, "the repeated cancel");
    let (status, body) = bench
        .op("session/s", &prompt, "set_mode", json!({"modeId": "plan"}))
        .await;
    not_allowed(status, &body, "a set_mode while the cancelled prompt runs");
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    assert_eq!(bench.document("session/s").await, before);

    link.reply(&request, json!({"stopReason": "cancelled"}))
        .await;
    watch.until_op("session/s", &prompt, "completed").await;
    assert_eq!(
        watch.live("session/s")["op"]["result"]["stopReason"],
        "cancelled",
        "an answered prompt completes with the Agent's stop reason"
    );
    assert_eq!(watch.entries("session/s").len(), 1, "the prompt reached the session");
    let (status, body) = bench.cancel("session/s", &prompt).await;
    not_allowed(status, &body, "a cancel of the ended prompt");

    let (status, body) = bench
        .op("session/s", &prompt, "set_mode", json!({"modeId": "plan"}))
        .await;
    let mode = assert_accepted(status, &body, "a set_mode after the cancelled prompt");
    let request = link.request("session/set_mode").await;
    link.reply(&request, json!({})).await;
    watch.until_op("session/s", &mode, "completed").await;
    watch.converged(&bench).await;
}

/// An `agent_request`'s cancel answers each pending interaction with its
/// cancel outcome; it ends `cancelled`, and a new operation is admitted on it.
#[tokio::test]
async fn a_cancelled_agent_request_answers_its_interactions_with_their_cancel_outcomes() {
    let (bench, mut link) = Bench::ws(json!({})).await;
    bench.create_with(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    link.agent_request(
        "permission",
        "session/request_permission",
        permission("s", "tool"),
    )
    .await;
    link.agent_request(
        "elicitation",
        "elicitation/create",
        form(json!({"sessionId": "s"}), "form?"),
    )
    .await;
    watch
        .until_live("session/s", "both interactions", |live| {
            interaction_ids(live).len() == 2
        })
        .await;
    let op = watch.live("session/s")["op"]["id"].clone();
    assert_eq!(watch.live("session/s")["op"]["kind"], "agent_request");

    let (status, body) = bench.cancel("session/s", &op).await;
    assert_joined(status, &body, &op, "the cancel");
    let responses = link.responses(&["permission", "elicitation"]).await;
    assert_eq!(
        responses["permission"]["result"],
        json!({"outcome": {"outcome": "cancelled"}})
    );
    assert_eq!(
        responses["elicitation"]["result"],
        json!({"action": "cancel"})
    );
    watch.until_op("session/s", &op, "cancelled").await;
    let live = watch.live("session/s");
    assert!(interaction_ids(&live).is_empty());
    assert!(live["op"].get("result").is_none(), "{live}");

    let (status, body) = bench.cancel("session/s", &op).await;
    not_allowed(status, &body, "a cancel of the ended agent request");
    let (status, body) = bench.act("session/s", "no-such-interaction", "answer", allow()).await;
    not_found(status, &body, "an answer to an interaction that is gone");
    link.quiet(QUIET).await;

    let (status, body) = bench
        .op(
            "session/s",
            &op,
            "prompt",
            json!({"prompt": [text("next")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "a prompt on the cancelled agent request");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &prompt, "completed").await;
    watch.converged(&bench).await;
}

/// Kinds other than `prompt`, `terminal_login` and `agent_request` refuse a
/// cancel while they run; it reaches no Agent and changes nothing.
#[tokio::test]
async fn other_kinds_refuse_a_cancel() {
    let (bench, mut link) = Bench::ws(json!({
        "loadSession": true,
        "sessionCapabilities": {"fork": {}, "list": {}},
    }))
    .await;
    bench.create_with(&mut link, "s", controls()).await;
    let mut watch = bench
        .watch(&scopes(&["global", "catalog", "session/s"]))
        .await;
    watch
        .until("the alignments", |watch| {
            watch.has("global") && watch.has("catalog") && watch.has("session/s")
        })
        .await;

    async fn refused(bench: &Bench, link: &mut Link, scope: &str, op: &Value, what: &str) {
        let before = bench.document(scope).await;
        let (status, body) = bench.cancel(scope, op).await;
        not_allowed(status, &body, what);
        link.quiet(QUIET).await;
        assert_eq!(bench.document(scope).await, before, "{what}");
    }

    let open = watch.live("session/s")["op"]["id"].clone();
    refused(&bench, &mut link, "session/s", &open, "the ended open").await;
    let (status, body) = bench
        .op("session/s", &open, "set_mode", json!({"modeId": "plan"}))
        .await;
    let mode = assert_accepted(status, &body, "set_mode");
    let request = link.request("session/set_mode").await;
    refused(&bench, &mut link, "session/s", &mode, "a running set_mode").await;
    link.reply(&request, json!({})).await;
    watch.until_op("session/s", &mode, "completed").await;

    let (status, body) = bench
        .op(
            "session/s",
            &mode,
            "set_config",
            json!({"configId": "verbose", "value": true}),
        )
        .await;
    let config = assert_accepted(status, &body, "set_config");
    let request = link.request("session/set_config_option").await;
    refused(
        &bench,
        &mut link,
        "session/s",
        &config,
        "a running set_config",
    )
    .await;
    link.reply(&request, json!({"configOptions": verbose(true)}))
        .await;
    watch.until_op("session/s", &config, "completed").await;

    let (status, body) = bench.op("session/s", &config, "fork", json!({})).await;
    let fork = assert_accepted(status, &body, "fork");
    let request = link.request("session/fork").await;
    refused(&bench, &mut link, "session/s", &fork, "a running fork").await;
    link.reply(&request, json!({"sessionId": "forked"})).await;
    // With `loadSession`, the fork's target is loaded.
    let load = link.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], "forked");
    link.reply(&load, json!({})).await;
    watch.until_op("session/s", &fork, "completed").await;

    let global = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench.op("global", &global, "create", json!({"cwd": CWD})).await;
    let create = assert_accepted(status, &body, "create");
    let request = link.request("session/new").await;
    refused(&bench, &mut link, "global", &create, "a running create").await;
    link.reply(&request, json!({"sessionId": "t"})).await;
    watch.until_op("global", &create, "completed").await;

    let catalog = watch.live("catalog")["op"]["id"].clone();
    let (status, body) = bench.op("catalog", &catalog, "list", json!({})).await;
    let list = assert_accepted(status, &body, "list");
    let request = link.request("session/list").await;
    refused(&bench, &mut link, "catalog", &list, "a running list").await;
    link.reply(&request, json!({"sessions": [], "nextCursor": null}))
        .await;

    let mut cold = bench.watch(&super::cold("cold")).await;
    let load = link.request("session/load").await;
    cold.until_live("session/cold", "the load runs", |live| {
        live["op"]["kind"] == "load" && live["op"]["status"] == "running"
    })
    .await;
    let loading = cold.live("session/cold")["op"]["id"].clone();
    refused(
        &bench,
        &mut link,
        "session/cold",
        &loading,
        "a running load",
    )
    .await;
    link.reply(&load, json!({})).await;
    cold.until_op("session/cold", &loading, "completed").await;
    watch.until_op("catalog", &list, "completed").await;
    watch
        .until_live("global", "the cold session's summary", |live| {
            live["sessions"]["cold"]["op"] == json!({"kind": "load", "status": "completed"})
        })
        .await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
    cold.converged(&bench).await;
}
