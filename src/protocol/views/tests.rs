//! The views of a session (design §4.5), read from the tree a commit makes.
use serde_json::json;

use super::*;
use crate::protocol::documents::{AgentInfo, AgentLaunch, AgentPhase};
use crate::protocol::tree_doc::Projector;
use crate::session_registry::SessionRegistry;

fn agent() -> AgentInfo {
    AgentInfo {
        phase: AgentPhase::Ready,
        error: None,
        launch: AgentLaunch::default(),
        initialize: None,
        authenticated_method_id: None,
    }
}

fn text(update: &str, id: &str, text: &str) -> serde_json::Value {
    json!({ "sessionUpdate": update, "messageId": id, "content": { "type": "text", "text": text } })
}

struct World {
    registry: SessionRegistry,
    projector: Projector,
    tree: Tree,
    wanted: BTreeSet<View>,
}

impl World {
    fn new() -> Self {
        let mut registry = SessionRegistry::new("epoch");
        registry.open_new("epoch", "s", "/work", json!({ "sessionId": "s" })).unwrap();
        Self { registry, projector: Projector::default(), tree: Tree::new("I"), wanted: BTreeSet::new() }
    }

    fn update(&mut self, update: serde_json::Value) -> &mut Self {
        self.registry.append_session_update("s", update).unwrap();
        self
    }

    fn commit(&mut self) -> Option<u64> {
        let next = self.tree.rev() + 1;
        let root = self.projector.project(&self.registry, &agent(), 1, next);
        let wanted = self.wanted.clone();
        self.tree.commit_with_views(root, |tree| derive(tree, &wanted)).map(|committed| committed.rev)
    }

    fn view(&self, path: &str) -> serde_json::Value {
        self.tree.get(path).unwrap_or_else(|| panic!("{path} is not published")).to_value()
    }

    fn history_rev(&self) -> u64 {
        self.tree.get("/sessions/s").unwrap().field("historyRev").and_then(Value::as_u64).unwrap()
    }
}

const LATEST: &str = "/views/s/output/latest";

fn latest() -> View {
    View::Latest { session: "s".into() }
}

#[test]
fn the_latest_output_is_the_last_agent_message_of_the_current_turn() {
    let mut world = World::new();
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "first"));
    world.update(text("user_message_chunk", "u2", "q2")).update(text("agent_message_chunk", "a2", "Hel"));
    world.update(text("agent_message_chunk", "a2", "lo"));
    world.wanted.insert(latest());
    world.commit().unwrap();
    let view = world.view(LATEST);
    assert_eq!(view["text"], "Hello", "the text blocks concatenated, nothing inserted");
    assert_eq!(view["entry"], 3);
    assert_eq!(view["other"], 0);
    assert_eq!(view["turn"], json!({ "historyRev": world.history_rev(), "start": 2, "messageId": "u2", "state": "ended" }));
}

#[test]
fn the_latest_output_moves_to_the_next_turn_and_to_a_later_message_after_a_tool_call() {
    let mut world = World::new();
    world.wanted.insert(latest());
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "one"));
    world.commit().unwrap();
    assert_eq!(world.view(LATEST)["text"], "one");
    world.update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x" }));
    world.update(text("agent_message_chunk", "a2", "two"));
    let rev = world.commit().unwrap();
    assert_eq!(world.view(LATEST)["text"], "two");
    assert_eq!(world.tree.get(LATEST).unwrap().rev(), rev);
    world.update(text("user_message_chunk", "u2", "q2"));
    world.commit().unwrap();
    let view = world.view(LATEST);
    assert_eq!((view["entry"].clone(), view["text"].clone()), (serde_json::Value::Null, json!("")), "{view}");
    assert_eq!(view["turn"]["start"], 4);
}

#[test]
fn a_session_with_no_turn_has_a_latest_view_with_no_turn() {
    let mut world = World::new();
    world.wanted.insert(latest());
    world.commit().unwrap();
    assert_eq!(world.view(LATEST), json!({ "turn": null, "entry": null, "text": "", "other": 0 }));
}

#[test]
fn a_session_whose_history_is_unknown_has_no_views() {
    let mut world = World::new();
    world.registry.register_cold("s");
    world.wanted.insert(latest());
    world.wanted.insert(View::Turn { session: "s".into(), instance: "I".into(), history_rev: 1, start: 0 });
    world.commit().unwrap();
    assert!(world.tree.get("/views/s").is_none());
}

