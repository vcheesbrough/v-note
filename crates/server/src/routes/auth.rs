use axum::{
    Extension, Json,
    extract::{OriginalUri, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use tracing::Instrument as _;

use crate::AppState;
use crate::auth::{AUTH_COOKIE, Claims, PKCE_COOKIE, STATE_COOKIE};

const STATE_COOKIE_MAX_AGE_SECS: i64 = 300;
const AUTH_COOKIE_MAX_AGE_SECS: i64 = 60 * 60 * 24;

/// 32 random bytes, base64url-encoded — 43 characters, within RFC 7636's 43..=128.
fn random_url_safe_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 7636 S256: `BASE64URL(SHA256(ASCII(code_verifier)))`.
fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Short-lived, `/auth`-scoped, HttpOnly cookie carrying one leg of the flow
/// (the `state` nonce or the PKCE `code_verifier`) across the IdP round trip.
fn transient_auth_cookie(name: &'static str, value: String) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(STATE_COOKIE_MAX_AGE_SECS))
        .build()
}

fn clear_transient_auth_cookie(name: &'static str) -> Cookie<'static> {
    Cookie::build((name, ""))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build()
}

/// Clear both transient cookies. An abandoned flow must not leave a replayable
/// `state`/`code_verifier` pair sitting in the browser for the rest of the
/// 300 s window — a failed callback ends the flow, so it ends the cookies too.
fn clear_flow_cookies(jar: CookieJar) -> CookieJar {
    jar.add(clear_transient_auth_cookie(STATE_COOKIE))
        .add(clear_transient_auth_cookie(PKCE_COOKIE))
}

/// Abandon the login flow: clear its cookies and answer with `status`.
fn abort_flow(jar: CookieJar, status: StatusCode, message: impl Into<String>) -> Response {
    (status, clear_flow_cookies(jar), message.into()).into_response()
}

#[tracing::instrument(skip_all)]
pub async fn login(State(state): State<AppState>, jar: CookieJar) -> Response {
    let auth = &state.auth;

    let nonce = random_url_safe_token();
    // PKCE (RFC 7636). The client is public — it has no secret — so the verifier
    // is what binds this authorization code to this browser. It never leaves the
    // server except as its S256 hash.
    let code_verifier = random_url_safe_token();
    let code_challenge = code_challenge_s256(&code_verifier);

    let authorize = match url::Url::parse_with_params(
        auth.authorize_url(),
        &[
            ("response_type", "code"),
            ("client_id", auth.client_id.as_str()),
            ("redirect_uri", auth.redirect_uri.as_str()),
            (
                "scope",
                &format!("openid profile email {}", auth.required_scope),
            ),
            ("state", &nonce),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
        ],
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(error = %error, "IdP authorization_endpoint URL is invalid");
            return (StatusCode::INTERNAL_SERVER_ERROR, "invalid IdP URL").into_response();
        }
    };

    let jar = jar
        .add(transient_auth_cookie(STATE_COOKIE, nonce))
        .add(transient_auth_cookie(PKCE_COOKIE, code_verifier));

    (jar, Redirect::to(authorize.as_str())).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: String,
    #[serde(default)]
    error: Option<String>,
    // Captured only to be logged. In #372 every environment's login failed with a
    // bare `error=invalid_request`, and the reason was carried entirely by
    // `error_description` ("The request is otherwise malformed") — which the
    // callback discarded, so the logs named the symptom and never the cause.
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Cap an IdP-supplied string before it reaches the logs.
///
/// Both `error` and `error_description` arrive on `/auth/callback`, which is
/// unauthenticated — the state check only proves the caller owns *its own*
/// cookie, which any caller can arrange. So these are attacker-controlled
/// values of unbounded length, and logging them verbatim lets one request write
/// an arbitrary amount into the log pipeline. Real IdP values are short; a
/// truncated one is still enough to diagnose with.
///
/// Truncation is on `char_indices`, not a byte slice: a byte slice can land
/// mid-codepoint and panic, which would turn a log line into a 500.
fn truncate_for_log(value: &str) -> Cow<'_, str> {
    const MAX_CHARS: usize = 200;

    match value.char_indices().nth(MAX_CHARS) {
        None => Cow::Borrowed(value),
        Some((cutoff, _)) => Cow::Owned(format!("{}…", &value[..cutoff])),
    }
}

#[tracing::instrument(skip_all)]
pub async fn callback(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<CallbackQuery>,
) -> Response {
    let auth = &state.auth;

    // `state` is checked **before** anything else, including an IdP `error`, and
    // before any cookie is cleared. The callback is a top-level GET, so `SameSite=Lax`
    // sends the flow cookies on a cross-site navigation: were an unauthenticated
    // branch to clear them, `…/auth/callback?state=x&error=y` from any page would
    // wipe a victim's in-flight login. Only a request that proves knowledge of the
    // state nonce is allowed to end the flow.
    let cookie_state = jar
        .get(STATE_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let Some(cookie_state) = cookie_state else {
        // Nothing proven, so nothing is cleared — note that `auth_pkce` may still be
        // present on its own, and clearing it here would be the same cross-site
        // login-denial the ordering above exists to prevent.
        //
        // Logged because these two rejections are the CSRF guard doing its job,
        // and a guard that fires silently cannot be told from one that never
        // fires. Neither logs the state value: the mismatching one is the
        // caller's, and echoing it into a log makes the log the payload
        // carrier the truncation below exists to prevent.
        tracing::warn!("auth callback without a state cookie");
        return (StatusCode::BAD_REQUEST, "missing state cookie").into_response();
    };
    if cookie_state != params.state {
        tracing::warn!("auth callback state did not match the state cookie");
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }

    // From here the caller has proven it owns this flow, so ending it is safe.
    if let Some(error) = &params.error {
        tracing::warn!(
            error = %truncate_for_log(error),
            error_description = %truncate_for_log(
                params.error_description.as_deref().unwrap_or("<none>")
            ),
            "auth callback received error",
        );
        // Truncated here too: the body reflects the caller's own value back, and
        // an unbounded one turns a diagnostic message into a payload carrier.
        return abort_flow(
            jar,
            StatusCode::FORBIDDEN,
            format!("authentication denied: {}", truncate_for_log(error)),
        );
    }

    // The PKCE verifier is mandatory: without it the exchange would fall back to
    // an unauthenticated public-client request, which is exactly what PKCE exists
    // to prevent. A missing cookie means a tampered or expired flow, not a
    // recoverable one.
    let code_verifier = jar
        .get(PKCE_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let Some(code_verifier) = code_verifier.filter(|verifier| !verifier.is_empty()) else {
        // Logged because this path is new (#274) and otherwise indistinguishable
        // from a generic 400: during rollout it is the signal that a browser is
        // finishing a flow it started against the pre-PKCE build.
        tracing::warn!("auth callback without a PKCE verifier cookie");
        return abort_flow(jar, StatusCode::BAD_REQUEST, "missing PKCE verifier cookie");
    };

    let Some(code) = params.code else {
        tracing::warn!("auth callback carried neither a code nor an error");
        return abort_flow(jar, StatusCode::BAD_REQUEST, "missing code");
    };

    let http = reqwest::Client::new();
    let token_response: TokenResponse = match http
        .post(auth.token_url())
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &auth.redirect_uri),
            ("client_id", &auth.client_id),
            ("code_verifier", &code_verifier),
        ])
        .send()
        .instrument(tracing::info_span!(
            "http.client",
            http.method = "POST",
            url = %auth.token_url(),
        ))
        .await
    {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => match ok.json().await {
                Ok(token) => token,
                Err(error) => {
                    tracing::error!(error = %error, "failed to parse token response");
                    return abort_flow(jar, StatusCode::BAD_GATEWAY, "token parse failed");
                }
            },
            Err(error) => {
                tracing::error!(error = %error, "token endpoint returned error");
                return abort_flow(jar, StatusCode::BAD_GATEWAY, "token exchange failed");
            }
        },
        Err(error) => {
            tracing::error!(error = %error, "token endpoint unreachable");
            return abort_flow(jar, StatusCode::BAD_GATEWAY, "token endpoint unreachable");
        }
    };

    if let Err(error) =
        crate::auth::validate_jwt(&token_response.access_token, auth, &state.jwks_cache).await
    {
        let message = match error {
            crate::auth::TokenValidationError::MissingScope => {
                "issued token missing required scope"
            }
            crate::auth::TokenValidationError::Invalid(reason) => reason,
        };
        tracing::warn!(reason = message, "issued access token failed validation");
        return abort_flow(jar, StatusCode::FORBIDDEN, "issued token failed validation");
    }

    let session = Cookie::build((AUTH_COOKIE, token_response.access_token))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(AUTH_COOKIE_MAX_AGE_SECS))
        .build();
    (clear_flow_cookies(jar).add(session), Redirect::to("/")).into_response()
}

