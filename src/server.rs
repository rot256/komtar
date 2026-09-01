use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    convert::Infallible,
    error::Error,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode, Uri,
    body::{Frame, Incoming},
    header::{
        self, ALLOW, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST, LOCATION,
    },
    service::service_fn,
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_staticfile::{ResolveResult, Resolver, ResponseBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use serde::Serialize;
use tokio::{
    io::copy_bidirectional,
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::{broadcast, watch},
    time::{Instant, interval_at},
};
use url::Url;

use crate::{
    BoxError,
    agent::{AgentEvent, AgentHub},
    fifo::CommentQueue,
    live::ReloadState,
    model::{CommentDraft, CommentRecord, MAX_REQUEST_BYTES, RequestError},
};

pub(crate) const RESERVED_PREFIX: &str = "/_komtar/";
const CLIENT_PATH: &str = "/_komtar/client.js";
const STATUS_PATH: &str = "/_komtar/api/status";
const COMMENTS_PATH: &str = "/_komtar/api/comments";
const MESSAGES_PATH: &str = "/_komtar/api/messages";
const RELOAD_PATH: &str = "/_komtar/api/reload";
const MAX_HTML_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const EDIT_SCRIPT: &[u8] = b"\n<script type=\"module\" src=\"/_komtar/client.js\"></script>\n";
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

type AppBody = UnsyncBoxBody<Bytes, BoxError>;
type HttpClient = Client<HttpsConnector<HttpConnector>, AppBody>;

#[derive(Clone)]
pub(crate) struct ServerState {
    queue: CommentQueue,
    messages: AgentHub,
    source: SourceState,
}

#[derive(Clone)]
enum SourceState {
    Proxy {
        upstream: Url,
        client: HttpClient,
    },
    Live {
        root: PathBuf,
        transport_paths: Arc<[PathBuf]>,
        resolver: Resolver,
        reload: ReloadState,
    },
}

#[derive(Clone, Copy)]
enum ClientMode<'a> {
    Edit,
    Live(&'a str),
}

impl ServerState {
    pub(crate) fn proxy(upstream: Url, queue: CommentQueue, messages: AgentHub) -> Self {
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            queue,
            messages,
            source: SourceState::Proxy { upstream, client },
        }
    }

    pub(crate) fn live(
        root: PathBuf,
        transport_paths: Vec<PathBuf>,
        queue: CommentQueue,
        messages: AgentHub,
        reload: ReloadState,
    ) -> Self {
        Self {
            queue,
            messages,
            source: SourceState::Live {
                resolver: Resolver::new(root.clone()),
                root,
                transport_paths: transport_paths.into(),
                reload,
            },
        }
    }
}

pub(crate) async fn serve(listener: TcpListener, state: ServerState) -> Result<(), BoxError> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                let connection_state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        handle(request, connection_state.clone())
                    });
                    let connection = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades();
                    if let Err(error) = connection.await {
                        tracing::debug!(%peer, %error, "HTTP connection ended with an error");
                    }
                });
            }
            signal = &mut shutdown => {
                signal?;
                return Ok(());
            }
        }
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut quit = signal(SignalKind::quit())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        signal = terminate.recv() => received_shutdown_signal(signal),
        signal = hangup.recv() => received_shutdown_signal(signal),
        signal = quit.recv() => received_shutdown_signal(signal),
    }
}

fn received_shutdown_signal(signal: Option<()>) -> std::io::Result<()> {
    signal.ok_or_else(|| std::io::Error::other("shutdown signal handler stopped"))
}

async fn handle(
    request: Request<Incoming>,
    state: ServerState,
) -> Result<Response<AppBody>, Infallible> {
    let path = request.uri().path();
    let result = if path.starts_with(RESERVED_PREFIX) {
        handle_reserved(request, &state).await
    } else {
        match &state.source {
            SourceState::Proxy { upstream, client } => {
                proxy_request(request, client, upstream).await
            }
            SourceState::Live {
                root,
                transport_paths,
                resolver,
                reload,
            } => {
                let revision = reload.current();
                static_request(request, root, transport_paths, resolver, &revision).await
            }
        }
    };

    Ok(result.unwrap_or_else(|error| {
        tracing::warn!(status = %error.status, error = %error.message, "request rejected");
        request_error_response(error)
    }))
}

