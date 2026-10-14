//! The per-event convergence gate on the session registry (docs/state-sync.md
//! §4.1) and acceptance cases 2, 5, 13 and 18 (client-api-redesign.md §11).
//!
//! A replica applies every committed event, one at a time, at the wire level,
//! and is compared with the published document after each one (I1, I2). The
//! lagging, coalesced and reconnecting replicas need the Hub and land with it
//! (migration step 5).
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::canon::{Attachments, ChainHash, normalize};
use super::diff;
use super::documents::{AgentInfo, AgentLaunch, AgentPhase, OperationKind, Scope};
use super::publish::Publisher;
use super::{Op, canon::Block};
use crate::runtime_state::SessionOperationKind;
use crate::session_registry::SessionRegistry;

const EPOCH: &str = "epoch";

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

/// A client's copy of one scope, built from wire events only.
#[derive(Debug, Clone, Default)]
struct Replica {
    live: Value,
    log: Vec<Value>,
}

impl Replica {
    fn hash_at(&self, index: usize) -> Option<String> {
        if index == 0 {
            return Some(ChainHash::ZERO.wire());
        }
        self.log
            .get(index - 1)
            .and_then(|entry| entry["hash"].as_str())
            .map(String::from)
    }

    fn apply(&mut self, event: &Value) -> Result<(), String> {
        if let Some(log) = event.get("log") {
            let from = log["from"].as_u64().ok_or("log.from")? as usize;
            if from > self.log.len() || self.hash_at(from).as_deref() != log["prev"].as_str() {
                return Err(format!("log change does not follow the replica: {event}"));
            }
            self.log.truncate(from);
            self.log.extend(
                log["entries"]
                    .as_array()
                    .ok_or("log.entries")?
                    .iter()
                    .cloned(),
            );
        }
        if let Some(live) = event.get("live") {
            if let Some(replace) = live.get("replace") {
                self.live = replace.clone();
            } else {
                let ops: Vec<Op> = serde_json::from_value(live["patch"].clone())
                    .map_err(|error| format!("patch: {error}"))?;
                for op in &ops {
                    diff::apply(&mut self.live, op)
                        .map_err(|error| format!("{op:?} does not apply: {error:?}"))?;
                }
            }
        }
        Ok(())
    }
}

/// Drives the registry, commits, and checks a replica after every event.
struct Gate {
    registry: SessionRegistry,
    publisher: Publisher,
    agent: AgentInfo,
    catalog_revision: u64,
    replicas: BTreeMap<String, Replica>,
}

impl Gate {
    fn new() -> Self {
        let mut gate = Self {
            registry: SessionRegistry::new(EPOCH),
            publisher: Publisher::default(),
            agent: agent(),
            catalog_revision: 0,
            replicas: BTreeMap::new(),
        };
        gate.commit();
        gate
    }

    /// Runs the commit step and applies its events one at a time.
    fn commit(&mut self) -> Vec<Value> {
        let events = self
            .publisher
            .commit(&mut self.registry, &self.agent, self.catalog_revision);
        let mut seen = std::collections::HashSet::new();
        let events: Vec<Value> = events
            .iter()
            .map(|event| {
                assert!(
                    seen.insert(event.scope().to_string()),
                    "two events for {} in one commit",
                    event.scope()
                );
                serde_json::to_value(event).unwrap()
            })
            .collect();
        for event in &events {
            let scope = event["scope"].as_str().unwrap().to_string();
            if event.get("end").is_some() {
                self.replicas.remove(&scope);
                assert!(self.publisher.document(&scope).is_none(), "{scope} ended");
                continue;
            }
            let replica = self.replicas.entry(scope.clone()).or_default();
            replica
                .apply(event)
                .unwrap_or_else(|error| panic!("{error}"));
            self.assert_replica(&scope);
        }
        let mut published = self.publisher.scopes();
        published.sort();
        assert_eq!(
            self.replicas.keys().cloned().collect::<Vec<_>>(),
            published,
            "every published scope reached the replica"
        );
        assert!(
            !self
                .publisher
                .diverges(&self.registry, &self.agent, self.catalog_revision),
            "a commit leaves nothing unpublished"
        );
        events
    }

