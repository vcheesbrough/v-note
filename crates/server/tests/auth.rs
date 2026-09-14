mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use server::auth::{JwksCache, validate_jwt};
use server::build_router;
use std::sync::Arc;
use tower::util::ServiceExt;

use common::{
    SignedTokenClaims, android_auth_config, one_hour_from_now, sign_test_token, test_auth_config,
    test_jwks_cache,
};

#[tokio::test]
async fn me_without_token_returns_401_when_auth_enabled() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), auth, jwks);

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
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), auth, jwks);

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
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), auth, jwks);

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
async fn mobile_callback_attempts_custom_scheme_handoff() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), auth, jwks);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/mobile/callback?code=test-code&state=test-state")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let html = String::from_utf8(body.to_vec()).expect("callback HTML should be UTF-8");
    assert!(html.contains("Returning to v-note"));
    assert!(html.contains("link.desync.vnote:/oauth2redirect?code=test-code&amp;state=test-state"));
    assert!(html.contains(
        "const appUrl = 'link.desync.vnote:/oauth2redirect?code=test-code&state=test-state';"
    ));
}

#[tokio::test]
async fn me_with_android_bearer_returns_200() {
    let config = android_auth_config();
    let auth = Arc::new(config.clone());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks);

    let exp = one_hour_from_now();
    let token = sign_test_token(SignedTokenClaims {
        sub: "android-user".to_string(),
        iss: config
            .android_issuer_url
            .as_ref()
            .expect("android issuer")
            .clone(),
        exp,
        scope: config.required_scope.clone(),
        aud: config
            .android_client_id
            .as_ref()
            .expect("android client id")
            .clone(),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/me")
                .header("Authorization", format!("Bearer {token}"))
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
    assert_eq!(payload["sub"], "android-user");
}

#[tokio::test]
async fn validate_jwt_accepts_android_aud_and_iss_when_configured() {
    let config = android_auth_config();
    let cache = test_jwks_cache();
    let exp = one_hour_from_now();
    let token = sign_test_token(SignedTokenClaims {
        sub: "android-user".to_string(),
        iss: config
            .android_issuer_url
            .as_ref()
            .expect("android issuer")
            .clone(),
        exp,
        scope: config.required_scope.clone(),
        aud: config
            .android_client_id
            .as_ref()
            .expect("android client id")
            .clone(),
    });

    let claims = validate_jwt(&token, &config, &cache)
        .await
        .expect("android-shaped token should validate");
    assert_eq!(claims.sub, "android-user");
}
