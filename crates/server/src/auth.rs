use std::collections::HashMap;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::CookieJar;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::Instrument as _;
use url::Url;

use crate::config::OidcConfig;

pub const AUTH_COOKIE: &str = "auth";
pub const STATE_COOKIE: &str = "auth_state";

#[derive(Clone)]
pub struct AuthConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub required_scope: String,
    pub end_session_url: Option<String>,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    /// Optional issuer URL for the Android OIDC provider (separate Authentik app).
    pub android_issuer_url: Option<String>,
    /// OAuth2 `client_id` for the Android provider. Tokens carry this in `aud`.
    pub android_client_id: Option<String>,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("required_scope", &self.required_scope)
            .field("end_session_url", &self.end_session_url)
            .field("authorize_endpoint", &self.authorize_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("jwks_uri", &self.jwks_uri)
            .field("android_issuer_url", &self.android_issuer_url)
            .field("android_client_id", &self.android_client_id)
            .finish()
    }
}

#[derive(Deserialize)]
struct DiscoveryDoc {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

impl AuthConfig {
    /// Build from the validated `oidc` config group, resolving the remaining
    /// endpoints via OIDC discovery.
    ///
    /// Presence and URL well-formedness are already guaranteed by [`OidcConfig`];
    /// this only performs the network discovery step, so the only way it fails is
    /// the issuer being unreachable or serving an unusable document.
    pub async fn from_oidc(oidc: &OidcConfig) -> Result<Self, String> {
        let issuer_url = oidc.issuer_url.to_string();

        let discovery = Self::discover(&issuer_url).await?;

        let authorize_endpoint = oidc
            .authorize_url
            .as_ref()
            .map(Url::to_string)
            .unwrap_or(discovery.authorization_endpoint);

        Ok(Self {
            issuer_url,
            client_id: oidc.client_id.clone(),
            client_secret: oidc.client_secret.clone(),
            redirect_uri: oidc.redirect_uri.to_string(),
            required_scope: oidc.required_scope.clone(),
            end_session_url: oidc.end_session_url.as_ref().map(Url::to_string),
            authorize_endpoint,
            token_endpoint: discovery.token_endpoint,
            jwks_uri: discovery.jwks_uri,
            android_issuer_url: oidc.android.issuer_url.as_ref().map(Url::to_string),
            android_client_id: oidc.android.client_id.clone(),
        })
    }

    async fn discover(issuer_url: &str) -> Result<DiscoveryDoc, String> {
        let base = issuer_url.trim_end_matches('/');
        let url = format!("{base}/.well-known/openid-configuration");
        let mut last_err = String::new();
        for attempt in 1..=10 {
            match reqwest::get(&url)
                .instrument(tracing::info_span!(
                    "http.client",
                    http.method = "GET",
                    url = %url,
                ))
                .await
            {
                Ok(resp) => match resp.error_for_status() {
                    Ok(resp) => match resp.json::<DiscoveryDoc>().await {
                        Ok(doc) => return Ok(doc),
                        Err(error) => last_err = format!("parsing JSON: {error}"),
                    },
                    Err(error) => last_err = format!("non-success status: {error}"),
                },
                Err(error) => last_err = format!("fetch failed: {error}"),
            }
            tracing::warn!(url = %url, attempt, error = %last_err, "OIDC discovery attempt failed; retrying");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err(format!(
            "OIDC discovery {url} failed after retries: {last_err}"
        ))
    }

    pub fn authorize_url(&self) -> &str {
        &self.authorize_endpoint
    }

    pub fn token_url(&self) -> &str {
        &self.token_endpoint
    }
}

pub struct JwksCache {
    keys: RwLock<HashMap<String, DecodingKey>>,
    http: reqwest::Client,
    jwks_url: String,
}

impl JwksCache {
    pub fn new(jwks_url: String) -> Self {
        Self {
            keys: RwLock::new(HashMap::new()),
            http: reqwest::Client::new(),
            jwks_url,
        }
    }

    pub fn with_keys(keys: HashMap<String, DecodingKey>) -> Self {
        Self {
            keys: RwLock::new(keys),
            http: reqwest::Client::new(),
            jwks_url: String::new(),
        }
    }

