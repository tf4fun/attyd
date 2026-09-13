//! Typed Agent callbacks after admission at the shared ingress boundary.
//!
//! Request parsing/registration errors are protocol replies, not dispatcher
//! failures. Keep the responder until registration and task spawning succeed;
//! only then transfer it to the task which waits for the user or external I/O.

use super::inbound_requests::{CapturedInboundRequest, InboundRequestKind, InboundRequestLease};
use super::scheduling::{ExecutionTurn, IngressSender};
use super::*;
use crate::ordered_ingress::RequestOwner;
use agent_client_protocol::{
    Dispatch, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, Responder, UntypedMessage,
};

#[derive(Clone)]
pub(super) struct AgentContext {
    pub(super) state: Arc<Mutex<BridgeState>>,
    pub(super) sink: EventSink,
    pub(super) terminals: Option<TerminalManager>,
    pub(super) filesystem: Option<Arc<WorkspaceFileSystem>>,
    pub(super) mcp: McpManager,
    pub(super) ingress: IngressSender,
    // Captured by the coordinator before queueing; never recapture a replacement
    // incarnation merely because an external operation completed late.
    pub(super) owner: Option<SessionResourceOwner>,
    pub(super) dispatch_owner: RequestOwner,
    pub(super) url_registration: Option<UrlRegistration>,
    pub(super) request_lease: Option<InboundRequestLease>,
}

impl AgentContext {
    fn require_owner(
        &self,
        state: &BridgeState,
        session_id: &str,
    ) -> Result<SessionResourceOwner, Error> {
        self.owner
            .as_ref()
            .filter(|owner| owner.session_id == session_id)
            .filter(|owner| state.sessions.owns_resources(owner))
            .filter(|owner| live_session_incarnation(state, session_id) == Some(owner.incarnation))
            .cloned()
            .ok_or_else(|| {
                Error::invalid_params().data(format!(
                    "unknown, inactive, or retired session: {session_id}"
                ))
            })
    }

    async fn continue_local(&self) -> Result<ExecutionTurn, Error> {
        self.ingress
            .continue_owner(self.dispatch_owner.clone())
            .await
    }

    async fn workspace(
        &self,
        session_id: &str,
    ) -> Result<(SessionResourceOwner, WorkspaceFileSystem), Error> {
        let filesystem = self.filesystem.as_ref().ok_or_else(|| {
            Error::method_not_found()
                .data("filesystem methods are unavailable for remote transports")
        })?;
        let owner = self.require_owner(&*self.state.lock().await, session_id)?;
        let (incarnation, workspace) =
            require_session_workspace(session_id, &self.state, filesystem).await?;
        if incarnation != owner.incarnation {
            return Err(Error::request_cancelled().data("session workspace owner changed"));
        }
        Ok((owner, workspace))
    }
}

/// The lease is declared first so abnormal task drop queues cleanup before the
/// SDK responder's fallback reply. Normal replies explicitly preserve that order.
struct RequestResponder<T: JsonRpcResponse> {
    lease: Option<InboundRequestLease>,
    responder: Responder<T>,
}

impl RequestResponder<Value> {
    fn cast<R: JsonRpcResponse>(self) -> RequestResponder<R> {
        RequestResponder {
            lease: self.lease,
            responder: self.responder.cast(),
        }
    }
}

impl<T: JsonRpcResponse> RequestResponder<T> {
    fn cancellation(&self) -> agent_client_protocol::RequestCancellation {
        self.responder.cancellation()
    }

    fn respond_with_result(self, response: Result<T, Error>) -> Result<(), Error> {
        let Self { lease, responder } = self;
        // A peer can reuse its ID immediately after receiving this reply.
        // Cleanup must already be in the same ingress queue at that point.
        drop(lease);
        responder.respond_with_result(response)
    }

    fn respond_with_error(self, error: Error) -> Result<(), Error> {
        self.respond_with_result(Err(error))
    }
}

type Reply<T> = Option<RequestResponder<T>>;

/// Unlike the SDK handler macro, this dispatcher must explicitly reply on errors
/// before the responder has been transferred to an external waiting task.
async fn typed_request<R: JsonRpcRequest>(
    request: UntypedMessage,
    responder: RequestResponder<Value>,
    handler: impl AsyncFnOnce(R, &mut Reply<R::Response>) -> Result<(), Error>,
) -> Result<(), Error> {
    let request = match R::parse_message(request.method(), request.params()) {
        Ok(request) => request,
        Err(error) => return responder.respond_with_error(error),
    };
    let mut responder = Some(responder.cast());
    let result = handler(request, &mut responder).await;
    match (result, responder) {
        (Err(error), Some(responder)) => responder.respond_with_error(error),
        (Ok(()), Some(responder)) => responder.respond_with_error(
            Error::internal_error().data("Agent request handler did not transfer its responder"),
        ),
        (result, None) => result,
    }
}

fn respond<T: JsonRpcResponse>(
    responder: &mut Reply<T>,
    response: Result<T, Error>,
) -> Result<(), Error> {
    responder
        .take()
        .expect("untransferred responder")
        .respond_with_result(response)
}

