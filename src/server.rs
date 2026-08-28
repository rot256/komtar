use std::{collections::HashSet, convert::Infallible, error::Error, net::IpAddr, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode, Uri,
    body::Incoming,
    header::{
        self, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
        ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ALLOW, CACHE_CONTROL, CONTENT_LENGTH,
        CONTENT_TYPE, ETAG, HOST, LOCATION, ORIGIN, VARY,
    },
    service::service_fn,
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use serde::Serialize;
use tokio::{io::copy_bidirectional, net::TcpListener};
use url::{Host, Url};

use crate::{
    BoxError,
    fifo::CommentQueue,
    model::{CommentDraft, CommentRecord, MAX_REQUEST_BYTES, RequestError},
};

pub(crate) const RESERVED_PREFIX: &str = "/_web-fifo/";
const CLIENT_PATH: &str = "/_web-fifo/client.js";
const STATUS_PATH: &str = "/_web-fifo/api/status";
const COMMENTS_PATH: &str = "/_web-fifo/api/comments";
const MAX_HTML_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const INJECTED_SCRIPT: &[u8] =
    b"\n<script type=\"module\" src=\"/_web-fifo/client.js\"></script>\n";

type AppBody = UnsyncBoxBody<Bytes, BoxError>;
type HttpClient = Client<HttpConnector, AppBody>;

#[derive(Clone)]
pub(crate) enum ServerMode {
    Proxy { upstream: Url },
    Serve,
}

#[derive(Clone)]
pub(crate) struct ServerState {
    mode: ServerMode,
    queue: CommentQueue,
    allowed_origins: Arc<HashSet<String>>,
    client: HttpClient,
}

impl ServerState {
    pub(crate) fn new(
        mode: ServerMode,
        queue: CommentQueue,
        allowed_origins: HashSet<String>,
    ) -> Self {
        let connector = HttpConnector::new();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Self {
            mode,
            queue,
            allowed_origins: Arc::new(allowed_origins),
            client,
        }
    }
}

pub(crate) async fn serve(listener: TcpListener, state: ServerState) -> Result<(), BoxError> {
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
            signal = tokio::signal::ctrl_c() => {
                signal?;
                return Ok(());
            }
        }
    }
}

