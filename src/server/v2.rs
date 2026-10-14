//! Protocol v2 snapshot reads, the replication stream, session commands and
//! the immutable and workspace reads (protocol §4–§7). v1 stays unchanged
//! until migration step 7.
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::BridgeHub;
use crate::bridge::ReplicationSource;
use crate::bridge::commands::{
    GlobalCommand, HeldCommand, Outcome, OwnerTag, TerminalControl, valid_intent_key,
};
use crate::protocol::hub::{ScopeRequest, parse_event_id};
use crate::protocol::{
    AuthTerminalInputBody, AuthTerminalStartBody, AuthenticateBody, EmptyBody, ErrorBody,
    ErrorCode, InteractionResponseBody, ProtocolError, SessionCwdBody, SetConfigBody, SetModeBody,
    TerminalSizeBody, TurnBody,
};

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

    /// The current generation, or the last one after it ended. Reads never
    /// start a connection: an ended epoch's final `global` stays readable.
    async fn source(&self) -> Option<ReplicationSource> {
        match &self.0 {
            Sources::Hub(hub) => hub.state.lock().await.replication.clone(),
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
        .route("/api/v2/catalog", get(catalog))
        .route("/api/v2/agent/authenticate", post(authenticate))
        .route("/api/v2/agent/logout", post(logout))
        .route("/api/v2/agent/auth-terminals", post(start_auth_terminal))
        .route(
            "/api/v2/agent/auth-terminals/{request_id}/{control}",
            post(auth_terminal_control),
        )
        .route("/api/v2/sessions", post(create_session))
        .route(
            "/api/v2/url-flows/{interaction_id}",
            delete(acknowledge_global_url_flow),
        )
        .route(
            "/api/v2/sessions/{session_id}",
            get(session).delete(delete_session),
        )
        .route("/api/v2/sessions/{session_id}/open", post(open))
        .route("/api/v2/sessions/{session_id}/turns", post(start_turn))
        .route(
            "/api/v2/sessions/{session_id}/turns/{operation_id}/cancel",
            post(cancel_turn),
        )
        .route("/api/v2/sessions/{session_id}/mode", put(set_mode))
        .route(
            "/api/v2/sessions/{session_id}/config/{config_id}",
            put(set_config),
        )
        .route("/api/v2/sessions/{session_id}/fork", post(fork))
        .route("/api/v2/sessions/{session_id}/close", post(close))
        .route(
            "/api/v2/sessions/{session_id}/url-flows/{interaction_id}",
            delete(acknowledge_url_flow),
        )
        .route(
            "/api/v2/sessions/{session_id}/history/{history_revision}/turns/{turn_id}/process",
            get(process_page),
        )
        .route(
            "/api/v2/sessions/{session_id}/attachments/{attachment_id}",
            get(attachment),
        )
        .route("/api/v2/sessions/{session_id}/files", get(search_files))
        .route(
            "/api/v2/sessions/{session_id}/files/content",
            get(read_file),
        )
        .route("/api/v2/interactions/{interaction_id}", post(respond))
        .route("/api/v2/requests/{request_id}", delete(acknowledge_request))
        .route("/api/v2/stream", get(stream))
        .with_state(state)
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    rejected(ProtocolError::new(code, message))
}

fn rejected(error: ProtocolError) -> Response {
    let status =
        StatusCode::from_u16(error.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(ErrorBody { error })).into_response()
}

fn answered<T: serde::Serialize>(outcome: Outcome<T>) -> Response {
    match outcome {
        Ok(result) => axum::Json(result).into_response(),
        Err(error) => rejected(error),
    }
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidRequest, message)
}

/// The `Idempotency-Key` every operation command requires (protocol §5.2).
fn intent_key(headers: &HeaderMap) -> Result<String, ProtocolError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Err(ProtocolError::new(
            ErrorCode::PreconditionRequired,
            "operation commands require Idempotency-Key",
        ));
    };
    value
        .to_str()
        .ok()
        .filter(|key| valid_intent_key(key))
        .map(str::to_string)
        .ok_or_else(|| invalid("Idempotency-Key must be 1 to 200 visible ASCII characters"))
}

/// The `If-Match` owner tag (protocol §5.3).
fn owner_tag(headers: &HeaderMap) -> Result<OwnerTag, ProtocolError> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Err(ProtocolError::new(
            ErrorCode::PreconditionRequired,
            "session commands require If-Match with the owner tag",
        ));
    };
    value
        .to_str()
        .ok()
        .and_then(OwnerTag::parse)
        .ok_or_else(|| invalid("If-Match must be a strong owner tag"))
}

/// A JSON body; commands without parameters also accept an empty one.
fn body<T: DeserializeOwned>(bytes: &Bytes) -> Result<T, ProtocolError> {
    let bytes: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        bytes
    };
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("invalid body: {error}")))
}