/// The spawned task cannot run external work until the caller transfers the
/// responder. A failed spawn leaves the responder in the caller's Option, so the
/// original error can be replied explicitly and registered resources rolled back.
fn prepare_response_task<T, F>(
    connection: &ConnectionTo<Agent>,
    work: F,
) -> Result<oneshot::Sender<RequestResponder<T>>, Error>
where
    T: JsonRpcResponse,
    F: Future<Output = Result<T, Error>> + Send + 'static,
{
    let (transfer, receiver) = oneshot::channel::<RequestResponder<T>>();
    connection.spawn(async move {
        let Ok(responder) = receiver.await else {
            return Ok(());
        };
        responder.respond_with_result(work.await)
    })?;
    Ok(transfer)
}

fn transfer_response<T: JsonRpcResponse>(
    transfer: oneshot::Sender<RequestResponder<T>>,
    responder: &mut Reply<T>,
) -> Result<(), Error> {
    match transfer.send(responder.take().expect("untransferred responder")) {
        Ok(()) => Ok(()),
        Err(responder) => responder.respond_with_error(Error::request_cancelled()),
    }
}

pub(super) async fn handle_agent_dispatch(
    dispatch: Dispatch,
    connection: ConnectionTo<Agent>,
    mut context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    match dispatch {
        Dispatch::Request(request, responder) => {
            let responder = RequestResponder {
                lease: context.request_lease.take(),
                responder,
            };
            macro_rules! route {
                ($request:ty, $handler:ident) => {
                    if <$request>::matches_method(request.method()) {
                        return typed_request::<$request>(
                            request,
                            responder,
                            async move |request, responder| {
                                $handler(request, responder, connection, context, turn).await
                            },
                        )
                        .await;
                    }
                };
            }
            route!(RequestPermissionRequest, permission);
            route!(CreateElicitationRequest, elicitation);
            route!(ReadTextFileRequest, read_file);
            route!(WriteTextFileRequest, write_file);
            route!(CreateTerminalRequest, create_terminal);
            route!(TerminalOutputRequest, terminal_output);
            route!(WaitForTerminalExitRequest, terminal_wait);
            route!(KillTerminalRequest, terminal_kill);
            route!(ReleaseTerminalRequest, terminal_release);
            route!(ConnectMcpRequest, mcp_connect);
            route!(MessageMcpRequest, mcp_message);
            route!(DisconnectMcpRequest, mcp_disconnect);
            drop(turn);
            responder.respond_with_error(Error::method_not_found().data(request.method()))
        }
        Dispatch::Notification(notification) => {
            let method = notification.method().to_owned();
            let result = if SessionNotification::matches_method(&method) {
                match SessionNotification::parse_message(&method, notification.params()) {
                    Ok(notification) => session_update(notification, &context, turn).await,
                    Err(error) => Err(error),
                }
            } else if CompleteElicitationNotification::matches_method(&method) {
                match CompleteElicitationNotification::parse_message(&method, notification.params())
                {
                    Ok(notification) => complete_elicitation(notification, &context, turn).await,
                    Err(error) => Err(error),
                }
            } else if MessageMcpNotification::matches_method(&method) {
                let notification =
                    MessageMcpNotification::parse_message(&method, notification.params());
                drop(turn);
                match notification {
                    Ok(notification) => {
                        connection.spawn(async move { context.mcp.notify(notification).await })
                    }
                    Err(error) => Err(error),
                }
            } else {
                drop(turn);
                Ok(())
            };
            // Notifications have no responder. Their validation errors remain
            // diagnostics and must not tear down unrelated session RPCs.
            if let Err(error) = result {
                context.sink.acp_error(error, None, Some(&method));
            }
            Ok(())
        }
        Dispatch::Response(result, router) => {
            // Normally intercepted by OrderedIngress before this handler.
            drop(turn);
            router.route_with_result(result)
        }
    }
}

async fn session_update(
    notification: SessionNotification,
    context: &AgentContext,
    _turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    {
        let state = context.state.lock().await;
        let session_id = notification.session_id.0.as_ref();
        if context.owner.is_some() {
            context.require_owner(&state, session_id)?;
        } else if context.dispatch_owner.session_id.is_some()
            || live_session_incarnation(&state, session_id).is_some()
        {
            // An unknown-ID creation event may use the global staging lane. It
            // may not silently become an update for an already replaced owner.
            return Err(Error::invalid_params().data("session update has no captured owner"));
        }
    }
    // The helper now only takes short state locks: terminal references are
    // checked against canonical allocations in the same commit as semantic fold.
    handle_session_update(
        notification,
        &context.state,
        &context.sink,
        context.terminals.as_ref(),
    )
    .await;
    Ok(())
}

fn interaction_allocation(context: &AgentContext) -> Result<String, Error> {
    context
        .dispatch_owner
        .attempt_id
        .as_ref()
        .filter(|allocation| !allocation.is_empty())
        .cloned()
        .ok_or_else(|| Error::internal_error().data("interaction request has no allocation"))
}

