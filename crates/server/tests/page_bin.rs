//! The recycle bin for pages (#631), end to end through a real server, real
//! sockets and a real Postgres: a page moves live → bin → permanently deleted,
//! and only the last step destroys anything.
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
    CreatePageRequest, ListPagesResponse, PAGE_DELETED_ERROR, PageIds, PageResponse,
    PageServerMessage, Paper,
};
use server::build_router;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::util::ServiceExt;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpRequest, Options, WebSocket};

use common::{SignedTokenClaims, sign_test_token, test_auth_config, test_jwks_cache};

const OWNER: &str = "bin-owner";
const STRANGER: &str = "bin-stranger";

type Socket = WebSocket<TcpStream>;

fn token(user: &str) -> String {
    sign_test_token(SignedTokenClaims::valid_for(&test_auth_config(), user))
}

fn router(pool: &PgPool) -> axum::Router {
    build_router(
        "test-version".to_string(),
        Arc::new(test_auth_config()),
        Arc::new(test_jwks_cache()),
        pool.clone(),
    )
}

/// One server under test: REST calls go through `app`, sockets dial `address`,
/// and both share one state — so a REST bin reaches the open sockets' hub.
struct Server {
    app: axum::Router,
    address: std::net::SocketAddr,
}

/// One REST call as `user`: the status and the body.
async fn call(
    server: &Server,
    user: &str,
    method: Method,
    uri: &str,
    json: Option<String>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", token(user)));
    let body = match json {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json)
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

async fn create_page(server: &Server, title: &str) -> String {
    let body = serde_json::to_string(&CreatePageRequest {
        id: None,
        title: Some(title.to_string()),
        paper: Paper::None,
    })
    .expect("serializes");
    let (status, body) = call(server, OWNER, Method::POST, "/api/pages", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
    let page: PageResponse = serde_json::from_slice(&body).expect("a page response");
    page.page.id
}

async fn listed(server: &Server, uri: &str) -> ListPagesResponse {
    let (status, body) = call(server, OWNER, Method::GET, uri, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice(&body).expect("a page list")
}

fn ids(list: &ListPagesResponse) -> Vec<&str> {
    list.pages.iter().map(|page| page.id.as_str()).collect()
}

async fn serve(pool: &PgPool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = listener.local_addr().expect("has an address");
    let app = router(pool);
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await.expect("serves") });
    Server { app, address }
}

/// Opens the page channel as a Bearer caller (Android's path) and reads past
/// its `welcome`.
async fn open(address: std::net::SocketAddr, page_id: &str) -> Socket {
    let stream = TcpStream::connect(address).await.expect("connects");
    let url = format!("ws://{address}/api/pages/{page_id}/realtime")
        .parse()
        .expect("url");
    let mut socket = WebSocket::handshake_with_request(
        url,
        stream,
        Options::default(),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token(OWNER))),
    )
    .await
    .expect("upgrades");
    match next_message(&mut socket).await {
        PageServerMessage::Welcome { .. } => {}
        other => panic!("expected welcome, got {other:?}"),
    }
    socket
}

/// The status line the page channel's upgrade is answered with, as `user`.
async fn upgrade_status(address: std::net::SocketAddr, page_id: &str, user: &str) -> u16 {
    let mut stream = TcpStream::connect(address).await.expect("connects");
    let request = format!(
        "GET /api/pages/{page_id}/realtime HTTP/1.1\r\nHost: {address}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Authorization: Bearer {}\r\n\r\n",
        token(user)
    );
    stream.write_all(request.as_bytes()).await.expect("writes");
    let mut head = [0_u8; 12];
    stream
        .read_exact(&mut head)
        .await
        .expect("reads a status line");
    std::str::from_utf8(&head[9..12])
        .expect("ascii")
        .parse()
        .expect("a status code")
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

/// Reads until a message `keep` accepts, skipping the rest.
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

fn commit(batch: &str, stroke_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "commit-batch",
        "client_batch_id": batch,
        "strokes": [{
            "id": stroke_id,
            "style": {
                "tool_kind": "solid_round",
                "style_version": 2,
                "parameters": { "color": "#006400", "width": 2.0, "cap_style": "round", "join_style": "round" }
            },
            "points": [{ "x": 1.0, "y": 2.0, "t": 0 }, { "x": 30.0, "y": 40.0, "t": 16 }],
        }],
    })
}

async fn commit_and_await_echo(socket: &mut Socket, batch: &str, stroke_id: &str) {
    send(socket, commit(batch, stroke_id)).await;
    let echo = next_matching(socket, |message| {
        matches!(
            message,
            PageServerMessage::StrokeBatch(_) | PageServerMessage::Error { .. }
        )
    })
    .await;
    match echo {
        PageServerMessage::StrokeBatch(batch) => assert_eq!(batch.strokes[0].id, stroke_id),
        other => panic!("the commit should be accepted, got {other:?}"),
    }
}

