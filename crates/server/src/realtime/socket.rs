//! Axum handlers for the two WebSocket endpoints and the connection loops
//! behind them. Authentication and the socket lifecycle live here; what a
//! message *does* is `dispatch`, and where it is stored is `store`.

use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use protocol::{PageClientMessage, PageServerMessage, RealtimeTicketResponse};
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::Instrument as _;
use yawc::frame::{Frame, OpCode};
use yawc::{HttpWebSocket, IncomingUpgrade, Options};

use super::dispatch::{PageContext, dispatch_page_client_message, send_page, send_page_frame};
use super::hub::{Fanout, fanout_delivery_span, random_hex};
use super::store::{PageStore, PgPageStore};
use crate::AppState;
use crate::auth::{Claims, extract_bearer, validate_jwt};

/// The sending half of an upgraded realtime socket, which is what `dispatch`
/// writes replies to.
type PageSender = SplitSink<HttpWebSocket, Frame>;

/// Largest inbound message accepted on either channel.
///
/// Set explicitly rather than inherited: `yawc` defaults to 1 MiB where
/// `axum`/`tungstenite` defaulted to 64 MiB, so leaving it unstated would
/// silently tighten an inbound limit by 64x as a side effect of #342. 1 MiB is
/// the right number on its own merits — the largest thing a client sends is a
/// `commit-batch`, p95 ~8 KB on dev, so this is ~100x headroom — and it caps
/// what a `permessage-deflate` peer can force us to inflate. It bounds reads
/// only; the server's own replay frames are megabytes and are not affected.
const MAX_INBOUND_MESSAGE_BYTES: usize = 1024 * 1024;

/// WebSocket options for a realtime upgrade, with `permessage-deflate` offered
/// only when `realtime.compression` is on (#342).
///
/// **Compression level 6 with context takeover** (`Options::with_balanced_compression`,
/// whose doc comment claiming "no context takeover" contradicts its own body —
/// it leaves both `*_no_context_takeover` flags false). Both halves of that are
/// deliberate:
///
/// - **Context takeover is kept on** because the continuous win here is live
///   `stroke-batch` fan-out, where consecutive frames are near-identical JSON
///   from the same page and a carried-over dictionary is most of the benefit.
///   It costs a per-connection zlib window, which is the number #342 requires
///   be measured before this flag goes on in a deployed environment. Turning it
///   off would save
///   little in `yawc` anyway: `no_context_takeover` *resets* the compressor
///   between messages rather than freeing it.
/// - **Level 6, not 9**, because #323 bought a 13% replay latency win that
///   compression trades CPU against, and level 9 on a multi-megabyte replay
///   frame is where that gets given back. Level 6 is the starting point the
///   dev measurement is taken from, not a tuned result.
///
/// When the flag is off, no extension is offered, so every client is served
/// exactly the pre-#342 wire.
fn realtime_ws_options(state: &AppState) -> Options {
    let options = Options::default().with_max_payload_read(MAX_INBOUND_MESSAGE_BYTES);
    if state.realtime_compression {
        options.with_balanced_compression()
    } else {
        options
    }
}

