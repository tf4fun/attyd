use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, BoxFuture, Channel, Client, ConnectTo, Error, Lines,
};
use async_process::Child;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::{Either, select};
use futures::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use futures::{Stream, stream};

pub const MAX_AGENT_NDJSON_LINE_BYTES: usize = 8_000_000;
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);
const STDOUT_POLL_HEARTBEAT: Duration = Duration::from_millis(25);
const STDERR_CHUNK_BYTES: usize = 8 * 1024;

type StderrCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;
type FatalCallback = Arc<dyn Fn(Error) + Send + Sync + 'static>;

pub struct BoundedAcpAgent {
    config: AcpAgentConfig,
    stderr_callback: Option<StderrCallback>,
    fatal_callback: Option<FatalCallback>,
}

impl BoundedAcpAgent {
    pub fn new(config: AcpAgentConfig) -> Self {
        Self {
            config,
            stderr_callback: None,
            fatal_callback: None,
        }
    }

    pub fn on_stderr(mut self, callback: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.stderr_callback = Some(Arc::new(callback));
        self
    }

    pub fn on_fatal(mut self, callback: impl Fn(Error) + Send + Sync + 'static) -> Self {
        self.fatal_callback = Some(Arc::new(callback));
        self
    }
}

impl ConnectTo<Client> for BoundedAcpAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let (channel, transport) = self.into_channel_and_future();
        futures::try_join!(transport, ConnectTo::<Client>::connect_to(channel, client))?;
        Ok(())
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), Error>>) {
        match self.spawn_channel() {
            Ok(connection) => connection,
            Err(error) => {
                let (channel, peer) = Channel::duplex();
                drop(peer);
                (channel, Box::pin(async move { Err(error) }))
            }
        }
    }
}

impl BoundedAcpAgent {
    fn spawn_channel(self) -> Result<(Channel, BoxFuture<'static, Result<(), Error>>), Error> {
        let (child_stdin, child_stdout, mut child_stderr, child) =
            AcpAgent::new(self.config).spawn_process()?;
        let mut child = ProcessGuard(child);

        let stderr_callback = self.stderr_callback;
        let fatal_callback = self.fatal_callback;
        let stderr_future = async move {
            let mut buffer = vec![0_u8; STDERR_CHUNK_BYTES];
            loop {
                match child_stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if let Some(callback) = &stderr_callback {
                            callback(String::from_utf8_lossy(&buffer[..count]).into_owned());
                        }
                    }
                }
            }
        };

        let (incoming_tx, incoming) = mpsc::unbounded::<std::io::Result<String>>();
        let stdout_future = poll_with_heartbeat(async move {
            let lines = bounded_lines(BufReader::new(child_stdout), MAX_AGENT_NDJSON_LINE_BYTES);
            let mut lines = pin!(lines);
            while let Some(line) = lines.next().await {
                match line {
                    Ok(line) => incoming_tx
                        .unbounded_send(Ok(line))
                        .map_err(Error::into_internal_error)?,
                    Err(error) => return Err(Error::into_internal_error(error)),
                }
            }
            Ok(())
        });
        let outgoing = futures::sink::unfold(child_stdin, |mut writer, line: String| async move {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            Ok::<_, std::io::Error>(writer)
        });
        let (channel, protocol) =
            ConnectTo::<Client>::into_channel_and_future(Lines::new(outgoing, incoming));
        let protocol = async move {
            let protocol = pin!(protocol);
            let stdout_future = pin!(stdout_future);
            match select(protocol, stdout_future).await {
                Either::Left((result, _)) => result,
                Either::Right((stdout_result, protocol)) => {
                    stdout_result?;
                    match tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, protocol).await {
                        Ok(result) => result,
                        Err(_) => Ok(()),
                    }
                }
            }
        };

        let future = Box::pin(async move {
            let stderr_future = pin!(stderr_future);
            let protocol = pin!(protocol);
            let child_wait = pin!(async move { child.wait().await });
            let main = async {
                match select(protocol, child_wait).await {
                    Either::Left((result, child_wait)) => {
                        result?;
                        match tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, child_wait).await {
                            Ok(status) => {
                                validate_exit(status.map_err(Error::into_internal_error)?)
                            }
                            Err(_) => Ok(()),
                        }
                    }
                    Either::Right((status, protocol)) => {
                        validate_exit(status.map_err(Error::into_internal_error)?)?;
                        match tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, protocol).await {
                            Ok(result) => result,
                            Err(_) => Ok(()),
                        }
                    }
                }
            };
            let main = pin!(main);
            let result = match select(main, stderr_future).await {
                Either::Left((result, _)) => result,
                Either::Right(((), main)) => main.await,
            };
            if let (Err(error), Some(callback)) = (&result, fatal_callback) {
                callback(error.clone());
            }
            result
        });
        Ok((channel, future))
    }
}

