//! The authenticated ingress for client telemetry (#354):
//! `POST /otlp/{client}/v1/{signal}`.
//!
//! This is an **opaque byte proxy**. It authenticates the caller, bounds the
//! body, and hands the bytes to the Alloy sidecar unread — there is no OTLP
//! decoding here, no `opentelemetry-proto`, and nothing that inspects or
//! rewrites a span. Deciding what client telemetry is allowed to *say* is the
//! sidecar's job (`deploy/alloy/client-telemetry.alloy`); deciding who is allowed
//! to send any is this module's.
//!
//! It lives in the app rather than behind a Traefik `forwardauth` because the
//! e2e and local stacks have no Traefik: there, a `/otlp` request would fall
//! through to the SPA's catch-all and come back `200 index.html`, so the one
//! path CI could exercise would not be the one that ships.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::Instrument as _;
use url::Url;

use crate::AppState;
use crate::config::ClientTelemetryUpstreams;
use crate::observability::metrics;

/// The largest export body accepted, on the wire (so: after any `gzip` the client
/// applied). A batched, sampled export is a few KiB; this is two orders of
/// magnitude of headroom, and still small enough to buffer without thinking
/// about it. The sidecar separately bounds the *decompressed* size.
pub(crate) const MAX_EXPORT_BYTES: usize = 1024 * 1024;

/// Telemetry must never be able to slow the product down, so a sidecar that is
/// down or wedged has to cost a request almost nothing. Both are generous for a
/// hop to a container on the same host.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// What the ingress needs at request time. `None` in [`AppState`] is the kill
/// switch: no upstreams means no ingest.
#[derive(Clone)]
pub struct ClientTelemetryIngress {
    upstreams: ClientTelemetryUpstreams,
    http: reqwest::Client,
}

impl ClientTelemetryIngress {
    pub fn new(upstreams: ClientTelemetryUpstreams) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            // The sidecar is a fixed, configured origin. A redirect from it is a
            // misconfiguration, and following one would send a client's export
            // somewhere nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the client telemetry HTTP client has no fallible configuration");
        Self { upstreams, http }
    }

    fn upstream(&self, client: ClientKind, signal: Signal) -> Url {
        let base = match client {
            ClientKind::Spa => &self.upstreams.spa,
            ClientKind::Android => &self.upstreams.android,
        };
        // `validate()` guarantees a bare origin, so this can only ever produce
        // `<origin>/v1/<signal>`; the path is built from two constants.
        base.join(signal.path())
            .expect("a bare origin joined with a constant path is a valid URL")
    }
}

/// Which receiver an export is for. The sidecar forces `service.name` per
/// receiver, so this segment — not anything inside the payload — is what decides
/// how the telemetry is labelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientKind {
    Spa,
    Android,
}

impl ClientKind {
    fn parse(segment: &str) -> Option<Self> {
        match segment {
            "spa" => Some(Self::Spa),
            "android" => Some(Self::Android),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Spa => "spa",
            Self::Android => "android",
        }
    }
}

/// `metrics` is absent on purpose: client metrics were dropped from #354 and the
/// sidecar has no pipeline for them, so the honest answer is "no such route".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Traces,
    Logs,
}

impl Signal {
    fn parse(segment: &str) -> Option<Self> {
        match segment {
            "traces" => Some(Self::Traces),
            "logs" => Some(Self::Logs),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Logs => "logs",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Traces => "v1/traces",
            Self::Logs => "v1/logs",
        }
    }
}