async fn handle_reserved(
    request: Request<Incoming>,
    state: &ServerState,
) -> Result<Response<AppBody>, RequestError> {
    let path = request.uri().path().to_owned();
    let result = match path.as_str() {
        CLIENT_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET"))
            } else {
                Ok(javascript_response(crate::CLIENT_JS))
            }
        }
        STATUS_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET"))
            } else {
                Ok(json_response(
                    StatusCode::OK,
                    &PendingResponse {
                        pending: state.queue.pending(),
                    },
                ))
            }
        }
        COMMENTS_PATH => {
            if request.method() != Method::POST {
                Err(RequestError::method_not_allowed("POST"))
            } else {
                read_draft(request)
                    .await
                    .and_then(CommentDraft::validate)
                    .and_then(|draft| {
                        state
                            .queue
                            .enqueue(CommentRecord::from(draft))
                            .map(|pending| {
                                json_response(StatusCode::ACCEPTED, &PendingResponse { pending })
                            })
                    })
            }
        }
        MESSAGES_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET"))
            } else {
                Ok(agent_message_response(state.messages.clone()))
            }
        }
        RELOAD_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET"))
            } else {
                match &state.source {
                    SourceState::Proxy { .. } => Err(RequestError::new(
                        StatusCode::NOT_FOUND,
                        "reserved komtar resource not found",
                    )),
                    SourceState::Live { reload, .. } => Ok(reload_response(reload.subscribe())),
                }
            }
        }
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            &ErrorResponse {
                error: "reserved komtar resource not found",
            },
        )),
    };
    Ok(result.unwrap_or_else(request_error_response))
}

async fn read_draft(request: Request<Incoming>) -> Result<CommentDraft, RequestError> {
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some("application/json") {
        return Err(RequestError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json",
        ));
    }
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_REQUEST_BYTES)
    {
        return Err(RequestError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body is too large",
        ));
    }

    let collected = Limited::new(request.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
        .map_err(|_| {
            RequestError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large")
        })?;
    serde_json::from_slice(&collected.to_bytes())
        .map_err(|_| RequestError::new(StatusCode::BAD_REQUEST, "request body must be valid JSON"))
}

async fn proxy_request(
    mut request: Request<Incoming>,
    client: &HttpClient,
    upstream: &Url,
) -> Result<Response<AppBody>, RequestError> {
    let downstream_upgrade = if request.headers().contains_key(header::UPGRADE) {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };
    let original_host = request.headers().get(HOST).cloned();
    let (mut parts, body) = request.into_parts();
    let request_method = parts.method.clone();
    parts.uri = upstream_uri(upstream, &parts.uri)?;
    parts.headers.insert(
        HOST,
        header::HeaderValue::from_str(upstream.authority())
            .map_err(|_| RequestError::new(StatusCode::BAD_GATEWAY, "invalid upstream host"))?,
    );
    parts.headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    if let Some(host) = original_host {
        parts
            .headers
            .insert(header::HeaderName::from_static("x-forwarded-host"), host);
    }
    parts.headers.insert(
        header::HeaderName::from_static("x-forwarded-proto"),
        header::HeaderValue::from_static("http"),
    );

    let outgoing = Request::from_parts(parts, incoming_body(body));
    let mut response = client.request(outgoing).await.map_err(|error| {
        tracing::warn!(%error, "upstream request failed");
        RequestError::new(StatusCode::BAD_GATEWAY, "upstream request failed")
    })?;

    rewrite_redirect(response.headers_mut(), upstream);

    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(downstream_upgrade) = downstream_upgrade {
            let upstream_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                match tokio::try_join!(downstream_upgrade, upstream_upgrade) {
                    Ok((downstream, upstream)) => {
                        let mut downstream = TokioIo::new(downstream);
                        let mut upstream = TokioIo::new(upstream);
                        if let Err(error) = copy_bidirectional(&mut downstream, &mut upstream).await
                        {
                            tracing::debug!(%error, "WebSocket tunnel closed with an error");
                        }
                    }
                    Err(error) => tracing::debug!(%error, "WebSocket upgrade failed"),
                }
            });
        }
        return Ok(map_incoming_response(response));
    }

    maybe_inject_client(
        map_incoming_response(response),
        &request_method,
        ClientMode::Edit,
    )
    .await
}