async fn handle(
    request: Request<Incoming>,
    state: ServerState,
) -> Result<Response<AppBody>, Infallible> {
    let path = request.uri().path();
    let result = if path.starts_with(RESERVED_PREFIX) {
        handle_reserved(request, &state).await
    } else {
        match &state.mode {
            ServerMode::Proxy { upstream } => proxy_request(request, &state, upstream).await,
            ServerMode::Serve => Ok(text_response(
                StatusCode::NOT_FOUND,
                "web-fifo serve only exposes /_web-fifo/ resources\n",
            )),
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
    let cors_origin = allowed_request_origin(&request, &state.allowed_origins)?;
    if request.method() == Method::OPTIONS {
        let mut response = empty_response(StatusCode::NO_CONTENT);
        add_cors_headers(&mut response, cors_origin.as_deref());
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        response.headers_mut().insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static("Content-Type"),
        );
        response.headers_mut().insert(
            ACCESS_CONTROL_MAX_AGE,
            header::HeaderValue::from_static("600"),
        );
        return Ok(response);
    }

    let path = request.uri().path().to_owned();
    let result = match path.as_str() {
        CLIENT_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET, OPTIONS"))
            } else {
                Ok(javascript_response(crate::CLIENT_JS))
            }
        }
        STATUS_PATH => {
            if request.method() != Method::GET {
                Err(RequestError::method_not_allowed("GET, OPTIONS"))
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
                Err(RequestError::method_not_allowed("POST, OPTIONS"))
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
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            &ErrorResponse {
                error: "reserved web-fifo resource not found",
            },
        )),
    };
    let mut response = result.unwrap_or_else(request_error_response);
    add_cors_headers(&mut response, cors_origin.as_deref());
    Ok(response)
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
    state: &ServerState,
    upstream: &Url,
) -> Result<Response<AppBody>, RequestError> {
    let downstream_upgrade = if request.headers().contains_key(header::UPGRADE) {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };
    let original_host = request.headers().get(HOST).cloned();
    let (mut parts, body) = request.into_parts();
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
    let mut response = state.client.request(outgoing).await.map_err(|error| {
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

    maybe_inject_client(response).await
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
    response: Response<Incoming>,
) -> Result<Response<AppBody>, RequestError> {
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"));
    let is_encoded = response.headers().contains_key(header::CONTENT_ENCODING);
    if !response.status().is_success() || !is_html || is_encoded {
        if is_html && is_encoded {
            tracing::warn!("upstream ignored Accept-Encoding: identity; HTML was not annotated");
        }
        return Ok(map_incoming_response(response));
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
        return Ok(map_incoming_response(response));
    }

    let (mut parts, body) = response.into_parts();
    let bytes = body.collect().await.map_err(|error| {
        tracing::warn!(%error, "could not read upstream HTML response");
        RequestError::new(StatusCode::BAD_GATEWAY, "could not read upstream response")
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

    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(ETAG);
    parts.headers.remove(header::CONTENT_ENCODING);
    parts.headers.remove(header::TRANSFER_ENCODING);
    parts
        .headers
        .insert(CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    Ok(Response::from_parts(
        parts,
        full_body(inject_client(&bytes)),
    ))
}

fn inject_client(html: &[u8]) -> Bytes {
    let position = html
        .windows(b"</body>".len())
        .rposition(|window| window.eq_ignore_ascii_case(b"</body>"));
    let mut output = Vec::with_capacity(html.len() + INJECTED_SCRIPT.len());
    if let Some(position) = position {
        output.extend_from_slice(html.get(..position).unwrap_or_default());
        output.extend_from_slice(INJECTED_SCRIPT);
        output.extend_from_slice(html.get(position..).unwrap_or_default());
    } else {
        output.extend_from_slice(html);
        output.extend_from_slice(INJECTED_SCRIPT);
    }
    Bytes::from(output)
}

fn allowed_request_origin(
    request: &Request<Incoming>,
    allowed_origins: &HashSet<String>,
) -> Result<Option<String>, RequestError> {
    let Some(raw_origin) = request.headers().get(ORIGIN) else {
        return Ok(None);
    };
    let origin = raw_origin
        .to_str()
        .map_err(|_| RequestError::new(StatusCode::FORBIDDEN, "request origin is not allowed"))?;
    let parsed = Url::parse(origin)
        .map_err(|_| RequestError::new(StatusCode::FORBIDDEN, "request origin is not allowed"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(RequestError::new(
            StatusCode::FORBIDDEN,
            "request origin is not allowed",
        ));
    }
    let normalized = parsed.origin().ascii_serialization();
    let same_host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| origin_matches_host(&parsed, host));
    if same_host || origin_is_loopback(&parsed) || allowed_origins.contains(&normalized) {
        return Ok(Some(normalized));
    }
    Err(RequestError::new(
        StatusCode::FORBIDDEN,
        "request origin is not allowed",
    ))
}

fn origin_matches_host(origin: &Url, host: &str) -> bool {
    let Ok(host_url) = Url::parse(&format!("http://{host}")) else {
        return false;
    };
    origin.host_str() == host_url.host_str()
        && origin.port_or_known_default() == host_url.port_or_known_default()
}

fn origin_is_loopback(origin: &Url) -> bool {
    match origin.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => IpAddr::V6(address).is_loopback(),
        Some(Host::Domain(_)) => false,
        None => false,
    }
}

fn add_cors_headers(response: &mut Response<AppBody>, origin: Option<&str>) {
    let Some(origin) = origin else {
        return;
    };
    let Ok(value) = header::HeaderValue::from_str(origin) else {
        return;
    };
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_ORIGIN, value);
    response
        .headers_mut()
        .insert(VARY, header::HeaderValue::from_static("Origin"));
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

fn text_response(status: StatusCode, text: &'static str) -> Response<AppBody> {
    let mut response = Response::new(full_body(Bytes::from_static(text.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn empty_response(status: StatusCode) -> Response<AppBody> {
    let mut response = Response::new(full_body(Bytes::new()));
    *response.status_mut() = status;
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

#[cfg(test)]
mod tests {
    use hyper::{Request, header::HOST};

    use super::{inject_client, origin_is_loopback, origin_matches_host, rewrite_redirect};

    #[test]
    fn injects_before_a_case_insensitive_body_close() {
        let output = inject_client(b"<html><body>Hello</BODY></html>");
        let output = String::from_utf8(output.to_vec()).expect("UTF-8 HTML");
        assert!(output.contains("Hello\n<script type=\"module\""));
        assert!(output.ends_with("</BODY></html>"));
    }

    #[test]
    fn appends_when_html_has_no_body_close() {
        let output = inject_client(b"<p>Hello</p>");
        assert!(String::from_utf8_lossy(&output).ends_with("</script>\n"));
    }

    #[test]
    fn recognizes_local_origins_and_exact_hosts() {
        assert!(origin_is_loopback(
            &url::Url::parse("http://127.0.0.1:8000").expect("URL")
        ));
        assert!(origin_is_loopback(
            &url::Url::parse("http://[::1]:8000").expect("URL")
        ));
        assert!(origin_matches_host(
            &url::Url::parse("http://example.test:3939").expect("URL"),
            "example.test:3939"
        ));
        assert!(!origin_matches_host(
            &url::Url::parse("http://example.test:4000").expect("URL"),
            "example.test:3939"
        ));
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
}
