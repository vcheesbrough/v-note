pub mod auth;
pub mod config;
pub mod csp;
pub mod observability;
mod realtime;
mod routes;
mod thumbnails;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::header::CONTENT_SECURITY_POLICY;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::{AuthConfig, JwksCache, auth_middleware};
use crate::config::{
    AndroidConfig, ClientTelemetryConfig, DatabaseConfig, OidcConfig, RealtimeConfig, ServerConfig,
};
use crate::observability::request_observability_middleware;
use crate::realtime::{RealtimeHub, page_socket, realtime_socket, realtime_ticket};
use crate::routes::auth::{assetlinks, callback, login, logout, me, mobile_callback};
use crate::routes::pages::{create_page, delete_page, get_page, get_thumbnail, list_pages};
use crate::routes::telemetry_config::telemetry_config;

#[derive(Clone)]
pub struct AppState {
    pub app_version: String,
    pub auth: Arc<AuthConfig>,
    pub jwks_cache: Arc<JwksCache>,
    pub db: PgPool,
    pub realtime: Arc<RealtimeHub>,
    /// Digital Asset Links JSON served at `/.well-known/assetlinks.json`;
    /// `None` when not configured (route returns 404).
    pub assetlinks_json: Option<Arc<str>>,
    /// `realtime.coalesce-replay` (#323): send a `subscribe` replay as one
    /// `page-replay` frame rather than a frame per stored batch.
    pub coalesce_replay: bool,
    /// `realtime.compression` (#342): offer `permessage-deflate` on the
    /// realtime upgrades. Read once per upgrade, so flipping it only affects
    /// connections opened afterwards.
    pub realtime_compression: bool,
    /// Where signed-in clients send their OTLP (#439): this environment's
    /// `otlp-collector-oidc` ingest, as `GET /api/telemetry/config` hands it
    /// out. `None` is "client telemetry off" — the route answers `204`.
    pub telemetry_endpoint: Option<Arc<str>>,
}

/// The build version: the release tag baked in at compile time (`V_NOTE_RELEASE`,
/// injected by `Dockerfile.web`), falling back to the crate version for local builds.
/// A build constant, not runtime config — mirrors how the SPA sources its version.
pub fn app_version() -> &'static str {
    match option_env!("V_NOTE_RELEASE") {
        Some(release) if !release.is_empty() => release,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// A startup dependency that could not be brought up.
///
/// Startup failures are reported, not panicked: `main` already returns `Result`
/// and prints a single clean line for a bad config value, so an unreachable
/// dependency should exit the same way rather than unwinding with a backtrace.
pub enum StartupError {
    OidcDiscovery(String),
    DatabaseConnect(sqlx::Error),
    DatabaseMigrate(sqlx::migrate::MigrateError),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartupError::OidcDiscovery(reason) => {
                write!(f, "OIDC discovery failed for `oidc.issuer-url`: {reason}")
            }
            StartupError::DatabaseConnect(source) => {
                write!(f, "database should be reachable: {source}")
            }
            StartupError::DatabaseMigrate(source) => {
                write!(f, "database migrations should apply: {source}")
            }
        }
    }
}

/// Startup failures surface through `Termination`, which prints `Debug`. Delegate
/// to `Display` so an operator sees the one-line reason, matching `ConfigError`.
impl std::fmt::Debug for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartupError::OidcDiscovery(_) => None,
            StartupError::DatabaseConnect(source) => Some(source),
            StartupError::DatabaseMigrate(source) => Some(source),
        }
    }
}

