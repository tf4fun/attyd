use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use agent_client_protocol::Error;
use agent_client_protocol::RequestCancellation;
use agent_client_protocol::schema::v1::*;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use uuid::Uuid;

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

#[derive(Clone)]
pub struct TerminalManager {
    filesystem: Arc<WorkspaceFileSystem>,
    terminals: Arc<Mutex<HashMap<String, Arc<Terminal>>>>,
    events: mpsc::UnboundedSender<String>,
    snapshots: Option<mpsc::UnboundedSender<TerminalSnapshot>>,
}

struct Terminal {
    id: String,
    session_id: String,
    incarnation: u64,
    output_limit: usize,
    state: Mutex<TerminalState>,
    changed: Notify,
    kill: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Default)]
struct TerminalState {
    output: Vec<u8>,
    truncated: bool,
    exit_status: Option<TerminalExitStatus>,
    released: bool,
}

impl TerminalManager {
    pub fn new_with_snapshots(
        filesystem: Arc<WorkspaceFileSystem>,
        events: mpsc::UnboundedSender<String>,
        snapshots: Option<mpsc::UnboundedSender<TerminalSnapshot>>,
    ) -> Self {
        Self {
            filesystem,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            events,
            snapshots,
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

        let mut child = match spawn_command(&request, &cwd, false) {
            Ok(child) => child,
            Err(_error)
                if request.args.is_empty() && looks_like_shell_command(&request.command) =>
            {
                spawn_command(&request, &cwd, true).map_err(terminal_spawn_error)?
            }
            Err(error) => return Err(terminal_spawn_error(error)),
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = Uuid::new_v4().to_string();
        let terminal = Arc::new(Terminal {
            id: id.clone(),
            session_id: request.session_id.0.to_string(),
            incarnation,
            output_limit,
            state: Mutex::new(TerminalState::default()),
            changed: Notify::new(),
            kill: Mutex::new(None),
        });
        let (kill_tx, kill_rx) = oneshot::channel();
        *terminal.kill.lock().await = Some(kill_tx);
        self.terminals
            .lock()
            .await
            .insert(id.clone(), terminal.clone());

        if let Some(stdout) = stdout {
            self.spawn_reader(terminal.clone(), stdout);
        }
        if let Some(stderr) = stderr {
            self.spawn_reader(terminal.clone(), stderr);
        }
        self.spawn_waiter(terminal.clone(), child, kill_rx);
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
        Ok(TerminalOutputResponse::new(
            String::from_utf8_lossy(&state.output).into_owned(),
            state.truncated,
        )
        .exit_status(state.exit_status.clone()))
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
        let terminal = self
            .require(&request.terminal_id.0, &request.session_id.0)
            .await?;
        Self::signal_kill(&terminal).await;
        terminal.state.lock().await.released = true;
        self.emit_snapshot(&terminal).await;
        self.terminals.lock().await.remove(terminal.id.as_str());
        terminal.changed.notify_waiters();
        Ok(ReleaseTerminalResponse::new())
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
            terminal.state.lock().await.released = true;
            self.emit_snapshot(&terminal).await;
            self.terminals.lock().await.remove(terminal.id.as_str());
            terminal.changed.notify_waiters();
        }
    }

    pub async fn assert_reference(&self, terminal_id: &str, session_id: &str) -> Result<(), Error> {
        self.require(terminal_id, session_id).await.map(|_| ())
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

    fn spawn_reader<R>(&self, terminal: Arc<Terminal>, mut reader: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                let count = match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                {
                    let mut state = terminal.state.lock().await;
                    state.output.extend_from_slice(&buffer[..count]);
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
        });
    }

    fn spawn_waiter(
        &self,
        terminal: Arc<Terminal>,
        mut child: Child,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = &mut kill_rx => {
                    let _ = child.kill().await;
                    child.wait().await
                }
            };
            let exit_status = match status {
                Ok(status) => normalize_exit_status(status),
                Err(error) => TerminalExitStatus::new().signal(error.to_string()),
            };
            terminal.state.lock().await.exit_status = Some(exit_status);
            terminal.changed.notify_waiters();
            manager.emit_snapshot(&terminal).await;
        });
    }

    async fn signal_kill(terminal: &Arc<Terminal>) {
        if terminal.state.lock().await.exit_status.is_some() {
            return;
        }
        if let Some(kill) = terminal.kill.lock().await.take() {
            let _ = kill.send(());
        }
    }

    async fn emit_snapshot(&self, terminal: &Terminal) {
        let state = terminal.state.lock().await;
        let snapshot = json!({
            "sessionId": terminal.session_id,
            "terminalId": terminal.id,
            "output": String::from_utf8_lossy(&state.output),
            "truncated": state.truncated,
            "exitStatus": state.exit_status,
            "released": state.released,
        });
        let _ = self.events.send(
            json!({
                "type": "acp/terminal_state",
                "terminal": snapshot,
            })
            .to_string(),
        );
        if let Some(snapshots) = &self.snapshots {
            let _ = snapshots.send(TerminalSnapshot {
                incarnation: terminal.incarnation,
                value: snapshot,
            });
        }
    }
}

fn spawn_command(
    request: &CreateTerminalRequest,
    cwd: &Path,
    shell: bool,
) -> std::io::Result<Child> {
    let mut command = if shell {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(&request.command);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg(&request.command);
            command
        }
    } else {
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
    command.spawn()
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

fn looks_like_shell_command(command: &str) -> bool {
    command.chars().any(char::is_whitespace)
        || [
            "|", "&", ";", "<", ">", "(", ")", "$", "`", "\\", "*", "?", "[", "]", "{", "}",
        ]
        .iter()
        .any(|character| command.contains(character))
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
        terminals
            .release(ReleaseTerminalRequest::new(
                "owner",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert!(
            terminals
                .output(output_request("owner", &created.terminal_id))
                .await
                .is_err()
        );

        let snapshots = std::iter::from_fn(|| events.try_recv().ok())
            .map(|event| serde_json::from_str::<Value>(&event).unwrap())
            .collect::<Vec<_>>();
        assert!(snapshots.iter().any(|event| {
            event["terminal"]["output"] == "ok" && event["terminal"]["exitStatus"]["exitCode"] == 0
        }));
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
    async fn falls_back_to_shell_only_for_compound_commands_without_args() {
        let root = tempfile::tempdir().unwrap();
        let (terminals, _events) = manager(root.path());
        let created = create_terminal(
            &terminals,
            CreateTerminalRequest::new("goose-compatible", "printf ATTYD_COMPOUND_OK")
                .cwd(root.path().to_path_buf()),
        )
        .await
        .unwrap();
        let output = wait_until_exited(&terminals, "goose-compatible", &created.terminal_id).await;
        assert_eq!(output.output, "ATTYD_COMPOUND_OK");

        assert!(
            create_terminal(
                &terminals,
                CreateTerminalRequest::new("strict", "printf ATTYD_MUST_NOT_RUN")
                    .args(vec!["keep-strict-argv".to_string()])
                    .cwd(root.path().to_path_buf()),
            )
            .await
            .is_err()
        );
        terminals.close_all().await;
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
                CreateTerminalRequest::new(
                    "session",
                    root.path().join("missing-command").to_string_lossy(),
                )
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
    fn validates_command_arguments_environment_and_shell_detection() {
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
        assert!(looks_like_shell_command("printf hello"));
        assert!(looks_like_shell_command("echo $VALUE"));
        assert!(!looks_like_shell_command("/usr/bin/printf"));
    }
}
