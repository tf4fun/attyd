//! v2 streams through the real coordinator and ACP transport (migration step
//! 5; docs/state-sync.md §2.4, §4.1; acceptance 3, 4 and 12).
//!
//! A stream is opened with `BridgeInput::Subscribe` and read one event at a
//! time by a `Reader`, which checks every event against the documents the
//! bridge published and the subscription's baseline and queued tail. The
//! published documents are only compared with: a stream that goes wrong fails
//! the test, it is never repaired from them.
use super::*;
use crate::protocol::hub::{Head, QUEUE_BUDGET};
use crate::protocol::stream_check::Reader;

const SAVED: &str = "saved";
const CWD: &str = "/workspace";

fn session(session_id: &str) -> String {
    format!("session/{session_id}")
}

fn user(content: Value) -> Value {
    json!({"sessionUpdate": "user_message_chunk", "content": content})
}

/// The replay of turns `(prompt, reply)`.
fn turns(turns: &[(&str, &str)]) -> Vec<Value> {
    turns
        .iter()
        .enumerate()
        .flat_map(|(index, (prompt, reply))| {
            [
                user(text(prompt)),
                chunk(&format!("reply-{index}"), text(reply)),
            ]
        })
        .collect()
}

fn loadable() -> Value {
    json!({"loadSession": true, "sessionCapabilities": {"close": {}}})
}

/// Text large enough that two updates pass a queue's budget, while one
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

/// Whether the running prompt shows the flood of `fill`.
fn flooded(live: &Value, fill: char) -> bool {
    let marker: String = std::iter::repeat_n(fill, 1024).collect();
    live["op"]["process"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item.to_string().contains(&marker)))
}

/// One client's stream: a reader and its current subscription. The reader's
/// copies outlive the subscription, as a client keeps them across streams.
struct Stream {
    reader: Reader,
    subscription: Option<Subscription>,
}

impl Stream {
    async fn open(peer: &Peer, scopes: &[&str], cwd: Option<&str>) -> Self {
        Self::cached(peer, Reader::new(scopes), cwd).await
    }

    /// A stream of a client that already holds copies in `reader`.
    async fn cached(peer: &Peer, mut reader: Reader, cwd: Option<&str>) -> Self {
        let subscription = peer.subscribe(reader.request(cwd)).await;
        Self {
            reader,
            subscription: Some(subscription),
        }
    }

    fn subscription(&self) -> &Subscription {
        self.subscription.as_ref().expect("an open stream")
    }

    /// Closes the stream and subscribes again with the heads it holds (S3).
    async fn reopen(&mut self, peer: &Peer, cwd: Option<&str>) {
        self.subscription = None;
        let request = self.reader.request(cwd);
        self.subscription = Some(peer.subscribe(request).await);
    }

    fn close(&mut self) {
        self.subscription = None;
    }

    /// Takes every queued event.
    fn take_all(&mut self, peer: &mut Peer) -> Vec<Value> {
        peer.drain();
        let subscription = self.subscription.as_ref().expect("an open stream");
        self.reader.drain(subscription, &peer.timeline)
    }

    /// Takes queued events until `condition` holds of the reader, applying the
    /// bridge's commits as they arrive.
    async fn until(
        &mut self,
        peer: &mut Peer,
        description: &str,
        condition: impl Fn(&Reader) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            self.take_all(peer);
            if condition(&self.reader) {
                return;
            }
            match tokio::time::timeout_at(deadline, peer.commits.recv()).await {
                Ok(Some(commit)) => peer.apply(commit),
                Ok(None) => {
                    self.take_all(peer);
                    assert!(
                        condition(&self.reader),
                        "the tap closed before {description}"
                    );
                    return;
                }
                Err(_) => panic!(
                    "timed out waiting for {description}; replicas: {:#?}",
                    self.reader.replicas
                ),
            }
        }
    }

    /// Every followed scope equals what the bridge publishes now.
    fn converged(&self, peer: &Peer) {
        self.reader
            .converged(peer.timeline.last().expect("published documents"));
    }

    fn live(&self, scope: &str) -> Value {
        self.reader.live(scope)
    }

    fn log(&self, scope: &str) -> Vec<Value> {
        self.reader.log(scope)
    }

    fn events_of(&self, scope: &str) -> Vec<Value> {
        self.reader.events_of(scope)
    }
}

impl Peer {
    /// Nothing reaches the Agent for `period`.
    async fn quiet(&mut self, period: Duration) {
        assert!(
            self.buffered.is_empty(),
            "unexpected Agent messages: {:?}",
            self.buffered
        );
        if let Ok(message) = tokio::time::timeout(period, self.requests.next()).await {
            panic!("unexpected Agent message {message:?}");
        }
    }

    async fn subscribers(&self, session_id: &str) -> usize {
        let lock = self.lock();
        let state = lock.lock().await;
        let publication = state.protocol.publication.lock();
        publication.hub.subscribers(session_id)
    }