/// The kill switch, as a layer **outside** authentication.
///
/// Order is the point. A client treats 404 as "telemetry is off here, stop
/// trying" and 401 as "not signed in yet, try again later". If authentication
/// ran first, a signed-out browser talking to a server with ingest disabled
/// would see 401 forever and never learn to stop.
pub(crate) async fn enabled_gate(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if state.client_telemetry.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

/// Anything under `/otlp` that is not the export route. Without this, such a
/// request reaches the SPA's catch-all — and a `GET` there is `200 index.html`.
pub(crate) async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

pub(crate) async fn ingest(
    State(state): State<AppState>,
    Path((client, signal)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // `enabled_gate` has already turned a disabled ingress away.
    let Some(ingress) = state.client_telemetry.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Plain strings matched by hand rather than `Path<(ClientKind, Signal)>`:
    // axum answers a path segment that fails to deserialize with 400, and an
    // unknown client kind is a route that does not exist, not a bad request.
    let (Some(client), Some(signal)) = (ClientKind::parse(&client), Signal::parse(&signal)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let span = tracing::info_span!(
        "client_telemetry.ingest",
        client = client.label(),
        signal = signal.label(),
        bytes = tracing::field::Empty,
        upstream.status = tracing::field::Empty,
    );
    let outcome = forward(ingress, client, signal, &headers, body)
        .instrument(span)
        .await;
    metrics().record_client_telemetry(client.label(), signal.label(), outcome.label());
    outcome.into_response()
}

enum Outcome {
    /// The sidecar accepted the export.
    Forwarded(Relayed),
    /// The sidecar answered, but not with a 2xx — a malformed payload, or its
    /// memory limiter refusing data. Relayed as-is so a client can back off.
    SidecarRejected(Relayed),
    SidecarUnreachable,
    TooLarge,
    BodyError,
}

struct Relayed {
    status: StatusCode,
    content_type: Option<axum::http::HeaderValue>,
    body: axum::body::Bytes,
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Forwarded(_) => "forwarded",
            Self::SidecarRejected(_) => "sidecar_rejected",
            Self::SidecarUnreachable => "sidecar_unreachable",
            Self::TooLarge => "too_large",
            Self::BodyError => "body_error",
        }
    }
}

impl IntoResponse for Outcome {
    fn into_response(self) -> Response {
        match self {
            Self::Forwarded(relayed) | Self::SidecarRejected(relayed) => {
                let mut response = (relayed.status, relayed.body).into_response();
                if let Some(content_type) = relayed.content_type {
                    response.headers_mut().insert(CONTENT_TYPE, content_type);
                }
                response
            }
            Self::SidecarUnreachable => StatusCode::BAD_GATEWAY.into_response(),
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Self::BodyError => StatusCode::BAD_REQUEST.into_response(),
        }
    }
}

async fn forward(
    ingress: &ClientTelemetryIngress,
    client: ClientKind,
    signal: Signal,
    headers: &HeaderMap,
    body: Body,
) -> Outcome {
    // An honest client is refused before its body is read at all.
    let declared_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > MAX_EXPORT_BYTES) {
        tracing::debug!(declared_length, "client telemetry export over the body cap");
        return Outcome::TooLarge;
    }

    // …and a chunked or dishonest one is cut off at the cap regardless.
    let body = match axum::body::to_bytes(body, MAX_EXPORT_BYTES).await {
        Ok(body) => body,
        Err(error) => {
            return if is_length_limit(&error) {
                tracing::debug!("client telemetry export over the body cap");
                Outcome::TooLarge
            } else {
                // The client went away mid-upload. Its problem, not ours.
                tracing::debug!(error = %error, "client telemetry export body could not be read");
                Outcome::BodyError
            };
        }
    };
    tracing::Span::current().record("bytes", body.len());

    // Built from nothing, not copied from the inbound request: the session
    // cookie and any bearer token stop here. The sidecar parses client-supplied
    // input and has no business holding a user's credentials — and OTLP defines
    // no other request header it needs.
    let mut request = ingress.http.post(ingress.upstream(client, signal));
    for name in [CONTENT_TYPE, CONTENT_ENCODING] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value);
        }
    }

    let response = match request.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            // `warn`, not `error`: the product is unaffected, and a sidecar
            // being down is a condition the deploy is designed to tolerate.
            let timeout = error.is_timeout();
            tracing::warn!(
                error = %error.without_url(),
                timeout,
                "client telemetry sidecar unreachable; export dropped"
            );
            return Outcome::SidecarUnreachable;
        }
    };

    let status = response.status();
    tracing::Span::current().record("upstream.status", status.as_u16());
    let content_type = response.headers().get(CONTENT_TYPE).cloned();
    let body = match response.bytes().await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(
                error = %error.without_url(),
                "client telemetry sidecar response could not be read"
            );
            return Outcome::SidecarUnreachable;
        }
    };

    let relayed = Relayed {
        status,
        content_type,
        body,
    };
    if status.is_success() {
        Outcome::Forwarded(relayed)
    } else {
        tracing::debug!(
            status = status.as_u16(),
            "client telemetry sidecar rejected an export"
        );
        Outcome::SidecarRejected(relayed)
    }
}

/// Whether a body read failed because it hit the cap, as opposed to the
/// connection breaking. `to_bytes` reports both as the same opaque error with
/// the cause in its source chain.
fn is_length_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = current.source();
    }
    false
}