    async fn get(&self, kid: &str) -> Option<DecodingKey> {
        if let Some(key) = self.keys.read().await.get(kid).cloned() {
            return Some(key);
        }
        if let Err(error) = self.refresh().await {
            tracing::warn!(error = %error, "JWKS refresh failed");
            return None;
        }
        self.keys.read().await.get(kid).cloned()
    }

    async fn refresh(&self) -> Result<(), String> {
        let jwks: Jwks = self
            .http
            .get(&self.jwks_url)
            .send()
            .instrument(tracing::info_span!(
                "http.client",
                http.method = "GET",
                url = %self.jwks_url,
            ))
            .await
            .map_err(|error| format!("fetching JWKS: {error}"))?
            .error_for_status()
            .map_err(|error| format!("JWKS HTTP status: {error}"))?
            .json()
            .await
            .map_err(|error| format!("parsing JWKS: {error}"))?;
        let mut new_keys = HashMap::new();
        for jwk in jwks.keys {
            let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e) else {
                continue;
            };
            if let Ok(key) = DecodingKey::from_rsa_components(&n, &e) {
                new_keys.insert(kid, key);
            }
        }
        *self.keys.write().await = new_keys;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    pub iss: String,
    pub exp: u64,
}

impl Claims {
    pub fn to_me_response(&self) -> protocol::MeResponse {
        protocol::MeResponse {
            sub: self.sub.clone(),
            email: self.email.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenValidationError {
    Invalid(&'static str),
    MissingScope,
}

#[tracing::instrument(skip_all)]
pub async fn validate_jwt(
    token: &str,
    config: &AuthConfig,
    cache: &JwksCache,
) -> Result<Claims, TokenValidationError> {
    let header =
        decode_header(token).map_err(|_| TokenValidationError::Invalid("invalid JWT header"))?;
    let kid = header
        .kid
        .ok_or(TokenValidationError::Invalid("JWT missing kid header"))?;
    let key = cache
        .get(&kid)
        .await
        .ok_or(TokenValidationError::Invalid("JWT kid not in JWKS"))?;

    let allowed_algs = [Algorithm::RS256, Algorithm::RS384, Algorithm::RS512];
    if !allowed_algs.contains(&header.alg) {
        return Err(TokenValidationError::Invalid("unsupported JWT algorithm"));
    }

    let mut validation = Validation::new(header.alg);
    validation.algorithms = allowed_algs.to_vec();
    let mut audiences = vec![config.client_id.as_str()];
    if let Some(android_cid) = config.android_client_id.as_deref() {
        audiences.push(android_cid);
    }
    validation.set_audience(&audiences);
    let mut issuers = vec![config.issuer_url.as_str()];
    if let Some(android_iss) = config.android_issuer_url.as_deref() {
        issuers.push(android_iss);
    }
    validation.set_issuer(&issuers);

    let data = decode::<Claims>(token, &key, &validation).map_err(|error| {
        tracing::debug!(error = %error, "JWT validation failed");
        TokenValidationError::Invalid("JWT validation failed")
    })?;

    let scope = data.claims.scope.as_deref().unwrap_or("");
    if !scope
        .split_whitespace()
        .any(|value| value == config.required_scope)
    {
        return Err(TokenValidationError::MissingScope);
    }

    Ok(data.claims)
}

pub async fn auth_middleware(
    State(state): State<crate::AppState>,
    headers: HeaderMap,
    cookies: CookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    let auth_span = tracing::info_span!("auth.middleware");
    let token = extract_bearer(&headers).or_else(|| {
        cookies
            .get(AUTH_COOKIE)
            .map(|cookie| cookie.value().to_string())
    });
    let Some(token) = token else {
        crate::observability::metrics().record_auth_failure("missing_token");
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    };

    match validate_jwt(&token, &state.auth, &state.jwks_cache)
        .instrument(auth_span)
        .await
    {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(TokenValidationError::MissingScope) => {
            crate::observability::metrics().record_auth_failure("missing_scope");
            tracing::warn!("auth middleware rejected request: missing required scope");
            (StatusCode::FORBIDDEN, "missing required scope").into_response()
        }
        Err(TokenValidationError::Invalid(reason)) => {
            crate::observability::metrics().record_auth_failure("invalid_token");
            tracing::warn!(reason, "auth middleware rejected request");
            (StatusCode::UNAUTHORIZED, reason).into_response()
        }
    }
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty() {
        Some(token.to_string())
    } else {
        None
    }
}
