use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Map, Value, json};

const MAX_RUNTIME_EVENTS_PER_SESSION: usize = 100_000;
const MAX_RUNTIME_BYTES_PER_SESSION: usize = 32 * 1024 * 1024;
const MAX_RUNTIME_BYTES_TOTAL: usize = 64 * 1024 * 1024;
const MAX_PENDING_SESSION_EVENTS: usize = 10_000;
const MAX_GLOBAL_RUNTIME_EVENTS: usize = 1_024;
const MAX_GLOBAL_RUNTIME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct RuntimeCache {
    sessions: HashMap<String, RuntimeSession>,
    pending_session_events: HashMap<String, VecDeque<String>>,
    pending_session_truncated: HashSet<String>,
    prompt_sessions: HashMap<String, String>,
    operation_sessions: HashMap<String, String>,
    permission_sessions: HashMap<String, String>,
    elicitation_sessions: HashMap<String, Option<String>>,
    url_elicitation_sessions: HashMap<String, Option<String>>,
    global_events: VecDeque<String>,
    global_event_bytes: usize,
    clock: u64,
}

struct RuntimeSession {
    cwd: String,
    session: Value,
    events: VecDeque<String>,
    event_bytes: usize,
    truncated: bool,
    updated_at: u64,
    active_prompt: Option<String>,
    pending_permissions: HashMap<String, String>,
    pending_elicitations: HashMap<String, String>,
    active_url_flows: HashMap<String, Vec<String>>,
    terminal_states: HashMap<String, String>,
    active_operation: Option<String>,
}

