//! The page REST routes against a real Postgres (#337): create, list, read,
//! thumbnail and delete through the real router, with owner isolation. Compiled
//! only with the `postgres-tests` feature; `scripts/rust-ci-test.sh` provides
//! `DATABASE_URL`, and `#[sqlx::test]` gives each test a fresh migrated database.

#![cfg(feature = "postgres-tests")]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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

/// Every info-level event this test binary emits, as its fields.
type Events = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

/// The info-level events of the whole test binary, from a global subscriber
/// installed once. Global rather than a thread-local default on purpose: the
/// tests here run in parallel and all create pages, and a callsite first
/// reached by another test while no subscriber was listening caches "never
/// interested", so a thread-local capture misses the line — a flake that looks
/// exactly like the line being gone. Callers filter by the ids they own.
fn info_events() -> Events {
    use std::sync::OnceLock;
    use tracing_subscriber::layer::{Context, SubscriberExt as _};

    struct Fields<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    struct Capture(Events);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if *event.metadata().level() != tracing::Level::INFO {
                return;
            }
            let mut fields = BTreeMap::new();
            event.record(&mut Fields(&mut fields));
            self.0.lock().expect("capture lock").push(fields);
        }
    }

    static EVENTS: OnceLock<Events> = OnceLock::new();
    EVENTS
        .get_or_init(|| {
            let events = Events::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(Capture(Arc::clone(&events))),
            )
            .expect("no other test installs a global subscriber");
            events
        })
        .clone()
}

/// #417: the page CRUD surface logged one line before the sweep. Each mutation
/// now says what it did, at `info`, with the page it did it to — the line a
/// Loki search for a page id finds.
#[sqlx::test(migrations = "./migrations")]
async fn page_mutations_log_at_info_with_their_page_id(pool: PgPool) {
    let events = info_events();

    let id = create_page(&pool, Some("Logged"), Paper::None)
        .await
        .page
        .id;
    let (status, _, _) = send(
        &pool,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let events = events.lock().expect("capture lock").clone();
    for message in ["page created", "page deleted"] {
        let line = events.iter().find(|fields| {
            fields.get("message").map(String::as_str) == Some(message)
                && fields.get("page_id") == Some(&id)
        });
        assert!(
            line.is_some(),
            "no info-level {message:?} line with page_id {id}"
        );
    }
}
