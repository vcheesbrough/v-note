use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde::Serialize;
use server::auth::{validate_jwt, AuthConfig, JwksCache};
use server::build_router;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::util::ServiceExt;

const TEST_JWT_KID: &str = "test-kid";
const TEST_RSA_PRIVATE_PEM: &str = include_str!("fixtures/test_rsa_private.pem");
const TEST_RSA_N: &str =
    "n2LSwWaKa37_PfC0fQehlQkhj4KFZc5htmDM5PDWOvnwuxmQ9AC48APxN-p1gjxR6O7MRsui-73c2pbk2Fp7nLQPmhupMEMw2bXDKV3iUaqppBGgMnbG43RnK6ho814E1aeaDdoicAlOUZQhp2PkRRd-2xemtazForez00ig-HN7W_JAh00ZaXP6JifiPqseSLKB1DnaNj1rIxfyPBdxrKyvDtKTudT3pn1yI8Wkl3mI57upEG7CCssZcLmKhWB3dMdHOaT2dnFqeOka4e3dt7i6jJj6h7LuAb4mfuYYAfJDhq5Ls8x9kb_I4U2NoCLYUtC3UlnwUIacvLzp4_iVkQ";
const TEST_RSA_E: &str = "AQAB";

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
        android_issuer_url: None,
        android_client_id: None,
    })
}

fn android_auth_config() -> AuthConfig {
    AuthConfig {
        issuer_url: "http://mock-oidc:8080/default".to_string(),
        client_id: "v-note-test".to_string(),
        client_secret: "test-secret".to_string(),
        redirect_uri: "https://app:443/auth/callback".to_string(),
        required_scope: "v-note:test:access".to_string(),
        end_session_url: None,
        authorize_endpoint: "http://mock-oidc:8080/default/authorize".to_string(),
        token_endpoint: "http://mock-oidc:8080/default/token".to_string(),
        jwks_uri: "http://mock-oidc:8080/default/jwks".to_string(),
        android_issuer_url: Some("http://mock-oidc:8080/default-android".to_string()),
        android_client_id: Some("v-note-android-test".to_string()),
    }
}

fn test_jwks_cache() -> JwksCache {
    let decoding_key =
        DecodingKey::from_rsa_components(TEST_RSA_N, TEST_RSA_E).expect("test RSA components");
    let mut keys = HashMap::new();
    keys.insert(TEST_JWT_KID.to_string(), decoding_key);
    JwksCache::with_keys(keys)
}

#[derive(Serialize)]
struct SignedTokenClaims {
    sub: String,
    iss: String,
    exp: u64,
    scope: String,
    aud: String,
}

fn sign_test_token(claims: SignedTokenClaims) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_JWT_KID.to_string());
    let encoding_key =
        EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).expect("test RSA private key");
    encode(&header, &claims, &encoding_key).expect("token should encode")
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

#[tokio::test]
async fn me_with_android_bearer_returns_200() {
    let config = android_auth_config();
    let auth = Arc::new(config.clone());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), Some(auth), Some(jwks));

    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_secs()
        + 3600;
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
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_secs()
        + 3600;
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