#[test]
fn a_fixed_view_is_bound_to_the_instance_and_the_history_it_names() {
    let mut world = World::new();
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "one"));
    world.commit().unwrap();
    let h = world.history_rev();
    let output = |instance: &str, history_rev, start| View::Output {
        session: "s".into(),
        instance: instance.into(),
        history_rev,
        start,
    };
    assert_eq!(gone(&world.tree, &output("I", h, 0)), None);
    assert_eq!(gone(&world.tree, &output("I", h, 1)), Some(Gone::Absent), "no turn starts at 1");
    assert_eq!(gone(&world.tree, &output("OLD", h, 0)), Some(Gone::Replaced), "another instance, same revision");
    assert_eq!(gone(&world.tree, &output("I", h + 1, 0)), Some(Gone::Replaced));
    assert_eq!(gone(&world.tree, &latest()), None);

    world.wanted.insert(output("I", h, 0));
    world.wanted.insert(output("I", h, 1));
    world.wanted.insert(output("OLD", h, 0));
    world.commit().unwrap();
    let path = format!("/views/s/history/I/{h}/output");
    assert_eq!(world.view(&format!("{path}/0"))["text"], "one");
    assert!(world.tree.get(&format!("{path}/1")).is_none());
    assert!(world.tree.get(&format!("/views/s/history/OLD/{h}/output/0")).is_none());
}

#[test]
fn a_replaced_history_ends_the_fixed_views_of_the_old_one() {
    let mut world = World::new();
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "one"));
    world.commit().unwrap();
    let old = world.history_rev();
    let view = View::Output { session: "s".into(), instance: "I".into(), history_rev: old, start: 0 };
    world.registry.register_cold("s");
    world.registry.register_new("s");
    world.commit().unwrap();
    assert!(world.history_rev() > old);
    assert_eq!(gone(&world.tree, &view), Some(Gone::Replaced));
}

#[test]
fn a_turn_view_holds_the_turns_entries_and_shares_them_with_the_session() {
    let mut world = World::new();
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "one"));
    world.update(text("user_message_chunk", "u2", "q2")).update(text("agent_message_chunk", "a2", "two"));
    world.commit().unwrap();
    let h = world.history_rev();
    world.wanted.insert(View::Turn { session: "s".into(), instance: "I".into(), history_rev: h, start: 2 });
    world.commit().unwrap();
    let path = format!("/views/s/history/I/{h}/turn/2");
    let turn = world.view(&path);
    assert_eq!((turn["start"].clone(), turn["length"].clone()), (json!(2), json!(2)));
    assert_eq!(turn["entries"].as_array().unwrap().len(), 2);
    assert_eq!(turn["entries"][1]["chunks"][0]["content"]["text"], "two");
    let (view, session) = (
        world.tree.get(&format!("{path}/entries/1")).unwrap().value_arc().unwrap(),
        world.tree.get("/sessions/s/entries/3").unwrap().value_arc().unwrap(),
    );
    assert!(Arc::ptr_eq(&view, &session), "one copy in memory");
    assert_eq!(world.tree.get(&format!("{path}/entries/1")).unwrap().hash(), world.tree.get("/sessions/s/entries/3").unwrap().hash());
}

#[test]
fn an_unfollowed_view_is_removed_and_a_view_that_did_not_change_keeps_its_revision() {
    let mut world = World::new();
    world.update(text("user_message_chunk", "u1", "q1")).update(text("agent_message_chunk", "a1", "one"));
    world.wanted.insert(latest());
    let first = world.commit().unwrap();
    world.update(json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }));
    world.registry.update_control_state("epoch", "s", json!({ "sessionUpdate": "session_info_update", "title": "T" })).unwrap();
    world.commit().unwrap();
    assert_eq!(world.tree.get(LATEST).unwrap().rev(), first, "the output did not change");
    world.wanted.clear();
    world.commit().unwrap();
    assert!(world.tree.get(LATEST).is_none());
}

#[test]
fn a_path_inside_a_view_is_the_view_and_a_path_is_built_and_parsed_back() {
    let view = View::of("/views/a~1b/history/I/7/turn/4/entries/0").unwrap();
    assert_eq!(view, View::Turn { session: "a/b".into(), instance: "I".into(), history_rev: 7, start: 4 });
    assert_eq!(view.path(), "/views/a~1b/history/I/7/turn/4");
    assert_eq!(View::of("/views/s/output/latest/text"), Some(latest()));
    for not in ["/views/s", "/views/s/output", "/views/s/history/I/x/turn/1", "/views/s/history/I/1/other/1", "/sessions/s/op"] {
        assert_eq!(View::of(not), None, "{not}");
    }
}