    async fn until_subscribers(&self, session_id: &str, count: usize) {
        tokio::time::timeout(WAIT, async {
            while self.subscribers(session_id).await != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{session_id} did not reach {count} subscribers"));
    }

    /// Answers the `session/load` the bridge sends for `session_id` with
    /// `replay`; returns the request.
    async fn serve_load(&mut self, session_id: &str, replay: &[Value]) -> Value {
        let load = self.request("session/load").await;
        assert_eq!(load["params"]["sessionId"], session_id);
        for update in replay {
            self.update(session_id, update.clone()).await;
        }
        self.reply(&load, json!({})).await;
        load
    }

    /// Runs one whole turn of `session_id` with `reply`.
    async fn whole_turn(&mut self, session_id: &str, prompt: &str, reply: &str) {
        let scope = session(session_id);
        let prompt = vec![text(prompt)];
        let (op, request) = self.prompt(session_id, prompt).await;
        self.update(session_id, chunk(&Uuid::new_v4().to_string(), text(reply)))
            .await;
        self.reply(&request, json!({"stopReason": "end_turn"}))
            .await;
        self.until("the turn completes", |peer| {
            peer.live(&scope)["op"]["id"] == op && peer.live(&scope)["op"]["status"] == "completed"
        })
        .await;
    }

    fn released(&self, scope: &str) -> bool {
        self.gate.released.contains(scope)
    }
}

/// The log a load of `replay` gives `saved`: a client's cache from an
/// earlier run.
async fn history(replay: &[Value]) -> Vec<Value> {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let _stream = Stream::open(&peer, &[session(SAVED).as_str()], Some(CWD)).await;
    let request = peer.request("session/load").await;
    for update in replay {
        peer.update(SAVED, update.clone()).await;
    }
    peer.reply(&request, json!({})).await;
    peer.until("the load completes", |peer| {
        peer.live(&session(SAVED))["op"]["status"] == "completed"
    })
    .await;
    let log = peer.log(&session(SAVED));
    peer.stop().await.unwrap();
    log
}

fn hash_of(entry: &Value) -> String {
    entry["hash"].as_str().unwrap().to_string()
}

/// The prompt `op` ran until one event appended its turn, removed its process
/// and ended it; no state shows the new entry next to the process (S2, I8).
fn assert_committed_once(stream: &Stream, scope: &str, op: &str, entry: &str) {
    let carrying: Vec<Value> = stream
        .events_of(scope)
        .into_iter()
        .filter(|event| {
            event["log"]["entries"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|appended| appended["hash"] == entry))
        })
        .collect();
    assert_eq!(
        carrying.len(),
        1,
        "the turn reaches the stream once: {carrying:?}"
    );
    for (step_scope, replica) in &stream.reader.steps {
        if step_scope != scope {
            continue;
        }
        let committed = replica.log.iter().any(|appended| appended["hash"] == entry);
        if committed {
            assert!(
                replica.live["op"].get("process").is_none(),
                "the committed turn's process shows next to its entry: {replica:?}"
            );
        } else if replica.live["op"]["id"] == op {
            assert_eq!(
                replica.live["op"]["status"], "running",
                "the prompt ended before its turn was committed: {replica:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Reads, the cut and the prompt's commit boundary

#[tokio::test]
async fn a_stream_aligns_each_scope_at_once_and_follows_a_prompt_through_its_commit() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let scopes = ["global", "catalog", scope.as_str()];
    let mut before = Stream::open(&peer, &scopes, None).await;
    before
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.len() == 3
        })
        .await;
    let aligned = before.events_of(&scope);
    assert_eq!(aligned.len(), 1);
    assert_eq!(
        aligned[0]["log"]["entries"],
        json!([]),
        "a known empty log is []"
    );
    assert!(aligned[0]["live"].get("replace").is_some());
    assert_eq!(before.reader.received.len(), 3, "one alignment per scope");

    let prompt = vec![text("hi")];
    let (op, request) = peer.prompt(CREATED, prompt).await;
    peer.update(CREATED, chunk("answer", text("hel"))).await;
    before
        .until(&mut peer, "the first chunk", |reader| {
            reader.live(&session(CREATED))["op"]["process"][0]["content"]["text"] == "hel"
        })
        .await;
    let mut during = Stream::open(&peer, &scopes, None).await;
    during.take_all(&mut peer);
    peer.agent_request(
        "permission",
        "session/request_permission",
        permission(CREATED),
    )
    .await;
    before
        .until(&mut peer, "the permission", |reader| {
            interaction_ids(&reader.live(&session(CREATED))).len() == 1
        })
        .await;
    let interaction = interaction_ids(&before.live(&scope)).remove(0);
    peer.answer(
        &session(CREATED),
        &interaction,
        json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
    )
    .await;
    peer.response("permission").await;
    peer.update(CREATED, chunk("answer", text("lo"))).await;
    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    // A stream opened while the response is in flight: either side of the
    // commit, it gets the turn exactly once.
    let mut boundary = Stream::open(&peer, &scopes, None).await;
    for stream in [&mut before, &mut during, &mut boundary] {
        stream
            .until(&mut peer, "the commit", |reader| {
                reader.log(&session(CREATED)).len() == 1
            })
            .await;
    }
    peer.settled().await;
    let entry = hash_of(&before.log(&scope)[0]);
    for stream in [&before, &during, &boundary] {
        stream.converged(&peer);
        assert_committed_once(stream, &scope, &op, &entry);
        assert!(stream.subscription().try_next().is_none());
    }
    let commit = before
        .events_of(&scope)
        .into_iter()
        .find(|event| event.get("log").is_some() && event["live"].get("patch").is_some())
        .expect("the commit event");
    assert_eq!(commit["log"]["from"], 0);
}

#[tokio::test]
async fn a_held_session_ignores_the_load_directory() {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some("/elsewhere")).await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.contains_key(&session(CREATED))
        })
        .await;
    assert_eq!(stream.events_of(&scope)[0]["log"]["entries"], json!([]));
    peer.quiet(Duration::from_millis(200)).await;
    stream.converged(&peer);
}

// ---------------------------------------------------------------------------
// 2. Cold reopen and first load (protocol §5.1, acceptance 3 and 4)

