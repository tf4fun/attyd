//! Connection-level allocation before a session has a routable incarnation.
//!
//! Existing owners are reduced by their session queue. Only the first cold
//! attachment is prepared here, synchronously, so an incarnation-0 queue never
//! has to be replaced while it contains accepted observations.

use super::inbound_requests::InboundRequests;
use super::scheduling::{
    BridgeIngress, CapturedRoute, ExecutionTurn, IngressItem, IngressSender, ScheduledInput,
    Scheduling,
};
use super::*;
use crate::ordered_ingress::RequestOwner;
use crate::session_dispatch::SessionHandle;
use agent_client_protocol::{Dispatch, JsonRpcMessage};
use std::collections::VecDeque;

pub(super) struct Coordinator {
    handles: HashMap<String, SessionHandle>,
    creations: HashMap<String, CreationClaim>,
    inbound: InboundRequests,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            handles: HashMap::new(),
            creations: HashMap::new(),
            inbound: InboundRequests::new(),
        }
    }
}

struct CreationClaim {
    owner: RequestOwner,
    // These are after the response cut, not part of the creation replay. Their
    // original ingress budgets stay held until the target is installed or fails.
    following: VecDeque<IngressItem>,
}

/// Begin a queued local transition. RPC/user waits are transferred to owned
/// owners by the handler; the coordinator never waits for their completion.
pub(super) async fn dispatch_ready(
    delivery: ScheduledInput,
    scheduling: &Scheduling,
    connection: &ConnectionTo<Agent>,
    services: &agent_dispatch::AgentContext,
    stopping: bool,
) -> Result<Option<(BridgeInput, ExecutionTurn)>, Error> {
    let ScheduledInput {
        event,
        turn,
        route,
        request_lease,
    } = delivery;
    match event {
        BridgeIngress::Browser(input) if stopping => {
            reject_browser_input(
                &mut *services.state.lock().await,
                &services.sink,
                input,
                Error::request_cancelled(),
            );
        }
        BridgeIngress::Browser(input) => return Ok(Some((input, turn))),
        BridgeIngress::RpcCompleted(completion) => {
            scheduling.handoff_completion(completion, turn)?;
        }
        BridgeIngress::Continuation { reply, .. } => {
            let _ = reply.send(Ok(turn));
        }
        BridgeIngress::Terminal(snapshot) => {
            publish_terminal_snapshot(&mut *services.state.lock().await, &services.sink, snapshot);
        }
        BridgeIngress::Acp(Dispatch::Request(_, responder)) if stopping => {
            drop(request_lease);
            responder.respond_with_error(Error::request_cancelled())?;
        }
        BridgeIngress::Acp(dispatch) => {
            let route = route.ok_or_else(|| {
                Error::internal_error().data("queued ACP event has no captured route")
            })?;
            let context = agent_dispatch::AgentContext {
                owner: route.owner,
                dispatch_owner: route.dispatch_owner,
                url_registration: route.url_registration,
                request_lease,
                ..services.clone()
            };
            let task_connection = connection.clone();
            connection.spawn(async move {
                agent_dispatch::handle_agent_dispatch(
                    dispatch,
                    task_connection,
                    context,
                    Some(turn),
                )
                .await
            })?;
        }
        BridgeIngress::InboundRequestCancelled(capture) => {
            agent_dispatch::cancel_inbound_request(capture, services, Some(turn)).await?;
        }
        BridgeIngress::InboundRequestFinished { .. } => {
            unreachable!("request retirement is handled at ingress")
        }
        BridgeIngress::CreationFinished { .. } => {
            unreachable!("creation cuts are handled at ingress")
        }
    }
    Ok(None)
}