impl RuntimeCache {
    pub(crate) fn update(&mut self, event: &str) {
        let Ok(value) = serde_json::from_str::<Value>(event) else {
            return;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "acp/session_created" => {
                let Some(response) = value.get("response") else {
                    return;
                };
                let Some(session_id) = response.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                self.open_session(
                    session_id,
                    value.get("cwd").and_then(Value::as_str).unwrap_or_default(),
                    response,
                );
                if let Some(updates) = value.get("earlyUpdates").and_then(Value::as_array) {
                    for notification in updates {
                        self.record_session_value(
                            session_id,
                            json!({
                                "type": "acp/session_update",
                                "notification": notification,
                            }),
                        );
                    }
                }
            }
            "acp/session_attached" => {
                let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                let Some(response) = value.get("response") else {
                    return;
                };
                self.open_session(
                    session_id,
                    value.get("cwd").and_then(Value::as_str).unwrap_or_default(),
                    response,
                );
            }
            "acp/session_forked" => {
                let Some(response) = value.get("response") else {
                    return;
                };
                let Some(session_id) = response.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                let source_session_id = value
                    .get("sourceSessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.open_forked_session(
                    source_session_id,
                    session_id,
                    value.get("cwd").and_then(Value::as_str).unwrap_or_default(),
                    response,
                );
                if let Some(updates) = value.get("earlyUpdates").and_then(Value::as_array) {
                    for notification in updates {
                        self.record_session_value(
                            session_id,
                            json!({
                                "type": "acp/session_update",
                                "notification": notification,
                            }),
                        );
                    }
                }
                if let Some(request_id) = value.get("requestId").and_then(Value::as_str) {
                    self.complete_operation(request_id);
                }
            }
            "acp/session_closed" | "acp/session_deleted" => {
                if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                    self.remove_session(session_id);
                }
            }
            "acp/prompt_started" => {
                let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(request_id) = value.get("requestId").and_then(Value::as_str) {
                    self.prompt_sessions
                        .insert(request_id.to_string(), session_id.to_string());
                }
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.active_prompt = Some(event.to_string());
                }
                self.record_session_event(session_id, event);
            }
            "bridge/session_operation_started" => {
                let Some(request_id) = value.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                self.operation_sessions
                    .insert(request_id.to_string(), session_id.to_string());
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.active_operation = Some(event.to_string());
                }
                self.record_session_event(session_id, event);
            }
            "acp/prompt_complete" => {
                let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
                    return;
                };
                self.record_session_event(session_id, event);
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.active_prompt = None;
                }
                if let Some(request_id) = value.get("requestId").and_then(Value::as_str) {
                    self.prompt_sessions.remove(request_id);
                }
            }
            "acp/session_update" => {
                if let Some(session_id) = value
                    .get("notification")
                    .and_then(|notification| notification.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    self.record_session_event(session_id, event);
                }
            }
            "acp/terminal_state" => {
                if let Some(session_id) = value
                    .get("terminal")
                    .and_then(|terminal| terminal.get("sessionId"))
                    .and_then(Value::as_str)
                    && let Some(terminal_id) = value
                        .get("terminal")
                        .and_then(|terminal| terminal.get("terminalId"))
                        .and_then(Value::as_str)
                    && let Some(session) = self.sessions.get_mut(session_id)
                {
                    session
                        .terminal_states
                        .insert(terminal_id.to_string(), event.to_string());
                } else if let Some(session_id) = value
                    .get("terminal")
                    .and_then(|terminal| terminal.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    self.record_session_event(session_id, event);
                }
            }
            "acp/permission_request" => {
                let Some(permission_id) = value.get("permissionId").and_then(Value::as_str) else {
                    return;
                };
                let Some(session_id) = value
                    .get("request")
                    .and_then(|request| request.get("sessionId"))
                    .and_then(Value::as_str)
                else {
                    return;
                };
                self.permission_sessions
                    .insert(permission_id.to_string(), session_id.to_string());
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session
                        .pending_permissions
                        .insert(permission_id.to_string(), event.to_string());
                }
                self.record_session_event(session_id, event);
            }
            "acp/permission_resolved" => {
                let Some(permission_id) = value.get("permissionId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(session_id) = self.permission_sessions.remove(permission_id) {
                    self.record_session_event(&session_id, event);
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.pending_permissions.remove(permission_id);
                    }
                }
            }
            "acp/elicitation_request" => {
                let Some(elicitation_id) = value.get("elicitationId").and_then(Value::as_str)
                else {
                    return;
                };
                let request = value.get("request");
                let session_id = request
                    .and_then(|request| request.get("sessionId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(url_id) = request
                    .filter(|request| request.get("mode").and_then(Value::as_str) == Some("url"))
                    .and_then(|request| request.get("elicitationId"))
                    .and_then(Value::as_str)
                {
                    self.url_elicitation_sessions
                        .insert(url_id.to_string(), session_id.clone());
                }
                self.elicitation_sessions
                    .insert(elicitation_id.to_string(), session_id.clone());
                if let Some(session_id) = session_id.as_deref()
                    && let Some(session) = self.sessions.get_mut(session_id)
                {
                    session
                        .pending_elicitations
                        .insert(elicitation_id.to_string(), event.to_string());
                }
                self.record_optional_session_event(session_id.as_deref(), event);
            }
            "acp/elicitation_resolved" => {
                let Some(elicitation_id) = value.get("elicitationId").and_then(Value::as_str)
                else {
                    return;
                };
                let session_id = self.elicitation_sessions.remove(elicitation_id).flatten();
                self.record_optional_session_event(session_id.as_deref(), event);
                if let Some(session_id) = session_id.as_deref()
                    && let Some(session) = self.sessions.get_mut(session_id)
                    && let Some(request_event) = session.pending_elicitations.remove(elicitation_id)
                    && value
                        .get("response")
                        .and_then(|response| response.get("action"))
                        .and_then(Value::as_str)
                        == Some("accept")
                    && let Some(url_id) = url_elicitation_id(&request_event)
                {
                    session
                        .active_url_flows
                        .insert(url_id, vec![request_event, event.to_string()]);
                }
            }
            "acp/elicitation_complete" => {
                let Some(elicitation_id) = value
                    .get("notification")
                    .and_then(|notification| notification.get("elicitationId"))
                    .and_then(Value::as_str)
                else {
                    return;
                };
                let session_id = self
                    .url_elicitation_sessions
                    .remove(elicitation_id)
                    .flatten();
                self.record_optional_session_event(session_id.as_deref(), event);
                if let Some(session_id) = session_id.as_deref()
                    && let Some(session) = self.sessions.get_mut(session_id)
                {
                    session.active_url_flows.remove(elicitation_id);
                }
            }
            "acp/elicitation_aborted" => {
                let session_id = value.get("sessionId").and_then(Value::as_str);
                if let Some(elicitation_id) = value.get("elicitationId").and_then(Value::as_str) {
                    self.url_elicitation_sessions.remove(elicitation_id);
                    if let Some(session_id) = session_id
                        && let Some(session) = self.sessions.get_mut(session_id)
                    {
                        session.active_url_flows.remove(elicitation_id);
                    }
                }
                self.record_optional_session_event(session_id, event);
            }
            "acp/mode_changed" | "acp/config_changed" => {
                if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                    self.record_session_event(session_id, event);
                }
                if let Some(request_id) = value.get("requestId").and_then(Value::as_str) {
                    self.complete_operation(request_id);
                }
            }
            "bridge/error" => {
                let Some(request_id) = value.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(session_id) = self.prompt_sessions.remove(request_id) {
                    self.record_session_event(&session_id, event);
                    if let Some(session) = self.sessions.get_mut(&session_id) {
                        session.active_prompt = None;
                    }
                }
                self.complete_operation(request_id);
            }
            "bridge/auth_terminal_started"
            | "bridge/auth_terminal_output"
            | "bridge/auth_terminal_exited"
            | "acp/authenticated"
            | "acp/logged_out"
            | "acp/mcp_connection"
            | "acp/mcp_message" => self.record_global_event(event),
            _ => {}
        }
    }

    pub(crate) fn replay_events(&self) -> Vec<String> {
        let mut sessions = self.sessions.iter().collect::<Vec<_>>();
        sessions.sort_by_key(|(_, session)| session.updated_at);
        let mut replay = Vec::new();
        replay.push(
            json!({
                "type": "bridge/runtime_replay_started",
                "sessionCount": sessions.len(),
            })
            .to_string(),
        );
        let mut session_ids = Vec::with_capacity(sessions.len());
        for (session_id, session) in sessions {
            session_ids.push(session_id.clone());
            replay.extend(Self::session_events(session_id, session));
        }
        replay.extend(self.global_events.iter().cloned());
        replay.push(
            json!({
                "type": "bridge/runtime_replay_complete",
                "sessionIds": session_ids,
            })
            .to_string(),
        );
        replay
    }

    pub(crate) fn replay_session_events(&self, session_id: &str) -> Vec<String> {
        self.sessions
            .get(session_id)
            .map(|session| Self::session_events(session_id, session))
            .unwrap_or_default()
    }

    fn session_events(session_id: &str, session: &RuntimeSession) -> Vec<String> {
        let retained = session
            .events
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let recovery_count = usize::from(session.active_prompt.is_some())
            + session.pending_permissions.len()
            + session.pending_elicitations.len()
            + session.active_url_flows.len() * 2
            + session.terminal_states.len()
            + usize::from(session.active_operation.is_some());
        let mut replay = Vec::with_capacity(session.events.len() + recovery_count + 1);
        replay.push(
            json!({
                "type": "bridge/runtime_session",
                "sessionId": session_id,
                "cwd": session.cwd,
                "session": session.session,
                "truncated": session.truncated,
            })
            .to_string(),
        );
        if let Some(event) = &session.active_prompt
            && !retained.contains(event.as_str())
        {
            replay.push(event.clone());
        }
        for event in session.pending_permissions.values() {
            if !retained.contains(event.as_str()) {
                replay.push(event.clone());
            }
        }
        for event in session.pending_elicitations.values() {
            if !retained.contains(event.as_str()) {
                replay.push(event.clone());
            }
        }
        for events in session.active_url_flows.values() {
            for event in events {
                if !retained.contains(event.as_str()) {
                    replay.push(event.clone());
                }
            }
        }
        replay.extend(session.terminal_states.values().cloned());
        if let Some(event) = &session.active_operation {
            if !retained.contains(event.as_str()) {
                replay.push(event.clone());
            }
        }
        replay.extend(session.events.iter().cloned());
        replay
    }

    fn open_session(&mut self, session_id: &str, cwd: &str, response: &Value) {
        let mut session = response.as_object().cloned().unwrap_or_else(Map::new);
        session.insert("sessionId".to_string(), json!(session_id));
        self.clock = self.clock.wrapping_add(1);
        let mut runtime = RuntimeSession {
            cwd: cwd.to_string(),
            session: Value::Object(session),
            events: VecDeque::new(),
            event_bytes: 0,
            truncated: self.pending_session_truncated.remove(session_id),
            updated_at: self.clock,
            active_prompt: None,
            pending_permissions: HashMap::new(),
            pending_elicitations: HashMap::new(),
            active_url_flows: HashMap::new(),
            terminal_states: HashMap::new(),
            active_operation: None,
        };
        if let Some(pending) = self.pending_session_events.remove(session_id) {
            for event in pending {
                runtime.observe_liveness(&event);
                runtime.push(event);
            }
        }
        self.sessions.insert(session_id.to_string(), runtime);
        self.enforce_total_budget();
    }

    fn open_forked_session(
        &mut self,
        source_session_id: &str,
        session_id: &str,
        cwd: &str,
        response: &Value,
    ) {
        let inherited = self.sessions.get(source_session_id).map(|source| {
            (
                source
                    .events
                    .iter()
                    .filter(|event| inheritable_fork_event(event))
                    .map(|event| rewrite_session_event(event, source_session_id, session_id))
                    .collect::<VecDeque<_>>(),
                source.truncated,
            )
        });
        self.open_session(session_id, cwd, response);
        let Some((mut inherited_events, inherited_truncated)) = inherited else {
            return;
        };
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        inherited_events.append(&mut session.events);
        session.events = inherited_events;
        session.event_bytes = session.events.iter().map(String::len).sum();
        session.truncated |= inherited_truncated;
        while session.events.len() > MAX_RUNTIME_EVENTS_PER_SESSION
            || session.event_bytes > MAX_RUNTIME_BYTES_PER_SESSION
        {
            let Some(removed) = session.events.pop_front() else {
                break;
            };
            session.event_bytes = session.event_bytes.saturating_sub(removed.len());
            session.truncated = true;
        }
        self.enforce_total_budget();
    }

    fn record_session_value(&mut self, session_id: &str, event: Value) {
        self.record_session_event(session_id, &event.to_string());
    }

    fn record_session_event(&mut self, session_id: &str, event: &str) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.updated_at = self.clock;
            session.push(event.to_string());
            self.enforce_total_budget();
            return;
        }
        let pending = self
            .pending_session_events
            .entry(session_id.to_string())
            .or_default();
        pending.push_back(event.to_string());
        let mut pending_bytes = pending.iter().map(String::len).sum::<usize>();
        while pending.len() > MAX_PENDING_SESSION_EVENTS
            || pending_bytes > MAX_RUNTIME_BYTES_PER_SESSION
        {
            let Some(removed) = pending.pop_front() else {
                break;
            };
            pending_bytes = pending_bytes.saturating_sub(removed.len());
            self.pending_session_truncated
                .insert(session_id.to_string());
        }
    }

    fn record_optional_session_event(&mut self, session_id: Option<&str>, event: &str) {
        if let Some(session_id) = session_id {
            self.record_session_event(session_id, event);
        } else {
            self.record_global_event(event);
        }
    }

    fn record_global_event(&mut self, event: &str) {
        self.global_event_bytes = self.global_event_bytes.saturating_add(event.len());
        self.global_events.push_back(event.to_string());
        while self.global_events.len() > MAX_GLOBAL_RUNTIME_EVENTS
            || self.global_event_bytes > MAX_GLOBAL_RUNTIME_BYTES
        {
            let Some(removed) = self.global_events.pop_front() else {
                break;
            };
            self.global_event_bytes = self.global_event_bytes.saturating_sub(removed.len());
        }
    }

    fn remove_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.pending_session_events.remove(session_id);
        self.pending_session_truncated.remove(session_id);
        self.prompt_sessions.retain(|_, value| value != session_id);
        self.operation_sessions
            .retain(|_, value| value != session_id);
        self.permission_sessions
            .retain(|_, value| value != session_id);
        self.elicitation_sessions
            .retain(|_, value| value.as_deref() != Some(session_id));
        self.url_elicitation_sessions
            .retain(|_, value| value.as_deref() != Some(session_id));
    }

    fn complete_operation(&mut self, request_id: &str) {
        let Some(session_id) = self.operation_sessions.remove(request_id) else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&session_id)
            && session.active_operation.as_deref().is_some_and(|event| {
                serde_json::from_str::<Value>(event)
                    .ok()
                    .and_then(|value| value.get("requestId")?.as_str().map(str::to_string))
                    .as_deref()
                    == Some(request_id)
            })
        {
            session.active_operation = None;
        }
    }

    fn enforce_total_budget(&mut self) {
        let mut total = self
            .sessions
            .values()
            .map(|session| session.event_bytes)
            .sum::<usize>();
        while total > MAX_RUNTIME_BYTES_TOTAL {
            let Some(session_id) = self
                .sessions
                .iter()
                .filter(|(_, session)| !session.events.is_empty())
                .min_by_key(|(_, session)| session.updated_at)
                .map(|(session_id, _)| session_id.clone())
            else {
                break;
            };
            let Some(session) = self.sessions.get_mut(&session_id) else {
                break;
            };
            let Some(removed) = session.events.pop_front() else {
                break;
            };
            session.event_bytes = session.event_bytes.saturating_sub(removed.len());
            session.truncated = true;
            total = total.saturating_sub(removed.len());
        }
    }
}

