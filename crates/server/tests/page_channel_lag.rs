//! A page subscriber that falls behind the page channel is closed and recovers
//! by reconnecting (#279), end to end through a real server, real sockets and a
//! real Postgres.
//!
//! "Slow" is made the honest way: the subscriber simply stops reading, with a
//! tiny receive buffer so the kernel stops absorbing on its behalf. Once the
//! server's send to it blocks, everything the publisher commits piles up in the
//! page channel until it overflows — which is exactly what a frozen tab or a
//! stalled mobile link does. Compiled only with the `postgres-tests` feature;
//! `scripts/rust-ci-test.sh` provides `DATABASE_URL`.

#![cfg(feature = "postgres-tests")]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use protocol::{CreatePageRequest, PageResponse, PageServerMessage, Paper};
use server::build_router;
use sqlx::PgPool;
use tokio::net::{TcpSocket, TcpStream};
use tower::util::ServiceExt;
use yawc::close::CloseCode;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpRequest, Options, WebSocket};

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "lag-owner";

/// Large batches that fill the socket buffers between the server and the
/// stalled subscriber, so the server's send to it blocks. Well past the ~4 MiB
/// a loopback send buffer can autotune to.
const LARGE_BATCHES: usize = 16;
/// Points per large batch: ~50 bytes of JSON each, so ~450 KB per batch —
/// comfortably under the 1 MiB inbound cap.
const LARGE_BATCH_POINTS: usize = 9_000;
/// Small batches committed once the subscriber is stuck: more than the page
/// channel holds (256), so it must overflow.
const SMALL_BATCHES: usize = 300;

type Socket = WebSocket<TcpStream>;

fn token() -> String {
    sign_test_token(SignedTokenClaims::valid_for(&test_auth_config(), OWNER))
}

fn router(pool: &PgPool) -> axum::Router {
    build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        pool.clone(),
    )
}

async fn create_page(pool: &PgPool) -> String {
    let body = serde_json::to_string(&CreatePageRequest {
        title: Some("lag".to_string()),
        paper: Paper::None,
    })
    .expect("serializes");
    let response = router(pool)
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

async fn serve(pool: &PgPool) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("has an address");
    let app = router(pool);
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serves") });
    address
}

/// Opens the page channel as a Bearer caller (Android's path). A
/// `receive_buffer` shrinks the socket's receive buffer before connecting,
/// which is what lets a test stall the server by not reading.
async fn open(address: std::net::SocketAddr, page_id: &str, receive_buffer: Option<u32>) -> Socket {
    let socket = TcpSocket::new_v4().expect("socket");
    if let Some(bytes) = receive_buffer {
        socket.set_recv_buffer_size(bytes).expect("rcvbuf");
    }
    let stream = socket.connect(address).await.expect("connects");
    let url = format!("ws://{address}/api/pages/{page_id}/realtime")
        .parse()
        .expect("url");
    WebSocket::handshake_with_request(
        url,
        stream,
        // The catch-up replay carries the large batches, well past yawc's
        // 1 MiB default read cap.
        Options::default().with_max_payload_read(64 * 1024 * 1024),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token())),
    )
    .await
    .expect("upgrades")
}

async fn send(socket: &mut Socket, message: serde_json::Value) {
    socket
        .send(Frame::text(message.to_string()))
        .await
        .expect("sends");
}

/// How a socket ended: the close frame's code and reason, or `None` for a
/// stream that ended without one.
#[derive(Debug)]
struct Closed(Option<(CloseCode, Option<String>)>);

/// The next text message, skipping control frames, or how the socket ended.
async fn next_message(socket: &mut Socket) -> Result<PageServerMessage, Closed> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("the server answers within 30s")
            .ok_or(Closed(None))?;
        match frame.opcode() {
            OpCode::Text => {
                return Ok(serde_json::from_slice(frame.payload()).expect("a page message"));
            }
            OpCode::Close => {
                let code = frame.close_code().expect("a close frame with a code");
                let reason = frame.close_reason().expect("utf-8").map(str::to_string);
                return Err(Closed(Some((code, reason))));
            }
            _ => {}
        }
    }
}

async fn expect_welcome(socket: &mut Socket) -> Paper {
    match next_message(socket).await {
        Ok(PageServerMessage::Welcome { paper, .. }) => paper,
        other => panic!("expected welcome, got {other:?}"),
    }
}

