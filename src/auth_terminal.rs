use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::AuthMethodTerminal;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::json;
#[cfg(test)]
use tokio::sync::mpsc;

use crate::event_queue::EventSender;

const MAX_AUTH_ARGUMENTS: usize = 256;
const MAX_AUTH_ARGUMENT_LENGTH: usize = 16_384;
const MAX_AUTH_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_AUTH_ENVIRONMENT_NAME_LENGTH: usize = 256;
const MAX_AUTH_ENVIRONMENT_VALUE_LENGTH: usize = 65_536;
const MAX_AUTH_ENVIRONMENT_BYTES: usize = 1_000_000;
const MAX_AUTH_TERMINAL_OUTPUT_BYTES: usize = 4_000_000;
const MAX_AUTH_TERMINAL_EVENT_BYTES: usize = 32_768;

#[derive(Clone)]
pub struct AuthTerminalManager {
    active: Arc<Mutex<Option<Arc<ActiveAuthTerminal>>>>,
    events: EventSender,
}

struct AuthTerminalCleanup<'a>(&'a AuthTerminalManager);

impl Drop for AuthTerminalCleanup<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct ActiveAuthTerminal {
    request_id: String,
    method_id: String,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output_bytes: AtomicUsize,
    cancelled: AtomicBool,
    settled: AtomicBool,
    failure: Mutex<Option<String>>,
}

impl AuthTerminalManager {
    pub fn new<E>(events: E) -> Self
    where
        E: Into<EventSender>,
    {
        Self {
            active: Arc::new(Mutex::new(None)),
            events: events.into(),
        }
    }