impl RuntimeSession {
    fn observe_liveness(&mut self, event: &str) {
        let Ok(value) = serde_json::from_str::<Value>(event) else {
            return;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "acp/prompt_started" => self.active_prompt = Some(event.to_string()),
            "acp/prompt_complete" => {
                if same_request(&self.active_prompt, &value) {
                    self.active_prompt = None;
                }
            }
            "bridge/session_operation_started" => {
                self.active_operation = Some(event.to_string());
            }
            "bridge/error" => {
                if same_request(&self.active_prompt, &value) {
                    self.active_prompt = None;
                }
                if same_request(&self.active_operation, &value) {
                    self.active_operation = None;
                }
            }
            "acp/permission_request" => {
                if let Some(id) = value.get("permissionId").and_then(Value::as_str) {
                    self.pending_permissions
                        .insert(id.to_string(), event.to_string());
                }
            }
            "acp/permission_resolved" => {
                if let Some(id) = value.get("permissionId").and_then(Value::as_str) {
                    self.pending_permissions.remove(id);
                }
            }
            "acp/elicitation_request" => {
                if let Some(id) = value.get("elicitationId").and_then(Value::as_str) {
                    self.pending_elicitations
                        .insert(id.to_string(), event.to_string());
                }
            }
            "acp/elicitation_resolved" => {
                if let Some(id) = value.get("elicitationId").and_then(Value::as_str)
                    && let Some(request_event) = self.pending_elicitations.remove(id)
                    && value
                        .get("response")
                        .and_then(|response| response.get("action"))
                        .and_then(Value::as_str)
                        == Some("accept")
                    && let Some(url_id) = url_elicitation_id(&request_event)
                {
                    self.active_url_flows
                        .insert(url_id, vec![request_event, event.to_string()]);
                }
            }
            "acp/elicitation_complete" => {
                if let Some(id) = value
                    .get("notification")
                    .and_then(|notification| notification.get("elicitationId"))
                    .and_then(Value::as_str)
                {
                    self.active_url_flows.remove(id);
                }
            }
            "acp/elicitation_aborted" => {
                if let Some(id) = value.get("elicitationId").and_then(Value::as_str) {
                    self.active_url_flows.remove(id);
                }
            }
            "acp/terminal_state" => {
                if let Some(id) = value
                    .get("terminal")
                    .and_then(|terminal| terminal.get("terminalId"))
                    .and_then(Value::as_str)
                {
                    self.terminal_states
                        .insert(id.to_string(), event.to_string());
                }
            }
            "acp/mode_changed" | "acp/config_changed" => {
                if same_request(&self.active_operation, &value) {
                    self.active_operation = None;
                }
            }
            _ => {}
        }
    }

