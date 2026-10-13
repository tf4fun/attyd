//! Hub delivery scenarios on the session registry (plan §2.3): the cut,
//! per-scope `prev`, `position` heartbeats, coalescing, resume and `end`
//! reasons. Every stream feeds a protocol replica (`stream_replica`) that
//! must match fresh projections once it has applied the current position.
use std::sync::Arc;

use serde_json::{Value, json};

use super::hub::{Message, ScopeRequest, StreamQueue, parse_event_id};
use super::log::DEFAULT_LOG_BUDGET;
use super::publish_tests::{Bench, EPOCH, chunk};
use super::replica::fresh;
use super::stream_replica::StreamReplica;
use super::{EndReason, GLOBAL, RETIRED_OWNERS, Replication, session_scope};

fn owner(session_id: &str, incarnation: u64) -> ScopeRequest {
    ScopeRequest::Session {
        session_id: session_id.into(),
        epoch: EPOCH.into(),
        incarnation,
    }
}

/// One client: a replica kept across reconnects, and its current stream.
struct Client {
    scopes: Vec<ScopeRequest>,
    queue: Arc<StreamQueue>,
    replica: StreamReplica,
    last_id: Option<String>,
}

impl Client {
    fn subscribe(bench: &mut Bench, scopes: &[ScopeRequest]) -> Self {
        let names = scopes.iter().map(ScopeRequest::scope).collect::<Vec<_>>();
        let replica = StreamReplica::new(names.iter().map(String::as_str));
        let queue = Self::open(bench, scopes, None);
        Self {
            scopes: scopes.to_vec(),
            queue,
            replica,
            last_id: None,
        }
    }

    fn open(bench: &mut Bench, scopes: &[ScopeRequest], last_id: Option<&str>) -> Arc<StreamQueue> {
        bench.replication.subscribe(
            &bench.agent,
            0,
            &bench.registry,
            scopes.to_vec(),
            last_id.and_then(parse_event_id).as_ref(),
        )
    }

    /// The stream drops and the client resumes with its `Last-Event-ID`.
    fn reconnect(&mut self, bench: &mut Bench) {
        self.queue.close();
        self.queue = Self::open(bench, &self.scopes, self.last_id.as_deref());
    }

    /// Reads every queued message into the replica; returns the entries.
    fn drain(&mut self) -> Vec<Value> {
        let mut entries = Vec::new();
        while let Some(Message { id, data }) = self.queue.try_next() {
            self.replica.receive(&id, &data).unwrap();
            self.last_id = Some(id);
            entries.push(serde_json::from_str(&data).unwrap());
        }
        entries
    }

    /// Fetches the snapshot of every scope waiting after a `reset`.
    fn fetch(&mut self, bench: &Bench) {
        for scope in self.replica.fetches() {
            let session_id = scope.strip_prefix("session/").unwrap();
            if let Some(snapshot) = bench
                .replication
                .session_snapshot(&bench.registry, session_id)
            {
                self.replica
                    .install(
                        &scope,
                        &snapshot.at,
                        serde_json::to_value(snapshot.state).unwrap(),
                    )
                    .unwrap();
            }
        }
    }

    /// Drains, fetches, and compares with fresh projections (S2).
    fn sync(&mut self, bench: &Bench) -> Vec<Value> {
        let entries = self.drain();
        self.fetch(bench);
        assert_eq!(self.replica.applied(), bench.replication.log().seq());
        if let Some(divergence) = self
            .replica
            .diverges(&fresh(&bench.agent, 0, &bench.registry))
        {
            panic!("{divergence}");
        }
        entries
    }
}

