//! Controls are independent requests, not public operations (protocol §9).
//! Drive real ACP requests through the coordinator, and inspect both canonical
//! controls and the feed's independently applied replicas.
use super::stream_harness::*;
use super::*;
use crate::bridge::controls::{Control, ControlAnswer};
use crate::protocol::documents::ErrorCode;

fn offered() -> Value {
    json!({
        "modes": {"currentModeId": "ask", "availableModes": [
            {"id": "ask", "name": "Ask"}, {"id": "plan", "name": "Plan"},
        ]},
        "configOptions": verbose(false),
    })
}

fn verbose(value: bool) -> Value {
    json!([{"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": value}])
}

async fn mode(peer: &mut Peer, value: &str) -> (ControlAnswer, Value) {
    let answer = peer
        .control(
            CREATED,
            Control::SetMode {
                mode_id: value.into(),
            },
        )
        .await;
    let request = peer.request("session/set_mode").await;
    assert_eq!(request["params"]["modeId"], value);
    (answer, request)
}

async fn config(peer: &mut Peer, value: bool) -> (ControlAnswer, Value) {
    let answer = peer
        .control(
            CREATED,
            Control::SetConfig {
                config_id: "verbose".into(),
                value: json!(value),
            },
        )
        .await;
    let request = peer.request("session/set_config_option").await;
    assert_eq!(request["params"]["value"], value);
    (answer, request)
}

async fn ended(answer: ControlAnswer) -> Result<(), ProtocolError> {
    tokio::time::timeout(WAIT, answer)
        .await
        .expect("the control's answer did not settle")
        .expect("the control's answer was dropped")
}

#[tokio::test]
async fn concurrent_modes_reach_the_agent_and_fold_in_response_order() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let op = peer.live(&scope)["op"].clone();
    let (first, first_request) = mode(&mut peer, "plan").await;
    let (second, second_request) = mode(&mut peer, "ask").await;
    assert_ne!(first_request["id"], second_request["id"]);

    // A notification is reduced before either acknowledgement. The second
    // request answers first; the earlier request must still be live afterwards.
    peer.update(
        CREATED,
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
    )
    .await;
    peer.until("the notification is reduced", |peer| {
        peer.live(&scope)["controls"]["modes"]["currentModeId"] == "plan"
    })
    .await;
    peer.reply(&second_request, json!({})).await;
    ended(second).await.unwrap();
    assert_eq!(
        peer.controls(CREATED).await["modes"]["currentModeId"],
        "ask"
    );
    peer.reply(&first_request, json!({})).await;
    ended(first).await.unwrap();
    assert_eq!(
        peer.controls(CREATED).await["modes"]["currentModeId"],
        "plan"
    );
    peer.settled().await;
    assert_eq!(
        peer.live(&scope)["controls"]["modes"]["currentModeId"],
        "plan"
    );
    assert_eq!(peer.live(&scope)["op"], op, "controls install no operation");
}