    fn push(&mut self, event: String) {
        self.event_bytes = self.event_bytes.saturating_add(event.len());
        self.events.push_back(event);
        while self.events.len() > MAX_RUNTIME_EVENTS_PER_SESSION
            || self.event_bytes > MAX_RUNTIME_BYTES_PER_SESSION
        {
            let Some(removed) = self.events.pop_front() else {
                break;
            };
            self.event_bytes = self.event_bytes.saturating_sub(removed.len());
            self.truncated = true;
        }
    }
}

fn same_request(active: &Option<String>, candidate: &Value) -> bool {
    let Some(candidate_id) = candidate.get("requestId").and_then(Value::as_str) else {
        return false;
    };
    active.as_deref().is_some_and(|event| {
        serde_json::from_str::<Value>(event)
            .ok()
            .and_then(|value| value.get("requestId")?.as_str().map(str::to_string))
            .as_deref()
            == Some(candidate_id)
    })
}

fn inheritable_fork_event(event: &str) -> bool {
    serde_json::from_str::<Value>(event)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|kind| {
            matches!(
                kind.as_str(),
                "acp/session_update"
                    | "acp/terminal_state"
                    | "acp/prompt_started"
                    | "acp/prompt_complete"
                    | "bridge/error"
            )
        })
}

fn url_elicitation_id(event: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(event).ok()?;
    let request = value.get("request")?;
    (request.get("mode").and_then(Value::as_str) == Some("url"))
        .then(|| request.get("elicitationId").and_then(Value::as_str))
        .flatten()
        .map(str::to_string)
}