    fn assert_replica(&self, scope: &str) {
        let document = self.publisher.document(scope).expect("published scope");
        let replica = &self.replicas[scope];
        assert_eq!(replica.live, document.live, "live of {scope}");
        if let Some(log) = document.log {
            assert_eq!(
                replica.log,
                serde_json::to_value(log)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .clone(),
                "log of {scope}"
            );
        }
    }

    fn live(&self, scope: &str) -> Value {
        self.publisher
            .document(scope)
            .expect("published scope")
            .live
    }

    fn log(&self, scope: &str) -> Option<Value> {
        self.publisher
            .document(scope)
            .expect("published scope")
            .log
            .map(|log| serde_json::to_value(log).unwrap())
    }

    /// Opens a new session, as `session/new` does.
    fn open(&mut self, session_id: &str, response: Value) -> u64 {
        let incarnation = self
            .registry
            .open_new(EPOCH, session_id, format!("/{session_id}"), response)
            .unwrap();
        self.commit();
        incarnation
    }

    /// Holds a session by loading it with `replay`, as a first load does.
    fn load(&mut self, session_id: &str, replay: &[Value]) -> u64 {
        let incarnation = self
            .registry
            .start_attachment(
                EPOCH,
                session_id,
                format!("/{session_id}"),
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        self.registry.register_cold(session_id, incarnation);
        self.commit();
        self.registry
            .begin_load(session_id, incarnation, "load")
            .unwrap();
        for update in replay {
            self.registry
                .append_load_update(session_id, incarnation, "load", update.clone())
                .unwrap();
        }
        self.registry
            .commit_load(session_id, incarnation, "load")
            .unwrap();
        self.registry
            .complete_attachment(
                EPOCH,
                session_id,
                incarnation,
                "load",
                SessionOperationKind::Load,
                json!({}),
            )
            .unwrap();
        incarnation
    }

    /// Reloads a held session with `replay`, as after an Agent restart.
    fn reload(&mut self, session_id: &str, incarnation: u64, replay: &[Value]) {
        self.registry
            .start_reload(
                EPOCH,
                session_id,
                incarnation,
                "reload",
                SessionOperationKind::Load,
            )
            .unwrap();
        self.registry
            .begin_load(session_id, incarnation, "reload")
            .unwrap();
        for update in replay {
            self.registry
                .append_load_update(session_id, incarnation, "reload", update.clone())
                .unwrap();
        }
        self.registry
            .commit_load(session_id, incarnation, "reload")
            .unwrap();
        self.registry
            .complete_reload(EPOCH, session_id, incarnation, "reload", json!({}))
            .unwrap();
    }

    /// Installs a prompt operation and starts its turn.
    fn prompt(
        &mut self,
        session_id: &str,
        incarnation: u64,
        rpc: &str,
        prompt: Vec<Value>,
    ) -> String {
        let op = self.registry.install_prompt(session_id, &prompt).unwrap();
        self.registry
            .start_prompt(EPOCH, session_id, incarnation, rpc, prompt)
            .unwrap();
        op
    }

    fn update(&mut self, session_id: &str, incarnation: u64, update: Value) {
        self.registry
            .append_runtime_update_for_test(EPOCH, session_id, incarnation, update)
            .unwrap();
    }
}

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn image(data: &str) -> Value {
    json!({ "type": "image", "mimeType": "image/png", "data": data })
}

fn attachment_id(block: &Value) -> String {
    let mut attachments = Attachments::default();
    match normalize(std::slice::from_ref(block), &mut attachments).remove(0) {
        Block::Attachment(reference) => reference.id,
        Block::Content(_) => panic!("not an attachment"),
    }
}

fn user(content: Value) -> Value {
    json!({ "sessionUpdate": "user_message_chunk", "content": content })
}

fn agent_chunk(content: Value) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "content": content })
}

fn single(events: &[Value], scope: &str) -> Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["scope"] == scope)
        .collect();
    assert_eq!(matching.len(), 1, "one event for {scope}: {events:?}");
    matching[0].clone()
}

// ---------------------------------------------------------------------------
// Scopes and first operations

