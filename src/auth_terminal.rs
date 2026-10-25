use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::AuthMethodTerminal;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::bridge_event::{BridgeEvent, EventSender};

const AUTH_TERMINAL_READ_CHUNK_BYTES: usize = 32_768;

/// What a login terminal reports to its owner.
#[derive(Debug, Clone)]
pub(crate) enum AuthTerminalEvent {
    Output(String),
    Exited {
        exit_code: Option<i64>,
        signal: Option<String>,
        cancelled: bool,
        failure: Option<String>,
    },
}

/// Called from the terminal's blocking reader and waiter threads.
pub(crate) type AuthTerminalObserver = Arc<dyn Fn(AuthTerminalEvent) + Send + Sync>;

/// Writes one input to the login's stdin, in queue order; the sender learns
/// how the write ended.
type QueuedInput = (String, tokio::sync::oneshot::Sender<Result<(), Error>>);

type TerminalWriter = Arc<Mutex<Box<dyn Write + Send>>>;

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
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// The input queue of the terminal's own blocking writer.
    input: std::sync::mpsc::Sender<QueuedInput>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    cancelled: AtomicBool,
    settled: AtomicBool,
    failure: Mutex<Option<String>>,
    observer: Option<AuthTerminalObserver>,
}

impl AuthTerminalManager {
    /// `events` learns of a successful login, which restarts the connection.
    pub fn new(events: EventSender) -> Self {
        Self {
            active: Arc::new(Mutex::new(None)),
            events,
        }
    }

    /// Starts a login whose output and exit `observer` receives.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_observed(
        &self,
        request_id: String,
        method: AuthMethodTerminal,
        agent_command: &[String],
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
        observer: Option<AuthTerminalObserver>,
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
        let writer: TerminalWriter = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(internal_error)?,
        ));
        let (input, queued) = std::sync::mpsc::channel();
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
            request_id,
            master: Mutex::new(pair.master),
            input,
            killer: Mutex::new(child.clone_killer()),
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            failure: Mutex::new(None),
            observer,
        });
        *self.active.lock().map_err(lock_error)? = Some(active.clone());
        let (drained, reader_done) = std::sync::mpsc::channel();
        self.spawn_reader(active.clone(), reader, drained);
        spawn_writer(writer, queued);
        self.spawn_waiter(active, child, reader_done);
        Ok(())
    }

    /// Queues `data` for the login's stdin behind the input queued before it.
    /// The write runs on the terminal's own blocking writer, so a stdin that
    /// applies backpressure holds no caller's lock or async worker; the
    /// returned receiver resolves when the bytes are written.
    pub(crate) fn queue_input(
        &self,
        request_id: &str,
        data: String,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), Error>>, Error> {
        let active = self.require_active(request_id)?;
        let (written, done) = tokio::sync::oneshot::channel();
        active.input.send((data, written)).map_err(|_| {
            Error::invalid_request()
                .data("Agent terminal authentication request is no longer active")
        })?;
        Ok(done)
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

    fn spawn_reader(
        &self,
        active: Arc<ActiveAuthTerminal>,
        mut reader: Box<dyn Read + Send>,
        drained: std::sync::mpsc::Sender<()>,
    ) {
        tokio::task::spawn_blocking(move || {
            // Dropped when the output ends, so the exit follows the output.
            let _drained = drained;
            let mut buffer = [0_u8; AUTH_TERMINAL_READ_CHUNK_BYTES];
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                if let Some(observer) = &active.observer {
                    let data = String::from_utf8_lossy(&buffer[..count]).into_owned();
                    observer(AuthTerminalEvent::Output(data));
                }
            }
        });
    }

    fn spawn_waiter(
        &self,
        active: Arc<ActiveAuthTerminal>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
        reader_done: std::sync::mpsc::Receiver<()>,
    ) {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = child.wait();
            // Report the exit after the output the process wrote; a
            // descendant that keeps the terminal open does not delay it long.
            let _ = reader_done.recv_timeout(std::time::Duration::from_secs(1));
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
            // A signalled child has no exit status of its own.
            Ok(status) => match status.signal() {
                Some(signal) => (None, Some(signal.to_owned()), None),
                None => (Some(status.exit_code()), None, None),
            },
            Err(error) => (None, None, Some(error.to_string())),
        };
        let failure = active
            .failure
            .lock()
            .ok()
            .and_then(|failure| failure.clone())
            .or(wait_error);
        let cancelled = active.cancelled.load(Ordering::Acquire);
        let succeeded = !cancelled && exit_code == Some(0) && failure.is_none();
        if let Some(observer) = &active.observer {
            observer(AuthTerminalEvent::Exited {
                exit_code: exit_code.map(i64::from),
                signal,
                cancelled,
                failure,
            });
        }
        if succeeded {
            let _ = self.events.send(BridgeEvent::TerminalLoginSucceeded);
        }
    }
}