/// Assemble the application router from its already-loaded config groups.
///
/// Takes the validated DTOs directly — how they were sourced (sovereign-config,
/// env, a test fixture) is the caller's concern, not this module's. Connects the
/// database, runs migrations, and resolves OIDC discovery.
pub async fn build_app_router(
    database: &DatabaseConfig,
    oidc: &OidcConfig,
    android: &AndroidConfig,
    server: &ServerConfig,
    realtime: &RealtimeConfig,
    client_telemetry: &ClientTelemetryConfig,
) -> Result<Router, StartupError> {
    let auth = Arc::new(
        AuthConfig::from_oidc(oidc)
            .await
            .map_err(StartupError::OidcDiscovery)?,
    );
    let jwks_cache = Arc::new(JwksCache::new(auth.jwks_uri.clone()));

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(database_connect_options(database))
        .await
        .map_err(StartupError::DatabaseConnect)?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(StartupError::DatabaseMigrate)?;

    let state = AppState {
        app_version: app_version().to_string(),
        auth,
        jwks_cache,
        db: pool,
        realtime: Arc::new(RealtimeHub::default()),
        assetlinks_json: android.assetlinks_json().map(Arc::from),
        coalesce_replay: realtime.coalesce_replay,
        realtime_compression: realtime.compression,
        telemetry_endpoint: telemetry_endpoint(client_telemetry.ingest_endpoint()),
    };
    // Startup work, not router construction: resume thumbnail jobs a restart
    // interrupted. Kept out of `router` so tests, whose pool never connects,
    // build routers without launching a recovery query.
    thumbnails::recover_pending(state.clone());
    Ok(router(state, server.static_dir.clone()))
}

/// Logged once at startup: whether clients will be told to send telemetry is
/// what someone reading a quiet log after changing the leaf wants to confirm —
/// config is snapshotted at startup, so a change takes effect on a restart.
fn telemetry_endpoint(endpoint: Option<&url::Url>) -> Option<Arc<str>> {
    match endpoint {
        Some(endpoint) => {
            let endpoint = routes::telemetry_config::client_endpoint(endpoint);
            tracing::info!(endpoint = %endpoint, "client telemetry configured for clients");
            Some(Arc::from(endpoint))
        }
        None => {
            tracing::info!("client telemetry not configured; /api/telemetry/config answers 204");
            None
        }
    }
}

fn database_connect_options(database: &DatabaseConfig) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(&database.host)
        .port(database.port)
        .username(&database.user)
        .password(&database.password)
        .database(&database.name)
}

/// The application router over a caller-supplied pool, with no Android asset
/// links and no static SPA directory — what the integration tests drive.
/// Production goes through [`build_app_router`].
pub fn build_router(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: PgPool,
) -> Router {
    router(test_state(app_version, auth, jwks_cache, db), None)
}

/// As [`build_router`], with `client-telemetry.endpoint` set to `endpoint`
/// (#439) — or unset, which is what [`build_router`] gets.
pub fn build_router_with_telemetry_endpoint(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: PgPool,
    endpoint: Option<url::Url>,
) -> Router {
    router(
        AppState {
            telemetry_endpoint: telemetry_endpoint(endpoint.as_ref()),
            ..test_state(app_version, auth, jwks_cache, db)
        },
        None,
    )
}

/// As [`build_router_with_telemetry_endpoint`], also serving the SPA from
/// `static_dir` the way the image does — what the CSP tests (#444) drive.
pub fn build_router_with_spa(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: PgPool,
    endpoint: Option<url::Url>,
    static_dir: PathBuf,
) -> Router {
    router(
        AppState {
            telemetry_endpoint: telemetry_endpoint(endpoint.as_ref()),
            ..test_state(app_version, auth, jwks_cache, db)
        },
        Some(static_dir),
    )
}

/// The state the integration-test routers share: no asset links, realtime
/// defaults, and client telemetry off.
fn test_state(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: PgPool,
) -> AppState {
    AppState {
        app_version,
        auth,
        jwks_cache,
        db,
        realtime: Arc::new(RealtimeHub::default()),
        assetlinks_json: None,
        coalesce_replay: RealtimeConfig::default().coalesce_replay,
        realtime_compression: RealtimeConfig::default().compression,
        telemetry_endpoint: None,
    }
}