/// Completes an upgrade: turns `yawc`'s handshake response into an axum one and
/// spawns `handler` on the upgraded socket.
///
/// `upgrade_span` is the upgrade request's span. `yawc` hands back a future we
/// spawn ourselves rather than `axum`'s `on_upgrade` callback, so — exactly as
/// before — the connection is instrumented back into the request's span, which
/// keeps it in the same trace as the upgrade (and so under Traefik's edge span).
fn spawn_upgraded<F, Fut>(
    upgrade: IncomingUpgrade,
    options: Options,
    channel: &'static str,
    handler: F,
) -> Response
where
    F: FnOnce(HttpWebSocket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let (response, upgraded) = match upgrade.upgrade(options) {
        Ok(pair) => pair,
        Err(error) => {
            tracing::error!(error = %error, channel, "websocket upgrade failed");
            crate::observability::metrics().record_realtime_event(channel, "upgrade_error");
            return (StatusCode::BAD_REQUEST, "websocket upgrade failed").into_response();
        }
    };

    // Whether `permessage-deflate` was actually agreed, read off the handshake
    // response we are about to send rather than inferred from our own config:
    // agreement needs the client to have offered it too, so the flag being on
    // is necessary and not sufficient. This is the only server-side signal that
    // #342 is live on a connection, and it is what the e2e test asserts.
    //
    // Counted **here, at the handshake** — not after `upgraded.await` with the
    // other connection events. That makes these two the only values in this
    // counter that are not connection-lifecycle events, which is a deliberate
    // trade rather than an oversight: what the rollout needs to know is what
    // share of *upgrades* agreed the extension, and an upgrade that negotiates
    // and then fails to come up is still evidence the negotiation works. Such a
    // connection is counted both here and as `upgrade_error` below, so the two
    // do not sum to the connection count when upgrades are failing.
    let negotiated = response
        .headers()
        .contains_key(axum::http::header::SEC_WEBSOCKET_EXTENSIONS);
    crate::observability::metrics().record_realtime_event(
        channel,
        if negotiated {
            "compressed"
        } else {
            "uncompressed"
        },
    );
    tracing::Span::current().record("permessage_deflate", negotiated);

    let upgrade_span = tracing::Span::current();
    tokio::spawn(
        async move {
            match upgraded.await {
                Ok(socket) => handler(socket).await,
                // The 101 has already been written, so there is no status left
                // to return: the handshake completed and the connection then
                // failed to come up. Counted so it is not invisible.
                Err(error) => {
                    tracing::warn!(error = %error, channel, "websocket never came up");
                    crate::observability::metrics().record_realtime_event(channel, "upgrade_error");
                }
            }
        }
        .instrument(upgrade_span),
    );

    response.map(axum::body::Body::new)
}

/// Classifies the end of an inbound stream into the same `closed` / `recv_error`
/// split the pre-#342 socket reported, which `yawc`'s `Stream` impl would
/// otherwise collapse.
///
/// `yawc` maps a read error to `Poll::Ready(None)` (`impl Stream for WebSocket`),
/// so after `split()` an error is indistinguishable from the stream ending. The
/// distinction survives because it does not need the `Result`: a peer closing
/// cleanly sends a `Close` frame, which `yawc` passes through to the reader
/// before the stream ends. So a `Close` frame is `closed`, and the stream ending
/// without one is abnormal — a protocol error, or a peer that dropped the
/// connection without the closing handshake.
///
/// This is if anything closer to the truth than what it replaces: `axum`
/// previously reported a dropped connection as `Some(Err(_))` → `recv_error`,
/// but bare stream exhaustion as `closed`.
fn record_stream_end(channel: &'static str) {
    crate::observability::metrics().record_realtime_event(channel, "recv_error");
}

pub async fn realtime_ticket(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> Json<RealtimeTicketResponse> {
    Json(state.realtime.issue_ticket(claims.sub))
}

#[derive(Debug, Deserialize)]
pub struct RealtimeQuery {
    ticket: Option<String>,
}

#[tracing::instrument(skip_all, fields(permessage_deflate))]
pub async fn realtime_socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RealtimeQuery>,
    ws: IncomingUpgrade,
) -> Response {
    // Unchanged by #342: `IncomingUpgrade` is an extractor, so authentication
    // still runs here, before anything is upgraded. A rejected request never
    // reaches `spawn_upgraded` and so never switches protocols.
    let owner_id = match authenticate_realtime(&state, &headers, query.ticket.as_deref()).await {
        Ok(owner_id) => owner_id,
        Err(status) => {
            crate::observability::metrics().record_auth_failure("realtime_auth");
            return (status, "realtime authentication failed").into_response();
        }
    };

    let options = realtime_ws_options(&state);
    spawn_upgraded(ws, options, "library", move |socket| {
        handle_library_socket(state, owner_id, socket)
    })
}