impl Coordinator {
    pub(super) async fn route(
        &mut self,
        item: IngressItem,
        scheduling: &Scheduling,
        ingress: &IngressSender,
        state: &Arc<Mutex<BridgeState>>,
        sink: &EventSink,
    ) -> Result<(), Error> {
        let mut ready = VecDeque::from([item]);
        while let Some(mut item) = ready.pop_front() {
            if let BridgeIngress::InboundRequestFinished {
                request_id,
                allocation,
            } = &item.event
            {
                self.inbound.finish(request_id, allocation);
                continue;
            }
            if let BridgeIngress::Acp(Dispatch::Notification(message)) = &item.event {
                if CancelRequestNotification::matches_method(message.method()) {
                    let notification = match CancelRequestNotification::parse_message(
                        message.method(),
                        message.params(),
                    ) {
                        Ok(notification) => notification,
                        Err(error) => {
                            sink.acp_error(error, None, Some("$/cancel_request"));
                            continue;
                        }
                    };
                    let Some(capture) = self.inbound.capture_cancel(&notification.request_id)
                    else {
                        continue;
                    };
                    item.route = Some(capture.route.clone());
                    item.event = BridgeIngress::InboundRequestCancelled(capture);
                }
            }
            if let BridgeIngress::CreationFinished {
                owner,
                target_id,
                incarnation,
            } = &item.event
            {
                let Some(claim) = self.creations.get(target_id) else {
                    continue;
                };
                if claim.owner != *owner {
                    continue;
                }
                let claim = self.creations.remove(target_id).expect("claim checked");
                let accepted = {
                    let locked = state.lock().await;
                    incarnation.is_some_and(|expected| {
                        expected != 0
                            && locked
                                .sessions
                                .state(target_id)
                                .is_some_and(|session| session.incarnation == expected)
                    })
                };
                if accepted {
                    let installed_owner = state
                        .lock()
                        .await
                        .sessions
                        .resource_owner(target_id, incarnation.expect("accepted incarnation"))
                        .ok();
                    for mut following in claim.following.into_iter().rev() {
                        let mut bound = true;
                        if let Some(route) = &mut following.route {
                            if route.owner.is_none()
                                && route.dispatch_owner.session_id.as_deref() == Some(target_id)
                                && route.dispatch_owner.incarnation.is_none()
                            {
                                route.owner = installed_owner.clone();
                                route.dispatch_owner.incarnation = *incarnation;
                                match &mut following.event {
                                    BridgeIngress::Acp(Dispatch::Request(_, responder)) => {
                                        bound = self.inbound.rebind(
                                            responder.id(),
                                            route
                                                .dispatch_owner
                                                .attempt_id
                                                .as_deref()
                                                .expect("inbound allocation"),
                                            route.clone(),
                                        );
                                    }
                                    BridgeIngress::InboundRequestCancelled(capture) => {
                                        capture.route = route.clone()
                                    }
                                    _ => {}
                                }
                            }
                        }
                        if !bound {
                            reject_unrouted(
                                following,
                                ingress,
                                &Error::request_cancelled()
                                    .data("creation request allocation could not be bound"),
                                sink,
                            )?;
                            continue;
                        }
                        ready.push_front(following);
                    }
                } else {
                    let error = Error::request_cancelled()
                        .data("session creation did not install its target");
                    for following in claim.following {
                        reject_unrouted(following, ingress, &error, sink)?;
                    }
                }
                continue;
            }

            // A response's target claim precedes every following input, even
            // when the source session is busy reducing an earlier notification.
            if let BridgeIngress::RpcCompleted(completion) = &mut item.event {
                let locked = state.lock().await;
                if !owns_request(&locked, &completion.owner) {
                    drop(locked);
                    if let BridgeIngress::RpcCompleted(completion) = item.event {
                        ingress.discard_completion(completion)?;
                    }
                    continue;
                }
                if matches!(completion.method.as_str(), "session/new" | "session/fork") {
                    if let Some(target_id) = completion
                        .result
                        .as_ref()
                        .ok()
                        .and_then(|response| response.get("sessionId"))
                        .and_then(Value::as_str)
                        .filter(|id| validate_agent_session_id(id).is_ok())
                        .map(str::to_owned)
                    {
                        let occupied = locked.sessions.state(&target_id).is_some_and(|session| {
                            session.operation.is_some()
                                || session
                                    .live
                                    .as_ref()
                                    .is_some_and(|live| live.lifecycle != SessionLifecycle::Closed)
                        });
                        if occupied
                            || locked.catalog_deletions.contains_key(&target_id)
                            || self
                                .creations
                                .get(&target_id)
                                .is_some_and(|claim| claim.owner != completion.owner)
                        {
                            completion.result = Err(Error::invalid_request().data(
                                "Agent returned a session ID already owned by another allocation",
                            ));
                        } else {
                            self.creations
                                .entry(target_id)
                                .or_insert_with(|| CreationClaim {
                                    owner: completion.owner.clone(),
                                    following: VecDeque::new(),
                                });
                        }
                    }
                }
            }

            let requested_id = {
                let locked = state.lock().await;
                event_session_id(&item.event, &locked)
            };
            if let Some(claim) = requested_id
                .as_deref()
                .and_then(|id| self.creations.get_mut(id))
            {
                // Fork completion belongs to its source, and New completion is
                // global. Neither completion is held behind its own target claim.
                if !matches!(
                    item.event,
                    BridgeIngress::RpcCompleted(_)
                        | BridgeIngress::Acp(Dispatch::Request(..))
                        | BridgeIngress::InboundRequestCancelled(_)
                ) {
                    claim.following.push_back(item);
                    continue;
                }
            }

            if matches!(item.event, BridgeIngress::Browser(_)) {
                let event = std::mem::replace(
                    &mut item.event,
                    BridgeIngress::Browser(BridgeInput::RuntimeSnapshotRequest),
                );
                let BridgeIngress::Browser(input) = event else {
                    unreachable!()
                };
                let prepared = prepare_cold_input(&mut *state.lock().await, sink, input);
                let Some(input) = prepared else { continue };
                item.event = BridgeIngress::Browser(input);
            }

            let (mut route, mut incarnation) = if let Some(route) = item.route.clone() {
                let incarnation = route.dispatch_owner.incarnation;
                (route, incarnation)
            } else {
                let locked = state.lock().await;
                let requested_id = event_session_id(&item.event, &locked);
                let incarnation = requested_id
                    .as_deref()
                    .and_then(|id| locked.sessions.state(id))
                    .map(|session| session.incarnation)
                    .filter(|incarnation| *incarnation != 0);
                let expected = match &item.event {
                    BridgeIngress::RpcCompleted(completion) => Some(completion.owner.clone()),
                    BridgeIngress::Continuation { owner, .. } => Some(owner.clone()),
                    BridgeIngress::Terminal(snapshot) => Some(RequestOwner {
                        epoch: locked.sessions.epoch().to_string(),
                        session_id: requested_id.clone(),
                        incarnation: Some(snapshot.incarnation),
                        operation_id: "terminal-snapshot".to_string(),
                        attempt_id: None,
                    }),
                    _ => None,
                };
                if expected
                    .as_ref()
                    .is_some_and(|owner| !owns_request(&locked, owner))
                {
                    drop(locked);
                    reject_unrouted(
                        item,
                        ingress,
                        &Error::request_cancelled().data("session owner was replaced"),
                        sink,
                    )?;
                    continue;
                }
                let owner =
                    requested_id
                        .as_deref()
                        .zip(incarnation)
                        .and_then(|(id, incarnation)| {
                            locked.sessions.resource_owner(id, incarnation).ok()
                        });
                let dispatch_owner = expected.unwrap_or_else(|| RequestOwner {
                    epoch: locked.sessions.epoch().to_string(),
                    session_id: owner.as_ref().map(|owner| owner.session_id.clone()),
                    incarnation: owner.as_ref().map(|owner| owner.incarnation),
                    operation_id: event_operation_id(&item.event),
                    attempt_id: Some(Uuid::new_v4().to_string()),
                });
                let url_registration = match &item.event {
                    BridgeIngress::Acp(Dispatch::Notification(message))
                        if CompleteElicitationNotification::matches_method(message.method()) =>
                    {
                        message
                            .params()
                            .get("elicitationId")
                            .and_then(Value::as_str)
                            .and_then(|id| locked.sessions.url_owners.get(id))
                            .cloned()
                    }
                    _ => None,
                };
                (
                    CapturedRoute {
                        owner,
                        dispatch_owner,
                        url_registration,
                    },
                    incarnation,
                )
            };

            if matches!(item.event, BridgeIngress::Acp(Dispatch::Request(..)))
                && item.request_lease.is_none()
            {
                if let Some(target_id) = requested_id
                    .as_deref()
                    .filter(|id| self.creations.contains_key(*id))
                {
                    route.owner = None;
                    route.dispatch_owner.session_id = Some(target_id.to_owned());
                    route.dispatch_owner.incarnation = None;
                    incarnation = None;
                }
                if let BridgeIngress::Acp(Dispatch::Request(message, responder)) = &item.event {
                    match self.inbound.register(
                        responder.id().clone(),
                        message.method(),
                        &route,
                        ingress.clone(),
                    ) {
                        Ok(lease) => item.request_lease = Some(lease),
                        Err(error) => {
                            reject_unrouted(item, ingress, &error, sink)?;
                            continue;
                        }
                    }
                }
            }
            if matches!(
                item.event,
                BridgeIngress::Acp(Dispatch::Request(..))
                    | BridgeIngress::InboundRequestCancelled(_)
            ) {
                let target_id = route.dispatch_owner.session_id.as_deref();
                if let Some(claim) = target_id.and_then(|id| self.creations.get_mut(id)) {
                    item.route = Some(route);
                    claim.following.push_back(item);
                    continue;
                }
            }

            if let Some(owner) = &route.owner {
                if !state.lock().await.sessions.owns_resources(owner) {
                    reject_unrouted(
                        item,
                        ingress,
                        &Error::request_cancelled()
                            .data("captured Agent request owner was retired"),
                        sink,
                    )?;
                    continue;
                }
            }

            // Before an ID is returned, replay is a connection allocation
            // concern. Reduce this prefix now, before reading the next response.
            if incarnation.is_none()
                && matches!(&item.event,
                BridgeIngress::Acp(Dispatch::Notification(message)) if SessionNotification::matches_method(message.method()))
            {
                let BridgeIngress::Acp(dispatch) = item.event else {
                    unreachable!()
                };
                match dispatch.into_notification::<SessionNotification>() {
                    Ok(Ok(notification)) => {
                        handle_session_update(notification, state, sink, None).await
                    }
                    Err(error) => sink.acp_error(error, None, Some("session/update")),
                    Ok(Err(_)) => unreachable!("matched session notification"),
                }
                continue;
            }
            let handle = if let Some(owner) = &route.owner {
                match self.handles.get(&owner.session_id) {
                    Some(handle) if handle.incarnation() == owner.incarnation => {
                        Some(handle.clone())
                    }
                    _ => {
                        if let Some(previous) = self.handles.remove(&owner.session_id) {
                            scheduling.remove_session(&previous)?;
                        }
                        let handle =
                            scheduling.register_session(&owner.session_id, owner.incarnation)?;
                        self.handles
                            .insert(owner.session_id.clone(), handle.clone());
                        Some(handle)
                    }
                }
            } else {
                None
            };
            item.route = Some(route);
            let routed = if let Some(handle) = handle {
                scheduling.route_session(&handle, item)
            } else {
                scheduling.route_global(item)
            };
            if let Err(rejected) = routed {
                if let BridgeIngress::Browser(BridgeInput::PreparedAttachment {
                    command,
                    reservation,
                    ..
                }) = &rejected.item.event
                {
                    reject_prepared_attachment(
                        &mut *state.lock().await,
                        sink,
                        command,
                        reservation,
                        &rejected.error,
                    );
                }
                reject_unrouted(rejected.item, ingress, &rejected.error, sink)?;
            }
        }
        Ok(())
    }