fn router(state: AppState, static_dir: Option<PathBuf>) -> Router {
    let public_api = Router::new()
        .route("/meta", get(meta))
        .with_state(state.clone());

    let protected_api = Router::new()
        .route("/me", get(me))
        .route("/pages", get(list_pages).post(create_page))
        .route("/pages/{page_id}", get(get_page).delete(delete_page))
        .route(
            "/pages/{page_id}/thumbnails/{source_seq}",
            get(get_thumbnail),
        )
        .route("/realtime-ticket", post(realtime_ticket))
        .route("/telemetry/config", get(telemetry_config))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    let mut router = Router::new()
        .route("/health", get(health))
        .route(
            "/.well-known/assetlinks.json",
            get(assetlinks).with_state(state.clone()),
        )
        .nest(
            "/auth",
            Router::new()
                .route("/login", get(login))
                .route("/callback", get(callback))
                .route("/logout", get(logout))
                .route("/mobile/callback", get(mobile_callback))
                .with_state(state.clone()),
        )
        .nest("/api", public_api.merge(protected_api))
        .route(
            "/api/realtime",
            get(realtime_socket).with_state(state.clone()),
        )
        .route(
            "/api/pages/{page_id}/realtime",
            get(page_socket).with_state(state.clone()),
        );

    if let Some(static_dir) = static_dir {
        let index = static_dir.join("index.html");
        let policy = spa_content_security_policy(&index, state.telemetry_endpoint.as_deref());
        let spa_service = axum::routing::get_service(
            ServeDir::new(static_dir).not_found_service(ServeFile::new(index)),
        )
        .layer(SetResponseHeaderLayer::overriding(
            CONTENT_SECURITY_POLICY,
            policy,
        ));
        router = router.fallback_service(spa_service);
    }

    router.layer(middleware::from_fn(request_observability_middleware))
}

/// The SPA's CSP (#444), for every response the static service sends —
/// `index.html` under any deep link, and the assets it loads. Built once from
/// the `index.html` this process will serve, so the bootstrap hash always
/// matches the build in the image; and from the telemetry endpoint, whose
/// origin the page must be allowed to `fetch`.
fn spa_content_security_policy(
    index: &std::path::Path,
    telemetry_endpoint: Option<&str>,
) -> axum::http::HeaderValue {
    let script_hashes = match std::fs::read_to_string(index) {
        Ok(html) => csp::inline_script_hashes(&html),
        Err(error) => {
            // Not fatal: without an index.html there is no SPA to protect, and
            // the static service already answers 404 for it.
            tracing::warn!(path = %index.display(), %error, "SPA index.html unreadable; CSP allows no inline script");
            Vec::new()
        }
    };
    let connect_origins: Vec<String> = telemetry_endpoint
        .and_then(|endpoint| url::Url::parse(endpoint).ok())
        .map(|endpoint| endpoint.origin().ascii_serialization())
        .into_iter()
        .collect();
    tracing::info!(
        inline_scripts = script_hashes.len(),
        connect_origins = ?connect_origins,
        "SPA content security policy built"
    );
    csp::spa_policy_header(&script_hashes, &connect_origins)
}

#[tracing::instrument(skip_all)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

#[tracing::instrument(skip_all)]
async fn meta(axum::extract::State(state): axum::extract::State<AppState>) -> Json<MetaResponse> {
    Json(MetaResponse {
        app_name: "v-note".to_string(),
        app_version: state.app_version,
        protocol_version: PROTOCOL_VERSION,
    })
}

/// A pool that never connects, for in-crate unit tests: port 1 on loopback
/// refuses at once, and the short acquire timeout keeps a test that does reach
/// the database fast. Lazy, so it needs a Tokio runtime but no Postgres.
#[cfg(test)]
pub(crate) fn unreachable_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy_with(PgConnectOptions::new().host("127.0.0.1").port(1))
}

#[cfg(test)]
impl AppState {
    /// State for in-crate unit tests: placeholder OIDC settings, an empty JWKS
    /// cache, and [`unreachable_pool`].
    pub(crate) fn for_tests() -> Self {
        Self {
            app_version: "test".to_string(),
            auth: Arc::new(AuthConfig {
                issuer_url: "http://mock-oidc:8080/default".to_string(),
                client_id: "v-note-test".to_string(),
                redirect_uri: "https://app:443/auth/callback".to_string(),
                required_scope: "v-note:test:access".to_string(),
                end_session_url: None,
                authorize_endpoint: "http://mock-oidc:8080/default/authorize".to_string(),
                token_endpoint: "http://mock-oidc:8080/default/token".to_string(),
                jwks_uri: "http://mock-oidc:8080/default/jwks".to_string(),
            }),
            jwks_cache: Arc::new(JwksCache::with_keys(std::collections::HashMap::new())),
            db: unreachable_pool(),
            realtime: Arc::new(RealtimeHub::default()),
            assetlinks_json: None,
            coalesce_replay: RealtimeConfig::default().coalesce_replay,
            realtime_compression: RealtimeConfig::default().compression,
            telemetry_endpoint: None,
        }
    }
}
