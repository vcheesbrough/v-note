use std::env;
use std::net::SocketAddr;

use axum_server::tls_rustls::RustlsConfig;
use server::build_app_router;
use server::config::{
    build_config, load_group, AndroidConfig, DatabaseConfig, ObservabilityConfig, OidcConfig,
};
use server::observability::{init_tracing, run_metrics_server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    // Composition root: the only place that knows configuration is sourced from
    // sovereign-config. Assembling it blocks on a startup RPC (the provider runs it
    // on its own dedicated thread), which is fine to do inline here — no other task
    // is scheduled yet, so there is nothing for spawn_blocking to unblock. Any
    // missing/invalid value fails here. Each group is loaded + validated from its own
    // sub-branch, then handed to the feature that owns it — features only see their DTO.
    let cfg = build_config()?;
    let observability = load_group::<ObservabilityConfig>(&cfg, "observability")?;
    let database = load_group::<DatabaseConfig>(&cfg, "database")?;
    let oidc = load_group::<OidcConfig>(&cfg, "oidc")?;
    let android = load_group::<AndroidConfig>(&cfg, "android")?;
    drop(cfg);

    let _telemetry_guard = init_tracing(&observability);
    let metrics_addr = observability.metrics_socket_addr();

    let app = build_app_router(&database, &oidc, &android).await;
    tokio::spawn(async move {
        if let Err(error) = run_metrics_server(metrics_addr).await {
            tracing::error!(error = %error, "internal metrics listener stopped");
        }
    });

    match (env::var("TLS_CERT").ok(), env::var("TLS_KEY").ok()) {
        (Some(cert_path), Some(key_path)) => {
            let addr: SocketAddr = "0.0.0.0:443".parse()?;
            let config = RustlsConfig::from_pem_file(cert_path, key_path).await?;
            tracing::info!("Starting TLS server on {addr}");
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            let port = env::var("PORT")
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(8080);
            let addr = format!("0.0.0.0:{port}");
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("Starting HTTP server on {addr}");
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}