#[test]
fn global_and_catalog_start_with_a_completed_none_and_an_empty_log() {
    let gate = Gate::new();
    for scope in ["global", "catalog"] {
        let live = gate.live(scope);
        assert_eq!(live["op"]["kind"], "none");
        assert_eq!(live["op"]["status"], "completed");
        assert_eq!(live["op"]["id"].as_str().unwrap().len(), 22);
        assert_eq!(gate.log(scope), Some(json!([])));
    }
    assert_ne!(
        gate.live("global")["op"]["id"],
        gate.live("catalog")["op"]["id"]
    );
    assert_eq!(gate.live("catalog")["revision"], "0");
}

#[test]
fn a_created_session_starts_with_a_completed_open_and_a_known_empty_log() {
    let mut gate = Gate::new();
    gate.open("s", json!({ "sessionId": "s" }));
    let live = gate.live("session/s");
    assert_eq!(live["op"]["kind"], "open");
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["sessionId"], "s");
    assert_eq!(live["cwd"], "/s");
    assert_eq!(gate.log("session/s"), Some(json!([])));
    let summary = &gate.live("global")["sessions"]["s"];
    assert_eq!(
        summary["op"],
        json!({ "kind": "open", "status": "completed" })
    );
    assert_eq!(
        summary["pending"],
        json!({ "interactions": 0, "urlFlows": 0 })
    );
}

#[test]
fn a_first_load_runs_with_an_unknown_log_and_ends_with_the_replayed_log() {
    let mut gate = Gate::new();
    let incarnation = gate
        .registry
        .start_attachment(EPOCH, "s", "/s", "load", SessionOperationKind::Load)
        .unwrap();
    gate.registry.register_cold("s", incarnation);
    let events = gate.commit();
    let aligned = single(&events, "session/s");
    assert!(aligned.get("log").is_none(), "an unknown log is not sent");
    assert_eq!(aligned["live"]["replace"]["op"]["kind"], "load");
    assert_eq!(aligned["live"]["replace"]["op"]["status"], "running");
    assert_eq!(
        aligned["live"]["replace"]["op"]["input"],
        json!({ "cwd": "/s" })
    );
    assert_eq!(gate.log("session/s"), None);

    gate.registry.begin_load("s", incarnation, "load").unwrap();
    for update in [user(text("hi")), agent_chunk(text("hello"))] {
        gate.registry
            .append_load_update("s", incarnation, "load", update)
            .unwrap();
    }
    gate.registry.commit_load("s", incarnation, "load").unwrap();
    gate.registry
        .complete_attachment(
            EPOCH,
            "s",
            incarnation,
            "load",
            SessionOperationKind::Load,
            json!({}),
        )
        .unwrap();
    let events = gate.commit();
    let end = single(&events, "session/s");
    assert_eq!(end["log"]["from"], 0);
    assert_eq!(end["log"]["entries"].as_array().unwrap().len(), 1);
    assert!(
        end["live"].get("patch").is_some(),
        "the load ends in the same event"
    );
    assert_eq!(gate.live("session/s")["op"]["status"], "completed");
}

#[test]
fn a_failed_first_load_ends_the_scope_with_load_failed() {
    let mut gate = Gate::new();
    let incarnation = gate
        .registry
        .start_attachment(EPOCH, "s", "/s", "load", SessionOperationKind::Load)
        .unwrap();
    gate.registry.register_cold("s", incarnation);
    gate.commit();
    gate.registry
        .fail_attachment(
            EPOCH,
            "s",
            incarnation,
            "load",
            SessionOperationKind::Load,
            json!({ "code": -32002, "message": "no such session" }),
        )
        .unwrap();
    let events = gate.commit();
    let end = single(&events, "session/s");
    assert_eq!(end["end"]["reason"], "load_failed");
    assert_eq!(end["end"]["error"]["code"], "agent_error");
    assert_eq!(end["end"]["error"]["message"], "no such session");
    assert!(gate.live("global")["sessions"].get("s").is_none());
}

// ---------------------------------------------------------------------------
// Acceptance 2: commit (S2)