    pub fn start(
        &self,
        request_id: String,
        method: AuthMethodTerminal,
        agent_command: &[String],
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
    ) -> Result<(), Error> {
        validate_method(&method)?;
        validate_size(cols, rows)?;
        if self.active.lock().map_err(lock_error)?.is_some() {
            return Err(Error::invalid_request()
                .data("an Agent terminal authentication is already running"));
        }
        let program = agent_command
            .first()
            .ok_or_else(|| Error::invalid_request().data("Agent command is empty"))?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(internal_error)?;
        let reader = pair.master.try_clone_reader().map_err(internal_error)?;
        let writer = pair.master.take_writer().map_err(internal_error)?;
        let mut command = CommandBuilder::new(program);
        command.args(agent_command.iter().skip(1));
        command.args(&method.args);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        for (name, value) in &method.env {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command).map_err(internal_error)?;
        let active = Arc::new(ActiveAuthTerminal {
            request_id: request_id.clone(),
            method_id: method.id.0.to_string(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(child.clone_killer()),
            output_bytes: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        *self.active.lock().map_err(lock_error)? = Some(active.clone());
        // Publish the lifecycle boundary before either blocking worker can
        // publish output or completion, even if the process exits immediately.
        self.send(json!({
            "type": "bridge/auth_terminal_started",
            "requestId": request_id,
            "methodId": method.id,
        }));
        self.spawn_reader(active.clone(), reader);
        self.spawn_waiter(active, child);
        Ok(())
    }

    pub fn write(&self, request_id: &str, data: &str) -> Result<(), Error> {
        if data.len() > MAX_AUTH_TERMINAL_EVENT_BYTES {
            return Err(Error::invalid_params().data("terminal authentication input is too large"));
        }
        let active = self.require_active(request_id)?;
        let mut writer = active.writer.lock().map_err(lock_error)?;
        writer
            .write_all(data.as_bytes())
            .map_err(Error::into_internal_error)?;
        writer.flush().map_err(Error::into_internal_error)
    }

    pub fn resize(&self, request_id: &str, cols: u16, rows: u16) -> Result<(), Error> {
        validate_size(cols, rows)?;
        let active = self.require_active(request_id)?;
        active
            .master
            .lock()
            .map_err(lock_error)?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(internal_error)
    }

    pub fn cancel(&self, request_id: &str) -> Result<(), Error> {
        let active = self.require_active(request_id)?;
        active.cancelled.store(true, Ordering::Release);
        active
            .killer
            .lock()
            .map_err(lock_error)?
            .kill()
            .map_err(Error::into_internal_error)
    }

    pub fn close(&self) {
        let active = self.active.lock().ok().and_then(|active| active.clone());
        if let Some(active) = active {
            active.cancelled.store(true, Ordering::Release);
            if let Ok(mut killer) = active.killer.lock() {
                let _ = killer.kill();
            }
        }
    }

    /// Keep this guard in the owning connection scope, not in reader/waiter clones.
    #[must_use]
    pub fn close_on_drop(&self) -> impl Drop + '_ {
        AuthTerminalCleanup(self)
    }

    fn require_active(&self, request_id: &str) -> Result<Arc<ActiveAuthTerminal>, Error> {
        let active = self
            .active
            .lock()
            .map_err(lock_error)?
            .clone()
            .ok_or_else(|| {
                Error::invalid_request()
                    .data("Agent terminal authentication request is no longer active")
            })?;
        if active.request_id != request_id || active.settled.load(Ordering::Acquire) {
            return Err(Error::invalid_request()
                .data("Agent terminal authentication request is no longer active"));
        }
        Ok(active)
    }

    fn spawn_reader(&self, active: Arc<ActiveAuthTerminal>, mut reader: Box<dyn Read + Send>) {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut buffer = [0_u8; MAX_AUTH_TERMINAL_EVENT_BYTES];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let total = active.output_bytes.fetch_add(count, Ordering::AcqRel) + count;
                if total > MAX_AUTH_TERMINAL_OUTPUT_BYTES {
                    if let Ok(mut failure) = active.failure.lock() {
                        *failure = Some(format!(
                            "terminal authentication output exceeded {MAX_AUTH_TERMINAL_OUTPUT_BYTES} bytes"
                        ));
                    }
                    if let Ok(mut killer) = active.killer.lock() {
                        let _ = killer.kill();
                    }
                    break;
                }
                manager.send(json!({
                    "type": "bridge/auth_terminal_output",
                    "requestId": active.request_id,
                    "data": String::from_utf8_lossy(&buffer[..count]),
                }));
            }
        });
    }

    fn spawn_waiter(
        &self,
        active: Arc<ActiveAuthTerminal>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = child.wait();
            manager.finish(active, result);
        });
    }

    fn finish(
        &self,
        active: Arc<ActiveAuthTerminal>,
        result: std::io::Result<portable_pty::ExitStatus>,
    ) {
        if active.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut current) = self.active.lock()
            && current
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &active))
        {
            *current = None;
        }
        let (exit_code, signal, wait_error) = match result {
            Ok(status) => (
                Some(status.exit_code()),
                status.signal().map(ToOwned::to_owned),
                None,
            ),
            Err(error) => (None, None, Some(error.to_string())),
        };
        let failure = active
            .failure
            .lock()
            .ok()
            .and_then(|failure| failure.clone())
            .or(wait_error);
        let cancelled = active.cancelled.load(Ordering::Acquire);
        let status = if cancelled {
            "cancelled"
        } else if exit_code == Some(0) && failure.is_none() {
            "succeeded"
        } else {
            "failed"
        };
        let mut event = json!({
            "type": "bridge/auth_terminal_exited",
            "requestId": active.request_id,
            "methodId": active.method_id,
            "status": status,
            "exitCode": exit_code,
        });
        if let Some(signal) = signal {
            event["message"] = json!(format!(
                "terminal authentication was terminated by {signal}"
            ));
        }
        if let Some(failure) = failure {
            event["message"] = json!(failure);
        } else if status == "failed" {
            event["message"] = json!(match exit_code {
                Some(code) => format!("terminal authentication exited with status {code}"),
                None => "terminal authentication ended without an exit status".to_string(),
            });
        }
        self.send(event);
    }

    fn send(&self, event: serde_json::Value) {
        let _ = self.events.send(event.to_string());
    }
}