async fn poll_with_heartbeat<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut heartbeat = tokio::time::interval(STDOUT_POLL_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = heartbeat.tick() => {}
        }
    }
}

struct ProcessGuard(Child);

impl ProcessGuard {
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.status().await
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id().cast_signed()) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.kill();
    }
}

fn validate_exit(status: std::process::ExitStatus) -> Result<(), Error> {
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(
            -32_000,
            format!("Agent process exited with status {status}"),
        ))
    }
}

fn bounded_lines<R>(reader: R, maximum: usize) -> impl Stream<Item = std::io::Result<String>> + Send
where
    R: AsyncBufRead + Send + Unpin + 'static,
{
    stream::unfold(
        (reader, Vec::<u8>::new(), false),
        move |(mut reader, mut line, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let available = match reader.fill_buf().await {
                    Ok(available) => available,
                    Err(error) => return Some((Err(error), (reader, line, true))),
                };
                if available.is_empty() {
                    if line.is_empty() {
                        return None;
                    }
                    return Some((decode_line(line), (reader, Vec::new(), true)));
                }

                let newline = available.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(available.len(), |index| index + 1);
                let content = newline.map_or(available, |index| &available[..index]);
                if line.len().saturating_add(content.len()) > maximum {
                    reader.consume_unpin(consumed);
                    return Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Agent NDJSON line exceeds {maximum} bytes"),
                        )),
                        (reader, Vec::new(), true),
                    ));
                }
                line.extend_from_slice(content);
                reader.consume_unpin(consumed);
                if newline.is_some() {
                    return Some((decode_line(line), (reader, Vec::new(), false)));
                }
            }
        },
    )
}

fn decode_line(mut line: Vec<u8>) -> std::io::Result<String> {
    if line.ends_with(b"\r") {
        line.pop();
    }
    String::from_utf8(line)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::{ProtocolVersion, v1::InitializeRequest};
    use futures::StreamExt;
    use futures::io::Cursor;
    use std::io::Write;

    #[tokio::test]
    async fn bounds_agent_lines_by_encoded_bytes_and_resets_at_newlines() {
        let input = Cursor::new("12345\n你好\nnext".as_bytes().to_vec());
        let lines = bounded_lines(BufReader::new(input), 6)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].as_ref().unwrap(), "12345");
        assert_eq!(lines[1].as_ref().unwrap(), "你好");
        assert_eq!(lines[2].as_ref().unwrap(), "next");

        let oversized = Cursor::new("你好\n".as_bytes().to_vec());
        let lines = bounded_lines(BufReader::new(oversized), 5)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("exceeds 5 bytes")
        );
    }

    #[test]
    fn oversized_agent_child_fixture() {
        if std::env::var_os("ATTYD_OVERSIZED_AGENT_CHILD").is_none() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&vec![b'x'; MAX_AGENT_NDJSON_LINE_BYTES + 1])
            .unwrap();
        stdout.write_all(b"\n").unwrap();
        stdout.flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[tokio::test]
    async fn surfaces_a_real_childs_oversized_line_without_waiting_for_exit() {
        let executable = std::env::current_exe().unwrap();
        let config = AcpAgentConfig::new(executable)
            .args([
                "--exact",
                "agent_process::tests::oversized_agent_child_fixture",
                "--nocapture",
            ])
            .env("ATTYD_OVERSIZED_AGENT_CHILD", "1");
        let agent = BoundedAcpAgent::new(config);
        let (_channel, connection) = ConnectTo::<Client>::into_channel_and_future(agent);
        let error = tokio::time::timeout(Duration::from_secs(10), connection)
            .await
            .expect("oversized child stdout must not leave the connection pending")
            .expect_err("oversized child stdout must fail the connection");
        assert!(
            format!("{error:?}").contains("Agent NDJSON line exceeds 8000000 bytes"),
            "unexpected connection error: {error:?}"
        );
    }

    #[tokio::test]
    async fn surfaces_an_oversized_line_through_the_client_builder() {
        let executable = std::env::current_exe().unwrap();
        let config = AcpAgentConfig::new(executable)
            .args([
                "--exact",
                "agent_process::tests::oversized_agent_child_fixture",
                "--nocapture",
            ])
            .env("ATTYD_OVERSIZED_AGENT_CHILD", "1");
        let agent = BoundedAcpAgent::new(config);
        let connection = Client.builder().connect_with(agent, async |connection| {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            Ok(())
        });
        let error = tokio::time::timeout(Duration::from_secs(10), connection)
            .await
            .expect("transport failure must wake the client foreground")
            .expect_err("oversized child stdout must fail the client connection");
        assert!(
            format!("{error:?}").contains("Agent NDJSON line exceeds 8000000 bytes"),
            "unexpected connection error: {error:?}"
        );
    }
}