#[test]
fn a_completed_turn_appends_its_entry_removes_its_process_and_ends_its_op_in_one_event() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let op = gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    let events = gate.commit();
    let started = single(&events, "session/s");
    assert!(started.get("log").is_none());
    let live = gate.live("session/s");
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["op"]["kind"], "prompt");
    assert_eq!(live["op"]["status"], "running");
    assert_eq!(live["op"]["input"], json!({ "prompt": [text("hi")] }));
    assert_eq!(live["op"]["process"], json!([]));

    gate.update("s", incarnation, agent_chunk(text("hel")));
    gate.commit();
    gate.update(
        "s",
        incarnation,
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "t", "title": "read", "status": "pending",
        }),
    );
    gate.commit();
    gate.update(
        "s",
        incarnation,
        json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed",
        }),
    );
    let folded = single(&gate.commit(), "session/s");
    assert!(
        folded["live"]["patch"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op["path"].as_str().unwrap().starts_with("/op/process/")),
        "a folded update is set in place: {folded}"
    );
    gate.update("s", incarnation, agent_chunk(text("lo")));
    gate.commit();
    assert_eq!(
        gate.live("session/s")["op"]["process"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    gate.registry
        .complete_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "stopReason": "end_turn" }),
        )
        .unwrap();
    let events = gate.commit();
    let commit = single(&events, "session/s");
    assert_eq!(commit["log"]["from"], 0);
    assert_eq!(commit["log"]["prev"], ChainHash::ZERO.wire());
    assert_eq!(
        commit["log"]["entries"][0]["turn"],
        json!({ "prompt": [text("hi")], "reply": [text("hello")] })
    );
    assert!(
        commit["live"].get("patch").is_some(),
        "the same event ends the op"
    );
    let live = gate.live("session/s");
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["op"]["result"], json!({ "stopReason": "end_turn" }));
    assert!(
        live["op"].get("process").is_none(),
        "the commit removes the process"
    );
    assert_eq!(gate.log("session/s").unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn a_failed_turn_appends_nothing_and_keeps_its_prompt_and_error_on_op() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    gate.update("s", incarnation, agent_chunk(text("partial")));
    gate.commit();
    gate.registry
        .fail_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "code": -32603, "message": "boom" }),
        )
        .unwrap();
    let events = gate.commit();
    let failed = single(&events, "session/s");
    assert!(failed.get("log").is_none(), "a failed turn appends nothing");
    let live = gate.live("session/s");
    assert_eq!(live["op"]["status"], "failed");
    assert_eq!(live["op"]["error"]["code"], "agent_error");
    assert_eq!(live["op"]["error"]["message"], "boom");
    assert_eq!(live["op"]["error"]["data"]["code"], -32603);
    assert_eq!(live["op"]["input"], json!({ "prompt": [text("hi")] }));
    assert!(live["op"].get("process").is_none());
    assert_eq!(gate.log("session/s"), Some(json!([])));
}

#[test]
fn a_requested_cancel_shows_on_the_running_prompt_and_a_cancelled_stop_completes_it() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    gate.commit();
    gate.registry
        .request_cancel(EPOCH, "s", incarnation)
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["op"]["cancelRequested"], true);
    assert_eq!(gate.live("session/s")["op"]["status"], "running");
    gate.registry
        .complete_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "stopReason": "cancelled" }),
        )
        .unwrap();
    gate.commit();
    let live = gate.live("session/s");
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["op"]["result"]["stopReason"], "cancelled");
    assert_eq!(gate.log("session/s").unwrap().as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Acceptance 5: live changes (S1, S4)

#[test]
fn interactions_and_controls_change_in_live_while_the_log_stays() {
    let mut gate = Gate::new();
    let incarnation = gate.load("s", &[user(text("hi")), agent_chunk(text("hello"))]);
    gate.commit();
    let head = gate.log("session/s").unwrap();
    let request = json!({ "sessionId": "s", "toolCall": { "toolCallId": "t" }, "options": [] });

    gate.registry
        .upsert_permission(EPOCH, "s", incarnation, "p1", request.clone())
        .unwrap();
    let asked = single(&gate.commit(), "session/s");
    assert!(asked.get("log").is_none());
    let live = gate.live("session/s");
    assert_eq!(live["op"]["kind"], "agent_request");
    assert_eq!(live["op"]["status"], "running");
    assert_eq!(
        live["op"]["interactions"]["p1"],
        json!({ "interactionId": "p1", "kind": "permission", "request": request })
    );
    let agent_request = live["op"]["id"].clone();
    assert_eq!(
        gate.live("global")["sessions"]["s"]["pending"]["interactions"],
        1
    );

    gate.registry
        .update_control_state(
            EPOCH,
            "s",
            incarnation,
            "session_info_update",
            json!({ "sessionUpdate": "session_info_update", "title": "Renamed" }),
        )
        .unwrap();
    let renamed = single(&gate.commit(), "session/s");
    assert!(renamed.get("log").is_none());
    assert_eq!(
        gate.live("session/s")["controls"]["info"]["title"],
        "Renamed"
    );
    assert_eq!(gate.live("global")["sessions"]["s"]["title"], "Renamed");

    gate.registry
        .resolve_permission(EPOCH, "s", incarnation, "p1")
        .unwrap();
    let answered = single(&gate.commit(), "session/s");
    assert!(answered.get("log").is_none());
    let live = gate.live("session/s");
    assert_eq!(live["op"]["id"], agent_request);
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["op"]["interactions"], json!({}));
    assert_eq!(
        gate.log("session/s").unwrap(),
        head,
        "the log hash stays the same"
    );
}