pub(crate) fn validate_method(method: &AuthMethodTerminal) -> Result<(), Error> {
    if method.args.len() > MAX_AUTH_ARGUMENTS
        || method.args.iter().any(|argument| {
            argument.encode_utf16().count() > MAX_AUTH_ARGUMENT_LENGTH || argument.contains('\0')
        })
    {
        return Err(Error::invalid_params().data("terminal authentication arguments are invalid"));
    }
    if method.env.len() > MAX_AUTH_ENVIRONMENT_ENTRIES {
        return Err(Error::invalid_params()
            .data("terminal authentication environment has too many entries"));
    }
    let mut bytes = 0_usize;
    for (name, value) in &method.env {
        let valid_name = !name.is_empty()
            && name.len() <= MAX_AUTH_ENVIRONMENT_NAME_LENGTH
            && name.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (index > 0 && character.is_ascii_digit())
            });
        if !valid_name
            || value.encode_utf16().count() > MAX_AUTH_ENVIRONMENT_VALUE_LENGTH
            || value.contains('\0')
        {
            return Err(
                Error::invalid_params().data("terminal authentication environment is invalid")
            );
        }
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
    }
    if bytes > MAX_AUTH_ENVIRONMENT_BYTES {
        return Err(
            Error::invalid_params().data("terminal authentication environment is too large")
        );
    }
    Ok(())
}

