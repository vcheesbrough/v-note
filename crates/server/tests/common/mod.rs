//! Fixtures shared by the integration test binaries.
//!
//! Each file under `tests/` is its own crate, so there is no way to share an
//! `AuthConfig` or a signing key between them except through a module every one
//! of them declares. The RSA components and the PEM must agree exactly or every
//! signed token fails validation, which is precisely the pair that goes stale
//! when it is copied per file.
//!
//! Not every binary uses every item here, hence the blanket `dead_code` allow:
//! `mod common;` compiles the whole module into each one, so anything a given
//! test file does not call would otherwise warn.

#![allow(dead_code)]

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
use serde::Serialize;
use server::auth::{AuthConfig, JwksCache};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub const TEST_JWT_KID: &str = "test-kid";
pub const TEST_RSA_PRIVATE_PEM: &str = include_str!("../fixtures/test_rsa_private.pem");
pub const TEST_RSA_N: &str = "n2LSwWaKa37_PfC0fQehlQkhj4KFZc5htmDM5PDWOvnwuxmQ9AC48APxN-p1gjxR6O7MRsui-73c2pbk2Fp7nLQPmhupMEMw2bXDKV3iUaqppBGgMnbG43RnK6ho814E1aeaDdoicAlOUZQhp2PkRRd-2xemtazForez00ig-HN7W_JAh00ZaXP6JifiPqseSLKB1DnaNj1rIxfyPBdxrKyvDtKTudT3pn1yI8Wkl3mI57upEG7CCssZcLmKhWB3dMdHOaT2dnFqeOka4e3dt7i6jJj6h7LuAb4mfuYYAfJDhq5Ls8x9kb_I4U2NoCLYUtC3UlnwUIacvLzp4_iVkQ";
pub const TEST_RSA_E: &str = "AQAB";

/// The web client's OIDC configuration — no Android issuer or client id.
pub fn test_auth_config() -> AuthConfig {
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
        android_issuer_url: None,
        android_client_id: None,
    }
}

/// [`test_auth_config`] plus the second issuer/audience pair the Android client
/// authenticates with.
pub fn android_auth_config() -> AuthConfig {
    AuthConfig {
        android_issuer_url: Some("http://mock-oidc:8080/default-android".to_string()),
        android_client_id: Some("v-note-android-test".to_string()),
        ..test_auth_config()
    }
}

/// A Postgres pool that never connects. The router requires a pool; tests that
/// do not exercise the database hand it this one. Port 1 on loopback refuses at
/// once and the short acquire timeout bounds the wait, so a test that does reach
/// a query gets a prompt 500 rather than a hang. Lazy: needs a Tokio runtime,
/// not Postgres.
pub fn unreachable_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy_with(PgConnectOptions::new().host("127.0.0.1").port(1))
}

/// A JWKS cache pre-seeded with the fixture key, so no HTTP fetch is attempted.
pub fn test_jwks_cache() -> JwksCache {
    let decoding_key =
        DecodingKey::from_rsa_components(TEST_RSA_N, TEST_RSA_E).expect("test RSA components");
    let mut keys = HashMap::new();
    keys.insert(TEST_JWT_KID.to_string(), decoding_key);
    JwksCache::with_keys(keys)
}

#[derive(Serialize)]
pub struct SignedTokenClaims {
    pub sub: String,
    pub iss: String,
    pub exp: u64,
    pub scope: String,
    pub aud: String,
}

impl SignedTokenClaims {
    /// A token for `sub` that `config` accepts on its web issuer, expiring an
    /// hour out.
    pub fn valid_for(config: &AuthConfig, sub: &str) -> Self {
        Self {
            sub: sub.to_string(),
            iss: config.issuer_url.clone(),
            exp: one_hour_from_now(),
            scope: config.required_scope.clone(),
            aud: config.client_id.clone(),
        }
    }
}

pub fn one_hour_from_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_secs()
        + 3600
}

/// Signs any claims body with the fixture key — usually a [`SignedTokenClaims`],
/// or a `serde_json::json!` object when a test needs a claim left out entirely.
pub fn sign_test_token(claims: impl Serialize) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(TEST_JWT_KID.to_string());
    let encoding_key =
        EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).expect("test RSA private key");
    encode(&header, &claims, &encoding_key).expect("token should encode")
}
