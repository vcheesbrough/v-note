//! Concurrent editing of one page by two sessions (#630), end to end through a
//! real server, real sockets and a real Postgres.
//!
//! There is no edit lease: every session on a page may ink, erase and set the
//! paper, and the page converges by the merge rules in `docs/PLAN.md` →
//! *Multi-device / concurrent edit*. Strokes are a union by id, erasing wins
//! and is idempotent, paper is the last arrival, and stacking order is server
//! arrival order. Compiled only with the `postgres-tests` feature;
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
use tokio::net::TcpStream;
use tower::util::ServiceExt;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpRequest, Options, WebSocket};

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "concurrent-owner";

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
        id: None,
        title: Some("concurrent".to_string()),
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

/// Opens the page channel as a Bearer caller (Android's path) and reads its
/// `welcome`, returning the paper it carried.
async fn open(address: std::net::SocketAddr, page_id: &str) -> (Socket, Paper) {
    let stream = TcpStream::connect(address).await.expect("connects");
    let url = format!("ws://{address}/api/pages/{page_id}/realtime")
        .parse()
        .expect("url");
    let mut socket = WebSocket::handshake_with_request(
        url,
        stream,
        Options::default(),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token())),
    )
    .await
    .expect("upgrades");
    let paper = match next_message(&mut socket).await {
        PageServerMessage::Welcome { paper, .. } => paper,
        other => panic!("expected welcome, got {other:?}"),
    };
    (socket, paper)
}

async fn send(socket: &mut Socket, message: serde_json::Value) {
    socket
        .send(Frame::text(message.to_string()))
        .await
        .expect("sends");
}

/// The next text message, skipping control frames.
async fn next_message(socket: &mut Socket) -> PageServerMessage {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("the server answers within 30s")
            .expect("the socket stays open");
        match frame.opcode() {
            OpCode::Text => {
                return serde_json::from_slice(frame.payload()).expect("a page message");
            }
            OpCode::Close => panic!("the server closed the socket"),
            _ => {}
        }
    }
}

/// Reads until `count` messages matching `keep` have arrived, returning them in
/// arrival order. Every other message is skipped.
async fn collect(
    socket: &mut Socket,
    count: usize,
    keep: impl Fn(&PageServerMessage) -> bool,
) -> Vec<PageServerMessage> {
    let mut kept = Vec::new();
    while kept.len() < count {
        let message = next_message(socket).await;
        assert!(
            !matches!(message, PageServerMessage::Error { .. }),
            "no mutation is refused: {message:?}"
        );
        if keep(&message) {
            kept.push(message);
        }
    }
    kept
}

fn is_stroke_batch(message: &PageServerMessage) -> bool {
    matches!(message, PageServerMessage::StrokeBatch(_))
}

fn stroke(id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "style": {
            "tool_kind": "solid_round",
            "style_version": 2,
            "parameters": { "color": "#006400", "width": 2.0, "cap_style": "round", "join_style": "round" }
        },
        "points": [{ "x": 1.0, "y": 2.0, "t": 0 }, { "x": 30.0, "y": 40.0, "t": 16 }],
    })
}

fn commit(batch: &str, ids: &[&str]) -> serde_json::Value {
    let strokes: Vec<_> = ids.iter().copied().map(stroke).collect();
    serde_json::json!({ "type": "commit-batch", "client_batch_id": batch, "strokes": strokes })
}

fn erase(mutation: &str, ids: &[&str]) -> serde_json::Value {
    serde_json::json!({ "type": "commit-tombstones", "client_mutation_id": mutation, "stroke_ids": ids })
}

/// The canonical page as a fresh session sees it: the visible stroke ids, in
/// stacking order (server arrival order), from a full `subscribe` replay.
async fn canonical_strokes(address: std::net::SocketAddr, page_id: &str) -> Vec<String> {
    let (mut socket, _) = open(address, page_id).await;
    send(
        &mut socket,
        serde_json::json!({ "type": "subscribe", "from_seq": 0 }),
    )
    .await;
    // Fan-out published after this socket joined the page may come first.
    let replay = loop {
        if let PageServerMessage::PageReplay(replay) = next_message(&mut socket).await {
            break replay;
        }
    };
    let erased: BTreeSet<String> = replay
        .tombstones
        .iter()
        .flat_map(|batch| batch.stroke_ids.iter().cloned())
        .collect();
    replay
        .batches
        .iter()
        .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.clone()))
        .filter(|id| !erased.contains(id))
        .collect()
}