#[test]
fn an_interaction_joins_the_running_prompt() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let op = gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    gate.commit();
    gate.registry
        .upsert_permission(EPOCH, "s", incarnation, "p1", json!({ "sessionId": "s" }))
        .unwrap();
    gate.commit();
    let live = gate.live("session/s");
    assert_eq!(live["op"]["id"], op);
    assert!(live["op"]["interactions"].get("p1").is_some());
    gate.registry
        .complete_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "stopReason": "end_turn" }),
        )
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["op"]["interactions"], json!({}));
}

// ---------------------------------------------------------------------------
// Acceptance 13: attachments (§8)

#[test]
fn attachments_are_readable_while_log_or_live_references_them() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let (sent, said, used) = (image("c2VudA"), image("c2FpZA"), image("dXNlZA"));
    let (sent_id, said_id, used_id) = (
        attachment_id(&sent),
        attachment_id(&said),
        attachment_id(&used),
    );
    gate.prompt("s", incarnation, "rpc", vec![text("look"), sent.clone()]);
    gate.commit();
    assert_eq!(
        gate.live("session/s")["op"]["input"]["prompt"][1]["type"],
        "attachment"
    );
    assert_eq!(gate.publisher.attachment("s", &sent_id), Some(sent.clone()));

    gate.update("s", incarnation, agent_chunk(said.clone()));
    gate.update(
        "s",
        incarnation,
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "t", "title": "shot",
            "content": [{ "type": "content", "content": used.clone() }],
        }),
    );
    gate.commit();
    let process = gate.live("session/s")["op"]["process"].clone();
    assert_eq!(process[0]["content"]["type"], "attachment");
    assert_eq!(process[0]["content"]["id"], said_id);
    assert_eq!(process[1]["content"][0]["content"]["id"], used_id);
    assert_eq!(gate.publisher.attachment("s", &said_id), Some(said.clone()));
    assert_eq!(gate.publisher.attachment("s", &used_id), Some(used.clone()));

    gate.registry
        .complete_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "stopReason": "end_turn" }),
        )
        .unwrap();
    gate.commit();
    assert_eq!(
        gate.publisher.attachment("s", &sent_id),
        Some(sent),
        "the log's prompt"
    );
    assert_eq!(
        gate.publisher.attachment("s", &said_id),
        Some(said),
        "the log's reply"
    );
    assert_eq!(
        gate.publisher.attachment("s", &used_id),
        None,
        "only the released process referenced it"
    );
    assert_eq!(gate.publisher.attachment("other", &sent_id), None);
}

// ---------------------------------------------------------------------------
// Acceptance 18: controls (§4.2)

fn controlled(gate: &mut Gate) -> u64 {
    gate.open(
        "s",
        json!({
            "sessionId": "s",
            "modes": {
                "currentModeId": "ask",
                "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }],
            },
            "configOptions": [{ "id": "model", "currentValue": "a" }],
        }),
    )
}