/// Called only when the captured wire cancellation reaches its original FIFO.
/// No SDK marker watcher may make the equivalent business-state transition.
pub(super) async fn cancel_inbound_request(
    captured: CapturedInboundRequest,
    context: &AgentContext,
    _turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    if captured.route.dispatch_owner.attempt_id.as_deref() != Some(&captured.allocation) {
        return Err(Error::internal_error().data("cancel request allocation mismatch"));
    }
    match captured.kind {
        InboundRequestKind::Permission => {
            if let Some(owner) = &captured.route.owner {
                cancel_permission(context, owner, &captured.allocation).await?;
            }
        }
        InboundRequestKind::Elicitation => {
            // Unknown session-scoped requests are not global interactions.
            if captured.route.owner.is_some() || captured.route.dispatch_owner.session_id.is_none()
            {
                cancel_elicitation(context, captured.route.owner.as_ref(), &captured.allocation)
                    .await?;
            }
        }
        // Filesystem/terminal/MCP methods may observe the SDK marker to stop
        // external I/O, but it never authorizes a canonical business write.
        InboundRequestKind::Other => {}
    }
    Ok(())
}

async fn cancel_permission(
    context: &AgentContext,
    owner: &SessionResourceOwner,
    permission_id: &str,
) -> Result<(), Error> {
    let mut state = context.state.lock().await;
    if let Some(pending) = state.sessions.take_permission(permission_id, owner) {
        if interaction_owner_is_live(&state, owner) {
            state
                .sessions
                .resolve_permission(
                    &owner.epoch,
                    &owner.session_id,
                    owner.incarnation,
                    permission_id,
                )
                .map_err(runtime_state_error)?;
            let delta = advance_session_delta(
                &mut state,
                &owner.session_id,
                owner.incarnation,
                json!({ "kind": "interaction_remove", "interactionId": permission_id }),
            )?;
            flush_runtime(&mut state, &context.sink);
            context.sink.send(delta);
        }
        context
            .sink
            .send(json!({ "type": "acp/permission_resolved", "permissionId": permission_id }));
        drop(state);
        drop(pending);
    }
    Ok(())
}

async fn permission(
    request: RequestPermissionRequest,
    responder: &mut Reply<RequestPermissionResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    ensure_relay_size(&request, "permission request")?;
    let session_id = request.session_id.0.to_string();
    let owner = context.require_owner(&*context.state.lock().await, &session_id)?;
    let permission_id = interaction_allocation(&context)?;
    let request_value = serde_json::to_value(&request)?;
    let tool_call = request_value
        .get("toolCall")
        .ok_or_else(|| Error::invalid_request().data("missing permission tool call"))?;
    let tool_call_id = request.tool_call.tool_call_id.0.to_string();
    let (sender, receiver) = oneshot::channel();
    let registered = {
        let mut state = context.state.lock().await;
        context.require_owner(&state, &session_id)?;
        validate_terminal_references(&state, &session_id, owner.incarnation, tool_call)?;
        let option_ids = validate_permission_request(
            ensure_session_validation(&mut state, &session_id, owner.incarnation)?,
            &request,
        )
        .map_err(semantic_error)?;
        if state.sessions.permission_owners.len() >= MAX_PENDING_INTERACTIONS {
            false
        } else {
            if state
                .sessions
                .resources(&session_id, owner.incarnation)
                .map_err(mirror_error)?
                .permissions
                .values()
                .any(|pending| pending.tool_call_id == tool_call_id)
            {
                return Err(Error::invalid_request().data(format!(
                    "A permission request is already pending for tool call: {tool_call_id}"
                )));
            }
            state
                .sessions
                .upsert_permission(
                    &owner.epoch,
                    &session_id,
                    owner.incarnation,
                    permission_id.clone(),
                    request_value.clone(),
                )
                .map_err(runtime_state_error)?;
            state
                .sessions
                .insert_permission(
                    owner.clone(),
                    permission_id.clone(),
                    PermissionResponder {
                        tool_call_id,
                        option_ids,
                        sender,
                    },
                )
                .map_err(mirror_error)?;
            let delta = advance_session_delta(
                &mut state,
                &session_id,
                owner.incarnation,
                json!({
                    "kind": "interaction_upsert", "interaction": {
                        "interactionId": permission_id, "type": "permission", "request": request_value,
                    },
                }),
            )?;
            flush_runtime(&mut state, &context.sink);
            context.sink.send(delta);
            context.sink.send(json!({ "type": "acp/permission_request", "permissionId": permission_id, "request": request }));
            true
        }
    };
    if !registered {
        drop(turn);
        return respond(
            responder,
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            )),
        );
    }
    let transfer = prepare_response_task(&connection, async move {
        // Only the captured wire Cancel reducer removes this sender. The SDK
        // marker is flipped before raw ingress and cannot order business writes.
        receiver.await.map_err(|_| Error::request_cancelled())
    });
    let transfer = match transfer {
        Ok(transfer) => transfer,
        Err(error) => {
            cancel_permission(&context, &owner, &permission_id).await?;
            return Err(error);
        }
    };
    drop(turn);
    transfer_response(transfer, responder)
}