fn stroke_ids(messages: &[PageServerMessage]) -> BTreeSet<String> {
    messages
        .iter()
        .flat_map(|message| match message {
            PageServerMessage::StrokeBatch(batch) => {
                batch.strokes.iter().map(|s| s.id.clone()).collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

/// Acceptance 1: two sessions ink one page at the same time. Neither is
/// refused, each receives the other's strokes live, both see the same stacking
/// order, and the canonical page is the union.
#[sqlx::test(migrations = "./migrations")]
async fn two_sessions_inking_one_page_each_see_the_other_and_the_page_is_the_union(pool: PgPool) {
    let page_id = create_page(&pool).await;
    let address = serve(&pool).await;
    let (mut a, _) = open(address, &page_id).await;
    let (mut b, _) = open(address, &page_id).await;

    // Interleaved, without waiting for any echo in between: the commits race.
    const PER_SESSION: usize = 10;
    for index in 0..PER_SESSION {
        send(
            &mut a,
            commit(&format!("a-{index}"), &[&format!("a-stroke-{index}")]),
        )
        .await;
        send(
            &mut b,
            commit(&format!("b-{index}"), &[&format!("b-stroke-{index}")]),
        )
        .await;
    }

    let seen_by_a = collect(&mut a, 2 * PER_SESSION, is_stroke_batch).await;
    let seen_by_b = collect(&mut b, 2 * PER_SESSION, is_stroke_batch).await;
    let expected: BTreeSet<String> = (0..PER_SESSION)
        .flat_map(|index| [format!("a-stroke-{index}"), format!("b-stroke-{index}")])
        .collect();
    assert_eq!(
        stroke_ids(&seen_by_a),
        expected,
        "a sees its own and b's ink"
    );
    assert_eq!(
        stroke_ids(&seen_by_b),
        expected,
        "b sees its own and a's ink"
    );
    // One page channel, one order: both sessions see the same arrival order,
    // and its `seq` is gap-free.
    assert_eq!(seen_by_a, seen_by_b);
    let seqs: Vec<u64> = seen_by_a
        .iter()
        .map(|message| match message {
            PageServerMessage::StrokeBatch(batch) => batch.seq,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(seqs, (1..=2 * PER_SESSION as u64).collect::<Vec<_>>());

    let canonical = canonical_strokes(address, &page_id).await;
    let live_order: Vec<String> = seen_by_a
        .iter()
        .flat_map(|message| match message {
            PageServerMessage::StrokeBatch(batch) => batch
                .strokes
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(
        canonical, live_order,
        "the canonical page is the union, stacked in arrival order"
    );
}

/// A retried batch from either session is a no-op: union by stroke id, and a
/// repeated `client_batch_id` is not fanned out again.
#[sqlx::test(migrations = "./migrations")]
async fn a_retried_batch_does_not_duplicate_ink_on_a_shared_page(pool: PgPool) {
    let page_id = create_page(&pool).await;
    let address = serve(&pool).await;
    let (mut a, _) = open(address, &page_id).await;
    let (mut b, _) = open(address, &page_id).await;

    send(&mut a, commit("a-1", &["a-stroke"])).await;
    collect(&mut b, 1, is_stroke_batch).await;
    send(&mut a, commit("a-1", &["a-stroke"])).await;
    send(&mut b, commit("b-1", &["b-stroke"])).await;
    let seen = collect(&mut b, 2, is_stroke_batch).await;
    assert!(
        seen.iter().all(|message| matches!(
            message,
            PageServerMessage::StrokeBatch(batch) if batch.seq <= 2
        )),
        "the retry kept its original seq: {seen:?}"
    );

    assert_eq!(
        canonical_strokes(address, &page_id).await,
        ["a-stroke", "b-stroke"]
    );
}

/// Acceptance 2: drawing on one device while erasing on another gives the same
/// page whatever order the two mutations reach the server in.
#[sqlx::test(migrations = "./migrations")]
async fn drawing_and_erasing_on_two_devices_converge_in_either_arrival_order(pool: PgPool) {
    let address = serve(&pool).await;
    let mut outcomes = Vec::new();

    for erase_first in [false, true] {
        let page_id = create_page(&pool).await;
        let (mut drawer, _) = open(address, &page_id).await;
        let (mut eraser, _) = open(address, &page_id).await;
        // Both devices already hold `shared`; the eraser removes it, plus
        // `fresh`, which the drawer is committing right now.
        send(&mut drawer, commit("base", &["shared"])).await;
        collect(&mut eraser, 1, is_stroke_batch).await;

        let draw = commit("draw", &["fresh", "kept"]);
        let rub = erase("rub", &["shared", "fresh"]);
        let tombstone = |id: &'static str| move |m: &PageServerMessage| matches!(m, PageServerMessage::TombstoneBatch(batch) if batch.client_mutation_id == id);
        let batch = |id: &'static str| move |m: &PageServerMessage| matches!(m, PageServerMessage::StrokeBatch(batch) if batch.client_batch_id == id);
        if erase_first {
            send(&mut eraser, rub.clone()).await;
            collect(&mut drawer, 1, tombstone("rub")).await;
            send(&mut drawer, draw.clone()).await;
            collect(&mut eraser, 1, batch("draw")).await;
        } else {
            send(&mut drawer, draw.clone()).await;
            collect(&mut eraser, 1, batch("draw")).await;
            send(&mut eraser, rub.clone()).await;
            collect(&mut drawer, 1, tombstone("rub")).await;
        }
        // Erasing an already-erased stroke changes nothing.
        send(&mut drawer, erase("rub-again", &["shared"])).await;
        collect(&mut eraser, 1, tombstone("rub-again")).await;

        outcomes.push(canonical_strokes(address, &page_id).await);
    }

    assert_eq!(outcomes[0], ["kept"], "erase-after-draw leaves only `kept`");
    assert_eq!(
        outcomes[0], outcomes[1],
        "draw and erase converge on the same page in either order"
    );
}

/// Acceptance 3: two devices set the paper at the same time. Every session —
/// both editors and one that opens afterwards — ends on the last arrival.
#[sqlx::test(migrations = "./migrations")]
async fn paper_set_on_two_devices_at_once_converges_on_the_last_arrival(pool: PgPool) {
    let page_id = create_page(&pool).await;
    let address = serve(&pool).await;
    let (mut a, _) = open(address, &page_id).await;
    let (mut b, _) = open(address, &page_id).await;

    send(
        &mut a,
        serde_json::json!({ "type": "set-paper", "client_mutation_id": "pa", "paper": "ruled-wide" }),
    )
    .await;
    send(
        &mut b,
        serde_json::json!({ "type": "set-paper", "client_mutation_id": "pb", "paper": "squared-small" }),
    )
    .await;

    let paper_changed = |m: &PageServerMessage| matches!(m, PageServerMessage::PaperChanged { .. });
    let seen_by_a = collect(&mut a, 2, paper_changed).await;
    let seen_by_b = collect(&mut b, 2, paper_changed).await;
    assert_eq!(seen_by_a, seen_by_b, "both sessions see the same order");
    let papers: Vec<Paper> = seen_by_a
        .iter()
        .map(|message| match message {
            PageServerMessage::PaperChanged { paper, .. } => *paper,
            _ => unreachable!(),
        })
        .collect();
    let mut both = papers.clone();
    both.sort_by_key(|paper| paper.wire_value());
    assert_eq!(
        both,
        [Paper::RuledWide, Paper::SquaredSmall],
        "each change is applied, neither refused"
    );
    let last = papers[1];

    let (_late, welcomed) = open(address, &page_id).await;
    assert_eq!(
        welcomed, last,
        "a later session is welcomed with the last arrival"
    );
}
