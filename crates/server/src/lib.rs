use std::env;
use std::path::PathBuf;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use protocol::{HealthResponse, MetaResponse, PROTOCOL_VERSION};
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct AppState {
    app_version: String,
}

pub fn app_version_from_env() -> String {
    env::var("APP_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

pub fn router_from_env() -> Router {
    build_router(app_version_from_env())
}

pub fn build_router(app_version: String) -> Router {
    let state = AppState { app_version };
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/api/meta", get(meta))
        .with_state(state);

    if let Ok(static_dir) = env::var("STATIC_DIR") {
        let index = PathBuf::from(&static_dir).join("index.html");
        let static_service = axum::routing::get_service(
            ServeDir::new(static_dir).not_found_service(ServeFile::new(index)),
        );
        router = router.fallback_service(static_service);
    }

    router
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn meta(State(state): State<AppState>) -> Json<MetaResponse> {
    Json(MetaResponse {
        app_name: "v-note".to_string(),
        app_version: state.app_version,
        protocol_version: PROTOCOL_VERSION,
    })
}
