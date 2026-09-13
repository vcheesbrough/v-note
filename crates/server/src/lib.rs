pub mod auth;
pub mod config;
pub mod observability;
mod routes;
mod thumbnails;

use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::{AuthConfig, JwksCache, auth_middleware};
use crate::config::{AndroidConfig, DatabaseConfig, OidcConfig, ServerConfig};
use crate::observability::request_observability_middleware;
use crate::routes::auth::{assetlinks, callback, login, logout, me, mobile_callback};
use crate::routes::pages::{create_page, delete_page, get_page, get_thumbnail, list_pages};
use crate::routes::realtime::{RealtimeHub, page_socket, realtime_socket, realtime_ticket};

#[derive(Clone)]
pub struct AppState {
    pub app_version: String,
    pub auth: Arc<AuthConfig>,
    pub jwks_cache: Arc<JwksCache>,
    pub db: Option<PgPool>,
    pub realtime: Arc<RealtimeHub>,
    /// Digital Asset Links JSON served at `/.well-known/assetlinks.json`;
    /// `None` when not configured (route returns 404).
    pub assetlinks_json: Option<Arc<str>>,
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

    Ok(build_router_full(
        app_version().to_string(),
        auth,
        jwks_cache,
        Some(pool),
        android.assetlinks_json().map(Arc::from),
        server.static_dir.clone(),
    ))
}

fn database_connect_options(database: &DatabaseConfig) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(&database.host)
        .port(database.port)
        .username(&database.user)
        .password(&database.password)
        .database(&database.name)
}

pub fn build_router(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
) -> Router {
    build_router_with_db(app_version, auth, jwks_cache, None)
}

pub fn build_router_with_db(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: Option<PgPool>,
) -> Router {
    build_router_full(app_version, auth, jwks_cache, db, None, None)
}

pub fn build_router_full(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: Option<PgPool>,
    assetlinks_json: Option<Arc<str>>,
    static_dir: Option<PathBuf>,
) -> Router {
    let state = AppState {
        app_version,
        auth,
        jwks_cache,
        db,
        realtime: Arc::new(RealtimeHub::default()),
        assetlinks_json,
    };
    thumbnails::recover_pending(state.clone());

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
        let spa_service = axum::routing::get_service(
            ServeDir::new(static_dir).not_found_service(ServeFile::new(index)),
        );
        router = router.fallback_service(spa_service);
    }

    router.layer(middleware::from_fn(request_observability_middleware))
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