async fn cancel_elicitation(
    context: &AgentContext,
    owner: Option<&SessionResourceOwner>,
    elicitation_id: &str,
) -> Result<(), Error> {
    let mut state = context.state.lock().await;
    if let Some(pending) = take_pending_elicitation(&mut state, elicitation_id, owner) {
        if owner.is_none_or(|owner| interaction_owner_is_live(&state, owner)) {
            let epoch = state.sessions.epoch().to_owned();
            state
                .sessions
                .resolve_elicitation(
                    &epoch,
                    owner.map(|owner| (owner.session_id.as_str(), owner.incarnation)),
                    elicitation_id,
                    None,
                )
                .map_err(runtime_state_error)?;
            let delta = owner
                .map(|owner| {
                    advance_session_delta(
                        &mut state,
                        &owner.session_id,
                        owner.incarnation,
                        json!({ "kind": "interaction_remove", "interactionId": elicitation_id }),
                    )
                })
                .transpose()?;
            flush_runtime(&mut state, &context.sink);
            if let Some(delta) = delta {
                context.sink.send(delta);
            }
        }
        context.sink.send(json!({ "type": "acp/elicitation_resolved", "elicitationId": elicitation_id, "response": CreateElicitationResponse::new(ElicitationAction::Cancel) }));
        drop(state);
        drop(pending);
    }
    Ok(())
}

async fn elicitation(
    request: CreateElicitationRequest,
    responder: &mut Reply<CreateElicitationResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    ensure_relay_size(&request, "elicitation request")?;
    let request_value = validate_elicitation_request(&request)
        .map_err(|error| Error::invalid_params().data(error))?;
    let elicitation_id = interaction_allocation(&context)?;
    let url_elicitation_id = match &request.mode {
        ElicitationMode::Url(mode) => Some(mode.elicitation_id.0.to_string()),
        _ => None,
    };
    let session_id = match request.scope() {
        ElicitationScope::Session(scope) => Some(scope.session_id.0.to_string()),
        ElicitationScope::Request(_) => None,
        _ => None,
    };
    let (sender, receiver) = oneshot::channel();
    let (registered, owner) = {
        let mut state = context.state.lock().await;
        let owner = session_id
            .as_deref()
            .map(|id| context.require_owner(&state, id))
            .transpose()?;
        if pending_elicitation_count(&state) >= MAX_PENDING_INTERACTIONS {
            (false, owner)
        } else {
            if let Some(url_elicitation_id) = &url_elicitation_id {
                if url_elicitation_id.is_empty() || url_elicitation_id.len() > 1_024 {
                    return Err(Error::invalid_params()
                        .data("Agent returned an invalid URL elicitation ID"));
                }
                if state.sessions.url_owners.contains_key(url_elicitation_id)
                    || pending_url_elicitation_in_use(&state, url_elicitation_id)
                {
                    return Err(Error::invalid_params().data(format!(
                        "URL elicitation ID is already outstanding: {url_elicitation_id}"
                    )));
                }
                if state.sessions.url_owners.len() + pending_elicitation_count(&state)
                    >= MAX_URL_ELICITATION_IDS
                {
                    return Err(Error::invalid_request().data(format!(
                        "Agent exceeded {MAX_URL_ELICITATION_IDS} outstanding URL elicitations"
                    )));
                }
            }
            let epoch = state.sessions.epoch().to_owned();
            state
                .sessions
                .upsert_elicitation(
                    &epoch,
                    owner
                        .as_ref()
                        .map(|owner| (owner.session_id.as_str(), owner.incarnation)),
                    elicitation_id.clone(),
                    request_value.clone(),
                )
                .map_err(runtime_state_error)?;
            let pending = ElicitationResponder {
                url_elicitation_id,
                request: request_value.clone(),
                sender,
            };
            if let Some(owner) = &owner {
                state
                    .sessions
                    .insert_elicitation(owner.clone(), elicitation_id.clone(), pending)
                    .map_err(mirror_error)?;
            } else {
                state
                    .request_elicitations
                    .insert(elicitation_id.clone(), pending);
            }
            let delta = owner.as_ref().map(|owner| advance_session_delta(
                &mut state, &owner.session_id, owner.incarnation,
                json!({ "kind": "interaction_upsert", "interaction": {
                    "interactionId": elicitation_id, "type": "elicitation", "request": request_value,
                }}),
            )).transpose()?;
            flush_runtime(&mut state, &context.sink);
            if let Some(delta) = delta {
                context.sink.send(delta);
            }
            context.sink.send(json!({ "type": "acp/elicitation_request", "elicitationId": elicitation_id, "request": request }));
            (true, owner)
        }
    };
    if !registered {
        drop(turn);
        return respond(
            responder,
            Ok(CreateElicitationResponse::new(ElicitationAction::Cancel)),
        );
    }
    let transfer = prepare_response_task(&connection, async move {
        // Only the captured wire Cancel reducer removes this sender. The SDK
        // marker is flipped before raw ingress and cannot order business writes.
        receiver.await.map_err(|_| Error::request_cancelled())
    });
    let transfer = match transfer {
        Ok(transfer) => transfer,
        Err(error) => {
            cancel_elicitation(&context, owner.as_ref(), &elicitation_id).await?;
            return Err(error);
        }
    };
    drop(turn);
    transfer_response(transfer, responder)
}

