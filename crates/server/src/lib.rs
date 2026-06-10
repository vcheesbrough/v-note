pub mod auth;
mod routes;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::{auth_middleware, AuthConfig, JwksCache};
use crate::routes::auth::{assetlinks, callback, login, logout, me, mobile_callback};
use crate::routes::pages::{create_page, delete_page, get_page, list_pages};
use crate::routes::realtime::{realtime_socket, realtime_ticket, RealtimeHub};

#[derive(Clone)]
pub struct AppState {
    pub app_version: String,
    pub auth: Arc<AuthConfig>,
    pub jwks_cache: Arc<JwksCache>,
    pub db: Option<PgPool>,
    pub realtime: Arc<RealtimeHub>,
}

pub fn app_version_from_env() -> String {
    env::var("APP_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

pub async fn router_from_env() -> Router {
    let auth = Arc::new(AuthConfig::load().await);
    let jwks_cache = Arc::new(JwksCache::new(auth.jwks_uri.clone()));
    let db = match env::var("DATABASE_URL") {
        Ok(database_url) if !database_url.is_empty() => {
            let pool = PgPoolOptions::new()
                .max_connections(5)
                .connect(&database_url)
                .await
                .expect("DATABASE_URL should be reachable");
            sqlx::migrate!("./migrations")
                .run(&pool)
                .await
                .expect("database migrations should apply");
            Some(pool)
        }
        _ => None,
    };
    build_router_with_db(app_version_from_env(), auth, jwks_cache, db)
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
    let state = AppState {
        app_version,
        auth,
        jwks_cache,
        db,
        realtime: Arc::new(RealtimeHub::default()),
    };

    let public_api = Router::new()
        .route("/meta", get(meta))
        .with_state(state.clone());

    let protected_api = Router::new()
        .route("/me", get(me))
        .route("/pages", get(list_pages).post(create_page))
        .route("/pages/{page_id}", get(get_page).delete(delete_page))
        .route("/realtime-ticket", post(realtime_ticket))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    let mut router = Router::new()
        .route("/health", get(health))
        .route("/.well-known/assetlinks.json", get(assetlinks))
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
        );

    if let Ok(static_dir) = env::var("STATIC_DIR") {
        let index = PathBuf::from(&static_dir).join("index.html");
        let spa_service = axum::routing::get_service(
            ServeDir::new(static_dir).not_found_service(ServeFile::new(index)),
        );
        router = router.fallback_service(spa_service);
    }

    router
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn meta(axum::extract::State(state): axum::extract::State<AppState>) -> Json<MetaResponse> {
    Json(MetaResponse {
        app_name: "v-note".to_string(),
        app_version: state.app_version,
        protocol_version: PROTOCOL_VERSION,
    })
}
