//! The protocol v2 commit step in the bridge actor (docs/state-sync.md §2.3).
//!
//! The bridge state lives behind `BridgeLock`. A guard that took mutable access
//! runs the commit step when it is released, on every return path, so no
//! transition can change what clients see without publishing it (I1). The
//! published documents and the Hub are shared by the actor's generations
//! (`Publication`); a test tap also takes every commit.
use super::*;
use crate::protocol::documents::{
    AgentInfo, AgentLaunch, AgentPhase, AgentTransport, ErrorCode, McpServerKind, McpServerSummary,
    ProtocolError,
};
use crate::protocol::publication::Publication;

pub(super) struct BridgeLock(Mutex<BridgeState>);

pub(super) struct BridgeGuard<'a> {
    state: tokio::sync::MutexGuard<'a, BridgeState>,
    touched: bool,
}

impl BridgeLock {
    pub(super) fn new(state: BridgeState) -> Self {
        Self(Mutex::new(state))
    }

    pub(super) async fn lock(&self) -> BridgeGuard<'_> {
        BridgeGuard {
            state: self.0.lock().await,
            touched: false,
        }
    }

    #[cfg(test)]
    pub(super) fn try_lock(&self) -> Result<BridgeGuard<'_>, tokio::sync::TryLockError> {
        Ok(BridgeGuard {
            state: self.0.try_lock()?,
            touched: false,
        })
    }

    /// For threads outside the runtime, such as a login terminal's reader.
    pub(super) fn blocking_lock(&self) -> BridgeGuard<'_> {
        BridgeGuard {
            state: self.0.blocking_lock(),
            touched: false,
        }
    }

    /// Mutable access that skips the commit step, so the gate can prove it
    /// catches a transition without one.
    #[cfg(test)]
    pub(super) async fn lock_without_commit(&self) -> tokio::sync::MutexGuard<'_, BridgeState> {
        self.0.lock().await
    }
}

impl std::ops::Deref for BridgeGuard<'_> {
    type Target = BridgeState;

    fn deref(&self) -> &BridgeState {
        &self.state
    }
}

impl std::ops::DerefMut for BridgeGuard<'_> {
    fn deref_mut(&mut self) -> &mut BridgeState {
        self.touched = true;
        &mut self.state
    }
}

impl Drop for BridgeGuard<'_> {
    fn drop(&mut self) {
        if self.touched {
            self.state.commit();
        }
    }
}

/// The protocol side of the bridge state: the published documents and the
/// Hub, shared with the other generations, and the Agent's connection state.
pub(super) struct ProtocolState {
    pub(super) publication: Publication,
    pub(super) agent: AgentInfo,
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self {
            publication: Publication::default(),
            agent: AgentInfo {
                phase: AgentPhase::Starting,
                error: None,
                launch: AgentLaunch::default(),
                initialize: None,
                authenticated_method_id: None,
            },
        }
    }
}

/// What `global.agent.launch` shows of the options.
pub(super) fn agent_launch(options: &Options) -> AgentLaunch {
    AgentLaunch {
        transport: match options.transport {
            Transport::Stdio => AgentTransport::Stdio,
            Transport::Http => AgentTransport::Http,
            Transport::Ws => AgentTransport::Ws,
        },
        command: options.command.clone(),
        cwd: options.cwd.to_string_lossy().into_owned(),
        read_only: options.read_only,
        additional_directories: options
            .additional_directories
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect(),
        mcp_servers: options
            .mcp_servers
            .iter()
            .map(|server| McpServerSummary {
                name: server_name(server).to_string(),
                kind: match server_type(server) {
                    "http" => McpServerKind::Http,
                    "sse" => McpServerKind::Sse,
                    "acp" => McpServerKind::Acp,
                    _ => McpServerKind::Stdio,
                },
            })
            .collect(),
    }
}

