mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use protocol::HealthResponse;
use server::auth::JwksCache;
use server::build_router;
use server::observability::{CORRELATION_ID_HEADER, REQUEST_ID_HEADER, metrics, metrics_handler};
use tower::util::ServiceExt;

use common::{test_auth_config, unreachable_pool};

fn test_router() -> axum::Router {
    // An empty JWKS cache, not the fixture one: nothing here presents a token.
    let jwks = Arc::new(JwksCache::with_keys(HashMap::new()));
    build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        jwks,
        unreachable_pool(),
    )
}

#[tokio::test]
async fn health_returns_ok_payload() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let payload: HealthResponse =
        serde_json::from_slice(&body).expect("health response should deserialize");

    assert_eq!(payload.status, "ok");
}

#[tokio::test]
async fn health_response_includes_request_correlation_headers() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(REQUEST_ID_HEADER, "test-request-123")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header should be present"),
        "test-request-123",
    );
    assert_eq!(
        response
            .headers()
            .get(CORRELATION_ID_HEADER)
            .expect("correlation id header should be present"),
        "test-request-123",
    );
}

#[tokio::test]
async fn health_response_preserves_correlation_id_without_request_id() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(CORRELATION_ID_HEADER, "upstream-correlation-123")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("request should succeed");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header should be present"),
        "upstream-correlation-123",
    );
    assert_eq!(
        response
            .headers()
            .get(CORRELATION_ID_HEADER)
            .expect("correlation id header should be present"),
        "upstream-correlation-123",
    );
}

#[tokio::test]
async fn metrics_endpoint_exposes_build_and_http_metrics() {
    let app = test_router();

    let health_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("health request should succeed");
    assert_eq!(health_response.status(), StatusCode::OK);

    metrics().record_thumbnail_generation("success", 0.01);
    metrics().thumbnail_generation_queued();
    metrics().thumbnail_generation_finished();
    metrics().record_thumbnail_recovery("queued");
    metrics().observe_thumbnail_artifact_bytes(1024);

    let response = metrics_handler().await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("metrics should be UTF-8");

    assert!(text.contains("v_note_build_info"));
    // Derived from the canonical constant, so a protocol bump cannot leave this
    // assertion silently pinning the previous version.
    assert!(text.contains(&format!("protocol=\"{}\"", protocol::PROTOCOL_VERSION)));
    assert!(text.contains("v_note_thumbnail_generation_duration_seconds"));
    assert!(text.contains("v_note_thumbnail_queue_depth"));
    assert!(text.contains("v_note_thumbnail_recoveries_total"));
    assert!(text.contains("v_note_thumbnail_artifact_bytes"));
    assert!(text.contains("version=\""));
    assert!(text.contains("v_note_http_requests_total"));
    assert!(text.contains("route=\"/health\""));
    assert!(text.contains("status=\"200\""));
}

#[tokio::test]
async fn metrics_bucket_unknown_paths_to_static_route() {
    let app = test_router();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/unknown/random-cardinality-path")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("fallback request should succeed");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = metrics_handler().await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("metrics should be UTF-8");

    assert!(text.contains("route=\"/static/*\""));
    assert!(!text.contains("route=\"/unknown/random-cardinality-path\""));
}

/// Label keys that are unbounded per user, page, session or commit. A metric
/// carrying one of them grows a series per value — they belong in span fields.
const UNBOUNDED_LABELS: [&str; 4] = ["page_id", "session_id", "owner_id", "client_batch_id"];

/// Parses the Prometheus text exposition into `(name, label keys)` per sample.
/// Label *keys* only: values such as `route="/api/pages/{page_id}"` legitimately
/// contain those strings and are not a cardinality problem.
fn sample_label_keys(text: &str) -> Vec<(String, BTreeSet<String>)> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (series, _value) = line.rsplit_once(' ').expect("sample should have a value");
            let Some((name, body)) = series.split_once('{') else {
                return (series.to_string(), BTreeSet::new());
            };
            let mut keys = BTreeSet::new();
            let mut rest = body.strip_suffix('}').expect("labels should close");
            while let Some((key, after)) = rest.split_once("=\"") {
                keys.insert(key.to_string());
                let mut escaped = false;
                let end = after
                    .char_indices()
                    .find(|&(_, c)| {
                        let closes = c == '"' && !escaped;
                        escaped = c == '\\' && !escaped;
                        closes
                    })
                    .map(|(index, _)| index)
                    .expect("label value should close");
                rest = after[end + 1..].trim_start_matches(',');
            }
            (name.to_string(), keys)
        })
        .collect()
}

fn keys(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

#[tokio::test]
async fn realtime_metrics_carry_only_bounded_labels() {
    metrics().observe_realtime_message_bytes("page", "welcome", 96);
    metrics().observe_realtime_message_bytes("library", "page-updated", 120);
    metrics().observe_realtime_replay(3, 4096, 0.02);
    metrics().observe_realtime_message_handling("commit-batch", 0.004);
    metrics().record_realtime_event("page", "lagged");

    let response = metrics_handler().await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    let text = String::from_utf8(body.to_vec()).expect("metrics should be UTF-8");
    let samples = sample_label_keys(&text);

    let label_sets = |name: &str| -> BTreeSet<BTreeSet<String>> {
        samples
            .iter()
            .filter(|(sample, _)| sample == name)
            .map(|(_, keys)| keys.clone())
            .collect()
    };

    // The exact label set, so a new label cannot be added without this failing.
    assert_eq!(
        label_sets("v_note_realtime_message_bytes_bucket"),
        BTreeSet::from([keys(&["channel", "message_type", "le"])])
    );
    assert_eq!(
        label_sets("v_note_realtime_message_bytes_count"),
        BTreeSet::from([keys(&["channel", "message_type"])])
    );
    assert_eq!(
        label_sets("v_note_realtime_message_bytes_sum"),
        BTreeSet::from([keys(&["channel", "message_type"])])
    );
    for name in [
        "v_note_realtime_replay_bytes_count",
        "v_note_realtime_replay_frames_count",
        "v_note_realtime_replay_duration_seconds_count",
    ] {
        assert_eq!(label_sets(name), BTreeSet::from([keys(&[])]), "{name}");
    }
    assert_eq!(
        label_sets("v_note_realtime_message_handling_seconds_count"),
        BTreeSet::from([keys(&["message_type"])])
    );
    assert!(text.contains("result=\"lagged\""));

    // And across the whole scrape, not just the new series.
    for (name, label_keys) in &samples {
        for unbounded in UNBOUNDED_LABELS {
            assert!(
                !label_keys.contains(unbounded),
                "{name} is labelled by unbounded `{unbounded}`"
            );
        }
    }
}