fn validate_size(cols: u16, rows: u16) -> Result<(), Error> {
    if cols == 0 || rows == 0 {
        return Err(Error::invalid_params().data("terminal dimensions must be positive"));
    }
    Ok(())
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> Error {
    Error::internal_error().data(error.to_string())
}

fn internal_error(error: anyhow::Error) -> Error {
    Error::internal_error().data(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;

    fn method(value: Value) -> AuthMethodTerminal {
        serde_json::from_value(value).unwrap()
    }

    async fn next_event(receiver: &mut mpsc::UnboundedReceiver<String>) -> Value {
        let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("timed out waiting for auth terminal event")
            .expect("auth terminal event channel closed");
        serde_json::from_str(&event).unwrap()
    }

    async fn event_of_type(
        receiver: &mut mpsc::UnboundedReceiver<String>,
        event_type: &str,
    ) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = next_event(receiver).await;
                if event["type"] == event_type {
                    return event;
                }
            }
        })
        .await
        .expect("timed out waiting for the expected auth terminal event")
    }

    async fn output_until(
        receiver: &mut mpsc::UnboundedReceiver<String>,
        expected: &str,
    ) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                let event = next_event(receiver).await;
                assert_ne!(
                    event["type"], "bridge/auth_terminal_exited",
                    "auth terminal exited before {expected:?}; output={output:?}; event={event}"
                );
                if event["type"] == "bridge/auth_terminal_output" {
                    output.push_str(event["data"].as_str().unwrap());
                    if output.contains(expected) {
                        return output;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for complete auth terminal output")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn appends_agent_arguments_and_environment_to_base_invocation() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let manager = AuthTerminalManager::new(events);
        let _cleanup = manager.close_on_drop();
        let login = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal",
            "args": ["terminal-arg"],
            "env": { "ATTYD_AUTH_TEST": "method-env" }
        }));
        let script = "printf '%s|%s:%s> ' \"$0\" \"$1\" \"$ATTYD_AUTH_TEST\"; read answer; test \"$answer\" = ok";
        manager
            .start(
                "auth".to_string(),
                login,
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    script.to_string(),
                    "base-arg".to_string(),
                ],
                Path::new("/tmp"),
                80,
                24,
            )
            .unwrap();

        let started = next_event(&mut receiver).await;
        assert_eq!(started["type"], "bridge/auth_terminal_started");
        assert_eq!(started["requestId"], "auth");
        output_until(&mut receiver, "base-arg|terminal-arg:method-env>").await;
        manager.resize("auth", 100, 30).unwrap();
        manager.write("auth", "ok\n").unwrap();
        let exited = event_of_type(&mut receiver, "bridge/auth_terminal_exited").await;
        assert_eq!(exited["status"], "succeeded");
        assert_eq!(exited["exitCode"], 0);
        assert!(manager.write("auth", "late").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_cancellation_and_rejects_stale_input() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        let manager = AuthTerminalManager::new(events);
        let _cleanup = manager.close_on_drop();
        let login = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal"
        }));
        manager
            .start(
                "cancel-auth".to_string(),
                login,
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "while :; do sleep 1; done".to_string(),
                ],
                Path::new("/tmp"),
                80,
                24,
            )
            .unwrap();
        assert_eq!(
            next_event(&mut receiver).await["type"],
            "bridge/auth_terminal_started"
        );
        manager.cancel("cancel-auth").unwrap();
        let exited = event_of_type(&mut receiver, "bridge/auth_terminal_exited").await;
        assert_eq!(exited["status"], "cancelled");
        assert!(manager.write("cancel-auth", "late").is_err());
    }

    #[tokio::test]
    async fn collects_authentication_output_across_pty_chunks() {
        let (events, mut receiver) = mpsc::unbounded_channel();
        for data in ["base-", "arg|terminal-arg:", "method-env>"] {
            events
                .send(
                    json!({
                        "type": "bridge/auth_terminal_output",
                        "data": data,
                    })
                    .to_string(),
                )
                .unwrap();
        }
        drop(events);
        assert_eq!(
            output_until(&mut receiver, "base-arg|terminal-arg:method-env>").await,
            "base-arg|terminal-arg:method-env>"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publishes_started_before_immediate_output_and_exit() {
        for _ in 0..32 {
            let (events, mut receiver) = mpsc::unbounded_channel();
            let manager = AuthTerminalManager::new(events);
            let _cleanup = manager.close_on_drop();
            manager
                .start(
                    "immediate-auth".into(),
                    method(json!({ "id": "login", "name": "Login", "type": "terminal" })),
                    &["/bin/sh".into(), "-c".into(), "printf ready".into()],
                    Path::new("/tmp"),
                    80,
                    24,
                )
                .unwrap();
            let first = next_event(&mut receiver).await;
            assert_eq!(first["type"], "bridge/auth_terminal_started");
            assert_eq!(first["requestId"], "immediate-auth");
            assert_eq!(
                event_of_type(&mut receiver, "bridge/auth_terminal_exited").await["status"],
                "succeeded"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_unwind_closes_authentication_and_allows_runtime_shutdown() {
        const CHILD_ENV: &str = "ATTYD_TEST_AUTH_OWNER_UNWIND";
        if std::env::var_os(CHILD_ENV).is_none() {
            // A broken cleanup must fail this regression, not hang the CI runtime.
            assert_cmd::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "auth_terminal::tests::owner_unwind_closes_authentication_and_allows_runtime_shutdown", "--nocapture"])
                .env(CHILD_ENV, "1")
                .timeout(Duration::from_secs(15))
                .assert()
                .success();
            return;
        }
        use futures::FutureExt;
        use std::panic::AssertUnwindSafe;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (events, mut receiver) = mpsc::unbounded_channel();
            let manager = AuthTerminalManager::new(events);
            let result = AssertUnwindSafe(async {
                let _cleanup = manager.close_on_drop();
                manager
                    .start(
                        "unwind-auth".into(),
                        method(json!({ "id": "login", "name": "Login", "type": "terminal" })),
                        &[
                            "/bin/sh".into(),
                            "-c".into(),
                            "printf ready; read answer".into(),
                        ],
                        Path::new("/tmp"),
                        80,
                        24,
                    )
                    .unwrap();
                assert_eq!(
                    next_event(&mut receiver).await["type"],
                    "bridge/auth_terminal_started"
                );
                output_until(&mut receiver, "ready").await;
                panic!("simulate an assertion failure before terminal input");
            })
            .catch_unwind()
            .await;
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"simulate an assertion failure before terminal input")
            );
            let exited = event_of_type(&mut receiver, "bridge/auth_terminal_exited").await;
            assert_eq!(exited["status"], "cancelled");
            assert!(manager.write("unwind-auth", "late").is_err());
        });
        drop(runtime);
    }

    #[test]
    fn bounds_agent_arguments_environment_and_terminal_size() {
        let invalid_name = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal",
            "env": { "INVALID-NAME": "value" }
        }));
        assert!(validate_method(&invalid_name).is_err());

        let invalid_argument = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal",
            "args": ["x".repeat(MAX_AUTH_ARGUMENT_LENGTH + 1)]
        }));
        assert!(validate_method(&invalid_argument).is_err());

        let duplicate_free_valid = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal",
            "args": ["--login"],
            "env": { "VALID_NAME_2": "value" }
        }));
        assert!(validate_method(&duplicate_free_valid).is_ok());
        assert!(validate_size(0, 24).is_err());
        assert!(validate_size(80, 0).is_err());
        assert!(validate_size(80, 24).is_ok());
    }
}
