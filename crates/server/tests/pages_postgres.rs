//! The page REST routes against a real Postgres (#337): create, list, read,
//! thumbnail and delete through the real router, with owner isolation. Compiled
//! only with the `postgres-tests` feature; `scripts/rust-ci-test.sh` provides
//! `DATABASE_URL`, and `#[sqlx::test]` gives each test a fresh migrated database.

#![cfg(feature = "postgres-tests")]

mod common;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use protocol::{CreatePageRequest, ListPagesResponse, PageResponse, Paper, ThumbnailMetadata};
use server::build_router;
use sqlx::PgPool;
use tower::util::ServiceExt;

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "page-owner";
const STRANGER: &str = "someone-else";

/// Sends one request as `user` through a router over `pool`.
async fn send(
    pool: &PgPool,
    user: &str,
    method: Method,
    uri: &str,
    json: Option<String>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let config = test_auth_config();
    let token = sign_test_token(SignedTokenClaims::valid_for(&config, user));
    let app = build_router(
        "test-version".to_string(),
        Arc::new(config),
        Arc::new(test_jwks_cache()),
        pool.clone(),
    );
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match json {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json)
        }
        None => Body::empty(),
    };
    let response = app
        .oneshot(request.body(body).expect("request should build"))
        .await
        .expect("request should succeed");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should be readable");
    (status, headers, bytes.to_vec())
}

async fn create_page(pool: &PgPool, title: Option<&str>, paper: Paper) -> PageResponse {
    let request = serde_json::to_string(&CreatePageRequest {
        title: title.map(str::to_string),
        paper,
    })
    .expect("create-page request should serialize");
    let (status, _, body) = send(pool, OWNER, Method::POST, "/api/pages", Some(request)).await;
    assert_eq!(status, StatusCode::CREATED);
    serde_json::from_slice(&body).expect("a page response")
}

#[sqlx::test(migrations = "./migrations")]
async fn a_page_is_created_listed_read_and_deleted_only_by_its_owner(pool: PgPool) {
    let created = create_page(&pool, Some("  Groceries  "), Paper::RuledWide).await;
    assert_eq!(created.page.title, "Groceries", "the title is trimmed");
    assert_eq!(created.page.paper, Paper::RuledWide);
    assert_eq!(created.page.thumbnail, ThumbnailMetadata::Empty);
    let page_uri = format!("/api/pages/{}", created.page.id);

    let (status, _, body) = send(&pool, OWNER, Method::GET, "/api/pages", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed: ListPagesResponse = serde_json::from_slice(&body).expect("a page list");
    assert_eq!(listed.pages, std::slice::from_ref(&created.page));

    let (status, _, body) = send(&pool, STRANGER, Method::GET, "/api/pages", None).await;
    assert_eq!(status, StatusCode::OK);
    let foreign: ListPagesResponse = serde_json::from_slice(&body).expect("a page list");
    assert!(
        foreign.pages.is_empty(),
        "the library lists only the caller's pages"
    );

    let (status, _, body) = send(&pool, OWNER, Method::GET, &page_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let read: PageResponse = serde_json::from_slice(&body).expect("a page response");
    assert_eq!(read.page, created.page);

    for method in [Method::GET, Method::DELETE] {
        let label = format!("{method} by a stranger");
        let (status, _, body) = send(&pool, STRANGER, method, &page_uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}");
        assert_eq!(body, b"page not found", "{label}");
    }

    let (status, _, _) = send(&pool, OWNER, Method::DELETE, &page_uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(&pool, OWNER, Method::GET, &page_uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a deleted page is gone");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_thumbnail_is_served_immutable_while_available_and_gone_otherwise(pool: PgPool) {
    let id = create_page(&pool, None, Paper::None).await.page.id;
    sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status, png) VALUES ($1, 1, 'available', $2), ($1, 2, 'generating', NULL)",
    )
    .bind(&id)
    .bind(b"png-bytes".as_slice())
    .execute(&pool)
    .await
    .expect("thumbnail rows should insert");
    let thumbnail = |seq: u64| format!("/api/pages/{id}/thumbnails/{seq}");

    let (status, headers, body) = send(&pool, OWNER, Method::GET, &thumbnail(1), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(body, b"png-bytes");

    let (status, _, _) = send(&pool, OWNER, Method::GET, &thumbnail(2), None).await;
    assert_eq!(
        status,
        StatusCode::GONE,
        "a job still generating has no artifact"
    );
    let (status, _, _) = send(&pool, OWNER, Method::GET, &thumbnail(9), None).await;
    assert_eq!(status, StatusCode::GONE);
    let (status, _, _) = send(&pool, STRANGER, Method::GET, &thumbnail(1), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, _, body) = send(&pool, OWNER, Method::GET, &format!("/api/pages/{id}"), None).await;
    let read: PageResponse = serde_json::from_slice(&body).expect("a page response");
    assert_eq!(
        read.page.thumbnail,
        ThumbnailMetadata::Generating { source_seq: 2 },
        "a page reports its newest thumbnail job"
    );
}