fn identifier(value: &str, what: &str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        Err(invalid(format!("{what} must not be empty")))
    } else {
        Ok(())
    }
}

async fn live(state: &V2State) -> Result<ReplicationSource, ProtocolError> {
    state.source().await.ok_or_else(|| {
        ProtocolError::new(ErrorCode::Unavailable, "no Agent connection in this epoch")
    })
}

/// Runs a handler body; any rejection becomes its error response.
async fn run(body: impl std::future::Future<Output = Result<Response, ProtocolError>>) -> Response {
    body.await.unwrap_or_else(rejected)
}

async fn held(
    state: V2State,
    session_id: String,
    headers: HeaderMap,
    command: HeldCommand,
) -> Result<Response, ProtocolError> {
    identifier(&session_id, "session ID")?;
    let key = intent_key(&headers)?;
    let tag = owner_tag(&headers)?;
    let source = live(&state).await?;
    Ok(answered(
        source.held_command(key, session_id, tag, command).await,
    ))
}

async fn set_mode(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let SetModeBody { mode_id } = body(&bytes)?;
        identifier(&mode_id, "mode ID")?;
        held(state, session_id, headers, HeldCommand::SetMode { mode_id }).await
    })
    .await
}

async fn set_config(
    State(state): State<V2State>,
    Path((session_id, config_id)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let SetConfigBody { value } = body(&bytes)?;
        identifier(&config_id, "config ID")?;
        if !(value.is_string() || value.is_boolean()) {
            return Err(invalid("a config value is a string or a boolean"));
        }
        held(
            state,
            session_id,
            headers,
            HeldCommand::SetConfig { config_id, value },
        )
        .await
    })
    .await
}

async fn fork(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let EmptyBody {} = body(&bytes)?;
        held(state, session_id, headers, HeldCommand::Fork).await
    })
    .await
}

async fn close(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let EmptyBody {} = body(&bytes)?;
        held(state, session_id, headers, HeldCommand::Close).await
    })
    .await
}

async fn delete_session(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    run(async move {
        let tag = owner_tag(&headers)?;
        if tag.incarnation == Some(0) {
            // `"<epoch>:0"`: the bridge holds no incarnation (protocol §5.3).
            identifier(&session_id, "session ID")?;
            let key = intent_key(&headers)?;
            let source = live(&state).await?;
            return Ok(answered(source.cold_delete(key, session_id, tag).await));
        }
        held(state, session_id, headers, HeldCommand::Delete).await
    })
    .await
}

/// A global operation command: no owner tag (protocol §5.3).
async fn global_command(
    state: V2State,
    headers: HeaderMap,
    command: GlobalCommand,
) -> Result<Response, ProtocolError> {
    let key = intent_key(&headers)?;
    let source = live(&state).await?;
    Ok(answered(source.global_command(key, command).await))
}

async fn authenticate(State(state): State<V2State>, headers: HeaderMap, bytes: Bytes) -> Response {
    run(async move {
        let AuthenticateBody { method_id } = body(&bytes)?;
        identifier(&method_id, "method ID")?;
        global_command(state, headers, GlobalCommand::Authenticate { method_id }).await
    })
    .await
}

async fn logout(State(state): State<V2State>, headers: HeaderMap, bytes: Bytes) -> Response {
    run(async move {
        let EmptyBody {} = body(&bytes)?;
        global_command(state, headers, GlobalCommand::Logout).await
    })
    .await
}

async fn start_auth_terminal(
    State(state): State<V2State>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let AuthTerminalStartBody {
            method_id,
            cols,
            rows,
        } = body(&bytes)?;
        identifier(&method_id, "method ID")?;
        global_command(
            state,
            headers,
            GlobalCommand::AuthTerminal {
                method_id,
                cols,
                rows,
            },
        )
        .await
    })
    .await
}

async fn auth_terminal_control(
    State(state): State<V2State>,
    Path((request_id, control)): Path<(String, String)>,
    bytes: Bytes,
) -> Response {
    run(async move {
        identifier(&request_id, "request ID")?;
        let control = match control.as_str() {
            "input" => {
                let AuthTerminalInputBody { data } = body(&bytes)?;
                TerminalControl::Input { data }
            }
            "resize" => {
                let TerminalSizeBody { cols, rows } = body(&bytes)?;
                TerminalControl::Resize { cols, rows }
            }
            "cancel" => {
                let EmptyBody {} = body(&bytes)?;
                TerminalControl::Cancel
            }
            _ => {
                return Err(ProtocolError::new(
                    ErrorCode::NotFound,
                    format!("unknown auth terminal control: {control}"),
                ));
            }
        };
        let source = live(&state).await?;
        Ok(answered(
            source.auth_terminal_control(request_id, control).await,
        ))
    })
    .await
}