#[tokio::test]
async fn a_cold_reopen_keeps_the_client_log_and_confirms_it_when_the_load_ends() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let cached = history(&replay).await;
    assert_eq!(cached.len(), 2);
    let scope = session(SAVED);
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut stream = Stream::cached(
        &peer,
        Reader::new(&["global", scope.as_str()]).cache(&scope, cached.clone()),
        Some(CWD),
    )
    .await;
    let load = peer.request("session/load").await;
    assert_eq!(load["params"]["sessionId"], SAVED);
    assert_eq!(load["params"]["cwd"], CWD);
    stream
        .until(&mut peer, "the live-only alignment", |reader| {
            reader.live(&session(SAVED))["op"]["kind"] == "load"
        })
        .await;
    let aligned = stream.events_of(&scope).remove(0);
    assert!(
        aligned.get("log").is_none(),
        "an unknown log is omitted: {aligned}"
    );
    assert_eq!(aligned["live"]["replace"]["op"]["status"], "running");
    assert_eq!(
        stream.log(&scope),
        cached,
        "the client keeps its log, unconfirmed"
    );

    // The load's interactions show while it runs.
    peer.agent_request(
        "question",
        "elicitation/create",
        form(json!({"sessionId": SAVED})),
    )
    .await;
    stream
        .until(&mut peer, "the load's interaction", |reader| {
            interaction_ids(&reader.live(&session(SAVED))).len() == 1
        })
        .await;
    let interaction = interaction_ids(&stream.live(&scope)).remove(0);
    peer.answer(
        &session(SAVED),
        &interaction,
        json!({"response": {"action": "accept", "content": {}}}),
    )
    .await;
    assert_eq!(
        peer.response("question").await["result"]["action"],
        "accept"
    );

    for update in &replay {
        peer.update(SAVED, update.clone()).await;
    }
    peer.reply(&load, json!({})).await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let end = stream.events_of(&scope).pop().unwrap();
    assert_eq!(end["log"]["from"], 2, "the suffix from the baseline: {end}");
    assert_eq!(
        end["log"]["entries"],
        json!([]),
        "an empty suffix confirms the log"
    );
    assert!(
        end["live"].get("patch").is_some(),
        "the same event ends the load"
    );
    assert_eq!(stream.log(&scope), cached);
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn a_cold_reopen_with_a_changed_history_replaces_the_client_log() {
    let cached = history(&turns(&[("one", "1"), ("two", "2")])).await;
    let scope = session(SAVED);
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut stream = Stream::cached(
        &peer,
        Reader::new(&[scope.as_str()]).cache(&scope, cached.clone()),
        Some(CWD),
    )
    .await;
    peer.serve_load(SAVED, &turns(&[("one", "1"), ("other", "3")]))
        .await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let end = stream.events_of(&scope).pop().unwrap();
    assert_eq!(end["log"]["from"], 0, "the baseline does not match: {end}");
    assert_eq!(end["log"]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(hash_of(&stream.log(&scope)[0]), hash_of(&cached[0]));
    assert_ne!(hash_of(&stream.log(&scope)[1]), hash_of(&cached[1]));
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn two_subscriptions_share_one_load_and_each_gets_its_log_from_its_own_head() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let cached = history(&replay).await;
    let scope = session(SAVED);
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut fresh = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    let load = peer.request("session/load").await;
    let mut cached_stream = Stream::cached(
        &peer,
        Reader::new(&[scope.as_str()]).cache(&scope, cached.clone()),
        Some(CWD),
    )
    .await;
    for stream in [&mut fresh, &mut cached_stream] {
        stream
            .until(&mut peer, "the live-only alignment", |reader| {
                reader.live(&session(SAVED))["op"]["kind"] == "load"
            })
            .await;
    }
    assert_eq!(peer.subscribers(SAVED).await, 2);
    for update in &replay {
        peer.update(SAVED, update.clone()).await;
    }
    peer.reply(&load, json!({})).await;
    for stream in [&mut fresh, &mut cached_stream] {
        stream
            .until(&mut peer, "the load ends", |reader| {
                reader.live(&session(SAVED))["op"]["status"] == "completed"
            })
            .await;
    }
    let fresh_end = fresh.events_of(&scope).pop().unwrap();
    assert_eq!(fresh_end["log"]["from"], 0);
    assert_eq!(fresh_end["log"]["entries"].as_array().unwrap().len(), 2);
    let cached_end = cached_stream.events_of(&scope).pop().unwrap();
    assert_eq!(cached_end["log"]["from"], 2);
    assert_eq!(cached_end["log"]["entries"], json!([]));
    peer.quiet(Duration::from_millis(200)).await;
    peer.settled().await;
    fresh.converged(&peer);
    cached_stream.converged(&peer);
}

#[tokio::test]
async fn reconnecting_before_during_and_after_a_load_keeps_one_load_and_converges() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let cached = history(&replay).await;
    let scope = session(SAVED);
    let alignment_count = |reader: &Reader| {
        reader
            .events_of(&scope)
            .iter()
            .filter(|event| event["live"].get("replace").is_some())
            .count()
    };
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let mut stream = Stream::cached(
        &peer,
        Reader::new(&[scope.as_str()]).cache(&scope, cached.clone()),
        Some(CWD),
    )
    .await;
    let load = peer.request("session/load").await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.live(&session(SAVED))["op"]["kind"] == "load"
        })
        .await;

    // Before the replay: the running load keeps the session; a new stream
    // is aligned with live only and starts no second load.
    stream.close();
    stream.reopen(&peer, Some(CWD)).await;
    stream
        .until(&mut peer, "the second alignment", |reader| {
            alignment_count(reader) == 2
        })
        .await;
    assert!(stream.events_of(&scope)[1].get("log").is_none());

    // During the replay.
    peer.update(SAVED, replay[0].clone()).await;
    peer.update(SAVED, replay[1].clone()).await;
    // A barrier: the Agent's next request shows only once the bridge has
    // handled the replay before it.
    peer.agent_request(
        "barrier",
        "elicitation/create",
        form(json!({"sessionId": SAVED})),
    )
    .await;
    stream
        .until(&mut peer, "the barrier", |reader| {
            interaction_ids(&reader.live(&session(SAVED))).len() == 1
        })
        .await;
    stream.reopen(&peer, Some(CWD)).await;
    stream
        .until(&mut peer, "the third alignment", |reader| {
            // The barrier also delivered a live patch. Count alignments,
            // not all messages received across the three subscriptions.
            alignment_count(reader) == 3
        })
        .await;
    let barrier = interaction_ids(&stream.live(&scope)).remove(0);
    peer.answer(
        &session(SAVED),
        &barrier,
        json!({"response": {"action": "accept", "content": {}}}),
    )
    .await;
    peer.response("barrier").await;
    for update in &replay[2..] {
        peer.update(SAVED, update.clone()).await;
    }

    // After the load ended, before its log was delivered: the new stream's
    // alignment carries the log from the baseline.
    peer.reply(&load, json!({})).await;
    peer.until("the load ends", |peer| {
        peer.live(&session(SAVED))["op"]["status"] == "completed"
    })
    .await;
    assert!(
        stream.reader.log(&scope) == cached && stream.subscription().tail(&scope).is_some(),
        "the load's end is queued, not delivered"
    );
    stream.reopen(&peer, Some(CWD)).await;
    stream
        .until(&mut peer, "the aligned load", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let aligned = stream.events_of(&scope).pop().unwrap();
    assert!(aligned["live"].get("replace").is_some(), "{aligned}");
    assert_eq!(aligned["log"]["from"], 2);
    assert_eq!(aligned["log"]["entries"], json!([]));
    peer.quiet(Duration::from_millis(200)).await;
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn an_unheld_session_without_cwd_or_whose_load_fails_ends_with_load_failed() {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut missing = Stream::open(&peer, &["global", scope.as_str()], None).await;
    missing
        .until(&mut peer, "the end", |reader| {
            reader.ended.contains_key(&session(SAVED))
        })
        .await;
    let end = &missing.reader.ended[&scope];
    assert_eq!(end["reason"], "load_failed");
    assert_eq!(end["error"]["code"], "invalid_request");
    assert_eq!(
        missing.events_of(&scope).len(),
        1,
        "the end is the scope's only event"
    );
    peer.quiet(Duration::from_millis(200)).await;
    let before = missing.events_of("global").len();
    let op = peer
        .install("global", OperationKind::Logout, json!({}))
        .await;
    missing
        .until(&mut peer, "a later global change", |reader| {
            reader.live("global")["op"]["id"] == op
        })
        .await;
    assert!(
        missing.events_of("global").len() > before,
        "the stream goes on for its other scopes"
    );

    let mut failing = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    let load = peer.request("session/load").await;
    failing
        .until(&mut peer, "the alignment", |reader| {
            reader.live(&session(SAVED))["op"]["kind"] == "load"
        })
        .await;
    peer.refuse(&load).await;
    failing
        .until(&mut peer, "the end", |reader| {
            reader.ended.contains_key(&session(SAVED))
        })
        .await;
    let end = &failing.reader.ended[&scope];
    assert_eq!(end["reason"], "load_failed");
    assert_eq!(end["error"]["code"], "agent_error");
    peer.settled().await;
}

// ---------------------------------------------------------------------------
// 3. The queue through the bridge

#[tokio::test]
async fn a_paused_stream_follows_its_tail_and_is_coalesced_past_its_budget() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut paused = Stream::open(&peer, &["global", scope.as_str()], None).await;
    paused
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.len() == 2
        })
        .await;

    peer.whole_turn(CREATED, "one", "1").await;
    peer.whole_turn(CREATED, "two", "2").await;
    let published = peer.timeline.last().unwrap()[&scope].clone();
    let head = Head::of(published.log.as_ref().unwrap());
    assert_eq!(head.n, 2);
    let subscription = paused.subscription();
    assert_eq!(subscription.baseline(&scope), Some(Head::zero()));
    assert_eq!(subscription.tail(&scope), Some(head.clone()));
    assert_eq!(subscription.coalesced(), 0);

    // Two queued commits apply in order, each from the tail before it.
    let events = paused.take_all(&mut peer);
    let logs: Vec<&Value> = events
        .iter()
        .filter(|event| event.get("log").is_some())
        .collect();
    assert_eq!(logs.len(), 2, "{events:?}");
    assert_eq!(logs[1]["log"]["from"], 1);
    let delivered = paused.reader.head(&scope);

    // Past the budget: the queue is coalesced from the baseline.
    let prompt = vec![text("flood")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
    for fill in ['a', 'b'] {
        peer.update(CREATED, flood_update(fill)).await;
        peer.until("the flood is published", |peer| {
            flooded(&peer.live(&session(CREATED)), fill)
        })
        .await;
    }
    let subscription = paused.subscription();
    assert!(subscription.coalesced() >= 1, "the queue was coalesced");
    assert_eq!(subscription.baseline(&scope), Some(delivered));
    let events = paused.take_all(&mut peer);
    let aligned: Vec<&Value> = events
        .iter()
        .filter(|event| event["scope"] == scope)
        .collect();
    assert_eq!(aligned.len(), 1, "one alignment of the session: {events:?}");
    assert!(aligned[0]["live"].get("replace").is_some());
    assert_eq!(
        aligned[0]["log"]["from"], 2,
        "from the baseline, applied directly"
    );
    assert_eq!(aligned[0]["log"]["entries"], json!([]));

    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    paused
        .until(&mut peer, "the commit", |reader| {
            reader.log(&session(CREATED)).len() == 3
        })
        .await;
    peer.settled().await;
    paused.converged(&peer);
}