/// Every visible stroke id on the page, from a fresh full replay.
async fn replayed_strokes(address: std::net::SocketAddr, page_id: &str) -> Vec<String> {
    let mut socket = open(address, page_id).await;
    send(
        &mut socket,
        serde_json::json!({ "type": "subscribe", "from_seq": 0 }),
    )
    .await;
    let PageServerMessage::PageReplay(replay) = next_matching(&mut socket, |message| {
        matches!(message, PageServerMessage::PageReplay(_))
    })
    .await
    else {
        unreachable!()
    };
    replay
        .batches
        .iter()
        .flat_map(|batch| batch.strokes.iter().map(|stroke| stroke.id.clone()))
        .collect()
}

async fn rows_for(pool: &PgPool, table: &str, page_id: &str) -> i64 {
    let query = match table {
        "stroke_batches" => "SELECT COUNT(*) FROM stroke_batches WHERE page_id = $1",
        "stroke_tombstones" => "SELECT COUNT(*) FROM stroke_tombstones WHERE page_id = $1",
        "tombstone_batches" => "SELECT COUNT(*) FROM tombstone_batches WHERE page_id = $1",
        "page_thumbnails" => "SELECT COUNT(*) FROM page_thumbnails WHERE page_id = $1",
        "pages" => "SELECT COUNT(*) FROM pages WHERE id = $1",
        "purged_pages" => "SELECT COUNT(*) FROM purged_pages WHERE id = $1",
        other => panic!("unknown table {other}"),
    };
    sqlx::query_scalar(query)
        .bind(page_id)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// Acceptance 1 and 2: deleting moves the page to the bin — out of the library,
/// into the bin, every open session told — while its ink stays and new ink
/// committed to it is kept without restoring it; restoring brings it all back.
#[sqlx::test(migrations = "./migrations")]
async fn a_binned_page_keeps_its_ink_accepts_more_and_restores_whole(pool: PgPool) {
    let server = serve(&pool).await;
    let address = server.address;
    let page_id = create_page(&server, "binned").await;
    let mut editor = open(address, &page_id).await;
    commit_and_await_echo(&mut editor, "before", "stroke-before").await;

    let (status, _) = call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The open session is told at once.
    let binned_at = match next_matching(&mut editor, |message| {
        matches!(message, PageServerMessage::PageBinned { .. })
    })
    .await
    {
        PageServerMessage::PageBinned { binned_at } => binned_at,
        _ => unreachable!(),
    };
    assert!(
        ids(&listed(&server, "/api/pages").await).is_empty(),
        "the library no longer lists it"
    );
    let bin = listed(&server, "/api/bin").await;
    assert_eq!(ids(&bin), [page_id.as_str()]);
    assert_eq!(bin.pages[0].binned_at.as_deref(), Some(binned_at.as_str()));

    // Binning again is a no-op.
    let (status, _) = call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        listed(&server, "/api/bin").await.pages[0].binned_at,
        Some(binned_at.clone())
    );

    // A commit to the binned page is accepted and kept; the page stays binned.
    commit_and_await_echo(&mut editor, "after", "stroke-after").await;
    assert_eq!(
        ids(&listed(&server, "/api/bin").await),
        [page_id.as_str()],
        "still binned"
    );
    assert!(ids(&listed(&server, "/api/pages").await).is_empty());

    // A session that opens the binned page is told straight after `welcome`.
    let mut late = open(address, &page_id).await;
    assert_eq!(
        next_message(&mut late).await,
        PageServerMessage::PageBinned { binned_at }
    );

    let (status, body) = call(
        &server,
        OWNER,
        Method::POST,
        &format!("/api/pages/{page_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let restored: PageResponse = serde_json::from_slice(&body).expect("a page response");
    assert_eq!(restored.page.binned_at, None);
    for socket in [&mut editor, &mut late] {
        next_matching(socket, |message| {
            matches!(message, PageServerMessage::PageRestored)
        })
        .await;
    }
    assert_eq!(
        ids(&listed(&server, "/api/pages").await),
        [page_id.as_str()]
    );
    assert!(ids(&listed(&server, "/api/bin").await).is_empty());
    assert_eq!(
        replayed_strokes(address, &page_id).await,
        ["stroke-before", "stroke-after"],
        "restored with all its ink, including what was drawn while binned"
    );

    // Restoring a live page is a no-op that returns it.
    let (status, _) = call(
        &server,
        OWNER,
        Method::POST,
        &format!("/api/pages/{page_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Acceptance 3 and 4: permanent deletion is from the bin only, removes the
/// page and everything derived from it, and every later write for it is the
/// definitive `page_deleted` (`410` over REST) — not the stranger's `403`.
#[sqlx::test(migrations = "./migrations")]
async fn permanently_deleting_removes_everything_and_rejects_later_writes_distinctly(pool: PgPool) {
    let server = serve(&pool).await;
    let address = server.address;
    let page_id = create_page(&server, "doomed").await;
    let mut editor = open(address, &page_id).await;
    commit_and_await_echo(&mut editor, "batch-1", "stroke-1").await;
    commit_and_await_echo(&mut editor, "batch-2", "stroke-2").await;
    send(
        &mut editor,
        serde_json::json!({ "type": "commit-tombstones", "client_mutation_id": "erase-1", "stroke_ids": ["stroke-1"] }),
    )
    .await;
    next_matching(&mut editor, |message| {
        matches!(message, PageServerMessage::TombstoneBatch(_))
    })
    .await;
    for table in [
        "stroke_batches",
        "stroke_tombstones",
        "tombstone_batches",
        "page_thumbnails",
    ] {
        assert!(
            rows_for(&pool, table, &page_id).await > 0,
            "{table} has rows to remove"
        );
    }

    let purge = format!("/api/bin/{page_id}");
    let (status, body) = call(&server, OWNER, Method::DELETE, &purge, None).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a live page cannot be permanently deleted"
    );
    assert_eq!(body, b"page is not in the bin");

    call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    let (status, _) = call(&server, STRANGER, Method::DELETE, &purge, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "not the stranger's to delete"
    );
    let (status, _) = call(&server, OWNER, Method::DELETE, &purge, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    for table in [
        "pages",
        "stroke_batches",
        "stroke_tombstones",
        "tombstone_batches",
        "page_thumbnails",
    ] {
        assert_eq!(
            rows_for(&pool, table, &page_id).await,
            0,
            "{table} is purged"
        );
    }
    assert_eq!(rows_for(&pool, "purged_pages", &page_id).await, 1);
    assert!(ids(&listed(&server, "/api/bin").await).is_empty());

    // The open session is told, and its next commit is rejected as such.
    let told = next_matching(&mut editor, |message| {
        matches!(message, PageServerMessage::Error { .. })
    })
    .await;
    assert!(
        matches!(told, PageServerMessage::Error { ref code, client_mutation_id: None, .. } if code == PAGE_DELETED_ERROR),
        "{told:?}"
    );
    send(&mut editor, commit("batch-3", "stroke-3")).await;
    assert_eq!(
        next_message(&mut editor).await,
        PageServerMessage::Error {
            code: PAGE_DELETED_ERROR.to_string(),
            message: "this page was permanently deleted".to_string(),
            client_mutation_id: Some("batch-3".to_string()),
        }
    );

    // Over REST and on the upgrade it is `410 Gone` for the owner…
    for (method, uri) in [
        (Method::GET, format!("/api/pages/{page_id}")),
        (Method::DELETE, format!("/api/pages/{page_id}")),
        (Method::POST, format!("/api/pages/{page_id}/restore")),
    ] {
        let (status, body) = call(&server, OWNER, method.clone(), &uri, None).await;
        assert_eq!(status, StatusCode::GONE, "{method} {uri}");
        assert_eq!(body, b"page permanently deleted", "{method} {uri}");
    }
    assert_eq!(upgrade_status(address, &page_id, OWNER).await, 410);
    // …and a bin or restore of it is counted as `page_gone`, as the page
    // channel counts it, not as a stranger's `not_found`. Presence is proof:
    // no other test in this binary bins or restores a purged page.
    let response = server::observability::metrics_handler().await;
    let metrics = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("metrics body")
            .to_vec(),
    )
    .expect("utf-8");
    for operation in ["bin_page", "restore_page"] {
        assert!(
            metrics.contains(&format!(
                "v_note_page_mutations_total{{operation=\"{operation}\",result=\"page_gone\"}}"
            )),
            "{operation} on a purged page counts page_gone"
        );
    }
    // …idempotent for a repeated permanent delete…
    let (status, _) = call(&server, OWNER, Method::DELETE, &purge, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // …and still the plain not-found for anyone else.
    let (status, _) = call(
        &server,
        STRANGER,
        Method::GET,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(upgrade_status(address, &page_id, STRANGER).await, 403);
}

/// Empty bin permanently deletes every binned page of the caller's, and nothing
/// else: their live pages and another owner's bin are untouched.
#[sqlx::test(migrations = "./migrations")]
async fn emptying_the_bin_deletes_only_the_callers_binned_pages(pool: PgPool) {
    let server = serve(&pool).await;
    let live = create_page(&server, "live").await;
    let first = create_page(&server, "first").await;
    let second = create_page(&server, "second").await;
    for page in [&first, &second] {
        call(
            &server,
            OWNER,
            Method::DELETE,
            &format!("/api/pages/{page}"),
            None,
        )
        .await;
    }
    let body = serde_json::to_string(&CreatePageRequest {
        id: None,
        title: None,
        paper: Paper::None,
    })
    .expect("serializes");
    let (_, body) = call(&server, STRANGER, Method::POST, "/api/pages", Some(body)).await;
    let foreign: PageResponse = serde_json::from_slice(&body).expect("a page response");
    call(
        &server,
        STRANGER,
        Method::DELETE,
        &format!("/api/pages/{}", foreign.page.id),
        None,
    )
    .await;
    let mut binned = ids(&listed(&server, "/api/bin").await)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    binned.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(
        binned, expected,
        "the bin lists only the caller's binned pages"
    );

    // Binned on another device after the user was shown the bin: not named in
    // the request, so it must survive to be restored. The live page and the
    // stranger's binned page are named too, and must survive as well.
    let late = create_page(&server, "late").await;
    call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{late}"),
        None,
    )
    .await;
    let confirmed = serde_json::to_string(&PageIds {
        page_ids: vec![
            first.clone(),
            second.clone(),
            live.clone(),
            foreign.page.id.clone(),
        ],
    })
    .expect("serializes");
    let (status, body) = call(&server, OWNER, Method::DELETE, "/api/bin", Some(confirmed)).await;
    assert_eq!(status, StatusCode::OK);
    let mut deleted = serde_json::from_slice::<PageIds>(&body)
        .expect("the deleted ids")
        .page_ids;
    deleted.sort();
    assert_eq!(
        deleted, expected,
        "only the confirmed binned pages, and it says which"
    );

    assert_eq!(
        ids(&listed(&server, "/api/bin").await),
        [late.as_str()],
        "a page binned after the confirmation is not destroyed unseen"
    );
    assert_eq!(ids(&listed(&server, "/api/pages").await), [live.as_str()]);
    for page in [&first, &second] {
        let (status, _) = call(
            &server,
            OWNER,
            Method::GET,
            &format!("/api/pages/{page}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::GONE, "{page}");
    }
    assert_eq!(
        rows_for(&pool, "pages", &foreign.page.id).await,
        1,
        "another owner's bin is untouched"
    );
}

/// Bin and restore are the owner's alone: a stranger gets the same `403` a
/// read does, and nothing moves.
#[sqlx::test(migrations = "./migrations")]
async fn a_stranger_can_neither_bin_nor_restore_a_page(pool: PgPool) {
    let server = serve(&pool).await;
    let page_id = create_page(&server, "mine").await;
    let (status, _) = call(
        &server,
        STRANGER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        ids(&listed(&server, "/api/pages").await),
        [page_id.as_str()]
    );

    call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    let (status, _) = call(
        &server,
        STRANGER,
        Method::POST,
        &format!("/api/pages/{page_id}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(ids(&listed(&server, "/api/bin").await), [page_id.as_str()]);
}

/// An event published right after a library socket opens is delivered (#631).
///
/// This does **not** deterministically guard the ordering fix behind the CI
/// race in `bin.spec.ts` (the server subscribing before it answers the
/// upgrade): the publish here waits on a Postgres `UPDATE` first, so the old
/// ordering — subscribing in the spawned socket task — would almost always win
/// too. That ordering is pinned deterministically by
/// `the_library_socket_is_subscribed_before_its_upgrade_is_answered` in
/// `src/realtime/socket.rs` (#636), which reads the hub before the 101 is sent.
#[sqlx::test(migrations = "./migrations")]
async fn a_library_socket_hears_an_event_published_the_moment_it_opens(pool: PgPool) {
    let server = serve(&pool).await;
    let page_id = create_page(&server, "instant").await;
    let address = server.address;
    let stream = TcpStream::connect(address).await.expect("connects");
    let mut library = WebSocket::handshake_with_request(
        format!("ws://{address}/api/realtime").parse().expect("url"),
        stream,
        Options::default(),
        HttpRequest::builder().header("Authorization", format!("Bearer {}", token(OWNER))),
    )
    .await
    .expect("upgrades");

    let (status, _) = call(
        &server,
        OWNER,
        Method::DELETE,
        &format!("/api/pages/{page_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let frame = tokio::time::timeout(Duration::from_secs(10), library.next())
        .await
        .expect("the event arrives")
        .expect("the socket stays open");
    let event: protocol::LibraryEvent =
        serde_json::from_slice(frame.payload()).expect("a library event");
    assert!(
        matches!(event, protocol::LibraryEvent::PageBinned { ref page } if page.id == page_id),
        "{event:?}"
    );
}
