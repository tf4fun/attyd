use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_client_protocol::Error;
use agent_client_protocol::RequestCancellation;
use agent_client_protocol::schema::v1::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::event_queue::EventSender;
use crate::filesystem::WorkspaceFileSystem;

#[derive(Debug, Clone)]
pub(crate) struct TerminalSnapshot {
    pub incarnation: u64,
    pub value: serde_json::Value,
}

pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 1_000_000;
const DEFAULT_TERMINAL_OUTPUT_BYTES: usize = 200_000;
const MAX_TERMINALS: usize = 32;
const MAX_TERMINAL_COMMAND_LENGTH: usize = 16_384;
const MAX_TERMINAL_ARGS: usize = 4_096;
const MAX_TERMINAL_ARG_LENGTH: usize = 65_536;
const MAX_TERMINAL_ENV: usize = 256;
const MAX_TERMINAL_ENV_NAME_LENGTH: usize = 256;
const MAX_TERMINAL_ENV_VALUE_LENGTH: usize = 65_536;
const TERMINAL_OUTPUT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub struct TerminalManager {
    filesystem: Arc<WorkspaceFileSystem>,
    terminals: Arc<Mutex<HashMap<String, Arc<Terminal>>>>,
    events: EventSender,
    snapshots: Option<mpsc::Sender<TerminalSnapshot>>,
}

struct Terminal {
    id: String,
    session_id: String,
    incarnation: u64,
    output_limit: usize,
    state: Mutex<TerminalState>,
    changed: Notify,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    readers_remaining: AtomicUsize,
    reader_tasks: Mutex<Vec<AbortHandle>>,
}

struct TerminalProcess {
    child: Child,
    #[cfg(unix)]
    process_group: Option<rustix::process::Pid>,
}

