mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use base64::Engine;
use server::auth::{AuthConfig, JwksCache, validate_jwt};
use server::build_router;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::util::ServiceExt;

use common::{
    SignedTokenClaims, one_hour_from_now, sign_test_token, test_auth_config, test_jwks_cache,
    unreachable_pool,
};

#[tokio::test]
async fn me_without_token_returns_401_when_auth_enabled() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

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
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

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
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

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
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

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

/// The shape the retired standalone Android provider used to mint: its own
/// `client_id` as `aud` and its own application-slug issuer.
const LEGACY_ANDROID_CLIENT_ID: &str = "v-note-android-test";
const LEGACY_ANDROID_ISSUER: &str = "http://mock-oidc:8080/default-android";

/// #274 collapsed the two providers into one. A token from the old Android
/// provider carries a foreign `aud` *and* a foreign `iss`, and there is no
/// longer any configuration that would make the server accept it.
#[tokio::test]
async fn me_with_legacy_android_bearer_is_rejected() {
    let config = test_auth_config();
    let auth = Arc::new(config.clone());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let token = sign_test_token(SignedTokenClaims {
        sub: "android-user".to_string(),
        iss: LEGACY_ANDROID_ISSUER.to_string(),
        exp: one_hour_from_now(),
        scope: config.required_scope.clone(),
        aud: LEGACY_ANDROID_CLIENT_ID.to_string(),
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

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn validate_jwt_rejects_legacy_android_aud_and_iss() {
    let config = test_auth_config();
    let cache = test_jwks_cache();
    let token = sign_test_token(SignedTokenClaims {
        sub: "android-user".to_string(),
        iss: LEGACY_ANDROID_ISSUER.to_string(),
        exp: one_hour_from_now(),
        scope: config.required_scope.clone(),
        aud: LEGACY_ANDROID_CLIENT_ID.to_string(),
    });

    validate_jwt(&token, &config, &cache)
        .await
        .expect_err("a token from the retired Android provider must not validate");
}

/// The Android app now authenticates against the same public client as the SPA,
/// so its tokens are indistinguishable from browser ones — same `aud`, same
/// `iss`, only a different `sub`.
#[tokio::test]
async fn me_with_unified_client_bearer_from_android_returns_200() {
    let config = test_auth_config();
    let auth = Arc::new(config.clone());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let token = sign_test_token(SignedTokenClaims {
        sub: "android-user".to_string(),
        iss: config.issuer_url.clone(),
        exp: one_hour_from_now(),
        scope: config.required_scope.clone(),
        aud: config.client_id.clone(),
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

// ---------------------------------------------------------------------------
// PKCE (#274) — the unified client is public, so the verifier is the only thing
// binding an authorization code to the browser that requested it.
// ---------------------------------------------------------------------------

/// All `Set-Cookie` values on a response, as `(name, value)` pairs.
fn set_cookies(response: &axum::response::Response) -> Vec<(String, String)> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|header| header.to_str().ok())
        .filter_map(|header| {
            let first = header.split(';').next()?;
            let (name, value) = first.split_once('=')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn cookie_value(response: &axum::response::Response, name: &str) -> Option<String> {
    set_cookies(response)
        .into_iter()
        .find(|(cookie_name, _)| cookie_name == name)
        .map(|(_, value)| value)
}

async fn login_response() -> axum::response::Response {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    app.oneshot(
        Request::builder()
            .uri("/auth/login")
            .body(Body::empty())
            .expect("request should build"),
    )
    .await
    .expect("request should succeed")
}

#[tokio::test]
async fn login_sends_an_s256_pkce_challenge_matching_the_stored_verifier() {
    let response = login_response().await;

    let location = response
        .headers()
        .get("location")
        .expect("login should redirect to the IdP")
        .to_str()
        .expect("location should be ASCII");
    let authorize = url::Url::parse(location).expect("location should be a URL");
    let params: std::collections::HashMap<_, _> = authorize.query_pairs().into_owned().collect();

    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let challenge = params
        .get("code_challenge")
        .expect("login must send a PKCE challenge");

    // Recompute the challenge independently rather than trusting the server's
    // own helper: this is the assertion that PKCE is wired correctly end to end.
    let verifier = cookie_value(&response, "auth_pkce").expect("verifier cookie must be set");
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(challenge, &expected);

    // RFC 7636 §4.1 bounds the verifier at 43..=128 characters.
    assert!(
        (43..=128).contains(&verifier.len()),
        "verifier length {} outside RFC 7636 bounds",
        verifier.len()
    );
}

#[tokio::test]
async fn login_no_longer_sends_a_client_secret_and_scopes_the_verifier_cookie() {
    let response = login_response().await;

    let location = response
        .headers()
        .get("location")
        .expect("login should redirect")
        .to_str()
        .expect("location should be ASCII");
    assert!(
        !location.contains("client_secret"),
        "the authorize URL must never carry a secret: {location}"
    );

    let pkce_cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find(|header| header.starts_with("auth_pkce="))
        .expect("verifier cookie must be set");
    assert!(pkce_cookie.contains("HttpOnly"), "{pkce_cookie}");
    assert!(pkce_cookie.contains("Secure"), "{pkce_cookie}");
    assert!(pkce_cookie.contains("Path=/auth"), "{pkce_cookie}");
}

/// Two logins must not share a verifier, or one intercepted code would unlock
/// the next flow.
#[tokio::test]
async fn login_mints_a_fresh_verifier_per_flow() {
    let first = cookie_value(&login_response().await, "auth_pkce").expect("first verifier");
    let second = cookie_value(&login_response().await, "auth_pkce").expect("second verifier");
    assert_ne!(first, second);
}

/// Without the verifier cookie the exchange would degrade to a bare public-client
/// request. The callback must refuse before it ever reaches the token endpoint.
#[tokio::test]
async fn callback_without_the_verifier_cookie_is_rejected() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?code=test-code&state=test-state")
                .header("cookie", "auth_state=test-state")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The abandoned flow is ended, not left replayable for the rest of the 300 s
    // window: both transient cookies come back expired.
    assert_eq!(cookie_value(&response, "auth_state").as_deref(), Some(""));
    assert_eq!(cookie_value(&response, "auth_pkce").as_deref(), Some(""));

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("body should be UTF-8"),
        "missing PKCE verifier cookie"
    );
}

/// `state` is validated before anything is cleared. The callback is a top-level
/// GET and the flow cookies are `SameSite=Lax`, so a cross-site navigation to
/// `/auth/callback?state=…&error=…` carries them — if that path cleared cookies,
/// any page on the internet could cancel a victim's in-flight login.
#[tokio::test]
async fn callback_with_a_foreign_state_does_not_clear_an_in_flight_flow() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?state=attacker-state&error=access_denied")
                .header(
                    "cookie",
                    "auth_state=victim-state; auth_pkce=victim-verifier",
                )
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        set_cookies(&response).is_empty(),
        "an unauthenticated caller must not be able to end someone else's flow: {:?}",
        set_cookies(&response)
    );
}

/// The same request *with* the right state is allowed to end the flow.
#[tokio::test]
async fn callback_with_an_idp_error_and_matching_state_clears_the_flow() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?state=the-state&error=access_denied")
                .header("cookie", "auth_state=the-state; auth_pkce=the-verifier")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(cookie_value(&response, "auth_state").as_deref(), Some(""));
    assert_eq!(cookie_value(&response, "auth_pkce").as_deref(), Some(""));
}

/// The #372 callback verbatim. Authentik reported a provider it would not accept
/// as a bare `error=invalid_request`, and put the only diagnostic detail in
/// `error_description` — which the callback parsed past and dropped, so the logs
/// recorded the symptom and never the cause. The field is now captured for that
/// log line, and this pins that the extra parameter still deserializes and does
/// not change what the user or the flow sees.
#[tokio::test]
async fn callback_accepts_an_idp_error_description_alongside_the_error() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri(
                    "/auth/callback?state=the-state&error=invalid_request\
                     &error_description=The%20request%20is%20otherwise%20malformed",
                )
                .header("cookie", "auth_state=the-state; auth_pkce=the-verifier")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(cookie_value(&response, "auth_state").as_deref(), Some(""));
    assert_eq!(cookie_value(&response, "auth_pkce").as_deref(), Some(""));

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should read");
    assert_eq!(
        String::from_utf8_lossy(&body),
        "authentication denied: invalid_request",
    );
}

/// A state mismatch must still be caught first — the verifier is not a
/// substitute for CSRF protection on the callback.
#[tokio::test]
async fn callback_state_mismatch_is_still_rejected_with_a_verifier_present() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?code=test-code&state=attacker-state")
                .header("cookie", "auth_state=real-state; auth_pkce=some-verifier")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        set_cookies(&response).is_empty(),
        "a state mismatch must leave the real flow's cookies alone"
    );

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    assert_eq!(
        String::from_utf8(body.to_vec()).expect("body should be UTF-8"),
        "state mismatch"
    );
}

