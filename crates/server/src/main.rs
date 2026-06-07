use std::env;
use std::net::SocketAddr;

use axum_server::tls_rustls::RustlsConfig;
use server::router_from_env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "server=info,tower_http=info,axum=info".to_string()),
        )
        .init();

    let app = router_from_env().await;

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
