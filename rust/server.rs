use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde_json::json;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::bridge;
use crate::options::Options;

#[derive(RustEmbed)]
#[folder = "dist/client/"]
struct ClientAssets;

#[derive(Clone)]
struct AppState {
    options: Arc<Options>,
}

pub async fn serve(options: Options) -> Result<()> {
    let options = Arc::new(options.normalized().map_err(anyhow::Error::msg)?);
    if options.transport == crate::options::Transport::Stdio {
        std::env::set_current_dir(&options.cwd).with_context(|| {
            format!("failed to enter Agent workspace {}", options.cwd.display())
        })?;
    }
    let address = SocketAddr::new(options.host, options.port);
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/ws", get(websocket))
        .fallback(get(static_asset))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            options: options.clone(),
        });
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind http://{address}"))?;
    let actual = listener.local_addr()?;

    println!("attyd listening on http://{actual}");
    println!(
        "agent ({}): {}",
        options.transport.as_str(),
        options.command.join(" ")
    );
    if options.transport == crate::options::Transport::Stdio {
        println!("default Agent workspace: {}", options.cwd.display());
    } else {
        println!("Agent workspace: selected per new thread in the web UI");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let _ = &state.options;
    axum::Json(json!({ "ok": true, "protocol": "acp/v1", "backend": "rust" }))
}

async fn websocket(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(bridge::MAX_BRIDGE_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve_websocket(socket, state.options))
}

async fn serve_websocket(socket: WebSocket, options: Arc<Options>) {
    let (mut writer, mut reader) = socket.split();
    let (command_tx, command_rx) = tokio::sync::mpsc::channel::<String>(256);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let mut bridge_task = tokio::spawn(bridge::run(options, command_rx, event_tx));
    let mut writer_task = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if writer.send(Message::Text(event.into())).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = async {
            while let Some(message) = reader.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        if command_tx.send(text.to_string()).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Binary(_)) => break,
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                }
            }
        } => {},
        _ = &mut bridge_task => {},
        _ = &mut writer_task => {},
    }

    bridge_task.abort();
    writer_task.abort();
}

async fn static_asset(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    let (asset, served_path) = match ClientAssets::get(path) {
        Some(asset) => (Some(asset), path),
        None => (ClientAssets::get("index.html"), "index.html"),
    };
    let Some(asset) = asset else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let content_type = mime_guess::from_path(served_path).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type.as_ref())
        .body(Body::from(asset.data))
        .expect("valid embedded asset response")
}
