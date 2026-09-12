use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use protocol::HealthResponse;
use server::auth::{AuthConfig, JwksCache};
use server::build_router;
use server::observability::{CORRELATION_ID_HEADER, REQUEST_ID_HEADER, metrics, metrics_handler};
use tower::util::ServiceExt;

fn test_router() -> axum::Router {
    let auth = Arc::new(AuthConfig {
        issuer_url: "http://mock-oidc:8080/default".to_string(),
        client_id: "v-note-test".to_string(),
        client_secret: "test-secret".to_string(),
        redirect_uri: "https://app:443/auth/callback".to_string(),
        required_scope: "v-note:test:access".to_string(),
        end_session_url: None,
        authorize_endpoint: "http://mock-oidc:8080/default/authorize".to_string(),
        token_endpoint: "http://mock-oidc:8080/default/token".to_string(),
        jwks_uri: "http://mock-oidc:8080/default/jwks".to_string(),
        android_issuer_url: None,
        android_client_id: None,
    });
    let jwks = Arc::new(JwksCache::with_keys(HashMap::new()));
    build_router("test-version".to_string(), auth, jwks)
}

#[tokio::test]
async fn health_returns_ok_payload() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let payload: HealthResponse =
        serde_json::from_slice(&body).expect("health response should deserialize");

    assert_eq!(payload.status, "ok");
}

#[tokio::test]
async fn health_response_includes_request_correlation_headers() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(REQUEST_ID_HEADER, "test-request-123")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header should be present"),
        "test-request-123",
    );
    assert_eq!(
        response
            .headers()
            .get(CORRELATION_ID_HEADER)
            .expect("correlation id header should be present"),
        "test-request-123",
    );
}

#[tokio::test]
async fn health_response_preserves_correlation_id_without_request_id() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(CORRELATION_ID_HEADER, "upstream-correlation-123")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header should be present"),
        "upstream-correlation-123",
    );
    assert_eq!(
        response
            .headers()
            .get(CORRELATION_ID_HEADER)
            .expect("correlation id header should be present"),
        "upstream-correlation-123",
    );
}

#[tokio::test]
async fn metrics_endpoint_exposes_build_and_http_metrics() {
    let app = test_router();

    let health_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("health request should succeed");
    assert_eq!(health_response.status(), StatusCode::OK);

    metrics().record_thumbnail_generation("success", 0.01);
    metrics().thumbnail_generation_queued();
    metrics().thumbnail_generation_finished();
    metrics().record_thumbnail_recovery("queued");
    metrics().observe_thumbnail_artifact_bytes(1024);

    let response = metrics_handler().await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("metrics should be UTF-8");

    assert!(text.contains("v_note_build_info"));
    // Derived from the canonical constant, so a protocol bump cannot leave this
    // assertion silently pinning the previous version.
    assert!(text.contains(&format!("protocol=\"{}\"", protocol::PROTOCOL_VERSION)));
    assert!(text.contains("v_note_thumbnail_generation_duration_seconds"));
    assert!(text.contains("v_note_thumbnail_queue_depth"));
    assert!(text.contains("v_note_thumbnail_recoveries_total"));
    assert!(text.contains("v_note_thumbnail_artifact_bytes"));
    assert!(text.contains("version=\""));
    assert!(text.contains("v_note_http_requests_total"));
    assert!(text.contains("route=\"/health\""));
    assert!(text.contains("status=\"200\""));
}

#[tokio::test]
async fn metrics_bucket_unknown_paths_to_static_route() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/unknown/random-cardinality-path")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("fallback request should succeed");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = metrics_handler().await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("metrics should be UTF-8");

    assert!(text.contains("route=\"/static/*\""));
    assert!(!text.contains("route=\"/unknown/random-cardinality-path\""));
}
