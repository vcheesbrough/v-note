//! Offline mode's server half (#159), end to end through a real server, real
//! sockets and a real Postgres. Android creates pages offline under ids it
//! mints itself and keeps an outbox of stroke batches, erasures and paper
//! changes, which it replays over the page channel whenever it reconnects —
//! from new sessions, after restarts, possibly many times over. Everything
//! here is that replay: nothing it sends twice may land twice, and two devices
//! replaying in either order must converge.
//!
//! Compiled only with the `postgres-tests` feature; `scripts/rust-ci-test.sh`
//! provides `DATABASE_URL`, and `#[sqlx::test]` gives each test a fresh
//! migrated database.

#![cfg(feature = "postgres-tests")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use protocol::{
    LibraryEvent, PAGE_DELETED_ERROR, PageReplay, PageResponse, PageServerMessage, Paper,
};
use server::build_router;
use sqlx::PgPool;
use tokio::net::TcpStream;
use tower::util::ServiceExt;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpRequest, Options, WebSocket};

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "offline-owner";
const STRANGER: &str = "offline-stranger";
/// A client-minted id, as Android mints them: `page_` and 32 lowercase hex.
const PAGE_ID: &str = "page_0123456789abcdef0123456789abcdef";

type Socket = WebSocket<TcpStream>;

fn token(user: &str) -> String {
    sign_test_token(SignedTokenClaims::valid_for(&test_auth_config(), user))
}

/// One server under test: REST calls go through `app`, sockets dial `address`,
/// and both share one state — so a REST create reaches the library sockets.
struct Server {
    app: axum::Router,
    address: std::net::SocketAddr,
}

async fn serve(pool: &PgPool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("has an address");
    let app = build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        pool.clone(),
    );
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.expect("serves") });
    Server { app, address }
}

/// One REST call as `user`: the status and the body.
async fn call(
    server: &Server,
    user: &str,
    method: Method,
    uri: &str,
    json: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", token(user)));
    let body = match json {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = server
        .app
        .clone()
        .oneshot(request.body(body).expect("request builds"))
        .await
        .expect("request succeeds");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    (status, bytes.to_vec())
}

/// `POST /api/pages` with the body Android sends for a page created offline.
async fn create_with_id(server: &Server, user: &str, id: &str) -> (StatusCode, Vec<u8>) {
    call(
        server,
        user,
        Method::POST,
        "/api/pages",
        Some(serde_json::json!({ "id": id, "title": "", "paper": "none" })),
    )
    .await
}

fn page_of(body: &[u8]) -> protocol::PageSummary {
    serde_json::from_slice::<PageResponse>(body)
        .expect("a page response")
        .page
}

async fn rows(pool: &PgPool, query: &'static str, page_id: &str) -> i64 {
    sqlx::query_scalar(query)
        .bind(page_id)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn page_rows(pool: &PgPool, page_id: &str) -> i64 {
    rows(pool, "SELECT COUNT(*) FROM pages WHERE id = $1", page_id).await
}

async fn bin(server: &Server, page_id: &str) {
    let (status, _) = call(
        server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

async fn listed(server: &Server, uri: &str) -> Vec<String> {
    let (status, body) = call(server, OWNER, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice::<protocol::ListPagesResponse>(&body)
        .expect("a page list")
        .pages
        .into_iter()
        .map(|page| page.id)
        .collect()
}

// ---- Sockets --------------------------------------------------------------

async fn dial(address: std::net::SocketAddr, path: &str, user: &str) -> Socket {
    let stream = TcpStream::connect(address).await.expect("connects");
    WebSocket::handshake_with_request(
        format!("ws://{address}{path}").parse().expect("url"),
        stream,
        Options::default(),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token(user))),
    )
    .await
    .expect("upgrades")
}

/// Opens the page channel as the owner (Android's Bearer path) and returns
/// the socket and the paper its `welcome` names.
async fn open(address: std::net::SocketAddr, page_id: &str) -> (Socket, Paper) {
    let mut socket = dial(address, &format!("/api/pages/{page_id}/realtime"), OWNER).await;
    match next_message(&mut socket).await {
        PageServerMessage::Welcome { paper, .. } => (socket, paper),
        other => panic!("expected welcome, got {other:?}"),
    }
}

async fn send(socket: &mut Socket, message: serde_json::Value) {
    socket
        .send(Frame::text(message.to_string()))
        .await
        .expect("sends");
}

async fn next_text(socket: &mut Socket) -> Vec<u8> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("the server answers within 30s")
            .expect("the socket stays open");
        match frame.opcode() {
            OpCode::Text => return frame.payload().to_vec(),
            OpCode::Close => panic!("the server closed the socket"),
            _ => {}
        }
    }
}

async fn next_message(socket: &mut Socket) -> PageServerMessage {
    serde_json::from_slice(&next_text(socket).await).expect("a page message")
}

async fn next_matching(
    socket: &mut Socket,
    keep: impl Fn(&PageServerMessage) -> bool,
) -> PageServerMessage {
    loop {
        let message = next_message(socket).await;
        if keep(&message) {
            return message;
        }
    }
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

fn commit(batch: &str, stroke_ids: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "commit-batch",
        "client_batch_id": batch,
        "strokes": stroke_ids.iter().map(|id| stroke(id)).collect::<Vec<_>>(),
    })
}