async fn static_request(
    request: Request<Incoming>,
    root: &Path,
    transport_paths: &[PathBuf],
    resolver: &Resolver,
    revision: &str,
) -> Result<Response<AppBody>, RequestError> {
    let request_method = request.method().clone();
    if !matches!(request_method, Method::GET | Method::HEAD) {
        return Err(RequestError::method_not_allowed("GET, HEAD"));
    }
    match static_path_access(request.uri().path(), root, transport_paths).await? {
        StaticPathAccess::Serve => {}
        StaticPathAccess::Reject(status) => return Ok(empty_response(status)),
    }
    let resolved = resolver.resolve_request(&request).await.map_err(|error| {
        tracing::warn!(%error, "could not resolve static file");
        RequestError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read served directory",
        )
    })?;
    let resolved = match resolved {
        ResolveResult::MethodNotMatched => {
            return Err(RequestError::method_not_allowed("GET, HEAD"));
        }
        ResolveResult::NotFound => ResolveResult::NotFound,
        ResolveResult::PermissionDenied => ResolveResult::PermissionDenied,
        ResolveResult::IsDirectory { redirect_to } => ResolveResult::IsDirectory { redirect_to },
        ResolveResult::Found(file) => match tokio::fs::canonicalize(root.join(&file.path)).await {
            Ok(path) if path.starts_with(root) => ResolveResult::Found(file),
            Ok(path) => {
                tracing::warn!(path = %path.display(), "refused to serve a symlink outside the root");
                ResolveResult::NotFound
            }
            Err(error) if is_missing_path_error(&error) => ResolveResult::NotFound,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                ResolveResult::PermissionDenied
            }
            Err(error) => {
                tracing::warn!(%error, "could not verify resolved static file");
                return Err(RequestError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not read served directory",
                ));
            }
        },
    };
    let resolved_content_type = match &resolved {
        ResolveResult::Found(file) => file.content_type.clone(),
        ResolveResult::MethodNotMatched
        | ResolveResult::NotFound
        | ResolveResult::PermissionDenied
        | ResolveResult::IsDirectory { .. } => None,
    };

    let response = ResponseBuilder::new()
        .request(&request)
        .cache_headers(None)
        .build(resolved)
        .map_err(|error| {
            tracing::warn!(%error, "could not build static file response");
            RequestError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not build static file response",
            )
        })?;
    let mut response = map_static_response(response);
    if let Some(content_type) = resolved_content_type
        && let Ok(value) = header::HeaderValue::from_str(&content_type)
    {
        response.headers_mut().entry(CONTENT_TYPE).or_insert(value);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    maybe_inject_client(response, &request_method, ClientMode::Live(revision)).await
}

enum StaticPathAccess {
    Serve,
    Reject(StatusCode),
}

struct StaticRequestedPath {
    path: PathBuf,
    is_directory: bool,
}

impl StaticRequestedPath {
    fn resolve(request_path: &str) -> Self {
        let decoded = percent_encoding::percent_decode_str(request_path).decode_utf8_lossy();
        let mut path = PathBuf::new();
        for component in Path::new(decoded.as_ref()).components() {
            match component {
                Component::Normal(value) => {
                    if Path::new(value)
                        .components()
                        .all(|nested| matches!(nested, Component::Normal(_)))
                    {
                        path.push(value);
                    }
                }
                Component::ParentDir => {
                    path.pop();
                }
                Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            }
        }
        Self {
            path,
            is_directory: request_path.as_bytes().last() == Some(&b'/'),
        }
    }
}