impl TerminalProcess {
    fn terminate(&mut self) {
        // Completion and a kill/release request can become ready together.
        // Once the command has exited, its background jobs no longer belong to
        // this terminal. Forget the PGID before it can be reused by another job.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            #[cfg(unix)]
            {
                self.process_group = None;
            }
            return;
        }
        #[cfg(unix)]
        if let Some(group) = self.process_group.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for TerminalProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct TerminalReader(Arc<Terminal>);

impl Drop for TerminalReader {
    fn drop(&mut self) {
        self.0.readers_remaining.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

#[derive(Default)]
struct TerminalState {
    output: Vec<u8>,
    pending_output: Vec<u8>,
    last_published_output: Vec<u8>,
    truncated: bool,
    exit_status: Option<TerminalExitStatus>,
    released: bool,
}

impl TerminalManager {
    pub fn new_with_snapshots<E>(
        filesystem: Arc<WorkspaceFileSystem>,
        events: E,
        snapshots: Option<mpsc::Sender<TerminalSnapshot>>,
    ) -> Self
    where
        E: Into<EventSender>,
    {
        Self {
            filesystem,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            events: events.into(),
            snapshots,
        }
    }

    pub(crate) fn in_workspace(&self, filesystem: WorkspaceFileSystem) -> Self {
        Self {
            filesystem: Arc::new(filesystem),
            ..self.clone()
        }
    }

    pub(crate) async fn create_for_incarnation(
        &self,
        request: CreateTerminalRequest,
        incarnation: u64,
    ) -> Result<CreateTerminalResponse, Error> {
        validate_create_request(&request)?;
        if self.terminals.lock().await.len() >= MAX_TERMINALS {
            return Err(
                Error::invalid_request().data(format!("terminal limit reached ({MAX_TERMINALS})"))
            );
        }
        let cwd = self
            .filesystem
            .checked_directory(request.cwd.as_deref())
            .await?;
        let output_limit = request
            .output_byte_limit
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTES)
            .min(MAX_TERMINAL_OUTPUT_BYTES);

        let mut process = spawn_command(&request, &cwd).map_err(terminal_spawn_error)?;
        let stdout = process.child.stdout.take();
        let stderr = process.child.stderr.take();
        let reader_count = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
        let id = Uuid::new_v4().to_string();
        let terminal = Arc::new(Terminal {
            id: id.clone(),
            session_id: request.session_id.0.to_string(),
            incarnation,
            output_limit,
            state: Mutex::new(TerminalState::default()),
            changed: Notify::new(),
            kill: Mutex::new(None),
            readers_remaining: AtomicUsize::new(reader_count),
            reader_tasks: Mutex::new(Vec::with_capacity(reader_count)),
        });
        let (kill_tx, kill_rx) = oneshot::channel();
        *terminal.kill.lock().await = Some(kill_tx);
        self.terminals
            .lock()
            .await
            .insert(id.clone(), terminal.clone());

        let mut reader_tasks = Vec::with_capacity(reader_count);
        if let Some(stdout) = stdout {
            reader_tasks.push(self.spawn_reader(terminal.clone(), stdout));
        }
        if let Some(stderr) = stderr {
            reader_tasks.push(self.spawn_reader(terminal.clone(), stderr));
        }
        *terminal.reader_tasks.lock().await = reader_tasks;
        self.spawn_waiter(terminal.clone(), process, kill_rx);
        self.emit_snapshot(&terminal).await;
        Ok(CreateTerminalResponse::new(id))
    }

    pub async fn output(
        &self,
        request: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, Error> {
        let terminal = self
            .require(&request.terminal_id.0, &request.session_id.0)
            .await?;
        let state = terminal.state.lock().await;
        let (output, truncated) = output_text(&state, terminal.output_limit);
        Ok(TerminalOutputResponse::new(output, truncated).exit_status(state.exit_status.clone()))
    }

    pub async fn wait_for_exit(
        &self,
        request: WaitForTerminalExitRequest,
        cancellation: RequestCancellation,
    ) -> Result<WaitForTerminalExitResponse, Error> {
        let terminal = self
            .require(&request.terminal_id.0, &request.session_id.0)
            .await?;
        cancellation
            .run_until_cancelled(async {
                loop {
                    let notified = terminal.changed.notified();
                    if let Some(status) = terminal.state.lock().await.exit_status.clone() {
                        return Ok(WaitForTerminalExitResponse::new(status));
                    }
                    notified.await;
                }
            })
            .await
    }

    pub async fn kill(&self, request: KillTerminalRequest) -> Result<KillTerminalResponse, Error> {
        let terminal = self
            .require(&request.terminal_id.0, &request.session_id.0)
            .await?;
        Self::signal_kill(&terminal).await;
        Ok(KillTerminalResponse::new())
    }

    pub async fn release(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, Error> {
        self.release_with_snapshot(request)
            .await
            .map(|(response, _)| response)
    }

    pub(crate) async fn release_with_snapshot(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<(ReleaseTerminalResponse, TerminalSnapshot), Error> {
        let terminal = self
            .require(&request.terminal_id.0, &request.session_id.0)
            .await?;
        Self::signal_kill(&terminal).await;
        Self::abort_readers(&terminal).await;
        terminal.state.lock().await.released = true;
        self.emit_snapshot(&terminal).await;
        let snapshot = {
            let state = terminal.state.lock().await;
            let (output, truncated) = output_text(&state, terminal.output_limit);
            TerminalSnapshot {
                incarnation: terminal.incarnation,
                value: json!({
                    "sessionId": terminal.session_id,
                    "terminalId": terminal.id,
                    "output": output,
                    "truncated": truncated,
                    "exitStatus": state.exit_status,
                    "released": true,
                }),
            }
        };
        self.terminals.lock().await.remove(terminal.id.as_str());
        terminal.changed.notify_waiters();
        Ok((ReleaseTerminalResponse::new(), snapshot))
    }

    pub async fn release_session(&self, session_id: &str) {
        let terminals = {
            let terminals = self.terminals.lock().await;
            terminals
                .values()
                .filter(|terminal| terminal.session_id == session_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        for terminal in terminals {
            Self::signal_kill(&terminal).await;
            Self::abort_readers(&terminal).await;
            terminal.state.lock().await.released = true;
            self.emit_snapshot(&terminal).await;
            self.terminals.lock().await.remove(terminal.id.as_str());
            terminal.changed.notify_waiters();
        }
    }

    pub async fn assert_reference(&self, terminal_id: &str, session_id: &str) -> Result<(), Error> {
        self.require(terminal_id, session_id).await.map(|_| ())
    }

    #[cfg(test)]
    pub(crate) async fn pause_references_for_test(&self) -> impl Drop + '_ {
        self.terminals.lock().await
    }

    pub async fn close_all(&self) {
        let terminals = self
            .terminals
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for terminal in terminals {
            Self::signal_kill(&terminal).await;
            Self::abort_readers(&terminal).await;
            terminal.state.lock().await.released = true;
            self.emit_snapshot(&terminal).await;
        }
        self.terminals.lock().await.clear();
    }

    async fn require(&self, terminal_id: &str, session_id: &str) -> Result<Arc<Terminal>, Error> {
        let terminal = self
            .terminals
            .lock()
            .await
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| {
                Error::invalid_params().data(format!("unknown terminal: {terminal_id}"))
            })?;
        if terminal.session_id != session_id {
            return Err(Error::invalid_params().data(format!(
                "terminal {terminal_id} does not belong to session {session_id}"
            )));
        }
        Ok(terminal)
    }

    fn spawn_reader<R>(&self, terminal: Arc<Terminal>, mut reader: R) -> AbortHandle
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let manager = self.clone();
        // Construct before spawning so even an unpolled, aborted reader retires.
        let completion = TerminalReader(terminal.clone());
        tokio::spawn(async move {
            let _completion = completion;
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                let count = match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                {
                    let mut state = terminal.state.lock().await;
                    if state.released {
                        break;
                    }
                    state.output.extend_from_slice(&buffer[..count]);
                    state.pending_output.extend_from_slice(&buffer[..count]);
                    if state.output.len() > terminal.output_limit {
                        let overflow = state.output.len() - terminal.output_limit;
                        state.output.drain(..overflow);
                        while state.output.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
                            state.output.remove(0);
                        }
                        state.truncated = true;
                    }
                }
                manager.emit_snapshot(&terminal).await;
            }
        })
        .abort_handle()
    }

    fn spawn_waiter(
        &self,
        terminal: Arc<Terminal>,
        mut process: TerminalProcess,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = process.child.wait() => status,
                _ = &mut kill_rx => {
                    process.terminate();
                    process.child.wait().await
                }
            };
            // Drop clears the PGID of an exited command without signalling its
            // background jobs. Interrupted or failed waits still clean up a
            // running command; no PGID is retained during output draining.
            drop(process);
            let exit_status = match status {
                Ok(status) => normalize_exit_status(status),
                Err(error) => TerminalExitStatus::new().signal(error.to_string()),
            };
            if tokio::time::timeout(
                TERMINAL_OUTPUT_DRAIN_TIMEOUT,
                Self::wait_for_readers(&terminal),
            )
            .await
            .is_err()
            {
                // A background job can keep a pipe open after the command exits.
                // Bound final draining without reporting byte-limit truncation.
                Self::abort_readers(&terminal).await;
                Self::wait_for_readers(&terminal).await;
            }
            terminal.state.lock().await.exit_status = Some(exit_status);
            terminal.changed.notify_waiters();
            manager.emit_snapshot(&terminal).await;
        });
    }

    async fn wait_for_readers(terminal: &Terminal) {
        loop {
            let drained = terminal.changed.notified();
            if terminal.readers_remaining.load(Ordering::Acquire) == 0 {
                return;
            }
            drained.await;
        }
    }

    async fn signal_kill(terminal: &Arc<Terminal>) {
        if terminal.state.lock().await.exit_status.is_some() {
            return;
        }
        if let Some(kill) = terminal.kill.lock().await.take() {
            let _ = kill.send(());
        }
    }

    async fn abort_readers(terminal: &Arc<Terminal>) {
        for task in std::mem::take(&mut *terminal.reader_tasks.lock().await) {
            task.abort();
        }
    }

    async fn emit_snapshot(&self, terminal: &Terminal) {
        let (snapshot, internal_snapshot) = {
            let mut state = terminal.state.lock().await;
            let (output, append) = if !state.pending_output.is_empty() {
                (std::mem::take(&mut state.pending_output), true)
            } else if state.output.starts_with(&state.last_published_output) {
                (
                    state.output[state.last_published_output.len()..].to_vec(),
                    true,
                )
            } else {
                (state.output.clone(), false)
            };
            state.last_published_output = state.output.clone();
            let snapshot = json!({
                "sessionId": terminal.session_id,
                "terminalId": terminal.id,
                "output": String::from_utf8_lossy(&output),
                "outputBytes": BASE64_STANDARD.encode(&output),
                "outputAppend": append,
                "retainedBytes": state.output.len(),
                "truncated": state.truncated,
                "exitStatus": state.exit_status,
                "released": state.released,
            });

            let internal_snapshot = snapshot.clone();
            (snapshot, internal_snapshot)
        };
        let _ = self.events.send(
            json!({
                "type": "acp/terminal_state",
                "terminal": snapshot,
            })
            .to_string(),
        );
        if let Some(snapshots) = &self.snapshots {
            if snapshots
                .send(TerminalSnapshot {
                    incarnation: terminal.incarnation,
                    value: internal_snapshot,
                })
                .await
                .is_err()
            {
                self.events.cancel_generation();
            }
        }
    }
}