/// A v1 command's failure as an operation error: the Agent's error, or
/// `unavailable` once the connection is ending.
pub(super) fn operation_error(error: &Error, closing: bool) -> ProtocolError {
    if closing {
        return ProtocolError::new(ErrorCode::Unavailable, "the Agent connection ended");
    }
    ProtocolError::agent(&json!({
        "code": i32::from(error.code),
        "message": error.message,
        "data": error.data,
    }))
}

impl BridgeState {
    /// The commit step: settles what the transition left behind and publishes
    /// every change since the last commit.
    pub(super) fn commit(&mut self) {
        self.retire_orphaned_interactions();
        // The scopes the transitions ended are the feed's to end, with the reasons they gave;
        // they are consumed here. No transition changes what clients see without publishing
        // it, and the events enter every queue in the critical section that produced them (S3).
        let ended = std::mem::take(&mut self.sessions.scopes.ended);
        self.protocol.publication.lock().feed.commit_with(
            &self.sessions,
            &self.protocol.agent,
            self.catalog_revision,
            &ended,
        );
    }

    /// Whether a commit now would publish anything (the gate's check, §4.1).
    #[cfg(test)]
    pub(super) fn unpublished(&self) -> bool {
        self.protocol.publication.lock().feed.diverges(
            &self.sessions,
            &self.protocol.agent,
            self.catalog_revision,
        )
    }

    /// A request-scoped interaction whose operation ended is answered with its cancel
    /// outcome (see `SessionRegistry::orphaned_interactions`).
    fn retire_orphaned_interactions(&mut self) {
        for interaction_id in self.sessions.orphaned_interactions() {
            let epoch = self.sessions.epoch().to_owned();
            let responder = self.request_elicitations.remove(&interaction_id);
            let _ = self
                .sessions
                .resolve_elicitation(&epoch, None, &interaction_id, None);
            if let Some(responder) = responder {
                let _ = responder
                    .sender
                    .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
            }
            self.sessions.scopes.joined.remove(&interaction_id);
        }
        self.sessions.prune_joined();
    }

    /// The Agent connection ended (protocol §4.4): every running operation
    /// fails with `unavailable`, pending interactions and connection resources
    /// go, and the agent shows its final phase.
    pub(super) fn end_connection(&mut self, error: Option<&Error>) {
        let agent = &mut self.protocol.agent;
        match error {
            None => agent.phase = AgentPhase::Stopped,
            Some(error) => {
                agent.phase = AgentPhase::Error;
                agent.error = Some(ProtocolError::new(
                    ErrorCode::Unavailable,
                    error
                        .data
                        .as_ref()
                        .and_then(Value::as_str)
                        .map_or_else(|| error.message.clone(), str::to_string),
                ));
            }
        }
        for (_, pending) in std::mem::take(&mut self.request_elicitations) {
            let _ = pending
                .sender
                .send(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
        self.sessions.end_connection(ProtocolError::new(
            ErrorCode::Unavailable,
            "the Agent connection ended",
        ));
        self.hold_for_next_connection();
    }
}

/// Feeds a login terminal's output and exit into the bridge state. It runs on
/// the terminal's blocking threads, so it takes the lock blocking.
pub(super) fn login_observer(
    state: std::sync::Weak<BridgeLock>,
    op: String,
) -> crate::auth_terminal::AuthTerminalObserver {
    use crate::auth_terminal::AuthTerminalEvent;
    use crate::protocol::registry::LoginExit;
    Arc::new(move |event| {
        let Some(lock) = state.upgrade() else {
            return;
        };
        let mut state = lock.blocking_lock();
        match event {
            AuthTerminalEvent::Output(data) => state.sessions.login_output(&op, &data),
            AuthTerminalEvent::Exited {
                exit_code,
                signal,
                cancelled,
                failure,
            } => state.sessions.login_exited(
                &op,
                LoginExit {
                    exit_code,
                    signal,
                    cancelled,
                    failure,
                },
            ),
        }
    })
}