fn rewrite_session_event(event: &str, source_session_id: &str, session_id: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(event) else {
        return event.to_string();
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return event.to_string();
    };
    let target = match kind {
        "acp/session_update" => value.get_mut("notification"),
        "acp/terminal_state" => value.get_mut("terminal"),
        "acp/permission_request" | "acp/elicitation_request" => value.get_mut("request"),
        "acp/prompt_started"
        | "acp/prompt_complete"
        | "acp/elicitation_aborted"
        | "acp/mode_changed"
        | "acp/config_changed" => Some(&mut value),
        _ => None,
    };
    if let Some(target) = target
        && target.get("sessionId").and_then(Value::as_str) == Some(source_session_id)
        && let Some(object) = target.as_object_mut()
    {
        object.insert("sessionId".to_string(), json!(session_id));
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay_type_count(events: &[String], kind: &str) -> usize {
        events
            .iter()
            .filter(|event| {
                serde_json::from_str::<Value>(event)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("type")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some(kind)
            })
            .count()
    }

    #[test]
    fn replays_independent_concurrent_session_runtime_state() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","requestId":"new-a","cwd":"/a","response":{"sessionId":"a"}}"#,
        );
        cache.update(
            r#"{"type":"acp/session_created","requestId":"new-b","cwd":"/b","response":{"sessionId":"b"}}"#,
        );
        cache.update(
            r#"{"type":"acp/prompt_started","requestId":"prompt-a","sessionId":"a","prompt":[{"type":"text","text":"A"}]}"#,
        );
        cache.update(
            r#"{"type":"acp/prompt_started","requestId":"prompt-b","sessionId":"b","prompt":[{"type":"text","text":"B"}]}"#,
        );
        cache.update(
            r#"{"type":"acp/session_update","notification":{"sessionId":"b","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"B result"}}}}"#,
        );

        let replay = cache.replay_events().join("\n");
        assert!(replay.contains(r#""sessionId":"a""#));
        assert!(replay.contains(r#""sessionId":"b""#));
        assert!(replay.contains(r#""requestId":"prompt-a""#));
        assert!(replay.contains(r#""requestId":"prompt-b""#));
        assert!(replay.contains("B result"));
    }

    #[test]
    fn removes_closed_sessions_from_runtime_replay() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","requestId":"new","cwd":"/a","response":{"sessionId":"a"}}"#,
        );
        cache.update(r#"{"type":"acp/session_closed","requestId":"close","sessionId":"a"}"#);
        let replay = cache.replay_events().join("\n");
        assert!(!replay.contains(r#""sessionId":"a""#));
        assert!(replay.contains(r#""sessionCount":0"#));
    }

    #[test]
    fn replays_connection_scoped_auth_and_mcp_runtime_events() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"bridge/auth_terminal_started","requestId":"auth","methodId":"login"}"#,
        );
        cache
            .update(r#"{"type":"bridge/auth_terminal_output","requestId":"auth","data":"Code: "}"#);
        cache.update(
            r#"{"type":"acp/mcp_connection","action":"connected","serverId":"tools","connectionId":"connection","name":"Tools"}"#,
        );

        let replay = cache.replay_events().join("\n");
        assert!(replay.contains("auth_terminal_started"));
        assert!(replay.contains("Code: "));
        assert!(replay.contains("mcp_connection"));
    }

    #[test]
    fn keeps_url_elicitation_completion_with_its_session() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","cwd":"/source","response":{"sessionId":"source"}}"#,
        );
        cache.update(
            r#"{"type":"acp/elicitation_request","elicitationId":"bridge-url","request":{"sessionId":"source","mode":"url","message":"Connect","elicitationId":"agent-url","url":"https://example.test"}}"#,
        );
        cache.update(
            r#"{"type":"acp/elicitation_resolved","requestId":"accept","elicitationId":"bridge-url","response":{"action":"accept"}}"#,
        );
        cache.update(
            r#"{"type":"acp/elicitation_complete","notification":{"elicitationId":"agent-url"}}"#,
        );

        let replay = cache.replay_session_events("source").join("\n");
        assert!(replay.contains("acp/elicitation_complete"));
        assert!(cache.global_events.is_empty());
    }

    #[test]
    fn forks_inherit_rewritten_source_session_runtime_events() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","cwd":"/source","response":{"sessionId":"source"}}"#,
        );
        cache.update(
            r#"{"type":"acp/session_update","notification":{"sessionId":"source","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Inherited"}}}}"#,
        );
        cache.update(
            r#"{"type":"acp/mode_changed","requestId":"mode","sessionId":"source","modeId":"plan"}"#,
        );
        cache.update(
            r#"{"type":"acp/session_forked","sourceSessionId":"source","cwd":"/fork","response":{"sessionId":"fork"}}"#,
        );

        let replay = cache.replay_session_events("fork").join("\n");
        assert!(replay.contains("Inherited"));
        assert!(replay.contains(r#""sessionId":"fork""#));
        assert!(!replay.contains(r#""sessionId":"source""#));
        assert!(!replay.contains("acp/mode_changed"));
    }

    #[test]
    fn active_operation_is_replayed_exactly_once() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","cwd":"/workspace","response":{"sessionId":"session"}}"#,
        );
        cache.update(
            r#"{"type":"bridge/session_operation_started","requestId":"close","sessionId":"session","operation":"close"}"#,
        );

        let replay = cache.replay_session_events("session");
        assert_eq!(
            replay_type_count(&replay, "bridge/session_operation_started"),
            1,
            "a non-evicted active operation must not be injected and replayed twice",
        );
    }

    #[test]
    fn pre_open_liveness_events_are_pinned_when_session_commits() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/permission_request","permissionId":"permission","request":{"sessionId":"session","toolCall":{"toolCallId":"tool","title":"Confirm","kind":"other","status":"pending"},"options":[]}}"#,
        );
        cache.update(
            r#"{"type":"acp/elicitation_request","elicitationId":"form","request":{"sessionId":"session","mode":"form","message":"Input","requestedSchema":{"type":"object","properties":{}}}}"#,
        );
        cache.update(
            r#"{"type":"acp/terminal_state","terminal":{"sessionId":"session","terminalId":"terminal","output":"running","truncated":false,"released":false}}"#,
        );
        cache.update(
            r#"{"type":"acp/session_created","cwd":"/workspace","response":{"sessionId":"session"}}"#,
        );

        let session = cache.sessions.get("session").expect("session committed");
        assert!(session.pending_permissions.contains_key("permission"));
        assert!(session.pending_elicitations.contains_key("form"));
        assert!(session.terminal_states.contains_key("terminal"));
    }

    #[test]
    fn active_liveness_projection_survives_complete_history_eviction() {
        let mut cache = RuntimeCache::default();
        cache.update(
            r#"{"type":"acp/session_created","cwd":"/workspace","response":{"sessionId":"session"}}"#,
        );
        cache.update(
            r#"{"type":"acp/prompt_started","requestId":"prompt","sessionId":"session","prompt":[{"type":"text","text":"hello"}]}"#,
        );
        cache.update(
            r#"{"type":"acp/permission_request","permissionId":"permission","request":{"sessionId":"session","toolCall":{"toolCallId":"tool","title":"Confirm","kind":"other","status":"pending"},"options":[]}}"#,
        );
        cache.update(
            r#"{"type":"acp/elicitation_request","elicitationId":"form","request":{"sessionId":"session","mode":"form","message":"Input","requestedSchema":{"type":"object","properties":{}}}}"#,
        );
        cache.update(
            r#"{"type":"acp/terminal_state","terminal":{"sessionId":"session","terminalId":"terminal","output":"running","truncated":false,"released":false}}"#,
        );
        cache.update(
            r#"{"type":"bridge/session_operation_started","requestId":"mutation","sessionId":"session","operation":"mode"}"#,
        );

        let session = cache
            .sessions
            .get_mut("session")
            .expect("session committed");
        session.events.clear();
        session.event_bytes = 0;
        session.truncated = true;

        let replay = cache.replay_session_events("session");
        for kind in [
            "acp/prompt_started",
            "acp/permission_request",
            "acp/elicitation_request",
            "acp/terminal_state",
            "bridge/session_operation_started",
        ] {
            assert_eq!(replay_type_count(&replay, kind), 1, "missing pinned {kind}");
        }
    }
}
