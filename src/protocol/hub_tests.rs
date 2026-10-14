//! The Hub on the session registry (docs/state-sync.md §2.4, §4.1): the cut,
//! alignment from `have`, the baseline and the queued tail, coalescing and
//! `end`; acceptance cases 3 and 4 (client-api-redesign.md §11).
//!
//! The registry is driven directly and every commit is dispatched to the Hub,
//! as the bridge's commit step does. Each client takes its queue's events one
//! at a time and checks them against the published documents (`stream_check`).
use serde_json::{Value, json};

use super::canon::ChainHash;
use super::documents::{AgentInfo, AgentLaunch, AgentPhase, OperationKind, Scope};
use super::hub::{Head, Hub, QUEUE_BUDGET, Subscription, parse_stream_query};
use super::publish::Publisher;
use super::stream_check::{Documents, Reader, Replica, log_values};
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

fn text(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn user(content: Value) -> Value {
    json!({ "sessionUpdate": "user_message_chunk", "content": content })
}

fn agent_chunk(content: Value) -> Value {
    json!({ "sessionUpdate": "agent_message_chunk", "content": content })
}

/// The replay of turns `(prompt, reply)`.
fn replay(turns: &[(&str, &str)]) -> Vec<Value> {
    turns
        .iter()
        .flat_map(|(prompt, reply)| [user(text(prompt)), agent_chunk(text(reply))])
        .collect()
}

/// Text large enough that two updates pass the queue's budget, while one
/// stays under it.
fn big(fill: char) -> String {
    std::iter::repeat_n(fill, QUEUE_BUDGET * 3 / 5).collect()
}

/// A tool call whose content is replaced by `fill`'s big text: each update
/// and the committed turn stay under the budget, two updates pass it.
fn flood_update(fill: char) -> Value {
    let kind = if fill == 'a' {
        "tool_call"
    } else {
        "tool_call_update"
    };
    json!({
        "sessionUpdate": kind, "toolCallId": "flood", "title": "flood", "status": "pending",
        "content": [{"type": "content", "content": text(&big(fill))}],
    })
}

/// The registry, the commit step and the Hub, as the bridge runs them.
struct World {
    registry: SessionRegistry,
    publisher: Publisher,
    hub: Hub,
    agent: AgentInfo,
    /// The published documents after every commit.
    timeline: Vec<Documents>,
    rpc: u64,
}

impl World {
    fn new() -> Self {
        let mut world = Self {
            registry: SessionRegistry::new(EPOCH),
            publisher: Publisher::default(),
            hub: Hub::default(),
            agent: agent(),
            timeline: Vec::new(),
            rpc: 0,
        };
        world.commit();
        world
    }

    fn documents(&self) -> Documents {
        self.publisher
            .scopes()
            .into_iter()
            .map(|scope| {
                let document = self.publisher.document(&scope).unwrap();
                (scope, document)
            })
            .collect()
    }

    /// The commit step: publish, then queue the events for every
    /// subscription. Returns the events.
    fn commit(&mut self) -> Vec<Value> {
        let events = self.publisher.commit(&mut self.registry, &self.agent, 0);
        self.hub.dispatch(&self.publisher, &events);
        self.timeline.push(self.documents());
        events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect()
    }

    /// A new stream for `reader`, under the same critical section as the
    /// commits (the cut).
    fn subscribe(&mut self, reader: &mut Reader) -> Subscription {
        let request = reader.request(Some("/workspace"));
        self.hub.subscribe(&self.publisher, &request)
    }

    fn take(&self, reader: &mut Reader, subscription: &Subscription) -> Option<Value> {
        reader.take(subscription, &self.timeline)
    }

    fn drain(&self, reader: &mut Reader, subscription: &Subscription) -> Vec<Value> {
        reader.drain(subscription, &self.timeline)
    }

    fn head(&self, session_id: &str) -> Head {
        let document = self
            .publisher
            .document(&format!("session/{session_id}"))
            .unwrap();
        Head::of(document.log.as_ref().expect("a known log"))
    }

    fn log(&self, session_id: &str) -> Vec<Value> {
        log_values(
            self.publisher
                .document(&format!("session/{session_id}"))
                .unwrap()
                .log
                .as_ref()
                .unwrap(),
        )
    }

    fn open(&mut self, session_id: &str) -> u64 {
        let incarnation = self
            .registry
            .open_new(
                EPOCH,
                session_id,
                format!("/{session_id}"),
                json!({ "sessionId": session_id }),
            )
            .unwrap();
        self.commit();
        incarnation
    }

    /// Starts a first load: the session is held with an unknown log.
    fn start_load(&mut self, session_id: &str) -> u64 {
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
        incarnation
    }

    /// Ends a first load with `replay`.
    fn finish_load(&mut self, session_id: &str, incarnation: u64, replay: &[Value]) {
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
        self.commit();
    }

    fn load(&mut self, session_id: &str, replay: &[Value]) -> u64 {
        let incarnation = self.start_load(session_id);
        self.finish_load(session_id, incarnation, replay);
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
        self.commit();
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
        self.commit();
    }

    /// Starts a prompt; returns its RPC ID.
    fn prompt(&mut self, session_id: &str, incarnation: u64, prompt: &str) -> String {
        self.rpc += 1;
        let rpc = format!("rpc-{}", self.rpc);
        self.registry
            .install_prompt(session_id, &[text(prompt)])
            .unwrap();
        self.registry
            .start_prompt(EPOCH, session_id, incarnation, &rpc, vec![text(prompt)])
            .unwrap();
        self.commit();
        rpc
    }

    /// Streams `reply` into the running prompt; returns the commit's events.
    fn reply(&mut self, session_id: &str, incarnation: u64, reply: &str) -> Vec<Value> {
        self.send(session_id, incarnation, agent_chunk(text(reply)))
    }

    /// One update of the running prompt; returns the commit's events.
    fn send(&mut self, session_id: &str, incarnation: u64, update: Value) -> Vec<Value> {
        self.registry
            .append_runtime_update_for_test(EPOCH, session_id, incarnation, update)
            .unwrap();
        self.commit()
    }

    fn complete(&mut self, session_id: &str, incarnation: u64, rpc: &str) {
        self.registry
            .complete_prompt(
                EPOCH,
                session_id,
                incarnation,
                rpc,
                json!({ "stopReason": "end_turn" }),
            )
            .unwrap();
        self.commit();
    }

    /// One whole turn, committed to the log.
    fn turn(&mut self, session_id: &str, incarnation: u64, prompt: &str, reply: &str) {
        let rpc = self.prompt(session_id, incarnation, prompt);
        self.reply(session_id, incarnation, reply);
        self.complete(session_id, incarnation, &rpc);
    }

    fn delete(&mut self, session_id: &str, incarnation: u64) {
        self.registry
            .install_operation(
                &Scope::Global,
                OperationKind::Delete,
                json!({ "sessionId": session_id }),
            )
            .unwrap();
        self.registry
            .start_delete(EPOCH, session_id, incarnation, "del")
            .unwrap();
        self.commit();
        self.registry
            .delete_close_succeeded(EPOCH, session_id, incarnation, "del")
            .unwrap();
        self.registry
            .complete_delete(EPOCH, session_id, incarnation, "del")
            .unwrap();
        self.commit();
    }
}

/// The entries a load of `turns` produces.
fn entries_of(turns: &[(&str, &str)]) -> Vec<Value> {
    let mut world = World::new();
    world.load("s", &replay(turns));
    world.log("s")
}

fn bytes(events: &[Value]) -> usize {
    events
        .iter()
        .map(|event| serde_json::to_vec(event).unwrap().len())
        .sum()
}

fn single(events: &[Value], scope: &str) -> Value {
    let matching: Vec<_> = events
        .iter()
        .filter(|event| event["scope"] == scope)
        .collect();
    assert_eq!(matching.len(), 1, "one event for {scope}: {events:?}");
    matching[0].clone()
}

fn entry(hash: &str) -> Value {
    json!({ "turn": { "prompt": [], "reply": [] }, "hash": hash })
}

// ---------------------------------------------------------------------------
// The cut and alignment (protocol §5.1, S3)

#[test]
fn a_subscription_aligns_held_sessions_with_a_known_an_unknown_and_an_empty_log() {
    let mut world = World::new();
    world.load("known", &replay(&[("one", "1"), ("two", "2")]));
    world.start_load("unknown");
    world.open("empty");
    let mut reader = Reader::new(&[
        "global",
        "catalog",
        "session/known",
        "session/unknown",
        "session/empty",
    ]);
    let subscription = world.subscribe(&mut reader);
    let events = world.drain(&mut reader, &subscription);
    assert_eq!(events.len(), 5, "one alignment per scope: {events:?}");

    let known = single(&events, "session/known");
    assert_eq!(known["log"]["from"], 0);
    assert_eq!(known["log"]["prev"], ChainHash::ZERO.wire());
    assert_eq!(known["log"]["entries"].as_array().unwrap().len(), 2);
    assert!(known["live"].get("replace").is_some());

    let unknown = single(&events, "session/unknown");
    assert!(
        unknown.get("log").is_none(),
        "an unknown log is omitted, never sent as []: {unknown}"
    );
    assert_eq!(unknown["live"]["replace"]["op"]["kind"], "load");
    assert_eq!(unknown["live"]["replace"]["op"]["status"], "running");

    let empty = single(&events, "session/empty");
    assert_eq!(empty["log"]["from"], 0);
    assert_eq!(
        empty["log"]["entries"],
        json!([]),
        "a known empty log is []"
    );
    for scope in ["global", "catalog"] {
        let aligned = single(&events, scope);
        assert_eq!(aligned["log"]["entries"], json!([]), "{scope}");
        assert!(aligned["live"].get("replace").is_some(), "{scope}");
    }
    reader.converged(&world.documents());
}

#[test]
fn have_selects_the_suffix_and_a_mismatched_or_longer_head_gets_the_whole_log() {
    let mut world = World::new();
    world.load("s", &replay(&[("one", "1"), ("two", "2")]));
    let log = world.log("s");
    let cases = [
        (vec![], 0, 2),
        (log[..1].to_vec(), 1, 1),
        (log.clone(), 2, 0),
        (vec![entry("wrong")], 0, 2),
        (vec![log[0].clone(), log[1].clone(), entry("further")], 0, 2),
    ];
    for (cache, from, appended) in cases {
        let mut reader = Reader::new(&["session/s"]).cache("session/s", cache.clone());
        let subscription = world.subscribe(&mut reader);
        let events = world.drain(&mut reader, &subscription);
        let aligned = single(&events, "session/s");
        assert_eq!(aligned["log"]["from"], from, "have {cache:?}: {aligned}");
        assert_eq!(
            aligned["log"]["entries"].as_array().unwrap().len(),
            appended,
            "have {cache:?}: {aligned}"
        );
        assert!(aligned["live"].get("replace").is_some());
        reader.converged(&world.documents());
    }
}

#[test]
fn a_scope_published_later_is_aligned_when_it_appears() {
    let mut world = World::new();
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    assert!(
        world.drain(&mut reader, &subscription).is_empty(),
        "an unheld session has nothing to align yet"
    );
    world.open("s");
    let events = world.drain(&mut reader, &subscription);
    let aligned = single(&events, "session/s");
    assert_eq!(aligned["log"]["entries"], json!([]));
    assert!(aligned["live"].get("replace").is_some());
    reader.converged(&world.documents());
}

// ---------------------------------------------------------------------------
// First loads: the log from each subscription's baseline (protocol §5.1)

#[test]
fn a_first_load_sends_each_subscription_its_log_from_its_own_baseline() {
    let turns = [("one", "1"), ("two", "2")];
    let entries = entries_of(&turns);
    let caches = [
        ("none", vec![], 0, 2),
        ("prefix", entries[..1].to_vec(), 1, 1),
        ("same", entries.clone(), 2, 0),
        ("diverged", vec![entry("other")], 0, 2),
        (
            "longer",
            vec![entries[0].clone(), entries[1].clone(), entry("later")],
            0,
            2,
        ),
    ];
    let mut world = World::new();
    let mut clients: Vec<_> = caches
        .iter()
        .map(|(name, cache, from, appended)| {
            let mut reader = Reader::new(&["session/s"]).cache("session/s", cache.clone());
            let subscription = world.subscribe(&mut reader);
            (*name, reader, subscription, *from, *appended)
        })
        .collect();
    assert_eq!(world.hub.subscribers("s"), caches.len());

    let incarnation = world.start_load("s");
    for (name, reader, subscription, _, _) in &mut clients {
        let events = world.drain(reader, subscription);
        let aligned = single(&events, "session/s");
        assert!(
            aligned.get("log").is_none(),
            "{name}: the alignment of a loading session carries live only: {aligned}"
        );
        assert_eq!(aligned["live"]["replace"]["op"]["kind"], "load");
    }

    world.finish_load("s", incarnation, &replay(&turns));
    for (name, reader, subscription, from, appended) in &mut clients {
        let events = world.drain(reader, subscription);
        let end = single(&events, "session/s");
        assert_eq!(end["log"]["from"], *from, "{name}: {end}");
        assert_eq!(
            end["log"]["entries"].as_array().unwrap().len(),
            *appended,
            "{name}: an empty suffix still confirms the log: {end}"
        );
        assert!(
            end["live"].get("patch").is_some(),
            "{name}: the same event ends the load: {end}"
        );
        assert_eq!(reader.live("session/s")["op"]["status"], "completed");
        assert_eq!(reader.log("session/s"), entries, "{name}");
        reader.converged(&world.documents());
    }
}

#[test]
fn an_unknown_log_keeps_the_declared_baseline_through_live_only_changes() {
    let entries = entries_of(&[("one", "1")]);
    let mut world = World::new();
    let mut reader = Reader::new(&["session/s"]).cache("session/s", entries.clone());
    let subscription = world.subscribe(&mut reader);
    let incarnation = world.start_load("s");
    world.drain(&mut reader, &subscription);
    world.registry.fold_session_controls(
        "s",
        &json!({"sessionUpdate": "session_info_update", "title": "T"}),
    );
    world.commit();
    world.drain(&mut reader, &subscription);
    assert_eq!(
        subscription.baseline("session/s"),
        Some(
            Replica {
                live: Value::Null,
                log: entries.clone(),
            }
            .head()
        ),
        "live-only changes leave the declared baseline"
    );
    world.finish_load("s", incarnation, &replay(&[("one", "1")]));
    let events = world.drain(&mut reader, &subscription);
    let end = single(&events, "session/s");
    assert_eq!(end["log"]["from"], 1);
    assert_eq!(end["log"]["entries"], json!([]));
    reader.converged(&world.documents());
}

// ---------------------------------------------------------------------------
// The queue: baseline and queued tail (§2.4)

#[test]
fn queued_log_changes_follow_the_tail_and_only_the_output_advances_the_baseline() {
    let mut world = World::new();
    let incarnation = world.open("s");
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);

    world.turn("s", incarnation, "one", "1");
    let first = world.head("s");
    assert_eq!(subscription.tail("session/s"), Some(first.clone()));
    assert_eq!(subscription.baseline("session/s"), Some(Head::zero()));
    world.turn("s", incarnation, "two", "2");
    assert_eq!(subscription.tail("session/s"), Some(world.head("s")));
    assert_eq!(
        subscription.baseline("session/s"),
        Some(Head::zero()),
        "a queued change never advances the baseline"
    );

    let events = world.drain(&mut reader, &subscription);
    let logs: Vec<&Value> = events
        .iter()
        .filter(|event| event.get("log").is_some())
        .collect();
    assert_eq!(logs.len(), 2, "{events:?}");
    assert_eq!(logs[0]["log"]["from"], 0);
    assert_eq!(logs[1]["log"]["from"], 1);
    assert_eq!(
        logs[1]["log"]["prev"], first.hash,
        "the second follows the tail"
    );
    reader.converged(&world.documents());
}

