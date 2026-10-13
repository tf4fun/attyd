//! Pure projections of bridge state into the v2 documents (plan §2.1).
//!
//! These functions are the only definition of what replicas see. Privacy
//! release lives here: completed process bodies are collapsed out of
//! `history.timeline`, embedded attachment bytes become references, and
//! released terminals leave once nothing references them.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use super::{
    ActiveTurn, AgentState, CatalogState, CollapsedTurn, GlobalState, Interaction, OperationKind,
    PendingCounts, SessionHistory, SessionLifecycle, SessionOperation, SessionPhase, SessionState,
    SessionSummary, SessionWork, TerminalExitStatus, TerminalState, TurnOutcome, TurnOutcomeStatus,
    UrlFlow, UrlFlowStatus,
};
use crate::history_cache::{HistorySnapshot, SessionKey};
use crate::runtime_state::{self as rt, PendingInteraction, SessionOperationKind};
use crate::session_observers::SessionWork as LiveWork;
use crate::session_presentation::terminal_references;
use crate::session_registry::{SessionEntry, SessionRegistry};
use crate::session_resources::SessionResourceOwner;
use crate::session_state::MirrorPhase;

/// The `global` document. `agent` is owned by the connection task, not the
/// registry, so the caller supplies it.
pub(crate) fn project_global(
    agent: &AgentState,
    catalog_revision: u64,
    registry: &SessionRegistry,
) -> GlobalState {
    GlobalState {
        agent: agent.clone(),
        catalog: CatalogState {
            revision: catalog_revision.to_string(),
        },
        sessions: registry
            .sessions
            .iter()
            .filter_map(|(session_id, entry)| Some((session_id.clone(), summary(entry)?)))
            .collect(),
        requests: registry.commands.requests().clone(),
        interactions: registry
            .request_elicitations
            .values()
            .map(|pending| {
                let projected = elicitation(pending, None, None);
                (pending.interaction_id.clone(), projected)
            })
            .collect(),
        url_flows: registry
            .request_url_flows
            .values()
            .map(|flow| (flow.registration_id.clone(), url_flow(flow)))
            .collect(),
    }
}

/// The `session/<id>` document, or `None` when the bridge does not hold the
/// session.
pub(crate) fn project_session(
    registry: &SessionRegistry,
    session_id: &str,
) -> Option<SessionState> {
    let source = SessionSource::new(registry, session_id)?;
    let mut history = source.history();
    let mut refs = std::mem::take(&mut history.terminal_refs);
    let turn = source.turn_head().map(|head| {
        let updates = source.turn_updates().map_or_else(Vec::new, |updates| {
            updates.iter().map(|update| source.update(update)).collect()
        });
        head.with_updates(updates)
    });
    source.turn_references(&mut refs);
    let timeline = std::mem::take(&mut history.timeline);
    let mut state = source.assemble(history, turn, refs);
    state.history.timeline = timeline;
    Some(state)
}

/// A held session's sources. The commit step projects a session from these
/// parts, reusing the parts whose source identity did not change; the full
/// projection above composes the same parts.
pub(super) struct SessionSource<'a> {
    registry: &'a SessionRegistry,
    pub(super) state: &'a crate::session_state::SessionState,
    pub(super) live: &'a rt::SessionLiveState,
    key: SessionKey,
    owner: SessionResourceOwner,
}

/// The projected history: completed process bodies collapsed and embedded
/// attachment bytes replaced by references.
#[derive(Debug, Clone, Default)]
pub(super) struct HistorySection {
    pub timeline: Vec<Value>,
    pub collapsed_turns: Vec<CollapsedTurn>,
    pub outcomes: BTreeMap<String, TurnOutcome>,
    /// Terminals the projected timeline references.
    pub terminal_refs: HashSet<String>,
}

/// The active turn without its updates.
pub(super) struct TurnHead {
    pub operation_id: String,
    pub client_intent_id: String,
    pub prompt: Vec<Value>,
    pub cancel_requested: bool,
}

impl TurnHead {
    pub(super) fn with_updates(self, updates: Vec<Value>) -> ActiveTurn {
        ActiveTurn {
            operation_id: self.operation_id,
            client_intent_id: self.client_intent_id,
            prompt: self.prompt,
            updates,
            cancel_requested: self.cancel_requested,
        }
    }
}