async fn static_path_access(
    request_path: &str,
    root: &Path,
    transport_paths: &[PathBuf],
) -> Result<StaticPathAccess, RequestError> {
    let requested = StaticRequestedPath::resolve(request_path);
    if has_hidden_component(&requested.path) {
        return Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND));
    }
    let path = match tokio::fs::canonicalize(root.join(&requested.path)).await {
        Ok(path) => path,
        Err(error) => return static_path_error(error, "could not verify requested static path"),
    };
    if !path.starts_with(root) || transport_paths.contains(&path) {
        return Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND));
    }

    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return static_path_error(error, "could not read requested static path metadata");
        }
    };
    if metadata.is_file() && !requested.is_directory {
        return Ok(StaticPathAccess::Serve);
    }
    if !metadata.is_dir() {
        return Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND));
    }
    if !requested.is_directory {
        return Ok(StaticPathAccess::Serve);
    }

    let index = match tokio::fs::canonicalize(path.join("index.html")).await {
        Ok(index) => index,
        Err(error) => return static_path_error(error, "could not verify directory index"),
    };
    if !index.starts_with(root) || transport_paths.contains(&index) {
        return Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND));
    }
    let metadata = match tokio::fs::metadata(index).await {
        Ok(metadata) => metadata,
        Err(error) => {
            return static_path_error(error, "could not read directory index metadata");
        }
    };
    if metadata.is_file() {
        Ok(StaticPathAccess::Serve)
    } else {
        Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND))
    }
}

fn has_hidden_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
}

fn static_path_error(
    error: std::io::Error,
    context: &'static str,
) -> Result<StaticPathAccess, RequestError> {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return Ok(StaticPathAccess::Reject(StatusCode::FORBIDDEN));
    }
    if is_missing_path_error(&error) {
        return Ok(StaticPathAccess::Reject(StatusCode::NOT_FOUND));
    }
    tracing::warn!(%error, %context, "static path access failed");
    Err(RequestError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "could not read served directory",
    ))
}

fn is_missing_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::InvalidInput
    ) || matches!(
        error.raw_os_error(),
        Some(nix::libc::ELOOP) | Some(nix::libc::ENAMETOOLONG)
    )
}

fn upstream_uri(upstream: &Url, incoming: &Uri) -> Result<Uri, RequestError> {
    let mut target = upstream.clone();
    let base = upstream.path().trim_end_matches('/');
    let incoming_path = incoming.path();
    let path = if base.is_empty() {
        incoming_path.to_owned()
    } else {
        format!("{base}{incoming_path}")
    };
    target.set_path(&path);
    target.set_query(incoming.query());
    target
        .as_str()
        .parse()
        .map_err(|_| RequestError::new(StatusCode::BAD_GATEWAY, "could not construct upstream URL"))
}

fn rewrite_redirect(headers: &mut header::HeaderMap, upstream: &Url) {
    let Some(value) = headers.get(LOCATION) else {
        return;
    };
    let Ok(location) = value.to_str() else {
        return;
    };
    let Ok(url) = Url::parse(location) else {
        return;
    };
    if url.origin() != upstream.origin() {
        return;
    }
    let upstream_base = upstream.path().trim_end_matches('/');
    let redirected_path = url.path();
    let proxy_path = if upstream_base.is_empty() {
        redirected_path
    } else if redirected_path == upstream_base {
        "/"
    } else {
        redirected_path
            .strip_prefix(upstream_base)
            .filter(|suffix| suffix.starts_with('/'))
            .unwrap_or(redirected_path)
    };
    let mut rewritten = proxy_path.to_owned();
    if let Some(query) = url.query() {
        rewritten.push('?');
        rewritten.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        rewritten.push('#');
        rewritten.push_str(fragment);
    }
    if let Ok(value) = header::HeaderValue::from_str(&rewritten) {
        headers.insert(LOCATION, value);
    }
}