pub(crate) fn validate_method(method: &AuthMethodTerminal) -> Result<(), Error> {
    if method.args.iter().any(|argument| argument.contains('\0')) {
        return Err(Error::invalid_params().data("terminal authentication arguments are invalid"));
    }
    for (name, value) in &method.env {
        let valid_name = !name.is_empty()
            && name.chars().enumerate().all(|(index, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (index > 0 && character.is_ascii_digit())
            });
        if !valid_name || value.contains('\0') {
            return Err(
                Error::invalid_params().data("terminal authentication environment is invalid")
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_size(cols: u16, rows: u16) -> Result<(), Error> {
    if cols == 0 || rows == 0 {
        return Err(Error::invalid_params().data("terminal dimensions must be positive"));
    }
    Ok(())
}

/// The terminal's blocking writer: it writes the queued input in order and
/// ends when the terminal, which holds the queue, is gone.
fn spawn_writer(writer: TerminalWriter, queued: std::sync::mpsc::Receiver<QueuedInput>) {
    tokio::task::spawn_blocking(move || {
        while let Ok((data, written)) = queued.recv() {
            let result = writer.lock().map_err(lock_error).and_then(|mut writer| {
                writer
                    .write_all(data.as_bytes())
                    .and_then(|()| writer.flush())
                    .map_err(Error::into_internal_error)
            });
            let _ = written.send(result);
        }
    });
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
    const FORMER_AUTH_ARGUMENT_LENGTH: usize = 16_384;

    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn method(value: Value) -> AuthMethodTerminal {
        serde_json::from_value(value).unwrap()
    }

    /// A manager whose host signals `signals` receives, and an observer whose
    /// output and exit `observed` receives.
    fn observed() -> (
        AuthTerminalManager,
        mpsc::UnboundedReceiver<BridgeEvent>,
        Option<AuthTerminalObserver>,
        mpsc::UnboundedReceiver<AuthTerminalEvent>,
    ) {
        let (events, signals) = mpsc::unbounded_channel();
        let (observer, observed) = mpsc::unbounded_channel();
        let observer: AuthTerminalObserver = Arc::new(move |event| {
            let _ = observer.send(event);
        });
        (
            AuthTerminalManager::new(events),
            signals,
            Some(observer),
            observed,
        )
    }

    async fn next_event(
        observed: &mut mpsc::UnboundedReceiver<AuthTerminalEvent>,
    ) -> AuthTerminalEvent {
        tokio::time::timeout(Duration::from_secs(5), observed.recv())
            .await
            .expect("timed out waiting for auth terminal event")
            .expect("auth terminal observer dropped")
    }

    /// The exit, after any remaining output.
    async fn exited(
        observed: &mut mpsc::UnboundedReceiver<AuthTerminalEvent>,
    ) -> (Option<i64>, Option<String>, bool, Option<String>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AuthTerminalEvent::Exited {
                    exit_code,
                    signal,
                    cancelled,
                    failure,
                } = next_event(observed).await
                {
                    return (exit_code, signal, cancelled, failure);
                }
            }
        })
        .await
        .expect("timed out waiting for the auth terminal exit")
    }

    async fn output_until(
        observed: &mut mpsc::UnboundedReceiver<AuthTerminalEvent>,
        expected: &str,
    ) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = String::new();
            loop {
                match next_event(observed).await {
                    AuthTerminalEvent::Output(data) => {
                        output.push_str(&data);
                        if output.contains(expected) {
                            return output;
                        }
                    }
                    exit => panic!(
                        "auth terminal exited before {expected:?}; output={output:?}; {exit:?}"
                    ),
                }
            }
        })
        .await
        .expect("timed out waiting for complete auth terminal output")
    }

    /// The login's workers dropped the observer, after any host signal.
    async fn settled(observed: &mut mpsc::UnboundedReceiver<AuthTerminalEvent>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while observed.recv().await.is_some() {}
        })
        .await
        .expect("the login's workers did not finish");
    }

    async fn next_signal(signals: &mut mpsc::UnboundedReceiver<BridgeEvent>) -> BridgeEvent {
        tokio::time::timeout(Duration::from_secs(5), signals.recv())
            .await
            .expect("timed out waiting for a host signal")
            .expect("the host signal channel closed")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn appends_agent_arguments_and_environment_to_base_invocation() {
        let (manager, mut signals, observer, mut observed) = observed();
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
            .start_observed(
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
                observer,
            )
            .unwrap();

        output_until(&mut observed, "base-arg|terminal-arg:method-env>").await;
        manager.resize("auth", 100, 30).unwrap();
        manager.queue_input("auth", "ok\n".into()).unwrap().await.unwrap().unwrap();
        assert_eq!(exited(&mut observed).await, (Some(0), None, false, None));
        assert_eq!(
            next_signal(&mut signals).await,
            BridgeEvent::TerminalLoginSucceeded
        );
        assert!(manager.queue_input("auth", "late".into()).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_cancellation_and_rejects_stale_input() {
        let (manager, mut signals, observer, mut observed) = observed();
        let _cleanup = manager.close_on_drop();
        let login = method(json!({
            "id": "terminal-login",
            "name": "Terminal login",
            "type": "terminal"
        }));
        manager
            .start_observed(
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
                observer,
            )
            .unwrap();
        manager.cancel("cancel-auth").unwrap();
        let (_, _, cancelled, _) = exited(&mut observed).await;
        assert!(cancelled);
        assert!(manager.queue_input("cancel-auth", "late".into()).is_err());
        settled(&mut observed).await;
        assert!(
            signals.try_recv().is_err(),
            "a cancelled login restarts nothing"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_a_signalled_login_without_an_exit_code() {
        let (manager, mut signals, observer, mut observed) = observed();
        let _cleanup = manager.close_on_drop();
        manager
            .start_observed(
                "signal-auth".into(),
                method(json!({ "id": "login", "name": "Login", "type": "terminal" })),
                &["/bin/sh".into(), "-c".into(), "kill -TERM $$".into()],
                Path::new("/tmp"),
                80,
                24,
                observer,
            )
            .unwrap();
        let (exit_code, signal, cancelled, failure) = exited(&mut observed).await;
        assert_eq!(exit_code, None);
        assert!(signal.is_some());
        assert!(!cancelled);
        assert_eq!(failure, None);
        settled(&mut observed).await;
        assert!(
            signals.try_recv().is_err(),
            "a signalled login restarts nothing"
        );
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
            let (manager, _signals, observer, mut observed) = observed();
            let result = AssertUnwindSafe(async {
                let _cleanup = manager.close_on_drop();
                manager
                    .start_observed(
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
                        observer,
                    )
                    .unwrap();
                output_until(&mut observed, "ready").await;
                panic!("simulate an assertion failure before terminal input");
            })
            .catch_unwind()
            .await;
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"simulate an assertion failure before terminal input")
            );
            let (_, _, cancelled, _) = exited(&mut observed).await;
            assert!(cancelled);
            assert!(manager.queue_input("unwind-auth", "late".into()).is_err());
        });
        drop(runtime);
    }

    #[test]
    fn validates_agent_environment_and_terminal_size() {
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
            "args": ["x".repeat(FORMER_AUTH_ARGUMENT_LENGTH + 1)]
        }));
        assert!(validate_method(&invalid_argument).is_ok());

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
