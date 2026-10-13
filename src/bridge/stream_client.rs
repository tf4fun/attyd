//! A test client of `/api/v2` over the real routes: snapshot reads, and an
//! SSE stream that feeds a protocol replica kept across reconnects.
use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tower::ServiceExt;

use crate::protocol::Position;
use crate::protocol::stream_replica::StreamReplica;

pub(super) async fn get(router: &Router, path: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The scope parameter for an owner (protocol §4.2).
pub(super) fn owner_scope(session_id: &str, epoch: &str, incarnation: u64) -> String {
    format!("session/{session_id}@{epoch}:{incarnation}")
}

pub(super) struct HttpStream {
    scopes: Vec<String>,
    pub(super) replica: StreamReplica,
    pub(super) last_id: Option<String>,
    /// Every entry read, for assertions.
    pub(super) entries: Vec<Value>,
    messages: Option<mpsc::UnboundedReceiver<(String, String)>>,
    reader: Option<tokio::task::JoinHandle<()>>,
    /// While `false`, the response body is not polled, so the server-side
    /// queue lags as behind a slow network.
    reading: tokio::sync::watch::Sender<bool>,
}

impl HttpStream {
    fn new(scopes: &[String]) -> Self {
        let names = scopes
            .iter()
            .map(|scope| {
                scope
                    .split_once('@')
                    .map_or(scope.as_str(), |(name, _)| name)
            })
            .collect::<Vec<_>>();
        Self {
            scopes: scopes.to_vec(),
            replica: StreamReplica::new(names),
            last_id: None,
            entries: Vec::new(),
            messages: None,
            reader: None,
            reading: tokio::sync::watch::Sender::new(true),
        }
    }

    /// Opens `GET /api/v2/stream` for `scopes`, each `global` or an owner.
    pub(super) async fn open(router: &Router, scopes: &[String]) -> Self {
        let mut stream = Self::new(scopes);
        stream.connect(router).await;
        stream
    }

    /// Opens the stream without reading its body until `resume`.
    pub(super) async fn open_paused(router: &Router, scopes: &[String]) -> Self {
        let mut stream = Self::new(scopes);
        stream.reading.send_replace(false);
        stream.connect(router).await;
        stream
    }

    pub(super) fn resume(&self) {
        self.reading.send_replace(true);
    }

    async fn connect(&mut self, router: &Router) {
        let query = self
            .scopes
            .iter()
            .map(|scope| {
                format!(
                    "scope={}",
                    url::form_urlencoded::byte_serialize(scope.as_bytes()).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::get(format!("/api/v2/stream?{query}"));
        if let Some(id) = &self.last_id {
            request = request.header("last-event-id", id);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (sender, messages) = mpsc::unbounded_channel();
        let mut body = response.into_body().into_data_stream();
        let mut reading = self.reading.subscribe();
        self.reader = Some(tokio::spawn(async move {
            let mut buffer = String::new();
            loop {
                if reading.wait_for(|reading| *reading).await.is_err() {
                    return;
                }
                let Some(Ok(chunk)) = body.next().await else {
                    break;
                };
                buffer.push_str(std::str::from_utf8(&chunk).unwrap());
                while let Some(end) = buffer.find("\n\n") {
                    let frame = buffer.drain(..end + 2).collect::<String>();
                    let mut id = None;
                    let mut data = String::new();
                    for line in frame.lines() {
                        if let Some(value) = line.strip_prefix("id:") {
                            id = Some(value.trim_start().to_string());
                        } else if let Some(value) = line.strip_prefix("data:") {
                            data.push_str(value.strip_prefix(' ').unwrap_or(value));
                        }
                    }
                    // Keep-alive comments carry no id.
                    if let Some(id) = id
                        && sender.send((id, data)).is_err()
                    {
                        return;
                    }
                }
            }
        }));
        self.messages = Some(messages);
    }

    /// Drops the HTTP response, as a closed page or a network failure does.
    pub(super) fn disconnect(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        self.messages = None;
    }

    /// An EventSource retry: the same request, with `Last-Event-ID`.
    pub(super) async fn reconnect(&mut self, router: &Router) {
        self.disconnect();
        tokio::task::yield_now().await;
        self.connect(router).await;
    }

    fn receive(&mut self, id: String, data: String) {
        if let Err(error) = self.replica.receive(&id, &data) {
            panic!("{error}");
        }
        self.entries.push(serde_json::from_str(&data).unwrap());
        self.last_id = Some(id);
    }

    /// Reads until the replica received `seq`; `false` once the stream ended.
    pub(super) async fn pump(&mut self, seq: u64) -> bool {
        while self.replica.received < seq {
            let Some(messages) = self.messages.as_mut() else {
                return false;
            };
            let next = tokio::time::timeout(std::time::Duration::from_secs(3), messages.recv())
                .await
                .unwrap_or_else(|_| panic!("the stream never delivered position {seq}"));
            let Some((id, data)) = next else {
                self.messages = None;
                return false;
            };
            self.receive(id, data);
        }
        true
    }

    /// Reads the next entry, waiting for it.
    pub(super) async fn next(&mut self) -> Value {
        let messages = self.messages.as_mut().expect("the stream is open");
        let (id, data) = tokio::time::timeout(std::time::Duration::from_secs(3), messages.recv())
            .await
            .expect("the stream delivered nothing")
            .expect("the stream ended");
        self.receive(id, data);
        self.entries.last().unwrap().clone()
    }

    /// `true` once the server ended the stream.
    pub(super) async fn ended(&mut self) -> bool {
        let Some(messages) = self.messages.as_mut() else {
            return true;
        };
        match tokio::time::timeout(std::time::Duration::from_secs(3), messages.recv()).await {
            Ok(None) => {
                self.messages = None;
                true
            }
            Ok(Some((id, data))) => {
                self.receive(id, data);
                false
            }
            Err(_) => false,
        }
    }

    /// Fetches the snapshot of every scope waiting after a `reset`.
    pub(super) async fn fetch(&mut self, router: &Router) {
        for scope in self.replica.fetches() {
            let session_id = scope.strip_prefix("session/").unwrap();
            let (status, body) = get(router, &format!("/api/v2/sessions/{session_id}")).await;
            if status != StatusCode::OK {
                // Retired since its reset; the stream's `end` follows.
                assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
                continue;
            }
            let at: Position = serde_json::from_value(body["at"].clone()).unwrap();
            if let Err(error) = self.replica.install(&scope, &at, body["state"].clone()) {
                panic!("{error}");
            }
        }
    }

    /// One convergence check at log position `seq`: once the replica has
    /// applied exactly `seq`, its documents equal the fresh ones (S2).
    /// Returns whether it compared.
    pub(super) async fn check(
        &mut self,
        router: &Router,
        seq: u64,
        fresh: &BTreeMap<String, Value>,
    ) -> bool {
        if !self.pump(seq).await {
            return false;
        }
        loop {
            self.fetch(router).await;
            if self.replica.applied() == seq {
                break;
            }
            if self.replica.received > seq {
                return false; // the bridge moved on; a later check compares
            }
            // Several entries share a position (a cut's snapshot and resets):
            // read the rest of them.
            let Some(messages) = self.messages.as_mut() else {
                return false;
            };
            match tokio::time::timeout(std::time::Duration::from_millis(500), messages.recv()).await
            {
                Ok(Some((id, data))) => self.receive(id, data),
                _ => return false,
            }
        }
        if self.replica.ahead_of(seq) {
            return false; // a fetched snapshot is newer than this cut
        }
        if let Some(divergence) = self.replica.diverges(fresh) {
            panic!("{divergence}");
        }
        true
    }
}

impl Drop for HttpStream {
    fn drop(&mut self) {
        self.disconnect();
    }
}