fn output_text(state: &TerminalState, limit: usize) -> (String, bool) {
    let mut bytes = state.output.as_slice();
    if state.exit_status.is_none() && !state.released {
        // A reader may stop between bytes of a character. Hold that suffix until
        // the next read instead of expanding it into a replacement character.
        let mut position = 0;
        while let Err(error) = std::str::from_utf8(&bytes[position..]) {
            position += error.valid_up_to();
            match error.error_len() {
                Some(length) => position += length,
                None => {
                    bytes = &bytes[..position];
                    break;
                }
            }
        }
    }
    let output = String::from_utf8_lossy(bytes);
    let mut start = output.len().saturating_sub(limit);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    (output[start..].to_string(), state.truncated || start > 0)
}

fn spawn_command(request: &CreateTerminalRequest, cwd: &Path) -> std::io::Result<TerminalProcess> {
    #[cfg(unix)]
    let mut command = {
        let mut shell = Command::new("/bin/sh");
        shell.arg("-c");
        if request.args.is_empty() {
            shell.arg(&request.command);
        } else {
            // A fixed script expands positional parameters as literal words;
            // neither the executable name nor argument data becomes shell source.
            shell
                .arg(r#""$0" "$@""#)
                .arg(&request.command)
                .args(&request.args);
        }
        shell.process_group(0);
        shell
    };
    #[cfg(not(unix))]
    let mut command = {
        let mut command = Command::new(&request.command);
        command.args(&request.args);
        command
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for variable in &request.env {
        command.env(&variable.name, &variable.value);
    }
    let child = command.spawn()?;
    Ok(TerminalProcess {
        #[cfg(unix)]
        process_group: child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id.cast_signed())),
        child,
    })
}