// ---------------------------------------------------------------------------
// 4. Holding and release (S6)

#[tokio::test]
async fn subscribers_hold_a_loadable_session_and_the_last_one_leaving_releases_it() {
    let replay = turns(&[("one", "1")]);
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    let scope = session(SAVED);
    let mut first = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    peer.serve_load(SAVED, &replay).await;
    first
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let mut second = Stream::open(&peer, &["global", scope.as_str()], None).await;
    second
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.contains_key(&session(SAVED))
        })
        .await;
    let globals = Stream::open(&peer, &["global", "catalog"], None).await;
    assert_eq!(
        peer.subscribers(SAVED).await,
        2,
        "global and catalog hold nothing"
    );

    first.close();
    peer.until_subscribers(SAVED, 1).await;
    peer.quiet(Duration::from_millis(300)).await;
    second.close();
    peer.until_subscribers(SAVED, 0).await;
    let close = peer.request("session/close").await;
    assert_eq!(close["params"]["sessionId"], SAVED);
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| {
        peer.released(&session(SAVED))
    })
    .await;
    assert!(
        peer.events_of(&scope)
            .iter()
            .all(|event| event.get("end").is_none()),
        "a release sends no end"
    );
    assert!(peer.publication.lock().publisher.document(&scope).is_none());
    let cached = first.log(&scope);
    assert_eq!(cached.len(), 1, "the client keeps its copy");
    drop(globals);

    // A later subscription loads it again; the same history confirms the
    // client's head.
    first.reopen(&peer, Some(CWD)).await;
    peer.serve_load(SAVED, &replay).await;
    first
        .until(&mut peer, "the reload ends", |reader| {
            reader
                .events_of(&session(SAVED))
                .last()
                .is_some_and(|event| {
                    event["log"]["from"] == 1
                        && reader.live(&session(SAVED))["op"]["status"] == "completed"
                })
        })
        .await;
    assert_eq!(first.log(&scope), cached);
    peer.settled().await;
    first.converged(&peer);
}

#[tokio::test]
async fn a_loadable_session_without_close_is_released_locally() {
    let replay = turns(&[("one", "1")]);
    let mut peer = Peer::start_with(json!({"loadSession": true}), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    peer.serve_load(SAVED, &replay).await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let cached = stream.log(&scope);
    stream.close();
    peer.until("the session is released", |peer| {
        peer.released(&session(SAVED))
    })
    .await;
    peer.quiet(Duration::from_millis(200)).await;
    assert!(
        peer.events_of(&scope)
            .iter()
            .all(|event| event.get("end").is_none()),
        "a release sends no end"
    );
    assert!(peer.publication.lock().publisher.document(&scope).is_none());

    stream.reopen(&peer, Some(CWD)).await;
    peer.serve_load(SAVED, &replay).await;
    stream
        .until(&mut peer, "the reload ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
                && reader.events_of(&session(SAVED)).len() > 2
        })
        .await;
    assert_eq!(stream.log(&scope), cached);
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_released_before_its_first_subscription_is_loaded_by_it() {
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.session_unobserved_timeout = 0;
    })
    .await;
    peer.create(CREATED, json!({})).await;
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    let scope = session(CREATED);
    peer.until("the unneeded session is released", |peer| {
        peer.released(&session(CREATED))
    })
    .await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    peer.serve_load(CREATED, &[]).await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(CREATED))["op"]["status"] == "completed"
        })
        .await;
    assert_eq!(
        stream.events_of(&scope).pop().unwrap()["log"]["entries"],
        json!([])
    );
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_can_neither_load_nor_resume_stays_held() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"close": {}}}),
        json!([]),
        |options| options.session_unobserved_timeout = 0,
    )
    .await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None).await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.contains_key(&session(CREATED))
        })
        .await;
    stream.close();
    peer.until_subscribers(CREATED, 0).await;
    // Neither an ACP close nor a local retirement.
    peer.quiet(Duration::from_millis(500)).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "the session stays held");
    let mut late = Stream::open(&peer, &[scope.as_str()], None).await;
    late.until(&mut peer, "the late alignment", |reader| {
        reader.replicas.contains_key(&session(CREATED))
    })
    .await;
    assert_eq!(late.events_of(&scope)[0]["log"]["entries"], json!([]));
    late.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_can_only_resume_is_released() {
    let mut peer = Peer::start_with(
        json!({"sessionCapabilities": {"resume": {}, "close": {}}}),
        json!([]),
        |options| options.session_unobserved_timeout = 0,
    )
    .await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    let resume = peer.request("session/resume").await;
    assert_eq!(resume["params"]["cwd"], CWD);
    peer.reply(&resume, json!({})).await;
    stream
        .until(&mut peer, "the resume ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    stream.close();
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| {
        peer.released(&session(SAVED))
    })
    .await;
}