async fn authenticate_realtime(
    state: &AppState,
    headers: &HeaderMap,
    ticket: Option<&str>,
) -> Result<String, StatusCode> {
    if let Some(ticket) = ticket {
        return state
            .realtime
            .consume_ticket(ticket)
            .ok_or(StatusCode::UNAUTHORIZED);
    }

    let token = extract_bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    validate_jwt(&token, &state.auth, &state.jwks_cache)
        .await
        .map(|claims| claims.sub)
        .map_err(|error| match error {
            crate::auth::TokenValidationError::MissingScope => StatusCode::FORBIDDEN,
            crate::auth::TokenValidationError::Invalid(_) => StatusCode::UNAUTHORIZED,
        })
}

#[tracing::instrument(skip_all)]
async fn handle_library_socket(state: AppState, owner_id: String, socket: HttpWebSocket) {
    let _connection_guard = crate::observability::metrics().realtime_connection_guard();
    crate::observability::metrics().record_realtime_event("library", "connected");
    let mut receiver = state.realtime.subscribe_library(&owner_id);
    let (mut sender, mut inbound) = socket.split();

    loop {
        tokio::select! {
            event = receiver.recv() => {
                let Fanout { message: event, origin } = match event {
                    Ok(fanout) => fanout,
                    // Unlike the page channel, a lagged library socket closes.
                    // Counted here so it is visible; recovery is #279.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        crate::observability::metrics().record_realtime_event("library", "lagged");
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let message_type = event.message_type();
                let Ok(payload) = serde_json::to_string(&event) else {
                    continue;
                };
                let bytes = payload.len();
                let delivery = fanout_delivery_span("library", message_type, &origin);
                delivery.record("bytes", bytes);
                if sender
                    .send(Frame::text(payload))
                    .instrument(delivery)
                    .await
                    .is_err()
                {
                    crate::observability::metrics().record_realtime_event("library", "send_error");
                    break;
                }
                crate::observability::metrics().observe_realtime_message_bytes(
                    "library",
                    message_type,
                    bytes,
                );
            }
            message = inbound.next() => {
                match message {
                    Some(frame) if frame.opcode() == OpCode::Close => {
                        crate::observability::metrics().record_realtime_event("library", "closed");
                        break;
                    }
                    // The library channel is server→client only; anything else
                    // the peer sends (including its `ping`/`pong`, which `yawc`
                    // answers itself) is ignored.
                    Some(_) => {}
                    None => {
                        record_stream_end("library");
                        break;
                    }
                }
            }
        }
    }
}

// ---- Page channel (per-`page_id` ink WSS) --------------------------------

/// Per-page ink socket. Authenticates like the library socket (Bearer for
/// Android, realtime ticket for SPA), then enforces owner-only access to the
/// page before upgrading. Bidirectional: gap-fill/snapshot, edit lease, and
/// coalesced stroke-batch commits fanned out to the owner's sibling sessions.
#[tracing::instrument(skip_all, fields(page_id = %page_id, permessage_deflate))]
pub async fn page_socket(
    State(state): State<AppState>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RealtimeQuery>,
    ws: IncomingUpgrade,
) -> Response {
    // Both gates below still run before any upgrade — see `realtime_socket`.
    let owner_id = match authenticate_realtime(&state, &headers, query.ticket.as_deref()).await {
        Ok(owner_id) => owner_id,
        Err(status) => {
            crate::observability::metrics().record_auth_failure("realtime_auth");
            return (status, "realtime authentication failed").into_response();
        }
    };

    let store = PgPageStore::new(state.db.clone());

    match store.page_belongs_to_owner(&page_id, &owner_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::FORBIDDEN, "page not found").into_response(),
        Err(error) => {
            tracing::error!(error = %error, "page ownership check failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "ownership check failed").into_response();
        }
    }

    let options = realtime_ws_options(&state);
    spawn_upgraded(ws, options, "page", move |socket| {
        handle_page_socket(state, store, page_id, socket)
    })
}

#[tracing::instrument(skip_all, fields(page_id = %page_id, session_id))]
async fn handle_page_socket(
    state: AppState,
    store: PgPageStore,
    page_id: String,
    socket: HttpWebSocket,
) {
    let _connection_guard = crate::observability::metrics().realtime_connection_guard();
    crate::observability::metrics().record_realtime_event("page", "connected");
    let session_id = format!("session_{}", random_hex(16));
    tracing::Span::current().record("session_id", session_id.as_str());
    let mut receiver = state.realtime.subscribe_page(&page_id);
    let (mut sender, mut inbound) = socket.split();

    let last_seq = store.max_seq(&page_id).await.unwrap_or(0);
    let lease_holder = state.realtime.current_lease_holder(&page_id);
    // Carrying paper here makes the page channel self-sufficient: a reconnecting
    // client gets the authoritative value without a second REST round trip, which
    // is also what self-corrects a stale `PageSummary.paper` in an open library.
    //
    // A read failure closes the connection rather than degrading to `none`.
    // Unlike `last_seq` above — where a wrong value is repaired by the next
    // gap-fill — nothing downstream re-reads paper: `Subscribe` replays only ink
    // and tombstones, so a synthesized blank would render as a blank page for as
    // long as the socket stayed open. Failing loudly is recoverable; rendering
    // the wrong page silently is not.
    let paper = match store.current_paper(&page_id).await {
        Ok(paper) => paper,
        Err(error) => {
            tracing::error!(error = %error, %page_id, "could not read page paper");
            crate::observability::metrics().record_realtime_event("page", "paper_read_error");
            send_page(
                &mut sender,
                PageServerMessage::Error {
                    code: "welcome_failed".to_string(),
                    message: "could not load the page".to_string(),
                    client_mutation_id: None,
                },
            )
            .await;
            return;
        }
    };
    let welcome = PageServerMessage::Welcome {
        session_id: session_id.clone(),
        last_seq,
        lease_holder,
        paper,
    };
    if !send_page(&mut sender, welcome).await {
        crate::observability::metrics().record_realtime_event("page", "send_error");
        return;
    }

    loop {
        tokio::select! {
            event = receiver.recv() => {
                match event {
                    Ok(Fanout { message, origin }) => {
                        let delivery = fanout_delivery_span("page", message.message_type(), &origin);
                        delivery.record("session_id", session_id.as_str());
                        match send_page_frame(&mut sender, message)
                            .instrument(delivery.clone())
                            .await
                        {
                            Some(bytes) => {
                                delivery.record("bytes", bytes);
                            }
                            None => {
                                crate::observability::metrics().record_realtime_event("page", "send_error");
                                break;
                            }
                        }
                    }
                    // Dropped fan-out is still skipped — recovery is #279 — but
                    // no longer silently.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        crate::observability::metrics().record_realtime_event("page", "lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            inbound_message = inbound.next() => {
                match inbound_message {
                    Some(frame) if frame.opcode() == OpCode::Text => {
                        // `yawc` has already inflated the payload if the peer
                        // compressed it, so the dispatch path below sees the
                        // same JSON either way. A text frame whose payload is
                        // not UTF-8 is a protocol violation, not a message.
                        let Ok(text) = std::str::from_utf8(frame.payload()) else {
                            crate::observability::metrics().record_realtime_event("page", "recv_error");
                            break;
                        };
                        if !handle_page_client_message(
                            &state, &store, &page_id, &session_id, &mut sender, text,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Some(frame) if frame.opcode() == OpCode::Close => {
                        crate::observability::metrics().record_realtime_event("page", "closed");
                        break;
                    }
                    Some(_) => {}
                    None => {
                        record_stream_end("page");
                        break;
                    }
                }
            }
        }
    }

    // Release the lease on disconnect so a sibling session can take over.
    if state.realtime.release_lease(&page_id, &session_id) {
        state
            .realtime
            .publish_page(&page_id, PageServerMessage::LeaseChanged { holder: None });
    }
}

/// Parses, dispatches and times one inbound page-channel message. Returns false
/// when the socket should close (send failure).
///
/// Each message is its own trace root, *linked* to the connection span rather
/// than nested in it: a page socket stays open for hours, and a trace rooted at
/// the connection would not be complete — or usefully searchable by duration —
/// in Tempo until the socket closed.
#[tracing::instrument(
    skip_all,
    parent = None,
    follows_from = [tracing::Span::current().id()],
    fields(page_id = %page_id, session_id = %session_id, message_type),
)]
async fn handle_page_client_message(
    state: &AppState,
    store: &PgPageStore,
    page_id: &str,
    session_id: &str,
    sender: &mut PageSender,
    text: &str,
) -> bool {
    let received = Instant::now();
    let message: PageClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(_) => {
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "bad_message".to_string(),
                    message: "could not parse client message".to_string(),
                    client_mutation_id: None,
                },
            )
            .await;
        }
    };
    let message_type = message.message_type();
    tracing::Span::current().record("message_type", message_type);

    let ctx = PageContext {
        state,
        store,
        page_id,
        session_id,
    };
    let keep_open = dispatch_page_client_message(&ctx, sender, message, received).await;
    // Server-side handling only — parse to the last frame this handler sends.
    // Not end-to-end freshness, which needs client timestamps (#154).
    crate::observability::metrics()
        .observe_realtime_message_handling(message_type, received.elapsed().as_secs_f64());
    keep_open
}

