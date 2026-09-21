//! Regression gates for idle retirement, exercised through the real coordinator
//! and ACP transport. The peer is in memory and each test pauses Tokio's clock:
//! protocol replies, rather than wall-clock sleeps, control in-flight work.

use super::*;
use agent_client_protocol::Lines;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::io;

mod close;
mod history;
mod retirement;

struct TestAgent {
    commands: mpsc::UnboundedSender<BridgeInput>,
    requests: futures::channel::mpsc::Receiver<String>,
    responses: futures::channel::mpsc::Sender<io::Result<String>>,
    buffered: VecDeque<Value>,
    events: mpsc::UnboundedReceiver<String>,
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<(), Error>>>,
}

impl TestAgent {
    async fn start(timeout_seconds: i64, capabilities: Value) -> Self {
        let (outgoing, requests) = futures::channel::mpsc::channel::<String>(64);
        let (responses, incoming) = futures::channel::mpsc::channel::<io::Result<String>>(64);
        let transport = Lines::new(outgoing.sink_map_err(io::Error::other), incoming);
        let mut options =
            Options::try_parse_from(["attyd", "--transport", "ws", "--", "ws://127.0.0.1:1/acp"])
                .unwrap()
                .normalized()
                .unwrap();
        options.session_unobserved_timeout = timeout_seconds;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(run_connection(
            transport,
            Arc::new(options),
            command_rx,
            EventSink { tx: events.into() },
            cancellation.clone(),
        ));
        let mut agent = Self {
            commands,
            requests,
            responses,
            buffered: VecDeque::new(),
            events: event_rx,
            cancellation,
            task: Some(task),
        };
        let initialize = agent.next_request().await;
        assert_eq!(initialize["method"], "initialize");
        agent
            .reply(
                &initialize,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": capabilities,
                    "authMethods": [],
                }),
            )
            .await;
        agent
    }

    fn submit(&self, command: Value) -> oneshot::Receiver<Result<Value, BridgeRequestError>> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(BridgeInput::BusinessRequest { command, response })
            .unwrap();
        result
    }

    async fn result(
        receiver: oneshot::Receiver<Result<Value, BridgeRequestError>>,
    ) -> Result<Value, BridgeRequestError> {
        tokio::time::timeout(Duration::from_secs(3), receiver)
            .await
            .expect("business request did not settle after the Agent replied")
            .expect("business response channel closed unexpectedly")
    }

    async fn read_request(&mut self) -> Value {
        loop {
            if let Some(request) = self.buffered.pop_front() {
                return request;
            }
            let raw = self
                .requests
                .next()
                .await
                .expect("ACP connection ended unexpectedly");
            match serde_json::from_str(&raw).unwrap() {
                Value::Array(batch) => self.buffered.extend(batch),
                request => self.buffered.push_back(request),
            }
        }
    }

    async fn next_request(&mut self) -> Value {
        self.request_within(Duration::from_secs(3))
            .await
            .expect("expected request did not reach the Agent")
    }

    async fn request_within(&mut self, duration: Duration) -> Option<Value> {
        tokio::time::timeout(duration, self.read_request())
            .await
            .ok()
    }

    async fn send(&mut self, message: Value) {
        self.responses.send(Ok(message.to_string())).await.unwrap();
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn refuse(&mut self, request: &Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32600, "message": "Close refused"},
        }))
        .await;
    }

    async fn event(&mut self, kind: &str, session_id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let raw = self.events.recv().await.expect("bridge event stream ended");
                let event: Value = serde_json::from_str(&raw).unwrap();
                if event["type"] == kind
                    && (event["sessionId"] == session_id
                        || event["response"]["sessionId"] == session_id)
                {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing {kind} event for {session_id}"))
    }

    async fn stop(mut self) {
        self.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.task.take().unwrap())
            .await
            .expect("bridge did not stop")
            .unwrap()
            .unwrap();
    }
}

impl Drop for TestAgent {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
