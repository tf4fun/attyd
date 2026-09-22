//! Wire-level controls must update the Hub's current state without replacing
//! omitted turn/resource fields. The source of every delta is the real registry.
use super::tests::observation_hub;
use super::*;
use crate::runtime_state::{RuntimeState, SessionOperationKind};
use crate::semantic::{SessionUpdateSemanticState, validate_and_track_session_update};
use crate::session_state::TurnAdmission;
use serde_json::Value;

const SESSION: &str = "control-hub-session";
const EPOCH: &str = "control-hub-epoch";

struct ControlSession {
    runtime: RuntimeState,
    incarnation: u64,
    operation_id: String,
    published: u64,
    validation: SessionUpdateSemanticState,
}

impl ControlSession {
    fn new() -> Self {
        let mut runtime = RuntimeState::new(EPOCH);
        let incarnation = runtime.open_new_with_replay(EPOCH, SESSION, "/workspace", json!({
            "sessionId": SESSION,
            "modes": {"currentModeId": "build", "availableModes": [
                {"id": "build", "name": "Build"}, {"id": "plan", "name": "Plan"},
            ]},
            "configOptions": [{"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": false}],
        }), Vec::new()).unwrap();
        runtime.register_new(SESSION, incarnation);
        let history = runtime
            .state(SESSION)
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = runtime
            .admit_session_turn(
                EPOCH,
                SESSION,
                incarnation,
                &history,
                "keep-running-prompt",
                vec![json!({"type": "text", "text": "Preserve this prompt"})],
            )
            .unwrap()
        else {
            panic!("fresh turn must be admitted")
        };
        let mut fixture = Self {
            runtime,
            incarnation,
            operation_id,
            published: 0,
            validation: SessionUpdateSemanticState::default(),
        };
        fixture.text(&format!("existing-body:{}", "x".repeat(32 * 1024)));
        fixture
            .runtime
            .upsert_permission(
                EPOCH,
                SESSION,
                incarnation,
                "permission",
                json!({
                    "sessionId": SESSION, "toolCall": {"toolCallId": "tool", "title": "Read file"},
                    "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
                }),
            )
            .unwrap();
        fixture.runtime.upsert_terminal(EPOCH, SESSION, incarnation, "terminal", json!({
            "terminalId": "terminal", "output": "keep terminal output", "truncated": false, "released": false,
        })).unwrap();
        // An unsolicited Agent control update may arrive while set_mode is pending.
        fixture
            .runtime
            .start_operation(
                EPOCH,
                SESSION,
                incarnation,
                "pending-mode",
                SessionOperationKind::SetMode,
                "setting",
            )
            .unwrap();
        fixture
    }

    fn text(&mut self, text: &str) {
        let update = json!({"sessionUpdate": "agent_message_chunk", "messageId": "answer", "content": {"type": "text", "text": text}});
        validate_and_track_session_update(&mut self.validation, &update).unwrap();
        self.runtime
            .append_turn_update(
                SESSION,
                self.incarnation,
                &self.operation_id,
                update.clone(),
            )
            .unwrap();
        let overlay = self
            .runtime
            .state(SESSION)
            .unwrap()
            .active_turn
            .clone()
            .unwrap();
        self.runtime
            .project_turn_update(EPOCH, SESSION, self.incarnation, &overlay, update)
            .unwrap();
    }

    fn control(&mut self, update: Value) {
        validate_and_track_session_update(&mut self.validation, &update).unwrap();
        let key = update["sessionUpdate"].as_str().unwrap().to_string();
        self.runtime
            .update_control_state(EPOCH, SESSION, self.incarnation, key, update)
            .unwrap();
    }

    async fn snapshot(&mut self, hub: &BridgeHub) {
        let snapshot = self.runtime.snapshot();
        hub.publish(
            1,
            json!({"type": "bridge/internal_runtime_snapshot", "value": snapshot}).to_string(),
        )
        .await;
        self.published = snapshot.through_seq;
        self.runtime.release_published_runtime(self.published);
    }

    async fn publish_pending(&mut self, hub: &BridgeHub) {
        let deltas = self
            .runtime
            .deltas_after(self.published)
            .expect("fixture kept a contiguous suffix");
        for delta in deltas {
            assert_eq!(delta.seq, self.published + 1);
            self.published = delta.seq;
            hub.publish(
                1,
                json!({"type": "bridge/internal_runtime_delta", "value": delta}).to_string(),
            )
            .await;
        }
        self.runtime.release_published_runtime(self.published);
        self.assert_current(hub).await;
    }

    async fn assert_current(&self, hub: &BridgeHub) {
        let actual = hub
            .state
            .lock()
            .await
            .canonical
            .snapshot
            .clone()
            .expect("valid suffix must preserve the canonical projection");
        assert_eq!(
            actual,
            self.runtime.snapshot(),
            "control delivery must reconstruct all fields at the same cut"
        );
    }
}

