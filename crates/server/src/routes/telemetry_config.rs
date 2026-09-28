//! `GET /api/telemetry/config` (#439): where a signed-in client sends its
//! OTLP, and the bearer token to send it with.
//!
//! Client telemetry goes to this environment's `otlp-collector-oidc` ingest —
//! a separate container the app never proxies for — and that ingest accepts
//! only a bearer access token. The SPA's session is an `HttpOnly` cookie that
//! already *is* the OIDC access token (`routes/auth.rs`), so this route hands
//! that token to JavaScript. That is the deliberate cost of the reference
//! ingest (recorded in `AGENTS.md` and `docs/DEPLOY.md`): a script running in
//! the page can now read a token it could previously only cause to be sent.
//!
//! The contract with clients (`client-export.md`, *Telemetry configuration
//! comes from the product*): **no configuration, no telemetry.** An environment
//! with `client-telemetry.endpoint` unset answers `204`, and a client treats
//! that — like a `404` from an older server, or a failed fetch — as "never
//! initialise OTLP".

use axum::Extension;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, PRAGMA};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use protocol::TelemetryConfigResponse;

use crate::AppState;
use crate::auth::{AccessToken, Claims};

#[tracing::instrument(skip_all, fields(telemetry.enabled = tracing::field::Empty))]
pub async fn telemetry_config(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Extension(token): Extension<AccessToken>,
) -> Response {
    let Some(endpoint) = state.telemetry_endpoint.as_deref() else {
        tracing::Span::current().record("telemetry.enabled", false);
        return StatusCode::NO_CONTENT.into_response();
    };
    tracing::Span::current().record("telemetry.enabled", true);
    let body = TelemetryConfigResponse {
        endpoint: endpoint.to_string(),
        access_token: token.0,
        expires_at: claims.exp,
    };
    let mut response = axum::Json(body).into_response();
    // A bearer token in a response body: never cached, by the browser or by
    // anything in between.
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

/// The configured endpoint as clients receive it: the origin with no trailing
/// slash, so `{endpoint}/v1/traces` is the path every client builds.
pub fn client_endpoint(endpoint: &url::Url) -> String {
    endpoint.as_str().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::client_endpoint;

    #[test]
    fn endpoint_is_handed_out_without_a_trailing_slash() {
        let url = url::Url::parse("https://v-notes-dev.desync.link").expect("url");
        assert_eq!(client_endpoint(&url), "https://v-notes-dev.desync.link");
        let url = url::Url::parse("https://localhost:4318/").expect("url");
        assert_eq!(client_endpoint(&url), "https://localhost:4318");
    }
}