/// A loadable session, created and then followed by one stream, with a
/// release timer of one second.
async fn followed(peer_caps: Value) -> (Peer, Stream) {
    let mut peer = Peer::start_with(peer_caps, json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create(CREATED, json!({})).await;
    let mut stream = Stream::open(&peer, &[session(CREATED).as_str()], None).await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.contains_key(&session(CREATED))
        })
        .await;
    (peer, stream)
}

const BEYOND_TIMER: Duration = Duration::from_millis(1500);

#[tokio::test]
async fn an_accepted_url_flow_alone_keeps_the_session_needed() {
    for request_scoped in [false, true] {
        let (mut peer, mut stream) = followed(loadable()).await;
        let scope = session(CREATED);
        let prompt = vec![text("hi")];
        let (_, request) = peer.prompt(CREATED, prompt).await;
        let elicitation_scope = if request_scoped {
            json!({"requestId": request["id"]})
        } else {
            json!({"sessionId": CREATED})
        };
        peer.agent_request(
            "sign-in",
            "elicitation/create",
            url_elicitation(elicitation_scope, "flow"),
        )
        .await;
        stream
            .until(&mut peer, "the URL elicitation", |reader| {
                interaction_ids(&reader.live(&session(CREATED))).len() == 1
            })
            .await;
        let interaction = interaction_ids(&stream.live(&scope)).remove(0);
        peer.answer(
            &session(CREATED),
            &interaction,
            json!({"response": {"action": "accept"}}),
        )
        .await;
        peer.response("sign-in").await;
        peer.reply(&request, json!({"stopReason": "end_turn"}))
            .await;
        stream
            .until(&mut peer, "the prompt ends with its flow open", |reader| {
                let live = reader.live(&session(CREATED));
                live["op"]["status"] == "completed" && live["urlFlows"].get(&interaction).is_some()
            })
            .await;
        stream.close();
        peer.until_subscribers(CREATED, 0).await;
        peer.quiet(BEYOND_TIMER).await;
        peer.settled().await;
        assert!(!peer.released(&scope), "request_scoped={request_scoped}");

        peer.send(json!({
            "jsonrpc": "2.0", "method": "elicitation/complete",
            "params": {"elicitationId": "flow"},
        }))
        .await;
        let close = peer.request("session/close").await;
        peer.reply(&close, json!({})).await;
        peer.until("the session is released", |peer| {
            peer.released(&session(CREATED))
        })
        .await;
    }
}

#[tokio::test]
async fn an_exited_terminal_not_yet_released_keeps_the_session_needed() {
    // The bridge runs Agent terminals for a stdio Agent only, in a real
    // workspace.
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_string_lossy().into_owned();
    let mut peer = Peer::start_with(loadable(), json!([]), |options| {
        options.transport = Transport::Stdio;
        options.command = vec!["/bin/sh".into()];
        options.cwd = workspace.path().to_path_buf();
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create_in(CREATED, &cwd, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &[scope.as_str()], None).await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.contains_key(&session(CREATED))
        })
        .await;
    let prompt = vec![text("run")];
    let (_, request) = peer.prompt(CREATED, prompt).await;
    peer.agent_request(
        "terminal",
        "terminal/create",
        json!({"sessionId": CREATED, "command": "/bin/sh", "args": ["-c", "exit 0"]}),
    )
    .await;
    let created = peer.response("terminal").await;
    let terminal = created["result"]["terminalId"]
        .as_str()
        .unwrap()
        .to_string();
    stream
        .until(&mut peer, "the terminal exits", |reader| {
            reader.live(&session(CREATED))["terminals"]
                .as_object()
                .is_some_and(|terminals| {
                    terminals
                        .values()
                        .any(|terminal| !terminal["exitStatus"].is_null())
                })
        })
        .await;
    peer.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    stream
        .until(&mut peer, "the prompt ends", |reader| {
            reader.live(&session(CREATED))["op"]["status"] == "completed"
        })
        .await;
    stream.close();
    peer.until_subscribers(CREATED, 0).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(
        !peer.released(&scope),
        "an unreleased terminal keeps the need"
    );

    peer.agent_request(
        "release",
        "terminal/release",
        json!({"sessionId": CREATED, "terminalId": terminal}),
    )
    .await;
    peer.response("release").await;
    let close = peer.request("session/close").await;
    peer.reply(&close, json!({})).await;
    peer.until("the session is released", |peer| {
        peer.released(&session(CREATED))
    })
    .await;
}

#[tokio::test]
async fn a_new_subscription_or_an_occupation_cancels_the_release_timer() {
    let (mut peer, mut stream) = followed(loadable()).await;
    let scope = session(CREATED);
    stream.close();
    peer.until_subscribers(CREATED, 0).await;
    stream.reopen(&peer, None).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(
        !peer.released(&scope),
        "a new subscription cancels the timer"
    );

    stream.close();
    peer.until_subscribers(CREATED, 0).await;
    peer.install(
        "global",
        OperationKind::Delete,
        json!({"sessionId": CREATED}),
    )
    .await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(!peer.released(&scope), "an occupation keeps the need");
}

#[tokio::test]
async fn review_a_sessions_own_admitted_operation_keeps_it_held_before_dispatch() {
    let (mut peer, mut stream) = followed(loadable()).await;
    let scope = session(CREATED);
    let op = peer
        .install(
            &scope,
            OperationKind::Prompt,
            json!({"prompt": [text("waiting")]}),
        )
        .await;
    stream
        .until(&mut peer, "the admitted operation", |reader| {
            reader.live(&session(CREATED))["op"]["id"] == op
        })
        .await;
    // The operation owns this session before its ACP task starts. Dropping
    // the last observer must not let a retirement timer close its target.
    stream.close();
    peer.until_subscribers(CREATED, 0).await;
    peer.quiet(BEYOND_TIMER).await;
    peer.settled().await;
    assert!(
        !peer.released(&scope),
        "the session's own operation holds it"
    );
    assert_eq!(peer.live(&scope)["op"]["id"], op);
}