fn kinds(entries: &[Value]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|entry| {
            (
                entry["type"].as_str().unwrap().to_string(),
                entry["scope"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

#[test]
fn a_cut_starts_global_inline_and_a_held_owner_with_a_reset() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let cut = bench.replication.log().seq();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    let entries = client.drain();
    assert_eq!(
        kinds(&entries),
        [
            ("snapshot".into(), GLOBAL.into()),
            ("reset".into(), "session/s".into())
        ]
    );
    assert!(entries.iter().all(|entry| entry["at"]["seq"] == cut));
    assert_eq!(client.replica.applied(), 0, "a reset scope is pending");
    client.sync(&bench);

    // The next entries carry the stream's per-scope `prev`: the cut.
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Named" }),
    );
    bench.step();
    let entries = client.sync(&bench);
    assert_eq!(
        kinds(&entries),
        [
            ("ops".into(), "session/s".into()),
            ("ops".into(), GLOBAL.into())
        ]
    );
    assert!(entries.iter().all(|entry| entry["prev"] == cut));
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Renamed" }),
    );
    bench.step();
    let entries = client.sync(&bench);
    assert_eq!(
        entries[0]["prev"],
        entries[0]["at"]["seq"].as_u64().unwrap() - 2
    );
}

#[test]
fn traffic_on_other_scopes_advances_the_stream_with_one_coalesced_position() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    let operation = bench.start_turn("s", s, "turn");
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global]);
    client.sync(&bench);
    for text in ["a", "b", "c", "d"] {
        bench.update("s", s, &operation, chunk(text));
        bench.step();
    }
    let entries = client.sync(&bench);
    assert_eq!(
        entries,
        [
            json!({ "type": "position", "at": { "epoch": EPOCH, "seq": bench.replication.log().seq() } })
        ],
        "session-only commits reach a global stream as one position"
    );
}

#[test]
fn a_lagging_subscriber_coalesces_into_one_resync_per_scope() {
    let mut bench = Bench::with(Replication::with_budgets(DEFAULT_LOG_BUDGET, EPOCH, 2_048));
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    let operation = bench.start_turn("s", s, "turn");
    bench.step();
    let mut slow = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    let mut fast = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    slow.sync(&bench);
    fast.sync(&bench);
    for index in 0..200 {
        bench.update("s", s, &operation, chunk(&format!("chunk {index} ")));
        bench.step();
        fast.sync(&bench);
        assert!(
            slow.queue.queued_bytes() <= 2_048 + 1_024,
            "a slow subscriber's queue stays near its budget"
        );
    }
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Late" }),
    );
    bench.step();
    fast.sync(&bench);
    let entries = slow.sync(&bench);
    for scope in [GLOBAL, "session/s"] {
        assert!(
            entries
                .iter()
                .filter(|entry| entry["scope"] == scope
                    && (entry["type"] == "reset" || entry["type"] == "snapshot"))
                .count()
                <= 1,
            "at most one resync per scope: {:?}",
            kinds(&entries)
        );
    }
    assert!(slow.replica.resets >= 2, "the session scope was coalesced");
    assert_eq!(
        slow.replica.documents(),
        fast.replica.documents(),
        "coalescing gives the same final replica (S4)"
    );
}

#[test]
fn resume_continues_the_retained_suffix_and_starts_over_after_compaction() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    client.sync(&bench);
    for title in ["One", "Two"] {
        bench.control(
            "s",
            s,
            json!({ "sessionUpdate": "session_info_update", "title": title }),
        );
        bench.step();
    }
    client.reconnect(&mut bench);
    let entries = client.sync(&bench);
    assert!(
        entries.iter().all(|entry| entry["type"] == "ops"),
        "a retained suffix resumes without a reset: {:?}",
        kinds(&entries)
    );
    assert_eq!(client.replica.resets, 1);

    // The log compacts past the client's position while it is away.
    client.queue.close();
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Three" }),
    );
    bench.step();
    let seq = bench.replication.log().seq();
    bench.replication.log_mut_for_test().compact_through(seq);
    client.reconnect(&mut bench);
    let entries = client.sync(&bench);
    assert_eq!(
        kinds(&entries),
        [
            ("snapshot".into(), GLOBAL.into()),
            ("reset".into(), "session/s".into())
        ]
    );

    // A position from another epoch starts over too.
    client.last_id = Some("other-epoch:1".into());
    client.reconnect(&mut bench);
    assert_eq!(kinds(&client.sync(&bench)).len(), 2);
}