/// A one-shot stub token endpoint. Returns its bound URL and a handle that
/// yields the exact form body the server POSTed.
///
/// The e2e flow runs against `navikt/mock-oauth2-server`, which is not
/// contracted to enforce the challenge↔verifier binding — so it would stay green
/// even if the server posted the wrong value, or none. This asserts the wire
/// bytes directly.
async fn stub_token_endpoint(
    response_body: String,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub token endpoint should bind");
    let addr = listener.local_addr().expect("stub should have an address");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("stub should accept");

        // Read headers, then exactly Content-Length bytes of body.
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        let body = loop {
            let read = socket.read(&mut buf).await.expect("stub should read");
            if read == 0 {
                break String::new();
            }
            raw.extend_from_slice(&buf[..read]);
            let text = String::from_utf8_lossy(&raw).to_string();
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let length: usize = text[..header_end]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            if raw.len() - body_start >= length {
                break text[body_start..body_start + length].to_string();
            }
        };
        let _ = tx.send(body);

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });

    (format!("http://{addr}/token"), rx)
}

#[tokio::test]
async fn callback_exchanges_the_code_with_the_verifier_and_no_client_secret() {
    let verifier = "test-code-verifier-0123456789012345678901234";
    let config = test_auth_config();

    let access_token = sign_test_token(SignedTokenClaims {
        sub: "browser-user".to_string(),
        iss: config.issuer_url.clone(),
        exp: one_hour_from_now(),
        scope: config.required_scope.clone(),
        aud: config.client_id.clone(),
    });
    let (token_url, body_rx) =
        stub_token_endpoint(format!(r#"{{"access_token":"{access_token}"}}"#)).await;

    let auth = Arc::new(AuthConfig {
        token_endpoint: token_url,
        ..config
    });
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?code=the-code&state=the-state")
                .header(
                    "cookie",
                    format!("auth_state=the-state; auth_pkce={verifier}"),
                )
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    // The exchange succeeded and the session was established.
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session = cookie_value(&response, "auth").expect("session cookie must be set");
    assert_eq!(session, access_token);

    // The verifier cookie is consumed, not left replayable.
    assert_eq!(cookie_value(&response, "auth_pkce").as_deref(), Some(""));

    let body = body_rx.await.expect("stub should have captured a body");
    assert!(
        body.contains(&format!("code_verifier={verifier}")),
        "the exchange must post the verifier from the cookie, got: {body}"
    );
    assert!(
        !body.contains("client_secret"),
        "a public client must never post a secret, got: {body}"
    );
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains("code=the-code"), "{body}");
}

/// The sibling of `callback_with_a_foreign_state_does_not_clear_an_in_flight_flow`:
/// no state cookie at all. A verifier may still be present on its own, and
/// clearing it would be the same cross-site login-denial in a different shape.
#[tokio::test]
async fn callback_without_a_state_cookie_clears_nothing() {
    let auth = Arc::new(test_auth_config());
    let jwks = Arc::new(test_jwks_cache());
    let app = build_router("test-version".to_string(), auth, jwks, unreachable_pool());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auth/callback?code=c&state=attacker-state")
                .header("cookie", "auth_pkce=victim-verifier")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        set_cookies(&response).is_empty(),
        "a caller with no state cookie must not be able to expire someone's verifier: {:?}",
        set_cookies(&response)
    );
}
