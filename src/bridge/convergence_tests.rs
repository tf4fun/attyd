//! The convergence gate (plan §4.1) through the real coordinator and ACP
//! transport. A scripted Agent drives the bridge in process; after every step
//! the log-fed replicas must equal fresh projections of the bridge state.
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::Weak;

use agent_client_protocol::Lines;
use clap::Parser;
use futures::{SinkExt, StreamExt};

use super::*;
use crate::protocol::replica::{Convergence, fresh};

type Documents = BTreeMap<String, Value>;

struct Peer {
    commands: mpsc::UnboundedSender<BridgeInput>,
    requests: futures::channel::mpsc::Receiver<String>,
    responses: futures::channel::mpsc::Sender<io::Result<String>>,
    buffered: VecDeque<Value>,
    _events: mpsc::UnboundedReceiver<String>,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    state: Weak<BridgeLock>,
    convergence: Convergence,
}

impl Peer {
    async fn connect(timeout_seconds: i64) -> Self {
        let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
        let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let mut options =
            Options::try_parse_from(["attyd", "--transport", "ws", "--", "ws://127.0.0.1:1/acp"])
                .unwrap()
                .normalized()
                .unwrap();
        options.session_unobserved_timeout = timeout_seconds;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let probe = super::memory_retention_tests::probe_coordinator_state();
        let task = tokio::spawn(run_connection(
            transport,
            Arc::new(options),
            command_rx,
            EventSink { tx: events.into() },
            cancellation.clone(),
        ));
        Self {
            commands,
            requests,
            responses,
            buffered: VecDeque::new(),
            _events: event_rx,
            cancellation,
            task: Some(task),
            state: probe.await.unwrap(),
            convergence: Convergence::new(0xc0ffee),
        }
    }