impl<'a> SessionSource<'a> {
    pub(super) fn new(registry: &'a SessionRegistry, session_id: &str) -> Option<Self> {
        let state = &registry.sessions.get(session_id)?.state;
        let live = state.live.as_ref()?;
        Some(Self {
            registry,
            state,
            live,
            key: SessionKey::new(session_id, state.incarnation),
            owner: SessionResourceOwner::new(&registry.epoch, session_id, state.incarnation),
        })
    }

    /// The history baseline; with `turn_outcomes` and `history_revision` it
    /// determines `history()`.
    pub(super) fn history_snapshot(&self) -> Option<&'a Arc<HistorySnapshot>> {
        self.registry.history.peek(&self.key)
    }

    pub(super) fn history(&self) -> HistorySection {
        let view = json!({
            "historyRevision": self.state.history_revision,
            "timeline": self.history_snapshot().map(|snapshot| snapshot.updates()).unwrap_or(&[]),
            "turnOutcomes": self.state.turn_outcomes,
        });
        let mut view = crate::session_presentation::compact_view(view, None);
        let mut timeline = take_array(&mut view["timeline"]);
        let mut terminal_refs = HashSet::new();
        for update in &mut timeline {
            terminal_references(update, &mut terminal_refs);
            crate::attachments::project_update(update, &self.owner);
        }
        let intents = self
            .state
            .turn_outcomes
            .iter()
            .map(|outcome| {
                (
                    outcome.operation_id.as_str(),
                    outcome.client_intent_id.as_str(),
                )
            })
            .collect::<HashMap<_, _>>();
        let outcomes = take_array(&mut view["turnOutcomes"])
            .into_iter()
            .filter_map(|outcome| {
                let operation_id = outcome["operationId"].as_str()?.to_string();
                let projected = TurnOutcome {
                    client_intent_id: intents.get(operation_id.as_str())?.to_string(),
                    status: TurnOutcomeStatus::Completed,
                    response: Some(outcome["response"].clone()),
                    error: None,
                    prompt: None,
                    after_update: usize::try_from(outcome["afterUpdate"].as_u64()?).ok()?,
                    operation_id: operation_id.clone(),
                };
                Some((operation_id, projected))
            })
            .collect();
        HistorySection {
            timeline,
            collapsed_turns: serde_json::from_value(view["collapsedTurns"].take())
                .unwrap_or_default(),
            outcomes,
            terminal_refs,
        }
    }

    pub(super) fn turn_head(&self) -> Option<TurnHead> {
        let prompt = self.prompt(&self.state.active_turn.as_ref()?.prompt);
        self.turn_head_with(prompt)
    }

    /// The active turn's head with an already projected prompt.
    pub(super) fn turn_head_with(&self, prompt: Vec<Value>) -> Option<TurnHead> {
        let turn = self.state.active_turn.as_ref()?;
        Some(TurnHead {
            operation_id: turn.operation_id.clone(),
            client_intent_id: turn.client_intent_id.clone(),
            prompt,
            cancel_requested: turn
                .execution
                .as_ref()
                .is_some_and(|execution| execution.cancel_requested),
        })
    }

    pub(super) fn turn_updates(&self) -> Option<&'a rt::SharedTurnUpdates> {
        Some(&self.state.active_turn.as_ref()?.updates)
    }

    /// Adds the terminals the active turn's prompt and updates reference.
    pub(super) fn turn_references(&self, refs: &mut HashSet<String>) {
        if let Some(turn) = &self.state.active_turn {
            turn.prompt
                .iter()
                .chain(turn.updates.iter().map(|update| &**update))
                .for_each(|value| terminal_references(value, refs));
        }
    }

    /// One projected active-turn update; a pure function of the update.
    pub(super) fn update(&self, update: &Value) -> Value {
        let mut update = update.clone();
        crate::attachments::project_update(&mut update, &self.owner);
        update
    }

    pub(super) fn prompt(&self, prompt: &[Value]) -> Vec<Value> {
        prompt
            .iter()
            .map(|block| {
                let mut block = block.clone();
                crate::attachments::project_content(&mut block, &self.owner);
                block
            })
            .collect()
    }

    /// The document from its parts. `history.timeline` and `turn.updates` are
    /// taken as given, so the commit step can leave them empty; `refs` are
    /// the terminals the timeline and the active turn reference.
    pub(super) fn assemble(
        &self,
        history: HistorySection,
        turn: Option<ActiveTurn>,
        mut refs: HashSet<String>,
    ) -> SessionState {
        let state = self.state;
        let live = self.live;
        // Interactions record the turn's RPC operation, which is its client
        // intent; replicas address the turn by its public operation ID.
        let turn_operation = |internal: Option<&str>| {
            let turn = state.active_turn.as_ref()?;
            let rpc = turn
                .execution
                .as_ref()
                .map_or(turn.client_intent_id.as_str(), |execution| {
                    execution.rpc_operation_id.as_str()
                });
            (internal? == rpc).then(|| turn.operation_id.clone())
        };
        let mut interactions = BTreeMap::new();
        for (interaction_id, permission) in &live.permissions {
            let mut request = permission.request.clone();
            terminal_references(&request, &mut refs);
            crate::attachments::project_permission_request(&mut request, &self.owner);
            interactions.insert(
                interaction_id.clone(),
                Interaction::Permission {
                    interaction_id: interaction_id.clone(),
                    request,
                    operation_id: turn_operation(permission.operation_id.as_deref()),
                },
            );
        }
        for pending in live.elicitations.values() {
            let operation_id = turn_operation(pending.operation_id.as_deref());
            interactions.insert(
                pending.interaction_id.clone(),
                elicitation(pending, operation_id, None),
            );
        }
        // Released terminals leave once nothing references them.
        refs.extend(live.terminals.keys().cloned());
        let retained = self.registry.retained_terminals.get(&self.key);
        let terminals = retained
            .into_iter()
            .flatten()
            .filter(|(terminal_id, _)| !live.terminals.contains_key(*terminal_id))
            .chain(&live.terminals)
            .filter(|(terminal_id, _)| refs.contains(*terminal_id))
            .map(|(terminal_id, terminal)| {
                (terminal_id.clone(), terminal_state(terminal_id, terminal))
            })
            .collect();

        SessionState {
            session_id: state.session_id.clone(),
            incarnation: state.incarnation,
            cwd: live.cwd.to_string_lossy().into_owned(),
            controls: live.controls.clone(),
            history: SessionHistory {
                revision: state.history_revision.clone(),
                phase: phase(state.phase),
                error: state.sync_error.clone(),
                notice: state.history_notice.clone(),
                collapsed_turns: history.collapsed_turns,
                timeline: history.timeline,
            },
            turn,
            outcomes: history.outcomes,
            operation: state.operation.as_ref().map(|operation| SessionOperation {
                operation_id: operation.operation_id.clone(),
                kind: operation_kind(operation.kind),
                stage: operation.stage.clone(),
                // A command's operation carries its record's ID.
                request_id: self
                    .registry
                    .commands
                    .request(&operation.operation_id)
                    .map(|request| request.request_id.clone()),
            }),
            interactions,
            url_flows: live
                .url_flows
                .values()
                .chain(live.settled_url_flows.iter().map(|(flow, _)| flow))
                .map(|flow| (flow.registration_id.clone(), url_flow(flow)))
                .collect(),
            terminals,
        }
    }
}

