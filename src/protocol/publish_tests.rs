//! Commit-step scenarios on the session registry. Every step commits, then
//! the convergence gate compares log-fed replicas with fresh projections.
use std::sync::{Arc, Weak};
use std::time::Instant;

use serde_json::{Value, json};

use super::log::Change;
use super::replica::{Convergence, fresh};
use super::{AgentState, GLOBAL, Op, Replication, session_scope};
use crate::runtime_state::UrlFlowStatus;
use crate::session_registry::SessionRegistry;
use crate::session_state::TurnAdmission;

const EPOCH: &str = "epoch";

struct Bench {
    registry: SessionRegistry,
    replication: Replication,
    convergence: Convergence,
    agent: AgentState,
}

impl Bench {
    fn new() -> Self {
        Self::with(Replication::default())
    }

    fn with(replication: Replication) -> Self {
        let mut bench = Self {
            registry: SessionRegistry::new(EPOCH),
            replication,
            convergence: Convergence::new(0x5eed),
            agent: AgentState::default(),
        };
        bench.step();
        bench
    }

    /// Ends a transition: one commit, then the convergence gate. Returns the
    /// ops this commit appended, by scope.
    fn step(&mut self) -> Vec<(String, Vec<Op>)> {
        let before = self.replication.log().seq();
        self.replication.commit(&self.agent, 0, &self.registry);
        self.check().unwrap();
        self.replication
            .log()
            .after(before)
            .unwrap()
            .filter_map(|entry| match &entry.change {
                Change::Ops(ops) => Some((entry.scope.clone(), ops.to_vec())),
                Change::End => None,
            })
            .collect()
    }

    fn check(&mut self) -> Result<(), String> {
        let fresh = fresh(&self.agent, 0, &self.registry);
        self.convergence.check(&self.replication, &fresh)
    }

    fn settle(&mut self) {
        let fresh = fresh(&self.agent, 0, &self.registry);
        self.convergence.settle(&self.replication, &fresh).unwrap();
    }

    fn open(&mut self, session_id: &str, response: Value, early: Vec<Value>) -> u64 {
        let incarnation = self
            .registry
            .open_new_with_replay(EPOCH, session_id, format!("/{session_id}"), response, early)
            .unwrap();
        self.registry.register_new(session_id, incarnation);
        incarnation
    }

    fn start_turn(&mut self, session_id: &str, incarnation: u64, intent: &str) -> String {
        let revision = self
            .registry
            .state(session_id)
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let TurnAdmission::Accepted { operation_id } = self
            .registry
            .start_turn(
                session_id,
                incarnation,
                &revision,
                intent,
                vec![json!({ "type": "text", "text": "go" })],
            )
            .unwrap()
        else {
            panic!("a new intent is admitted");
        };
        operation_id
    }

    fn update(&mut self, session_id: &str, incarnation: u64, operation_id: &str, update: Value) {
        self.registry
            .append_turn_update(session_id, incarnation, operation_id, update)
            .unwrap();
    }

    fn finish_turn(
        &mut self,
        session_id: &str,
        incarnation: u64,
        intent: &str,
        operation_id: &str,
    ) {
        let response = json!({ "stopReason": "end_turn" });
        self.registry
            .complete_prompt(EPOCH, session_id, incarnation, intent, response.clone())
            .unwrap();
        self.registry
            .complete_turn(session_id, incarnation, operation_id, response)
            .unwrap();
        self.registry
            .commit_completed_turn_from_memory(session_id, incarnation, operation_id)
            .unwrap();
    }

    fn control(&mut self, session_id: &str, incarnation: u64, update: Value) {
        let kind = update["sessionUpdate"].as_str().unwrap().to_string();
        self.registry
            .update_control_state(EPOCH, session_id, incarnation, &kind, update)
            .unwrap();
    }
}

fn chunk(text: &str) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m",
            "content": { "type": "text", "text": text } })
}

fn paths(ops: &[(String, Vec<Op>)], scope: &str) -> Vec<String> {
    ops.iter()
        .filter(|(entry, _)| entry == scope)
        .flat_map(|(_, ops)| ops.iter())
        .map(|op| serde_json::to_value(op).unwrap().to_string())
        .collect()
}

