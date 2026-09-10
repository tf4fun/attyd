use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, Version, header};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo};

#[derive(Clone)]
pub(crate) struct DevProxy {
    origin: String,
    authority: HeaderValue,
    client: Client<HttpConnector, Body>,
}

impl DevProxy {
    pub(crate) fn new(origin: &str) -> Self {
        let uri: Uri = origin.parse().expect("validated development server origin");
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        // The direct connector deliberately ignores HTTP_PROXY and never follows
        // redirects. Browsers receive the development server's original response.
        Self {
            origin: origin.to_string(),
            authority: HeaderValue::from_str(uri.authority().unwrap().as_str())
                .expect("validated development server authority"),
            client: Client::builder(TokioExecutor::new()).build(connector),
        }
    }

    pub(crate) async fn forward(&self, mut request: Request) -> Response {
        let websocket = has_token(request.headers(), header::CONNECTION, "upgrade")
            && has_token(request.headers(), header::UPGRADE, "websocket");
        let downstream_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |path| path.as_str());
        *request.uri_mut() = format!("{}{path}", self.origin)
            .parse()
            .expect("validated origin and incoming request URI");
        *request.version_mut() = Version::HTTP_11;
        strip_hop_headers(request.headers_mut(), websocket);
        // OriginPolicy already checked the browser's Host and Origin. Rewrite
        // only Host for Vite's own host check; preserve the browser Origin.
        request
            .headers_mut()
            .insert(header::HOST, self.authority.clone());

        let response =
            tokio::time::timeout(Duration::from_secs(30), self.client.request(request)).await;
        let mut response = match response {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(%error, server = %self.origin, "frontend development proxy failed");
                return unavailable();
            }
            Err(_) => {
                tracing::warn!(server = %self.origin, "frontend development server response timed out");
                return unavailable();
            }
        };
        let upgraded = response.status() == StatusCode::SWITCHING_PROTOCOLS;
        if upgraded {
            let Some(downstream_upgrade) = downstream_upgrade else {
                return unavailable();
            };
            if !has_token(response.headers(), header::UPGRADE, "websocket") {
                return unavailable();
            }
            let upstream_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                let upgrades = tokio::time::timeout(Duration::from_secs(10), async {
                    tokio::try_join!(downstream_upgrade, upstream_upgrade)
                })
                .await;
                match upgrades {
                    Ok(Ok((downstream, upstream))) => {
                        if let Err(error) = tokio::io::copy_bidirectional(
                            &mut TokioIo::new(downstream),
                            &mut TokioIo::new(upstream),
                        )
                        .await
                        {
                            tracing::debug!(%error, "frontend development WebSocket closed");
                        }
                    }
                    result => {
                        tracing::debug!(?result, "frontend development WebSocket upgrade failed")
                    }
                }
            });
        }
        strip_hop_headers(response.headers_mut(), upgraded);
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/html"))
            })
        {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.headers_mut().append(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("frame-ancestors 'none'"),
            );
            response
                .headers_mut()
                .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        }
        response.map(Body::new)
    }
}

fn unavailable() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(header::CACHE_CONTROL, "no-store")],
        "Frontend development server is unavailable. Start it at the configured --dev-server address.",
    )
        .into_response()
}

fn has_token(headers: &HeaderMap, name: HeaderName, expected: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(expected))
        })
    })
}