async fn complete_elicitation(
    notification: CompleteElicitationNotification,
    context: &AgentContext,
    _turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    ensure_relay_size(&notification, "elicitation completion")?;
    let elicitation_id = notification.elicitation_id.0.to_string();
    let mut state = context.state.lock().await;
    let registration = context.url_registration.as_ref().ok_or_else(|| {
        Error::invalid_params().data(format!(
            "URL elicitation was not accepted or is no longer active: {elicitation_id}"
        ))
    })?;
    if registration
        .owner
        .as_ref()
        .is_some_and(|owner| !state.sessions.owns_resources(owner))
    {
        return Err(Error::invalid_params().data("URL elicitation references a retired session"));
    }
    let epoch = state.sessions.epoch().to_owned();
    let resolution = state
        .sessions
        .settle_url_registration(
            &epoch,
            registration
                .owner
                .as_ref()
                .map(|owner| (owner.session_id.as_str(), owner.incarnation)),
            &elicitation_id,
            &registration.registration_id,
            UrlFlowStatus::Completed,
        )
        .map_err(runtime_state_error)?;
    if resolution == UrlFlowResolution::StaleRegistration {
        return Err(Error::invalid_params().data("URL elicitation registration changed"));
    }
    state.sessions.take_url_route(&elicitation_id, registration);
    flush_runtime(&mut state, &context.sink);
    context
        .sink
        .send(json!({ "type": "acp/elicitation_complete", "notification": notification }));
    Ok(())
}

