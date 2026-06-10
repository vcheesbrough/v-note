use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;

use crate::auth::{Claims, AUTH_COOKIE, STATE_COOKIE};
use crate::AppState;

const STATE_COOKIE_MAX_AGE_SECS: i64 = 300;
const AUTH_COOKIE_MAX_AGE_SECS: i64 = 60 * 60 * 24;

pub async fn login(State(state): State<AppState>, jar: CookieJar) -> Response {
    let auth = &state.auth;

    let mut nonce_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce_bytes);

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
        ],
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::error!(error = %error, "IdP authorization_endpoint URL is invalid");
            return (StatusCode::INTERNAL_SERVER_ERROR, "invalid IdP URL").into_response();
        }
    };

    let state_cookie = Cookie::build((STATE_COOKIE, nonce))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(STATE_COOKIE_MAX_AGE_SECS))
        .build();

    (jar.add(state_cookie), Redirect::to(authorize.as_str())).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

pub async fn callback(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<CallbackQuery>,
) -> Response {
    let auth = &state.auth;

    if let Some(error) = &params.error {
        tracing::warn!(error = %error, "auth callback received error");
        return (
            StatusCode::FORBIDDEN,
            format!("authentication denied: {error}"),
        )
            .into_response();
    }

    let cookie_state = jar
        .get(STATE_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let Some(cookie_state) = cookie_state else {
        return (StatusCode::BAD_REQUEST, "missing state cookie").into_response();
    };
    if cookie_state != params.state {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }

    let Some(code) = params.code else {
        return (StatusCode::BAD_REQUEST, "missing code").into_response();
    };

    let http = reqwest::Client::new();
    let token_response: TokenResponse = match http
        .post(auth.token_url())
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &auth.redirect_uri),
            ("client_id", &auth.client_id),
            ("client_secret", &auth.client_secret),
        ])
        .send()
        .await
    {
        Ok(resp) => match resp.error_for_status() {
            Ok(ok) => match ok.json().await {
                Ok(token) => token,
                Err(error) => {
                    tracing::error!(error = %error, "failed to parse token response");
                    return (StatusCode::BAD_GATEWAY, "token parse failed").into_response();
                }
            },
            Err(error) => {
                tracing::error!(error = %error, "token endpoint returned error");
                return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
            }
        },
        Err(error) => {
            tracing::error!(error = %error, "token endpoint unreachable");
            return (StatusCode::BAD_GATEWAY, "token endpoint unreachable").into_response();
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
        return (StatusCode::FORBIDDEN, "issued token failed validation").into_response();
    }

    let session = Cookie::build((AUTH_COOKIE, token_response.access_token))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::seconds(AUTH_COOKIE_MAX_AGE_SECS))
        .build();
    let clear_state = Cookie::build((STATE_COOKIE, ""))
        .path("/auth")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build();

    (jar.add(session).add(clear_state), Redirect::to("/")).into_response()
}

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

pub async fn me(Extension(claims): Extension<Claims>) -> Json<protocol::MeResponse> {
    Json(claims.to_me_response())
}

pub async fn mobile_callback() -> &'static str {
    "v-note mobile auth callback"
}

pub async fn assetlinks() -> Response {
    match std::env::var("ASSETLINKS_JSON") {
        Ok(json) if !json.is_empty() => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "assetlinks not configured").into_response(),
    }
}