fn strip_hop_headers(headers: &mut HeaderMap, websocket: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
    if websocket {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
}

pub(crate) async fn reject_self_proxy(origin: &str, listener: SocketAddr) -> Result<()> {
    let target = url::Url::parse(origin).expect("validated development server origin");
    let port = target.port_or_known_default().unwrap();
    if port != listener.port() {
        return Ok(());
    }
    let host = target.host_str().unwrap().trim_matches(['[', ']']);
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .context("failed to resolve frontend development server")?;
    for address in addresses {
        if targets_listener(address.ip(), listener.ip())? {
            bail!(
                "--dev-server points back to attyd's own listening port {port}; choose a separate frontend development port"
            );
        }
    }
    Ok(())
}

fn targets_listener(target: IpAddr, listener: IpAddr) -> Result<bool> {
    let target = target.to_canonical();
    let listener = listener.to_canonical();
    if target == listener || target.is_unspecified() {
        return Ok(true);
    }
    if target.is_loopback() {
        return Ok(listener.is_unspecified() || listener.is_loopback());
    }
    if !listener.is_unspecified() {
        return Ok(false);
    }
    // A UDP route lookup sends no packet. It detects local LAN addresses too,
    // avoiding a proxy loop when attyd listens on every interface.
    let bind = if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let probe = UdpSocket::bind(bind).context("failed to check development proxy address")?;
    probe
        .connect(SocketAddr::new(target, 9))
        .context("failed to check development proxy route")?;
    Ok(probe.local_addr()?.ip() == target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    struct TestServer {
        origin: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start(app: Router) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        TestServer { origin, task }
    }

    #[tokio::test]
    async fn preserves_http_requests_responses_and_streams_without_buffering() {
        let (send_frame, receive_frame) =
            tokio::sync::mpsc::channel::<Result<_, std::io::Error>>(1);
        let frames = std::sync::Arc::new(tokio::sync::Mutex::new(Some(receive_frame)));
        let upstream = start(Router::new().fallback(move |request: Request| {
            let frames = frames.clone();
            async move {
                assert_eq!(
                    request.uri().path_and_query().unwrap().as_str(),
                    "/module.js?t=42&name=%2F"
                );
                assert_eq!(request.method(), "POST");
                assert_eq!(request.headers()[header::ORIGIN], "http://localhost:7331");
                assert_eq!(request.headers()[header::IF_NONE_MATCH], "\"v1\"");
                assert!(!request.headers().contains_key("x-hop"));
                assert!(
                    request.headers()[header::HOST]
                        .to_str()
                        .unwrap()
                        .starts_with("127.0.0.1:")
                );
                assert_eq!(
                    axum::body::to_bytes(request.into_body(), 100)
                        .await
                        .unwrap(),
                    "request-body"
                );
                let receiver = frames.lock().await.take().unwrap();
                let stream = futures::stream::unfold(receiver, |mut receiver| async {
                    receiver.recv().await.map(|frame| (frame, receiver))
                });
                Response::builder()
                    .status(StatusCode::ACCEPTED)
                    .header(header::CONTENT_TYPE, "application/javascript")
                    .header(header::ETAG, "\"v2\"")
                    .header(header::CONNECTION, "x-upstream-hop")
                    .header("x-upstream-hop", "discard")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        }))
        .await;
        let proxy = DevProxy::new(&upstream.origin);
        let response = proxy
            .forward(
                Request::builder()
                    .method("POST")
                    .uri("/module.js?t=42&name=%2F")
                    .header(header::HOST, "localhost:7331")
                    .header(header::ORIGIN, "http://localhost:7331")
                    .header(header::IF_NONE_MATCH, "\"v1\"")
                    .header(header::CONNECTION, "x-hop")
                    .header("x-hop", "discard")
                    .body(Body::from("request-body"))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/javascript"
        );
        assert_eq!(response.headers()[header::ETAG], "\"v2\"");
        assert!(!response.headers().contains_key("x-upstream-hop"));
        let mut body = response.into_body().into_data_stream();
        send_frame
            .send(Ok(axum::body::Bytes::from_static(b"first")))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), body.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "first"
        );
        send_frame
            .send(Ok(axum::body::Bytes::from_static(b"second")))
            .await
            .unwrap();
        drop(send_frame);
        assert_eq!(body.next().await.unwrap().unwrap(), "second");
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn keeps_redirects_errors_and_html_cache_security_headers_explicit() {
        let upstream = start(Router::new().fallback(|request: Request| async move {
            match request.uri().path() {
                "/redirect" => Response::builder()
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header(header::LOCATION, "/destination")
                    .body(Body::empty())
                    .unwrap(),
                "/missing" => (StatusCode::NOT_FOUND, "missing module").into_response(),
                "/cached" => Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, "\"same\"")
                    .body(Body::empty())
                    .unwrap(),
                _ => Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header(header::CONTENT_SECURITY_POLICY, "script-src 'self'")
                    .header(header::CACHE_CONTROL, "max-age=3600")
                    .body(Body::from("<html>dev</html>"))
                    .unwrap(),
            }
        }))
        .await;
        let proxy = DevProxy::new(&upstream.origin);
        for (path, status) in [
            ("/redirect", StatusCode::TEMPORARY_REDIRECT),
            ("/missing", StatusCode::NOT_FOUND),
            ("/cached", StatusCode::NOT_MODIFIED),
        ] {
            let response = proxy
                .forward(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await;
            assert_eq!(response.status(), status);
            if path == "/redirect" {
                assert_eq!(response.headers()[header::LOCATION], "/destination");
            }
            if path == "/cached" {
                assert_eq!(response.headers()[header::ETAG], "\"same\"");
            }
        }
        let html = proxy.forward(Request::new(Body::empty())).await;
        assert_eq!(html.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(html.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(
            html.headers()
                .get_all(header::CONTENT_SECURITY_POLICY)
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["script-src 'self'", "frame-ancestors 'none'"]
        );
        let origin = upstream.origin.clone();
        drop(upstream);
        tokio::task::yield_now().await;
        let response = DevProxy::new(&origin)
            .forward(Request::new(Body::empty()))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            String::from_utf8(
                axum::body::to_bytes(response.into_body(), 1000)
                    .await
                    .unwrap()
                    .to_vec()
            )
            .unwrap()
            .contains("--dev-server")
        );
    }

    #[tokio::test]
    async fn tunnels_websocket_hmr_messages_and_subprotocol() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_origin = format!("http://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(socket, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.uri().path_and_query().unwrap().as_str(), "/hmr?token=dev");
                assert_eq!(request.headers()[header::ORIGIN], "http://localhost:7331");
                assert_eq!(request.headers()[header::SEC_WEBSOCKET_PROTOCOL], "vite-hmr");
                response.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static("vite-hmr"));
                Ok(response)
            }).await.unwrap();
            assert_eq!(
                socket.next().await.unwrap().unwrap(),
                Message::Text("ping from browser".into())
            );
            socket
                .send(Message::Text("update from vite".into()))
                .await
                .unwrap();
            socket.close(None).await.unwrap();
        });
        let proxy = DevProxy::new(&upstream_origin);
        let frontend = start(Router::new().fallback(move |request: Request| {
            let proxy = proxy.clone();
            async move { proxy.forward(request).await }
        }))
        .await;
        let mut request = format!(
            "{}/hmr?token=dev",
            frontend.origin.replacen("http", "ws", 1)
        )
        .into_client_request()
        .unwrap();
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:7331"),
        );
        request.headers_mut().insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("vite-hmr"),
        );
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(
            response.headers()[header::SEC_WEBSOCKET_PROTOCOL],
            "vite-hmr"
        );
        socket
            .send(Message::Text("ping from browser".into()))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Message::Text("update from vite".into())
        );
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_local_proxy_loops_after_resolving_the_listening_port() {
        let listener = "127.0.0.1:7331".parse().unwrap();
        for origin in [
            "http://127.0.0.1:7331",
            "http://localhost:7331",
            "http://0.0.0.0:7331",
            "http://[::ffff:127.0.0.1]:7331",
        ] {
            assert!(
                reject_self_proxy(origin, listener)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("own listening port")
            );
        }
        assert!(
            reject_self_proxy("http://127.0.0.1:7331", "0.0.0.0:7331".parse().unwrap())
                .await
                .is_err()
        );
        assert!(
            reject_self_proxy("http://[::1]:7331", "[::]:7331".parse().unwrap())
                .await
                .is_err()
        );
        assert!(
            reject_self_proxy("http://127.0.0.1:5173", listener)
                .await
                .is_ok()
        );
        assert!(
            !targets_listener("192.0.2.1".parse().unwrap(), "127.0.0.1".parse().unwrap()).unwrap()
        );
    }
}