#[test]
fn agent_control_updates_converge_in_arrival_order() {
    let mut bench = Bench::new();
    let s = bench.open(
        "s",
        json!({ "sessionId": "s", "modes": {
            "currentModeId": "ask", "availableModes": [{ "id": "ask", "name": "Ask" }, { "id": "code", "name": "Code" }],
        } }),
        vec![
            json!({ "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "init", "description": "Init" }] }),
            json!({ "sessionUpdate": "session_info_update", "title": "Early" }),
        ],
    );
    let created = bench.step();
    assert_eq!(
        created
            .iter()
            .map(|(scope, _)| scope.as_str())
            .collect::<Vec<_>>(),
        ["session/s", GLOBAL],
        "a session entry precedes the global summary it changes"
    );
    for update in [
        json!({ "sessionUpdate": "session_info_update", "updatedAt": "2026-10-01T00:00:00Z" }),
        json!({ "sessionUpdate": "usage_update", "used": 10, "size": 100,
                "cost": { "amount": 1.5, "currency": "USD" } }),
        json!({ "sessionUpdate": "usage_update", "used": 20, "size": 100 }),
        json!({ "sessionUpdate": "session_info_update", "title": null }),
        json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" }),
        json!({ "sessionUpdate": "config_option_update", "configOptions": [] }),
        json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }),
        json!({ "sessionUpdate": "session_info_update", "title": "Later" }),
    ] {
        bench.control("s", s, update);
        let ops = bench.step();
        assert!(!ops.is_empty(), "every control update is published");
    }
    let ops = {
        bench.control(
            "s",
            s,
            json!({ "sessionUpdate": "session_info_update", "title": "Renamed" }),
        );
        bench.step()
    };
    assert_eq!(
        paths(&ops, "session/s"),
        [json!({ "op": "set", "path": "/controls/info/title", "value": "Renamed" }).to_string()]
    );
    assert_eq!(
        paths(&ops, GLOBAL),
        [json!({ "op": "set", "path": "/sessions/s/title", "value": "Renamed" }).to_string()]
    );
    bench.settle();
}

#[test]
fn a_turn_streams_appends_and_folds_tool_calls_into_their_slot() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let operation = bench.start_turn("s", s, "intent");
    let started = bench.step();
    assert!(
        paths(&started, GLOBAL)
            .iter()
            .any(|op| op.contains("/sessions/s/")),
        "a turn started elsewhere shows in the summary: {started:?}"
    );
    bench.update("s", s, &operation, json!({
        "sessionUpdate": "tool_call", "toolCallId": "a", "title": "Read", "status": "in_progress",
    }));
    bench.step();
    for text in ["Hel", "lo"] {
        bench.update("s", s, &operation, chunk(text));
        bench.step();
    }
    let streamed = {
        bench.update("s", s, &operation, chunk(", world"));
        bench.step()
    };
    assert_eq!(
        paths(&streamed, "session/s"),
        [
            json!({ "op": "append", "path": "/turn/updates/1/content/text", "text": ", world" })
                .to_string()
        ]
    );
    let folded = {
        bench.update(
            "s",
            s,
            &operation,
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed",
            }),
        );
        bench.step()
    };
    assert_eq!(
        paths(&folded, "session/s"),
        [
            json!({ "op": "set", "path": "/turn/updates/0/status", "value": "completed" })
                .to_string()
        ],
        "the update keeps the tool call's original slot"
    );
    bench.finish_turn("s", s, "intent", &operation);
    bench.step();

    // A background update after the turn folds into committed history.
    let projections = bench.replication.history_projections;
    bench
        .registry
        .append_session_update(
            "s",
            s,
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "failed",
            }),
        )
        .unwrap();
    let background = bench.step();
    assert_eq!(bench.replication.history_projections, projections + 1);
    assert!(
        paths(&background, "session/s")
            .iter()
            .all(|op| !op.contains("\"path\":\"/history/timeline\"")),
        "the timeline is not resent: {background:?}"
    );
    bench.settle();
}

