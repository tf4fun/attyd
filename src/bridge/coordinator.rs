//! Connection-level allocation before a session is in canonical state.
//!
//! Existing owners are reduced by their session queue.

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
        BridgeIngress::Browser(input) if stopping => input.reject(Error::request_cancelled()),
        BridgeIngress::Browser(input) => return Ok(Some((input, turn))),
        BridgeIngress::RpcCompleted(completion) => {
            scheduling.handoff_completion(completion, turn)?;
        }
        BridgeIngress::Continuation { reply, .. } => {
            let _ = reply.send(Ok(turn));
        }
        BridgeIngress::Terminal(snapshot) => {
            publish_terminal_snapshot(&mut *services.state.lock().await, snapshot);
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
                request_scope: route.request_scope,
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
        state: &Arc<BridgeLock>,
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
                committed,
            } = &item.event
            {
                let Some(claim) = self.creations.get(target_id) else {
                    continue;
                };
                if claim.owner != *owner {
                    continue;
                }
                let claim = self.creations.remove(target_id).expect("claim checked");
                let installed_owner = if *committed {
                    state.lock().await.sessions.resource_owner(target_id).ok()
                } else {
                    None
                };
                if let Some(installed_owner) = installed_owner {
                    for mut following in claim.following.into_iter().rev() {
                        let mut bound = true;
                        if let Some(route) = &mut following.route {
                            if route.owner.is_none()
                                && route.dispatch_owner.session_id.as_deref() == Some(target_id)
                            {
                                route.owner = Some(installed_owner.clone());
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

            let mut route = if let Some(route) = item.route.clone() {
                route
            } else {
                let locked = state.lock().await;
                // A request-scoped elicitation of a session's request runs in
                // that session's order, before the request's response.
                let requested_id =
                    event_session_id(&item.event, &locked).or_else(|| match &item.request_scope {
                        Some(OpScope::Session(session_id)) => Some(session_id.clone()),
                        _ => None,
                    });
                let owner = requested_id
                    .as_deref()
                    .and_then(|id| locked.sessions.resource_owner(id).ok());
                let expected = match &item.event {
                    BridgeIngress::RpcCompleted(completion) => Some(completion.owner.clone()),
                    BridgeIngress::Continuation { owner, .. } => Some(owner.clone()),
                    BridgeIngress::Terminal(_) => Some(RequestOwner {
                        epoch: locked.sessions.epoch().to_string(),
                        session_id: requested_id.clone(),
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
                        &Error::request_cancelled().data("session owner was removed"),
                        sink,
                    )?;
                    continue;
                }
                let dispatch_owner = expected.unwrap_or_else(|| RequestOwner {
                    epoch: locked.sessions.epoch().to_string(),
                    session_id: owner.as_ref().map(|owner| owner.session_id.clone()),
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
                CapturedRoute {
                    owner,
                    dispatch_owner,
                    url_registration,
                    request_scope: item.request_scope.clone(),
                }
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
            if route.owner.is_none()
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
                    Some(handle) => Some(handle.clone()),
                    None => {
                        let handle = scheduling.register_session(&owner.session_id)?;
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
            let removed = session.is_none();
            // Failed cold materialization keeps a queryable Blocked entry, but
            // an idle history-only entry must not permanently occupy a queue.
            let dormant = session
                .is_some_and(|session| session.live.is_none() && session.operation.is_none())
                && scheduling.session_is_idle(handle)?;
            if removed || dormant {
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
        && owner
            .session_id
            .as_deref()
            .is_none_or(|id| state.sessions.state(id).is_some())
}

fn event_operation_id(event: &BridgeIngress) -> String {
    match event {
        BridgeIngress::Acp(dispatch) => dispatch.id().and_then(|id| serde_json::to_string(id).ok()),
        _ => None,
    }
    .unwrap_or_else(|| format!("bridge-local-{}", Uuid::new_v4()))
}

fn event_session_id(event: &BridgeIngress, state: &BridgeState) -> Option<String> {
    match event {
        BridgeIngress::Browser(input) => input.session_id().map(str::to_owned),
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