fn validate_create_request(request: &CreateTerminalRequest) -> Result<(), Error> {
    if request.command.is_empty()
        || request.command.len() > MAX_TERMINAL_COMMAND_LENGTH
        || request.command.contains('\0')
    {
        return Err(Error::invalid_params().data(format!(
            "terminal command must contain 1..={MAX_TERMINAL_COMMAND_LENGTH} characters without NUL bytes"
        )));
    }
    if request.args.len() > MAX_TERMINAL_ARGS
        || request
            .args
            .iter()
            .any(|arg| arg.len() > MAX_TERMINAL_ARG_LENGTH || arg.contains('\0'))
    {
        return Err(Error::invalid_params().data("terminal arguments exceed the supported limits"));
    }
    if request.env.len() > MAX_TERMINAL_ENV {
        return Err(Error::invalid_params().data(format!(
            "terminal environment exceeds {MAX_TERMINAL_ENV} variables"
        )));
    }
    let mut names = HashSet::new();
    for variable in &request.env {
        if variable.name.is_empty()
            || variable.name.len() > MAX_TERMINAL_ENV_NAME_LENGTH
            || variable.name.contains(['=', '\0'])
            || variable.value.len() > MAX_TERMINAL_ENV_VALUE_LENGTH
            || variable.value.contains('\0')
            || !names.insert(variable.name.as_str())
        {
            return Err(Error::invalid_params().data("terminal environment is invalid"));
        }
    }
    Ok(())
}

fn terminal_spawn_error(error: std::io::Error) -> Error {
    Error::internal_error().data(format!("failed to start terminal command: {error}"))
}