    async fn start(timeout_seconds: i64, capabilities: Value) -> Self {
        let mut peer = Self::connect(timeout_seconds).await;
        let initialize = peer.next_request().await;
        assert_eq!(initialize["method"], "initialize");
        peer.reply(
            &initialize,
            json!({"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []}),
        )
        .await;
        peer.until("the agent is ready", |documents| {
            documents[crate::protocol::GLOBAL]["agent"]["phase"] == "ready"
        })
        .await;
        peer
    }

    async fn next_request(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(request) = self.buffered.pop_front() {
                    return request;
                }
                let raw = self.requests.next().await.expect("ACP connection ended");
                match serde_json::from_str(&raw).unwrap() {
                    Value::Array(batch) => self.buffered.extend(batch),
                    request => self.buffered.push_back(request),
                }
            }
        })
        .await
        .expect("expected request did not reach the Agent")
    }

    async fn send(&mut self, message: Value) {
        self.responses.send(Ok(message.to_string())).await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn update(&mut self, update: Value) {
        self.send(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": SESSION, "update": update,
            }}),
        )
        .await;
    }

    fn submit(&self, command: Value) -> oneshot::Receiver<Result<Value, BridgeRequestError>> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(BridgeInput::BusinessRequest { command, response })
            .unwrap();
        result
    }

    /// One convergence check at the current cut.
    async fn check(&mut self) -> Documents {
        let state = self.state.upgrade().expect("bridge state is alive");
        let state = state.lock_without_commit().await;
        let documents = fresh(&state.agent, state.catalog_revision, &state.sessions);
        if let Err(divergence) = self.convergence.check(&state.replication, &documents) {
            panic!("{divergence}");
        }
        documents
    }

    /// Checks convergence at every poll until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Documents) -> bool) -> Documents {
        for _ in 0..300 {
            let documents = self.check().await;
            if done(&documents) {
                return documents;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} never happened");
    }

    async fn settle(&mut self) {
        let state = self.state.upgrade().expect("bridge state is alive");
        let state = state.lock_without_commit().await;
        let documents = fresh(&state.agent, state.catalog_revision, &state.sessions);
        if let Err(divergence) = self.convergence.settle(&state.replication, &documents) {
            panic!("{divergence}");
        }
    }

    async fn stop(mut self) {
        self.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
            .await
            .expect("bridge did not stop")
            .unwrap()
            .unwrap();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

const SESSION: &str = "convergence-session";

fn session(documents: &Documents) -> &Value {
    &documents[&crate::protocol::session_scope(SESSION)]
}

fn held(documents: &Documents) -> bool {
    documents.contains_key(&crate::protocol::session_scope(SESSION))
}

async fn create(peer: &mut Peer) {
    create_unobserved(peer).await;
    peer.until("the session is held", held).await;
}

async fn create_unobserved(peer: &mut Peer) {
    let created = peer.submit(json!({
        "type": "session/new", "requestId": "convergence-new", "cwd": "/workspace",
    }));
    let request = peer.next_request().await;
    assert_eq!(request["method"], "session/new");
    peer.reply(
        &request,
        json!({"sessionId": SESSION, "modes": {
            "currentModeId": "ask",
            "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "code", "name": "Code"}],
        }}),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), created)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn controls_turns_permissions_close_and_delete_converge() {
    let mut peer = Peer::start(
        -1,
        json!({"sessionCapabilities": {"close": {}, "delete": {}, "list": {}}}),
    )
    .await;
    create(&mut peer).await;

    // Agent control updates, including sparse info and usage without cost.
    for (update, done) in [
        (
            json!({"sessionUpdate": "session_info_update", "title": "Named"}),
            json!({"/controls/info/title": "Named"}),
        ),
        (
            json!({"sessionUpdate": "session_info_update", "updatedAt": "2026-10-01T00:00:00Z"}),
            json!({"/controls/info/updatedAt": "2026-10-01T00:00:00Z"}),
        ),
        (
            json!({"sessionUpdate": "usage_update", "used": 10, "size": 100,
                   "cost": {"amount": 1.5, "currency": "USD"}}),
            json!({"/controls/usage/used": 10}),
        ),
        (
            json!({"sessionUpdate": "usage_update", "used": 20, "size": 100}),
            json!({"/controls/usage/used": 20}),
        ),
        (
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"}),
            json!({"/controls/modes/currentModeId": "code"}),
        ),
        (
            json!({"sessionUpdate": "available_commands_update",
                   "availableCommands": [{"name": "init", "description": "Init"}]}),
            json!({"/controls/commands/0/name": "init"}),
        ),
        (
            json!({"sessionUpdate": "config_option_update", "configOptions": []}),
            json!({"/controls/configOptions": []}),
        ),
    ] {
        peer.update(update.clone()).await;
        let (pointer, expected) = done.as_object().unwrap().iter().next().unwrap();
        let documents = peer
            .until(&format!("{update}"), |documents| {
                held(documents) && session(documents).pointer(pointer) == Some(expected)
            })
            .await;
        assert_eq!(
            documents[crate::protocol::GLOBAL]["sessions"][SESSION]["title"],
            "Named"
        );
    }

    // A turn started by another client streams into the session and summary.
    let revision = session(&peer.check().await)["history"]["revision"].clone();
    let (response, admitted) = oneshot::channel();
    peer.commands
        .send(BridgeInput::TurnRequest {
            session_id: SESSION.into(),
            history_revision: revision.as_str().unwrap().into(),
            client_intent_id: "convergence-turn".into(),
            prompt: vec![json!({"type": "text", "text": "Run it"})],
            response,
        })
        .unwrap();
    let admitted = tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(admitted["disposition"], "accepted");
    let prompt = peer.next_request().await;
    assert_eq!(prompt["method"], "session/prompt");
    peer.until("the turn runs", |documents| {
        documents[crate::protocol::GLOBAL]["sessions"][SESSION]["work"] == "running"
            && !session(documents)["turn"].is_null()
    })
    .await;
    peer.update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "tool-a", "title": "Read", "status": "in_progress",
    }))
    .await;
    for text in ["Hel", "lo", ", world"] {
        peer.update(json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "answer",
            "content": {"type": "text", "text": text},
        }))
        .await;
        peer.check().await;
    }
    peer.update(json!({
        "sessionUpdate": "tool_call", "toolCallId": "tool-b", "title": "Write", "status": "in_progress",
    }))
    .await;
    // A tool call update folds into the earlier slot of its tool call.
    peer.update(json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "tool-a", "status": "completed",
    }))
    .await;
    peer.until("the tool call folds into its slot", |documents| {
        let updates = &session(documents)["turn"]["updates"];
        updates[0]["status"] == "completed"
            && updates[1]["content"]["text"] == "Hello, world"
            && updates[2]["toolCallId"] == "tool-b"
    })
    .await;

    // A permission while the replica only follows the summaries.
    peer.send(json!({
        "jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
        "params": {
            "sessionId": SESSION, "toolCall": {"toolCallId": "tool-b"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
        },
    }))
    .await;
    let documents = peer
        .until("the permission is pending", |documents| {
            documents[crate::protocol::GLOBAL]["sessions"][SESSION]["pending"]["permissions"] == 1
        })
        .await;
    let permission_id = session(&documents)["interactions"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let responded = peer.submit(json!({
        "type": "permission/respond", "requestId": "convergence-permission",
        "permissionId": permission_id,
        "outcome": {"outcome": "selected", "optionId": "allow"},
    }));
    let delivered = peer.next_request().await;
    assert_eq!(delivered["id"], 900);
    assert_eq!(delivered["result"]["outcome"]["optionId"], "allow");
    tokio::time::timeout(Duration::from_secs(3), responded)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the permission is answered", |documents| {
        session(documents)["interactions"] == json!({})
            && documents[crate::protocol::GLOBAL]["sessions"][SESSION]["pending"]["permissions"]
                == 0
    })
    .await;

    peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
    let finished = peer
        .until("the turn completes", |documents| {
            session(documents)["turn"].is_null()
                && session(documents)["outcomes"]
                    .as_object()
                    .is_some_and(|outcomes| outcomes.len() == 1)
                && session(documents)["history"]["phase"] == "ready"
        })
        .await;

    // A background update after the turn folds into committed history.
    let committed = session(&finished)["history"]["revision"].clone();
    peer.update(json!({
        "sessionUpdate": "tool_call_update", "toolCallId": "tool-b", "status": "completed",
    }))
    .await;
    peer.until("the background update is committed", |documents| {
        session(documents)["history"]["revision"] != committed
    })
    .await;
    peer.settle().await;

    // Close ends the session scope.
    let closed = peer.submit(json!({
        "type": "session/close", "requestId": "convergence-close", "sessionId": SESSION,
    }));
    let close = peer.next_request().await;
    assert_eq!(close["method"], "session/close");
    peer.reply(&close, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), closed)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.until("the closed session leaves", |documents| !held(documents))
        .await;

    // Delete of the closed session.
    let deleted = peer.submit(json!({
        "type": "session/delete", "requestId": "convergence-delete", "sessionId": SESSION,
    }));
    let delete = peer.next_request().await;
    assert_eq!(delete["method"], "session/delete");
    peer.reply(&delete, json!({})).await;
    tokio::time::timeout(Duration::from_secs(3), deleted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peer.check().await;
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn unobserved_retirement_ends_the_session_scope() {
    let mut peer = Peer::start(0, json!({})).await;
    // With a zero timeout the session may retire before any poll sees it
    // held (the race the first-subscription lease closes in step 4), so the
    // log is what proves it was published and then ended.
    create_unobserved(&mut peer).await;
    peer.until("the unobserved session retires", |documents| {
        !held(documents)
    })
    .await;
    let state = peer.state.upgrade().unwrap();
    let changes = {
        let state = state.lock_without_commit().await;
        state
            .replication
            .log()
            .after(0)
            .unwrap()
            .filter(|entry| entry.scope == crate::protocol::session_scope(SESSION))
            .map(|entry| {
                (
                    entry.prev,
                    entry.change == crate::protocol::log::Change::End,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        changes.first().map(|change| change.0),
        Some(0),
        "{changes:?}"
    );
    assert_eq!(
        changes.last().map(|change| change.1),
        Some(true),
        "{changes:?}"
    );
    peer.settle().await;
    peer.stop().await;
}

#[tokio::test]
async fn a_bridge_transition_without_a_commit_is_caught() {
    let mut peer = Peer::start(-1, json!({})).await;
    create(&mut peer).await;
    peer.settle().await;
    {
        let state = peer.state.upgrade().unwrap();
        let mut state = state.lock_without_commit().await;
        let epoch = state.sessions.epoch().to_string();
        let incarnation = state.sessions.state(SESSION).unwrap().incarnation;
        state
            .sessions
            .update_control_state(
                &epoch,
                SESSION,
                incarnation,
                "session_info_update",
                json!({"sessionUpdate": "session_info_update", "title": "Uncommitted"}),
            )
            .unwrap();
    }
    let caught = tokio::spawn(async move {
        peer.check().await;
        peer
    })
    .await;
    let Err(panic) = caught else {
        panic!("the harness must catch the omitted commit");
    };
    let message = panic
        .into_panic()
        .downcast::<String>()
        .map(|message| *message)
        .unwrap_or_default();
    assert!(message.contains("Uncommitted"), "{message}");
}

#[tokio::test]
async fn cancellation_commits_agent_stopped() {
    let mut peer = Peer::start(-1, json!({})).await;
    // Keep the epoch's authority alive to inspect its final cut after teardown.
    let retained = peer.state.upgrade().expect("ready connection owns state");
    peer.cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), peer.task.take().unwrap())
        .await
        .expect("bridge did not stop")
        .expect("bridge task panicked")
        .expect("ordinary cancellation is a clean stop");

    {
        let state = retained.lock_without_commit().await;
        assert_eq!(
            state.agent.phase,
            AgentPhase::Stopped,
            "the connection ended but the v2 authority still advertises readiness"
        );
        assert!(state.agent.error.is_none());
    }
    // The existing ready replica must reach stopped through the log.
    let documents = peer.check().await;
    assert_eq!(
        documents[crate::protocol::GLOBAL]["agent"]["phase"],
        "stopped"
    );
}

#[tokio::test]
async fn initialize_refusal_commits_agent_error() {
    let mut peer = Peer::connect(-1).await;
    let initialize = peer.next_request().await;
    assert_eq!(initialize["method"], "initialize");
    let retained = peer
        .state
        .upgrade()
        .expect("initializing connection owns state");
    // Install the initializing cut before rejecting the RPC: the final check
    // must follow a log entry, not bootstrap from the final snapshot.
    let initial = peer.check().await;
    assert_eq!(
        initial[crate::protocol::GLOBAL]["agent"]["phase"],
        "initializing"
    );
    peer.send(json!({
        "jsonrpc": "2.0", "id": initialize["id"],
        "error": {
            "code": -32603,
            "message": "initialization refused",
            "data": {"cause": "fixture"},
        },
    }))
    .await;
    let error = tokio::time::timeout(Duration::from_secs(3), peer.task.take().unwrap())
        .await
        .expect("bridge did not settle after initialization was refused")
        .expect("bridge task panicked")
        .expect_err("the ACP initialize error must fail this connection");
    assert_eq!(error.message, "initialization refused");

    {
        let state = retained.lock_without_commit().await;
        assert_eq!(
            state.agent.phase,
            AgentPhase::Error,
            "the failed connection leaves the v2 authority stuck initializing"
        );
        let published_error = state
            .agent
            .error
            .as_ref()
            .expect("v2 keeps the connection error");
        assert_eq!(published_error.code, crate::protocol::ErrorCode::AgentError);
        assert_eq!(published_error.message, "initialization refused");
        assert_eq!(
            published_error.data,
            Some(serde_json::to_value(&error).unwrap())
        );
    }
    let documents = peer.check().await;
    assert_eq!(
        documents[crate::protocol::GLOBAL]["agent"]["phase"],
        "error"
    );
}

#[tokio::test]
async fn workspace_setup_failure_commits_agent_error() {
    struct Capture {
        probe: std::sync::Mutex<Option<oneshot::Receiver<Weak<BridgeLock>>>>,
        retained: std::sync::Mutex<Option<Arc<BridgeLock>>>,
    }

    impl futures::task::ArcWake for Capture {
        fn wake_by_ref(this: &Arc<Self>) {
            let mut probe = this.probe.lock().unwrap();
            let Some(receiver) = probe.as_mut() else {
                return;
            };
            match receiver.try_recv() {
                Ok(weak) => {
                    *this.retained.lock().unwrap() = weak.upgrade();
                    *probe = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => *probe = None,
            }
        }
    }

    // Setup can return before yielding after it publishes the weak probe.
    // Retain the state synchronously, without delaying or changing production
    // execution, so the final epoch cut remains inspectable after teardown.
    let capture = Arc::new(Capture {
        probe: std::sync::Mutex::new(Some(
            super::memory_retention_tests::probe_coordinator_state(),
        )),
        retained: std::sync::Mutex::new(None),
    });
    let waker = futures::task::waker(capture.clone());
    {
        let mut probe = capture.probe.lock().unwrap();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(
            std::future::Future::poll(std::pin::Pin::new(probe.as_mut().unwrap()), &mut cx,)
                .is_pending()
        );
    }
    // A spurious wake must leave the pending receiver available.
    waker.wake_by_ref();

    let workspace = tempfile::tempdir().unwrap();
    let missing = workspace.path().join("missing-additional-root");
    let options = Options::try_parse_from([
        std::ffi::OsString::from("attyd"),
        "--cwd".into(),
        workspace.path().as_os_str().to_owned(),
        "--add-dir".into(),
        missing.as_os_str().to_owned(),
        "--".into(),
        "unused-agent-before-filesystem-setup".into(),
    ])
    .unwrap()
    .normalized()
    .unwrap();
    let (_commands, command_rx) = mpsc::unbounded_channel();
    let (events, mut events_rx) = mpsc::unbounded_channel();
    run_with_cancellation(
        Arc::new(options),
        command_rx,
        events.into(),
        CancellationToken::new(),
    )
    .await;
    // Repeated wakes after capture must not consume the probe a second time.
    waker.wake_by_ref();

    let events = std::iter::from_fn(|| events_rx.try_recv().ok())
        .map(|raw| serde_json::from_str::<Value>(&raw).unwrap())
        .collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| { event["type"] == "bridge/phase" && event["phase"] == "error" })
    );
    assert!(
        events.iter().any(|event| {
            event["type"] == "bridge/error"
                && event["data"].as_str().is_some_and(|message| {
                    message.contains("workspace root cannot be resolved")
                        && message.contains(missing.to_string_lossy().as_ref())
                })
        }),
        "setup must fail on the additional root before trying to spawn the placeholder Agent: {events:?}"
    );

    let retained = capture
        .retained
        .lock()
        .unwrap()
        .take()
        .expect("captured authority before setup returned");
    let state = retained.lock_without_commit().await;
    assert_eq!(state.agent.phase, AgentPhase::Error);
    assert!(state.agent.error.is_some());

    // Rebuild from the actual retained operations, rather than installing a
    // fresh final snapshot that could conceal an authority-only repair.
    let mut global = json!({});
    let mut applied = 0;
    for entry in state
        .replication
        .log()
        .after(0)
        .expect("the small setup log retains its initial entry")
        .filter(|entry| entry.scope == crate::protocol::GLOBAL)
    {
        assert_eq!(entry.prev, applied);
        let crate::protocol::log::Change::Ops(ops) = &entry.change else {
            panic!("global remains present until the epoch ends");
        };
        for op in ops.iter() {
            crate::protocol::diff::apply(&mut global, op).unwrap();
        }
        applied = entry.seq;
    }
    assert_eq!(global["agent"]["phase"], "error");
    assert!(!global["agent"]["error"].is_null());
    assert_eq!(
        global,
        json!(crate::protocol::project_global(
            &state.agent,
            state.catalog_revision,
            &state.sessions,
        ))
    );
}