#[test]
fn a_set_mode_acknowledged_without_a_notification_shows_the_mode_when_its_op_ends() {
    let mut gate = Gate::new();
    let incarnation = controlled(&mut gate);
    assert_eq!(
        gate.live("session/s")["controls"]["modes"]["currentModeId"],
        "ask"
    );
    let op = gate
        .registry
        .install_operation(
            &Scope::Session("s".into()),
            OperationKind::SetMode,
            json!({ "modeId": "code" }),
        )
        .unwrap();
    gate.registry
        .start_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetMode,
            "request",
        )
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["op"]["status"], "running");
    gate.registry
        .complete_control_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetMode,
            "current_mode_update",
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
            json!({}),
        )
        .unwrap();
    let ended = single(&gate.commit(), "session/s");
    let paths: Vec<_> = ended["live"]["patch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        paths.iter().any(|path| path.starts_with("/controls")),
        "{paths:?}"
    );
    assert!(
        paths.iter().any(|path| path.starts_with("/op")),
        "{paths:?}"
    );
    let live = gate.live("session/s");
    assert_eq!(live["op"]["id"], op);
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["controls"]["modes"]["currentModeId"], "code");
    assert_eq!(
        live["controls"]["modes"]["availableModes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_set_config_acknowledged_without_a_notification_shows_the_confirmed_options() {
    let mut gate = Gate::new();
    let incarnation = controlled(&mut gate);
    gate.registry
        .install_operation(
            &Scope::Session("s".into()),
            OperationKind::SetConfig,
            json!({ "configId": "model", "value": "b" }),
        )
        .unwrap();
    gate.registry
        .start_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetConfig,
            "request",
        )
        .unwrap();
    gate.commit();
    let confirmed = json!([{ "id": "model", "currentValue": "b" }]);
    gate.registry
        .complete_control_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetConfig,
            "config_option_update",
            json!({ "sessionUpdate": "config_option_update", "configOptions": confirmed }),
            json!({}),
        )
        .unwrap();
    gate.commit();
    let live = gate.live("session/s");
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(live["controls"]["configOptions"], confirmed);
}

#[test]
fn a_failed_set_mode_changes_no_control() {
    let mut gate = Gate::new();
    let incarnation = controlled(&mut gate);
    let before = gate.live("session/s")["controls"].clone();
    gate.registry
        .install_operation(
            &Scope::Session("s".into()),
            OperationKind::SetMode,
            json!({ "modeId": "code" }),
        )
        .unwrap();
    gate.registry
        .start_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetMode,
            "request",
        )
        .unwrap();
    gate.commit();
    gate.registry
        .fail_operation(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            SessionOperationKind::SetMode,
            json!({ "message": "refused" }),
        )
        .unwrap();
    gate.commit();
    let live = gate.live("session/s");
    assert_eq!(live["op"]["status"], "failed");
    assert_eq!(live["op"]["error"]["message"], "refused");
    assert_eq!(live["controls"], before);
}

#[test]
fn control_updates_fold_after_the_response() {
    let mut gate = Gate::new();
    let incarnation = controlled(&mut gate);
    gate.registry
        .update_control_state(
            EPOCH,
            "s",
            incarnation,
            "current_mode_update",
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
        )
        .unwrap();
    gate.registry
        .update_control_state(
            EPOCH,
            "s",
            incarnation,
            "usage_update",
            json!({ "sessionUpdate": "usage_update", "used": 1, "size": 9, "cost": { "amount": 1, "currency": "USD" } }),
        )
        .unwrap();
    gate.registry
        .update_control_state(
            EPOCH,
            "s",
            incarnation,
            "usage_update",
            json!({ "sessionUpdate": "usage_update", "used": 2, "size": 9 }),
        )
        .unwrap();
    gate.commit();
    let controls = gate.live("session/s")["controls"].clone();
    assert_eq!(controls["modes"]["currentModeId"], "code");
    assert_eq!(
        controls["usage"],
        json!({ "used": 2, "size": 9, "cost": { "amount": 1, "currency": "USD" } })
    );
}

// ---------------------------------------------------------------------------
// Occupation (S5) and ends