#[tokio::test]
async fn review_a_subscription_after_delete_admission_waits_for_its_end() {
    let caps = json!({"loadSession": true, "sessionCapabilities": {"delete": {}}});
    let mut peer = Peer::start(caps).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;

    // An admitted delete that has not ended occupies the session: a
    // subscription opened meanwhile starts no load and waits for its end.
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    let delete = peer.request("session/delete").await;
    peer.quiet(Duration::from_millis(200)).await;
    assert!(
        stream.take_all(&mut peer).is_empty(),
        "the occupied scope waits"
    );
    peer.reply(&delete, json!({})).await;
    stream
        .until(&mut peer, "the admitted delete ends", |reader| {
            reader.ended.contains_key(&session(SAVED))
        })
        .await;
    assert_eq!(stream.reader.received.len(), 1, "End is the first event");
    assert_eq!(stream.reader.ended[&scope]["reason"], "deleted");
    peer.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_subscription_waits_for_a_cold_delete_and_ends_with_deleted() {
    let caps = json!({"loadSession": true, "sessionCapabilities": {"delete": {}}});
    let mut peer = Peer::start(caps).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;
    let delete = peer.request("session/delete").await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    peer.quiet(Duration::from_millis(200)).await;
    assert!(stream.take_all(&mut peer).is_empty(), "the stream waits");
    peer.reply(&delete, json!({})).await;
    stream
        .until(&mut peer, "the end", |reader| {
            reader.ended.contains_key(&session(SAVED))
        })
        .await;
    assert_eq!(
        stream.reader.received.len(),
        1,
        "the end is the first message"
    );
    assert_eq!(stream.reader.ended[&scope]["reason"], "deleted");
    peer.quiet(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_subscription_waiting_for_a_failed_cold_delete_loads_the_session() {
    let caps = json!({"loadSession": true, "sessionCapabilities": {"delete": {}}});
    let mut peer = Peer::start(caps).await;
    let scope = session(SAVED);
    peer.start_op("global", "delete", json!({"sessionId": SAVED}))
        .await;
    let delete = peer.request("session/delete").await;
    let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
    peer.refuse(&delete).await;
    peer.serve_load(SAVED, &turns(&[("one", "1")])).await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    assert!(stream.reader.ended.is_empty());
    peer.settled().await;
    stream.converged(&peer);
}

// ---------------------------------------------------------------------------
// 5. The Agent connection and the four consumers (§4.1)

/// A loaded `saved`, followed with `global`, on a connection that then ends
/// with an EOF.
async fn loaded_then_eof(replay: &[Value]) -> (Peer, Stream) {
    let mut peer = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&peer, &["global", scope.as_str()], Some(CWD)).await;
    peer.serve_load(SAVED, replay).await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    peer.settled().await;
    peer.eof().await.unwrap();
    stream
        .until(&mut peer, "the waiting load", |reader| {
            let live = reader.live(&session(SAVED));
            live["op"]["kind"] == "load" && live["op"]["status"] == "running"
        })
        .await;
    assert_ne!(
        stream.live("global")["agent"]["phase"],
        "ready",
        "global shows why"
    );
    (peer, stream)
}

async fn next_generation(peer: &mut Peer, caps: Value) -> Peer {
    let (publication, timeline) = peer.handover();
    Peer::start_on(publication, timeline, caps, json!([]), |_| {}).await
}

#[tokio::test]
async fn the_next_connection_s_equal_replay_ends_the_waiting_load() {
    let replay = turns(&[("one", "1"), ("two", "2")]);
    let (mut old, mut stream) = loaded_then_eof(&replay).await;
    let scope = session(SAVED);
    let cached = stream.log(&scope);
    let mut peer = next_generation(&mut old, json!({"loadSession": true})).await;
    let load = peer.serve_load(SAVED, &replay).await;
    assert_eq!(
        load["params"]["cwd"], CWD,
        "the session's earlier directory"
    );
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    assert_eq!(stream.log(&scope), cached, "the same history keeps the log");
    assert_eq!(stream.live("global")["agent"]["phase"], "ready");
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn the_next_connection_s_diverging_replay_rebuilds_from_the_common_prefix() {
    let (mut old, mut stream) = loaded_then_eof(&turns(&[("one", "1"), ("two", "2")])).await;
    let scope = session(SAVED);
    let cached = stream.log(&scope);
    let mut peer = next_generation(&mut old, json!({"loadSession": true})).await;
    peer.serve_load(SAVED, &turns(&[("one", "1"), ("other", "3")]))
        .await;
    stream
        .until(&mut peer, "the load ends", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let rebuilt = stream
        .events_of(&scope)
        .into_iter()
        .rev()
        .find(|event| event.get("log").is_some())
        .unwrap();
    assert_eq!(rebuilt["log"]["from"], 1, "{rebuilt}");
    assert_eq!(hash_of(&stream.log(&scope)[0]), hash_of(&cached[0]));
    assert_ne!(hash_of(&stream.log(&scope)[1]), hash_of(&cached[1]));
    peer.settled().await;
    stream.converged(&peer);
}

#[tokio::test]
async fn review_an_unfollowed_session_is_released_when_the_agent_connection_ends() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &["global"], None).await;
    stream
        .until(&mut peer, "the global alignment", |reader| {
            reader.live("global")["sessions"].get(CREATED).is_some()
        })
        .await;
    assert_eq!(peer.subscribers(CREATED).await, 0);
    assert!(peer.publication.lock().publisher.document(&scope).is_some());

    // EOF completes the final commit; this test starts no next generation.
    peer.eof().await.unwrap();
    stream.take_all(&mut peer);
    assert_eq!(stream.live("global")["agent"]["phase"], "stopped");
    let published = peer.publication.lock();
    assert!(
        published.publisher.document(&scope).is_none(),
        "an unneeded session must leave the publisher at EOF, without waiting for another connection"
    );
    let global = published.publisher.document("global").unwrap();
    assert!(
        global.live["sessions"].get(CREATED).is_none(),
        "the final global document must no longer list the released session"
    );
    assert!(stream.live("global")["sessions"].get(CREATED).is_none());
    drop(published);
    stream.converged(&peer);
}

#[tokio::test]
async fn a_session_the_agent_cannot_load_again_ends_with_load_failed_at_the_connection_end() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&peer, &["global", scope.as_str()], None).await;
    stream
        .until(&mut peer, "the alignment", |reader| {
            reader.replicas.len() == 2
        })
        .await;
    peer.eof().await.unwrap();
    stream
        .until(&mut peer, "the end", |reader| {
            reader.ended.contains_key(&session(CREATED))
        })
        .await;
    assert_eq!(stream.reader.ended[&scope]["reason"], "load_failed");
    assert_eq!(stream.live("global")["agent"]["phase"], "stopped");
    stream.converged(&peer);
}