#[test]
fn an_undelivered_reload_to_the_same_length_is_followed_by_the_next_append() {
    let mut world = World::new();
    let incarnation = world.load("s", &replay(&[("one", "1"), ("two", "2")]));
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);
    let delivered = world.head("s");

    world.reload("s", incarnation, &replay(&[("one", "1"), ("other", "3")]));
    let reloaded = world.head("s");
    assert_eq!(reloaded.n, delivered.n, "[A,B] became [A,C]");
    assert_ne!(reloaded.hash, delivered.hash);
    assert_eq!(subscription.tail("session/s"), Some(reloaded.clone()));
    assert_eq!(subscription.baseline("session/s"), Some(delivered));
    world.turn("s", incarnation, "more", "4");

    let events = world.drain(&mut reader, &subscription);
    let logs: Vec<&Value> = events
        .iter()
        .filter(|event| event.get("log").is_some())
        .collect();
    assert_eq!(logs.len(), 2, "{events:?}");
    assert_eq!(logs[0]["log"]["from"], 1);
    assert_eq!(logs[1]["log"]["from"], 2);
    assert_eq!(logs[1]["log"]["prev"], reloaded.hash);
    reader.converged(&world.documents());
}

#[test]
fn an_undelivered_shrink_is_followed_by_the_next_append() {
    let mut world = World::new();
    let incarnation = world.load("s", &replay(&[("one", "1"), ("two", "2")]));
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);

    world.reload("s", incarnation, &replay(&[("one", "1")]));
    let shrunk = world.head("s");
    assert_eq!(shrunk.n, 1);
    assert_eq!(subscription.tail("session/s"), Some(shrunk.clone()));
    world.turn("s", incarnation, "other", "3");

    let events = world.drain(&mut reader, &subscription);
    let logs: Vec<&Value> = events
        .iter()
        .filter(|event| event.get("log").is_some())
        .collect();
    assert_eq!(logs.len(), 2, "{events:?}");
    assert_eq!(logs[0]["log"]["from"], 1);
    assert_eq!(
        logs[0]["log"]["entries"],
        json!([]),
        "an empty change truncates"
    );
    assert_eq!(logs[1]["log"]["from"], 1);
    assert_eq!(logs[1]["log"]["prev"], shrunk.hash);
    reader.converged(&world.documents());
}