#[test]
fn plans_and_compactions_fold_in_place_and_removals_shrink_the_turn() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let operation = bench.start_turn("s", s, "intent");
    bench.step();
    let mut published = Vec::new();
    for update in [
        json!({ "sessionUpdate": "plan",
                "entries": [{ "content": "a", "priority": "high", "status": "pending" }] }),
        json!({ "sessionUpdate": "plan",
                "entries": [{ "content": "a", "priority": "high", "status": "completed" }] }),
        json!({ "sessionUpdate": "plan_update",
                "plan": { "planId": "p", "type": "items", "entries": [] } }),
        json!({ "sessionUpdate": "plan_update", "plan": { "planId": "p", "type": "items",
                "entries": [{ "content": "b", "priority": "low", "status": "pending" }] } }),
        json!({ "sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress" }),
        json!({ "sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
                "content": { "type": "text", "text": "Summary" } }),
        json!({ "sessionUpdate": "plan_removed", "planId": "p" }),
    ] {
        bench.update("s", s, &operation, update);
        published.push(paths(&bench.step(), "session/s"));
    }
    assert_eq!(
        published[1],
        [json!({ "op": "set", "path": "/turn/updates/0/entries/0/status", "value": "completed" })
            .to_string()],
        "a replaced legacy plan keeps its slot"
    );
    assert!(
        published[3]
            .iter()
            .all(|op| op.contains("/turn/updates/1/")),
        "an identified plan update keeps its slot: {:?}",
        published[3]
    );
    assert!(
        published[6]
            .iter()
            .any(|op| op.starts_with(r#"{"op":"set","path":"/turn/updates","#)),
        "a removal shrinks the updates: {:?}",
        published[6]
    );
    bench.finish_turn("s", s, "intent", &operation);
    bench.step();
    bench.settle();
}

#[test]
fn a_terminal_rolling_past_its_bound_trims_and_appends() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut retained = String::new();
    let mut trimmed = false;
    for chunk in ["abcdef", "ghij", "kl", "mnopqrstuvwx", "yz"] {
        retained.push_str(chunk);
        let start = retained.len().saturating_sub(8);
        retained.drain(..start);
        bench
            .registry
            .upsert_terminal(
                EPOCH,
                "s",
                s,
                "term",
                json!({
                    "sessionId": "s", "terminalId": "term", "output": chunk, "outputAppend": true,
                    "retainedBytes": retained.len(), "truncated": start > 0,
                    "exitStatus": null, "released": false,
                }),
            )
            .unwrap();
        let ops = bench.step();
        trimmed |= paths(&ops, "session/s")
            .iter()
            .any(|op| op.contains("\"op\":\"trim\"") && op.contains("/terminals/term/output"));
    }
    assert!(trimmed, "the bounded tail moves with trim plus append");
    bench
        .registry
        .upsert_terminal(
            EPOCH,
            "s",
            s,
            "term",
            json!({
                "sessionId": "s", "terminalId": "term", "output": "", "outputAppend": true,
                "retainedBytes": 8, "truncated": true,
                "exitStatus": { "exitCode": 0, "signal": null }, "released": true,
            }),
        )
        .unwrap();
    let released = bench.step();
    assert!(
        paths(&released, "session/s")
            .contains(&json!({ "op": "remove", "path": "/terminals/term" }).to_string()),
        "an unreferenced released terminal leaves: {released:?}"
    );
    bench.settle();
}

#[test]
fn url_flows_complete_and_cancel_in_session_and_global_scopes() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    for scope in [Some(("s", s)), None] {
        for (name, status) in [
            ("complete", UrlFlowStatus::Completed),
            ("cancel", UrlFlowStatus::Cancelled),
        ] {
            let interaction = &format!(
                "{}-{name}",
                if scope.is_some() {
                    "session"
                } else {
                    "request"
                }
            );
            let request = json!({
                "mode": "url", "elicitationId": interaction, "url": "https://example.test",
                "message": "Sign in",
            });
            bench
                .registry
                .upsert_elicitation(EPOCH, scope, interaction, request)
                .unwrap();
            bench.step();
            bench
                .registry
                .resolve_elicitation(EPOCH, scope, interaction, Some(interaction))
                .unwrap();
            let accepted = bench.step();
            let document = if scope.is_some() { "session/s" } else { GLOBAL };
            assert!(
                paths(&accepted, document)
                    .iter()
                    .any(|op| op.contains(&format!("/urlFlows/{interaction}"))),
                "{document}: {accepted:?}"
            );
            bench
                .registry
                .settle_url_registration(EPOCH, scope, interaction, interaction, status)
                .unwrap();
            assert!(!bench.step().is_empty());
        }
    }
    bench.settle();
}

#[test]
fn ended_and_replaced_sessions_end_their_scope() {
    let mut bench = Bench::new();
    let first = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let seq = bench.replication.scope_seq("session/s").unwrap();
    bench.registry.sessions.remove("s");
    bench.step();
    let end = bench
        .replication
        .log()
        .after(seq)
        .unwrap()
        .find(|entry| entry.scope == session_scope("s"))
        .unwrap();
    assert_eq!(
        (end.scope.as_str(), end.prev, &end.change),
        ("session/s", seq, &Change::End)
    );
    assert_eq!(bench.replication.scope_seq("session/s"), None);

    let second = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    assert_ne!(first, second);
    let before = bench.replication.log().seq();
    bench.step();
    let created = bench
        .replication
        .log()
        .after(before)
        .unwrap()
        .find(|entry| entry.scope == session_scope("s"))
        .unwrap();
    assert_eq!(created.prev, 0, "a new incarnation is a new document");
    bench.settle();
}

