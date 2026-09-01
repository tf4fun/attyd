use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_client_protocol::schema::v1::*;
use agent_client_protocol::{Agent, ConnectionTo, Error, RequestCancellation};
use serde_json::{Map, Value, json, value::to_raw_value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::mcp_config::AcpMcpProvider;

const MAX_CONNECTIONS: usize = 16;
const MAX_PENDING_REQUESTS: usize = 128;
const MAX_MESSAGE_BYTES: usize = 3_000_000;
const MAX_INPUT_BYTES: usize = 5_000_000;

type PendingResult = Result<Value, Error>;

struct PendingMcpRequest {
    method: String,
    sender: oneshot::Sender<PendingResult>,
}

#[derive(Clone)]
pub struct McpManager {
    cwd: std::path::PathBuf,
    providers: Arc<HashMap<String, AcpMcpProvider>>,
    connections: Arc<Mutex<HashMap<String, Arc<McpConnection>>>>,
    connect_lock: Arc<Mutex<()>>,
    events: mpsc::UnboundedSender<String>,
}

struct McpConnection {
    id: String,
    provider: AcpMcpProvider,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<String, PendingMcpRequest>>,
    next_request_id: AtomicU64,
    announced: AtomicBool,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

impl McpManager {
    pub fn new(
        cwd: std::path::PathBuf,
        providers: Vec<AcpMcpProvider>,
        events: mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            cwd,
            providers: Arc::new(
                providers
                    .into_iter()
                    .map(|provider| (provider.server_id.clone(), provider))
                    .collect(),
            ),
            connections: Arc::new(Mutex::new(HashMap::new())),
            connect_lock: Arc::new(Mutex::new(())),
            events,
        }
    }

    pub async fn connect(
        &self,
        request: ConnectMcpRequest,
        acp: ConnectionTo<Agent>,
        cancellation: RequestCancellation,
    ) -> Result<ConnectMcpResponse, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::request_cancelled());
        }
        let _connect_guard = self.connect_lock.lock().await;
        if cancellation.is_cancelled() {
            return Err(Error::request_cancelled());
        }
        if self.connections.lock().await.len() >= MAX_CONNECTIONS {
            return Err(Error::new(
                -32000,
                format!("at most {MAX_CONNECTIONS} MCP connections are allowed"),
            ));
        }
        let server_id = request.server_id.0.to_string();
        let provider = self.providers.get(&server_id).cloned().ok_or_else(|| {
            Error::resource_not_found(None).data(format!("unknown ACP MCP server: {server_id}"))
        })?;
        let mut command = Command::new(&provider.command);
        command
            .args(&provider.args)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for variable in &provider.env {
            command.env(&variable.name, &variable.value);
        }
        let mut child = command.spawn().map_err(|error| {
            Error::new(
                -32000,
                format!("could not start MCP server {}", provider.name),
            )
            .data(error.to_string())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::internal_error().data("MCP child did not provide stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal_error().data("MCP child did not provide stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let events = self.events.clone();
            let name = provider.name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut chunk = Vec::new();
                loop {
                    chunk.clear();
                    match reader.read_until(b'\n', &mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&chunk);
                            let _ = events.send(
                                json!({
                                    "type": "bridge/stderr",
                                    "chunk": format!("[MCP {name}] {text}"),
                                })
                                .to_string(),
                            );
                        }
                    }
                }
            });
        }
        let id = Uuid::new_v4().to_string();
        let (kill_tx, kill_rx) = oneshot::channel();
        let connection = Arc::new(McpConnection {
            id: id.clone(),
            provider: provider.clone(),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_request_id: AtomicU64::new(0),
            announced: AtomicBool::new(false),
            kill: Mutex::new(Some(kill_tx)),
        });
        self.connections
            .lock()
            .await
            .insert(id.clone(), connection.clone());
        self.spawn_reader(connection.clone(), stdout, acp);
        self.spawn_waiter(connection.clone(), child, kill_rx);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                self.terminate(&connection, Error::request_cancelled()).await;
                return Err(Error::request_cancelled());
            }
            () = tokio::task::yield_now() => {}
        }
        if cancellation.is_cancelled() {
            self.terminate(&connection, Error::request_cancelled())
                .await;
            return Err(Error::request_cancelled());
        }
        if !self.connections.lock().await.contains_key(&id) {
            return Err(Error::new(
                -32_000,
                format!("MCP server {} exited during startup", provider.name),
            ));
        }
        connection.announced.store(true, Ordering::Release);
        self.connection_event("connected", &connection);
        Ok(ConnectMcpResponse::new(id))
    }

    pub async fn message(
        &self,
        request: MessageMcpRequest,
        cancellation: RequestCancellation,
    ) -> Result<MessageMcpResponse, Error> {
        validate_message(&request.method, request.params.as_ref())?;
        let connection = self.require(&request.connection_id.0).await?;
        if connection.pending.lock().await.len() >= MAX_PENDING_REQUESTS {
            return Err(Error::new(
                -32000,
                format!("at most {MAX_PENDING_REQUESTS} MCP requests may be pending"),
            ));
        }
        let id = format!(
            "attyd-{}",
            connection.next_request_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (sender, receiver) = oneshot::channel();
        connection.pending.lock().await.insert(
            id.clone(),
            PendingMcpRequest {
                method: request.method.clone(),
                sender,
            },
        );
        self.activity(
            "agent-to-server",
            &connection,
            &request.method,
            "request",
            request
                .params
                .as_ref()
                .map(|value| Value::Object(value.clone())),
            None,
        );
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": request.method,
            "params": request.params,
        });
        if let Err(error) = write_json(&connection, &message).await {
            connection.pending.lock().await.remove(&id);
            return Err(error);
        }
        let result = tokio::select! {
            result = receiver => result.map_err(|_| Error::request_cancelled())?,
            _ = cancellation.cancelled() => {
                connection.pending.lock().await.remove(&id);
                let params = json!({ "requestId": id, "reason": "ACP request cancelled" });
                let _ = write_json(&connection, &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": params,
                })).await;
                self.activity(
                    "agent-to-server",
                    &connection,
                    "notifications/cancelled",
                    "notification",
                    Some(params),
                    None,
                );
                return Err(Error::request_cancelled());
            }
        };
        let value = result?;
        let raw = to_raw_value(&value).map_err(Error::from)?;
        Ok(MessageMcpResponse::new(Arc::from(raw)))
    }

    pub async fn notify(&self, notification: MessageMcpNotification) -> Result<(), Error> {
        validate_message(&notification.method, notification.params.as_ref())?;
        let connection = self.require(&notification.connection_id.0).await?;
        self.activity(
            "agent-to-server",
            &connection,
            &notification.method,
            "notification",
            notification
                .params
                .as_ref()
                .map(|value| Value::Object(value.clone())),
            None,
        );
        write_json(
            &connection,
            &json!({
                "jsonrpc": "2.0",
                "method": notification.method,
                "params": notification.params,
            }),
        )
        .await
    }

    pub async fn disconnect(
        &self,
        request: DisconnectMcpRequest,
    ) -> Result<DisconnectMcpResponse, Error> {
        let connection = self.require(&request.connection_id.0).await?;
        self.terminate(
            &connection,
            Error::new(-32_000, "MCP connection was disconnected"),
        )
        .await;
        Ok(DisconnectMcpResponse::new())
    }

    pub async fn close_all(&self) {
        let connections = self
            .connections
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for connection in connections {
            self.terminate(&connection, Error::request_cancelled())
                .await;
        }
    }

    async fn require(&self, connection_id: &str) -> Result<Arc<McpConnection>, Error> {
        self.connections
            .lock()
            .await
            .get(connection_id)
            .cloned()
            .ok_or_else(|| {
                Error::resource_not_found(None)
                    .data(format!("unknown MCP connection: {connection_id}"))
            })
    }

    fn spawn_reader(
        &self,
        connection: Arc<McpConnection>,
        stdout: ChildStdout,
        acp: ConnectionTo<Agent>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let count = match read_bounded_line(&mut reader, &mut line, MAX_INPUT_BYTES).await {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) => {
                        manager
                            .terminate(
                                &connection,
                                Error::invalid_request().data(error.to_string()),
                            )
                            .await;
                        break;
                    }
                };
                if count > MAX_MESSAGE_BYTES {
                    manager
                        .terminate(
                            &connection,
                            Error::invalid_request()
                                .data("MCP server emitted an oversized message"),
                        )
                        .await;
                    break;
                }
                if line.ends_with(b"\n") {
                    line.pop();
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    manager.stderr(&connection, "invalid JSON-RPC message ignored");
                    continue;
                };
                if let Err(error) = manager.route(&connection, &acp, value).await {
                    manager.stderr(&connection, &format!("MCP routing failed: {error}"));
                }
            }
        });
    }

    fn spawn_waiter(
        &self,
        connection: Arc<McpConnection>,
        mut child: Child,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {},
                _ = &mut kill_rx => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
            manager
                .terminate(&connection, Error::new(-32000, "MCP server exited"))
                .await;
        });
    }

    async fn route(
        &self,
        connection: &Arc<McpConnection>,
        acp: &ConnectionTo<Agent>,
        value: Value,
    ) -> Result<(), Error> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::invalid_request().data("MCP message must be an object"))?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(Error::invalid_request().data("MCP message must use JSON-RPC 2.0"));
        }
        if let Some(method) = object.get("method").and_then(Value::as_str) {
            let params = parse_params(object.get("params"))?;
            if let Some(id) = object.get("id") {
                self.activity(
                    "server-to-agent",
                    connection,
                    method,
                    "request",
                    params.clone().map(Value::Object),
                    None,
                );
                let response = acp
                    .send_request(
                        MessageMcpRequest::new(connection.id.clone(), method.to_string())
                            .params(params),
                    )
                    .block_task()
                    .await;
                match response {
                    Ok(response) => {
                        let result: Value = serde_json::from_str(response.0.get())?;
                        write_json(
                            connection,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result,
                            }),
                        )
                        .await?;
                        self.activity(
                            "agent-to-server",
                            connection,
                            method,
                            "response",
                            None,
                            Some(result),
                        );
                    }
                    Err(error) => {
                        write_json(
                            connection,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": i32::from(error.code),
                                    "message": error.message,
                                    "data": error.data,
                                },
                            }),
                        )
                        .await?;
                    }
                }
            } else {
                self.activity(
                    "server-to-agent",
                    connection,
                    method,
                    "notification",
                    params.clone().map(Value::Object),
                    None,
                );
                acp.send_notification(
                    MessageMcpNotification::new(connection.id.clone(), method.to_string())
                        .params(params),
                )?;
            }
            return Ok(());
        }

        let Some(id) = object.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(pending) = connection.pending.lock().await.remove(id) else {
            self.stderr(connection, "unknown MCP response id ignored");
            return Ok(());
        };
        let result = if let Some(error) = object.get("error").and_then(Value::as_object) {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(-32603);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("MCP request failed");
            Err(Error::new(code, message).data(error.get("data").cloned()))
        } else if let Some(result) = object.get("result") {
            Ok(result.clone())
        } else {
            Err(Error::internal_error().data("MCP response has neither result nor error"))
        };
        let activity_result = result.as_ref().ok().cloned();
        self.activity(
            "server-to-agent",
            connection,
            &pending.method,
            "response",
            None,
            activity_result,
        );
        let _ = pending.sender.send(result);
        Ok(())
    }

    async fn terminate(&self, connection: &Arc<McpConnection>, reason: Error) {
        let removed = self
            .connections
            .lock()
            .await
            .remove(&connection.id)
            .is_some();
        if !removed {
            return;
        }
        if let Some(kill) = connection.kill.lock().await.take() {
            let _ = kill.send(());
        }
        for (_, pending) in connection.pending.lock().await.drain() {
            let _ = pending.sender.send(Err(reason.clone()));
        }
        if connection.announced.load(Ordering::Acquire) {
            self.connection_event("disconnected", connection);
        }
    }

    fn connection_event(&self, action: &str, connection: &McpConnection) {
        let _ = self.events.send(
            json!({
                "type": "acp/mcp_connection",
                "action": action,
                "serverId": connection.provider.server_id,
                "connectionId": connection.id,
                "name": connection.provider.name,
            })
            .to_string(),
        );
    }

    fn activity(
        &self,
        direction: &str,
        connection: &McpConnection,
        method: &str,
        kind: &str,
        params: Option<Value>,
        result: Option<Value>,
    ) {
        let mut event = json!({
            "type": "acp/mcp_message",
            "direction": direction,
            "connectionId": connection.id,
            "method": method,
            "kind": kind,
        });
        if let Some(params) = params {
            event["params"] = params;
        }
        if let Some(result) = result {
            event["result"] = result;
        }
        let _ = self.events.send(event.to_string());
    }

    fn stderr(&self, connection: &McpConnection, message: &str) {
        let _ = self.events.send(
            json!({
                "type": "bridge/stderr",
                "chunk": format!("[MCP {}] {message}\n", connection.provider.name),
            })
            .to_string(),
        );
    }
}