#[test]
fn owners_the_bridge_does_not_hold_end_at_once_with_their_reason() {
    let mut bench = Bench::new();
    let mut owners = Vec::new();
    for (session_id, reason) in [
        ("closed", Some(EndReason::Closed)),
        ("deleted", Some(EndReason::Deleted)),
        ("unobserved", Some(EndReason::Unobserved)),
        ("replaced", None),
    ] {
        let incarnation = bench.open(session_id, json!({ "sessionId": session_id }), Vec::new());
        bench.step();
        if let Some(reason) = reason {
            bench
                .replication
                .note_retirement(session_id, incarnation, reason);
        }
        bench.registry.sessions.remove(session_id);
        owners.push(owner(session_id, incarnation));
    }
    bench.step();
    let current = bench.open("replaced", json!({ "sessionId": "replaced" }), Vec::new());
    bench.step();
    let other_epoch = ScopeRequest::Session {
        session_id: "replaced".into(),
        epoch: "older-epoch".into(),
        incarnation: current,
    };
    owners.extend([owner("never-held", 1), other_epoch]);
    let mut reasons = Vec::new();
    for request in owners {
        let mut client = Client::subscribe(&mut bench, &[request]);
        let entries = client.drain();
        assert_eq!(entries.len(), 1, "{entries:?}");
        reasons.push(entries[0]["reason"].as_str().unwrap().to_string());
    }
    assert_eq!(
        reasons,
        [
            "closed",
            "deleted",
            "unobserved",
            "replaced",
            "gone",
            "replaced"
        ]
    );
    assert_eq!(
        bench.replication.held_incarnation("replaced"),
        Some(current)
    );
}

#[test]
fn the_retired_table_outlives_log_compaction_within_its_bound() {
    let mut bench = Bench::new();
    let first = bench.open("first", json!({ "sessionId": "first" }), Vec::new());
    bench.step();
    bench
        .replication
        .note_retirement("first", first, EndReason::Closed);
    bench.registry.sessions.remove("first");
    bench.step();
    let seq = bench.replication.log().seq();
    bench.replication.log_mut_for_test().compact_through(seq);
    assert_eq!(
        bench.replication.retired("first", first),
        Some(EndReason::Closed)
    );
    let mut client = Client::subscribe(&mut bench, &[owner("first", first)]);
    assert_eq!(client.drain()[0]["reason"], "closed");

    for index in 0..RETIRED_OWNERS {
        let session_id = format!("s{index}");
        bench.open(&session_id, json!({ "sessionId": session_id }), Vec::new());
        bench.step();
        bench.registry.sessions.remove(&session_id);
        bench.step();
    }
    assert_eq!(bench.replication.retired("first", first), None);
    let mut client = Client::subscribe(&mut bench, &[owner("first", first)]);
    assert_eq!(client.drain()[0]["reason"], "gone");
}

#[test]
fn a_subscribed_scope_ends_once_and_never_follows_the_next_incarnation() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    client.sync(&bench);
    bench
        .replication
        .note_retirement("s", s, EndReason::Unobserved);
    bench.registry.sessions.remove("s");
    bench.step();
    let entries = client.sync(&bench);
    let end = entries
        .iter()
        .find(|entry| entry["type"] == "end")
        .expect("the scope ends");
    assert_eq!(end["reason"], "unobserved");
    assert_eq!(
        client.replica.ended("session/s"),
        Some(EndReason::Unobserved)
    );

    let next = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    assert_ne!(next, s);
    bench.step();
    let entries = client.sync(&bench);
    assert!(
        entries
            .iter()
            .all(|entry| entry["scope"] != session_scope("s")),
        "nothing follows end: {entries:?}"
    );
}

#[test]
fn a_snapshot_that_overtakes_its_entries_is_not_reset_again() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[owner("s", s)]);
    client.drain();
    for title in ["One", "Two", "Three"] {
        bench.control(
            "s",
            s,
            json!({ "sessionUpdate": "session_info_update", "title": title }),
        );
        bench.step();
    }
    // The HTTP snapshot is taken after the entries were queued, and is
    // installed before the stream delivers them.
    client.fetch(&bench);
    let entries = client.sync(&bench);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["type"] == "ops")
            .count(),
        3,
        "{:?}",
        kinds(&entries)
    );
    assert_eq!((client.replica.covered, client.replica.resets), (3, 1));
}