#[test]
fn a_delete_occupies_its_session_and_ends_it_with_deleted() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let op = gate
        .registry
        .install_operation(
            &Scope::Global,
            OperationKind::Delete,
            json!({ "sessionId": "s" }),
        )
        .unwrap();
    assert_eq!(
        gate.registry.install_prompt("s", &[text("hi")]),
        Err(super::registry::InstallError::NotFree),
        "an occupied session is not free"
    );
    gate.registry
        .start_delete(EPOCH, "s", incarnation, "del")
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("global")["op"]["id"], op);
    assert_eq!(gate.live("session/s")["op"]["id"], op, "the occupying op");
    assert_eq!(gate.live("session/s")["op"]["kind"], "delete");
    assert_eq!(gate.live("global")["sessions"]["s"]["op"]["kind"], "delete");

    gate.registry
        .delete_close_succeeded(EPOCH, "s", incarnation, "del")
        .unwrap();
    gate.commit();
    gate.registry
        .complete_delete(EPOCH, "s", incarnation, "del")
        .unwrap();
    let events = gate.commit();
    assert_eq!(
        single(&events, "session/s")["end"],
        json!({ "reason": "deleted" })
    );
    let global = gate.live("global");
    assert_eq!(global["op"]["status"], "completed");
    assert!(global["sessions"].get("s").is_none());
    assert!(gate.registry.scopes.occupied.is_empty());
}

#[test]
fn a_failed_delete_frees_its_session_and_keeps_its_error_on_global() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let open = gate.live("session/s")["op"]["id"].clone();
    gate.registry
        .install_operation(
            &Scope::Global,
            OperationKind::Delete,
            json!({ "sessionId": "s" }),
        )
        .unwrap();
    gate.registry
        .start_delete(EPOCH, "s", incarnation, "del")
        .unwrap();
    gate.commit();
    gate.registry
        .fail_operation(
            EPOCH,
            "s",
            incarnation,
            "del",
            SessionOperationKind::Delete,
            json!({ "message": "busy" }),
        )
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("global")["op"]["status"], "failed");
    assert_eq!(gate.live("global")["op"]["error"]["message"], "busy");
    assert_eq!(gate.live("session/s")["op"]["id"], open, "its own op again");
    assert!(gate.registry.install_prompt("s", &[text("hi")]).is_ok());
}

#[test]
fn a_running_operation_is_not_replaced() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    assert_eq!(
        gate.registry.install_operation(
            &Scope::Session("s".into()),
            OperationKind::SetMode,
            json!({ "modeId": "code" }),
        ),
        Err(super::registry::InstallError::NotFree)
    );
    assert_eq!(
        gate.registry.install_operation(
            &Scope::Session("x".into()),
            OperationKind::SetMode,
            json!({})
        ),
        Err(super::registry::InstallError::NotFound)
    );
}

// ---------------------------------------------------------------------------
// Gate scenarios (§4.1)

#[test]
fn reloads_with_an_equal_replay_change_no_log_and_a_diverging_one_rebuilds_from_the_common_prefix()
{
    let first = [
        user(text("one")),
        agent_chunk(text("1")),
        user(text("two")),
        agent_chunk(text("2")),
    ];
    let mut gate = Gate::new();
    let incarnation = gate.load("s", &first);
    gate.commit();
    let log = gate.log("session/s").unwrap();
    assert_eq!(log.as_array().unwrap().len(), 2);

    // The same messages in other chunks: the same entries (I5).
    let rechunked = [
        user(text("o")),
        user(text("ne")),
        agent_chunk(text("1")),
        user(text("two")),
        agent_chunk(text("2")),
    ];
    gate.reload("s", incarnation, &rechunked);
    let events = gate.commit();
    assert!(
        single(&events, "session/s").get("log").is_none(),
        "{events:?}"
    );
    assert_eq!(gate.log("session/s").unwrap(), log);

    let diverged = [
        user(text("one")),
        agent_chunk(text("1")),
        user(text("other")),
        agent_chunk(text("3")),
        user(text("more")),
    ];
    gate.reload("s", incarnation, &diverged);
    let events = gate.commit();
    let rebuilt = single(&events, "session/s");
    assert_eq!(rebuilt["log"]["from"], 1);
    assert_eq!(rebuilt["log"]["prev"], log[0]["hash"]);
    assert_eq!(rebuilt["log"]["entries"].as_array().unwrap().len(), 2);
}