fn summary(entry: &SessionEntry) -> Option<SessionSummary> {
    let state = &entry.state;
    let live = state.live.as_ref()?;
    Some(SessionSummary {
        session_id: state.session_id.clone(),
        incarnation: state.incarnation,
        cwd: live.cwd.to_string_lossy().into_owned(),
        title: live.controls.info.title.clone(),
        lifecycle: lifecycle(&live.lifecycle),
        phase: phase(state.phase),
        work: match LiveWork::of(entry) {
            LiveWork::Idle => SessionWork::Idle,
            LiveWork::Running => SessionWork::Running,
            LiveWork::AwaitingInteraction => SessionWork::AwaitingInput,
        },
        pending: PendingCounts {
            permissions: live.permissions.len(),
            elicitations: live.elicitations.len(),
            url_flows: live
                .url_flows
                .values()
                .filter(|flow| flow.status == rt::UrlFlowStatus::Waiting)
                .count(),
        },
    })
}

fn elicitation(
    pending: &PendingInteraction,
    operation_id: Option<String>,
    request_id: Option<String>,
) -> Interaction {
    Interaction::Elicitation {
        interaction_id: pending.interaction_id.clone(),
        request: pending.request.clone(),
        operation_id,
        request_id,
    }
}

fn url_flow(flow: &rt::UrlFlow) -> UrlFlow {
    UrlFlow {
        interaction_id: flow.registration_id.clone(),
        elicitation_id: flow.elicitation_id.clone(),
        // Global parents arrive with migration step 5.
        request_id: None,
        request: flow.request.clone(),
        status: match flow.status {
            rt::UrlFlowStatus::Waiting => UrlFlowStatus::Waiting,
            rt::UrlFlowStatus::Completed => UrlFlowStatus::Completed,
            rt::UrlFlowStatus::Cancelled => UrlFlowStatus::Cancelled,
        },
    }
}