#[tokio::test]
async fn ending_the_epoch_drains_and_closes_every_stream() {
    let mut bench = Bench::new();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global]);
    bench.agent.phase = super::AgentPhase::Stopped;
    bench.step();
    bench.replication.finish();
    assert_eq!(bench.replication.subscribers(), 0);
    let entries = client.sync(&bench);
    assert_eq!(kinds(&entries).last().unwrap().0, "ops");
    assert_eq!(
        client.replica.documents()[GLOBAL]["agent"]["phase"],
        "stopped"
    );
    assert!(
        client.queue.next().await.is_none(),
        "a finished queue ends the stream"
    );
}

#[test]
fn partial_initial_cut_survives_reconnect() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    let Message { id, data } = client.queue.try_next().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&data).unwrap()["type"],
        "snapshot"
    );
    client.replica.receive(&id, &data).unwrap();
    client.last_id = Some(id);
    // Disconnect before the session's initial reset, which carries the same ID.
    client.reconnect(&mut bench);
    // The production subscription also commits when its bridge guard drops.
    bench.step();
    let entries = client.drain();
    client.fetch(&bench);
    assert_eq!(
        client.replica.applied(),
        bench.replication.log().seq(),
        "entries: {entries:?}"
    );
    assert!(
        client
            .replica
            .diverges(&fresh(&bench.agent, 0, &bench.registry))
            .is_none(),
        "the resumed cut must initialize every subscribed scope"
    );
}

#[test]
fn partial_coalesced_cut_survives_reconnect() {
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[ScopeRequest::Global, owner("s", s)]);
    client.sync(&bench);
    bench.replication.set_subscriber_budget(EPOCH, 128);
    bench.control(
        "s",
        s,
        json!({ "sessionUpdate": "session_info_update", "title": "Changed" }),
    );
    bench.step();
    let Message { id, data } = client.queue.try_next().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&data).unwrap()["type"],
        "snapshot"
    );
    client.replica.receive(&id, &data).unwrap();
    client.last_id = Some(id);
    // Disconnect before the coalesced session reset at the same global position.
    client.reconnect(&mut bench);
    bench.step();
    let entries = client.drain();
    client.fetch(&bench);
    assert_eq!(client.replica.applied(), bench.replication.log().seq());
    assert!(
        client
            .replica
            .diverges(&fresh(&bench.agent, 0, &bench.registry))
            .is_none(),
        "the replica claims to be current but its session is stale; entries: {entries:?}"
    );
}

#[test]
fn finished_turn_releases_slow_stream_process_bytes() {
    const PROCESS_BODY: &str = "completed-process-must-leave-subscriber-queue";
    let mut bench = Bench::new();
    let s = bench.open("s", json!({ "sessionId": "s" }), Vec::new());
    bench.step();
    let mut client = Client::subscribe(&mut bench, &[owner("s", s)]);
    client.sync(&bench);
    let operation = bench.start_turn("s", s, "intent");
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
    bench.update("s", s, &operation, chunk("Done"));
    bench.finish_turn("s", s, "intent", &operation);
    bench.step();
    bench.registry.release_published_runtime(u64::MAX);
    assert!(
        !serde_json::to_string(&fresh(&bench.agent, 0, &bench.registry))
            .unwrap()
            .contains(PROCESS_BODY)
    );
    let log = bench.replication.log();
    let retained = log
        .after(log.seq() - log.len() as u64)
        .unwrap()
        .map(|entry| format!("{entry:?}"))
        .collect::<Vec<_>>();
    assert!(!retained.join("").contains(PROCESS_BODY));
    let entries = client.drain();
    assert!(
        !serde_json::to_string(&entries)
            .unwrap()
            .contains(PROCESS_BODY),
        "released process content remains queued: {entries:?}"
    );
    client.sync(&bench);
}
