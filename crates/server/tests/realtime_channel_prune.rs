//! Realtime broadcast channels live only while a socket is subscribed to them
//! (#633), end to end through a real server, real sockets and a real Postgres:
//! opening N page and library sockets and closing them — cleanly or by just
//! dropping the connection — leaves no channel in memory.
//!
//! What is observed is `v_note_realtime_channels`, the gauge the hub moves as
//! it creates and prunes channels. It is process-wide, so this binary holds
//! this one test and nothing else moves it. Compiled only with the
//! `postgres-tests` feature; `scripts/rust-ci-test.sh` provides `DATABASE_URL`.

#![cfg(feature = "postgres-tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use protocol::{CreatePageRequest, PageResponse, PageServerMessage, Paper};
use server::build_router;
use server::observability::metrics_handler;
use sqlx::PgPool;
use tokio::net::TcpStream;
use tower::util::ServiceExt;
use yawc::close::CloseCode;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpRequest, Options, WebSocket};

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "prune-owner";

type Socket = WebSocket<TcpStream>;

fn token() -> String {
    sign_test_token(SignedTokenClaims::valid_for(&test_auth_config(), OWNER))
}

async fn create_page(app: &axum::Router, title: &str) -> String {
    let body = serde_json::to_string(&CreatePageRequest {
        id: None,
        title: Some(title.to_string()),
        paper: Paper::None,
    })
    .expect("serializes");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/pages")
                .header(header::AUTHORIZATION, format!("Bearer {}", token()))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    let page: PageResponse = serde_json::from_slice(&bytes).expect("a page response");
    page.page.id
}

async fn connect(address: std::net::SocketAddr, path: &str) -> Socket {
    let stream = TcpStream::connect(address).await.expect("connects");
    WebSocket::handshake_with_request(
        format!("ws://{address}{path}").parse().expect("url"),
        stream,
        Options::default(),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token())),
    )
    .await
    .expect("upgrades")
}

/// Opens a page socket and reads its `welcome`, by which point the server's
/// session has subscribed to the page channel.
async fn open_page(address: std::net::SocketAddr, page_id: &str) -> Socket {
    let mut socket = connect(address, &format!("/api/pages/{page_id}/realtime")).await;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("the server answers within 30s")
            .expect("the socket stays open");
        if frame.opcode() == OpCode::Text {
            let message: PageServerMessage =
                serde_json::from_slice(frame.payload()).expect("a page message");
            assert!(
                matches!(message, PageServerMessage::Welcome { .. }),
                "expected welcome, got {message:?}"
            );
            return socket;
        }
    }
}

/// The gauge's current value for one channel kind.
async fn channels(kind: &str) -> i64 {
    let response = metrics_handler().await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("metrics body reads");
    let text = String::from_utf8(body.to_vec()).expect("metrics are UTF-8");
    let series = format!("v_note_realtime_channels{{channel=\"{kind}\"}} ");
    text.lines()
        .find_map(|line| line.strip_prefix(series.as_str()))
        .unwrap_or_else(|| panic!("{series} is exported"))
        .trim()
        .parse()
        .expect("a gauge value")
}

/// Polls until both channel kinds read `page` / `library`: a socket's close
/// reaches the server's task asynchronously.
async fn expect_channels(page: i64, library: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let now = (channels("page").await, channels("library").await);
        if now == (page, library) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected (page, library) channels {:?}, still {now:?}",
            (page, library)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn closing_every_realtime_socket_leaves_no_channel_behind(pool: PgPool) {
    let app = build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        pool.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("has an address");
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.expect("serves") });

    expect_channels(0, 0).await;

    let pages = [
        create_page(&app, "one").await,
        create_page(&app, "two").await,
        create_page(&app, "three").await,
    ];
    let mut page_sockets = Vec::new();
    for n in 0..9 {
        page_sockets.push(open_page(address, &pages[n % pages.len()]).await);
    }
    let mut library_sockets = Vec::new();
    for _ in 0..4 {
        library_sockets.push(connect(address, "/api/realtime").await);
    }
    // One channel per open page and one for the owner's library, however many
    // sockets share them. The library subscribes before its upgrade is
    // answered, so it is counted the moment the handshake returns.
    expect_channels(3, 1).await;

    // Half of each closes cleanly, the rest just drop the connection: both
    // end the server's socket task, and either way the channel goes.
    for (n, mut socket) in page_sockets.into_iter().chain(library_sockets).enumerate() {
        if n % 2 == 0 {
            socket
                .send(Frame::close(CloseCode::Normal, b"bye"))
                .await
                .expect("sends a close");
        }
        drop(socket);
    }
    expect_channels(0, 0).await;

    // A page opened again gets a channel again.
    let _again = open_page(address, &pages[0]).await;
    expect_channels(1, 0).await;
}