fn terminal_state(terminal_id: &str, terminal: &Value) -> TerminalState {
    TerminalState {
        terminal_id: terminal_id.to_string(),
        output: terminal["output"].as_str().unwrap_or_default().to_string(),
        truncated: terminal["truncated"].as_bool().unwrap_or(false),
        exit_status: terminal
            .get("exitStatus")
            .filter(|status| !status.is_null())
            .map(|status| TerminalExitStatus {
                exit_code: status["exitCode"]
                    .as_u64()
                    .and_then(|code| u32::try_from(code).ok()),
                signal: status["signal"].as_str().map(str::to_owned),
            }),
        released: terminal["released"].as_bool().unwrap_or(false),
    }
}

fn lifecycle(lifecycle: &rt::SessionLifecycle) -> SessionLifecycle {
    match lifecycle {
        rt::SessionLifecycle::Attaching => SessionLifecycle::Attaching,
        rt::SessionLifecycle::Active => SessionLifecycle::Active,
        rt::SessionLifecycle::Closing => SessionLifecycle::Closing,
        rt::SessionLifecycle::ClosingForDelete => SessionLifecycle::ClosingForDelete,
        rt::SessionLifecycle::Deleting => SessionLifecycle::Deleting,
        rt::SessionLifecycle::Closed => SessionLifecycle::Closed,
        rt::SessionLifecycle::Uncertain => SessionLifecycle::Uncertain,
    }
}

fn phase(phase: MirrorPhase) -> SessionPhase {
    match phase {
        MirrorPhase::Cold => SessionPhase::Cold,
        MirrorPhase::Loading => SessionPhase::Loading,
        MirrorPhase::Ready => SessionPhase::Ready,
        MirrorPhase::Running => SessionPhase::Running,
        MirrorPhase::Reconciling => SessionPhase::Reconciling,
        MirrorPhase::Blocked => SessionPhase::Blocked,
    }
}

fn operation_kind(kind: SessionOperationKind) -> OperationKind {
    match kind {
        SessionOperationKind::Load => OperationKind::Load,
        SessionOperationKind::Resume => OperationKind::Resume,
        SessionOperationKind::Fork => OperationKind::Fork,
        SessionOperationKind::Close => OperationKind::Close,
        SessionOperationKind::Delete => OperationKind::Delete,
        SessionOperationKind::SetMode => OperationKind::SetMode,
        SessionOperationKind::SetConfig => OperationKind::SetConfig,
    }
}

