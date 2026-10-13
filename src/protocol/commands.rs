//! Command records and the intent table (protocol §3.1, §5.2; plan §2.4).
//!
//! The book lives in the session registry, so it shares the epoch: a new
//! Agent connection starts with no records and no intents. Records are the
//! public `global.requests`; intents are private and are never evicted
//! within their epoch. Once a record is gone, its intent stays as a
//! fixed-size tombstone that still answers a resend.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    ProtocolError, Request, RequestResult, RequestStatus, RequestTarget, TurnOutcomeStatus,
};

/// The default intent table budget per epoch. Tombstones of about a hundred
/// bytes make this cover tens of thousands of commands.
pub(crate) const DEFAULT_INTENT_BUDGET: usize = 4 * 1024 * 1024;

/// The longest accepted `Idempotency-Key`, which bounds a tombstone.
pub(crate) const INTENT_KEY_LIMIT: usize = 200;

/// The minimum retention of finished records (protocol §3.4): the last
/// `RETAINED_RECORDS`, and every record finished within `RECORD_RETENTION`.
pub(crate) const RETAINED_RECORDS: usize = 16;
pub(crate) const RECORD_RETENTION: Duration = Duration::from_secs(600);

/// The fixed part of an intent's accounted bytes: the fingerprint hash, the
/// final status and the table's own overhead.
const INTENT_OVERHEAD: usize = 96;

const REQUEST_PREFIX: &str = "request-";

/// `(operation, complete target, owner tag, body digest)` (protocol §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// `target` lists every path parameter and the scope it belongs to, so
    /// equal bodies sent to different resources never alias.
    pub(crate) fn of(
        operation: &str,
        target: &[(&str, &str)],
        owner_tag: &str,
        body: &Value,
    ) -> Self {
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        };
        field(operation.as_bytes());
        for (name, value) in target {
            field(name.as_bytes());
            field(value.as_bytes());
        }
        field(owner_tag.as_bytes());
        field(canonical(body).as_bytes());
        Self(hasher.finalize().into())
    }
}

