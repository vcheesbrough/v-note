//! `GET /api/telemetry/config` (#439), through the real router: who may read
//! it, what it hands back, and that an environment without the ingest hands
//! back nothing.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, COOKIE};
use axum::http::{Request, StatusCode};
use protocol::TelemetryConfigResponse;
use server::build_router_with_telemetry_endpoint;
use tower::ServiceExt;
use url::Url;

use common::{
    SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache, unreachable_pool,
};

const INGEST: &str = "https://v-notes-dev.desync.link";

fn app(endpoint: Option<&str>) -> Router {
    build_router_with_telemetry_endpoint(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
        endpoint.map(|endpoint| Url::parse(endpoint).expect("endpoint")),
    )
}

fn token() -> (String, u64) {
    let claims = SignedTokenClaims::valid_for(&test_auth_config(), "user-1");
    let exp = claims.exp;
    (sign_test_token(claims), exp)
}

async fn get(
    app: Router,
    header: Option<(axum::http::HeaderName, String)>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().uri("/api/telemetry/config");
    if let Some((name, value)) = header {
        request = request.header(name, value);
    }
    let response = app
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

/// The route hands out a token, so an anonymous caller must get nothing —
/// not even whether telemetry is on.
#[tokio::test]
async fn anonymous_callers_are_refused_whether_or_not_ingest_is_on() {
    for endpoint in [Some(INGEST), None] {
        let (status, _, body) = get(app(endpoint), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{endpoint:?}");
        assert!(
            !String::from_utf8_lossy(&body).contains("endpoint"),
            "{endpoint:?}: {}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[tokio::test]
async fn an_invalid_token_is_refused() {
    let (status, _, _) = get(
        app(Some(INGEST)),
        Some((AUTHORIZATION, "Bearer not-a-jwt".to_string())),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The cookie the SPA holds is the access token; this is the one place it
/// leaves the server again, and it must be exactly that token, with its expiry.
#[tokio::test]
async fn a_cookie_session_gets_the_endpoint_and_its_own_token() {
    let (token, exp) = token();
    let (status, headers, body) =
        get(app(Some(INGEST)), Some((COOKIE, format!("auth={token}")))).await;

    assert_eq!(status, StatusCode::OK);
    let config: TelemetryConfigResponse = serde_json::from_slice(&body).expect("json");
    assert_eq!(config.endpoint, INGEST, "a bare origin, no trailing slash");
    assert_eq!(config.access_token, token);
    assert_eq!(config.expires_at, exp);
    assert_eq!(
        headers
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store"),
        "a token must never be cached"
    );
}

/// Android authenticates with a bearer and gets the same answer.
#[tokio::test]
async fn a_bearer_gets_the_same_configuration() {
    let (token, _) = token();
    let (status, _, body) = get(
        app(Some(INGEST)),
        Some((AUTHORIZATION, format!("Bearer {token}"))),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let config: TelemetryConfigResponse = serde_json::from_slice(&body).expect("json");
    assert_eq!(config.endpoint, INGEST);
    assert_eq!(config.access_token, token);
}

/// Ingest off is `204` with no body — and a client's "no configuration" — not
/// an error, and never a token.
#[tokio::test]
async fn ingest_off_answers_no_content() {
    let (token, _) = token();
    let (status, _, body) = get(app(None), Some((COOKIE, format!("auth={token}")))).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty(), "{}", String::from_utf8_lossy(&body));
}
