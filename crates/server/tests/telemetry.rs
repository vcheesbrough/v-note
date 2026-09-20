//! The `/otlp` client telemetry ingress (#354), driven through the real router
//! against a stub sidecar that records what reached it.
//!
//! "What reached it" carries most of the weight here. The ingress is a proxy, so
//! almost every property worth having is a statement about the upstream request:
//! that there was one, that there was *not* one, which receiver it went to, and
//! what it did and did not carry.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, COOKIE, LOCATION};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use server::config::ClientTelemetryUpstreams;
use server::{build_router, build_router_with_client_telemetry};
use tower::ServiceExt;
use url::Url;

use common::{
    SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache, unreachable_pool,
};

/// Mirrors `routes::telemetry::MAX_EXPORT_BYTES`. Restated rather than imported
/// so that changing the cap is a decision that has to be made twice.
const MAX_EXPORT_BYTES: usize = 1024 * 1024;

const OTLP_JSON: &str = r#"{"resourceSpans":[]}"#;

#[derive(Debug, Clone)]
struct Received {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

/// A stand-in for one Alloy OTLP receiver: records every request and answers
/// with whatever `reply` says.
#[derive(Clone)]
struct StubSidecar {
    origin: Url,
    received: Arc<Mutex<Vec<Received>>>,
}

type Reply = Arc<dyn Fn() -> Response + Send + Sync>;

#[derive(Clone)]
struct StubState {
    received: Arc<Mutex<Vec<Received>>>,
    reply: Reply,
}

impl StubSidecar {
    async fn accepting() -> Self {
        Self::replying(|| {
            (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/json")],
                r#"{"partialSuccess":{}}"#,
            )
                .into_response()
        })
        .await
    }

    async fn replying(reply: impl Fn() -> Response + Send + Sync + 'static) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let state = StubState {
            received: received.clone(),
            reply: Arc::new(reply),
        };
        let app = Router::new().fallback(record).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("stub sidecar should bind");
        let addr = listener.local_addr().expect("stub sidecar address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("stub sidecar serves");
        });
        Self {
            origin: origin_of(addr),
            received,
        }
    }

    fn requests(&self) -> Vec<Received> {
        self.received.lock().expect("stub lock").clone()
    }

    fn only_request(&self) -> Received {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected exactly one upstream request");
        requests.into_iter().next().expect("one request")
    }
}

async fn record(
    State(state): State<StubState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    state.received.lock().expect("stub lock").push(Received {
        method,
        path: uri.path().to_string(),
        headers,
        body,
    });
    (state.reply)()
}

fn origin_of(addr: SocketAddr) -> Url {
    Url::parse(&format!("http://{addr}")).expect("stub origin")
}

/// An origin nothing is listening on: bind, read the port, drop the listener.
async fn closed_origin() -> Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    origin_of(addr)
}

fn app_with(spa: &Url, android: &Url) -> Router {
    build_router_with_client_telemetry(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
        Some(ClientTelemetryUpstreams {
            spa: spa.clone(),
            android: android.clone(),
        }),
    )
}

fn valid_token() -> String {
    sign_test_token(SignedTokenClaims::valid_for(&test_auth_config(), "user-1"))
}

fn export(path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(CONTENT_TYPE, "application/json")
}

async fn body_of(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body")
}

// ---------------------------------------------------------------------------
// The happy path, and what it does and does not pass along
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_bearer_export_is_forwarded_byte_for_byte_and_the_reply_relayed() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .header(CONTENT_ENCODING, "gzip")
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).expect("content type"),
        "application/json"
    );
    assert_eq!(body_of(response).await, r#"{"partialSuccess":{}}"#);

    let upstream = sidecar.only_request();
    assert_eq!(upstream.method, Method::POST);
    assert_eq!(
        upstream.path, "/v1/traces",
        "the /otlp/spa prefix is the ingress's, not the receiver's"
    );
    assert_eq!(upstream.body, OTLP_JSON, "the payload is not re-encoded");
    assert_eq!(
        upstream.headers.get(CONTENT_TYPE).expect("type"),
        "application/json"
    );
    // Forwarded unread: the ingress never decompresses, so the sidecar must be
    // told the body is gzip or it will try to parse the compressed bytes.
    assert_eq!(
        upstream.headers.get(CONTENT_ENCODING).expect("encoding"),
        "gzip"
    );
}