#[test]
fn an_equal_reload_of_a_known_log_changes_no_log() {
    let turns = [("one", "1"), ("two", "2")];
    let mut world = World::new();
    let incarnation = world.load("s", &replay(&turns));
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);
    world.reload("s", incarnation, &replay(&turns));
    let events = world.drain(&mut reader, &subscription);
    assert!(
        events.iter().all(|event| event.get("log").is_none()),
        "{events:?}"
    );
    reader.converged(&world.documents());
}

// ---------------------------------------------------------------------------
// Coalescing (§2.4, I4)

/// Starts a prompt and sends two big updates into it, past the queue's
/// budget; returns the prompt's RPC and the bytes of those commits' events.
fn flood(world: &mut World, session_id: &str, incarnation: u64) -> (String, usize) {
    let rpc = world.prompt(session_id, incarnation, "flood");
    let mut events = world.send(session_id, incarnation, flood_update('a'));
    events.extend(world.send(session_id, incarnation, flood_update('b')));
    (rpc, bytes(&events))
}

#[test]
fn a_coalesced_queue_aligns_an_unsent_log_from_the_baseline_and_applies_directly() {
    let mut world = World::new();
    let incarnation = world.load("s", &replay(&[("one", "1"), ("two", "2")]));
    let mut reader = Reader::new(&["global", "session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);
    let baseline = world.head("s");

    world.reload("s", incarnation, &replay(&[("one", "1"), ("other", "3")]));
    assert_eq!(subscription.coalesced(), 0);
    let (rpc, flooded) = flood(&mut world, "s", incarnation);
    assert!(flooded > QUEUE_BUDGET, "the budget was exceeded: {flooded}");
    assert!(subscription.coalesced() >= 1, "the queue was coalesced");
    assert_eq!(
        subscription.baseline("session/s"),
        Some(baseline.clone()),
        "coalescing advances no baseline"
    );
    assert_eq!(subscription.tail("session/s"), Some(world.head("s")));

    let events = world.drain(&mut reader, &subscription);
    let aligned = single(&events, "session/s");
    assert!(aligned["live"].get("replace").is_some(), "{aligned}");
    assert_eq!(
        aligned["log"]["from"], 0,
        "the reloaded log does not hold the baseline's hash"
    );
    assert!(
        events
            .iter()
            .filter(|event| event["scope"] == "global")
            .count()
            <= 1
    );
    reader.converged(&world.documents());

    // The queue continues from the alignment's target.
    world.complete("s", incarnation, &rpc);
    let events = world.drain(&mut reader, &subscription);
    let commit = single(&events, "session/s");
    assert_eq!(commit["log"]["from"], 2);
    reader.converged(&world.documents());
}

#[test]
fn a_coalesce_after_a_partial_delivery_aligns_from_the_advanced_baseline() {
    let mut world = World::new();
    let incarnation = world.open("s");
    let mut reader = Reader::new(&["session/s"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);

    world.turn("s", incarnation, "one", "1");
    let first = world.head("s");
    world.turn("s", incarnation, "two", "2");
    while subscription.baseline("session/s") != Some(first.clone()) {
        world
            .take(&mut reader, &subscription)
            .expect("the first turn's commit is queued");
    }
    let (_, flooded) = flood(&mut world, "s", incarnation);
    assert!(flooded > QUEUE_BUDGET);
    assert!(subscription.coalesced() >= 1);
    assert_eq!(subscription.baseline("session/s"), Some(first));

    let events = world.drain(&mut reader, &subscription);
    let aligned = single(&events, "session/s");
    assert!(aligned["live"].get("replace").is_some(), "{aligned}");
    assert_eq!(aligned["log"]["from"], 1, "the suffix from the baseline");
    assert_eq!(aligned["log"]["entries"].as_array().unwrap().len(), 1);
    reader.converged(&world.documents());
}

#[test]
fn a_coalesce_keeps_a_queued_end_and_nothing_of_its_scope_follows() {
    let mut world = World::new();
    let first = world.open("s1");
    let second = world.open("s2");
    let mut reader = Reader::new(&["global", "session/s1", "session/s2"]);
    let subscription = world.subscribe(&mut reader);
    world.drain(&mut reader, &subscription);

    world.delete("s2", second);
    let (rpc, _) = flood(&mut world, "s1", first);
    assert!(subscription.coalesced() >= 1);
    world.complete("s1", first, &rpc);
    world.turn("s1", first, "after", "end");

    let events = world.drain(&mut reader, &subscription);
    let ended = single(&events, "session/s2");
    assert_eq!(ended["end"]["reason"], "deleted", "{ended}");
    assert!(
        events
            .iter()
            .filter(|event| event["scope"] == "session/s1")
            .any(|event| event["live"].get("replace").is_some()),
        "the existing scope is aligned"
    );
    assert!(reader.ended.contains_key("session/s2"));
    reader.converged(&world.documents());
}

#[test]
fn a_turn_completion_and_a_reload_coalesce_to_the_same_cut_a_follower_reaches() {
    let mut world = World::new();
    let incarnation = world.load("s", &replay(&[("one", "1")]));
    let mut follower = Reader::new(&["global", "session/s"]);
    let following = world.subscribe(&mut follower);
    let mut lagging = Reader::new(&["global", "session/s"]);
    let lagged = world.subscribe(&mut lagging);
    world.drain(&mut follower, &following);
    world.drain(&mut lagging, &lagged);

    world.turn("s", incarnation, "two", "2");
    world.drain(&mut follower, &following);
    world.reload("s", incarnation, &replay(&[("one", "1"), ("other", "3")]));
    world.drain(&mut follower, &following);
    // The follower takes every commit as it comes; the lagger stays paused.
    let rpc = world.prompt("s", incarnation, "flood");
    world.drain(&mut follower, &following);
    for fill in ['a', 'b'] {
        world.send("s", incarnation, flood_update(fill));
        world.drain(&mut follower, &following);
    }
    world.complete("s", incarnation, &rpc);
    world.drain(&mut follower, &following);
    assert!(lagged.coalesced() >= 1);
    assert_eq!(following.coalesced(), 0, "a follower is never coalesced");

    world.drain(&mut lagging, &lagged);
    assert_eq!(lagging.replicas, follower.replicas);
    lagging.converged(&world.documents());
    follower.converged(&world.documents());
}

// ---------------------------------------------------------------------------
// Holding counts (S6)

#[test]
fn session_subscribers_count_open_subscriptions_only() {
    let mut world = World::new();
    world.open("s");
    let mut a = Reader::new(&["global", "session/s"]);
    let first = world.subscribe(&mut a);
    let mut b = Reader::new(&["session/s"]);
    let second = world.subscribe(&mut b);
    let mut c = Reader::new(&["global", "catalog"]);
    let unheld = world.subscribe(&mut c);
    assert_eq!(world.hub.subscribers("s"), 2);
    drop(first);
    assert_eq!(world.hub.subscribers("s"), 1);
    drop(second);
    assert_eq!(world.hub.subscribers("s"), 0);
    drop(unheld);
    assert_eq!(world.hub.subscribers("s"), 0);
}

// ---------------------------------------------------------------------------
// The client's checks fail on an event that does not apply

#[test]
fn a_replica_rejects_a_log_change_or_patch_that_does_not_apply() {
    let mut replica = Replica {
        live: Value::Null,
        log: vec![entry("a")],
    };
    assert!(
        replica
            .apply(&json!({"scope": "session/s", "log": {"from": 1, "prev": "b", "entries": []}}))
            .is_err(),
        "a wrong prev"
    );
    assert!(
        replica
            .apply(&json!({"scope": "session/s", "log": {"from": 2, "prev": "a", "entries": []}}))
            .is_err(),
        "a from past the log"
    );
    replica.live = json!({ "op": {} });
    assert!(
        replica
            .apply(&json!({
                "scope": "session/s",
                "live": {"patch": [{"op": "remove", "path": "/missing"}]},
            }))
            .is_err(),
        "a patch whose target is missing"
    );
}

// ---------------------------------------------------------------------------
// The stream query (protocol §5.1)

#[test]
fn a_stream_query_names_scopes_heads_and_load_directories() {
    let scopes = parse_stream_query(
        "scope=global&scope=session%2FS&have=session%2FS%3A12%3Aabc_-&cwd=session%2FS%3AC%3A%2Fw%2Fx",
    )
    .unwrap();
    assert_eq!(scopes.len(), 2);
    assert_eq!(scopes[0].scope, Scope::Global);
    assert_eq!(scopes[0].have, Head::zero());
    assert_eq!(scopes[0].cwd, None);
    assert_eq!(scopes[1].scope, Scope::Session("S".into()));
    assert_eq!(
        scopes[1].have,
        Head {
            n: 12,
            hash: "abc_-".into(),
        }
    );
    assert_eq!(scopes[1].cwd.as_deref(), Some("C:/w/x"));

    let unencoded =
        parse_stream_query("scope=session/S&have=session/S:1:h&cwd=session/S:/w").unwrap();
    assert_eq!(
        unencoded[0].have,
        Head {
            n: 1,
            hash: "h".into()
        }
    );
    assert_eq!(unencoded[0].cwd.as_deref(), Some("/w"));
    let without = parse_stream_query("scope=session/S").unwrap();
    assert_eq!(
        without[0].have,
        Head::zero(),
        "without have the head is (0, h_0)"
    );

    for invalid in [
        "",
        "scope=elsewhere",
        "scope=session/",
        "scope=global&have=session/S:1:h",
        "scope=session/S&have=session/S:x:h",
        "scope=session/S&have=session/S:1",
        "scope=session/S&cwd=session/T:/w",
    ] {
        let error = parse_stream_query(invalid).unwrap_err();
        assert_eq!(
            error.code,
            super::documents::ErrorCode::InvalidRequest,
            "{invalid:?}"
        );
    }
}
