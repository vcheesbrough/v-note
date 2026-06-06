pub mod auth;
mod routes;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware;
use axum::routing::get;
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::{auth_middleware, AuthConfig, JwksCache};
use crate::routes::auth::{assetlinks, callback, login, logout, me, mobile_callback};

#[derive(Clone)]
pub struct AppState {
    pub app_version: String,
    pub auth: Option<Arc<AuthConfig>>,
    pub jwks_cache: Option<Arc<JwksCache>>,
}

pub fn app_version_from_env() -> String {
    env::var("APP_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

pub async fn router_from_env() -> Router {
    let auth = AuthConfig::load().await;
    let jwks_cache = auth
        .as_ref()
        .map(|config| Arc::new(JwksCache::new(config.jwks_uri.clone())));
    build_router(
        app_version_from_env(),
        auth.map(Arc::new),
        jwks_cache,
    )
}

pub fn build_router(
    app_version: String,
    auth: Option<Arc<AuthConfig>>,
    jwks_cache: Option<Arc<JwksCache>>,
) -> Router {
    let state = AppState {
        app_version,
        auth,
        jwks_cache,
    };

    let public_api = Router::new()
        .route("/meta", get(meta))
        .with_state(state.clone());

    let protected_api = Router::new()
        .route("/me", get(me))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    let mut router = Router::new()
        .route("/health", get(health))
        .route(
            "/.well-known/assetlinks.json",
            get(assetlinks),
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
        .nest("/api", public_api.merge(protected_api));

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
