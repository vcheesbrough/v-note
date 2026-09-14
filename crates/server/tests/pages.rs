//! Page-route error paths that need no database.
//!
//! Every handler in `routes::pages` now returns `ApiError` rather than a fully
//! built `Response` (clippy's `result_large_err`). `db()` is the one error path
//! reachable without Postgres, so it is where the rewritten `Err` variant gets
//! exercised end to end — through the real router, middleware and all.
//! The database-backed paths (403 page-not-found, 410 thumbnail-gone) are
//! covered by the Playwright suite in `e2e/tests/`.

mod common;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use protocol::{CreatePageRequest, Paper};
use server::build_router;
use tower::util::ServiceExt;

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

/// A router with `db: None` — exactly the state `db()` rejects — plus a token
/// that clears the auth middleware, so the request reaches the handler.
fn router_without_db() -> (axum::Router, String) {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims::valid_for(&config, "page-user"));
    let app = build_router(
        "test-version".to_string(),
        Arc::new(config),
        Arc::new(test_jwks_cache()),
    );
    (app, token)
}

/// Built from the protocol type so the payload cannot drift from the wire
/// contract and turn a 503 assertion into an accidental 422.
fn create_page_body() -> String {
    serde_json::to_string(&CreatePageRequest {
        title: Some("note".to_string()),
        paper: Paper::RuledWide,
    })
    .expect("create-page request should serialize")
}

async fn request_without_db(method: Method, uri: &str, body: Body) -> (StatusCode, String, String) {
    let (app, token) = router_without_db();
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).expect("body should be UTF-8"),
    )
}

#[tokio::test]
async fn every_page_route_returns_503_when_no_database_is_configured() {
    let cases = [
        (Method::GET, "/api/pages", Body::empty()),
        (Method::POST, "/api/pages", Body::from(create_page_body())),
        (Method::GET, "/api/pages/page_abc", Body::empty()),
        (Method::DELETE, "/api/pages/page_abc", Body::empty()),
        (
            Method::GET,
            "/api/pages/page_abc/thumbnails/1",
            Body::empty(),
        ),
    ];

    for (method, uri, body) in cases {
        let label = format!("{method} {uri}");
        let (status, content_type, rendered) = request_without_db(method, uri, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{label}");
        assert_eq!(content_type, "text/plain; charset=utf-8", "{label}");
        assert_eq!(rendered, "database not configured", "{label}");
    }
}
