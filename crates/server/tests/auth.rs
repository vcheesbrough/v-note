use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use server::auth::{AuthConfig, JwksCache};
use server::build_router;
use std::sync::Arc;
use tower::util::ServiceExt;

fn test_auth_config() -> Arc<AuthConfig> {
    Arc::new(AuthConfig {
        issuer_url: "http://mock-oidc:8080/default".to_string(),
        client_id: "v-note-test".to_string(),
        client_secret: "test-secret".to_string(),
        redirect_uri: "https://app:443/auth/callback".to_string(),
        required_scope: "v-note:test:access".to_string(),
        end_session_url: None,
        authorize_endpoint: "http://mock-oidc:8080/default/authorize".to_string(),
        token_endpoint: "http://mock-oidc:8080/default/token".to_string(),
        jwks_uri: "http://mock-oidc:8080/default/jwks".to_string(),
    })
}

#[tokio::test]
async fn me_without_token_returns_401_when_auth_enabled() {
    let auth = test_auth_config();
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), Some(auth), Some(jwks));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_malformed_bearer_returns_401() {
    let auth = test_auth_config();
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), Some(auth), Some(jwks));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .header("Authorization", "Bearer not.a.real.jwt")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_is_public_when_auth_enabled() {
    let auth = test_auth_config();
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), Some(auth), Some(jwks));

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
}

#[tokio::test]
async fn me_with_anonymous_claim_when_auth_disabled() {
    let app = build_router("test-version".to_string(), None, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let payload: serde_json::Value =
        serde_json::from_slice(&body).expect("me response should deserialize");
    assert_eq!(payload["sub"], "anonymous");
}