/// The SPA's whole auth story: a same-origin `fetch` carries the `HttpOnly`
/// session cookie and nothing else, so the browser never holds a token.
#[tokio::test]
async fn the_session_cookie_alone_authenticates_an_export() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/logs")
                .header(COOKIE, format!("auth={}", valid_token()))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(sidecar.only_request().path, "/v1/logs");
}

/// The sidecar parses input that originated on a client. It must never be
/// handed the credentials that authorised the request.
#[tokio::test]
async fn the_callers_credentials_never_reach_the_sidecar() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);
    let token = valid_token();

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header(COOKIE, format!("auth={token}; other=1"))
                .header("x-request-id", "spa_abc")
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let upstream = sidecar.only_request();
    assert!(
        upstream.headers.get(AUTHORIZATION).is_none(),
        "bearer leaked upstream"
    );
    assert!(
        upstream.headers.get(COOKIE).is_none(),
        "session cookie leaked upstream"
    );
    let leaked = upstream
        .headers
        .values()
        .any(|value| value.to_str().is_ok_and(|text| text.contains(&token)));
    assert!(!leaked, "the token appears in some other upstream header");
    // An allow-list, not a deny-list: nothing the ingress was not written to
    // forward gets through, including headers nobody has thought of yet.
    assert!(upstream.headers.get("x-request-id").is_none());
}

/// `service.name` is forced per *receiver*, so which upstream a request lands
/// on is what labels the telemetry. Two stubs, so a mix-up cannot hide.
#[tokio::test]
async fn each_client_kind_reaches_its_own_receiver() {
    let spa = StubSidecar::accepting().await;
    let android = StubSidecar::accepting().await;
    let app = app_with(&spa.origin, &android.origin);

    for path in ["/otlp/android/v1/traces", "/otlp/android/v1/logs"] {
        let response = app
            .clone()
            .oneshot(
                export(path)
                    .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                    .body(Body::from(OTLP_JSON))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }

    assert!(
        spa.requests().is_empty(),
        "an android export reached the spa receiver"
    );
    let paths: Vec<_> = android.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, ["/v1/traces", "/v1/logs"]);
}

// ---------------------------------------------------------------------------
// Authentication — and that a refusal costs the sidecar nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unauthenticated_export_is_401_and_never_forwarded() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(sidecar.requests().is_empty());
}

/// 403, not the 401 the pre-pickup card asked for: this is `auth_middleware`,
/// and it answers a valid token without the scope the way it does everywhere.
#[tokio::test]
async fn a_token_without_the_required_scope_is_403_and_never_forwarded() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);
    let mut claims = SignedTokenClaims::valid_for(&test_auth_config(), "user-1");
    claims.scope = "openid v-note:wrong:access".to_string();

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", sign_test_token(claims)))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(sidecar.requests().is_empty());
}

// ---------------------------------------------------------------------------
// The kill switch
// ---------------------------------------------------------------------------