    pub(super) fn retire_missing(
        &mut self,
        state: &BridgeState,
        scheduling: &Scheduling,
    ) -> Result<(), Error> {
        let mut retired = Vec::new();
        for (id, handle) in &self.handles {
            let session = state.sessions.state(id);
            let replaced =
                session.is_none_or(|session| session.incarnation != handle.incarnation());
            // Failed cold materialization keeps a queryable Blocked entry, but
            // an idle history-only entry must not permanently occupy a queue.
            let dormant = session
                .is_some_and(|session| session.live.is_none() && session.operation.is_none())
                && scheduling.session_is_idle(handle)?;
            if replaced || dormant {
                retired.push((id.clone(), handle.clone()));
            }
        }
        for (id, handle) in retired {
            scheduling.remove_session(&handle)?;
            self.handles.remove(&id);
        }
        Ok(())
    }

    pub(super) fn close(&mut self, ingress: &IngressSender, sink: &EventSink) -> Result<(), Error> {
        let error =
            Error::request_cancelled().data("connection closed before session creation completed");
        let mut first_error = None;
        for claim in std::mem::take(&mut self.creations).into_values() {
            for item in claim.following {
                if let Err(error) = reject_unrouted(item, ingress, &error, sink) {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn owns_request(state: &BridgeState, owner: &RequestOwner) -> bool {
    owner.epoch == state.sessions.epoch()
        && match (&owner.session_id, owner.incarnation) {
            (Some(id), Some(incarnation)) => state
                .sessions
                .state(id)
                .is_some_and(|session| session.incarnation == incarnation),
            (None, None) => true,
            _ => false,
        }
}

fn event_operation_id(event: &BridgeIngress) -> String {
    match event {
        BridgeIngress::Browser(
            BridgeInput::BusinessRequest { command, .. }
            | BridgeInput::PreparedAttachment { command, .. },
        ) => command
            .get("requestId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        BridgeIngress::Browser(BridgeInput::TurnRequest {
            client_intent_id, ..
        }) => Some(client_intent_id.clone()),
        BridgeIngress::Acp(dispatch) => dispatch.id().and_then(|id| serde_json::to_string(id).ok()),
        _ => None,
    }
    .unwrap_or_else(|| format!("bridge-local-{}", Uuid::new_v4()))
}

fn event_session_id(event: &BridgeIngress, state: &BridgeState) -> Option<String> {
    match event {
        BridgeIngress::Browser(input) => input.session_id(state).map(str::to_owned),
        BridgeIngress::RpcCompleted(completion) => completion.owner.session_id.clone(),
        BridgeIngress::Continuation { owner, .. } => owner.session_id.clone(),
        BridgeIngress::InboundRequestCancelled(capture) => {
            capture.route.dispatch_owner.session_id.clone()
        }
        BridgeIngress::InboundRequestFinished { .. } => None,
        BridgeIngress::CreationFinished { .. } => None,
        BridgeIngress::Terminal(snapshot) => snapshot
            .value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        BridgeIngress::Acp(dispatch) => match dispatch {
            Dispatch::Notification(message)
                if CompleteElicitationNotification::matches_method(message.method()) =>
            {
                message
                    .params()
                    .get("elicitationId")
                    .and_then(Value::as_str)
                    .and_then(|id| state.sessions.url_owners.get(id))
                    .and_then(|registration| registration.owner.as_ref())
                    .map(|owner| owner.session_id.clone())
            }
            Dispatch::Request(message, _) | Dispatch::Notification(message) => {
                // MCP connections are owned by the connection, regardless of
                // IDs nested inside an MCP payload.
                if ConnectMcpRequest::matches_method(message.method())
                    || MessageMcpRequest::matches_method(message.method())
                    || MessageMcpNotification::matches_method(message.method())
                    || DisconnectMcpRequest::matches_method(message.method())
                {
                    None
                } else {
                    message
                        .params()
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }
            }
            Dispatch::Response(..) => None,
        },
    }
}

fn reject_unrouted(
    item: IngressItem,
    ingress: &IngressSender,
    error: &Error,
    sink: &EventSink,
) -> Result<(), Error> {
    drop(item.request_lease);
    match item.event {
        BridgeIngress::Browser(input) => input.reject(error.clone()),
        BridgeIngress::Acp(Dispatch::Request(_, responder)) => {
            responder.respond_with_error(error.clone())?
        }
        BridgeIngress::Acp(Dispatch::Notification(message)) => {
            sink.acp_error(error.clone(), None, Some(message.method()))
        }
        BridgeIngress::Acp(Dispatch::Response(result, router)) => {
            router.route_with_result(result)?
        }
        BridgeIngress::RpcCompleted(completion) => ingress.discard_completion(completion)?,
        BridgeIngress::Continuation { reply, .. } => {
            let _ = reply.send(Err(error.clone()));
        }
        BridgeIngress::Terminal(_)
        | BridgeIngress::CreationFinished { .. }
        | BridgeIngress::InboundRequestCancelled(_)
        | BridgeIngress::InboundRequestFinished { .. } => {}
    }
    Ok(())
}

/// Reject a queued command before releasing its local execution ticket. Cold
/// preparation already owns a real allocation and possibly GET/Observe waiters.
fn reject_browser_input(
    state: &mut BridgeState,
    sink: &EventSink,
    input: BridgeInput,
    error: Error,
) {
    if let BridgeInput::PreparedAttachment {
        command,
        reservation,
        ..
    } = &input
    {
        reject_prepared_attachment(state, sink, command, reservation, &error);
    }
    input.reject(error);
}

pub(super) fn reject_prepared_attachment(
    state: &mut BridgeState,
    sink: &EventSink,
    command: &Value,
    prepared: &PreparedAttachment,
    error: &Error,
) {
    let Some(session_id) = command.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let kind = if command["type"] == "session/load" {
        RuntimeSessionOperationKind::Load
    } else {
        RuntimeSessionOperationKind::Resume
    };
    let reservation = &prepared.reservation;
    if state.sessions.state(session_id).is_none_or(|session| {
        session.incarnation != reservation.incarnation()
            || session.operation.as_ref().is_none_or(|operation| {
                operation.operation_id != prepared.operation_id || operation.kind != kind
            })
    }) {
        return;
    }
    let validation = replay_validation_backup(state, session_id).map(|backup| backup.owner.clone());
    if fail_runtime_attachment(
        state,
        session_id,
        reservation.incarnation(),
        &prepared.operation_id,
        kind,
        reservation.is_reload(),
        error,
    )
    .is_err()
    {
        return;
    }
    if !reservation.is_reload() {
        state.sessions.remove(session_id, reservation.incarnation());
    }
    if let Some(validation) = validation {
        clear_attachment_tracking(state, session_id, &validation, false);
    }
    if let Some(materialization_id) = command
        .get("bridgeMaterializationId")
        .and_then(Value::as_str)
    {
        resolve_session_view_waiters_locked(
            state,
            sink,
            session_id,
            materialization_id,
            &Err(error.clone()),
        );
    }
    flush_runtime(state, sink);
}

pub(super) fn prepare_cold_input(
    state: &mut BridgeState,
    sink: &EventSink,
    input: BridgeInput,
) -> Option<BridgeInput> {
    let Some(session_id) = input.session_id(state).map(str::to_owned) else {
        return Some(input);
    };
    if state
        .sessions
        .state(&session_id)
        .is_some_and(|owner| owner.incarnation != 0)
    {
        return Some(input);
    }

    let (command, response) = match input {
        BridgeInput::SessionViewRequest {
            session_id,
            cwd,
            expected_owner,
            response,
        } => (
            prepare_session_view_request_for_owner(
                state,
                sink,
                session_id,
                cwd,
                expected_owner,
                SessionViewWaiter::View(response),
            )?,
            None,
        ),
        BridgeInput::ObserveSession {
            session_id,
            cwd,
            expected_owner,
            observer_id,
            lease,
            reply,
        } => (
            prepare_session_view_request_for_owner(
                state,
                sink,
                session_id,
                cwd,
                expected_owner,
                SessionViewWaiter::Observe {
                    observer_id,
                    lease,
                    reply,
                },
            )?,
            None,
        ),
        BridgeInput::BusinessRequest { command, response }
            if matches!(
                command.get("type").and_then(Value::as_str),
                Some("session/load" | "session/resume")
            ) =>
        {
            (command, Some(response))
        }
        input => return Some(input),
    };

    let reserved = (|| {
        let operation = nonempty_string_field(&command, "type")?;
        let operation_id = string_field(&command, "requestId")?.to_string();
        let session_id = string_field(&command, "sessionId")?;
        let kind = if operation == "session/load" {
            RuntimeSessionOperationKind::Load
        } else {
            RuntimeSessionOperationKind::Resume
        };
        let supported = state.agent_capabilities.as_ref().is_some_and(|caps| {
            if kind == RuntimeSessionOperationKind::Load {
                caps.load_session
            } else {
                caps.session_capabilities.resume.is_some()
            }
        });
        if !supported {
            return Err(Error::method_not_found()
                .data(format!("Agent did not advertise support for {operation}")));
        }
        let reservation = reserve_attachment_locked(
            state,
            session_id,
            kind,
            command.get("cwd").and_then(Value::as_str),
            &operation_id,
            command
                .get("bridgeMaterializationId")
                .and_then(Value::as_str),
        )?;
        Ok(PreparedAttachment {
            operation_id,
            reservation,
        })
    })();

    match reserved {
        Ok(reservation) => Some(BridgeInput::PreparedAttachment {
            command,
            reservation,
            response,
        }),
        Err(error) => {
            if let Some(materialization_id) = command
                .get("bridgeMaterializationId")
                .and_then(Value::as_str)
            {
                resolve_session_view_waiters_locked(
                    state,
                    sink,
                    &session_id,
                    materialization_id,
                    &Err(error.clone()),
                );
            }
            if let Some(response) = response {
                let _ = response.send(Err(BridgeRequestError::from_acp(&error)));
            }
            None
        }
    }
}

#[cfg(test)]
mod prepared_rejection_tests {
    use super::*;

    fn setup() -> (BridgeState, EventSink, mpsc::UnboundedReceiver<String>) {
        let (events, received) = mpsc::unbounded_channel();
        (
            BridgeState {
                agent_capabilities: Some(AgentCapabilities::new().load_session(true)),
                ..BridgeState::default()
            },
            EventSink { tx: events.into() },
            received,
        )
    }

    fn business(
        state: &mut BridgeState,
        sink: &EventSink,
        request_id: &str,
    ) -> (
        Value,
        PreparedAttachment,
        oneshot::Sender<Result<Value, BridgeRequestError>>,
        oneshot::Receiver<Result<Value, BridgeRequestError>>,
    ) {
        let (response, received) = oneshot::channel();
        let input = prepare_cold_input(
            state,
            sink,
            BridgeInput::BusinessRequest {
                command: json!({"type":"session/load", "requestId":request_id,
                "sessionId":"cold", "cwd":"/tmp"}),
                response,
            },
        )
        .expect("cold input must be prepared");
        let BridgeInput::PreparedAttachment {
            command,
            reservation,
            response,
        } = input
        else {
            panic!("expected prepared attachment")
        };
        (command, reservation, response.unwrap(), received)
    }

    #[test]
    fn duplicate_global_request_id_releases_the_prepared_cold_allocation() {
        let (mut state, sink, _events) = setup();
        state.in_flight_request_ids.insert("dup".into()); // an outstanding global list
        let (command, prepared, response, mut received) = business(&mut state, &sink, "dup");
        let incarnation = prepared.reservation.incarnation();
        assert!(incarnation > 0);
        assert_eq!(state.sessions.allocated_session_count(), 1);
        let error = reserve_command_request_id(&mut state, &sink, "dup", &command, Some(&prepared))
            .expect_err("global request ID collision must reject the cold command");
        BusinessResponder::new(response).error(&error);
        assert!(received.try_recv().unwrap().is_err());
        assert!(
            state.in_flight_request_ids.contains("dup"),
            "the original list still owns its ID"
        );
        assert_eq!(state.sessions.allocated_session_count(), 0);
        assert!(
            state.sessions.state("cold").is_none(),
            "rejected load cannot retain a busy owner"
        );
        let (_, retry, _, _) = business(&mut state, &sink, "retry");
        assert_ne!(retry.reservation.incarnation(), incarnation);
    }

    #[test]
    fn stopping_prepared_materialization_resolves_view_and_observe_before_retirement() {
        let (mut state, sink, _events) = setup();
        let (response, mut view) = oneshot::channel();
        let input = prepare_cold_input(
            &mut state,
            &sink,
            BridgeInput::SessionViewRequest {
                expected_owner: None,
                session_id: "cold".into(),
                cwd: Some("/tmp".into()),
                response,
            },
        )
        .expect("first view prepares the actual attachment");
        let lease = ObservationLease::new();
        let (reply, mut observe) = oneshot::channel();
        assert!(
            prepare_session_view_request(
                &mut state,
                &sink,
                "cold".into(),
                Some("/tmp".into()),
                SessionViewWaiter::Observe {
                    observer_id: 7,
                    lease: lease.clone(),
                    reply
                }
            )
            .is_none()
        );
        let cancellation = session_materialization(&state, "cold")
            .unwrap()
            .cancellation
            .clone();
        reject_browser_input(
            &mut state,
            &sink,
            input,
            Error::request_cancelled().data("stopped before attachment dispatch"),
        );
        for result in [
            view.try_recv().unwrap().map(|_| ()),
            observe.try_recv().unwrap(),
        ] {
            assert!(
                matches!(result, Err(SessionViewError::Unavailable(message))
                if message.contains("stopped before attachment dispatch")),
                "joined waiters receive the specific rejection, not a dropped-sender error"
            );
        }
        assert!(lease.is_cancelled());
        assert!(cancellation.is_cancelled());
        assert!(state.sessions.state("cold").is_none());
        assert_eq!(state.sessions.allocated_session_count(), 0);
    }

    #[test]
    fn stale_prepared_rejection_preserves_a_replacement_attachment() {
        let (mut state, sink, _events) = setup();
        let (old_command, old, _, _) = business(&mut state, &sink, "old");
        let rejected = Error::request_cancelled();
        reject_prepared_attachment(&mut state, &sink, &old_command, &old, &rejected);
        let (_, new, _, _) = business(&mut state, &sink, "new");
        let allocation = session_validation(&state, "cold")
            .unwrap()
            .allocation
            .clone();
        reject_prepared_attachment(&mut state, &sink, &old_command, &old, &rejected);
        let current = state.sessions.state("cold").unwrap();
        assert_eq!(current.incarnation, new.reservation.incarnation());
        assert_eq!(current.operation.as_ref().unwrap().operation_id, "new");
        assert!(Arc::ptr_eq(
            &allocation,
            &session_validation(&state, "cold").unwrap().allocation
        ));
        assert_eq!(state.sessions.allocated_session_count(), 1);
    }
}

#[cfg(test)]
mod blocked_queue_retirement_tests {
    use super::*;
    use crate::session_dispatch::TrafficClass;

    enum Waiter {
        View(oneshot::Receiver<Result<Value, SessionViewError>>),
        Observe(
            oneshot::Receiver<Result<(), SessionViewError>>,
            ObservationLease,
        ),
    }

    #[test]
    fn failed_cold_materializations_release_idle_queues_but_preserve_blocked_views() {
        let (events, _received) = mpsc::unbounded_channel();
        let sink = EventSink { tx: events.into() };
        let mut state = BridgeState {
            agent_capabilities: Some(AgentCapabilities::new().load_session(true)),
            ..BridgeState::default()
        };
        let (mut scheduling, ingress) = Scheduling::new(
            state.sessions.epoch().to_string(),
            sink.clone(),
            CancellationToken::new(),
        )
        .unwrap();
        let mut coordinator = Coordinator::default();
        for index in 0..34 {
            let session_id = format!("failed-cold-{index}");
            let (input, mut waiter) = if index % 2 == 0 {
                let (response, received) = oneshot::channel();
                (
                    BridgeInput::SessionViewRequest {
                        expected_owner: None,
                        session_id: session_id.clone(),
                        cwd: Some("/tmp".into()),
                        response,
                    },
                    Waiter::View(received),
                )
            } else {
                let (reply, received) = oneshot::channel();
                let lease = ObservationLease::new();
                (
                    BridgeInput::ObserveSession {
                        expected_owner: None,
                        session_id: session_id.clone(),
                        cwd: Some("/tmp".into()),
                        observer_id: index as u64,
                        lease: lease.clone(),
                        reply,
                    },
                    Waiter::Observe(received, lease),
                )
            };
            let input = prepare_cold_input(&mut state, &sink, input).unwrap();
            let incarnation = state.sessions.state(&session_id).unwrap().incarnation;
            let handle = scheduling
                .register_session(&session_id, incarnation)
                .expect("prior blocked records must not consume dispatch capacity");
            coordinator
                .handles
                .insert(session_id.clone(), handle.clone());
            ingress.try_browser(input, TrafficClass::Ordinary).unwrap();
            let item = scheduling.ingress_rx.try_recv().unwrap();
            scheduling
                .route_session(&handle, item)
                .unwrap_or_else(|_| panic!("route failed"));
            let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
            let BridgeIngress::Browser(BridgeInput::PreparedAttachment {
                command,
                reservation,
                ..
            }) = event
            else {
                panic!("expected actual managed attachment preparation")
            };
            let request_id = &reservation.operation_id;
            state
                .sessions
                .begin_load(&session_id, incarnation, request_id)
                .unwrap();
            let validation_owner = replay_validation_backup(&state, &session_id)
                .unwrap()
                .owner
                .clone();
            let error = Error::invalid_params().data("deterministic cold replay failure");
            fail_runtime_attachment(
                &mut state,
                &session_id,
                incarnation,
                request_id,
                RuntimeSessionOperationKind::Load,
                false,
                &error,
            )
            .unwrap();
            fail_mirror_attachment(
                &mut state,
                &session_id,
                incarnation,
                request_id,
                false,
                true,
                false,
                &error.message,
            );
            clear_attachment_tracking(&mut state, &session_id, &validation_owner, true);
            resolve_session_view_waiters_locked(
                &mut state,
                &sink,
                &session_id,
                command["bridgeMaterializationId"].as_str().unwrap(),
                &Err(error),
            );
            let failed = state.sessions.state(&session_id).unwrap();
            assert_eq!(failed.phase, MirrorPhase::Blocked);
            assert!(failed.live.is_none() && failed.operation.is_none());
            match &mut waiter {
                Waiter::View(received) => assert!(matches!(
                    received.try_recv().unwrap(),
                    Err(SessionViewError::Unavailable(_))
                )),
                Waiter::Observe(received, lease) => {
                    assert!(matches!(
                        received.try_recv().unwrap(),
                        Err(SessionViewError::Unavailable(_))
                    ));
                    assert!(lease.is_cancelled());
                }
            }
            coordinator.retire_missing(&state, &scheduling).unwrap();
            assert!(
                coordinator.handles.contains_key(&session_id),
                "a result still owns the execution ticket"
            );
            let (response, mut view) = oneshot::channel();
            ingress
                .try_browser(
                    BridgeInput::SessionViewRequest {
                        expected_owner: None,
                        session_id: session_id.clone(),
                        cwd: None,
                        response,
                    },
                    TrafficClass::Ordinary,
                )
                .unwrap();
            let queued = scheduling.ingress_rx.try_recv().unwrap();
            scheduling
                .route_session(&handle, queued)
                .unwrap_or_else(|_| panic!("view route failed"));
            drop(turn);
            coordinator.retire_missing(&state, &scheduling).unwrap();
            assert!(
                coordinator.handles.contains_key(&session_id),
                "an already queued view retains its route"
            );
            let ScheduledInput { event, turn, .. } = scheduling.pump().unwrap().unwrap();
            let BridgeIngress::Browser(BridgeInput::SessionViewRequest {
                session_id: requested,
                cwd,
                response,
                ..
            }) = event
            else {
                panic!("queued view disappeared")
            };
            assert!(
                prepare_session_view_request(
                    &mut state,
                    &sink,
                    requested,
                    cwd,
                    SessionViewWaiter::View(response)
                )
                .is_none()
            );
            assert_eq!(
                view.try_recv().unwrap().unwrap()["session"]["phase"],
                "blocked"
            );
            drop(turn);
            coordinator.retire_missing(&state, &scheduling).unwrap();
            assert!(!coordinator.handles.contains_key(&session_id));
            assert_eq!(
                state.sessions.state(&session_id).unwrap().phase,
                MirrorPhase::Blocked
            );
            assert_eq!(state.sessions.allocated_session_count(), 0);
        }
        assert!(scheduling.register_session("healthy", 1).is_ok());
    }
}