async fn read_bounded_line<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    maximum: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content_bytes = newline.unwrap_or(available.len());
        if line.len().saturating_add(content_bytes) > maximum {
            reader.consume(consumed);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP server output exceeds {maximum} buffered bytes"),
            ));
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(line.len());
        }
    }
}

async fn write_json(connection: &McpConnection, value: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Error::invalid_params().data("MCP message exceeds the size limit"));
    }
    let mut stdin = connection.stdin.lock().await;
    stdin
        .write_all(&bytes)
        .await
        .map_err(Error::into_internal_error)?;
    stdin.flush().await.map_err(Error::into_internal_error)
}

fn validate_message(method: &str, params: Option<&Map<String, Value>>) -> Result<(), Error> {
    if method.is_empty() || method.len() > 1_024 {
        return Err(
            Error::invalid_params().data("MCP method must contain between 1 and 1024 characters")
        );
    }
    if serde_json::to_vec(&params).map_err(Error::from)?.len() > MAX_MESSAGE_BYTES {
        return Err(Error::invalid_params().data("MCP params exceed the size limit"));
    }
    Ok(())
}

fn parse_params(value: Option<&Value>) -> Result<Option<Map<String, Value>>, Error> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(params)) => Ok(Some(params.clone())),
        Some(_) => Err(Error::invalid_params().data("MCP params must be an object or null")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_method_params_and_message_size() {
        assert!(validate_message("tools/list", None).is_ok());
        assert!(validate_message("", None).is_err());
        assert!(validate_message(&"x".repeat(1_025), None).is_err());

        let mut oversized = Map::new();
        oversized.insert("padding".to_string(), json!("x".repeat(MAX_MESSAGE_BYTES)));
        assert!(validate_message("tools/call", Some(&oversized)).is_err());

        let object = json!({ "name": "fixture" });
        assert_eq!(
            parse_params(Some(&object)).unwrap().unwrap()["name"],
            "fixture"
        );
        assert!(parse_params(None).unwrap().is_none());
        assert!(parse_params(Some(&Value::Null)).unwrap().is_none());
        assert!(parse_params(Some(&json!(["not", "an", "object"]))).is_err());
    }

    #[tokio::test]
    async fn isolates_unknown_connections_and_closes_empty_manager() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let manager = McpManager::new(std::env::temp_dir(), Vec::new(), events);
        let error = match manager.require("unknown").await {
            Ok(_) => panic!("unknown MCP connection unexpectedly resolved"),
            Err(error) => error,
        };
        assert_eq!(i32::from(error.code), -32_002);
        assert!(
            error
                .data
                .as_ref()
                .is_some_and(|data| data.to_string().contains("unknown MCP connection"))
        );
        manager.close_all().await;
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn bounds_incomplete_mcp_output_before_allocating_an_unbounded_line() {
        let mut reader = BufReader::new(&b"12345\nnext\n"[..]);
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 5).await.unwrap(),
            6
        );
        assert_eq!(line, b"12345\n");
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 5).await.unwrap(),
            5
        );
        assert_eq!(line, b"next\n");

        let mut oversized = BufReader::new(&b"123456"[..]);
        assert!(
            read_bounded_line(&mut oversized, &mut line, 5)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds 5 buffered bytes")
        );
    }
}