fn normalize_exit_status(status: std::process::ExitStatus) -> TerminalExitStatus {
    if let Some(code) = status.code().and_then(|code| u32::try_from(code).ok()) {
        return TerminalExitStatus::new().exit_code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return TerminalExitStatus::new().signal(format!("SIG{signal}"));
        }
    }
    TerminalExitStatus::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::Duration;

    fn manager(root: &Path) -> (TerminalManager, mpsc::UnboundedReceiver<String>) {
        let filesystem = Arc::new(WorkspaceFileSystem::new(root, false, &[]).unwrap());
        let (events, receiver) = mpsc::unbounded_channel();
        (
            TerminalManager::new_with_snapshots(filesystem, events, None),
            receiver,
        )
    }

    async fn create_terminal(
        terminals: &TerminalManager,
        request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, Error> {
        terminals.create_for_incarnation(request, 0).await
    }

    fn output_request(session_id: &str, terminal_id: &TerminalId) -> TerminalOutputRequest {
        TerminalOutputRequest::new(session_id.to_string(), terminal_id.clone())
    }

    async fn wait_until_exited(
        terminals: &TerminalManager,
        session_id: &str,
        terminal_id: &TerminalId,
    ) -> TerminalOutputResponse {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let output = terminals
                    .output(output_request(session_id, terminal_id))
                    .await
                    .unwrap();
                if output.exit_status.is_some() {
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal did not exit")
    }

    #[tokio::test]
    async fn output_respects_utf8_byte_budget_while_a_character_is_incomplete() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let terminal = Arc::new(Terminal {
            id: "partial".to_string(),
            session_id: "session".to_string(),
            incarnation: 1,
            output_limit: 1,
            state: Mutex::new(TerminalState {
                output: vec![0xe4],
                ..Default::default()
            }),
            changed: Notify::new(),
            kill: Mutex::new(None),
            readers_remaining: AtomicUsize::new(1),
            reader_tasks: Mutex::new(Vec::new()),
        });
        terminals
            .terminals
            .lock()
            .await
            .insert(terminal.id.clone(), terminal.clone());
        let running = terminals
            .output(TerminalOutputRequest::new("session", "partial"))
            .await
            .unwrap();
        assert!(running.output.len() <= 1);
        assert_eq!(
            running.output, "",
            "an incomplete UTF-8 character is not output yet"
        );
        terminal.state.lock().await.exit_status = Some(TerminalExitStatus::new().exit_code(0));
        let completed = terminals
            .output(TerminalOutputRequest::new("session", "partial"))
            .await
            .unwrap();
        assert!(
            completed.output.len() <= 1,
            "replacement characters also obey the byte budget"
        );
        let (_, retained) = terminals
            .release_with_snapshot(ReleaseTerminalRequest::new("session", "partial"))
            .await
            .unwrap();
        assert!(retained.value["output"].as_str().unwrap().len() <= 1);
    }

    #[tokio::test]
    async fn terminal_n_bytes_produce_linear_internal_and_public_bytes() {
        const CHUNK_COUNT: usize = 8;
        const CHUNK_BYTES: usize = 1_024;

        let root = tempfile::tempdir().unwrap();
        let filesystem = Arc::new(WorkspaceFileSystem::new(root.path(), false, &[]).unwrap());
        let (events, mut event_rx) = mpsc::unbounded_channel();
        let (snapshots, mut snapshot_rx) = mpsc::channel(CHUNK_COUNT + 1);
        let terminals = TerminalManager::new_with_snapshots(filesystem, events, Some(snapshots));
        let terminal = Arc::new(Terminal {
            id: "linear-terminal".to_string(),
            session_id: "session".to_string(),
            incarnation: 1,
            output_limit: CHUNK_COUNT * CHUNK_BYTES,
            state: Mutex::new(TerminalState::default()),
            changed: Notify::new(),
            kill: Mutex::new(None),
            readers_remaining: AtomicUsize::new(0),
            reader_tasks: Mutex::new(Vec::new()),
        });

        for index in 0..CHUNK_COUNT {
            terminal
                .state
                .lock()
                .await
                .output
                .extend(vec![b'a' + index as u8; CHUNK_BYTES]);
            terminals.emit_snapshot(&terminal).await;
        }

        let public_output_bytes = std::iter::from_fn(|| event_rx.try_recv().ok())
            .map(|event| {
                serde_json::from_str::<Value>(&event).unwrap()["terminal"]["output"]
                    .as_str()
                    .unwrap()
                    .len()
            })
            .sum::<usize>();
        let internal_output_bytes = std::iter::from_fn(|| snapshot_rx.try_recv().ok())
            .map(|snapshot| snapshot.value["output"].as_str().unwrap().len())
            .sum::<usize>();
        let input_bytes = CHUNK_COUNT * CHUNK_BYTES;

        assert!(
            public_output_bytes + internal_output_bytes <= input_bytes * 2,
            "each output byte may cross each publication channel once; cumulative full snapshots published {public_output_bytes} public bytes and {internal_output_bytes} internal bytes for {input_bytes} input bytes"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scopes_handles_emits_snapshots_and_releases_terminal() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, mut events) = manager(root.path());
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new("owner", "/bin/sh")
                .args(vec!["-c".to_string(), "printf ok".to_string()])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        let exited = wait_until_exited(&terminals, "owner", &created.terminal_id).await;
        assert_eq!(exited.output, "ok");
        assert!(
            terminals
                .output(output_request("other", &created.terminal_id))
                .await
                .is_err()
        );
        let (_, retained) = terminals
            .release_with_snapshot(ReleaseTerminalRequest::new(
                "owner",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(retained.value["output"], "ok");
        assert_eq!(retained.value["exitStatus"]["exitCode"], 0);
        assert_eq!(retained.value["released"], true);
        assert!(
            terminals
                .output(output_request("owner", &created.terminal_id))
                .await
                .is_err()
        );

        let snapshots = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .collect::<Vec<_>>();
        let mut published_output = String::new();
        for event in &snapshots {
            let terminal = &event["terminal"];
            if terminal["outputAppend"] == false {
                published_output.clear();
            }
            published_output.push_str(terminal["output"].as_str().unwrap());
        }
        assert_eq!(published_output, "ok");
        assert!(
            snapshots
                .iter()
                .any(|event| event["terminal"]["exitStatus"]["exitCode"] == 0)
        );
        assert_eq!(snapshots.last().unwrap()["terminal"]["released"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truncates_output_only_at_utf8_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "/bin/sh")
                .args(vec![
                    "-c".to_string(),
                    r"printf '\360\237\231\202\360\237\231\202\360\237\231\202'".to_string(),
                ])
                .cwd(root.path().to_path_buf())
                .output_byte_limit(9),
        )
        .await
        .unwrap();
        let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
        assert!(output.truncated);
        assert_eq!(output.output, "🙂🙂");
        assert!(output.output.len() <= 9);
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executes_shell_commands_with_environment_and_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        for script in [
            "pwd",
            "uname -a",
            "command -v sh",
            "printf '%s\\n' 'shell-ok'; uname -a; command -v sh; printf '%s' \"$ATTYD_TERMINAL_VALUE\" | tr 'a-z' 'A-Z' > result; cat result",
        ] {
            let created = create_terminal(
                &terminals,
                CreateTerminalRequest::new("session", script)
                    .cwd(root.path().to_path_buf())
                    .env(vec![EnvVariable::new("ATTYD_TERMINAL_VALUE", "two words")]),
            )
            .await
            .expect("shell command must create a terminal");
            let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
            assert_eq!(output.exit_status.unwrap().exit_code, Some(0), "{script}");
            assert!(!output.output.is_empty());
            terminals
                .release(ReleaseTerminalRequest::new("session", created.terminal_id))
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("result")).unwrap(),
            "TWO WORDS"
        );
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserves_explicit_command_and_arguments_as_literal_values() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let unexpected = root.path().join("unexpected");

        // Whitespace and shell metacharacters remain part of an executable path.
        let executable = root.path().join("printf with spaces;$VALUE");
        std::os::unix::fs::symlink("/usr/bin/printf", &executable).unwrap();
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", executable.to_string_lossy())
                .args(vec![
                    "%s\\n".to_string(),
                    String::new(),
                    "$(touch unexpected); $HOME".to_string(),
                    "two words".to_string(),
                    "'single' \"double\" \\ *".to_string(),
                ])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
        assert_eq!(
            output.output,
            "\n$(touch unexpected); $HOME\ntwo words\n'single' \"double\" \\ *\n"
        );
        assert!(!unexpected.exists());

        let builtin = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "command")
                .args(vec!["-v".to_string(), "sh".to_string()]),
        )
        .await
        .expect("explicit arguments also support shell builtins");
        assert_eq!(
            wait_until_exited(&terminals, "session", &builtin.terminal_id)
                .await
                .exit_status
                .unwrap()
                .exit_code,
            Some(0)
        );

        let shell = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "/bin/sh")
                .args(vec!["-c".to_string(), "printf explicit-shell".to_string()])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        assert_eq!(
            wait_until_exited(&terminals, "session", &shell.terminal_id)
                .await
                .output,
            "explicit-shell"
        );
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_commands_report_shell_exit_status_and_allow_subsequent_commands() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        for args in [Vec::new(), vec!["literal argument".to_string()]] {
            let created = create_terminal(
                &terminals,
                CreateTerminalRequest::new("session", "./missing-command")
                    .args(args)
                    .cwd(root.path().to_path_buf()),
            )
            .await
            .expect("the shell starts even if the requested command does not exist");
            let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
            assert_eq!(output.exit_status.unwrap().exit_code, Some(127));
            assert!(output.output.contains("missing-command"));
            terminals
                .release(ReleaseTerminalRequest::new("session", created.terminal_id))
                .await
                .unwrap();
        }
        let recovered = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "./missing-command; printf recovered")
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        let output = wait_until_exited(&terminals, "session", &recovered.terminal_id).await;
        assert_eq!(output.exit_status.unwrap().exit_code, Some(0));
        assert!(output.output.contains("recovered"));
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_exit_is_published_only_after_stdout_and_stderr_are_drained() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new(
                "session",
                "i=0; while [ \"$i\" -lt 2048 ]; do printf 'abcdefgh'; printf 'ABCDEFGH' >&2; i=$((i + 1)); done",
            ),
        )
        .await
        .unwrap();
        let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
        assert_eq!(output.exit_status.unwrap().exit_code, Some(0));
        assert!(!output.truncated);
        assert_eq!(output.output.len(), 2048 * 16);
        for byte in b"abcdefghABCDEFGH" {
            assert_eq!(
                output.output.bytes().filter(|value| value == byte).count(),
                2048
            );
        }
        terminals.close_all().await;
    }

    #[cfg(unix)]
    struct BackgroundProcess(u32);

    #[cfg(unix)]
    impl Drop for BackgroundProcess {
        fn drop(&mut self) {
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(self.0.cast_signed()).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
    }

    #[cfg(unix)]
    async fn assert_process_running(pid: u32) {
        let state = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .await
            .expect("inspect the fixture's background process");
        let state_text = String::from_utf8(state.stdout).unwrap();
        assert!(
            state.status.success()
                && !state_text.trim().is_empty()
                && !state_text.trim_start().starts_with('Z'),
            "an exited terminal must not stop its background process: {state_text}"
        );
    }

    #[cfg(unix)]
    async fn terminal_child_pid(terminals: &TerminalManager, terminal_id: &TerminalId) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let output = terminals
                    .output(output_request("session", terminal_id))
                    .await
                    .unwrap();
                if let Some(line) = output.output.lines().next()
                    && let Ok(pid) = line.trim().parse::<u32>()
                {
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal did not report its child PID")
    }

    #[cfg(unix)]
    async fn assert_process_stopped(pid: u32) {
        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "stat="])
                    .output()
                    .await
                    .expect("inspect the terminal's child process");
                assert!(state.status.success() || state.status.code() == Some(1));
                let state = String::from_utf8(state.stdout).unwrap();
                if state.trim().is_empty() || state.trim_start().starts_with('Z') {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if stopped.is_err() {
            // A broken cleanup implementation must not leak the fixture child.
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(pid.cast_signed()).unwrap(),
                rustix::process::Signal::KILL,
            );
        }
        stopped.expect("terminal cleanup left its shell child running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_cleanup_terminates_shell_children_for_each_lifecycle_action() {
        for action in ["kill", "release", "session", "shutdown"] {
            let root = tempfile::tempdir().unwrap();
            let (terminals, _events) = manager(root.path());
            let created = create_terminal(
                &terminals,
                CreateTerminalRequest::new(
                    "session",
                    "sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait \"$child\"",
                ),
            )
            .await
            .unwrap();
            let pid = terminal_child_pid(&terminals, &created.terminal_id).await;
            match action {
                "kill" => {
                    terminals
                        .kill(KillTerminalRequest::new(
                            "session",
                            created.terminal_id.clone(),
                        ))
                        .await
                        .unwrap();
                    let output =
                        wait_until_exited(&terminals, "session", &created.terminal_id).await;
                    assert!(output.exit_status.unwrap().signal.is_some());
                }
                "release" => {
                    terminals
                        .release(ReleaseTerminalRequest::new(
                            "session",
                            created.terminal_id.clone(),
                        ))
                        .await
                        .unwrap();
                }
                "session" => terminals.release_session("session").await,
                "shutdown" => terminals.close_all().await,
                _ => unreachable!(),
            }
            assert_process_stopped(pid).await;
            terminals.close_all().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_exit_preserves_background_processes_after_terminal_release() {
        for exit_code in [0, 7] {
            let root = tempfile::tempdir().unwrap();
            let (terminals, _events) = manager(root.path());
            let created = create_terminal(
                &terminals,
                CreateTerminalRequest::new(
                    "session",
                    format!(
                        "nohup sleep 60 </dev/null >/dev/null 2>&1 & printf '%s\\n' \"$!\"; exit {exit_code}"
                    ),
                ),
            )
            .await
            .unwrap();
            let child =
                BackgroundProcess(terminal_child_pid(&terminals, &created.terminal_id).await);
            let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
            assert_eq!(
                output.exit_status.as_ref().unwrap().exit_code,
                Some(exit_code)
            );
            assert!(!output.truncated);
            assert_process_running(child.0).await;

            terminals
                .kill(KillTerminalRequest::new(
                    "session",
                    created.terminal_id.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(
                terminals
                    .output(output_request("session", &created.terminal_id))
                    .await
                    .unwrap(),
                output
            );
            terminals
                .release(ReleaseTerminalRequest::new(
                    "session",
                    created.terminal_id.clone(),
                ))
                .await
                .unwrap();
            assert!(
                terminals
                    .output(output_request("session", &created.terminal_id))
                    .await
                    .is_err()
            );
            terminals.release_session("session").await;
            terminals.close_all().await;
            assert_process_running(child.0).await;
            let pid = child.0;
            drop(child);
            assert_process_stopped(pid).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_an_already_exited_process_does_not_signal_its_background_group() {
        let root = tempfile::tempdir().unwrap();
        let mut process = spawn_command(
            &CreateTerminalRequest::new(
                "session",
                "nohup sleep 60 </dev/null >/dev/null 2>&1 & printf '%s\\n' \"$!\"",
            ),
            root.path(),
        )
        .unwrap();
        let mut output = String::new();
        process
            .child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .await
            .unwrap();
        let child = BackgroundProcess(output.trim().parse().unwrap());
        assert!(process.child.wait().await.unwrap().success());
        // The kill request can win select! even when child.wait() is also ready.
        process.terminate();
        drop(process);
        assert_process_running(child.0).await;
        let pid = child.0;
        drop(child);
        assert_process_stopped(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_output_writers_cannot_prevent_terminal_exit() {
        for request in [
            CreateTerminalRequest::new("session", "sleep 60 & printf '%s\\n' \"$!\""),
            CreateTerminalRequest::new("session", "node").args(vec![
                "-e".to_string(),
                "const child = require('node:child_process').spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { detached: true, stdio: 'inherit' }); console.log(child.pid); child.unref();".to_string(),
            ]),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (terminals, _events) = manager(root.path());
            let created = create_terminal(&terminals, request).await.unwrap();
            let child = BackgroundProcess(terminal_child_pid(&terminals, &created.terminal_id).await);
            let terminal = terminals.require(&created.terminal_id.0, "session").await.unwrap();
            let output = wait_until_exited(&terminals, "session", &created.terminal_id).await;
            assert_eq!(output.exit_status.unwrap().exit_code, Some(0));
            assert!(
                !output.truncated,
                "a pipe held open by a background process is not byte-limit truncation"
            );
            assert_eq!(terminal.readers_remaining.load(Ordering::Acquire), 0);
            assert_process_running(child.0).await;
            let pid = child.0;
            drop(child);
            assert_process_stopped(pid).await;
            terminals.close_all().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn immediate_release_retires_unpolled_readers_and_waiter() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "sleep 60"),
        )
        .await
        .unwrap();
        let terminal = terminals
            .require(&created.terminal_id.0, "session")
            .await
            .unwrap();
        terminals
            .release(ReleaseTerminalRequest::new("session", created.terminal_id))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = terminal.changed.notified();
                if terminal.state.lock().await.exit_status.is_some() {
                    assert_eq!(terminal.readers_remaining.load(Ordering::Acquire), 0);
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("immediate release must not leave its waiter blocked");
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[test]
    fn dropping_runtime_terminates_the_terminal_process_group() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let pid = runtime.block_on(async {
            let (terminals, _events) = manager(root.path());
            let created = create_terminal(
                &terminals,
                CreateTerminalRequest::new(
                    "session",
                    "sleep 60 & child=$!; printf '%s\\n' \"$child\"; wait \"$child\"",
                ),
            )
            .await
            .unwrap();
            terminal_child_pid(&terminals, &created.terminal_id).await
        });
        drop(runtime);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(assert_process_stopped(pid));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_hostile_create_inputs_without_poisoning_manager() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, mut events) = manager(root.path());
        assert!(
            create_terminal(
                &terminals,
                CreateTerminalRequest::new("session", "x".repeat(MAX_TERMINAL_COMMAND_LENGTH + 1),)
            )
            .await
            .is_err()
        );
        assert!(
            create_terminal(
                &terminals,
                CreateTerminalRequest::new("session", "/bin/sh").env(vec![
                    EnvVariable::new("DUPLICATE", "one"),
                    EnvVariable::new("DUPLICATE", "two"),
                ])
            )
            .await
            .is_err()
        );
        assert!(
            create_terminal(
                &terminals,
                CreateTerminalRequest::new("session", "pwd",)
                    .cwd(root.path().join("missing-directory"))
            )
            .await
            .is_err()
        );
        assert!(events.try_recv().is_err());

        let recovered = create_terminal(
            &terminals,
            CreateTerminalRequest::new("session", "/bin/sh")
                .args(vec!["-c".to_string(), "printf recovered".to_string()])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        assert_eq!(
            wait_until_exited(&terminals, "session", &recovered.terminal_id)
                .await
                .output,
            "recovered"
        );
        terminals.close_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn releases_only_terminals_owned_by_closed_session() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let owner = create_terminal(
            &terminals,
            CreateTerminalRequest::new("owner", "/bin/sh")
                .args(vec![
                    "-c".to_string(),
                    "while :; do sleep 1; done".to_string(),
                ])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        let survivor = create_terminal(
            &terminals,
            CreateTerminalRequest::new("survivor", "/bin/sh")
                .args(vec!["-c".to_string(), "printf alive".to_string()])
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();

        terminals.release_session("owner").await;
        assert!(
            terminals
                .output(output_request("owner", &owner.terminal_id))
                .await
                .is_err()
        );
        assert_eq!(
            wait_until_exited(&terminals, "survivor", &survivor.terminal_id)
                .await
                .output,
            "alive"
        );
        terminals.close_all().await;
    }

    #[test]
    fn validates_command_arguments_and_environment() {
        assert!(validate_create_request(&CreateTerminalRequest::new("session", "")).is_err());
        assert!(
            validate_create_request(
                &CreateTerminalRequest::new("session", "command")
                    .args(vec!["bad\0argument".to_string()]),
            )
            .is_err()
        );
        assert!(
            validate_create_request(
                &CreateTerminalRequest::new("session", "command")
                    .env(vec![EnvVariable::new("BAD=NAME", "value")]),
            )
            .is_err()
        );
    }
}