#[test]
fn a_transition_without_a_commit_is_caught() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Uncommitted" }),
    );
    let divergence = bench.check().unwrap_err();
    assert!(divergence.contains("session/s"), "{divergence}");
    assert!(divergence.contains("Uncommitted"), "{divergence}");
    // The next commit publishes it, and the replicas converge again.
    bench.step();
    bench.settle();
}

#[test]
fn published_documents_and_the_log_release_turn_content() {
    let mut bench = Bench::with(Replication::with_log_budget(16 * 1024));
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let operation = bench.start_turn("s", s, "intent");
    for index in 0..64 {
        bench.update(
            "s",
            s,
            &operation,
            json!({
                "sessionUpdate": "tool_call", "toolCallId": format!("tool-{index}"),
                "title": "x".repeat(512), "status": "completed",
            }),
        );
        bench.step();
        assert!(bench.replication.log().bytes() <= 16 * 1024 || bench.replication.log().len() == 1);
    }
    let turn = bench
        .registry
        .state("s")
        .unwrap()
        .active_turn
        .clone()
        .unwrap();
    let updates: Weak<Vec<Arc<Value>>> = Arc::downgrade(&turn.updates);
    let first: Weak<Value> = Arc::downgrade(&turn.updates[0]);
    let prompt = Arc::downgrade(&turn.prompt);
    drop(turn);
    let history = Arc::downgrade(
        bench
            .registry
            .history
            .peek(&crate::history_cache::SessionKey::new("s", s))
            .unwrap(),
    );

    bench.finish_turn("s", s, "intent", &operation);
    bench.step();
    bench.registry.release_published_runtime(u64::MAX);
    assert!(
        updates.upgrade().is_none(),
        "the published turn released its updates"
    );
    assert!(first.upgrade().is_none());
    assert!(prompt.upgrade().is_none());
    assert!(
        history.upgrade().is_none(),
        "the replaced baseline is not retained"
    );
    bench.settle();
}

#[test]
fn completed_process_content_is_released_from_the_retained_log() {
    const PROCESS_BODY: &str = "completed-process-body-must-leave-the-log";

    fn retained_ops(replication: &Replication) -> String {
        let log = replication.log();
        // Permit release to compact the prefix. Inspect every surviving entry,
        // rather than requiring an obsolete cursor to remain replayable.
        let before_retained = log.seq() - log.len() as u64;
        let ops = log
            .after(before_retained)
            .expect("the retained suffix is continuous")
            .flat_map(|entry| entry.change_ops())
            .collect::<Vec<_>>();
        serde_json::to_string(&ops).unwrap()
    }

    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let operation = bench.start_turn("s", s, "release-intent");
    bench.update(
        "s",
        s,
        &operation,
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "read-tool", "title": "Read",
            "status": "completed", "content": [{ "type": "content", "content": {
                "type": "text", "text": PROCESS_BODY,
            } }],
        }),
    );
    bench.step();
    assert!(
        retained_ops(&bench.replication).contains(PROCESS_BODY),
        "the log must deliver the process body while its turn is active"
    );

    bench.update("s", s, &operation, chunk("Done"));
    bench.finish_turn("s", s, "release-intent", &operation);
    bench.step();
    bench.registry.release_published_runtime(u64::MAX);
    let documents = fresh(&bench.agent, 0, &bench.registry);
    let session = &documents[&session_scope("s")];
    assert!(session["turn"].is_null());
    assert!(session["history"]["timeline"].to_string().contains("Done"));
    assert!(
        !serde_json::to_string(&documents)
            .unwrap()
            .contains(PROCESS_BODY)
    );
    assert!(
        !retained_ops(&bench.replication).contains(PROCESS_BODY),
        "released process bytes must leave log-owned Op values too, without waiting for later traffic to exhaust the budget"
    );
    bench.settle();
}

