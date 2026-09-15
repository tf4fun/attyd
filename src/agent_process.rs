use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{
    AcpAgentConfig, Agent, BoxFuture, Channel, Client, ConnectTo, Error, Lines,
};
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::{Either, select};
use futures::{Stream, stream};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(1);
const STDERR_CHUNK_BYTES: usize = 8 * 1024;

type StderrCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;
type FatalCallback = Arc<dyn Fn(Error) + Send + Sync + 'static>;

pub struct StdioAcpAgent {
    config: AcpAgentConfig,
    stderr_callback: Option<StderrCallback>,
    fatal_callback: Option<FatalCallback>,
}

impl StdioAcpAgent {
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

impl ConnectTo<Client> for StdioAcpAgent {
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

impl StdioAcpAgent {
    fn spawn_channel(self) -> Result<(Channel, BoxFuture<'static, Result<(), Error>>), Error> {
        let mut command = Command::new(self.config.command());
        command
            .args(self.config.arguments())
            .envs(self.config.environment())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(Error::into_internal_error)?;
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::internal_error().data("Failed to open Agent stdin"))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::internal_error().data("Failed to open Agent stdout"))?;
        let mut child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::internal_error().data("Failed to open Agent stderr"))?;
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
        let stdout_future = async move {
            let lines = ndjson_lines(BufReader::new(child_stdout));
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
        };
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
                    // EOF seals the input; let every accepted frame drain.
                    protocol.await
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
                        // Natural process exit must not time out accepted output.
                        protocol.await
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

struct ProcessGuard(Child);

impl ProcessGuard {
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.wait().await
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0.id()
            && let Some(pid) = rustix::process::Pid::from_raw(id.cast_signed())
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.start_kill();
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

fn ndjson_lines<R>(reader: R) -> impl Stream<Item = std::io::Result<String>> + Send
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
                line.extend_from_slice(content);
                reader.consume(consumed);
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

    use futures::StreamExt;

    #[tokio::test]
    async fn preserves_large_utf8_lines_and_resets_at_newlines() {
        let large = "你好".repeat(1_400_000);
        let input = format!("{large}\r\nnext\nlast");
        let lines = ndjson_lines(BufReader::new(std::io::Cursor::new(input.into_bytes())))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].as_ref().unwrap().len(), large.len());
        assert_eq!(lines[0].as_ref().unwrap(), &large);
        assert_eq!(lines[1].as_ref().unwrap(), "next");
        assert_eq!(lines[2].as_ref().unwrap(), "last");
    }
}
