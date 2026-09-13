//! Wire-level cancellation ordering through the production coordinator and pump.

use super::agent_dispatch::AgentContext;
use super::coordinator::{self, Coordinator};
use super::scheduling::{BridgeIngress, ScheduledInput, Scheduling};
use super::*;
use agent_client_protocol::{Dispatch, Lines};
use futures::{SinkExt, StreamExt};
use std::io;

const SESSION: &str = "ordered-interaction";
const AGENT_REQUEST_ID: u64 = 17;

fn setup() -> (AgentContext, Scheduling, mpsc::UnboundedReceiver<String>) {
    let state = Arc::new(Mutex::new(BridgeState::default()));
    let (events, receiver) = mpsc::unbounded_channel();
    let sink = EventSink { tx: events.into() };
    let epoch = state.try_lock().unwrap().sessions.epoch().to_owned();
    let (scheduling, ingress) =
        Scheduling::new(epoch.clone(), sink.clone(), CancellationToken::new()).unwrap();
    (
        AgentContext {
            state,
            mcp: McpManager::new(PathBuf::from("/workspace"), Vec::new(), sink.tx.clone()),
            sink,
            terminals: None,
            filesystem: None,
            ingress,
            owner: None,
            dispatch_owner: rpc_owner(&epoch, None, "coordinator-test", None),
            url_registration: None,
            request_lease: None,
        },
        scheduling,
        receiver,
    )
}

fn install_session(state: &mut BridgeState) -> u64 {
    let epoch = state.sessions.epoch().to_owned();
    let incarnation = state
        .sessions
        .open_new(
            &epoch,
            SESSION,
            "/workspace",
            json!({ "sessionId": SESSION }),
        )
        .unwrap();
    state.sessions.register_new(SESSION, incarnation);
    incarnation
}

fn interaction_batch(permission: bool) -> Vec<Value> {
    let (method, params) = if permission {
        (
            "session/request_permission",
            json!({
                "sessionId": SESSION,
                "toolCall": {"toolCallId":"ordered-tool", "title":"Confirm", "status":"pending"},
                "options":[{"optionId":"allow", "name":"Allow once", "kind":"allow_once"}],
            }),
        )
    } else {
        (
            "elicitation/create",
            json!({
                "sessionId":SESSION, "mode":"form", "message":"Confirm",
                "requestedSchema":{"type":"object", "properties":{}},
            }),
        )
    };
    vec![
        json!({"jsonrpc":"2.0", "id":AGENT_REQUEST_ID, "method":method, "params":params}),
        json!({"jsonrpc":"2.0", "method":"$/cancel_request", "params":{"requestId":AGENT_REQUEST_ID}}),
        json!({
            "jsonrpc":"2.0", "method":"session/update", "params":{
                "sessionId":SESSION, "update":{
                    "sessionUpdate":"agent_message_chunk", "messageId":"after-cancel",
                    "content":{"type":"text", "text":"After the cancellation."},
                },
            },
        }),
    ]
}

async fn next_delivery(scheduling: &mut Scheduling) -> ScheduledInput {
    loop {
        if let Some(delivery) = scheduling.pump().unwrap() {
            return delivery;
        }
        scheduling.wake.notified().await;
    }
}