#[tokio::test]
async fn memory_efficiency_hub_controls_preserve_omitted_turn_and_resource_fields() {
    let (hub, mut commands) = observation_hub(0).await;
    let mut fixture = ControlSession::new();
    fixture.snapshot(&hub).await;
    let before = fixture.runtime.snapshot();
    let original = &before.sessions[SESSION];
    assert!(original.operation.is_some());
    assert!(!original.permissions.is_empty());
    assert!(!original.terminals.is_empty());
    let updates = [
        json!({"sessionUpdate": "usage_update", "used": 1, "size": 100, "cost": {"amount": 2, "currency": "USD"}}),
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
        json!({"sessionUpdate": "config_option_update", "configOptions": [
            {"type": "boolean", "id": "verbose", "name": "Verbose", "currentValue": true},
        ]}),
        json!({"sessionUpdate": "session_info_update", "title": "Keep title", "updatedAt": "before"}),
        json!({"sessionUpdate": "usage_update", "used": 2, "size": 100, "cost": null}),
        json!({"sessionUpdate": "session_info_update", "updatedAt": "after"}),
        json!({"sessionUpdate": "usage_update", "used": 3, "size": 100}),
    ];
    for (index, update) in updates.into_iter().enumerate() {
        fixture.control(update);
        fixture.publish_pending(&hub).await;
        let current = hub.state.lock().await.canonical.snapshot.clone().unwrap();
        let session = &current.sessions[SESSION];
        assert_eq!(current.through_seq, before.through_seq + index as u64 + 1);
        assert_eq!(session.revision, original.revision + index as u64 + 1);
        assert_eq!(session.incarnation, original.incarnation);
        assert_eq!(session.active_turn, original.active_turn);
        assert_eq!(session.operation, original.operation);
        assert_eq!(session.permissions, original.permissions);
        assert_eq!(session.terminals, original.terminals);
        assert_eq!(session.session, original.session);
        assert_eq!(session.phase, original.phase);
        assert_eq!(session.history_revision, original.history_revision);
        assert_eq!(session.sync_error, original.sync_error);
    }
    let current = hub.state.lock().await.canonical.snapshot.clone().unwrap();
    let controls = &current.sessions[SESSION].control_state;
    assert_eq!(controls["usage_update"]["cost"]["amount"], 2);
    assert_eq!(controls["session_info_update"]["title"], "Keep title");
    assert_eq!(controls["session_info_update"]["updatedAt"], "after");
    fixture.control(json!({"sessionUpdate": "session_info_update", "title": null}));
    fixture.publish_pending(&hub).await;
    let current = hub.state.lock().await.canonical.snapshot.clone().unwrap();
    assert_eq!(
        current.sessions[SESSION].control_state["session_info_update"]["title"],
        Value::Null
    );
    assert_eq!(
        current.sessions[SESSION].control_state["session_info_update"]["updatedAt"],
        "after"
    );
    assert!(
        commands.try_recv().is_err(),
        "valid controls must not force snapshot recovery"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn memory_efficiency_control_snapshot_suffix_and_gap_recovery_keep_one_cut() {
    let (hub, mut commands) = observation_hub(0).await;
    let mut fixture = ControlSession::new();
    let initial = fixture.runtime.snapshot();
    fixture.snapshot(&hub).await;
    fixture.text("-first");
    fixture.publish_pending(&hub).await;
    for update in [
        json!({"sessionUpdate": "usage_update", "used": 4, "size": 100}),
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
        json!({"sessionUpdate": "config_option_update", "configOptions": []}),
    ] {
        fixture.control(update);
        fixture.publish_pending(&hub).await;
    }
    fixture.text("-second");
    fixture.publish_pending(&hub).await;
    assert!(
        !initial.sessions[SESSION]
            .active_turn
            .as_ref()
            .unwrap()
            .updates[0]["content"]["text"]
            .as_str()
            .unwrap()
            .ends_with("-first-second"),
        "capturing a snapshot must keep its original cut"
    );

    // Lose a real control delta, then deliver the following real text delta.
    // The Hub must recover once instead of applying text against an older cut.
    fixture.control(json!({"sessionUpdate": "usage_update", "used": 5, "size": 100}));
    fixture.text("-after-gap");
    let pending = fixture.runtime.deltas_after(fixture.published).unwrap();
    assert_eq!(pending.len(), 2);
    hub.publish(
        1,
        json!({"type": "bridge/internal_runtime_delta", "value": pending[1]}).to_string(),
    )
    .await;
    assert!(hub.state.lock().await.canonical.snapshot.is_none());
    assert!(matches!(
        commands.try_recv().unwrap(),
        bridge::BridgeInput::RuntimeSnapshotRequest
    ));
    hub.publish(
        1,
        json!({"type": "bridge/internal_runtime_delta", "value": pending[1]}).to_string(),
    )
    .await;
    assert!(
        commands.try_recv().is_err(),
        "a shared gap must request only one snapshot"
    );
    fixture.snapshot(&hub).await;
    fixture.assert_current(&hub).await;
    let recovery_cut = fixture.published;
    fixture.control(json!({"sessionUpdate": "usage_update", "used": 6, "size": 100}));
    fixture.publish_pending(&hub).await;
    assert_eq!(fixture.published, recovery_cut + 1);
    fixture.text("-recovered");
    fixture.publish_pending(&hub).await;
    let current = hub.state.lock().await.canonical.snapshot.clone().unwrap();
    assert!(
        current.sessions[SESSION]
            .active_turn
            .as_ref()
            .unwrap()
            .updates[0]["content"]["text"]
            .as_str()
            .unwrap()
            .ends_with("-first-second-after-gap-recovered")
    );
    assert!(commands.try_recv().is_err());
    hub.shutdown().await;
}