fn erase(mutation: &str, stroke_ids: &[&str]) -> serde_json::Value {
    serde_json::json!({ "type": "commit-tombstones", "client_mutation_id": mutation, "stroke_ids": stroke_ids })
}

fn set_paper(mutation: &str, paper: Paper) -> serde_json::Value {
    serde_json::json!({ "type": "set-paper", "client_mutation_id": mutation, "paper": paper.wire_value() })
}

/// Sends `subscribe` from 0 and returns the replay. The server handles one
/// socket's messages in order, so this is also a barrier: everything sent on
/// `socket` before it has been stored (or refused) by the time it answers.
async fn replay(socket: &mut Socket) -> PageReplay {
    send(
        socket,
        serde_json::json!({ "type": "subscribe", "from_seq": 0 }),
    )
    .await;
    match next_matching(socket, |message| {
        matches!(message, PageServerMessage::PageReplay(_))
    })
    .await
    {
        PageServerMessage::PageReplay(replay) => replay,
        _ => unreachable!(),
    }
}

/// A fresh session's view of the page: its paper and its full replay.
async fn canonical(address: std::net::SocketAddr, page_id: &str) -> (Paper, PageReplay) {
    let (mut socket, paper) = open(address, page_id).await;
    (paper, replay(&mut socket).await)
}

fn batch_ids(replay: &PageReplay) -> Vec<&str> {
    replay
        .batches
        .iter()
        .map(|batch| batch.client_batch_id.as_str())
        .collect()
}

fn visible_strokes(replay: &PageReplay) -> Vec<&str> {
    replay
        .batches
        .iter()
        .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.as_str()))
        .collect()
}

async fn commit_and_await_echo(socket: &mut Socket, batch: &str, stroke_id: &str) -> u64 {
    send(socket, commit(batch, &[stroke_id])).await;
    match next_matching(socket, |message| {
        matches!(
            message,
            PageServerMessage::StrokeBatch(_) | PageServerMessage::Error { .. }
        )
    })
    .await
    {
        PageServerMessage::StrokeBatch(echo) => {
            assert_eq!(echo.client_batch_id, batch);
            assert_eq!(echo.strokes.len(), 1);
            assert_eq!(echo.strokes[0].id, stroke_id);
            echo.seq
        }
        other => panic!("the commit should be accepted, got {other:?}"),
    }
}

// ---- Create with a client-minted id ---------------------------------------