async fn read_file(
    request: ReadTextFileRequest,
    responder: &mut Reply<ReadTextFileResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let (_, filesystem) = context.workspace(&request.session_id.0).await?;
    let cancellation = responder
        .as_ref()
        .expect("untransferred responder")
        .cancellation();
    let transfer = prepare_response_task(&connection, async move {
        filesystem.read_cancellable(request, cancellation).await
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn write_file(
    request: WriteTextFileRequest,
    responder: &mut Reply<WriteTextFileResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let (_, filesystem) = context.workspace(&request.session_id.0).await?;
    let cancellation = responder
        .as_ref()
        .expect("untransferred responder")
        .cancellation();
    let transfer = prepare_response_task(&connection, async move {
        filesystem.write_cancellable(request, cancellation).await
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn create_terminal(
    request: CreateTerminalRequest,
    responder: &mut Reply<CreateTerminalResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let terminals = context.terminals.clone().ok_or_else(terminal_unavailable)?;
    let session_id = request.session_id.0.to_string();
    let (owner, filesystem) = context.workspace(&session_id).await?;
    let transfer = prepare_response_task(&connection, async move {
        let response = terminals
            .in_workspace(filesystem)
            .create_for_incarnation(request, owner.incarnation)
            .await?;
        let local_turn = context.continue_local().await;
        let validation = match &local_turn {
            Ok(_) => {
                let mut state = context.state.lock().await;
                context
                    .require_owner(&state, &session_id)
                    .map_err(|_| {
                        Error::request_cancelled()
                            .data("session closed while terminal was being created")
                    })
                    .and_then(|_| {
                        register_created_terminal_locked(
                            &mut state,
                            &context.sink,
                            &owner,
                            &response.terminal_id.0,
                        )
                    })
            }
            Err(error) => Err(error.clone()),
        };
        // The Agent can reference this allocation immediately after the reply;
        // canonical registration must therefore precede the ACP reply.
        drop(local_turn);
        if let Err(error) = validation {
            let _ = terminals
                .release(ReleaseTerminalRequest::new(
                    session_id,
                    response.terminal_id,
                ))
                .await;
            return Err(error);
        }
        Ok(response)
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

fn terminal_unavailable() -> Error {
    Error::method_not_found().data("terminal methods are unavailable for remote transports")
}

async fn terminal_output(
    request: TerminalOutputRequest,
    responder: &mut Reply<TerminalOutputResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let terminals = context.terminals.clone().ok_or_else(terminal_unavailable)?;
    context.require_owner(&*context.state.lock().await, &request.session_id.0)?;
    let transfer =
        prepare_response_task(&connection, async move { terminals.output(request).await })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn terminal_wait(
    request: WaitForTerminalExitRequest,
    responder: &mut Reply<WaitForTerminalExitResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let terminals = context.terminals.clone().ok_or_else(terminal_unavailable)?;
    context.require_owner(&*context.state.lock().await, &request.session_id.0)?;
    let cancellation = responder
        .as_ref()
        .expect("untransferred responder")
        .cancellation();
    let transfer = prepare_response_task(&connection, async move {
        terminals.wait_for_exit(request, cancellation).await
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn terminal_kill(
    request: KillTerminalRequest,
    responder: &mut Reply<KillTerminalResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let terminals = context.terminals.clone().ok_or_else(terminal_unavailable)?;
    context.require_owner(&*context.state.lock().await, &request.session_id.0)?;
    let transfer =
        prepare_response_task(&connection, async move { terminals.kill(request).await })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn terminal_release(
    request: ReleaseTerminalRequest,
    responder: &mut Reply<ReleaseTerminalResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let terminals = context.terminals.clone().ok_or_else(terminal_unavailable)?;
    context.require_owner(&*context.state.lock().await, &request.session_id.0)?;
    let transfer = prepare_response_task(&connection, async move {
        let (response, snapshot) = terminals.release_with_snapshot(request).await?;
        let _turn = context.continue_local().await?;
        let mut state = context.state.lock().await;
        publish_terminal_snapshot(&mut state, &context.sink, snapshot);
        Ok(response)
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn mcp_connect(
    request: ConnectMcpRequest,
    responder: &mut Reply<ConnectMcpResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let cancellation = responder
        .as_ref()
        .expect("untransferred responder")
        .cancellation();
    let acp = connection.clone();
    let transfer = prepare_response_task(&connection, async move {
        context.mcp.connect(request, acp, cancellation).await
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn mcp_message(
    request: MessageMcpRequest,
    responder: &mut Reply<MessageMcpResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let cancellation = responder
        .as_ref()
        .expect("untransferred responder")
        .cancellation();
    let transfer = prepare_response_task(&connection, async move {
        context.mcp.message(request, cancellation).await
    })?;
    drop(turn);
    transfer_response(transfer, responder)
}

async fn mcp_disconnect(
    request: DisconnectMcpRequest,
    responder: &mut Reply<DisconnectMcpResponse>,
    connection: ConnectionTo<Agent>,
    context: AgentContext,
    turn: Option<ExecutionTurn>,
) -> Result<(), Error> {
    let transfer =
        prepare_response_task(
            &connection,
            async move { context.mcp.disconnect(request).await },
        )?;
    drop(turn);
    transfer_response(transfer, responder)
}

#[cfg(test)]
mod tests {
    use super::super::inbound_requests::InboundRequests;
    use super::super::scheduling::{BridgeIngress, CapturedRoute, Scheduling};
    use super::*;
    use agent_client_protocol::{Handled, Lines};
    use futures::{SinkExt, StreamExt};
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn context() -> (AgentContext, Scheduling, mpsc::UnboundedReceiver<String>) {
        let state = Arc::new(Mutex::new(BridgeState::default()));
        let (events, receiver) = mpsc::unbounded_channel();
        let sink = EventSink { tx: events.into() };
        let epoch = state.try_lock().unwrap().sessions.epoch().to_owned();
        let (scheduling, ingress) =
            Scheduling::new(epoch.clone(), sink.clone(), CancellationToken::new()).unwrap();
        (
            AgentContext {
                state,
                mcp: McpManager::new(PathBuf::from("/tmp"), Vec::new(), sink.tx.clone()),
                sink,
                terminals: None,
                filesystem: None,
                ingress,
                owner: None,
                dispatch_owner: RequestOwner {
                    epoch,
                    session_id: None,
                    incarnation: None,
                    operation_id: "incoming-test".to_owned(),
                    attempt_id: Some("allocation".to_owned()),
                },
                url_registration: None,
                request_lease: None,
            },
            scheduling,
            receiver,
        )
    }

    #[tokio::test]
    async fn captured_cancel_drops_only_its_interaction_for_session_and_global_scopes() {
        for (kind, scoped) in [
            (InboundRequestKind::Permission, true),
            (InboundRequestKind::Elicitation, true),
            (InboundRequestKind::Elicitation, false),
        ] {
            let (context, _scheduling, mut events) = context();
            let owner = if scoped {
                let mut state = context.state.lock().await;
                let epoch = state.sessions.epoch().to_owned();
                let incarnation = state
                    .sessions
                    .open_new(
                        &epoch,
                        "session",
                        "/workspace",
                        json!({"sessionId":"session"}),
                    )
                    .unwrap();
                session_mirror(&mut state).register_new("session", incarnation);
                Some(
                    state
                        .sessions
                        .resource_owner("session", incarnation)
                        .unwrap(),
                )
            } else {
                None
            };
            let mut route = CapturedRoute {
                owner: owner.clone(),
                dispatch_owner: context.dispatch_owner.clone(),
                url_registration: None,
            };
            route.dispatch_owner.session_id = owner.as_ref().map(|owner| owner.session_id.clone());
            route.dispatch_owner.incarnation = owner.as_ref().map(|owner| owner.incarnation);
            route.dispatch_owner.attempt_id = Some("old".to_owned());
            let method = if kind == InboundRequestKind::Permission {
                "session/request_permission"
            } else {
                "elicitation/create"
            };
            let mut inbound = InboundRequests::new(1);
            let id = agent_client_protocol::schema::v1::RequestId::from("reused-id".to_owned());
            let lease = inbound
                .register(id.clone(), method, &route, context.ingress.clone(), 0)
                .unwrap();
            let old_cancel = inbound.capture_cancel(&id).unwrap();

            async fn pending(
                context: &AgentContext,
                kind: InboundRequestKind,
                owner: Option<&SessionResourceOwner>,
                allocation: &str,
            ) -> (
                Option<oneshot::Receiver<RequestPermissionResponse>>,
                Option<oneshot::Receiver<CreateElicitationResponse>>,
            ) {
                let mut state = context.state.lock().await;
                let epoch = state.sessions.epoch().to_owned();
                if kind == InboundRequestKind::Permission {
                    let owner = owner.unwrap();
                    let (sender, receiver) = oneshot::channel();
                    state
                        .sessions
                        .upsert_permission(
                            &epoch,
                            &owner.session_id,
                            owner.incarnation,
                            allocation,
                            json!({}),
                        )
                        .unwrap();
                    state
                        .sessions
                        .insert_permission(
                            owner.clone(),
                            allocation.to_owned(),
                            PermissionResponder {
                                tool_call_id: "tool".to_owned(),
                                option_ids: HashSet::new(),
                                sender,
                            },
                        )
                        .unwrap();
                    (Some(receiver), None)
                } else {
                    let (sender, receiver) = oneshot::channel();
                    state
                        .sessions
                        .upsert_elicitation(
                            &epoch,
                            owner.map(|owner| (owner.session_id.as_str(), owner.incarnation)),
                            allocation,
                            json!({"mode":"form"}),
                        )
                        .unwrap();
                    let responder = ElicitationResponder {
                        url_elicitation_id: None,
                        request: json!({"mode":"form"}),
                        sender,
                    };
                    if let Some(owner) = owner {
                        state
                            .sessions
                            .insert_elicitation(owner.clone(), allocation.to_owned(), responder)
                            .unwrap();
                    } else {
                        state
                            .request_elicitations
                            .insert(allocation.to_owned(), responder);
                    }
                    (None, Some(receiver))
                }
            }

            let (permission, elicitation) = pending(&context, kind, owner.as_ref(), "old").await;
            cancel_inbound_request(old_cancel.clone(), &context, None)
                .await
                .unwrap();
            if let Some(receiver) = permission {
                assert!(receiver.await.is_err());
            }
            if let Some(receiver) = elicitation {
                assert!(receiver.await.is_err());
            }
            let resolved = if kind == InboundRequestKind::Permission {
                "acp/permission_resolved"
            } else {
                "acp/elicitation_resolved"
            };
            let mut resolution_count = 0;
            while let Ok(event) = events.try_recv() {
                let value: Value = serde_json::from_str(&event).unwrap();
                if value["type"] == resolved {
                    resolution_count += 1;
                }
            }
            assert_eq!(resolution_count, 1);
            drop(lease);
            assert!(inbound.finish(&id, "old"));
            route.dispatch_owner.attempt_id = Some("new".to_owned());
            let _new_lease = inbound
                .register(id.clone(), method, &route, context.ingress.clone(), 0)
                .unwrap();
            let (mut permission, mut elicitation) =
                pending(&context, kind, owner.as_ref(), "new").await;
            cancel_inbound_request(old_cancel, &context, None)
                .await
                .unwrap();
            assert!(events.try_recv().is_err());
            if let Some(receiver) = &mut permission {
                assert!(matches!(
                    receiver.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
            }
            if let Some(receiver) = &mut elicitation {
                assert!(matches!(
                    receiver.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
            }
            cancel_inbound_request(inbound.capture_cancel(&id).unwrap(), &context, None)
                .await
                .unwrap();
            if let Some(receiver) = permission {
                assert!(receiver.await.is_err());
            }
            if let Some(receiver) = elicitation {
                assert!(receiver.await.is_err());
            }
        }
    }

    // Run the typed dispatcher in a separate consumer. The SDK handler has
    // already returned Handled::Yes, so it cannot turn worker errors into replies.
    #[tokio::test]
    async fn detached_dispatch_explicitly_replies_to_parse_and_registration_errors() {
        let (context, _scheduling, _events) = context();
        let state = context.state.clone();
        let (incoming_tx, mut received) = mpsc::channel(4);
        let (outgoing, mut peer_responses) = futures::channel::mpsc::channel::<String>(8);
        let (mut peer_requests, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(8);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                async move |dispatch: Dispatch, connection| {
                    incoming_tx
                        .try_send((dispatch, connection))
                        .map_err(|error| Error::internal_error().data(error.to_string()))?;
                    Ok(Handled::Yes)
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |_connection| {
                finished.await.map_err(|_| Error::internal_error())?;
                Ok(())
            });
        let worker = async move {
            for _ in 0..4 {
                let (dispatch, connection) = received.recv().await.unwrap();
                handle_agent_dispatch(dispatch, connection, context.clone(), None)
                    .await
                    .unwrap();
            }
        };
        let peer = async move {
            let cases = [
                ("session/request_permission", json!({}), -32602),
                (
                    "session/request_permission",
                    json!({
                        "sessionId": "missing", "toolCall": {"toolCallId": "tool"}, "options": []
                    }),
                    -32602,
                ),
                (
                    "elicitation/create",
                    json!({
                        "sessionId": "missing", "mode": "form", "message": "Question",
                        "requestedSchema": {"type": "object", "properties": {}}
                    }),
                    -32602,
                ),
                ("test/unknown", json!({}), -32601),
            ];
            for (id, (method, params, code)) in cases.into_iter().enumerate() {
                peer_requests
                    .send(Ok(
                        json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
                            .to_string(),
                    ))
                    .await
                    .unwrap();
                let response: Value =
                    serde_json::from_str(&peer_responses.next().await.unwrap()).unwrap();
                assert_eq!(response["id"], id);
                assert_eq!(response["error"]["code"], code, "{response}");
                if matches!(id, 1 | 2) {
                    assert!(
                        response["error"]["data"]
                            .as_str()
                            .unwrap()
                            .contains("retired session")
                    );
                }
            }
            done.send(()).unwrap();
        };
        let (result, (), ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join3(connection, worker, peer),
        )
        .await
        .expect("a detached request was left without a reply");
        result.unwrap();
        let state = state.lock().await;
        assert!(state.sessions.permission_owners.is_empty());
        assert_eq!(pending_elicitation_count(&state), 0);
    }

    #[tokio::test]
    async fn responder_transfer_starts_work_only_after_acceptance_and_reports_failed_delivery() {
        let (context, mut scheduling, _events) = context();
        let mut inbound = InboundRequests::new(3);
        let (incoming_tx, mut received) = mpsc::channel(3);
        let (outgoing, mut peer_responses) = futures::channel::mpsc::channel::<String>(8);
        let (mut peer_requests, incoming) =
            futures::channel::mpsc::channel::<io::Result<String>>(8);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let (done, finished) = oneshot::channel();
        let connection = Client
            .builder()
            .on_receive_dispatch(
                async move |dispatch: Dispatch, connection| {
                    incoming_tx
                        .try_send((dispatch, connection))
                        .map_err(|error| Error::internal_error().data(error.to_string()))?;
                    Ok(Handled::Yes)
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_with(transport, async move |_connection| {
                finished.await.map_err(|_| Error::internal_error())?;
                Ok(())
            });
        let worker = async move {
            for index in 0..3 {
                let (Dispatch::Request(request, responder), connection) =
                    received.recv().await.unwrap()
                else {
                    panic!("expected an incoming request");
                };
                let mut route = CapturedRoute {
                    owner: None,
                    dispatch_owner: context.dispatch_owner.clone(),
                    url_registration: None,
                };
                route.dispatch_owner.attempt_id = Some(format!("allocation-{index}"));
                let lease = inbound
                    .register(
                        responder.id().clone(),
                        request.method(),
                        &route,
                        context.ingress.clone(),
                        serialized_value_len(request.params()),
                    )
                    .unwrap();
                typed_request::<RequestPermissionRequest>(
                    request,
                    RequestResponder {
                        lease: Some(lease),
                        responder,
                    },
                    async move |_request, responder| match index {
                        0 => Err(Error::invalid_request().data("registration rejected")),
                        1 => {
                            let started = Arc::new(AtomicBool::new(false));
                            let work_started = started.clone();
                            let transfer = prepare_response_task(&connection, async move {
                                work_started.store(true, Ordering::SeqCst);
                                Ok(RequestPermissionResponse::new(
                                    RequestPermissionOutcome::Cancelled,
                                ))
                            })?;
                            assert!(!started.load(Ordering::SeqCst));
                            transfer_response(transfer, responder)
                        }
                        _ => {
                            let (transfer, receiver) = oneshot::channel();
                            drop(receiver);
                            transfer_response(transfer, responder)
                        }
                    },
                )
                .await
                .unwrap();
            }
        };
        let peer = async move {
            for id in 0..3 {
                peer_requests.send(Ok(json!({
                    "jsonrpc":"2.0", "id":id, "method":"session/request_permission",
                    "params":{"sessionId":"session", "toolCall":{"toolCallId":"tool"}, "options":[]}
                }).to_string())).await.unwrap();
                let response: Value =
                    serde_json::from_str(&peer_responses.next().await.unwrap()).unwrap();
                assert_eq!(response["id"], id);
                let BridgeIngress::InboundRequestFinished {
                    request_id,
                    allocation,
                } = scheduling
                    .ingress_rx
                    .try_recv()
                    .expect("cleanup must precede the peer reply")
                    .event
                else {
                    panic!("expected responder lifetime cleanup");
                };
                assert_eq!(serde_json::to_value(request_id).unwrap(), json!(id));
                assert_eq!(allocation, format!("allocation-{id}"));
                assert!(scheduling.ingress_rx.try_recv().is_err());
                match id {
                    0 => {
                        assert_eq!(response["error"]["code"], -32600);
                        assert_eq!(response["error"]["data"], "registration rejected");
                    }
                    1 => assert_eq!(response["result"]["outcome"]["outcome"], "cancelled"),
                    _ => assert_eq!(
                        response["error"]["code"],
                        i32::from(Error::request_cancelled().code)
                    ),
                }
            }
            done.send(()).unwrap();
        };
        let (result, (), ()) = tokio::time::timeout(
            Duration::from_secs(3),
            futures::future::join3(connection, worker, peer),
        )
        .await
        .expect("responder transfer did not finish");
        result.unwrap();
    }
}
