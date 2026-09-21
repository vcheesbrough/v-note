pub mod auth;
pub mod config;
pub mod observability;
mod realtime;
mod routes;
mod thumbnails;

use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::{AuthConfig, JwksCache, auth_middleware};
use crate::config::{
    AndroidConfig, ClientTelemetryConfig, ClientTelemetryUpstreams, DatabaseConfig, OidcConfig,
    RealtimeConfig, ServerConfig,
};
use crate::observability::request_observability_middleware;
use crate::realtime::{RealtimeHub, page_socket, realtime_socket, realtime_ticket};
use crate::routes::auth::{assetlinks, callback, login, logout, me, mobile_callback};
use crate::routes::pages::{create_page, delete_page, get_page, get_thumbnail, list_pages};
use crate::routes::telemetry::ClientTelemetryIngress;

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
    /// The `/otlp` client telemetry ingress (#354). `None` **is** the kill
    /// switch (`client-telemetry.enabled`): with no upstreams there is nothing
    /// to forward to, and every `/otlp` request is a 404.
    pub client_telemetry: Option<Arc<ClientTelemetryIngress>>,
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
        client_telemetry: client_telemetry_ingress(client_telemetry.upstreams()),
    };
    // Startup work, not router construction: resume thumbnail jobs a restart
    // interrupted. Kept out of `router` so tests, whose pool never connects,
    // build routers without launching a recovery query.
    thumbnails::recover_pending(state.clone());
    Ok(router(state, server.static_dir.clone()))
}

/// Logged once, at `info`, because whether ingest is on is exactly what someone
/// reading a quiet startup log after flipping the switch wants to confirm — the
/// config is snapshotted at startup, so the switch only takes on a restart.
fn client_telemetry_ingress(
    upstreams: Option<ClientTelemetryUpstreams>,
) -> Option<Arc<ClientTelemetryIngress>> {
    match upstreams {
        Some(upstreams) => {
            tracing::info!(
                spa_endpoint = %upstreams.spa,
                android_endpoint = %upstreams.android,
                "client telemetry ingress enabled"
            );
            Some(Arc::new(ClientTelemetryIngress::new(upstreams)))
        }
        None => {
            tracing::info!("client telemetry ingress disabled; /otlp answers 404");
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
    build_router_with_client_telemetry(app_version, auth, jwks_cache, db, None)
}

/// As [`build_router`], with the `/otlp` ingress pointed at `upstreams` — or
/// switched off, which is what [`build_router`] gets. A sibling rather than a
/// fifth parameter so the tests that have nothing to do with telemetry do not
/// each have to say so.
pub fn build_router_with_client_telemetry(
    app_version: String,
    auth: Arc<AuthConfig>,
    jwks_cache: Arc<JwksCache>,
    db: PgPool,
    upstreams: Option<ClientTelemetryUpstreams>,
) -> Router {
    router(
        AppState {
            app_version,
            auth,
            jwks_cache,
            db,
            realtime: Arc::new(RealtimeHub::default()),
            assetlinks_json: None,
            coalesce_replay: RealtimeConfig::default().coalesce_replay,
            realtime_compression: RealtimeConfig::default().compression,
            client_telemetry: upstreams
                .map(|upstreams| Arc::new(ClientTelemetryIngress::new(upstreams))),
        },
        None,
    )
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
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    // Layers run outermost-first and the last one added is outermost, so the
    // kill switch is consulted *before* authentication. See `enabled_gate` for
    // why that order is load-bearing. The fallback keeps every other path under
    // `/otlp` away from the SPA's catch-all below, which would answer a stray
    // `GET /otlp/...` with `200 index.html`.
    let client_telemetry = Router::new()
        .route("/{client}/v1/{signal}", post(routes::telemetry::ingest))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            routes::telemetry::enabled_gate,
        ))
        .fallback(routes::telemetry::not_found)
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
        .nest("/otlp", client_telemetry)
        // Not covered by the nest above, and not redundant with its fallback:
        // axum registers a nest as `/otlp/{*tail}`, and a catch-all does not
        // match an empty tail — so `/otlp/` would otherwise fall through to the
        // SPA and be answered with `index.html` (found by e2e, #354).
        .route("/otlp/", any(routes::telemetry::not_found))
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
            client_telemetry: None,
        }
    }
}

/// The `/otlp` ingress against the router as it is deployed — **with** the SPA's
/// static fallback. The integration tests build the router without one, and
/// that hid a real escape: axum's nested catch-all does not match an empty tail,
/// so `/otlp/` fell past the ingress to `ServeDir`, which answers a `GET` with
/// `200 index.html`. Found by the e2e suite (#354), pinned here.
#[cfg(test)]
mod otlp_route_tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt as _;

    use super::{AppState, router};

    const INDEX: &str = "<!doctype html><title>spa</title>";

    fn static_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "v-note-otlp-route-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("temp static dir");
        std::fs::write(dir.join("index.html"), INDEX).expect("index.html");
        dir
    }

    #[tokio::test]
    async fn nothing_under_otlp_reaches_the_spa_fallback() {
        let dir = static_dir();
        let app = router(AppState::for_tests(), Some(dir.clone()));

        for method in [Method::GET, Method::POST] {
            for path in ["/otlp", "/otlp/", "/otlp/spa", "/otlp/anything/else"] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.clone())
                            .uri(path)
                            .body(Body::empty())
                            .expect("request"),
                    )
                    .await
                    .expect("response");
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body");
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
                assert_ne!(body, INDEX, "{method} {path} was served the SPA");
            }
        }

        // …while the fallback itself still works, or this proves nothing. By
        // body, not status: `ServeDir`'s `not_found_service` serves index.html
        // *with a 404*, which is also why the status check above could not catch
        // the escape on its own.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/p/some-page")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body, INDEX, "the SPA fallback should serve a deep link");
        let _ = std::fs::remove_dir_all(dir);
    }
}
