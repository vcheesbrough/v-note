//! Axum handlers for the two WebSocket endpoints and the connection loops
//! behind them. Authentication and the socket lifecycle live here; what a
//! message *does* is `dispatch`, and where it is stored is `store`.

use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use protocol::{PageClientMessage, PageServerMessage, RealtimeTicketResponse};
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::Instrument as _;

use super::dispatch::{PageContext, dispatch_page_client_message, send_page, send_page_frame};
use super::hub::{Fanout, fanout_delivery_span, random_hex};
use super::store::{PageStore, PgPageStore};
use crate::AppState;
use crate::auth::{Claims, extract_bearer, validate_jwt};

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

#[tracing::instrument(skip_all)]
pub async fn realtime_socket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RealtimeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let owner_id = match authenticate_realtime(&state, &headers, query.ticket.as_deref()).await {
        Ok(owner_id) => owner_id,
        Err(status) => {
            crate::observability::metrics().record_auth_failure("realtime_auth");
            return (status, "realtime authentication failed").into_response();
        }
    };

    // `on_upgrade` runs the socket in a spawned task, which has no current span.
    // Calling the handler inside the upgrade request's span keeps the connection
    // in the same trace as the request (and so under Traefik's edge span).
    let upgrade_span = tracing::Span::current();
    ws.on_upgrade(move |socket| {
        async move {
            handle_library_socket(state, owner_id, socket).await;
        }
        .instrument(upgrade_span)
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
async fn handle_library_socket(state: AppState, owner_id: String, socket: WebSocket) {
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
                    .send(Message::Text(payload.into()))
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
                    Some(Ok(Message::Close(_))) | None => {
                        crate::observability::metrics().record_realtime_event("library", "closed");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        crate::observability::metrics().record_realtime_event("library", "recv_error");
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
#[tracing::instrument(skip_all, fields(page_id = %page_id))]
pub async fn page_socket(
    State(state): State<AppState>,
    Path(page_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<RealtimeQuery>,
    ws: WebSocketUpgrade,
) -> Response {
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

    // See `realtime_socket`: keep the connection in the upgrade request's trace.
    let upgrade_span = tracing::Span::current();
    ws.on_upgrade(move |socket| {
        async move {
            handle_page_socket(state, store, page_id, socket).await;
        }
        .instrument(upgrade_span)
    })
}

#[tracing::instrument(skip_all, fields(page_id = %page_id, session_id))]
async fn handle_page_socket(
    state: AppState,
    store: PgPageStore,
    page_id: String,
    socket: WebSocket,
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
                    Some(Ok(Message::Text(text))) => {
                        if !handle_page_client_message(
                            &state, &store, &page_id, &session_id, &mut sender, &text,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        crate::observability::metrics().record_realtime_event("page", "closed");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        crate::observability::metrics().record_realtime_event("page", "recv_error");
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
    sender: &mut SplitSink<WebSocket, Message>,
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
