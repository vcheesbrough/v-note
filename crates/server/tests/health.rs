use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use protocol::HealthResponse;
use server::auth::{AuthConfig, JwksCache};
use server::build_router;
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