#[tracing::instrument(skip_all)]
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> Response {
    let clear = Cookie::build((AUTH_COOKIE, ""))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build();
    let jar = jar.add(clear);

    let target = state
        .auth
        .end_session_url
        .clone()
        .unwrap_or_else(|| "/".to_string());

    (jar, Redirect::to(&target)).into_response()
}

#[tracing::instrument(skip_all)]
pub async fn me(Extension(claims): Extension<Claims>) -> Json<protocol::MeResponse> {
    Json(claims.to_me_response())
}

#[tracing::instrument(skip_all)]
pub async fn mobile_callback(OriginalUri(uri): OriginalUri) -> Html<String> {
    let custom_scheme_url = match uri.query() {
        Some(query) if !query.is_empty() => {
            format!("link.desync.vnote:/oauth2redirect?{query}")
        }
        _ => "link.desync.vnote:/oauth2redirect".to_string(),
    };
    let escaped_custom_scheme_url = escape_html(&custom_scheme_url);
    let js_custom_scheme_url = escape_js_string(&custom_scheme_url);

    Html(format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>v-note mobile auth callback</title>
  </head>
  <body style="font-family: system-ui, sans-serif; padding: 2rem; line-height: 1.5;">
    <p>Returning to v-note…</p>
    <p><a href="{escaped_custom_scheme_url}">Open the app</a></p>
    <script>
      const appUrl = '{js_custom_scheme_url}';
      window.location.replace(appUrl);
      window.setTimeout(() => window.location.assign(appUrl), 250);
    </script>
  </body>
</html>"#,
    ))
}