#[tokio::test]
async fn concurrent_configs_fold_each_success_and_a_later_error_changes_nothing() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let op = peer.live(&scope)["op"].clone();
    let (first, first_request) = config(&mut peer, true).await;
    let (second, second_request) = config(&mut peer, false).await;
    peer.reply(&second_request, json!({"configOptions": verbose(false)}))
        .await;
    ended(second).await.unwrap();
    peer.reply(&first_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(first).await.unwrap();
    assert_eq!(peer.controls(CREATED).await["configOptions"], verbose(true));

    let (failed, failed_request) = config(&mut peer, false).await;
    let (successful, successful_request) = config(&mut peer, true).await;
    peer.reply(&successful_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(successful).await.unwrap();
    peer.refuse(&failed_request).await;
    assert_eq!(ended(failed).await.unwrap_err().code, ErrorCode::AgentError);
    assert_eq!(peer.controls(CREATED).await["configOptions"], verbose(true));
    peer.settled().await;
    assert_eq!(
        peer.live(&scope)["controls"]["configOptions"],
        verbose(true)
    );
    assert_eq!(peer.live(&scope)["op"], op);
}

#[tokio::test]
async fn a_prompt_and_controls_progress_independently_but_public_operations_stay_serial() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"fork": {}}})).await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let (mode_answer, mode_request) = mode(&mut peer, "plan").await;
    let (prompt, prompt_request) = peer.prompt(CREATED, vec![text("hello")]).await;
    let (config_answer, config_request) = config(&mut peer, true).await;
    let prompt_op = peer.live(&scope)["op"].clone();
    assert_eq!(prompt_op["id"], prompt);

    for (kind, args) in [
        ("prompt", json!({"prompt": [text("not concurrent")]})),
        ("fork", json!({"cwd": CWD})),
    ] {
        let refusal = match peer.write(&scope, kind, args).await {
            Err(error) => error,
            Ok(_) => panic!("{kind} must not replace the running prompt"),
        };
        assert_eq!(refusal.code, ErrorCode::NotAllowed);
    }
    peer.reply(&config_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(config_answer).await.unwrap();
    peer.refuse(&mode_request).await;
    assert_eq!(
        ended(mode_answer).await.unwrap_err().code,
        ErrorCode::AgentError
    );
    peer.settled().await;
    assert_eq!(peer.live(&scope)["op"], prompt_op);
    let controls = peer.controls(CREATED).await;
    assert_eq!(controls["configOptions"], verbose(true));
    assert_eq!(controls["modes"]["currentModeId"], "ask");

    peer.reply(&prompt_request, json!({"stopReason": "end_turn"}))
        .await;
    peer.until("the original prompt completes", |peer| {
        peer.live(&scope)["op"]["id"] == prompt && peer.live(&scope)["op"]["status"] == "completed"
    })
    .await;
    // Only that completion frees the public operation slot.
    let (_, forked) = peer.start_op(&scope, "fork", json!({"cwd": CWD})).await;
    let fork = peer.request("session/fork").await;
    peer.reply(&fork, json!({"sessionId": "forked"})).await;
    assert_eq!(
        Peer::ended(forked).await.unwrap(),
        json!({"sessionId": "forked"})
    );
    peer.settled().await;
}

#[tokio::test]
async fn each_concurrent_control_ends_only_its_own_request_scoped_interaction() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let (first, first_request) = mode(&mut peer, "plan").await;
    let (second, second_request) = config(&mut peer, true).await;
    peer.agent_request(
        "mode-question",
        "elicitation/create",
        form(json!({"requestId": first_request["id"]})),
    )
    .await;
    peer.until("the first parent has its interaction", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 1
    })
    .await;
    let first_interaction = interaction_ids(&peer.live(&scope)).remove(0);
    peer.agent_request(
        "config-question",
        "elicitation/create",
        form(json!({"requestId": second_request["id"]})),
    )
    .await;
    peer.until("both parents have interactions", |peer| {
        interaction_ids(&peer.live(&scope)).len() == 2
    })
    .await;

    peer.reply(&second_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(second).await.unwrap();
    assert_eq!(
        peer.response("config-question").await["result"]["action"],
        "cancel"
    );
    peer.settled().await;
    assert_eq!(
        interaction_ids(&peer.live(&scope)),
        vec![first_interaction.clone()]
    );
    peer.answer(
        &scope,
        &first_interaction,
        json!({"response": {"action": "accept"}}),
    )
    .await;
    assert_eq!(
        peer.response("mode-question").await["result"]["action"],
        "accept"
    );
    peer.reply(&first_request, json!({})).await;
    ended(first).await.unwrap();
    peer.settled().await;
    assert!(interaction_ids(&peer.live(&scope)).is_empty());
    let lock = peer.lock();
    let state = lock.lock().await;
    assert!(
        state.action_replies.is_empty(),
        "both action waiters were settled"
    );
    assert!(
        state.in_flight_request_ids.is_empty(),
        "both command owners were released"
    );
}

