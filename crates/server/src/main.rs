use std::net::SocketAddr;

use axum_server::tls_rustls::RustlsConfig;
use server::build_app_router;
use server::config::{
    AndroidConfig, DatabaseConfig, ObservabilityConfig, OidcConfig, ServerConfig, build_config,
    load_group,
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
    let server = load_group::<ServerConfig>(&cfg, "server")?;
    drop(cfg);

    let _telemetry_guard = init_tracing(&observability);
    let metrics_addr = observability.metrics_socket_addr();

    let app = build_app_router(&database, &oidc, &android, &server).await?;
    tokio::spawn(async move {
        if let Err(error) = run_metrics_server(metrics_addr).await {
            tracing::error!(error = %error, "internal metrics listener stopped");
        }
    });

    match server.tls_pair() {
        Some((cert_path, key_path)) => {
            let addr: SocketAddr = "0.0.0.0:443".parse()?;
            let config = RustlsConfig::from_pem_file(cert_path, key_path).await?;
            tracing::info!("Starting TLS server on {addr}");
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let addr = format!("0.0.0.0:{}", server.http_port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            tracing::info!("Starting HTTP server on {addr}");
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}