/// JSON with object keys sorted, so a body's digest does not depend on the
/// order a client wrote its keys in.
fn canonical(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys = map.keys().collect::<Vec<_>>();
                keys.sort();
                out.push('{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// What an intent produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Subject {
    Request(String),
    /// A turn; `operation_id` is unknown until the bridge admits it.
    Turn {
        session_id: String,
        incarnation: u64,
        operation_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalStatus {
    Request(RequestStatus),
    Turn(TurnOutcomeStatus),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Intent {
    pub fingerprint: Fingerprint,
    pub subject: Subject,
    /// `None` while the subject runs.
    pub status: Option<FinalStatus>,
    /// The incarnation a finished request returned (open, fork), kept so a
    /// resend can renew its lease after the record is gone. Incarnations are
    /// unique within an epoch, so this fits the fixed overhead.
    pub owner: Option<u64>,
    /// Which admission produced this intent; a stale caller cannot withdraw
    /// a later one.
    attempt: u64,
    bytes: usize,
}

/// The intent table's answer for a key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IntentLookup<'a> {
    /// A first submission; the table has room for it.
    Unknown,
    Known(&'a Intent),
    /// The key was used with a different fingerprint.
    Conflict,
    /// A first submission, but the table is full (`intent_capacity`).
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcknowledgeError {
    NotFound,
    Running,
}

#[derive(Debug, Clone)]
pub(crate) struct CommandBook {
    requests: BTreeMap<String, Request>,
    /// Finished records, oldest first, with their finish time.
    finished: VecDeque<(String, Instant)>,
    intents: HashMap<String, Intent>,
    /// Turn intents whose final status is not known yet.
    running_turns: BTreeSet<String>,
    intent_bytes: usize,
    intent_budget: usize,
    retained_records: usize,
    record_retention: Duration,
    next_request: u64,
    next_attempt: u64,
}

impl Default for CommandBook {
    fn default() -> Self {
        Self {
            requests: BTreeMap::new(),
            finished: VecDeque::new(),
            intents: HashMap::new(),
            running_turns: BTreeSet::new(),
            intent_bytes: 0,
            intent_budget: DEFAULT_INTENT_BUDGET,
            retained_records: RETAINED_RECORDS,
            record_retention: RECORD_RETENTION,
            next_request: 0,
            next_attempt: 0,
        }
    }
}

/// Whether `id` names a record of this book; a session operation a command
/// started carries its record's ID as its operation ID.
pub(crate) fn is_request_id(id: &str) -> bool {
    id.strip_prefix(REQUEST_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

impl CommandBook {
    #[cfg(test)]
    pub(crate) fn set_intent_budget(&mut self, budget: usize) {
        self.intent_budget = budget;
    }

    #[cfg(test)]
    pub(crate) fn set_retention(&mut self, records: usize, age: Duration) {
        self.retained_records = records;
        self.record_retention = age;
    }

    #[cfg(test)]
    pub(crate) fn intent_bytes(&self) -> usize {
        self.intent_bytes
    }

    pub(crate) fn requests(&self) -> &BTreeMap<String, Request> {
        &self.requests
    }

    pub(crate) fn request(&self, request_id: &str) -> Option<&Request> {
        self.requests.get(request_id)
    }

    pub(crate) fn intent(&self, key: &str) -> Option<&Intent> {
        self.intents.get(key)
    }

    pub(crate) fn lookup(&self, key: &str, fingerprint: Fingerprint) -> IntentLookup<'_> {
        match self.intents.get(key) {
            Some(intent) if intent.fingerprint == fingerprint => IntentLookup::Known(intent),
            Some(_) => IntentLookup::Conflict,
            None if self.intent_bytes + intent_bytes(key, None) > self.intent_budget => {
                IntentLookup::Full
            }
            None => IntentLookup::Unknown,
        }
    }

    /// The next record ID, unique in the epoch.
    pub(crate) fn allocate_request_id(&mut self) -> String {
        self.next_request += 1;
        format!("{REQUEST_PREFIX}{}", self.next_request)
    }

    /// Records a first submission. The caller checked `lookup` under the
    /// same lock, so the key is new and the table has room. Returns the
    /// admission's attempt, which `withdraw_attempt` needs.
    pub(crate) fn admit(&mut self, key: &str, fingerprint: Fingerprint, subject: Subject) -> u64 {
        let bytes = intent_bytes(key, Some(&subject));
        if matches!(subject, Subject::Turn { .. }) {
            self.running_turns.insert(key.to_string());
        }
        self.next_attempt += 1;
        self.intent_bytes += bytes;
        self.intents.insert(
            key.to_string(),
            Intent {
                fingerprint,
                subject,
                status: None,
                owner: None,
                attempt: self.next_attempt,
                bytes,
            },
        );
        self.next_attempt
    }

    /// Withdraws a turn intent the bridge never admitted, if it is still the
    /// admission `attempt` made. An admitted turn keeps its intent.
    pub(crate) fn withdraw_attempt(&mut self, key: &str, attempt: u64) {
        if self.intents.get(key).is_some_and(|intent| {
            intent.attempt == attempt
                && matches!(
                    intent.subject,
                    Subject::Turn {
                        operation_id: None,
                        ..
                    }
                )
        }) {
            self.withdraw(key);
        }
    }

    /// The bridge refused a turn of `key` on `session_id` before admitting
    /// it. Only a still-unadmitted v2 intent for that session is withdrawn: a
    /// v1 turn may reuse the key on another session.
    pub(crate) fn turn_refused(&mut self, key: &str, session_id: &str) {
        if self
            .intents
            .get(key)
            .is_some_and(|intent| pending_turn_on(intent, session_id))
        {
            self.withdraw(key);
        }
    }

    /// Withdraws an intent whose command was rejected after all: a rejected
    /// command commits nothing, so its key stays unknown.
    pub(crate) fn withdraw(&mut self, key: &str) {
        if let Some(intent) = self.intents.remove(key) {
            self.intent_bytes -= intent.bytes;
        }
        self.running_turns.remove(key);
    }

    /// The bridge admitted a turn of `key` on `session_id` as
    /// `operation_id`. It settles only a still-unadmitted v2 intent for that
    /// session; another session's turn under the same key changes nothing.
    pub(crate) fn turn_admitted(&mut self, key: &str, session_id: &str, operation_id: &str) {
        if let Some(intent) = self
            .intents
            .get_mut(key)
            .filter(|intent| pending_turn_on(intent, session_id))
            && let Subject::Turn {
                operation_id: admitted,
                ..
            } = &mut intent.subject
        {
            *admitted = Some(operation_id.to_string());
        }
    }

    /// Turn intents still running: `(key, session_id, incarnation, operation_id)`.
    pub(crate) fn running_turns(&self) -> impl Iterator<Item = (&str, &str, u64, Option<&str>)> {
        self.running_turns.iter().filter_map(|key| {
            let Subject::Turn {
                session_id,
                incarnation,
                operation_id,
            } = &self.intents.get(key)?.subject
            else {
                return None;
            };
            Some((
                key.as_str(),
                session_id.as_str(),
                *incarnation,
                operation_id.as_deref(),
            ))
        })
    }

    pub(crate) fn settle_turn(&mut self, key: &str, status: TurnOutcomeStatus) {
        if self.running_turns.remove(key)
            && let Some(intent) = self.intents.get_mut(key)
        {
            intent.status = Some(FinalStatus::Turn(status));
        }
    }

    /// Opens a running record.
    pub(crate) fn open(&mut self, request: Request) {
        self.requests.insert(request.request_id.clone(), request);
    }

    /// Sets the owner a running record targets, once the bridge knows it.
    pub(crate) fn target(&mut self, request_id: &str, target: RequestTarget) {
        if let Some(request) = self
            .requests
            .get_mut(request_id)
            .filter(|request| request.status == RequestStatus::Running)
        {
            request.target = Some(target);
        }
    }

    /// Finishes a running record; a finished record never changes again.
    pub(crate) fn finish(
        &mut self,
        request_id: &str,
        outcome: Result<RequestResult, ProtocolError>,
        now: Instant,
    ) -> bool {
        let status = match &outcome {
            Ok(_) => RequestStatus::Succeeded,
            Err(_) => RequestStatus::Failed,
        };
        self.end(
            request_id,
            status,
            outcome.as_ref().ok().cloned(),
            outcome.err(),
            now,
        )
    }

    pub(crate) fn cancel(&mut self, request_id: &str, now: Instant) -> bool {
        self.end(request_id, RequestStatus::Cancelled, None, None, now)
    }

    fn end(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        result: Option<RequestResult>,
        error: Option<ProtocolError>,
        now: Instant,
    ) -> bool {
        let Some(request) = self
            .requests
            .get_mut(request_id)
            .filter(|request| request.status == RequestStatus::Running)
        else {
            return false;
        };
        request.status = status;
        request.result = result;
        request.error = error;
        if let Some(intent) = request
            .client_intent_id
            .as_deref()
            .and_then(|key| self.intents.get_mut(key))
        {
            intent.status = Some(FinalStatus::Request(status));
            if let Some(RequestResult::Session { incarnation, .. }) = &request.result {
                intent.owner = Some(*incarnation);
            }
        }
        self.finished.push_back((request_id.to_string(), now));
        self.retain(now);
        true
    }

    /// `DELETE /api/v2/requests/{requestId}`: a control action that removes a
    /// finished record and creates nothing.
    pub(crate) fn acknowledge(&mut self, request_id: &str) -> Result<(), AcknowledgeError> {
        match self.requests.get(request_id) {
            None => Err(AcknowledgeError::NotFound),
            Some(request) if request.status == RequestStatus::Running => {
                Err(AcknowledgeError::Running)
            }
            Some(_) => {
                self.requests.remove(request_id);
                self.finished.retain(|(id, _)| id != request_id);
                Ok(())
            }
        }
    }

    /// Drops finished records outside both retention minimums: a record
    /// leaves only once it is older than the window and the last
    /// `retained_records` stay.
    pub(crate) fn retain(&mut self, now: Instant) {
        while let Some((request_id, finished_at)) = self.finished.front() {
            let over = self.finished.len() > self.retained_records;
            let expired = now.saturating_duration_since(*finished_at) >= self.record_retention;
            if !(over && expired) {
                break;
            }
            self.requests.remove(request_id);
            self.finished.pop_front();
        }
    }
}

fn pending_turn_on(intent: &Intent, session: &str) -> bool {
    matches!(
        &intent.subject,
        Subject::Turn {
            session_id,
            operation_id: None,
            ..
        } if session_id == session
    )
}

fn intent_bytes(key: &str, subject: Option<&Subject>) -> usize {
    let subject = match subject {
        Some(Subject::Request(id)) => id.len(),
        Some(Subject::Turn {
            session_id,
            operation_id,
            ..
        }) => session_id.len() + operation_id.as_ref().map_or(64, String::len),
        // Room for any subject: a lookup must not admit what cannot fit.
        None => 128,
    };
    INTENT_OVERHEAD + key.len() + subject
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::{ErrorCode, RequestOperation};

    fn record(book: &mut CommandBook, key: &str) -> String {
        let request_id = book.allocate_request_id();
        book.open(Request {
            request_id: request_id.clone(),
            operation: RequestOperation::SetMode,
            method: "session/set_mode".into(),
            client_intent_id: Some(key.into()),
            target: Some(RequestTarget {
                session_id: "s".into(),
                incarnation: Some(1),
            }),
            interaction_id: None,
            method_id: None,
            params: BTreeMap::new(),
            status: RequestStatus::Running,
            result: None,
            error: None,
            terminal: None,
        });
        request_id
    }

    fn mode(value: &str) -> Fingerprint {
        Fingerprint::of(
            "set_mode",
            &[("sessionId", "s")],
            "e:1",
            &json!({ "modeId": value }),
        )
    }

    #[test]
    fn fingerprints_cover_operation_target_owner_and_body_but_not_key_order() {
        let body = json!({ "value": "on", "nested": { "b": 1, "a": [1, 2] } });
        let reordered: Value =
            serde_json::from_str(r#"{"nested":{"a":[1,2],"b":1},"value":"on"}"#).unwrap();
        let config = |config_id: &str, tag: &str, body: &Value| {
            Fingerprint::of(
                "set_config",
                &[("sessionId", "s"), ("configId", config_id)],
                tag,
                body,
            )
        };
        assert_eq!(config("a", "e:1", &body), config("a", "e:1", &reordered));
        assert_ne!(config("a", "e:1", &body), config("b", "e:1", &body));
        assert_ne!(config("a", "e:1", &body), config("a", "e:2", &body));
        assert_ne!(
            config("a", "e:1", &body),
            config("a", "e:1", &json!({ "value": "off" }))
        );
        assert_ne!(
            Fingerprint::of("set_mode", &[("sessionId", "s")], "e:1", &json!({})),
            Fingerprint::of("close", &[("sessionId", "s")], "e:1", &json!({})),
        );
        // A field boundary cannot be shifted between parts.
        assert_ne!(
            Fingerprint::of("ab", &[], "", &json!({})),
            Fingerprint::of("a", &[], "b", &json!({})),
        );
    }

    #[test]
    fn a_known_key_answers_from_its_record_and_then_from_its_tombstone() {
        let mut book = CommandBook::default();
        assert_eq!(book.lookup("k", mode("a")), IntentLookup::Unknown);
        let request_id = record(&mut book, "k");
        book.admit("k", mode("a"), Subject::Request(request_id.clone()));
        assert_eq!(book.lookup("k", mode("b")), IntentLookup::Conflict);
        let IntentLookup::Known(intent) = book.lookup("k", mode("a")) else {
            panic!("the key is known");
        };
        assert_eq!(intent.subject, Subject::Request(request_id.clone()));
        assert_eq!(intent.status, None);

        assert_eq!(
            book.acknowledge(&request_id),
            Err(AcknowledgeError::Running),
            "a running record cannot be acknowledged"
        );
        let refused = ProtocolError::new(ErrorCode::AgentError, "refused");
        assert!(book.finish(&request_id, Err(refused.clone()), Instant::now()));
        assert!(!book.finish(&request_id, Ok(RequestResult::Done), Instant::now()));
        assert_eq!(book.request(&request_id).unwrap().error, Some(refused));
        let bytes = book.intent_bytes();
        book.acknowledge(&request_id).unwrap();
        assert_eq!(
            book.acknowledge(&request_id),
            Err(AcknowledgeError::NotFound)
        );
        assert!(book.requests().is_empty());
        let IntentLookup::Known(intent) = book.lookup("k", mode("a")) else {
            panic!("the tombstone outlives its record");
        };
        assert_eq!(
            intent.status,
            Some(FinalStatus::Request(RequestStatus::Failed))
        );
        assert_eq!(book.intent_bytes(), bytes, "a tombstone keeps its size");
    }

    #[test]
    fn a_full_table_refuses_new_keys_and_still_answers_known_ones() {
        let mut book = CommandBook::default();
        // Room for one admitted intent plus one lookup's worst case.
        book.set_intent_budget(
            intent_bytes("k1", Some(&Subject::Request("request-1".into())))
                + intent_bytes("k2", None),
        );
        for key in ["k1", "k2"] {
            assert_eq!(book.lookup(key, mode(key)), IntentLookup::Unknown);
            let request_id = record(&mut book, key);
            book.admit(key, mode(key), Subject::Request(request_id));
        }
        assert_eq!(book.lookup("k3", mode("k3")), IntentLookup::Full);
        assert!(matches!(
            book.lookup("k1", mode("k1")),
            IntentLookup::Known(_)
        ));
        assert_eq!(book.lookup("k2", mode("other")), IntentLookup::Conflict);
        book.withdraw("k2");
        assert_eq!(book.lookup("k3", mode("k3")), IntentLookup::Unknown);
    }

    #[test]
    fn returned_owners_fit_the_intent_budget_after_running_requests_finish() {
        let mut book = CommandBook::default();
        let budget = 550;
        book.set_intent_budget(budget);
        let mut accepted = Vec::new();
        loop {
            let index = accepted.len();
            let key = format!("k{index}");
            let source = format!("source-{index}");
            let fingerprint = Fingerprint::of("fork", &[("sessionId", &source)], "e:1", &json!({}));
            match book.lookup(&key, fingerprint) {
                IntentLookup::Full => break,
                IntentLookup::Unknown => {}
                other => panic!("a fresh key must be admitted or refused for capacity: {other:?}"),
            }
            let request_id = book.allocate_request_id();
            book.open(Request {
                request_id: request_id.clone(),
                operation: RequestOperation::Fork,
                method: "session/fork".into(),
                client_intent_id: Some(key.clone()),
                target: Some(RequestTarget {
                    session_id: source,
                    incarnation: Some(1),
                }),
                interaction_id: None,
                method_id: None,
                params: BTreeMap::new(),
                status: RequestStatus::Running,
                result: None,
                error: None,
                terminal: None,
            });
            book.admit(&key, fingerprint, Subject::Request(request_id.clone()));
            accepted.push((key, fingerprint, request_id));
            assert!(
                book.intent_bytes() <= budget,
                "admission must respect the intent budget"
            );
        }
        assert!(!accepted.is_empty(), "the budget has room for a command");

        // All accepted commands are still running when capacity is reached.
        // Their successful results must remain recoverable after ACK, without
        // adding unreserved owner storage to the permanent intent table.
        for (index, (_, _, request_id)) in accepted.iter().enumerate() {
            assert!(book.finish(
                request_id,
                Ok(RequestResult::Session {
                    session_id: format!("00000000-0000-0000-0000-{index:012}"),
                    incarnation: index as u64 + 1,
                }),
                Instant::now(),
            ));
            book.acknowledge(request_id).unwrap();
        }
        assert!(book.requests().is_empty());
        for (key, fingerprint, request_id) in &accepted {
            let IntentLookup::Known(intent) = book.lookup(key, *fingerprint) else {
                panic!("an accepted key must still answer after acknowledgement");
            };
            assert_eq!(intent.subject, Subject::Request(request_id.clone()));
            assert_eq!(
                intent.status,
                Some(FinalStatus::Request(RequestStatus::Succeeded))
            );
        }
        assert!(
            book.intent_bytes() <= budget,
            "finished owner results exceeded the {budget}-byte intent budget: {} bytes remain after ACK",
            book.intent_bytes()
        );
    }

    #[test]
    fn finished_records_stay_for_the_last_count_and_the_retention_window() {
        let mut book = CommandBook::default();
        book.set_retention(2, Duration::from_secs(60));
        let start = Instant::now();
        let ids = (0..4)
            .map(|index| {
                let id = record(&mut book, &format!("k{index}"));
                book.finish(&id, Ok(RequestResult::Done), start);
                id
            })
            .collect::<Vec<_>>();
        assert_eq!(book.requests().len(), 4, "all are inside the window");
        book.retain(start + Duration::from_secs(60));
        assert_eq!(
            book.requests().keys().cloned().collect::<BTreeSet<_>>(),
            ids[2..].iter().cloned().collect(),
            "past the window only the last two stay"
        );
        let running = record(&mut book, "running");
        book.retain(start + Duration::from_secs(3600));
        assert!(
            book.request(&running).is_some(),
            "running records never leave"
        );
        assert_eq!(book.requests().len(), 3);
    }

    #[test]
    fn a_burst_of_finished_requests_keeps_every_result_for_ten_minutes() {
        let mut book = CommandBook::default();
        let start = Instant::now();
        let ids = (0..4097)
            .map(|index| {
                let key = format!("burst-{index}");
                let id = record(&mut book, &key);
                book.admit(&key, mode("code"), Subject::Request(id.clone()));
                book.finish(&id, Ok(RequestResult::Done), start);
                id
            })
            .collect::<Vec<_>>();

        book.retain(start + Duration::from_secs(599));
        assert_eq!(
            book.requests().len(),
            ids.len(),
            "protocol §3.4 keeps every unacknowledged result for at least ten minutes, even during a burst"
        );
        assert!(book.request(&ids[0]).is_some());

        book.retain(start + Duration::from_secs(600));
        assert_eq!(
            book.requests().keys().cloned().collect::<BTreeSet<_>>(),
            ids[ids.len() - 16..].iter().cloned().collect(),
            "after the time minimum, the last sixteen finished records remain"
        );
    }

    #[test]
    fn turn_intents_settle_once_and_withdrawn_keys_are_unknown_again() {
        let mut book = CommandBook::default();
        let turn = Fingerprint::of("turn", &[("sessionId", "s")], "e:1:r", &json!({}));
        book.admit(
            "t",
            turn,
            Subject::Turn {
                session_id: "s".into(),
                incarnation: 1,
                operation_id: None,
            },
        );
        book.turn_admitted("t", "other", "op");
        book.turn_admitted("t", "s", "op");
        book.turn_admitted("t", "s", "later");
        assert_eq!(
            book.running_turns().collect::<Vec<_>>(),
            vec![("t", "s", 1, Some("op"))]
        );
        book.settle_turn("t", TurnOutcomeStatus::Completed);
        book.settle_turn("t", TurnOutcomeStatus::Failed);
        assert_eq!(book.running_turns().count(), 0);
        assert_eq!(
            book.intent("t").unwrap().status,
            Some(FinalStatus::Turn(TurnOutcomeStatus::Completed))
        );

        book.admit(
            "u",
            turn,
            Subject::Turn {
                session_id: "s".into(),
                incarnation: 1,
                operation_id: None,
            },
        );
        book.withdraw("u");
        assert_eq!(book.lookup("u", turn), IntentLookup::Unknown);
        assert_eq!(book.running_turns().count(), 0);
    }

    #[test]
    fn request_ids_are_recognizable_and_unique() {
        let mut book = CommandBook::default();
        let first = book.allocate_request_id();
        assert_ne!(first, book.allocate_request_id());
        assert!(is_request_id(&first));
        assert!(!is_request_id("bridge-materialize-1"));
        assert!(!is_request_id("request-"));
    }
}
