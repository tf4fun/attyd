use super::*;

const SESSION: &str = "close-session";

async fn create_close_session(agent: &mut TestAgent) -> Value {
    let result = agent.submit(json!({"type":"session/new", "requestId":"new", "cwd":"/repo"}));
    let request = agent.next_request().await;
    assert_eq!(request["method"], "session/new");
    agent.reply(&request, json!({"sessionId":SESSION})).await;
    TestAgent::result(result).await.unwrap()
}

fn assert_close(request: &Value) {
    assert_eq!(request["method"], "session/close", "{request}");
    assert_eq!(request["params"]["sessionId"], SESSION);
}

async fn refused_close_has_no_automatic_retry(timeout_seconds: i64) {
    let mut agent =
        TestAgent::start(timeout_seconds, json!({"sessionCapabilities":{"close":{}}})).await;
    create_close_session(&mut agent).await;
    let close = agent
        .request_within(Duration::from_secs(timeout_seconds as u64 + 1))
        .await
        .expect("the first idle interval must dispatch close");
    assert_close(&close);
    agent.refuse(&close).await;

    // Cross more than one complete interval. The old 250ms check with a 1s
    // timeout ended before the erroneous replacement timer could fire.
    let interval = Duration::from_secs(timeout_seconds.max(1) as u64 * 3);
    let duplicate = agent.request_within(interval).await;
    agent.stop().await;
    assert!(
        duplicate.is_none(),
        "a refused automatic close must remain settled until a new observation \
         interval; timeout={timeout_seconds}, duplicate={duplicate:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_refused_immediate_close_does_not_spin() {
    refused_close_has_no_automatic_retry(0).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_refused_close_does_not_restart_the_timeout() {
    refused_close_has_no_automatic_retry(30).await;
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_successful_close_retires_once() {
    let mut agent = TestAgent::start(30, json!({"sessionCapabilities":{"close":{}}})).await;
    create_close_session(&mut agent).await;
    let close = agent
        .request_within(Duration::from_secs(31))
        .await
        .expect("the idle interval must dispatch close");
    assert_close(&close);
    agent.reply(&close, json!({})).await;
    let retired = agent.event("bridge/session_retired", SESSION).await;
    assert_eq!(retired["reason"], "closed");
    let duplicate = agent.request_within(Duration::from_secs(90)).await;
    agent.stop().await;
    assert!(
        duplicate.is_none(),
        "closed session was retried: {duplicate:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_retirement_refused_close_preserves_turns_and_rearms_after_observation() {
    let mut agent = TestAgent::start(30, json!({"sessionCapabilities":{"close":{}}})).await;
    let created = create_close_session(&mut agent).await;
    let close = agent
        .request_within(Duration::from_secs(31))
        .await
        .expect("the idle interval must dispatch close");
    assert_close(&close);
    agent.refuse(&close).await;
    // Let refusal settle before observing again, staying inside the interval so
    // this guard also passes independently of the no-retry regression above.
    assert!(agent.request_within(Duration::from_secs(1)).await.is_none());

    let lease = ObservationLease::new();
    let (reply, observed) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::ObserveSession {
            session_id: SESSION.into(),
            cwd: None,
            expected_owner: None,
            observer_id: 1,
            lease: lease.clone(),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), observed)
        .await
        .expect("refused session could not be observed again")
        .unwrap()
        .unwrap();
    assert!(
        agent
            .request_within(Duration::from_secs(90))
            .await
            .is_none()
    );

    let (response, admitted) = oneshot::channel();
    agent
        .commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.into(),
            history_revision: created["view"]["session"]["historyRevision"]
                .as_str()
                .unwrap()
                .into(),
            client_intent_id: "after-refusal".into(),
            prompt: vec![json!({"type":"text", "text":"still usable"})],
            response,
        })
        .unwrap();
    let admitted = tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .expect("refused session could not admit a new turn")
        .unwrap()
        .unwrap();
    assert_eq!(admitted["disposition"], "accepted");
    let prompt = agent.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], SESSION);
    agent.reply(&prompt, json!({"stopReason":"end_turn"})).await;
    agent.event("acp/prompt_complete", SESSION).await;

    lease.cancel();
    agent
        .commands
        .send(BridgeInput::UnobserveSession {
            session_id: SESSION.into(),
            observer_id: 1,
            lease,
        })
        .unwrap();
    assert!(
        agent
            .request_within(Duration::from_secs(29))
            .await
            .is_none()
    );
    let next_close = agent
        .request_within(Duration::from_secs(2))
        .await
        .expect("a new observation interval must permit a new close");
    assert_close(&next_close);
    assert_ne!(next_close["id"], close["id"]);
    agent.reply(&next_close, json!({})).await;
    agent.event("bridge/session_retired", SESSION).await;
    agent.stop().await;
}