#[test]
fn identity_diff_cost_on_long_timelines() {
    const UPDATES: usize = 2_000;
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let operation = bench.start_turn("s", s, "long");
    for index in 0..UPDATES {
        bench.update("s", s, &operation, json!({
            "sessionUpdate": "tool_call", "toolCallId": format!("tool-{index}"), "title": "Read",
        }));
    }
    bench.update("s", s, &operation, chunk("Done"));
    bench.finish_turn("s", s, "long", &operation);
    bench.step();

    let operation = bench.start_turn("s", s, "next");
    for index in 0..UPDATES {
        bench.update("s", s, &operation, json!({
            "sessionUpdate": "tool_call", "toolCallId": format!("next-{index}"), "title": "Read",
        }));
    }
    bench.step();
    let projections = bench.replication.history_projections;
    let started = Instant::now();
    const CHUNKS: u32 = 50;
    for _ in 0..CHUNKS {
        bench.update("s", s, &operation, chunk("x"));
        bench.replication.commit(&bench.agent, 0, &bench.registry);
    }
    let per_commit = started.elapsed() / CHUNKS;
    assert_eq!(
        bench.replication.history_projections, projections,
        "streaming never re-projects the history"
    );
    let ops = bench
        .replication
        .log()
        .after(bench.replication.log().seq() - 1)
        .unwrap()
        .next()
        .unwrap();
    assert!(
        serde_json::to_vec(&ops.change_ops()).unwrap().len() < 256,
        "a chunk publishes only its growth"
    );
    eprintln!(
        "identity diff: {per_commit:?} per commit with {UPDATES} history entries and {UPDATES} turn updates"
    );
    bench.settle();
}

impl super::log::LogEntry {
    fn change_ops(&self) -> Vec<Op> {
        match &self.change {
            Change::Ops(ops) => ops.to_vec(),
            Change::End => Vec::new(),
        }
    }
}

#[test]
fn unchanged_session_prompt_does_not_scale_other_session_commit_allocations() {
    fn commit_allocations(prompt_bytes: usize) -> crate::test_allocations::AllocationStats {
        let mut bench = Bench::new();
        let quiet = bench.open("quiet", json!({ "sessionId": "quiet" }), Vec::new());
        let streaming = bench.open("streaming", json!({ "sessionId": "streaming" }), Vec::new());
        let revision = bench
            .registry
            .state("quiet")
            .unwrap()
            .history_revision
            .clone()
            .unwrap();
        let admission = bench
            .registry
            .admit_session_turn(
                EPOCH,
                "quiet",
                quiet,
                &revision,
                "quiet-intent",
                vec![json!({ "type": "text", "text": "q".repeat(prompt_bytes) })],
            )
            .unwrap();
        assert!(matches!(admission, TurnAdmission::Accepted { .. }));
        let operation = bench.start_turn("streaming", streaming, "streaming-intent");
        bench.step();

        // Publish once before measuring, so the next commit changes an
        // existing update in one session while the other session is untouched.
        bench.update("streaming", streaming, &operation, chunk("first"));
        bench.step();
        let before = bench.replication.log().seq();
        let quiet_seq = bench.replication.scope_seq("session/quiet");
        bench.update("streaming", streaming, &operation, chunk(" next"));

        // Input construction, the authoritative fold and fresh-projection
        // checks are outside the allocation measurement.
        let (_, allocations) = crate::test_allocations::measure_allocations(|| {
            bench.replication.commit(&bench.agent, 0, &bench.registry);
        });

        assert_eq!(bench.replication.scope_seq("session/quiet"), quiet_seq);
        assert!(
            bench
                .replication
                .log()
                .after(before)
                .unwrap()
                .any(|entry| entry.scope == session_scope("streaming")),
            "the measured commit must publish the other session's update"
        );
        bench.check().unwrap();
        allocations
    }

    const SMALL_PROMPT: usize = 64 * 1024;
    const LARGE_PROMPT: usize = 1024 * 1024;
    let small = commit_allocations(SMALL_PROMPT);
    let large = commit_allocations(LARGE_PROMPT);
    let extra = large.requested_bytes.saturating_sub(small.requested_bytes);
    let prompt_growth = LARGE_PROMPT - SMALL_PROMPT;
    // Allow metadata/bookkeeping variation without allowing even one full
    // copy of the additional, unchanged prompt content. This counts requested
    // allocation bytes, not elapsed time, live memory, or allocator RSS.
    let allowance = 64 * 1024 + prompt_growth / 16;
    assert!(
        extra <= allowance,
        "an untouched prompt must not be copied for another session's update: \
         small={small:?}, large={large:?}, extra={extra}, allowance={allowance}"
    );
}
