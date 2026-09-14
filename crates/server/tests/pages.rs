//! Page-route error paths that need no database.
//!
//! Every handler in `routes::pages` now returns `ApiError` rather than a fully
//! built `Response` (clippy's `result_large_err`). `db()` is the one error path
//! reachable without Postgres, so it is where the rewritten `Err` variant gets
//! exercised end to end — through the real router, middleware and all.
//! The database-backed paths (403 page-not-found, 410 thumbnail-gone) are
//! covered by the Playwright suite in `e2e/tests/`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
use protocol::{CreatePageRequest, Paper};
use serde::Serialize;
use server::auth::{AuthConfig, JwksCache};
use server::build_router;
use tower::util::ServiceExt;

const TEST_JWT_KID: &str = "test-kid";
const TEST_RSA_PRIVATE_PEM: &str = include_str!("fixtures/test_rsa_private.pem");
const TEST_RSA_N: &str = "n2LSwWaKa37_PfC0fQehlQkhj4KFZc5htmDM5PDWOvnwuxmQ9AC48APxN-p1gjxR6O7MRsui-73c2pbk2Fp7nLQPmhupMEMw2bXDKV3iUaqppBGgMnbG43RnK6ho814E1aeaDdoicAlOUZQhp2PkRRd-2xemtazForez00ig-HN7W_JAh00ZaXP6JifiPqseSLKB1DnaNj1rIxfyPBdxrKyvDtKTudT3pn1yI8Wkl3mI57upEG7CCssZcLmKhWB3dMdHOaT2dnFqeOka4e3dt7i6jJj6h7LuAb4mfuYYAfJDhq5Ls8x9kb_I4U2NoCLYUtC3UlnwUIacvLzp4_iVkQ";
const TEST_RSA_E: &str = "AQAB";

#[derive(Serialize)]
struct SignedTokenClaims {
    sub: String,
    iss: String,
    exp: u64,
    scope: String,
    aud: String,
}

fn test_auth_config() -> AuthConfig {
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

/// A router with `db: None` — exactly the state `db()` rejects.
fn router_without_db() -> (axum::Router, String) {
    let config = test_auth_config();
    let decoding_key =
        DecodingKey::from_rsa_components(TEST_RSA_N, TEST_RSA_E).expect("test RSA components");
    let mut keys = HashMap::new();
    keys.insert(TEST_JWT_KID.to_string(), decoding_key);

    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_secs()
        + 3600;
    let mut jwt_header = Header::new(Algorithm::RS256);
    jwt_header.kid = Some(TEST_JWT_KID.to_string());
    let token = encode(
        &jwt_header,
        &SignedTokenClaims {
            sub: "page-user".to_string(),
            iss: config.issuer_url.clone(),
            exp,
            scope: config.required_scope.clone(),
            aud: config.client_id.clone(),
        },
        &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_PEM.as_bytes()).expect("test RSA private key"),
    )
    .expect("token should encode");

    let app = build_router(
        "test-version".to_string(),
        Arc::new(config),
        Arc::new(JwksCache::with_keys(keys)),
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