fn take_array(value: &mut Value) -> Vec<Value> {
    match value.take() {
        Value::Array(values) => values,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::{AgentLaunch, AgentPhase, AgentTransport};
    use crate::session_state::TurnAdmission;

    fn agent() -> AgentState {
        AgentState {
            phase: AgentPhase::Ready,
            error: None,
            launch: AgentLaunch {
                transport: AgentTransport::Stdio,
                command: vec!["agent".into()],
                cwd: "/work".into(),
                read_only: false,
                additional_directories: Vec::new(),
                mcp_servers: Vec::new(),
            },
            initialize: Some(json!({ "protocolVersion": 1 })),
            authenticated_method_id: None,
            predecessor: None,
        }
    }

    fn open(registry: &mut SessionRegistry, session_id: &str) -> u64 {
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                session_id,
                format!("/{session_id}"),
                json!({ "sessionId": session_id }),
                vec![],
            )
            .unwrap();
        registry.register_new(session_id, incarnation);
        incarnation
    }

    fn text(text: &str) -> Value {
        json!({ "type": "text", "text": text })
    }

    #[test]
    fn projected_modes_follow_early_updates_response_and_later_updates() {
        let mut registry = SessionRegistry::new("epoch");
        let available = json!([
            { "id": "ask", "name": "Ask" },
            { "id": "code", "name": "Code" },
        ]);
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "s",
                "/s",
                json!({ "sessionId": "s", "modes": {
                    "currentModeId": "ask", "availableModes": available,
                } }),
                vec![json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" })],
            )
            .unwrap();
        registry.register_new("s", incarnation);
        let after_response = project_session(&registry, "s").unwrap().controls.modes;
        registry
            .update_control_state(
                "epoch",
                "s",
                incarnation,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
            )
            .unwrap();
        let after_later_update = project_session(&registry, "s").unwrap().controls.modes;

        // Capture both cuts before asserting: always applying the response last
        // would fix the first cut but incorrectly overwrite the later update.
        assert_eq!(
            json!([after_response, after_later_update]),
            json!([
                { "currentModeId": "ask", "availableModes": available },
                { "currentModeId": "code", "availableModes": available },
            ]),
        );
    }

    #[test]
    fn projected_config_preserves_response_clear_and_later_updates() {
        let mut registry = SessionRegistry::new("epoch");
        let options = json!([{
            "id": "mode", "name": "Mode", "type": "select", "currentValue": "code",
            "options": [{ "value": "code", "name": "Code" }],
        }]);
        let update = json!({ "sessionUpdate": "config_option_update", "configOptions": options });
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "s",
                "/s",
                json!({ "sessionId": "s", "configOptions": [] }),
                vec![update.clone()],
            )
            .unwrap();
        registry.register_new("s", incarnation);
        let after_response = project_session(&registry, "s")
            .unwrap()
            .controls
            .config_options;
        registry
            .update_control_state("epoch", "s", incarnation, "config_option_update", update)
            .unwrap();
        let after_later_update = project_session(&registry, "s")
            .unwrap()
            .controls
            .config_options;

        // An explicit response [] clears the early value, but a subsequent
        // notification with that same value must still become current.
        assert_eq!(
            json!([after_response, after_later_update]),
            json!([[], options])
        );
    }

    #[test]
    fn projected_mode_reflects_successful_control_response_without_notification() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "s",
                "/s",
                json!({ "sessionId": "s", "modes": {
                    "currentModeId": "ask",
                    "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }],
                } }),
                vec![],
            )
            .unwrap();
        registry.register_new("s", incarnation);
        registry
            .start_operation(
                "epoch",
                "s",
                incarnation,
                "set-mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        // Follow the bridge's successful session/set_mode response path.
        // The Agent does not have to send a separate control notification.
        registry
            .complete_control_operation(
                "epoch",
                "s",
                incarnation,
                "set-mode",
                SessionOperationKind::SetMode,
                "current_mode_update",
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
                json!({}),
            )
            .unwrap();

        let session = project_session(&registry, "s").unwrap();
        assert!(session.operation.is_none(), "the control command completed");
        assert_eq!(session.controls.modes.unwrap().current_mode_id, "code");
    }

    #[test]
    fn projected_config_reflects_successful_control_response_without_notification() {
        let mut registry = SessionRegistry::new("epoch");
        let initial_options = json!([{
            "id": "mode", "name": "Mode", "type": "select", "currentValue": "ask",
            "options": [{ "value": "ask", "name": "Ask" }, { "value": "code", "name": "Code" }],
        }]);
        let mut returned_options = initial_options.clone();
        returned_options[0]["currentValue"] = json!("code");
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "s",
                "/s",
                json!({ "sessionId": "s", "configOptions": initial_options }),
                vec![],
            )
            .unwrap();
        registry.register_new("s", incarnation);
        registry
            .start_operation(
                "epoch",
                "s",
                incarnation,
                "set-config",
                SessionOperationKind::SetConfig,
                "setting",
            )
            .unwrap();
        // The response carries the complete current config options. Project
        // that result even if no config_option_update notification arrives.
        registry
            .complete_control_operation(
                "epoch", "s", incarnation, "set-config", SessionOperationKind::SetConfig,
                "config_option_update",
                json!({ "sessionUpdate": "config_option_update", "configOptions": returned_options }),
                json!({ "configOptions": returned_options }),
            )
            .unwrap();

        let session = project_session(&registry, "s").unwrap();
        assert!(session.operation.is_none(), "the control command completed");
        assert_eq!(json!(session.controls.config_options), returned_options);
    }

    fn admit_projected_turn(registry: &mut SessionRegistry, incarnation: u64) -> String {
        let revision = registry
            .state("s")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = registry
            .admit_session_turn(
                "epoch",
                "s",
                incarnation,
                &revision,
                "client-intent-X",
                vec![text("Run it")],
            )
            .unwrap()
        else {
            panic!("new intent");
        };
        // The runtime-only start_prompt fixture makes these identities equal
        // and would hide an incorrect interaction-to-turn projection.
        assert_ne!(operation_id, "client-intent-X");
        operation_id
    }

    #[test]
    fn projected_permission_references_the_public_turn_operation() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = open(&mut registry, "s");
        let operation_id = admit_projected_turn(&mut registry, incarnation);
        registry
            .upsert_permission(
                "epoch",
                "s",
                incarnation,
                "permission",
                json!({
                    "sessionId": "s", "toolCall": { "toolCallId": "tool", "title": "Run" },
                    "options": [{ "optionId": "yes", "name": "Allow", "kind": "allow_once" }],
                }),
            )
            .unwrap();
        let session = project_session(&registry, "s").unwrap();
        assert_eq!(session.turn.as_ref().unwrap().operation_id, operation_id);
        let Interaction::Permission {
            operation_id: parent,
            ..
        } = &session.interactions["permission"]
        else {
            panic!("permission interaction");
        };
        assert_eq!(parent.as_deref(), Some(operation_id.as_str()));
    }

    #[test]
    fn projected_elicitation_references_the_public_turn_operation() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = open(&mut registry, "s");
        let operation_id = admit_projected_turn(&mut registry, incarnation);
        registry
            .upsert_elicitation(
                "epoch",
                Some(("s", incarnation)),
                "form",
                json!({
                    "sessionId": "s", "mode": "form", "message": "Name?",
                    "requestedSchema": { "type": "object", "properties": {} },
                }),
            )
            .unwrap();
        let session = project_session(&registry, "s").unwrap();
        assert_eq!(session.turn.as_ref().unwrap().operation_id, operation_id);
        let Interaction::Elicitation {
            operation_id: parent,
            ..
        } = &session.interactions["form"]
        else {
            panic!("elicitation interaction");
        };
        assert_eq!(parent.as_deref(), Some(operation_id.as_str()));
    }

    #[test]
    fn projected_compaction_attachments_are_references_resolvable_from_authority() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = open(&mut registry, "s");
        let operation_id = admit_projected_turn(&mut registry, incarnation);
        let update = json!({
            "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed",
            "summary": [
                { "type": "image", "mimeType": "image/png", "data": "UFJJVkFURS1QSVhFTFM=" },
                { "type": "audio", "mimeType": "audio/wav", "data": "UFJJVkFURS1BVURJTw==" },
                { "type": "resource", "resource": { "uri": "file:///note.txt", "text": "private-summary-text" } },
                { "type": "resource", "resource": { "uri": "file:///data.bin", "blob": "UFJJVkFURS1CTE9C" } },
            ],
        });
        // Exercise a valid ACP payload, including the SDK's normalization.
        let typed: agent_client_protocol::schema::v1::SessionUpdate =
            serde_json::from_value(update).unwrap();
        let update = serde_json::to_value(typed).unwrap();
        crate::semantic::validate_and_track_session_update(
            &mut crate::semantic::SessionUpdateSemanticState::default(),
            &update,
        )
        .unwrap();
        let originals = update["summary"].as_array().unwrap().clone();
        registry
            .append_turn_update("s", incarnation, &operation_id, update)
            .unwrap();

        let session = project_session(&registry, "s").unwrap();
        let turn = session.turn.as_ref().unwrap();
        let summary = turn.updates[0]["summary"].as_array().unwrap();
        assert_eq!(summary.len(), originals.len());
        let mut authority = registry.view_value("s", incarnation).unwrap();
        for (block, original) in summary.iter().zip(&originals) {
            let reference = crate::attachments::reference(block)
                .unwrap_or_else(|| panic!("summary attachment was not projected: {block}"));
            assert_eq!(reference.bridge_epoch, "epoch");
            assert_eq!(reference.session_id, "s");
            assert_eq!(reference.session_incarnation, incarnation);
            assert!(block.get("data").is_none() && block.get("resource").is_none());
            assert_eq!(
                crate::attachments::find(&mut authority, &reference.id).as_ref(),
                Some(original)
            );
        }
        let wire = serde_json::to_string(&session).unwrap();
        for private in [
            "UFJJVkFURS1QSVhFTFM=",
            "UFJJVkFURS1BVURJTw==",
            "private-summary-text",
            "UFJJVkFURS1CTE9C",
        ] {
            assert!(
                !wire.contains(private),
                "attachment payload entered the projection"
            );
        }
    }

    #[test]
    fn summaries_report_work_and_pending_counts() {
        let mut registry = SessionRegistry::new("epoch");
        let idle = open(&mut registry, "idle");
        registry
            .update_control_state(
                "epoch",
                "idle",
                idle,
                "session_info_update",
                json!({ "sessionUpdate": "session_info_update", "title": "Named" }),
            )
            .unwrap();
        let running = open(&mut registry, "running");
        registry
            .start_prompt("epoch", "running", running, "turn", vec![text("go")])
            .unwrap();
        let waiting = open(&mut registry, "waiting");
        registry
            .upsert_permission(
                "epoch",
                "waiting",
                waiting,
                "permission",
                json!({ "sessionId": "waiting", "toolCall": { "toolCallId": "t" }, "options": [] }),
            )
            .unwrap();
        registry
            .upsert_elicitation(
                "epoch",
                Some(("waiting", waiting)),
                "form",
                json!({ "sessionId": "waiting", "mode": "form", "message": "Name?" }),
            )
            .unwrap();

        let global = project_global(&agent(), 4, &registry);
        assert_eq!(global.catalog.revision, "4");
        assert_eq!(global.agent, agent());
        let summary = |id: &str| global.sessions[id].clone();
        assert_eq!(summary("idle").work, SessionWork::Idle);
        assert_eq!(summary("idle").title.as_deref(), Some("Named"));
        assert_eq!(summary("idle").lifecycle, SessionLifecycle::Active);
        assert_eq!(summary("idle").cwd, "/idle");
        assert_eq!(summary("idle").pending, PendingCounts::default());
        assert_eq!(summary("running").work, SessionWork::Running);
        assert_eq!(summary("running").phase, SessionPhase::Running);
        assert_eq!(summary("waiting").work, SessionWork::AwaitingInput);
        assert_eq!(
            summary("waiting").pending,
            PendingCounts {
                permissions: 1,
                elicitations: 1,
                url_flows: 0,
            }
        );
        assert_eq!(summary("waiting").incarnation, waiting);

        let session = project_session(&registry, "waiting").unwrap();
        assert!(matches!(
            &session.interactions["permission"],
            Interaction::Permission { interaction_id, .. } if interaction_id == "permission"
        ));
        assert!(matches!(
            &session.interactions["form"],
            Interaction::Elicitation {
                request_id: None,
                ..
            }
        ));
        assert!(project_session(&registry, "missing").is_none());
    }

    #[test]
    fn request_scoped_interactions_and_url_flows_are_global_and_keyed_by_interaction() {
        let mut registry = SessionRegistry::new("epoch");
        for id in ["form", "url"] {
            registry
                .upsert_elicitation(
                    "epoch",
                    None,
                    id,
                    json!({ "mode": "url", "elicitationId": "flow", "url": "https://example.com", "message": "Sign in" }),
                )
                .unwrap();
        }
        registry
            .resolve_elicitation("epoch", None, "url", Some("flow"))
            .unwrap();

        let global = project_global(&agent(), 0, &registry);
        assert_eq!(global.interactions.keys().collect::<Vec<_>>(), ["form"]);
        let flow = &global.url_flows["url"];
        assert_eq!(flow.interaction_id, "url");
        assert_eq!(flow.elicitation_id, "flow");
        assert_eq!(flow.status, UrlFlowStatus::Waiting);
        assert!(global.sessions.is_empty() && global.requests.is_empty());
    }

    #[test]
    fn completed_turns_release_process_content_and_attachment_bytes() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = open(&mut registry, "s");
        let revision = registry
            .state("s")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let prompt = vec![
            text("Run it"),
            json!({ "type": "image", "mimeType": "image/png", "data": "UFJJVkFURS1QSVhFTFM=" }),
        ];
        let TurnAdmission::Accepted { operation_id } = registry
            .start_turn("s", incarnation, &revision, "intent", prompt)
            .unwrap()
        else {
            panic!("new intent");
        };
        for update in [
            json!({ "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "private-thought" } }),
            json!({ "sessionUpdate": "tool_call", "toolCallId": "tool",
                    "title": "private-tool", "status": "completed" }),
            json!({ "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "visible-answer" } }),
        ] {
            registry
                .append_turn_update("s", incarnation, &operation_id, update)
                .unwrap();
        }

        let running = project_session(&registry, "s").unwrap();
        let turn = running.turn.as_ref().unwrap();
        assert_eq!(turn.client_intent_id, "intent");
        assert_eq!(turn.updates.len(), 3);
        assert!(!turn.cancel_requested);
        assert_eq!(turn.prompt[1]["type"], "resource_link");
        assert!(
            !serde_json::to_string(&running)
                .unwrap()
                .contains("UFJJVkFURS1QSVhFTFM=")
        );

        registry
            .complete_prompt("epoch", "s", incarnation, "intent", json!({}))
            .unwrap();
        registry
            .complete_turn(
                "s",
                incarnation,
                &operation_id,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        registry
            .commit_completed_turn_from_memory("s", incarnation, &operation_id)
            .unwrap();

        let session = project_session(&registry, "s").unwrap();
        let wire = serde_json::to_string(&session).unwrap();
        for private in ["private-thought", "private-tool", "UFJJVkFURS1QSVhFTFM="] {
            assert!(!wire.contains(private), "{private} leaked: {wire}");
        }
        assert!(wire.contains("visible-answer"));
        assert!(session.turn.is_none());
        assert_eq!(session.history.phase, SessionPhase::Ready);
        assert_eq!(
            session.history.revision,
            registry.state("s").unwrap().history_revision
        );
        let [collapsed] = session.history.collapsed_turns.as_slice() else {
            panic!("one completed turn: {:?}", session.history.collapsed_turns);
        };
        assert_eq!(collapsed.process_count, 2);
        assert_eq!(
            collapsed.operation_id.as_deref(),
            Some(operation_id.as_str())
        );
        assert_eq!(collapsed.after_update, session.history.timeline.len());
        let outcome = &session.outcomes[&operation_id];
        assert_eq!(outcome.client_intent_id, "intent");
        assert_eq!(outcome.status, TurnOutcomeStatus::Completed);
        assert_eq!(outcome.response, Some(json!({ "stopReason": "end_turn" })));
        assert_eq!(outcome.after_update, session.history.timeline.len());
    }

    #[test]
    fn controls_operations_and_live_terminals_are_projected() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = registry
            .open_new_with_replay(
                "epoch",
                "s",
                "/s",
                json!({
                    "sessionId": "s",
                    "modes": { "currentModeId": "ask", "availableModes": [{ "id": "ask", "name": "Ask" }] },
                }),
                vec![json!({ "sessionUpdate": "available_commands_update",
                             "availableCommands": [{ "name": "init", "description": "Init" }] })],
            )
            .unwrap();
        registry.register_new("s", incarnation);
        registry
            .upsert_terminal(
                "epoch",
                "s",
                incarnation,
                "term",
                json!({
                    "sessionId": "s", "terminalId": "term", "output": "done\n", "outputAppend": true,
                    "truncated": false, "exitStatus": { "exitCode": 0, "signal": null }, "released": false,
                }),
            )
            .unwrap();
        registry
            .begin_exclusive("s", incarnation, SessionOperationKind::SetMode, "mode-op")
            .unwrap();

        let session = project_session(&registry, "s").unwrap();
        assert_eq!(session.cwd, "/s");
        assert_eq!(
            session.controls.modes.as_ref().unwrap().current_mode_id,
            "ask"
        );
        assert_eq!(session.controls.config_options, None);
        assert_eq!(session.controls.commands.len(), 1);
        assert_eq!(
            session.terminals["term"],
            TerminalState {
                terminal_id: "term".into(),
                output: "done\n".into(),
                truncated: false,
                exit_status: Some(TerminalExitStatus {
                    exit_code: Some(0),
                    signal: None,
                }),
                released: false,
            }
        );
        let operation = session.operation.as_ref().unwrap();
        assert_eq!(operation.operation_id, "mode-op");
        assert_eq!(operation.kind, OperationKind::SetMode);
        assert_eq!(operation.request_id, None);
        assert_eq!(
            project_global(&agent(), 0, &registry).sessions["s"].work,
            SessionWork::Running
        );
    }
}