async fn create_session(
    State(state): State<V2State>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        let SessionCwdBody { cwd } = body(&bytes)?;
        identifier(&cwd, "cwd")?;
        global_command(state, headers, GlobalCommand::Create { cwd }).await
    })
    .await
}

async fn acknowledge_global_url_flow(
    State(state): State<V2State>,
    Path(interaction_id): Path<String>,
) -> Response {
    run(async move {
        identifier(&interaction_id, "interaction ID")?;
        let source = live(&state).await?;
        Ok(answered(
            source.acknowledge_global_url_flow(interaction_id).await,
        ))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogQuery {
    cursor: Option<String>,
    revision: Option<String>,
}

/// `GET /api/v2/catalog?cursor=&revision=`: the Agent's paged
/// `session/list`; `revision` is `global.catalog.revision` (protocol §6).
async fn catalog(
    State(state): State<V2State>,
    query: Result<Query<CatalogQuery>, QueryRejection>,
) -> Response {
    run(async move {
        let Query(CatalogQuery { cursor, revision }) =
            query.map_err(|error| invalid(format!("invalid query: {error}")))?;
        let cursor = cursor.filter(|cursor| !cursor.is_empty());
        let revision = revision.filter(|revision| !revision.is_empty());
        let source = live(&state).await?;
        Ok(axum::Json(source.catalog(cursor, revision).await?).into_response())
    })
    .await
}

async fn open(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        let key = intent_key(&headers)?;
        let SessionCwdBody { cwd } = body(&bytes)?;
        identifier(&cwd, "cwd")?;
        let source = live(&state).await?;
        Ok(answered(source.open(key, session_id, cwd).await))
    })
    .await
}

async fn start_turn(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        let key = intent_key(&headers)?;
        let tag = owner_tag(&headers)?;
        let TurnBody { mut prompt } = body(&bytes)?;
        let source = live(&state).await?;
        resolve_attachments(&source, &session_id, &tag, &mut prompt).await?;
        Ok(answered(
            source.start_turn(key, session_id, tag, prompt).await,
        ))
    })
    .await
}

/// Browser history carries attachment references. A resent prompt gets the
/// original blocks back from the owner it names before admission, as v1 does.
async fn resolve_attachments(
    source: &ReplicationSource,
    session_id: &str,
    tag: &OwnerTag,
    prompt: &mut [Value],
) -> Result<(), ProtocolError> {
    let mut view = None;
    for block in prompt.iter_mut() {
        if block
            .get("_meta")
            .and_then(|meta| meta.get(crate::attachments::REFERENCE_KEY))
            .is_none()
        {
            continue;
        }
        let reference = crate::attachments::reference(block)
            .ok_or_else(|| invalid("invalid attachment reference"))?;
        let owner = reference.owner();
        if owner.session_id != session_id
            || owner.epoch != tag.epoch
            || Some(owner.incarnation) != tag.incarnation
        {
            return Err(invalid(
                "attachment reference belongs to another session owner",
            ));
        }
        if view.is_none() {
            view = Some(source.owner_view(session_id, Some(tag)).await?);
        }
        *block = crate::attachments::find(view.as_mut().expect("read above"), &reference.id)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::NotFound, "the attachment is no longer available")
            })?;
    }
    Ok(())
}

async fn cancel_turn(
    State(state): State<V2State>,
    Path((session_id, operation_id)): Path<(String, String)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        identifier(&operation_id, "operation ID")?;
        let tag = owner_tag(&headers)?;
        let EmptyBody {} = body(&bytes)?;
        let source = live(&state).await?;
        Ok(answered(
            source.cancel_turn(session_id, tag, operation_id).await,
        ))
    })
    .await
}

async fn respond(
    State(state): State<V2State>,
    Path(interaction_id): Path<String>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    run(async move {
        identifier(&interaction_id, "interaction ID")?;
        let key = intent_key(&headers)?;
        let tag = owner_tag(&headers)?;
        let response: InteractionResponseBody = body(&bytes)?;
        let source = live(&state).await?;
        Ok(answered(
            source.respond(key, interaction_id, tag, response).await,
        ))
    })
    .await
}

async fn acknowledge_request(
    State(state): State<V2State>,
    Path(request_id): Path<String>,
) -> Response {
    run(async move {
        identifier(&request_id, "request ID")?;
        let source = live(&state).await?;
        Ok(answered(source.acknowledge_request(request_id).await))
    })
    .await
}

