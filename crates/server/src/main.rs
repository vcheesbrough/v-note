use std::env;
use std::net::SocketAddr;

use axum_server::tls_rustls::RustlsConfig;
use server::observability::{init_tracing, run_metrics_server};
use server::router_from_env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    let _telemetry_guard = init_tracing();

    let app = router_from_env().await;
    tokio::spawn(async {
        if let Err(error) = run_metrics_server().await {
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
