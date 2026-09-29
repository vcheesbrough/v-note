//! The SPA's `Content-Security-Policy` (#444), through the real router over a
//! real static directory: which responses carry it, that it allows exactly the
//! inline bootstrap `index.html` holds, and that it follows the telemetry
//! endpoint.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::CONTENT_SECURITY_POLICY;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use server::build_router_with_spa;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use url::Url;

use common::{test_auth_config, test_jwks_cache, unreachable_pool};

const BOOTSTRAP: &str = "\nimport init from '/frontend-0123.js';\nawait init({ module_or_path: '/frontend-0123_bg.wasm' });\n";

/// A throwaway dist directory shaped like Trunk's output. Removed on drop.
struct Dist(PathBuf);

impl Dist {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("v-note-csp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dist dir");
        std::fs::write(
            dir.join("index.html"),
            format!(
                "<!doctype html><html><head>\
                 <link rel=\"stylesheet\" href=\"/styles-0123.css\"/>\
                 <script type=\"module\">{BOOTSTRAP}</script>\
                 <link rel=\"modulepreload\" href=\"/frontend-0123.js\"></head><body></body></html>"
            ),
        )
        .expect("index.html");
        std::fs::write(
            dir.join("frontend-0123.js"),
            "export default function init() {}",
        )
        .expect("asset");
        Dist(dir)
    }
}

impl Drop for Dist {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app(dist: &Dist, endpoint: Option<&str>) -> Router {
    build_router_with_spa(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        unreachable_pool(),
        endpoint.map(|endpoint| Url::parse(endpoint).expect("endpoint")),
        dist.0.clone(),
    )
}

async fn csp_of(app: Router, uri: &str) -> (StatusCode, Option<String>) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let csp = response
        .headers()
        .get(CONTENT_SECURITY_POLICY)
        .map(|value| value.to_str().expect("ascii").to_string());
    (response.status(), csp)
}

fn bootstrap_hash() -> String {
    let digest = Sha256::digest(BOOTSTRAP.as_bytes());
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

fn expected_policy(connect_src: &str) -> String {
    format!(
        "default-src 'self'; \
         script-src 'self' 'wasm-unsafe-eval' {}; \
         connect-src {connect_src}; \
         img-src 'self' data: blob:; \
         style-src 'self' 'unsafe-inline'; \
         frame-ancestors 'self'; \
         base-uri 'none'; \
         object-src 'none'",
        bootstrap_hash()
    )
}

/// The document itself, under its own path and under any SPA deep link (the
/// `index.html` fallback), and the assets it loads, all carry the policy.
#[tokio::test]
async fn every_spa_response_carries_the_policy() {
    let dist = Dist::new();
    let expected = expected_policy("'self'");
    for uri in ["/", "/index.html", "/frontend-0123.js"] {
        let (status, csp) = csp_of(app(&dist, None), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(csp.as_deref(), Some(expected.as_str()), "{uri}");
    }

    // A deep link is answered by the `not_found_service` — `index.html` with a
    // 404 status — and is still the document the browser runs.
    let (status, csp) = csp_of(app(&dist, None), "/pages/some-page-id").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(csp.as_deref(), Some(expected.as_str()));
}

/// The ingest is not always same-origin (the e2e stack and `just run-compose`
/// serve it on its own port), so its origin is allowed explicitly — derived
/// from `client-telemetry.endpoint`, not configured a second time.
#[tokio::test]
async fn the_telemetry_endpoint_origin_may_be_fetched() {
    let dist = Dist::new();
    let (_, csp) = csp_of(app(&dist, Some("https://otlp-collector-oidc:4318")), "/").await;
    assert_eq!(
        csp.as_deref(),
        Some(expected_policy("'self' https://otlp-collector-oidc:4318").as_str())
    );

    // A deployed environment's endpoint is the app's own origin; listing it is
    // redundant with `'self'` but harmless, and the default port is not spelled.
    let (_, csp) = csp_of(app(&dist, Some("https://v-notes-dev.desync.link/")), "/").await;
    assert!(
        csp.as_deref()
            .expect("csp")
            .contains("connect-src 'self' https://v-notes-dev.desync.link;"),
        "{csp:?}"
    );
}

/// The Android auth bounce page is not the SPA: it runs its own inline script
/// to hand the code to the app, which the SPA's policy would refuse. Scoping
/// the policy to the static service is what keeps mobile login working.
#[tokio::test]
async fn non_spa_routes_do_not_carry_the_spa_policy() {
    let dist = Dist::new();
    for uri in ["/auth/mobile/callback?code=abc&state=xyz", "/health"] {
        let (status, csp) = csp_of(app(&dist, None), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(csp, None, "{uri}");
    }
}
