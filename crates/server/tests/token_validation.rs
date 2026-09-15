//! Pins the token checks `validate_jwt` depends on, so a jsonwebtoken upgrade that
//! quietly loosens a validation default fails here rather than in production.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::json;
use server::auth::{AuthConfig, TokenValidationError, validate_jwt};
use server::build_router;
use std::sync::Arc;
use tower::util::ServiceExt;

use common::{
    SignedTokenClaims, TEST_JWT_KID, TEST_RSA_N, one_hour_from_now, sign_test_token,
    test_auth_config, test_jwks_cache, unreachable_pool,
};

const VALIDATION_FAILED: TokenValidationError =
    TokenValidationError::Invalid("JWT validation failed");

async fn rejection(token: &str, config: &AuthConfig) -> TokenValidationError {
    validate_jwt(token, config, &test_jwks_cache())
        .await
        .expect_err("token should be rejected")
}

#[tokio::test]
async fn accepts_web_token() {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims::valid_for(&config, "web-user"));

    let claims = validate_jwt(&token, &config, &test_jwks_cache())
        .await
        .expect("web token should validate");
    assert_eq!(claims.sub, "web-user");
}

#[tokio::test]
async fn rejects_token_without_aud() {
    let config = test_auth_config();
    let token = sign_test_token(json!({
        "sub": "web-user",
        "iss": config.issuer_url,
        "exp": one_hour_from_now(),
        "scope": config.required_scope,
    }));

    assert_eq!(rejection(&token, &config).await, VALIDATION_FAILED);
}

#[tokio::test]
async fn rejects_foreign_audience() {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims {
        aud: "some-other-client".to_string(),
        ..SignedTokenClaims::valid_for(&config, "web-user")
    });

    assert_eq!(rejection(&token, &config).await, VALIDATION_FAILED);
}

#[tokio::test]
async fn rejects_foreign_issuer() {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims {
        iss: "http://mock-oidc:8080/some-other-issuer".to_string(),
        ..SignedTokenClaims::valid_for(&config, "web-user")
    });

    assert_eq!(rejection(&token, &config).await, VALIDATION_FAILED);
}

#[tokio::test]
async fn rejects_expired_token() {
    let config = test_auth_config();
    // An hour past expiry — well beyond the library's 60s leeway.
    let token = sign_test_token(SignedTokenClaims {
        exp: one_hour_from_now() - 2 * 3600,
        ..SignedTokenClaims::valid_for(&config, "web-user")
    });

    assert_eq!(rejection(&token, &config).await, VALIDATION_FAILED);
}

#[tokio::test]
async fn rejects_payload_swapped_under_genuine_signature() {
    let config = test_auth_config();
    let genuine = sign_test_token(SignedTokenClaims::valid_for(&config, "web-user"));
    let other = sign_test_token(SignedTokenClaims::valid_for(&config, "someone-else"));
    let genuine: Vec<&str> = genuine.split('.').collect();
    let other: Vec<&str> = other.split('.').collect();
    let forged = format!("{}.{}.{}", genuine[0], other[1], genuine[2]);

    assert_eq!(rejection(&forged, &config).await, VALIDATION_FAILED);
}

#[tokio::test]
async fn rejects_hmac_token_keyed_with_public_key_material() {
    let config = test_auth_config();
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(TEST_JWT_KID.to_string());
    let token = encode(
        &header,
        &SignedTokenClaims::valid_for(&config, "web-user"),
        &EncodingKey::from_secret(TEST_RSA_N.as_bytes()),
    )
    .expect("token should encode");

    assert_eq!(
        rejection(&token, &config).await,
        TokenValidationError::Invalid("unsupported JWT algorithm")
    );
}

#[tokio::test]
async fn reports_missing_scope_separately() {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims {
        scope: "openid profile".to_string(),
        ..SignedTokenClaims::valid_for(&config, "web-user")
    });

    assert_eq!(
        rejection(&token, &config).await,
        TokenValidationError::MissingScope
    );
}

#[tokio::test]
async fn me_with_expired_bearer_returns_401() {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims {
        exp: one_hour_from_now() - 2 * 3600,
        ..SignedTokenClaims::valid_for(&config, "web-user")
    });
    let app = build_router(
        "test-version".to_string(),
        Arc::new(config),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
    );

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

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