async fn maybe_inject_client(
    response: Response<AppBody>,
    request_method: &Method,
    mode: ClientMode<'_>,
) -> Result<Response<AppBody>, RequestError> {
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"));
    let is_encoded = response.headers().contains_key(header::CONTENT_ENCODING);
    let status_allows_injection =
        response.status().is_success() && response.status() != StatusCode::PARTIAL_CONTENT;
    if !matches!(*request_method, Method::GET | Method::HEAD)
        || !status_allows_injection
        || !is_html
        || is_encoded
    {
        if is_html && is_encoded {
            tracing::warn!("encoded HTML was not annotated");
        }
        return Ok(response);
    }
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_HTML_RESPONSE_BYTES)
    {
        tracing::warn!(
            limit = MAX_HTML_RESPONSE_BYTES,
            "HTML response is too large to annotate"
        );
        return Ok(response);
    }

    if request_method == Method::HEAD {
        let mut response = response;
        let script = client_script(mode);
        let annotated_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(|length| length.checked_add(script.len()));
        annotate_headers(response.headers_mut(), annotated_length);
        return Ok(response);
    }

    let (mut parts, body) = response.into_parts();
    let body_error_status = match mode {
        ClientMode::Edit => StatusCode::BAD_GATEWAY,
        ClientMode::Live(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let bytes = body.collect().await.map_err(|error| {
        tracing::warn!(%error, "could not read HTML response");
        RequestError::new(body_error_status, "could not read HTML response")
    })?;
    let bytes = bytes.to_bytes();
    if bytes.len() > MAX_HTML_RESPONSE_BYTES {
        tracing::warn!(
            size = bytes.len(),
            limit = MAX_HTML_RESPONSE_BYTES,
            "HTML response is too large to annotate"
        );
        return Ok(Response::from_parts(parts, full_body(bytes)));
    }

    let bytes = inject_client(&bytes, mode);
    annotate_headers(&mut parts.headers, Some(bytes.len()));
    Ok(Response::from_parts(parts, full_body(bytes)))
}

fn annotate_headers(headers: &mut header::HeaderMap, content_length: Option<usize>) {
    headers.remove(CONTENT_LENGTH);
    if let Some(content_length) = content_length
        && let Ok(value) = header::HeaderValue::from_str(&content_length.to_string())
    {
        headers.insert(CONTENT_LENGTH, value);
    }
    headers.remove(ETAG);
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::TRANSFER_ENCODING);
    headers.insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
}

fn client_script(mode: ClientMode<'_>) -> Cow<'_, [u8]> {
    match mode {
        ClientMode::Edit => Cow::Borrowed(EDIT_SCRIPT),
        ClientMode::Live(revision) => Cow::Owned(
            format!(
                "\n<script type=\"module\" src=\"/_komtar/client.js?live={revision}\"></script>\n"
            )
            .into_bytes(),
        ),
    }
}

fn inject_client(html: &[u8], mode: ClientMode<'_>) -> Bytes {
    inject_client_script(html, client_script(mode).as_ref())
}

fn inject_client_script(html: &[u8], script: &[u8]) -> Bytes {
    let position = html
        .windows(b"</body>".len())
        .rposition(|window| window.eq_ignore_ascii_case(b"</body>"));
    let mut output = Vec::with_capacity(html.len() + script.len());
    if let Some(position) = position {
        output.extend_from_slice(html.get(..position).unwrap_or_default());
        output.extend_from_slice(script);
        output.extend_from_slice(html.get(position..).unwrap_or_default());
    } else {
        output.extend_from_slice(html);
        output.extend_from_slice(script);
    }
    Bytes::from(output)
}

#[derive(Clone, Copy)]
enum ReloadStreamPhase {
    Initial,
    Listening,
}

fn reload_response(receiver: watch::Receiver<String>) -> Response<AppBody> {
    let mut heartbeat = interval_at(Instant::now() + SSE_HEARTBEAT, SSE_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let events = stream::unfold(
        (receiver, heartbeat, ReloadStreamPhase::Initial),
        |(mut receiver, mut heartbeat, phase)| async move {
            match phase {
                ReloadStreamPhase::Initial => {
                    let revision = receiver.borrow_and_update().clone();
                    Some((
                        Ok::<_, Infallible>(reload_frame(&revision)),
                        (receiver, heartbeat, ReloadStreamPhase::Listening),
                    ))
                }
                ReloadStreamPhase::Listening => {
                    tokio::select! {
                        changed = receiver.changed() => {
                            if changed.is_err() {
                                return None;
                            }
                            let revision = receiver.borrow_and_update().clone();
                            Some((
                                Ok(reload_frame(&revision)),
                                (receiver, heartbeat, ReloadStreamPhase::Listening),
                            ))
                        }
                        _ = heartbeat.tick() => Some((
                            Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))),
                            (receiver, heartbeat, ReloadStreamPhase::Listening),
                        )),
                    }
                }
            }
        },
    );
    let body = StreamBody::new(events)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    response.headers_mut().insert(
        CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(CONNECTION, header::HeaderValue::from_static("keep-alive"));
    response
}

fn reload_frame(revision: &str) -> Frame<Bytes> {
    Frame::data(Bytes::from(format!("data: {revision}\n\n")))
}