/// `v_note_realtime_events_total{channel="page",result="lagged"}`, read the
/// way Prometheus reads it.
async fn page_lagged_total() -> f64 {
    let response = server::observability::metrics_handler().await;
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("metrics body");
    String::from_utf8(body.to_vec())
        .expect("utf-8")
        .lines()
        .find(|line| {
            line.starts_with("v_note_realtime_events_total{")
                && line.contains(r#"channel="page""#)
                && line.contains(r#"result="lagged""#)
        })
        .and_then(|line| line.rsplit(' ').next()?.parse().ok())
        .unwrap_or(0.0)
}

fn stroke(id: &str, points: usize) -> serde_json::Value {
    let points: Vec<_> = (0..points)
        .map(|index| {
            // Varying digits so neither side's buffers see a trivially small
            // payload (compression is off here, but keep the shape honest).
            let x = (index * 37 % 1400) as f64 + 0.25;
            let y = (index * 53 % 2000) as f64 + 0.75;
            serde_json::json!({ "x": x, "y": y, "t": index })
        })
        .collect();
    serde_json::json!({
        "id": id,
        "style": {
            "tool_kind": "solid_round",
            "style_version": 2,
            "parameters": { "color": "#006400", "width": 2.0, "cap_style": "round", "join_style": "round" }
        },
        "points": points,
    })
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stalled_subscriber_is_closed_on_lag_and_converges_by_resubscribing(pool: PgPool) {
    let page_id = create_page(&pool).await;
    let address = serve(&pool).await;
    let lagged_before = page_lagged_total().await;

    // The subscriber: welcomed and caught up, then it stops reading.
    let mut subscriber = open(address, &page_id, Some(4096)).await;
    expect_welcome(&mut subscriber).await;
    send(
        &mut subscriber,
        serde_json::json!({ "type": "subscribe", "from_seq": 0 }),
    )
    .await;
    match next_message(&mut subscriber).await {
        Ok(PageServerMessage::PageReplay(replay)) => assert_eq!(replay.last_seq, 0),
        other => panic!("expected the empty replay, got {other:?}"),
    }

    // The publisher holds the lease and commits; a reader task keeps it drained
    // and reports the seq of every echo, so it never lags itself.
    let publisher = open(address, &page_id, None).await;
    let (mut publisher_tx, mut publisher_rx) = publisher.split();
    let (echoes_tx, mut echoes) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(frame) = publisher_rx.next().await {
            if frame.opcode() != OpCode::Text {
                continue;
            }
            if let Ok(PageServerMessage::StrokeBatch(batch)) =
                serde_json::from_slice(frame.payload())
                && echoes_tx.send(batch.seq).is_err()
            {
                break;
            }
        }
    });
    let lease = serde_json::json!({ "type": "acquire-lease" }).to_string();
    publisher_tx.send(Frame::text(lease)).await.expect("sends");

    let total = LARGE_BATCHES + SMALL_BATCHES;
    let mut expected_strokes = BTreeSet::new();
    for index in 0..total {
        let points = if index < LARGE_BATCHES {
            LARGE_BATCH_POINTS
        } else {
            2
        };
        let id = format!("stroke-{index}");
        expected_strokes.insert(id.clone());
        let commit = serde_json::json!({
            "type": "commit-batch",
            "client_batch_id": format!("batch-{index}"),
            "strokes": [stroke(&id, points)],
        });
        publisher_tx
            .send(Frame::text(commit.to_string()))
            .await
            .expect("sends");
    }
    let mut head = 0;
    while head < total as u64 {
        head = tokio::time::timeout(Duration::from_secs(60), echoes.recv())
            .await
            .expect("every commit is echoed within 60s")
            .expect("the publisher stays open");
    }

    // Read again: the subscriber gets an unbroken prefix of what was published,
    // then a clean "try again later" close — never a gap on an open socket.
    let mut delivered = Vec::new();
    let close = loop {
        match next_message(&mut subscriber).await {
            Ok(PageServerMessage::StrokeBatch(batch)) => delivered.push(batch.seq),
            // The publisher taking the lease is fanned out too.
            Ok(_) => {}
            Err(closed) => break closed,
        }
    };
    let Closed(Some((code, reason))) = close else {
        panic!("the stream ended without a close frame");
    };
    assert_eq!(code, CloseCode::Again, "1013 Try Again Later");
    assert_eq!(reason.as_deref(), Some("lagged"));
    let cursor = delivered.len() as u64;
    eprintln!("subscriber received {cursor} of {total} batches before the lagged close");
    assert_eq!(
        delivered,
        (1..=cursor).collect::<Vec<_>>(),
        "what arrived before the close is a gap-free prefix"
    );
    assert!(
        cursor < total as u64,
        "the subscriber must actually have missed something ({cursor} of {total})"
    );
    assert!(
        page_lagged_total().await > lagged_before,
        "the lag is counted"
    );

    // Recovery is the ordinary reconnect: a fresh welcome, then a subscribe
    // from the cursor fills exactly what was missed.
    let mut reconnected = open(address, &page_id, None).await;
    expect_welcome(&mut reconnected).await;
    send(
        &mut reconnected,
        serde_json::json!({ "type": "subscribe", "from_seq": cursor }),
    )
    .await;
    let replay = loop {
        match next_message(&mut reconnected).await {
            Ok(PageServerMessage::PageReplay(replay)) => break replay,
            Ok(_) => {}
            Err(close) => panic!("the reconnected socket closed: {close:?}"),
        }
    };
    let replayed: Vec<u64> = replay.batches.iter().map(|batch| batch.seq).collect();
    assert_eq!(replayed, ((cursor + 1)..=total as u64).collect::<Vec<_>>());
    assert_eq!(replay.last_seq, total as u64);

    let converged: BTreeSet<String> = (1..=cursor)
        .map(|seq| format!("stroke-{}", seq - 1))
        .chain(
            replay
                .batches
                .iter()
                .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.clone())),
        )
        .collect();
    assert_eq!(
        converged, expected_strokes,
        "the client converges on the server's page"
    );
}
