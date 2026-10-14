//! The reduced control state of a session (protocol §4.5).
//!
//! Sources fold in order: early updates, then the `session/new`, load, resume
//! or fork response, then later updates. The response overrides exactly the
//! fields it provides; every update kind owns disjoint fields. A holder calls
//! `settle` at each response, then `fold_update` for every later update.
use std::collections::BTreeMap;

use serde_json::Value;

use super::{SessionControls, SessionModes, SessionUsage};

impl SessionControls {
    /// Folds one `SessionUpdate`. Returns `false` for non-control kinds, which
    /// leave the state unchanged.
    pub(crate) fn fold_update(&mut self, update: &Value) -> bool {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("current_mode_update") => {
                let Some(mode) = update.get("currentModeId").and_then(Value::as_str) else {
                    return false;
                };
                match &mut self.modes {
                    Some(modes) => modes.current_mode_id = mode.to_string(),
                    None => {
                        self.modes = Some(SessionModes {
                            current_mode_id: mode.to_string(),
                            available_modes: Vec::new(),
                        })
                    }
                }
            }
            Some("config_option_update") => {
                let Some(options) = update.get("configOptions").and_then(Value::as_array) else {
                    return false;
                };
                self.config_options = Some(options.clone());
            }
            Some("available_commands_update") => {
                let Some(commands) = update.get("availableCommands").and_then(Value::as_array)
                else {
                    return false;
                };
                self.commands = commands.clone();
            }
            Some("session_info_update") => {
                // Absent keeps the field, `null` clears it, a value sets it.
                if let Some(title) = update.get("title") {
                    self.info.title = title.as_str().map(str::to_owned);
                }
                if let Some(updated_at) = update.get("updatedAt") {
                    self.info.updated_at = updated_at.as_str().map(str::to_owned);
                }
                if let Some(meta) = update.get("_meta") {
                    self.info.meta = (!meta.is_null()).then(|| meta.clone());
                }
            }
            Some("usage_update") => {
                let (Some(used), Some(size)) = (
                    update.get("used").and_then(Value::as_u64),
                    update.get("size").and_then(Value::as_u64),
                ) else {
                    return false;
                };
                // An absent or `null` cost keeps the last known total.
                let cost = update
                    .get("cost")
                    .filter(|cost| !cost.is_null())
                    .cloned()
                    .or_else(|| self.usage.as_ref().and_then(|usage| usage.cost.clone()));
                self.usage = Some(SessionUsage { used, size, cost });
            }
            _ => return false,
        }
        true
    }

    /// Applies a session setup response: `modes` and `configOptions` override
    /// the current value only when the response provides them.
    pub(crate) fn apply_response(&mut self, response: &Value) {
        if let Some(modes) = response.get("modes").filter(|modes| !modes.is_null())
            && let Some(current_mode_id) = modes.get("currentModeId").and_then(Value::as_str)
        {
            self.modes = Some(SessionModes {
                current_mode_id: current_mode_id.to_string(),
                available_modes: modes
                    .get("availableModes")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
            self.config_options = Some(options.clone());
        }
    }

    /// The state at a setup response: `early` holds the control updates
    /// that arrived before it, as the latest update of each kind. The kinds own
    /// disjoint fields, so their arrival order does not matter.
    pub(crate) fn settle(early: &BTreeMap<String, Value>, response: &Value) -> Self {
        let mut controls = Self::default();
        for update in early.values() {
            controls.fold_update(update);
        }
        controls.apply_response(response);
        controls
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::SessionInfo;

    fn fold(updates: &[Value]) -> SessionControls {
        let mut controls = SessionControls::default();
        for update in updates {
            assert!(controls.fold_update(update), "{update}");
        }
        controls
    }

    #[test]
    fn sparse_session_info_keeps_absent_fields_clears_null_and_sets_values() {
        let mut controls = fold(&[json!({
            "sessionUpdate": "session_info_update",
            "title": "First",
            "updatedAt": "2026-10-01T00:00:00Z",
            "_meta": { "tag": 1 },
        })]);
        controls.fold_update(&json!({
            "sessionUpdate": "session_info_update",
            "updatedAt": "2026-10-02T00:00:00Z",
        }));
        assert_eq!(
            controls.info,
            SessionInfo {
                title: Some("First".into()),
                updated_at: Some("2026-10-02T00:00:00Z".into()),
                meta: Some(json!({ "tag": 1 })),
            }
        );
        controls.fold_update(&json!({
            "sessionUpdate": "session_info_update",
            "title": null,
            "_meta": null,
        }));
        assert_eq!(
            controls.info,
            SessionInfo {
                title: None,
                updated_at: Some("2026-10-02T00:00:00Z".into()),
                meta: None,
            }
        );
    }

    #[test]
    fn usage_without_cost_keeps_the_last_known_cost() {
        let cost = json!({ "amount": 1.5, "currency": "USD" });
        let mut controls = fold(&[json!({
            "sessionUpdate": "usage_update", "used": 10, "size": 100, "cost": cost,
        })]);
        controls.fold_update(&json!({ "sessionUpdate": "usage_update", "used": 20, "size": 100 }));
        assert_eq!(
            controls.usage,
            Some(SessionUsage {
                used: 20,
                size: 100,
                cost: Some(cost.clone()),
            })
        );
        controls.fold_update(
            &json!({ "sessionUpdate": "usage_update", "used": 30, "size": 200, "cost": null }),
        );
        assert_eq!(controls.usage.as_ref().unwrap().cost, Some(cost));
        assert_eq!(controls.usage.as_ref().unwrap().used, 30);
    }

    #[test]
    fn response_overrides_only_its_fields_between_early_and_later_updates() {
        let mut controls = fold(&[
            json!({ "sessionUpdate": "session_info_update", "title": "Early" }),
            json!({ "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "init", "description": "Init" }] }),
            json!({ "sessionUpdate": "config_option_update", "configOptions": [{ "id": "early" }] }),
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "early" }),
            json!({ "sessionUpdate": "usage_update", "used": 1, "size": 2 }),
        ]);
        controls.apply_response(&json!({
            "sessionId": "s",
            "modes": { "currentModeId": "ask", "availableModes": [{ "id": "ask" }, { "id": "code" }] },
            "configOptions": [{ "id": "response" }],
        }));
        assert_eq!(controls.modes.as_ref().unwrap().current_mode_id, "ask");
        assert_eq!(
            controls.config_options,
            Some(vec![json!({ "id": "response" })])
        );
        assert_eq!(controls.info.title.as_deref(), Some("Early"));
        assert_eq!(controls.commands.len(), 1);
        assert_eq!(controls.usage.as_ref().unwrap().used, 1);

        controls.fold_update(
            &json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
        );
        let modes = controls.modes.as_ref().unwrap();
        assert_eq!(modes.current_mode_id, "code");
        assert_eq!(modes.available_modes.len(), 2);

        // A response without the fields keeps the early values.
        let mut kept = fold(&[
            json!({ "sessionUpdate": "config_option_update", "configOptions": [{ "id": "early" }] }),
        ]);
        kept.apply_response(&json!({ "sessionId": "s", "modes": null }));
        assert_eq!(kept.config_options, Some(vec![json!({ "id": "early" })]));
        assert_eq!(kept.modes, None);
    }

    #[test]
    fn config_options_distinguish_never_provided_from_provided_empty() {
        let mut omitted = SessionControls::default();
        omitted.apply_response(&json!({ "sessionId": "s" }));
        assert_eq!(omitted.config_options, None);
        assert_eq!(
            serde_json::to_value(&omitted).unwrap()["configOptions"],
            Value::Null
        );

        let mut empty = SessionControls::default();
        empty.apply_response(&json!({ "sessionId": "s", "configOptions": [] }));
        assert_eq!(empty.config_options, Some(Vec::new()));
        assert_eq!(
            serde_json::to_value(&empty).unwrap()["configOptions"],
            json!([])
        );

        let mut cleared = fold(&[
            json!({ "sessionUpdate": "config_option_update", "configOptions": [{ "id": "a" }] }),
            json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
        ]);
        assert_eq!(cleared.config_options, Some(Vec::new()));
        cleared.apply_response(&json!({ "sessionId": "s", "configOptions": null }));
        assert_eq!(cleared.config_options, Some(Vec::new()));
    }

    #[test]
    fn non_control_and_malformed_updates_leave_the_state_unchanged() {
        let mut controls = SessionControls::default();
        for update in [
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "x" } }),
            json!({ "sessionUpdate": "current_mode_update" }),
            json!({ "sessionUpdate": "config_option_update", "configOptions": null }),
            json!({ "sessionUpdate": "available_commands_update" }),
            json!({ "sessionUpdate": "usage_update", "used": 1 }),
            json!({}),
        ] {
            assert!(!controls.fold_update(&update), "{update}");
        }
        assert_eq!(controls, SessionControls::default());
    }

    #[test]
    fn settling_lets_the_response_override_only_its_fields() {
        let response = json!({
            "sessionId": "s",
            "modes": { "currentModeId": "ask", "availableModes": [{ "id": "ask" }, { "id": "code" }] },
            "configOptions": [],
        });
        let mut early = BTreeMap::new();
        for update in [
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
            json!({ "sessionUpdate": "config_option_update", "configOptions": [{ "id": "early" }] }),
            json!({ "sessionUpdate": "session_info_update", "title": "Title", "updatedAt": null }),
            json!({ "sessionUpdate": "usage_update", "used": 3, "size": 9,
                    "cost": { "amount": 2, "currency": "USD" } }),
        ] {
            let key = update["sessionUpdate"].as_str().unwrap().to_string();
            let folded = crate::runtime_state::fold_control_update(early.get(&key), update);
            early.insert(key, folded);
        }
        let controls = SessionControls::settle(&early, &response);
        assert_eq!(controls.modes.as_ref().unwrap().current_mode_id, "ask");
        assert_eq!(controls.modes.as_ref().unwrap().available_modes.len(), 2);
        assert_eq!(controls.config_options, Some(Vec::new()));
        assert_eq!(controls.info.title.as_deref(), Some("Title"));
        assert_eq!(controls.usage.as_ref().unwrap().size, 9);
        assert_eq!(
            SessionControls::settle(&BTreeMap::new(), &json!({ "sessionId": "s" })),
            SessionControls::default()
        );
    }
}