#[cfg(test)]
mod tests {
    //! The upgrade options this server chooses (#342). Negotiation itself is
    //! `yawc`'s to get right — and is covered end to end against a real browser
    //! by the `permessage-deflate` e2e test — so what is worth pinning here is
    //! the part v-note decides: whether the extension is offered at all, and
    //! the two settings that were chosen rather than inherited.

    use super::*;

    /// The flag off must offer **no** extension, so a rollback is a true
    /// rollback: with nothing negotiated, every client is served the exact
    /// pre-#342 wire rather than a differently-tuned compressed one.
    #[tokio::test]
    async fn compression_off_offers_no_extension() {
        let state = AppState {
            realtime_compression: false,
            ..AppState::for_tests()
        };

        assert!(
            realtime_ws_options(&state).compression.is_none(),
            "the rollback path must not offer permessage-deflate"
        );
    }

    /// The flag on offers compression at the level and context-takeover setting
    /// `realtime_ws_options` documents. Both are deliberate choices #342 has to
    /// justify with measurements, so a silent change to either should fail here.
    #[tokio::test]
    async fn compression_on_offers_level_six_with_context_takeover() {
        let state = AppState {
            realtime_compression: true,
            ..AppState::for_tests()
        };

        let deflate = realtime_ws_options(&state)
            .compression
            .expect("compression should be offered");

        assert_eq!(
            deflate.level.level(),
            6,
            "level 6 is the documented trade against #323's replay-latency win"
        );
        // Context takeover on: the live `stroke-batch` fan-out win depends on
        // carrying the dictionary between messages. `yawc`'s own doc comment on
        // `balanced()` claims the opposite of what its body does, so this is
        // asserted against behaviour rather than trusted from the docs.
        assert!(
            !deflate.server_no_context_takeover,
            "server-side context takeover should stay on"
        );
        assert!(
            !deflate.client_no_context_takeover,
            "the server should not force clients to drop their context"
        );
    }

    /// The inbound cap is stated rather than inherited, in both settings —
    /// `yawc` defaults to 1 MiB where `axum`/`tungstenite` defaulted to 64 MiB,
    /// and that difference should be a decision, not a side effect of #342.
    #[tokio::test]
    async fn the_inbound_cap_is_stated_whether_or_not_compression_is_on() {
        for compression in [false, true] {
            let state = AppState {
                realtime_compression: compression,
                ..AppState::for_tests()
            };

            assert_eq!(
                realtime_ws_options(&state).max_payload_read,
                Some(MAX_INBOUND_MESSAGE_BYTES),
                "inbound cap should be explicit with compression {compression}"
            );
        }
    }
}