async fn assert_interaction_sequence(
    permission: bool,
    incarnation: u64,
    scheduling: &mut Scheduling,
    connection: &ConnectionTo<Agent>,
    services: &AgentContext,
    events: &mut mpsc::UnboundedReceiver<String>,
) {
    let request = next_delivery(scheduling).await;
    assert!(matches!(
        &request.event,
        BridgeIngress::Acp(Dispatch::Request(..))
    ));
    let route = request.route.as_ref().expect("request route captured");
    assert_eq!(route.dispatch_owner.session_id.as_deref(), Some(SESSION));
    assert_eq!(route.dispatch_owner.incarnation, Some(incarnation));
    assert_eq!(route.owner.as_ref().unwrap().incarnation, incarnation);
    let allocation = route.dispatch_owner.attempt_id.clone().unwrap();
    assert_eq!(
        request.request_lease.as_ref().unwrap().allocation(),
        allocation
    );
    assert!(
        coordinator::dispatch_ready(request, scheduling, connection, services, false)
            .await
            .unwrap()
            .is_none()
    );

    // The SDK has already marked the request cancelled while parsing the batch.
    // Business cancellation must still wait for this second FIFO delivery.
    let cancel = next_delivery(scheduling).await;
    let BridgeIngress::InboundRequestCancelled(capture) = &cancel.event else {
        panic!("the later update overtook the wire cancellation");
    };
    assert_eq!(capture.allocation, allocation);
    assert_eq!(capture.route.dispatch_owner.incarnation, Some(incarnation));
    assert_eq!(
        capture.route.owner.as_ref().unwrap().incarnation,
        incarnation
    );
    assert_eq!(cancel.turn.handle().unwrap().incarnation(), incarnation);
    {
        let mut state = services.state.lock().await;
        let resources = state.sessions.resources(SESSION, incarnation).unwrap();
        assert!(
            if permission {
                resources.permissions.contains_key(&allocation)
            } else {
                resources.elicitations.contains_key(&allocation)
            },
            "the SDK cancellation marker bypassed the captured local delivery"
        );
        assert!(
            state
                .sessions
                .view(SESSION, incarnation)
                .unwrap()
                .baseline
                .updates()
                .is_empty()
        );
    }
    coordinator::dispatch_ready(cancel, scheduling, connection, services, false)
        .await
        .unwrap();
    {
        let mut state = services.state.lock().await;
        let resources = state.sessions.resources(SESSION, incarnation).unwrap();
        assert!(resources.permissions.is_empty());
        assert!(resources.elicitations.is_empty());
        let runtime = state.sessions.session(SESSION).unwrap();
        assert!(runtime.permissions.is_empty());
        assert!(runtime.elicitations.is_empty());
        assert!(
            state
                .sessions
                .view(SESSION, incarnation)
                .unwrap()
                .baseline
                .updates()
                .is_empty()
        );
    }

    let update = next_delivery(scheduling).await;
    assert!(matches!(
        &update.event,
        BridgeIngress::Acp(Dispatch::Notification(_))
    ));
    coordinator::dispatch_ready(update, scheduling, connection, services, false)
        .await
        .unwrap();
    let mut published = Vec::new();
    loop {
        let event: Value = serde_json::from_str(&events.recv().await.unwrap()).unwrap();
        let updated = event["type"] == "acp/session_update";
        published.push(event);
        if updated {
            break;
        }
    }
    let (request_type, resolved_type, id_field) = if permission {
        (
            "acp/permission_request",
            "acp/permission_resolved",
            "permissionId",
        )
    } else {
        (
            "acp/elicitation_request",
            "acp/elicitation_resolved",
            "elicitationId",
        )
    };
    let created = published
        .iter()
        .position(|event| event["type"] == request_type)
        .unwrap();
    let removed = published
        .iter()
        .position(|event| event["type"] == resolved_type)
        .unwrap();
    assert!(created < removed && removed < published.len() - 1);
    assert_eq!(published[created][id_field], allocation);
    assert_eq!(published[removed][id_field], allocation);
    let mut state = services.state.lock().await;
    let view = state.sessions.view(SESSION, incarnation).unwrap();
    assert_eq!(view.baseline.updates().len(), 1);
    assert_eq!(
        view.baseline.updates()[0]["content"]["text"],
        "After the cancellation."
    );
}