/// A create replayed after a lost response is the same page: `201` the first
/// time, `200` with the page as it stands every time after (whatever the
/// replay says its title and paper are), and one `page-created` only.
#[sqlx::test(migrations = "./migrations")]
async fn a_create_with_a_client_id_is_idempotent_for_its_owner(pool: PgPool) {
    let server = serve(&pool).await;
    let mut library = dial(server.address, "/api/realtime", OWNER).await;

    let (status, body) = create_with_id(&server, OWNER, PAGE_ID).await;
    assert_eq!(status, StatusCode::CREATED);
    let created = page_of(&body);
    assert_eq!(created.id, PAGE_ID);
    assert_eq!(created.paper, Paper::None);
    assert_eq!(created.binned_at, None);

    // Android's body with the title omitted, then a replay that disagrees on
    // the title and paper: neither changes the page.
    let (status, body) = call(
        &server,
        OWNER,
        Method::POST,
        "/api/pages",
        Some(serde_json::json!({ "id": PAGE_ID, "paper": "none" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_of(&body), created);
    let (status, body) = call(
        &server,
        OWNER,
        Method::POST,
        "/api/pages",
        Some(serde_json::json!({ "id": PAGE_ID, "title": "renamed", "paper": "ruled-wide" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        page_of(&body),
        created,
        "a replay returns the page as it is"
    );
    assert_eq!(page_rows(&pool, PAGE_ID).await, 1);

    // One `page-created` for the client id. A server-picked create afterwards
    // is the marker: once its event arrives, every earlier event has too.
    let (status, body) = call(
        &server,
        OWNER,
        Method::POST,
        "/api/pages",
        Some(serde_json::json!({ "title": "marker" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let marker = page_of(&body).id;
    assert!(protocol::is_page_id(&marker), "server ids keep the shape");
    let mut created_events = Vec::new();
    loop {
        let event: LibraryEvent =
            serde_json::from_slice(&next_text(&mut library).await).expect("a library event");
        if let LibraryEvent::PageCreated { page } = event {
            if page.id == marker {
                break;
            }
            created_events.push(page.id);
        }
    }
    assert_eq!(created_events, [PAGE_ID]);
}

/// Another owner's id is the stranger's answer every page route gives — the
/// not-found `403` — so a create never reveals that a page exists, and the
/// page is untouched. A malformed id is a `400` and creates nothing.
#[sqlx::test(migrations = "./migrations")]
async fn a_client_id_is_validated_and_never_reveals_another_owners_page(pool: PgPool) {
    let server = serve(&pool).await;
    let (status, body) = create_with_id(&server, OWNER, PAGE_ID).await;
    assert_eq!(status, StatusCode::CREATED);
    let created = page_of(&body);

    let (status, body) = create_with_id(&server, STRANGER, PAGE_ID).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, b"page not found");
    let (status, _) = call(
        &server,
        STRANGER,
        Method::GET,
        &format!("/api/pages/{PAGE_ID}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the same answer a read gets");
    let (status, body) = call(
        &server,
        OWNER,
        Method::GET,
        &format!("/api/pages/{PAGE_ID}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_of(&body), created, "the owner's page is untouched");

    for invalid in [
        "page_0123456789ABCDEF0123456789abcdef",
        "page_0123",
        "0123456789abcdef0123456789abcdef",
        "page_0123456789abcdef0123456789abcdef/../x",
        "",
    ] {
        let (status, body) = create_with_id(&server, OWNER, invalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid:?}");
        assert_eq!(body, b"invalid page id");
        assert_eq!(page_rows(&pool, invalid).await, 0, "{invalid:?}");
    }
    assert_eq!(listed(&server, "/api/pages").await, [PAGE_ID]);
}

/// A binned page is returned as it is, with `binned_at`, and stays binned; a
/// permanently deleted one is `410` for its owner and the stranger's `403`
/// for anyone else — and its id is never reused, so a replayed create cannot
/// resurrect a page its owner destroyed.
#[sqlx::test(migrations = "./migrations")]
async fn a_replayed_create_of_a_binned_or_purged_page_is_never_a_new_page(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;
    bin(&server, PAGE_ID).await;

    let (status, body) = create_with_id(&server, OWNER, PAGE_ID).await;
    assert_eq!(status, StatusCode::OK);
    let replayed = page_of(&body);
    assert!(replayed.binned_at.is_some(), "returned binned");
    assert_eq!(listed(&server, "/api/bin").await, [PAGE_ID], "still binned");
    assert!(listed(&server, "/api/pages").await.is_empty());

    let (status, _) = call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/bin/{PAGE_ID}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = create_with_id(&server, OWNER, PAGE_ID).await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body, b"page permanently deleted");
    let (status, body) = create_with_id(&server, STRANGER, PAGE_ID).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, b"page not found");
    assert_eq!(page_rows(&pool, PAGE_ID).await, 0, "never resurrected");
    assert!(listed(&server, "/api/pages").await.is_empty());
    assert!(listed(&server, "/api/bin").await.is_empty());
}

// ---- Replaying the outbox -------------------------------------------------

/// Session A commits a batch and goes away without waiting for the echo (the
/// app was killed, the network dropped); session B, a new socket, catches up
/// and replays the same batch. However the two raced, the page holds it once.
#[sqlx::test(migrations = "./migrations")]
async fn a_batch_replayed_from_a_new_session_is_stored_once(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;

    let (mut first, _) = open(server.address, PAGE_ID).await;
    send(&mut first, commit("batch-b", &["stroke-b"])).await;
    first.close().await.ok();
    drop(first);

    let (mut second, _) = open(server.address, PAGE_ID).await;
    let caught_up = replay(&mut second).await;
    assert!(batch_ids(&caught_up).len() <= 1, "{caught_up:?}");
    commit_and_await_echo(&mut second, "batch-b", "stroke-b").await;

    let (_, replay) = canonical(server.address, PAGE_ID).await;
    assert_eq!(batch_ids(&replay), ["batch-b"]);
    assert_eq!(visible_strokes(&replay), ["stroke-b"]);
    assert_eq!(
        rows(
            &pool,
            "SELECT COUNT(*) FROM stroke_batches WHERE page_id = $1",
            PAGE_ID
        )
        .await,
        1
    );
}

/// The restart shape: the same outbox entry sent from three sockets in a row,
/// each acknowledged with the same `seq`, stored once.
#[sqlx::test(migrations = "./migrations")]
async fn a_batch_replayed_after_every_restart_keeps_its_seq_and_is_stored_once(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;

    let mut seqs = Vec::new();
    for _ in 0..3 {
        let (mut socket, _) = open(server.address, PAGE_ID).await;
        seqs.push(commit_and_await_echo(&mut socket, "batch-r", "stroke-r").await);
    }
    assert_eq!(seqs, [1, 1, 1]);

    let (_, replay) = canonical(server.address, PAGE_ID).await;
    assert_eq!(batch_ids(&replay), ["batch-r"]);
    assert_eq!(visible_strokes(&replay), ["stroke-r"]);
    assert_eq!(replay.last_seq, 1);
}

/// One device's outbox, sent in order on its own socket and stored by the
/// time this returns.
async fn replay_outbox(address: std::net::SocketAddr, outbox: &[serde_json::Value]) {
    let (mut socket, _) = open(address, PAGE_ID).await;
    for message in outbox {
        send(&mut socket, message.clone()).await;
    }
    replay(&mut socket).await;
}

/// Device 1 drew s1 and s2 and chose ruled paper; device 2, which had seen
/// s1, drew s3, erased s1 and chose squared paper — all offline.
fn device_outboxes() -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    (
        vec![
            commit("d1-b1", &["s1"]),
            commit("d1-b2", &["s2"]),
            set_paper("d1-paper", Paper::RuledWide),
        ],
        vec![
            commit("d2-b3", &["s3"]),
            erase("d2-erase", &["s1"]),
            set_paper("d2-paper", Paper::SquaredSmall),
        ],
    )
}

/// The ink-only part of an outbox: what a second replay of it may resend
/// without changing anything (a paper change is last-arrival-wins, so it is
/// not idempotent across devices and is left out).
fn ink_only(outbox: &[serde_json::Value]) -> Vec<serde_json::Value> {
    outbox
        .iter()
        .filter(|message| message["type"] != "set-paper")
        .cloned()
        .collect()
}

/// Device 1 reconnects first. s1 is stored and then erased (delete wins),
/// every batch is stored once in arrival order, and the paper is device 2's,
/// which arrived last. Replaying both outboxes again changes nothing.
#[sqlx::test(migrations = "./migrations")]
async fn two_devices_replaying_device_one_first_converge(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;
    let (device_1, device_2) = device_outboxes();

    replay_outbox(server.address, &device_1).await;
    replay_outbox(server.address, &device_2).await;

    let (paper, replay) = canonical(server.address, PAGE_ID).await;
    assert_eq!(batch_ids(&replay), ["d1-b1", "d1-b2", "d2-b3"]);
    assert_eq!(visible_strokes(&replay), ["s2", "s3"]);
    assert_eq!(paper, Paper::SquaredSmall, "the last arrival wins");
    assert_eq!(replay.tombstones.len(), 1);
    assert_eq!(replay.tombstones[0].stroke_ids, ["s1"]);

    replay_outbox(server.address, &ink_only(&device_2)).await;
    replay_outbox(server.address, &ink_only(&device_1)).await;
    let (paper_again, replay_again) = canonical(server.address, PAGE_ID).await;
    assert_eq!(replay_again, replay, "a second replay changes nothing");
    assert_eq!(paper_again, paper);
}

/// Device 2 reconnects first, so the erase of s1 arrives before s1 itself:
/// the tombstone still suppresses the later add (delete wins whatever the
/// order), so d1-b1 — all of whose ink is erased — stores nothing. The
/// surviving batches stack in arrival order, and the paper is device 1's.
#[sqlx::test(migrations = "./migrations")]
async fn two_devices_replaying_device_two_first_converge(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;
    let (device_1, device_2) = device_outboxes();

    replay_outbox(server.address, &device_2).await;
    replay_outbox(server.address, &device_1).await;

    let (paper, replay) = canonical(server.address, PAGE_ID).await;
    assert_eq!(batch_ids(&replay), ["d2-b3", "d1-b2"]);
    assert_eq!(visible_strokes(&replay), ["s3", "s2"]);
    assert_eq!(paper, Paper::RuledWide, "the last arrival wins");
    assert_eq!(replay.tombstones.len(), 1);
    assert_eq!(replay.tombstones[0].stroke_ids, ["s1"]);

    replay_outbox(server.address, &ink_only(&device_1)).await;
    replay_outbox(server.address, &ink_only(&device_2)).await;
    let (paper_again, replay_again) = canonical(server.address, PAGE_ID).await;
    assert_eq!(replay_again, replay, "a second replay changes nothing");
    assert_eq!(paper_again, paper);
}

/// A page binned on another device while this one was offline: the replayed
/// outbox is accepted and kept, and the page stays in the bin.
#[sqlx::test(migrations = "./migrations")]
async fn an_outbox_replayed_to_a_binned_page_is_stored_and_the_page_stays_binned(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;
    bin(&server, PAGE_ID).await;
    let (status, body) = call(&server, OWNER, Method::GET, "/api/bin", None).await;
    assert_eq!(status, StatusCode::OK);
    let binned_at = serde_json::from_slice::<protocol::ListPagesResponse>(&body)
        .expect("the bin")
        .pages[0]
        .binned_at
        .clone();

    replay_outbox(
        server.address,
        &[
            commit("binned-b1", &["kept-1", "erased"]),
            erase("binned-erase", &["erased"]),
            set_paper("binned-paper", Paper::RuledNarrow),
        ],
    )
    .await;

    let (paper, replay) = canonical(server.address, PAGE_ID).await;
    assert_eq!(batch_ids(&replay), ["binned-b1"]);
    assert_eq!(visible_strokes(&replay), ["kept-1"]);
    assert_eq!(paper, Paper::RuledNarrow);
    let (status, body) = call(&server, OWNER, Method::GET, "/api/bin", None).await;
    assert_eq!(status, StatusCode::OK);
    let bin = serde_json::from_slice::<protocol::ListPagesResponse>(&body).expect("the bin");
    assert_eq!(bin.pages.len(), 1);
    assert_eq!(bin.pages[0].id, PAGE_ID);
    assert_eq!(bin.pages[0].binned_at, binned_at, "still binned, unchanged");
    assert!(listed(&server, "/api/pages").await.is_empty());
}

/// A page permanently deleted while this device was offline: every replayed
/// mutation is the definitive `page_deleted`, naming what it refused, and
/// nothing is stored.
#[sqlx::test(migrations = "./migrations")]
async fn an_outbox_replayed_to_a_purged_page_is_page_deleted(pool: PgPool) {
    let server = serve(&pool).await;
    create_with_id(&server, OWNER, PAGE_ID).await;
    let (mut socket, _) = open(server.address, PAGE_ID).await;
    commit_and_await_echo(&mut socket, "purged-b1", "stroke-1").await;
    bin(&server, PAGE_ID).await;
    let (status, _) = call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/bin/{PAGE_ID}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The purge notice the open session is sent.
    next_matching(&mut socket, |message| {
        matches!(
            message,
            PageServerMessage::Error {
                client_mutation_id: None,
                ..
            }
        )
    })
    .await;

    for (message, mutation) in [
        (commit("purged-b1", &["stroke-1"]), "purged-b1"),
        (commit("purged-b2", &["stroke-2"]), "purged-b2"),
        (erase("purged-erase", &["stroke-1"]), "purged-erase"),
        (set_paper("purged-paper", Paper::RuledWide), "purged-paper"),
    ] {
        send(&mut socket, message).await;
        assert_eq!(
            next_message(&mut socket).await,
            PageServerMessage::Error {
                code: PAGE_DELETED_ERROR.to_string(),
                message: "this page was permanently deleted".to_string(),
                client_mutation_id: Some(mutation.to_string()),
            },
            "{mutation}"
        );
    }
    assert_eq!(
        rows(
            &pool,
            "SELECT COUNT(*) FROM stroke_batches WHERE page_id = $1",
            PAGE_ID
        )
        .await,
        0
    );
    assert_eq!(page_rows(&pool, PAGE_ID).await, 0);
}