/// `build_router` is the switch in its off position.
#[tokio::test]
async fn with_the_switch_off_every_export_is_404() {
    let app = build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
    );

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// The ordering that matters. A client reads 404 as "off, stop trying" and 401
/// as "sign in and retry". Were authentication consulted first, a signed-out
/// browser would get 401 from a disabled server indefinitely and never stop.
#[tokio::test]
async fn the_switch_is_consulted_before_authentication() {
    let app = build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
    );

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a signed-out client must learn ingest is off, not be told to sign in"
    );
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// `metrics` is the case with teeth: it is a real OTLP signal, a client might
/// reasonably send it, and the sidecar has no pipeline for it.
#[tokio::test]
async fn unknown_clients_and_signals_are_404_and_never_forwarded() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    for path in [
        "/otlp/spa/v1/metrics",
        "/otlp/ios/v1/traces",
        "/otlp/SPA/v1/traces",
        "/otlp/spa/v2/traces",
        "/otlp/v1/traces",
        "/otlp/spa/v1/traces/extra",
        "/otlp/",
    ] {
        let response = app
            .clone()
            .oneshot(
                export(path)
                    .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                    .body(Body::from(OTLP_JSON))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    assert!(sidecar.requests().is_empty());
}

/// Nothing under `/otlp` may reach the SPA's catch-all, which answers any `GET`
/// it does not recognise with `200 index.html`. This router has no static
/// directory, so the assertion is the weaker "not 2xx" — the e2e suite checks
/// the same path against the real image, where the catch-all exists.
#[tokio::test]
async fn a_get_under_otlp_is_not_a_success() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    for (path, expected) in [
        ("/otlp/spa/v1/traces", StatusCode::METHOD_NOT_ALLOWED),
        ("/otlp/anything/else", StatusCode::NOT_FOUND),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), expected, "{path}");
    }
    assert!(sidecar.requests().is_empty());
}

// ---------------------------------------------------------------------------
// The body cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_exactly_at_the_cap_is_forwarded() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(vec![b'a'; MAX_EXPORT_BYTES]))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(sidecar.only_request().body.len(), MAX_EXPORT_BYTES);
}

#[tokio::test]
async fn a_body_one_byte_over_the_cap_is_413_and_never_forwarded() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(vec![b'a'; MAX_EXPORT_BYTES + 1]))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(sidecar.requests().is_empty());
}

/// No `Content-Length` to read, so the declared-length check cannot fire: the
/// cap has to hold on the bytes actually received. This is the dishonest
/// client, and the reason the cap is not just a header comparison.
#[tokio::test]
async fn a_streamed_body_with_no_declared_length_is_still_capped() {
    let sidecar = StubSidecar::accepting().await;
    let app = app_with(&sidecar.origin, &sidecar.origin);
    let chunks = (0..5).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 256 * 1024])));

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from_stream(stream::iter(chunks)))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(sidecar.requests().is_empty());
}

// ---------------------------------------------------------------------------
// A sidecar that is down, unhappy, or misbehaving
// ---------------------------------------------------------------------------

/// "No product request path slows down" starts here: a dead sidecar has to be
/// cheap. A refused connection is immediate, so this is bounded far below the
/// 1s connect timeout — a regression that retried, or waited the timeout out,
/// fails it.
#[tokio::test]
async fn an_unreachable_sidecar_is_a_prompt_502() {
    let nowhere = closed_origin().await;
    let app = app_with(&nowhere, &nowhere);

    let started = Instant::now();
    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "a refused connection took {:?}",
        started.elapsed()
    );
}

/// 400 for a payload the receiver cannot parse, 429/503 when its memory limiter
/// is refusing data. A client can only back off from a status it is shown.
#[tokio::test]
async fn a_sidecar_refusal_is_relayed_with_its_body() {
    let sidecar = StubSidecar::replying(|| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(CONTENT_TYPE, "application/json")],
            r#"{"code":8,"message":"data refused due to high memory usage"}"#,
        )
            .into_response()
    })
    .await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        String::from_utf8_lossy(&body_of(response).await).contains("high memory usage"),
        "the sidecar's explanation should reach the client"
    );
}

/// The upstream is a configured origin. If it ever answers with a redirect,
/// following it would post a client's export to wherever `Location` says.
#[tokio::test]
async fn a_redirect_from_the_sidecar_is_not_followed() {
    let elsewhere = StubSidecar::accepting().await;
    let target = elsewhere
        .origin
        .join("v1/traces")
        .expect("target")
        .to_string();
    let sidecar = StubSidecar::replying(move || {
        (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, target.clone())]).into_response()
    })
    .await;
    let app = app_with(&sidecar.origin, &sidecar.origin);

    let response = app
        .oneshot(
            export("/otlp/spa/v1/traces")
                .header(AUTHORIZATION, format!("Bearer {}", valid_token()))
                .body(Body::from(OTLP_JSON))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(sidecar.requests().len(), 1);
    assert!(
        elsewhere.requests().is_empty(),
        "the export was re-posted to the redirect target"
    );
}