async fn run_wire_batch(permission: bool, creation_pending: bool) {
    let (services, mut scheduling, mut events) = setup();
    let mut coordinator = Coordinator::default();
    let incarnation = if creation_pending {
        services.state.lock().await.pending_creations = 1;
        None
    } else {
        Some(install_session(&mut *services.state.lock().await))
    };
    let (outgoing, mut peer_responses) = futures::channel::mpsc::channel::<String>(8);
    let (mut peer_requests, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(8);
    let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
    let (host_done, host_finished) = oneshot::channel();
    let (peer_done, peer_finished) = oneshot::channel();
    let ingress = services.ingress.clone();
    let connection = Client
        .builder()
        .on_receive_dispatch(
            async move |dispatch: Dispatch, _connection| ingress.receive_dispatch(dispatch),
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(transport, async move |connection| {
            let new_owner = services.dispatch_owner.clone();
            let pending = if creation_pending {
                Some(
                    services
                        .ingress
                        .prepare_rpc(RequestClass::Control, new_owner.clone(), None)
                        .unwrap()
                        .send(&connection, NewSessionRequest::new("/workspace"))
                        .unwrap(),
                )
            } else {
                None
            };
            // Route the complete wire batch before executing any handler. This is
            // the deterministic case that lets an SDK marker outrun a watcher task.
            for _ in 0..if creation_pending { 4 } else { 3 } {
                let item = scheduling.ingress_rx.recv().await.unwrap();
                coordinator
                    .route(
                        item,
                        &scheduling,
                        &services.ingress,
                        &services.state,
                        &services.sink,
                    )
                    .await
                    .unwrap();
            }
            let incarnation = if let Some(pending) = pending {
                let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
                let BridgeIngress::RpcCompleted(completion) = event else {
                    panic!("creation response lost its position")
                };
                assert!(
                    scheduling.pump().unwrap().is_none(),
                    "unknown target requests escaped their claim"
                );
                scheduling.handoff_completion(completion, turn).unwrap();
                let (response, turn) = pending.wait().await.unwrap();
                assert_eq!(response.unwrap().session_id.0.as_ref(), SESSION);
                let incarnation = {
                    let mut state = services.state.lock().await;
                    let incarnation = install_session(&mut state);
                    state.pending_creations = 0;
                    incarnation
                };
                services
                    .ingress
                    .finish_creation(new_owner, SESSION.into(), Some(incarnation))
                    .unwrap();
                drop(turn);
                let installed = scheduling.ingress_rx.recv().await.unwrap();
                assert!(matches!(
                    &installed.event,
                    BridgeIngress::CreationFinished { .. }
                ));
                coordinator
                    .route(
                        installed,
                        &scheduling,
                        &services.ingress,
                        &services.state,
                        &services.sink,
                    )
                    .await
                    .unwrap();
                incarnation
            } else {
                incarnation.unwrap()
            };
            assert_interaction_sequence(
                permission,
                incarnation,
                &mut scheduling,
                &connection,
                &services,
                &mut events,
            )
            .await;
            host_done.send(()).unwrap();
            peer_finished.await.unwrap();
            Ok(())
        });
    let peer = async move {
        let mut batch = interaction_batch(permission);
        if creation_pending {
            let request: Value =
                serde_json::from_str(&peer_responses.next().await.unwrap()).unwrap();
            assert_eq!(request["method"], "session/new");
            batch.insert(
                0,
                json!({"jsonrpc":"2.0", "id":request["id"], "result":{"sessionId":SESSION}}),
            );
        }
        peer_requests
            .send(Ok(Value::Array(batch).to_string()))
            .await
            .unwrap();
        let frame: Value = serde_json::from_str(&peer_responses.next().await.unwrap()).unwrap();
        // A batch containing one request and two notifications still receives
        // an array. Notifications (and the creation response) add no reply slots.
        let responses = frame
            .as_array()
            .unwrap_or_else(|| panic!("expected a JSON-RPC batch response, received {frame}"));
        assert_eq!(responses.len(), 1, "unexpected batch reply slots: {frame}");
        let response = &responses[0];
        assert_eq!(
            response["id"], AGENT_REQUEST_ID,
            "wrong reply owner: {frame}"
        );
        assert_eq!(
            response["error"]["code"],
            i32::from(Error::request_cancelled().code),
            "the original request was not cancelled: {frame}"
        );
        host_finished.await.unwrap();
        peer_done.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(
        Duration::from_secs(3),
        futures::future::join(connection, peer),
    )
    .await
    .expect("wire cancellation or following update remained parked");
    result.unwrap();
}

#[tokio::test]
async fn wire_permission_and_elicitation_cancel_precede_the_following_session_update() {
    for permission in [true, false] {
        run_wire_batch(permission, false).await;
    }
}

#[tokio::test]
async fn creation_staged_request_and_cancel_bind_the_same_installed_incarnation() {
    run_wire_batch(true, true).await;
}