struct AgentStreamState {
    receiver: broadcast::Receiver<Arc<AgentEvent>>,
    hub: AgentHub,
    pending: VecDeque<Arc<AgentEvent>>,
    seen: HashSet<String>,
    heartbeat: tokio::time::Interval,
}

fn agent_message_response(hub: AgentHub) -> Response<AppBody> {
    // Subscribe before taking the snapshot. Any message racing with the snapshot is
    // then present in at least one source, and the per-stream ID set removes duplicates.
    let receiver = hub.subscribe();
    let pending = hub.snapshot().into();
    let mut heartbeat = interval_at(Instant::now() + SSE_HEARTBEAT, SSE_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let state = AgentStreamState {
        receiver,
        hub,
        pending,
        seen: HashSet::new(),
        heartbeat,
    };
    let events = stream::unfold(state, next_agent_frame);
    let body = StreamBody::new(events)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    response.headers_mut().insert(
        CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(CONNECTION, header::HeaderValue::from_static("keep-alive"));
    response
}

async fn next_agent_frame(
    mut state: AgentStreamState,
) -> Option<(Result<Frame<Bytes>, Infallible>, AgentStreamState)> {
    loop {
        while let Some(event) = state.pending.pop_front() {
            if state.seen.insert(event.id.clone()) {
                return Some((Ok(agent_frame(&event)), state));
            }
        }

        tokio::select! {
            received = state.receiver.recv() => {
                match received {
                    Ok(event) => {
                        if state.seen.insert(event.id.clone()) {
                            return Some((Ok(agent_frame(&event)), state));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "agent message subscriber lagged; refreshing snapshot");
                        state.pending.extend(state.hub.snapshot());
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
            _ = state.heartbeat.tick() => {
                return Some((
                    Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))),
                    state,
                ));
            }
        }
    }
}

fn agent_frame(event: &AgentEvent) -> Frame<Bytes> {
    let data = serde_json::to_vec(event).unwrap_or_else(|_| b"{}".to_vec());
    let mut frame = Vec::with_capacity(event.id.len() + data.len() + 12);
    frame.extend_from_slice(b"id: ");
    frame.extend_from_slice(event.id.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    frame.extend_from_slice(&data);
    frame.extend_from_slice(b"\n\n");
    Frame::data(Bytes::from(frame))
}

#[derive(Serialize)]
struct PendingResponse {
    pending: usize,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

fn request_error_response(error: RequestError) -> Response<AppBody> {
    let RequestError {
        status,
        message,
        allow,
    } = error;
    let mut response = json_response(
        status,
        &ErrorResponse {
            error: message.as_str(),
        },
    );
    if let Some(allow) = allow {
        response
            .headers_mut()
            .insert(ALLOW, header::HeaderValue::from_static(allow));
    }
    response
}

fn json_response(status: StatusCode, body: &impl Serialize) -> Response<AppBody> {
    let bytes = serde_json::to_vec(body)
        .unwrap_or_else(|_| b"{\"error\":\"serialization failed\"}".to_vec());
    let mut response = Response::new(full_body(Bytes::from(bytes)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        header::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

fn javascript_response(script: &'static str) -> Response<AppBody> {
    let mut response = Response::new(full_body(Bytes::from_static(script.as_bytes())));
    response.headers_mut().insert(
        CONTENT_TYPE,
        header::HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

fn empty_response(status: StatusCode) -> Response<AppBody> {
    let mut response = Response::new(full_body(Bytes::new()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

fn full_body(bytes: Bytes) -> AppBody {
    Full::new(bytes)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn incoming_body(body: Incoming) -> AppBody {
    body.map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
        .boxed_unsync()
}

fn map_incoming_response(response: Response<Incoming>) -> Response<AppBody> {
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, incoming_body(body))
}

fn map_static_response(response: Response<hyper_staticfile::Body>) -> Response<AppBody> {
    let (parts, body) = response.into_parts();
    let body = body
        .map_err(|error| Box::new(error) as Box<dyn Error + Send + Sync>)
        .boxed_unsync();
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf, time::Duration};

    use hyper::{Request, header::HOST};

    use super::{
        AgentStreamState, ClientMode, StaticRequestedPath, has_hidden_component, inject_client,
        next_agent_frame, rewrite_redirect,
    };
    use crate::{agent::AgentHub, model::AgentMessage};

    #[test]
    fn injects_before_a_case_insensitive_body_close() {
        let output = inject_client(b"<html><body>Hello</BODY></html>", ClientMode::Edit);
        let output = String::from_utf8(output.to_vec()).expect("UTF-8 HTML");
        assert!(output.contains("Hello\n<script type=\"module\""));
        assert!(output.ends_with("</BODY></html>"));
    }

    #[test]
    fn appends_when_html_has_no_body_close() {
        let output = inject_client(b"<p>Hello</p>", ClientMode::Edit);
        assert!(String::from_utf8_lossy(&output).ends_with("</script>\n"));
    }

    #[test]
    fn marks_the_live_client_script() {
        let output = inject_client(b"<body>Hello</body>", ClientMode::Live("revision"));
        assert!(String::from_utf8_lossy(&output).contains("client.js?live=revision"));
    }

    #[test]
    fn sanitizes_paths_without_leaving_the_static_root() {
        let requested = StaticRequestedPath::resolve("/docs/../../index.html");
        assert_eq!(requested.path, PathBuf::from("index.html"));
        assert!(!requested.is_directory);

        let encoded = StaticRequestedPath::resolve("/%2e%2e/%2eenv");
        assert_eq!(encoded.path, PathBuf::from(".env"));
        assert!(has_hidden_component(&encoded.path));
    }

    #[test]
    fn rewrites_only_redirects_to_the_upstream_origin() {
        let upstream = url::Url::parse("http://127.0.0.1:8000").expect("URL");
        let mut response = Request::builder()
            .header(HOST, "irrelevant")
            .header(
                hyper::header::LOCATION,
                "http://127.0.0.1:8000/next?q=1#part",
            )
            .body(())
            .expect("request")
            .into_parts()
            .0
            .headers;
        rewrite_redirect(&mut response, &upstream);
        assert_eq!(
            response.get(hyper::header::LOCATION).expect("location"),
            "/next?q=1#part"
        );

        let upstream_with_base = url::Url::parse("http://127.0.0.1:8000/base/").expect("URL");
        response.insert(
            hyper::header::LOCATION,
            hyper::header::HeaderValue::from_static("http://127.0.0.1:8000/base/next"),
        );
        rewrite_redirect(&mut response, &upstream_with_base);
        assert_eq!(
            response.get(hyper::header::LOCATION).expect("location"),
            "/next"
        );
    }

    #[tokio::test]
    async fn lagged_agent_stream_refreshes_from_recent_history() {
        let hub = AgentHub::new();
        let receiver = hub.subscribe();
        for index in 0..600 {
            hub.publish(
                AgentMessage::new(format!("message {index}"), None).expect("valid message"),
            );
        }
        let expected = hub.snapshot().first().expect("history snapshot").id.clone();
        let heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(60),
            Duration::from_secs(60),
        );
        let state = AgentStreamState {
            receiver,
            hub,
            pending: std::collections::VecDeque::new(),
            seen: HashSet::new(),
            heartbeat,
        };
        let Some((Ok(_frame), state)) = next_agent_frame(state).await else {
            panic!("lag recovery frame");
        };
        assert!(state.seen.contains(&expected));
    }

    #[tokio::test]
    async fn agent_stream_deduplicates_snapshot_and_live_delivery() {
        let hub = AgentHub::new();
        let receiver = hub.subscribe();
        let event =
            hub.publish(AgentMessage::new("one message".to_owned(), None).expect("valid message"));
        let heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(10),
            Duration::from_secs(60),
        );
        let state = AgentStreamState {
            receiver,
            pending: hub.snapshot().into(),
            hub,
            seen: HashSet::new(),
            heartbeat,
        };

        let Some((Ok(first), state)) = next_agent_frame(state).await else {
            panic!("snapshot frame");
        };
        let first = first.into_data().expect("snapshot data");
        assert!(String::from_utf8_lossy(&first).contains(&event.id));

        let Some((Ok(second), state)) = next_agent_frame(state).await else {
            panic!("heartbeat frame");
        };
        assert_eq!(
            second.into_data().expect("heartbeat data").as_ref(),
            b": keep-alive\n\n"
        );
        assert_eq!(state.seen.len(), 1);
    }
}