async fn acknowledge_url_flow(
    State(state): State<V2State>,
    Path((session_id, interaction_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        identifier(&interaction_id, "interaction ID")?;
        let tag = owner_tag(&headers)?;
        let source = live(&state).await?;
        Ok(answered(
            source
                .acknowledge_url_flow(session_id, tag, interaction_id)
                .await,
        ))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    #[serde(default)]
    offset: usize,
}

/// An immutable process page, addressed by `(historyRevision, turnId)`.
async fn process_page(
    State(state): State<V2State>,
    Path((session_id, history_revision, turn_id)): Path<(String, String, String)>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        let Ok(Query(PageQuery { offset })) = query else {
            return Err(invalid("invalid process page query"));
        };
        let source = live(&state).await?;
        let raw = source
            .owner_view(&session_id, None)
            .await
            .map_err(|error| match error.code {
                // The revision belonged to an owner that is gone.
                ErrorCode::NotFound => ProtocolError::new(ErrorCode::Gone, error.message),
                _ => error,
            })?;
        let mut view =
            super::business_session_view(&raw).ok_or_else(|| invalid("the session has no view"))?;
        if view["historyRevision"] == history_revision.as_str() {
            let current = view["terminals"].take();
            view["terminals"] = source
                .process_terminals(&session_id, &history_revision, current)
                .await?;
        }
        use crate::session_presentation::PageError;
        let mut page =
            crate::session_presentation::process_page(&view, &turn_id, &history_revision, offset)
                .map_err(|error| match error {
                PageError::StaleHistory => ProtocolError::new(
                    ErrorCode::Gone,
                    "the history revision is no longer retained",
                ),
                PageError::TurnNotFound => {
                    ProtocolError::new(ErrorCode::NotFound, "no such completed turn")
                }
                PageError::InvalidOffset => invalid("invalid process page offset"),
            })?;
        crate::attachments::project(&mut page);
        let mut response = axum::Json(page).into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("private, max-age=31536000, immutable"),
        );
        Ok(response)
    })
    .await
}

/// An attachment of exactly the owner `If-Match` names.
async fn attachment(
    State(state): State<V2State>,
    Path((session_id, attachment_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        if attachment_id.len() != 64 || !attachment_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("invalid attachment reference"));
        }
        let tag = owner_tag(&headers)?;
        let source = live(&state).await?;
        let view = source.owner_view(&session_id, Some(&tag)).await?;
        let response = super::attachment_response(view, &attachment_id);
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ProtocolError::new(
                ErrorCode::NotFound,
                "the attachment is no longer available",
            ));
        }
        Ok(response)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FilesQuery {
    #[serde(default)]
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQuery {
    path: String,
}

async fn search_files(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    query: Result<Query<FilesQuery>, QueryRejection>,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        let Ok(Query(FilesQuery { query })) = query else {
            return Err(invalid("invalid files query"));
        };
        let source = live(&state).await?;
        let found = source
            .read(json!({
                "type": "context/search",
                "requestId": format!("v2-files-{}", uuid::Uuid::new_v4()),
                "sessionId": session_id,
                "query": query,
            }))
            .await?;
        Ok(axum::Json(found).into_response())
    })
    .await
}

async fn read_file(
    State(state): State<V2State>,
    Path(session_id): Path<String>,
    query: Result<Query<FileQuery>, QueryRejection>,
) -> Response {
    run(async move {
        identifier(&session_id, "session ID")?;
        let Ok(Query(FileQuery { path })) = query else {
            return Err(invalid("files/content needs a path"));
        };
        identifier(&path, "path")?;
        let source = live(&state).await?;
        let content = source
            .read(json!({
                "type": "context/read",
                "requestId": format!("v2-file-{}", uuid::Uuid::new_v4()),
                "sessionId": session_id,
                "path": path,
            }))
            .await?;
        Ok(axum::Json(content).into_response())
    })
    .await
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
    // The bridge manages its Agent connection: once an epoch has ended, the
    // next subscriber gets a new one, as v1's global stream does. Reads never
    // start one.
    if let Sources::Hub(hub) = &state.0 {
        hub.ensure_runtime().await;
    }
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
        let mut event = SseEvent::default().data(message.data);
        if !message.id.is_empty() {
            event = event.id(message.id);
        }
        Some((Ok::<_, Infallible>(event), subscription))
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

    #[tokio::test]
    async fn a_stream_after_the_epoch_ended_starts_the_next_connection() {
        use axum::body::Body;
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
        let ended = || async {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while hub.state.lock().await.input.is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the unavailable Agent must finish its generation");
        };
        hub.ensure_runtime().await;
        ended().await;
        let generation = hub.state.lock().await.generation;
        // The bridge owns the Agent connection: a client only resumes its
        // stream, and the hub starts the next epoch for it.
        let response = router(V2State::hub(hub.clone()))
            .oneshot(
                Request::builder()
                    .uri("/api/v2/stream?scope=global")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The new Agent may already have failed again; the restart is the point.
        assert_eq!(hub.state.lock().await.generation, generation + 1);
        drop(response);
        ended().await;
        hub.shutdown().await;
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