#[tracing::instrument(skip_all)]
pub async fn assetlinks(State(state): State<AppState>) -> Response {
    match state.assetlinks_json.as_deref() {
        Some(json) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json.to_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "assetlinks not configured").into_response(),
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_js_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::truncate_for_log;

    #[test]
    fn short_values_are_passed_through_unchanged() {
        let value = "The request is otherwise malformed";
        assert_eq!(truncate_for_log(value), value);
    }

    #[test]
    fn long_values_are_capped() {
        // Unbounded here means one request can write as much as it likes into
        // the log pipeline; /auth/callback is unauthenticated.
        let value = "a".repeat(10_000);
        let truncated = truncate_for_log(&value);

        assert_eq!(
            truncated.chars().count(),
            201,
            "200 chars plus the ellipsis"
        );
        assert!(truncated.ends_with('…'));
    }

    /// A byte-slice truncation would panic here instead of logging: the cut at
    /// 200 bytes lands inside a multi-byte character. A panic in the log line
    /// would turn a diagnosable 403 into a 500.
    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        for filler in ["é", "→", "😀"] {
            let value = filler.repeat(500);
            let truncated = truncate_for_log(&value);

            assert_eq!(truncated.chars().count(), 201, "filler {filler}");
            assert!(
                truncated
                    .trim_end_matches('…')
                    .chars()
                    .all(|c| value.contains(c)),
                "filler {filler} was corrupted",
            );
        }
    }

    #[test]
    fn a_value_exactly_at_the_cap_is_not_truncated() {
        let value = "x".repeat(200);
        assert_eq!(truncate_for_log(&value), value);
    }
}