#[test]
fn terminals_roll_their_bounded_tail_with_trim_and_append_and_leave_on_release() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let chunk = |text: &str| json!({ "output": text, "outputAppend": true, "retainedBytes": 6, "released": false });
    gate.registry
        .upsert_terminal(EPOCH, "s", incarnation, "t", chunk("abcd"))
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["terminals"]["t"]["output"], "abcd");
    gate.registry
        .upsert_terminal(EPOCH, "s", incarnation, "t", chunk("efgh"))
        .unwrap();
    let rolled = single(&gate.commit(), "session/s");
    let kinds: Vec<_> = rolled["live"]["patch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["trim", "append"]);
    assert_eq!(gate.live("session/s")["terminals"]["t"]["output"], "cdefgh");
    gate.registry
        .upsert_terminal(
            EPOCH,
            "s",
            incarnation,
            "t",
            json!({ "output": "", "outputAppend": true, "released": true }),
        )
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["terminals"], json!({}));
}

#[test]
fn url_flows_appear_on_accept_and_leave_on_completion() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    let request = json!({ "mode": "url", "elicitationId": "e", "url": "https://example.test" });
    gate.registry
        .upsert_elicitation(EPOCH, Some(("s", incarnation)), "i1", request.clone())
        .unwrap();
    gate.commit();
    assert_eq!(
        gate.live("session/s")["op"]["interactions"]["i1"]["kind"],
        "elicitation"
    );
    gate.registry
        .resolve_elicitation(EPOCH, Some(("s", incarnation)), "i1", Some("e"))
        .unwrap();
    gate.commit();
    let live = gate.live("session/s");
    assert_eq!(
        live["urlFlows"]["i1"],
        json!({ "interactionId": "i1", "elicitationId": "e", "request": request })
    );
    assert_eq!(live["op"]["status"], "completed");
    assert_eq!(
        gate.live("global")["sessions"]["s"]["pending"]["urlFlows"],
        1
    );
    gate.registry
        .settle_url_registration(
            EPOCH,
            Some(("s", incarnation)),
            "e",
            "i1",
            crate::runtime_state::UrlFlowStatus::Completed,
        )
        .unwrap();
    gate.commit();
    assert_eq!(gate.live("session/s")["urlFlows"], json!({}));
}

#[test]
fn request_scoped_elicitations_run_as_an_agent_request_of_global() {
    let mut gate = Gate::new();
    let request = json!({ "mode": "url", "elicitationId": "e", "url": "https://example.test" });
    gate.registry
        .upsert_elicitation(EPOCH, None, "i1", request.clone())
        .unwrap();
    gate.commit();
    let global = gate.live("global");
    assert_eq!(global["op"]["kind"], "agent_request");
    assert_eq!(global["op"]["interactions"]["i1"]["request"], request);
    gate.registry
        .resolve_elicitation(EPOCH, None, "i1", Some("e"))
        .unwrap();
    gate.commit();
    let global = gate.live("global");
    assert_eq!(global["op"]["status"], "completed");
    assert_eq!(global["urlFlows"]["i1"]["elicitationId"], "e");
}

#[test]
fn the_gate_reports_a_transition_that_was_not_committed() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    assert!(
        !gate
            .publisher
            .diverges(&gate.registry, &gate.agent, gate.catalog_revision)
    );
    gate.registry
        .update_control_state(
            EPOCH,
            "s",
            incarnation,
            "current_mode_update",
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
        )
        .unwrap();
    assert!(
        gate.publisher
            .diverges(&gate.registry, &gate.agent, gate.catalog_revision),
        "an uncommitted change is reported"
    );
    gate.catalog_revision = 1;
    gate.commit();
    assert_eq!(gate.live("catalog")["revision"], "1");
}

#[test]
fn every_event_round_trips_its_documented_wire_shape() {
    let mut gate = Gate::new();
    let incarnation = gate.open("s", json!({ "sessionId": "s" }));
    gate.prompt("s", incarnation, "rpc", vec![text("hi")]);
    gate.update("s", incarnation, agent_chunk(text("hey")));
    gate.registry
        .complete_prompt(
            EPOCH,
            "s",
            incarnation,
            "rpc",
            json!({ "stopReason": "end_turn" }),
        )
        .unwrap();
    for event in gate.commit() {
        let keys: Vec<_> = event.as_object().unwrap().keys().cloned().collect();
        assert!(
            [
                vec!["live", "log", "scope"],
                vec!["live", "scope"],
                vec!["log", "scope"],
                vec!["end", "scope"],
            ]
            .contains(&keys.iter().map(String::as_str).collect::<Vec<_>>()),
            "{event}"
        );
    }
}
