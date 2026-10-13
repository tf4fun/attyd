//! Protocol v2 snapshot reads and the replication stream (protocol §4, §6).
//! Commands arrive in migration step 4; v1 stays unchanged until step 7.
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::BridgeHub;
use crate::bridge::ReplicationSource;
use crate::protocol::hub::{ScopeRequest, parse_event_id};
use crate::protocol::{ErrorBody, ErrorCode, ProtocolError};

#[derive(Clone)]
enum Sources {
    Hub(Arc<BridgeHub>),
    #[cfg(test)]
    Fixed(ReplicationSource, Arc<AtomicU64>),
}

/// Where the v2 routes find the current generation's state.
#[derive(Clone)]
pub(crate) struct V2State(Sources);

impl V2State {
    pub(super) fn hub(hub: Arc<BridgeHub>) -> Self {
        Self(Sources::Hub(hub))
    }

    /// Routes over one bridge generation, for tests that drive it directly.
    #[cfg(test)]
    pub(crate) fn fixed(source: ReplicationSource) -> Self {
        Self(Sources::Fixed(source, Arc::new(AtomicU64::new(1))))
    }

    /// Reads, like the v1 global stream, start the Agent connection when
    /// none is running.
    async fn source(&self) -> Option<ReplicationSource> {
        match &self.0 {
            Sources::Hub(hub) => {
                hub.ensure_runtime().await;
                hub.state.lock().await.replication.clone()
            }
            #[cfg(test)]
            Sources::Fixed(source, _) => Some(source.clone()),
        }
    }

    /// Observer IDs share one space with v1 observers of the same session.
    fn observer_id(&self) -> u64 {
        match &self.0 {
            Sources::Hub(hub) => hub.next_subscriber_id.fetch_add(1, Ordering::Relaxed),
            #[cfg(test)]
            Sources::Fixed(_, ids) => ids.fetch_add(1, Ordering::Relaxed),
        }
    }
}

pub(crate) fn router(state: V2State) -> Router {
    Router::new()
        .route("/api/v2/global", get(global))
        .route("/api/v2/sessions/{session_id}", get(session))
        .route("/api/v2/stream", get(stream))
        .with_state(state)
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    let status = StatusCode::from_u16(code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        axum::Json(ErrorBody {
            error: ProtocolError::new(code, message),
        }),
    )
        .into_response()
}

fn unavailable() -> Response {
    error(ErrorCode::Unavailable, "no Agent connection in this epoch")
}

async fn global(State(state): State<V2State>) -> Response {
    let Some(source) = state.source().await else {
        return unavailable();
    };
    match source.global().await {
        Some(snapshot) => axum::Json(snapshot).into_response(),
        None => unavailable(),
    }
}

/// A held session's document. Reading never materializes a session.
async fn session(Path(session_id): Path<String>, State(state): State<V2State>) -> Response {
    if session_id.is_empty() {
        return error(ErrorCode::InvalidRequest, "session ID must not be empty");
    }
    let Some(source) = state.source().await else {
        return unavailable();
    };
    match source.session(&session_id).await {
        Some(Some(snapshot)) => axum::Json(snapshot).into_response(),
        Some(None) => error(
            ErrorCode::NotFound,
            format!("session {session_id} is not held"),
        ),
        None => unavailable(),
    }
}

/// `scope=global` and `scope=session/<id>@<epoch>:<incarnation>`, each at
/// most once per scope.
fn stream_scopes(query: Option<&str>) -> Result<Vec<ScopeRequest>, String> {
    let mut scopes = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key != "scope" {
            return Err(format!("unknown stream parameter {key}"));
        }
        let request = ScopeRequest::parse(&value).ok_or_else(|| {
            format!("invalid scope {value}: expected global or session/<id>@<epoch>:<incarnation>")
        })?;
        match scopes.insert(request.scope(), request.clone()) {
            Some(previous) if previous != request => {
                return Err(format!(
                    "scope {} names two owners in one stream",
                    request.scope()
                ));
            }
            _ => {}
        }
    }
    if scopes.is_empty() {
        return Err("a stream needs at least one scope".into());
    }
    Ok(scopes.into_values().collect())
}

async fn stream(
    State(state): State<V2State>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let scopes = match stream_scopes(query.as_deref()) {
        Ok(scopes) => scopes,
        Err(message) => return error(ErrorCode::InvalidRequest, message),
    };
    // A malformed or foreign Last-Event-ID starts every scope fresh.
    let last_event = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_event_id);
    let Some(source) = state.source().await else {
        return unavailable();
    };
    let Some(subscription) = source
        .subscribe(scopes, last_event, state.observer_id())
        .await
    else {
        return unavailable();
    };
    let stream = futures::stream::unfold(subscription, |subscription| async move {
        let message = subscription.queue.next().await?;
        Some((
            Ok::<_, Infallible>(SseEvent::default().id(message.id).data(message.data)),
            subscription,
        ))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn production_global_read_preserves_an_ended_generation() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use clap::Parser;
        use tower::ServiceExt;

        let directory = tempfile::tempdir().unwrap();
        let missing_agent = directory.path().join("agent-that-does-not-exist");
        let options = crate::options::Options::try_parse_from([
            "attyd",
            "--cwd",
            directory.path().to_str().unwrap(),
            "--",
            missing_agent.to_str().unwrap(),
        ])
        .unwrap();
        let hub = BridgeHub::new(Arc::new(options));
        hub.ensure_runtime().await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while hub.state.lock().await.input.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the unavailable Agent must finish its generation");
        let (generation, final_source) = {
            let state = hub.state.lock().await;
            (state.generation, state.replication.clone().unwrap())
        };
        let final_snapshot = final_source.global().await.unwrap();
        assert_eq!(
            final_snapshot.state.agent.phase,
            crate::protocol::AgentPhase::Error
        );

        let response = router(V2State::hub(hub.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/v2/global")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let after_generation = hub.state.lock().await.generation;
        hub.shutdown().await;

        assert_eq!(
            after_generation, generation,
            "reading final global must not restart the Agent: status={status}, body={document}"
        );
        assert_eq!(status, StatusCode::OK);
        assert_eq!(document, serde_json::to_value(final_snapshot).unwrap());
    }

    #[test]
    fn stream_scopes_name_complete_owners_once() {
        let scopes = stream_scopes(Some(
            "scope=global&scope=session%2Fa%40b%40e-1%3A3&scope=global",
        ))
        .unwrap();
        assert_eq!(
            scopes,
            [
                ScopeRequest::Global,
                ScopeRequest::Session {
                    session_id: "a@b".into(),
                    epoch: "e-1".into(),
                    incarnation: 3,
                },
            ]
        );
        for invalid in [
            None,
            Some(""),
            Some("scope=session/a"),
            Some("scope=session/a@e"),
            Some("scope=session/a@e:0"),
            Some("scope=session/@e:1"),
            Some("scope=other"),
            Some("scope=global&cursor=1"),
            Some("scope=session/a@e:1&scope=session/a@e:2"),
        ] {
            assert!(stream_scopes(invalid).is_err(), "{invalid:?}");
        }
    }
}
