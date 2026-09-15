//! ACP WebSocket transport with no message or frame size policy.
//!
//! The SDK's HTTP client currently exposes no WebSocket configuration hook.
//! Keep its Channel framing and independent read/write lifetime here so removing
//! the bridge's limits does not reveal another cutoff in tungstenite defaults.

use agent_client_protocol::{Agent, Channel, Client, ConnectTo, Error, TransportFrame};
use async_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use futures::{StreamExt, future::BoxFuture};

pub(crate) struct WebSocketAgent {
    endpoint: String,
}

impl WebSocketAgent {
    pub(crate) fn new(endpoint: String) -> Self {
        Self { endpoint }
    }
}

impl ConnectTo<Client> for WebSocketAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> Result<(), Error> {
        let (channel, transport) = self.into_channel_and_future();
        let shutdown = channel.tx.clone();
        match futures::future::select(
            std::pin::pin!(client.connect_to(channel)),
            std::pin::pin!(transport),
        )
        .await
        {
            futures::future::Either::Left((result, transport)) => {
                result?;
                shutdown.close_channel();
                transport.await
            }
            futures::future::Either::Right((result, _)) => result,
        }
    }

    fn into_channel_and_future(self) -> (Channel, BoxFuture<'static, Result<(), Error>>) {
        let (caller, transport) = Channel::duplex();
        (caller, Box::pin(run(self.endpoint, transport)))
    }
}

async fn run(endpoint: String, channel: Channel) -> Result<(), Error> {
    let config = WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let (socket, _) = async_tungstenite::tokio::connect_async_with_config(endpoint, Some(config))
        .await
        .map_err(|error| Error::internal_error().data(format!("WebSocket connect: {error}")))?;
    let (mut writer, mut reader) = socket.split();
    let Channel {
        rx: mut outgoing,
        tx: incoming,
    } = channel;
    let write = async move {
        while let Some(frame) = outgoing.next().await {
            let text = frame.to_json().map_err(Error::into_internal_error)?;
            writer
                .send(Message::Text(text.into()))
                .await
                .map_err(|error| {
                    Error::internal_error().data(format!("WebSocket send: {error}"))
                })?;
        }
        let _ = writer.send(Message::Close(None)).await;
        Ok(())
    };
    let read = async move {
        let mut discard_incoming = false;
        while let Some(message) = reader.next().await {
            match message.map_err(|error| {
                Error::internal_error().data(format!("WebSocket receive: {error}"))
            })? {
                Message::Text(text) if !discard_incoming => {
                    // Preserve malformed and batch frames for the SDK's protocol router.
                    if incoming
                        .unbounded_send(TransportFrame::parse_json(&text))
                        .is_err()
                    {
                        // The caller may be draining accepted outgoing work after closing input.
                        discard_incoming = true;
                    }
                }
                Message::Close(frame) => {
                    return Err(Error::internal_error()
                        .data(format!("WebSocket closed by peer: {frame:?}")));
                }
                Message::Binary(_) => {
                    tracing::warn!("ignoring binary WebSocket frame (ACP uses text)")
                }
                _ => {}
            }
        }
        Err(Error::internal_error().data("WebSocket stream ended"))
    };
    match futures::future::select(std::pin::pin!(write), std::pin::pin!(read)).await {
        futures::future::Either::Left((result, _))
        | futures::future::Either::Right((result, _)) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message as PeerMessage;

    #[tokio::test]
    async fn receives_a_frame_larger_than_both_former_websocket_limits() {
        const PAYLOAD_BYTES: usize = 65 * 1024 * 1024;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let text = format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":"{}"}}"#,
                "x".repeat(PAYLOAD_BYTES)
            );
            socket.send(PeerMessage::Text(text.into())).await.unwrap();
            // Stay connected until the caller has consumed the complete frame.
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    break;
                }
            }
        });
        let (mut channel, transport) =
            WebSocketAgent::new(format!("ws://{address}")).into_channel_and_future();
        let transport = tokio::spawn(transport);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(30), channel.rx.next())
            .await
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&frame.to_json().unwrap()).unwrap();
        assert_eq!(value["result"].as_str().unwrap().len(), PAYLOAD_BYTES);
        drop(channel);
        tokio::time::timeout(std::time::Duration::from_secs(5), transport)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        peer.await.unwrap();
    }
}
