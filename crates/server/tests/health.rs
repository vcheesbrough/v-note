use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use protocol::HealthResponse;
use server::build_router;
use tower::util::ServiceExt;

#[tokio::test]
async fn health_returns_ok_payload() {
    let app = build_router("test-version".to_string(), None, None);

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