#[tokio::test]
async fn an_old_control_response_cannot_change_a_reinstalled_session_with_the_same_id() {
    let mut peer = Peer::start(json!({"sessionCapabilities": {"close": {}}})).await;
    peer.create(CREATED, offered()).await;
    let (old, old_request) = mode(&mut peer, "plan").await;
    let closing = peer.control(CREATED, Control::Close).await;
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    ended(closing).await.unwrap();
    peer.create(CREATED, offered()).await;
    let (fresh, fresh_request) = config(&mut peer, true).await;

    peer.reply(&old_request, json!({})).await;
    // The obsolete action must settle, but it owns no value in the new installation.
    let _old_outcome = ended(old).await;
    assert_eq!(
        peer.controls(CREATED).await["modes"]["currentModeId"],
        "ask"
    );
    peer.reply(&fresh_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(fresh).await.unwrap();
    let controls = peer.controls(CREATED).await;
    assert_eq!(controls["modes"]["currentModeId"], "ask");
    assert_eq!(controls["configOptions"], verbose(true));
    peer.settled().await;
    let lock = peer.lock();
    let state = lock.lock().await;
    assert!(state.action_replies.is_empty());
    assert!(state.in_flight_request_ids.is_empty());
}

#[tokio::test]
async fn eof_settles_all_controls_and_the_next_connection_has_no_old_control_work() {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the session is followed", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    let (first, _) = mode(&mut peer, "plan").await;
    let (second, _) = config(&mut peer, true).await;
    let old_state = peer.lock();
    peer.eof().await.unwrap();
    for answer in [first, second] {
        assert_eq!(
            ended(answer).await.unwrap_err().code,
            ErrorCode::Unavailable
        );
    }
    {
        let state = old_state.lock().await;
        assert!(
            state.action_replies.is_empty(),
            "EOF settles every action waiter"
        );
        assert!(
            state.in_flight_request_ids.is_empty(),
            "EOF frees every command owner"
        );
    }

    let publication = peer.handover();
    let mut next = Peer::start_on(publication, json!({"loadSession": true})).await;
    let load = next.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], CREATED);
    next.reply(&load, offered()).await;
    stream
        .until(
            &mut next,
            "the next connection loads the session",
            |stream| {
                stream.live(&scope)["op"]["kind"] == "load"
                    && stream.live(&scope)["op"]["status"] == "completed"
            },
        )
        .await;
    assert_eq!(
        next.controls(CREATED).await["modes"]["currentModeId"],
        "ask"
    );
    assert_eq!(
        next.controls(CREATED).await["configOptions"],
        verbose(false)
    );
    let (fresh, request) = mode(&mut next, "plan").await;
    next.reply(&request, json!({})).await;
    ended(fresh).await.unwrap();
    assert_eq!(
        next.controls(CREATED).await["modes"]["currentModeId"],
        "plan"
    );
    next.settled().await;
}

#[tokio::test]
async fn the_last_outstanding_control_keeps_an_unfollowed_session_needed() {
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create(CREATED, offered()).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None);
    stream
        .until(&mut peer, "the session is followed", |stream| {
            stream.replicas.contains_key(&scope)
        })
        .await;
    let (first, first_request) = mode(&mut peer, "plan").await;
    let (second, second_request) = config(&mut peer, true).await;
    stream.close();
    peer.until_followers(CREATED, 0).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.reply(&first_request, json!({})).await;
    ended(first).await.unwrap();
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(
        !peer.released(&scope),
        "the other control still owns live work"
    );

    peer.reply(&second_request, json!({"configOptions": verbose(true)}))
        .await;
    ended(second).await.unwrap();
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    peer.until("only the last completion permits release", |peer| {
        peer.released(&scope)
    })
    .await;
    peer.settled().await;
}