/// A deterministic generator for the reconnecting consumer.
struct Lcg(u64);

impl Lcg {
    fn draw(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[tokio::test]
async fn four_consumers_converge_one_by_one_lagging_reconnecting_and_paused() {
    let mut peer = Peer::start(json!({})).await;
    peer.create(CREATED, json!({})).await;
    let scope = session(CREATED);
    let scopes = ["global", "catalog", scope.as_str()];
    let mut follower = Stream::open(&peer, &scopes, None).await;
    let mut lagger = Stream::open(&peer, &scopes, None).await;
    let mut reconnector = Stream::open(&peer, &scopes, None).await;
    let mut pauser = Stream::open(&peer, &scopes, None).await;
    let mut random = Lcg(37);
    let mut paused = false;

    for round in 0..4 {
        let prompt = vec![text(&format!("turn {round}"))];
        let (op, request) = peer.prompt(CREATED, prompt).await;
        if round == 2 {
            paused = true;
        }
        for step in 0..2 {
            if round == 2 {
                let fill = ['a', 'b'][step];
                peer.update(CREATED, flood_update(fill)).await;
                peer.until("the flood streams", |peer| {
                    flooded(&peer.live(&session(CREATED)), fill)
                })
                .await;
            } else {
                let part = ["hel", "lo"][step];
                peer.update(CREATED, chunk(&format!("reply-{round}"), text(part)))
                    .await;
                let length = if step == 0 { 3 } else { 5 };
                peer.until("the chunk streams", |peer| {
                    peer.live(&session(CREATED))["op"]["process"][0]["content"]["text"]
                        .as_str()
                        .is_some_and(|text| text.len() == length)
                })
                .await;
            }
            follower.take_all(&mut peer);
            match random.draw() % 3 {
                0 => reconnector.reopen(&peer, None).await,
                1 => {
                    reconnector.take_all(&mut peer);
                }
                _ => {}
            }
            if !paused {
                pauser.take_all(&mut peer);
            }
        }
        if round == 1 {
            peer.agent_request(
                "permission",
                "session/request_permission",
                permission(CREATED),
            )
            .await;
            peer.until("the permission", |peer| {
                interaction_ids(&peer.live(&session(CREATED))).len() == 1
            })
            .await;
            let interaction = interaction_ids(&peer.live(&scope)).remove(0);
            peer.answer(
                &session(CREATED),
                &interaction,
                json!({"outcome": {"outcome": "selected", "optionId": "allow"}}),
            )
            .await;
            peer.response("permission").await;
            reconnector.reopen(&peer, None).await;
        }
        peer.reply(&request, json!({"stopReason": "end_turn"}))
            .await;
        peer.until("the turn completes", |peer| {
            peer.live(&session(CREATED))["op"]["id"] == op
                && peer.live(&session(CREATED))["op"]["status"] == "completed"
        })
        .await;
        follower.take_all(&mut peer);
        if round == 2 {
            let subscription = pauser.subscription();
            assert!(
                subscription.coalesced() >= 1,
                "the paused queue was coalesced"
            );
            assert_eq!(
                subscription.baseline(&scope),
                Some(pauser.reader.head(&scope)),
                "coalescing advanced no baseline"
            );
            let published = peer.timeline.last().unwrap()[&scope].clone();
            assert_eq!(
                subscription.tail(&scope),
                Some(Head::of(published.log.as_ref().unwrap()))
            );
            let resumed = pauser.take_all(&mut peer);
            let first = resumed
                .iter()
                .find(|event| event["scope"] == scope)
                .unwrap();
            assert!(
                first["live"].get("replace").is_some(),
                "the paused stream applies the alignment directly: {first}"
            );
            paused = false;
        }
    }
    assert!(
        lagger.subscription().coalesced() >= 1,
        "the lagging queue was coalesced"
    );
    assert_eq!(follower.subscription().coalesced(), 0);
    peer.settled().await;
    for stream in [&mut follower, &mut lagger, &mut reconnector, &mut pauser] {
        stream.take_all(&mut peer);
        stream.converged(&peer);
    }
    assert_eq!(follower.reader.replicas, lagger.reader.replicas);
    assert_eq!(follower.reader.replicas, reconnector.reader.replicas);
    assert_eq!(follower.reader.replicas, pauser.reader.replicas);
    assert_eq!(follower.log(&scope).len(), 4);
}

// Resume is a valid way to attach after a connection ends, but ACP does not
// promise that it replays history. The bridge must keep the log it already owns.
async fn review_resume_on_next_connection_keeps_known_log(
    old: &mut Peer,
    stream: &mut Stream,
    session_id: &str,
    cached: &[Value],
) {
    let scope = session(session_id);
    let caps = json!({"sessionCapabilities": {"resume": {}}});
    let mut peer = next_generation(old, caps).await;
    stream.take_all(&mut peer);
    assert!(
        !stream.reader.ended.contains_key(&scope),
        "resume support keeps the original subscription alive across connections"
    );
    assert_eq!(stream.log(&scope), cached);
    let resume = peer.request("session/resume").await;
    assert_eq!(resume["params"]["sessionId"], session_id);
    assert_eq!(resume["params"]["cwd"], CWD);
    // A conforming resume response sends no historical message notifications.
    peer.reply(&resume, json!({})).await;
    stream
        .until(
            &mut peer,
            "the resume completes on the original stream",
            |reader| reader.live(&session(session_id))["op"]["status"] == "completed",
        )
        .await;
    peer.settled().await;
    stream.take_all(&mut peer);
    assert!(stream.reader.ended.is_empty());
    assert_eq!(
        stream.log(&scope),
        cached,
        "no resume replay is not evidence of an empty history"
    );
    stream.converged(&peer);
    peer.quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn review_resume_only_session_waits_across_eof_and_preserves_its_known_log() {
    let caps = json!({"sessionCapabilities": {"resume": {}}});
    let mut old = Peer::start(caps).await;
    old.create_in(CREATED, CWD, json!({})).await;
    let scope = session(CREATED);
    let mut stream = Stream::open(&old, &[scope.as_str()], None).await;
    old.whole_turn(CREATED, "keep this prompt", "keep this answer")
        .await;
    stream
        .until(&mut old, "the nonempty history", |reader| {
            reader.log(&session(CREATED)).len() == 1
        })
        .await;
    let cached = stream.log(&scope);
    old.eof().await.unwrap();
    stream.take_all(&mut old);
    assert!(
        !stream.reader.ended.contains_key(&scope),
        "an Agent that supports resume can attach this session again"
    );
    assert_eq!(stream.live(&scope)["op"]["kind"], "load");
    assert_eq!(stream.live(&scope)["op"]["status"], "running");
    assert_eq!(stream.log(&scope), cached);
    review_resume_on_next_connection_keeps_known_log(&mut old, &mut stream, CREATED, &cached).await;
}

#[tokio::test]
async fn review_waiting_session_uses_the_new_connections_resume_capability() {
    let mut old = Peer::start(json!({"loadSession": true})).await;
    let scope = session(SAVED);
    let mut stream = Stream::open(&old, &[scope.as_str()], Some(CWD)).await;
    old.serve_load(SAVED, &turns(&[("one", "1"), ("two", "2")]))
        .await;
    stream
        .until(&mut old, "the initial load completes", |reader| {
            reader.live(&session(SAVED))["op"]["status"] == "completed"
        })
        .await;
    let cached = stream.log(&scope);
    assert_eq!(cached.len(), 2);
    old.eof().await.unwrap();
    stream.take_all(&mut old);
    assert_eq!(stream.live(&scope)["op"]["kind"], "load");
    assert_eq!(stream.live(&scope)["op"]["status"], "running");
    review_resume_on_next_connection_keeps_known_log(&mut old, &mut stream, SAVED, &cached).await;
}

/// Automatic release is still an ordinary ACP close: a refusal keeps the
/// session usable and must not turn an idle timer into a retry loop.
#[tokio::test(start_paused = true)]
async fn review_refused_automatic_close_waits_for_a_new_subscription_cycle() {
    for timeout_seconds in [0, 1] {
        let mut peer = Peer::start_with(loadable(), json!([]), |options| {
            options.session_unobserved_timeout = timeout_seconds;
        })
        .await;
        let scope = session(SAVED);
        let mut stream = Stream::open(&peer, &[scope.as_str()], Some(CWD)).await;
        peer.serve_load(SAVED, &turns(&[("before close", "kept reply")]))
            .await;
        stream
            .until(&mut peer, "the initial load", |reader| {
                reader.live(&session(SAVED))["op"]["status"] == "completed"
            })
            .await;
        let history = stream.log(&scope);
        let before_reopen = stream.events_of(&scope).len();

        stream.close();
        peer.until_subscribers(SAVED, 0).await;
        let refused = peer.request("session/close").await;
        assert_eq!(refused["params"]["sessionId"], SAVED);
        peer.refuse(&refused).await;
        // Cross several full intervals, including the immediate-release case.
        // No other command or subscription may reset the timer during this wait.
        let several_intervals = Duration::from_secs(timeout_seconds.max(1) as u64 * 3);
        peer.quiet(several_intervals).await;
        peer.settled().await;
        assert!(!peer.released(&scope), "a refused close keeps the session");

        stream.reopen(&peer, None).await;
        stream
            .until(&mut peer, "the returning subscription", |reader| {
                reader.events_of(&session(SAVED)).len() > before_reopen
            })
            .await;
        assert_eq!(stream.log(&scope), history);
        peer.quiet(several_intervals).await;
        let (op, prompt) = peer.prompt(SAVED, vec![text("still usable")]).await;
        peer.update(SAVED, chunk("after-refusal", text("still here")))
            .await;
        peer.reply(&prompt, json!({"stopReason": "end_turn"})).await;
        stream
            .until(&mut peer, "the new prompt completes", |reader| {
                reader.live(&session(SAVED))["op"]["id"] == op
                    && reader.live(&session(SAVED))["op"]["status"] == "completed"
            })
            .await;
        assert_eq!(stream.log(&scope).len(), history.len() + 1);
        assert_eq!(
            stream.log(&scope).last().unwrap()["turn"],
            json!({
                "items": [
                    {"type": "message", "role": "user", "chunks": [{"content": text("still usable")}]},
                    {"type": "message", "role": "agent", "messageId": "after-refusal", "chunks": [{"content": text("still here")}]},
                ],
                "end": {"response": {"stopReason": "end_turn"}},
            })
        );
        peer.settled().await;
        stream.converged(&peer);

        stream.close();
        peer.until_subscribers(SAVED, 0).await;
        let close = peer.request("session/close").await;
        assert_eq!(close["params"]["sessionId"], SAVED);
        assert_ne!(close["id"], refused["id"]);
        peer.reply(&close, json!({})).await;
        peer.until("the next close releases the session", |peer| {
            peer.released(&session(SAVED))
        })
        .await;
        peer.quiet(several_intervals).await;
        peer.settled().await;
    }
}

/// A creation's early controls, sent before its response, stay with the new
/// session after an earlier session was locally released.
#[tokio::test(start_paused = true)]
async fn a_new_sessions_early_controls_survive_its_creation_response() {
    let mut peer = Peer::start_with(json!({"loadSession": true}), json!([]), |options| {
        options.session_unobserved_timeout = 1;
    })
    .await;
    peer.create("old", json!({})).await;
    peer.until("the old session is locally released", |peer| {
        peer.released(&session("old"))
    })
    .await;

    let (_, created) = peer.start_op("global", "create", json!({"cwd": CWD})).await;
    let request = peer.request("session/new").await;
    let commands = json!([{"name": "new-command", "description": "For the new session"}]);
    peer.send(json!([
        {
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "new", "update": {
                "sessionUpdate": "available_commands_update", "availableCommands": commands,
            }},
        },
        {"jsonrpc": "2.0", "id": request["id"], "result": {"sessionId": "new"}},
    ]))
    .await;
    assert_eq!(
        Peer::ended(created).await.unwrap(),
        json!({"sessionId": "new"})
    );

    let scope = session("new");
    let mut stream = Stream::open(&peer, &[scope.as_str()], None).await;
    stream
        .until(
            &mut peer,
            "the newly created session's alignment",
            |reader| reader.replicas.contains_key(&session("new")),
        )
        .await;
    assert_eq!(
        stream.live(&scope)["controls"]["commands"],
        commands,
        "the new session keeps its own early controls"
    );
    assert!(peer.live("global")["sessions"].get("old").is_none());
    peer.quiet(Duration::from_secs(2)).await;
    peer.settled().await;
    stream.converged(&peer);
}
