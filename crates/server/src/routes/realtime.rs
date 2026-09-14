use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use futures_util::stream::SplitSink;
use futures_util::{Sink, SinkExt, StreamExt};
use protocol::{
    LibraryEvent, PageClientMessage, PageServerMessage, Paper, RealtimeTicketResponse, Stroke,
    StrokeBatch, TombstoneBatch,
};
use rand::Rng;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::AppState;
use crate::auth::{Claims, validate_jwt};

const TICKET_TTL_SECONDS: i64 = 60;
const LIBRARY_CHANNEL_CAPACITY: usize = 64;
const PAGE_CHANNEL_CAPACITY: usize = 256;
const LEASE_TTL_SECONDS: i64 = 30;

#[derive(Default)]
pub struct RealtimeHub {
    tickets: Mutex<HashMap<String, Ticket>>,
    library_channels: Mutex<HashMap<String, broadcast::Sender<LibraryEvent>>>,
    page_channels: Mutex<HashMap<String, broadcast::Sender<PageServerMessage>>>,
    leases: Mutex<HashMap<String, Lease>>,
}

#[derive(Clone)]
struct Ticket {
    owner_id: String,
    expires_at: DateTime<Utc>,
}

struct Lease {
    holder: String,
    expires_at: DateTime<Utc>,
}

enum LeaseOutcome {
    Granted,
    Denied { holder: String },
}

impl RealtimeHub {
    pub fn issue_ticket(&self, owner_id: String) -> RealtimeTicketResponse {
        let ticket = format!("ticket_{}", random_hex(32));
        let expires_at = Utc::now() + Duration::seconds(TICKET_TTL_SECONDS);
        self.tickets.lock().expect("ticket mutex poisoned").insert(
            ticket.clone(),
            Ticket {
                owner_id,
                expires_at,
            },
        );
        RealtimeTicketResponse {
            ticket,
            expires_at: expires_at.to_rfc3339(),
        }
    }

    fn consume_ticket(&self, ticket: &str) -> Option<String> {
        let now = Utc::now();
        let mut tickets = self.tickets.lock().expect("ticket mutex poisoned");
        tickets.retain(|_, value| value.expires_at > now);
        let ticket = tickets.remove(ticket)?;
        (ticket.expires_at > now).then_some(ticket.owner_id)
    }

    fn subscribe_library(&self, owner_id: &str) -> broadcast::Receiver<LibraryEvent> {
        let mut channels = self
            .library_channels
            .lock()
            .expect("library channel mutex poisoned");
        channels
            .entry(owner_id.to_string())
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(LIBRARY_CHANNEL_CAPACITY);
                sender
            })
            .subscribe()
    }

    pub fn publish_library_event(&self, owner_id: &str, event: LibraryEvent) {
        let sender = {
            let mut channels = self
                .library_channels
                .lock()
                .expect("library channel mutex poisoned");
            channels
                .entry(owner_id.to_string())
                .or_insert_with(|| {
                    let (sender, _) = broadcast::channel(LIBRARY_CHANNEL_CAPACITY);
                    sender
                })
                .clone()
        };
        let _ = sender.send(event);
    }

    fn subscribe_page(&self, page_id: &str) -> broadcast::Receiver<PageServerMessage> {
        let mut channels = self
            .page_channels
            .lock()
            .expect("page channel mutex poisoned");
        channels
            .entry(page_id.to_string())
            .or_insert_with(|| {
                let (sender, _) = broadcast::channel(PAGE_CHANNEL_CAPACITY);
                sender
            })
            .subscribe()
    }

    fn publish_page(&self, page_id: &str, message: PageServerMessage) {
        let sender = {
            let mut channels = self
                .page_channels
                .lock()
                .expect("page channel mutex poisoned");
            channels
                .entry(page_id.to_string())
                .or_insert_with(|| {
                    let (sender, _) = broadcast::channel(PAGE_CHANNEL_CAPACITY);
                    sender
                })
                .clone()
        };
        let _ = sender.send(message);
    }

    /// Acquire (or renew, for the current holder) the single-editor edit lease.
    fn acquire_lease(&self, page_id: &str, session_id: &str) -> LeaseOutcome {
        let now = Utc::now();
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        if let Some(existing) = leases.get(page_id)
            && existing.expires_at <= now
        {
            leases.remove(page_id);
        }
        match leases.get_mut(page_id) {
            Some(existing) if existing.holder == session_id => {
                existing.expires_at = now + Duration::seconds(LEASE_TTL_SECONDS);
                LeaseOutcome::Granted
            }
            Some(existing) => LeaseOutcome::Denied {
                holder: existing.holder.clone(),
            },
            None => {
                leases.insert(
                    page_id.to_string(),
                    Lease {
                        holder: session_id.to_string(),
                        expires_at: now + Duration::seconds(LEASE_TTL_SECONDS),
                    },
                );
                LeaseOutcome::Granted
            }
        }
    }

    /// Release the lease if held by `session_id`; returns true when released.
    fn release_lease(&self, page_id: &str, session_id: &str) -> bool {
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        match leases.get(page_id) {
            Some(existing) if existing.holder == session_id => {
                leases.remove(page_id);
                true
            }
            _ => false,
        }
    }

    fn current_lease_holder(&self, page_id: &str) -> Option<String> {
        let now = Utc::now();
        let mut leases = self.leases.lock().expect("lease mutex poisoned");
        match leases.get(page_id) {
            Some(existing) if existing.expires_at > now => Some(existing.holder.clone()),
            Some(_) => {
                leases.remove(page_id);
                None
            }
            None => None,
        }
    }
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

    ws.on_upgrade(move |socket| async move {
        handle_library_socket(state, owner_id, socket).await;
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
                let event = match event {
                    Ok(event) => event,
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
                if sender.send(Message::Text(payload.into())).await.is_err() {
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

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()).then(|| token.to_string())
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
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

    let Some(pool) = state.db.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "database not configured").into_response();
    };

    match page_belongs_to_owner(&pool, &page_id, &owner_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::FORBIDDEN, "page not found").into_response(),
        Err(error) => {
            tracing::error!(error = %error, "page ownership check failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "ownership check failed").into_response();
        }
    }

    ws.on_upgrade(move |socket| async move {
        handle_page_socket(state, pool, page_id, socket).await;
    })
}

#[tracing::instrument(skip_all, fields(page_id = %page_id, session_id))]
async fn handle_page_socket(state: AppState, pool: PgPool, page_id: String, socket: WebSocket) {
    let _connection_guard = crate::observability::metrics().realtime_connection_guard();
    crate::observability::metrics().record_realtime_event("page", "connected");
    let session_id = format!("session_{}", random_hex(16));
    tracing::Span::current().record("session_id", session_id.as_str());
    let mut receiver = state.realtime.subscribe_page(&page_id);
    let (mut sender, mut inbound) = socket.split();

    let last_seq = max_seq(&pool, &page_id).await.unwrap_or(0);
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
    let paper = match current_paper(&pool, &page_id).await {
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
                    Ok(message) => {
                        if !send_page(&mut sender, message).await {
                            crate::observability::metrics().record_realtime_event("page", "send_error");
                            break;
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
                            &state, &pool, &page_id, &session_id, &mut sender, &text,
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
    pool: &PgPool,
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

    let keep_open =
        dispatch_page_client_message(state, pool, page_id, session_id, sender, message, received)
            .await;
    // Server-side handling only — parse to the last frame this handler sends.
    // Not end-to-end freshness, which needs client timestamps (#154).
    crate::observability::metrics()
        .observe_realtime_message_handling(message_type, received.elapsed().as_secs_f64());
    keep_open
}

async fn dispatch_page_client_message(
    state: &AppState,
    pool: &PgPool,
    page_id: &str,
    session_id: &str,
    sender: &mut SplitSink<WebSocket, Message>,
    message: PageClientMessage,
    received: Instant,
) -> bool {
    match message {
        PageClientMessage::Subscribe { from_seq } => {
            replay_page(pool, page_id, sender, from_seq, received).await
        }
        PageClientMessage::AcquireLease => {
            match state.realtime.acquire_lease(page_id, session_id) {
                LeaseOutcome::Granted => {
                    state.realtime.publish_page(
                        page_id,
                        PageServerMessage::LeaseChanged {
                            holder: Some(session_id.to_string()),
                        },
                    );
                    send_page(sender, PageServerMessage::LeaseGranted).await
                }
                LeaseOutcome::Denied { holder } => {
                    crate::observability::metrics().record_realtime_event("page", "lease_denied");
                    send_page(sender, PageServerMessage::LeaseDenied { holder }).await
                }
            }
        }
        PageClientMessage::RenewLease => match state.realtime.acquire_lease(page_id, session_id) {
            LeaseOutcome::Granted => send_page(sender, PageServerMessage::LeaseGranted).await,
            LeaseOutcome::Denied { holder } => {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                send_page(sender, PageServerMessage::LeaseDenied { holder }).await
            }
        },
        PageClientMessage::ReleaseLease => {
            if state.realtime.release_lease(page_id, session_id) {
                state
                    .realtime
                    .publish_page(page_id, PageServerMessage::LeaseChanged { holder: None });
            }
            true
        }
        PageClientMessage::CommitBatch {
            client_batch_id,
            strokes,
        } => {
            // Single active editor: only the lease holder may ink. Acquiring
            // also renews the holder's lease on each commit.
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            if strokes
                .iter()
                .any(|stroke| stroke.id.is_empty() || stroke.validate().is_err())
            {
                return send_page(
                    sender,
                    PageServerMessage::Error {
                        code: "invalid_stroke_style".to_string(),
                        message: "strokes require a supported immutable solid_round style \
                                  with in-range pressure"
                            .to_string(),
                        client_mutation_id: None,
                    },
                )
                .await;
            }
            match persist_batch(pool, page_id, &client_batch_id, &strokes).await {
                Ok(persisted) => {
                    let seq = persisted.seq;
                    crate::observability::metrics().record_page_mutation("commit_batch", "success");
                    // Delete-wins: broadcast only strokes that survived tombstone
                    // filtering. An add fully suppressed by tombstones changes no
                    // visible state, so it is acknowledged without a fan-out.
                    if !persisted.visible_strokes.is_empty() {
                        state.realtime.publish_page(
                            page_id,
                            PageServerMessage::StrokeBatch(StrokeBatch {
                                seq,
                                client_batch_id,
                                strokes: persisted.visible_strokes,
                            }),
                        );
                    }
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        let page_id = page_id.to_string();
                        crate::observability::metrics().thumbnail_generation_queued();
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.clone(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id,
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "stroke commit failed");
                    crate::observability::metrics().record_page_mutation("commit_batch", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "commit_failed".to_string(),
                            message: "could not persist strokes".to_string(),
                            client_mutation_id: None,
                        },
                    )
                    .await
                }
            }
        }
        PageClientMessage::CommitTombstones {
            client_mutation_id,
            stroke_ids,
        } => {
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            match persist_tombstones(pool, page_id, &client_mutation_id, &stroke_ids).await {
                Ok(persisted) => {
                    let event = TombstoneBatch {
                        revision: persisted.revision,
                        client_mutation_id,
                        stroke_ids: persisted.stroke_ids,
                    };
                    state
                        .realtime
                        .publish_page(page_id, PageServerMessage::TombstoneBatch(event));
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.to_string(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id.to_string(),
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    crate::observability::metrics()
                        .record_page_mutation("commit_tombstones", "success");
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "tombstone commit failed");
                    crate::observability::metrics()
                        .record_page_mutation("commit_tombstones", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "tombstone_failed".to_string(),
                            message: "could not persist deleted strokes".to_string(),
                            client_mutation_id: Some(client_mutation_id),
                        },
                    )
                    .await
                }
            }
        }
        PageClientMessage::SetPaper {
            client_mutation_id,
            paper,
        } => {
            // Paper is a visible page mutation that bumps the revision, mints a
            // thumbnail and re-sorts the library — exactly the class the
            // single-editor invariant governs. Acquiring also renews the holder's
            // lease, identically to CommitBatch/CommitTombstones.
            if let LeaseOutcome::Denied { holder } =
                state.realtime.acquire_lease(page_id, session_id)
            {
                crate::observability::metrics().record_realtime_event("page", "lease_denied");
                return send_page(sender, PageServerMessage::LeaseDenied { holder }).await;
            }
            match persist_paper(pool, page_id, paper).await {
                Ok(persisted) => {
                    if !persisted.changed {
                        // Value-idempotent: nothing bumped, nothing fanned out.
                        // Ack directly so a racing client still converges.
                        crate::observability::metrics().record_page_mutation("set_paper", "noop");
                        return send_page(
                            sender,
                            PageServerMessage::PaperChanged {
                                paper,
                                revision: persisted.revision,
                            },
                        )
                        .await;
                    }
                    // The broadcast reaches the sender too, as with StrokeBatch.
                    state.realtime.publish_page(
                        page_id,
                        PageServerMessage::PaperChanged {
                            paper,
                            revision: persisted.revision,
                        },
                    );
                    if let Some(updated_at) = persisted.updated_at {
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageUpdated {
                                page_id: page_id.to_string(),
                                updated_at,
                            },
                        );
                    }
                    if persisted.thumbnail_job_created {
                        crate::observability::metrics().thumbnail_generation_queued();
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.to_string(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: persisted.revision,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(
                            state.clone(),
                            page_id.to_string(),
                            persisted.owner_id,
                            persisted.revision,
                        );
                    }
                    crate::observability::metrics().record_page_mutation("set_paper", "success");
                    true
                }
                Err(error) => {
                    tracing::error!(error = %error, "paper change failed");
                    crate::observability::metrics().record_page_mutation("set_paper", "error");
                    send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "paper_failed".to_string(),
                            message: "could not persist the page paper".to_string(),
                            client_mutation_id: Some(client_mutation_id),
                        },
                    )
                    .await
                }
            }
        }
    }
}

/// What one replay put on the wire.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReplayCost {
    frames: u64,
    bytes: u64,
}

impl ReplayCost {
    fn add_frame(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
    }
}

/// Replays a page to one subscriber: surviving stroke batches, every tombstone
/// batch, then `synced`. Its frames, bytes and duration are recorded once
/// `synced` is sent; a replay cut short by a failed read or send records none.
#[tracing::instrument(skip_all, fields(from_seq = from_seq, frames, bytes))]
async fn replay_page(
    pool: &PgPool,
    page_id: &str,
    sender: &mut SplitSink<WebSocket, Message>,
    from_seq: u64,
    received: Instant,
) -> bool {
    let (batches, tombstones) = match load_page_replay(pool, page_id, from_seq).await {
        Ok(replay) => replay,
        Err(error) => {
            tracing::error!(error = %error, "stroke replay failed");
            crate::observability::metrics().record_realtime_event("page", "replay_error");
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "replay_failed".to_string(),
                    message: "could not load page ink".to_string(),
                    client_mutation_id: None,
                },
            )
            .await;
        }
    };
    let mut cost = ReplayCost::default();
    if !send_replay_frames(sender, batches, tombstones, &mut cost).await {
        return false;
    }
    let last_seq = max_seq(pool, page_id).await.unwrap_or(from_seq);
    let Some(synced_bytes) = send_page_frame(sender, PageServerMessage::Synced { last_seq }).await
    else {
        return false;
    };
    cost.add_frame(synced_bytes);

    let span = tracing::Span::current();
    span.record("frames", cost.frames);
    span.record("bytes", cost.bytes);
    crate::observability::metrics().observe_realtime_replay(
        cost.frames,
        cost.bytes,
        received.elapsed().as_secs_f64(),
    );
    true
}

/// Sends a replay's stroke-batch and tombstone-batch frames, adding each to
/// `cost`. Returns false when a send fails.
async fn send_replay_frames<S>(
    sender: &mut S,
    batches: Vec<StrokeBatch>,
    tombstones: Vec<TombstoneBatch>,
    cost: &mut ReplayCost,
) -> bool
where
    S: Sink<Message> + Unpin,
{
    let deleted_ids: HashSet<&str> = tombstones
        .iter()
        .flat_map(|batch| batch.stroke_ids.iter().map(String::as_str))
        .collect();
    for mut batch in batches {
        batch
            .strokes
            .retain(|stroke| !deleted_ids.contains(stroke.id.as_str()));
        let Some(bytes) = send_page_frame(sender, PageServerMessage::StrokeBatch(batch)).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    }
    // Replaying tombstones is required even when `from_seq` skips their
    // source stroke batches: a reconnecting client may still cache them.
    for batch in tombstones {
        let Some(bytes) = send_page_frame(sender, PageServerMessage::TombstoneBatch(batch)).await
        else {
            return false;
        };
        cost.add_frame(bytes);
    }
    true
}

async fn send_page<S>(sender: &mut S, message: PageServerMessage) -> bool
where
    S: Sink<Message> + Unpin,
{
    send_page_frame(sender, message).await.is_some()
}

/// Serializes and sends one page-channel frame, returning its size in bytes, or
/// `None` when the socket should close. Every server→client page frame passes
/// through here, so this is where frame size is recorded — after the send, so
/// the histogram counts only frames that reached the socket.
async fn send_page_frame<S>(sender: &mut S, message: PageServerMessage) -> Option<usize>
where
    S: Sink<Message> + Unpin,
{
    let message_type = message.message_type();
    let payload = serde_json::to_string(&message).ok()?;
    let bytes = payload.len();
    sender.send(Message::Text(payload.into())).await.ok()?;
    crate::observability::metrics().observe_realtime_message_bytes("page", message_type, bytes);
    Some(bytes)
}

async fn page_belongs_to_owner(
    pool: &PgPool,
    page_id: &str,
    owner_id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT 1 FROM pages WHERE id = $1 AND owner_id = $2")
        .bind(page_id)
        .bind(owner_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// The page's current paper. An unrecognised stored value (only reachable if the
/// `pages_paper_known` CHECK were dropped) degrades to a blank page rather than
/// failing the connection.
async fn current_paper(pool: &PgPool, page_id: &str) -> Result<Paper, sqlx::Error> {
    let stored: String = sqlx::query_scalar("SELECT paper FROM pages WHERE id = $1")
        .bind(page_id)
        .fetch_one(pool)
        .await?;
    Ok(Paper::from_wire(&stored).unwrap_or_default())
}

async fn max_seq(pool: &PgPool, page_id: &str) -> Result<u64, sqlx::Error> {
    let seq: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1")
            .bind(page_id)
            .fetch_one(pool)
            .await?;
    Ok(seq as u64)
}

#[derive(sqlx::FromRow)]
struct StrokeBatchRow {
    seq: i64,
    client_batch_id: String,
    strokes: sqlx::types::Json<Vec<Stroke>>,
}

async fn load_batches_after(
    pool: &PgPool,
    page_id: &str,
    from_seq: u64,
) -> Result<Vec<StrokeBatch>, sqlx::Error> {
    let rows = sqlx::query_as::<_, StrokeBatchRow>(
        r#"
        SELECT seq, client_batch_id, strokes
        FROM stroke_batches
        WHERE page_id = $1 AND seq > $2
        ORDER BY seq
        "#,
    )
    .bind(page_id)
    .bind(from_seq as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| StrokeBatch {
            seq: row.seq as u64,
            client_batch_id: row.client_batch_id,
            strokes: row.strokes.0,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct TombstoneBatchRow {
    revision: i64,
    client_mutation_id: String,
    stroke_ids: sqlx::types::Json<Vec<String>>,
}

async fn load_page_replay(
    pool: &PgPool,
    page_id: &str,
    from_seq: u64,
) -> Result<(Vec<StrokeBatch>, Vec<TombstoneBatch>), sqlx::Error> {
    let batches = load_batches_after(pool, page_id, from_seq).await?;
    let tombstones = sqlx::query_as::<_, TombstoneBatchRow>(
        r#"
        SELECT revision, client_mutation_id, stroke_ids
        FROM tombstone_batches
        WHERE page_id = $1
        ORDER BY revision, client_mutation_id
        "#,
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| TombstoneBatch {
        revision: row.revision as u64,
        client_mutation_id: row.client_mutation_id,
        stroke_ids: row.stroke_ids.0,
    })
    .collect();
    Ok((batches, tombstones))
}

/// Persist a stroke batch with a per-page monotonic sequence. Idempotent by
/// `client_batch_id` so reconnect retries return the existing seq instead of
/// double-inserting. New batches atomically bump the page `updated_at` and
/// persist their thumbnail job before the commit is acknowledged.
struct PersistedBatch {
    seq: u64,
    revision: u64,
    owner_id: String,
    /// Strokes that survive delete-wins filtering — the only ones to broadcast
    /// and store. Empty means the whole add was suppressed by tombstones and no
    /// visible state changed.
    visible_strokes: Vec<Stroke>,
    thumbnail_job_created: bool,
    /// The new `updated_at` (RFC 3339) when this commit actually bumped the row,
    /// so the library can re-sort. `None` for idempotent retries and fully
    /// suppressed adds, which change no last-edited timestamp.
    updated_at: Option<String>,
}

async fn persist_batch(
    pool: &PgPool,
    page_id: &str,
    client_batch_id: &str,
    strokes: &[Stroke],
) -> Result<PersistedBatch, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Serialize seq allocation for this page against concurrent commits. `paper`
    // is read under the same lock so the thumbnail job records the paper in force
    // at the revision it will rasterize.
    let (owner_id, current_revision, paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;

    // Delete-wins: a permanent tombstone for a stroke id suppresses every later
    // add of that id. Filtering before insert stops an add-after-delete (or a
    // stale offline replay) from advancing the revision or being broadcast as
    // visible ink, while identical retries stay idempotent by `client_batch_id`.
    let submitted_ids: Vec<String> = strokes.iter().map(|stroke| stroke.id.clone()).collect();
    let tombstoned: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND stroke_id = ANY($2)",
    )
    .bind(page_id)
    .bind(&submitted_ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let visible_strokes: Vec<Stroke> = strokes
        .iter()
        .filter(|stroke| !tombstoned.contains(&stroke.id))
        .cloned()
        .collect();

    let existing = sqlx::query_as::<_, (i64, i64)>(
        "SELECT seq, revision FROM stroke_batches WHERE page_id = $1 AND client_batch_id = $2",
    )
    .bind(page_id)
    .bind(client_batch_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((seq, revision)) = existing {
        // Idempotent retry: re-echo only the strokes still visible today.
        tx.commit().await?;
        return Ok(PersistedBatch {
            seq: seq as u64,
            revision: revision as u64,
            owner_id,
            visible_strokes,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    if visible_strokes.is_empty() {
        // Every submitted stroke is tombstoned — acknowledge the add as a no-op
        // without inserting a batch, advancing the revision, or broadcasting.
        let head_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1",
        )
        .bind(page_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(PersistedBatch {
            seq: head_seq as u64,
            revision: current_revision as u64,
            owner_id,
            visible_strokes,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM stroke_batches WHERE page_id = $1",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;
    let revision = current_revision + 1;

    sqlx::query(
        "INSERT INTO stroke_batches (page_id, seq, revision, client_batch_id, strokes) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(page_id)
    .bind(next)
    .bind(revision)
    .bind(client_batch_id)
    .bind(sqlx::types::Json(&visible_strokes))
    .execute(&mut *tx)
    .await?;

    let updated_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "UPDATE pages SET updated_at = now(), ink_revision = $2 WHERE id = $1 RETURNING updated_at",
    )
    .bind(page_id)
    .bind(revision)
    .fetch_one(&mut *tx)
    .await?
    .to_rfc3339();
    let thumbnail_job_created = sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING",
    )
    .bind(page_id)
    .bind(revision)
    .bind(&paper)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;

    tx.commit().await?;
    Ok(PersistedBatch {
        seq: next as u64,
        revision: revision as u64,
        owner_id,
        visible_strokes,
        thumbnail_job_created,
        updated_at: Some(updated_at),
    })
}

struct PersistedTombstones {
    revision: u64,
    owner_id: String,
    stroke_ids: Vec<String>,
    thumbnail_job_created: bool,
    /// See `PersistedBatch::updated_at` — `None` when the erase changed no
    /// strokes (idempotent replay or all-already-tombstoned).
    updated_at: Option<String>,
}

async fn persist_tombstones(
    pool: &PgPool,
    page_id: &str,
    client_mutation_id: &str,
    requested_ids: &[String],
) -> Result<PersistedTombstones, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (owner_id, current_revision, paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some((revision, ids)) = sqlx::query_as::<_, (i64, sqlx::types::Json<Vec<String>>)>(
        "SELECT revision, stroke_ids FROM tombstone_batches WHERE page_id = $1 AND client_mutation_id = $2",
    )
    .bind(page_id)
    .bind(client_mutation_id)
    .fetch_optional(&mut *tx)
    .await? {
        tx.commit().await?;
        return Ok(PersistedTombstones { revision: revision as u64, owner_id, stroke_ids: ids.0, thumbnail_job_created: false, updated_at: None });
    }

    let mut ids = requested_ids.to_vec();
    ids.sort();
    ids.dedup();
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND stroke_id = ANY($2)",
    )
    .bind(page_id)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    ids.retain(|id| !existing.contains(id));
    let revision = if ids.is_empty() {
        current_revision
    } else {
        current_revision + 1
    };
    for id in &ids {
        sqlx::query("INSERT INTO stroke_tombstones (page_id, stroke_id, deleted_revision) VALUES ($1, $2, $3)")
            .bind(page_id).bind(id).bind(revision).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO tombstone_batches (page_id, client_mutation_id, revision, stroke_ids) VALUES ($1, $2, $3, $4)")
        .bind(page_id).bind(client_mutation_id).bind(revision).bind(sqlx::types::Json(&ids)).execute(&mut *tx).await?;
    let updated_at = if !ids.is_empty() {
        Some(
            sqlx::query_scalar::<_, DateTime<Utc>>(
                "UPDATE pages SET updated_at = now(), ink_revision = ink_revision + 1 WHERE id = $1 RETURNING updated_at",
            )
            .bind(page_id)
            .fetch_one(&mut *tx)
            .await?
            .to_rfc3339(),
        )
    } else {
        None
    };
    let thumbnail_job_created = if ids.is_empty() {
        false
    } else {
        sqlx::query("INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING")
            .bind(page_id).bind(revision).bind(&paper).execute(&mut *tx).await?.rows_affected() == 1
    };
    tx.commit().await?;
    Ok(PersistedTombstones {
        revision: revision as u64,
        owner_id,
        stroke_ids: ids,
        thumbnail_job_created,
        updated_at,
    })
}

/// Outcome of a paper change.
///
/// Note this widens `pages.ink_revision` from "ink mutations" to "anything that
/// changes how the page renders". That is safe: gap-fill and `Welcome.last_seq`
/// use `stroke_batches.seq`, a *separate* sequence, and `thumbnails::generate`
/// filters revisions with range predicates, so gaps in `revision` are harmless.
/// The column is deliberately **not** renamed — it is read in three query sites.
struct PersistedPaper {
    /// False when the page already had this paper: nothing bumped, nothing
    /// stored, nothing to fan out.
    changed: bool,
    /// The revision the paper is in force at — the new one when the page had
    /// visible ink, otherwise the unchanged current one.
    revision: u64,
    owner_id: String,
    thumbnail_job_created: bool,
    /// See `PersistedBatch::updated_at` — `None` for a same-value no-op.
    updated_at: Option<String>,
}

async fn persist_paper(
    pool: &PgPool,
    page_id: &str,
    paper: Paper,
) -> Result<PersistedPaper, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (owner_id, current_revision, current_paper) = sqlx::query_as::<_, (String, i64, String)>(
        "SELECT owner_id, ink_revision, paper FROM pages WHERE id = $1 FOR UPDATE",
    )
    .bind(page_id)
    .fetch_one(&mut *tx)
    .await?;

    if current_paper == paper.wire_value() {
        tx.commit().await?;
        return Ok(PersistedPaper {
            changed: false,
            revision: current_revision as u64,
            owner_id,
            thumbnail_job_created: false,
            updated_at: None,
        });
    }

    // Does the page have any ink still *visible* at this revision? Erased ink
    // does not count — an all-erased page is as inkless as a never-inked one.
    let has_visible_ink: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (
             SELECT 1
             FROM stroke_batches b
             CROSS JOIN LATERAL jsonb_array_elements(b.strokes) AS s(stroke)
             WHERE b.page_id = $1
               AND b.revision <= $2
               AND NOT EXISTS (
                 SELECT 1 FROM stroke_tombstones t
                 WHERE t.page_id = b.page_id
                   AND t.stroke_id = s.stroke ->> 'id'
                   AND t.deleted_revision <= $2
               )
           )"#,
    )
    .bind(page_id)
    .bind(current_revision)
    .fetch_one(&mut *tx)
    .await?;

    // With visible ink the existing thumbnail is now stale, so bump the revision
    // to mint a fresh immutable artifact (new source_seq → new URL) and let the
    // existing PageThumbnailUpdated fan-out and `source_seq >=` freshness guards
    // do the rest.
    //
    // With no visible ink there is nothing to invalidate — and bumping would
    // break an invariant `thumbnails::cleanup` depends on: it protects the head
    // artifact with `source_seq <> (SELECT ink_revision …)`. Today ink_revision
    // always names an existing thumbnail row; a bump with no job created would
    // leave it naming nothing, making *every* surviving thumbnail of that page
    // retention-eligible and silently dropping the library preview after 7 days.
    let revision = if has_visible_ink {
        current_revision + 1
    } else {
        current_revision
    };

    let updated_at = sqlx::query_scalar::<_, DateTime<Utc>>(
        "UPDATE pages SET paper = $2, updated_at = now(), ink_revision = $3 WHERE id = $1 RETURNING updated_at",
    )
    .bind(page_id)
    .bind(paper.wire_value())
    .bind(revision)
    .fetch_one(&mut *tx)
    .await?
    .to_rfc3339();

    let thumbnail_job_created = if has_visible_ink {
        sqlx::query(
            "INSERT INTO page_thumbnails (page_id, source_seq, status, paper) VALUES ($1, $2, 'generating', $3) ON CONFLICT (page_id, source_seq) DO NOTHING",
        )
        .bind(page_id)
        .bind(revision)
        .bind(paper.wire_value())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1
    } else {
        false
    };

    tx.commit().await?;
    Ok(PersistedPaper {
        changed: true,
        revision: revision as u64,
        owner_id,
        thumbnail_job_created,
        // The library re-sorts on a paper change even for an inkless page: the
        // page really was last edited now.
        updated_at: Some(updated_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_lease_grants_then_blocks_second_session() {
        let hub = RealtimeHub::default();

        // First session acquires; the holder may renew freely.
        assert!(matches!(
            hub.acquire_lease("page_1", "session_a"),
            LeaseOutcome::Granted
        ));
        assert!(matches!(
            hub.acquire_lease("page_1", "session_a"),
            LeaseOutcome::Granted
        ));

        // A second session is blocked and told the current holder.
        match hub.acquire_lease("page_1", "session_b") {
            LeaseOutcome::Denied { holder } => assert_eq!(holder, "session_a"),
            LeaseOutcome::Granted => panic!("second session should be blocked"),
        }
        assert_eq!(
            hub.current_lease_holder("page_1").as_deref(),
            Some("session_a")
        );

        // A non-holder cannot release the lease.
        assert!(!hub.release_lease("page_1", "session_b"));

        // The holder releases and the page frees up for the next session.
        assert!(hub.release_lease("page_1", "session_a"));
        assert_eq!(hub.current_lease_holder("page_1"), None);
        assert!(matches!(
            hub.acquire_lease("page_1", "session_b"),
            LeaseOutcome::Granted
        ));
    }

    fn fixture_stroke(id: &str) -> Stroke {
        let mut stroke: Stroke =
            serde_json::from_str(include_str!("../../../../contracts/fixtures/stroke.json"))
                .expect("stroke fixture should parse");
        stroke.id = id.to_string();
        stroke
    }

    fn text_frames(sink: &[Message]) -> Vec<&str> {
        sink.iter()
            .map(|message| match message {
                Message::Text(text) => text.as_str(),
                other => panic!("expected a text frame, got {other:?}"),
            })
            .collect()
    }

    // `send_page_frame` is generic over the sink precisely so the chokepoint can
    // be exercised without a socket: a `Vec<Message>` is a sink that never fails.
    #[tokio::test]
    async fn send_page_frame_records_the_bytes_it_sent() {
        let metrics = crate::observability::metrics();
        let before = metrics.realtime_message_count("page", "lease-granted");
        let mut sink: Vec<Message> = Vec::new();

        let bytes = send_page_frame(&mut sink, PageServerMessage::LeaseGranted)
            .await
            .expect("a Vec sink never fails");

        let frames = text_frames(&sink);
        assert_eq!(frames, [r#"{"type":"lease-granted"}"#]);
        assert_eq!(bytes, frames[0].len());
        // No other test in this binary sends `lease-granted`, so the delta on the
        // process-wide histogram is exact.
        assert_eq!(
            metrics.realtime_message_count("page", "lease-granted"),
            before + 1
        );
    }

    #[tokio::test]
    async fn replay_frames_apply_delete_wins_and_count_every_frame() {
        let batches = vec![
            StrokeBatch {
                seq: 1,
                client_batch_id: "batch_1".to_string(),
                strokes: vec![fixture_stroke("kept"), fixture_stroke("erased")],
            },
            StrokeBatch {
                seq: 2,
                client_batch_id: "batch_2".to_string(),
                strokes: vec![fixture_stroke("later")],
            },
        ];
        let tombstones = vec![TombstoneBatch {
            revision: 3,
            client_mutation_id: "erase_1".to_string(),
            stroke_ids: vec!["erased".to_string()],
        }];
        let mut sink: Vec<Message> = Vec::new();
        let mut cost = ReplayCost::default();

        assert!(send_replay_frames(&mut sink, batches, tombstones, &mut cost).await);

        let frames = text_frames(&sink);
        let types: Vec<String> = frames
            .iter()
            .map(|frame| {
                serde_json::from_str::<serde_json::Value>(frame).expect("frame should be JSON")
                    ["type"]
                    .as_str()
                    .expect("frame should carry a type")
                    .to_string()
            })
            .collect();
        // One frame per stored batch plus one per tombstone batch — the shape
        // #323 collapses into a single frame.
        assert_eq!(types, ["stroke-batch", "stroke-batch", "tombstone-batch"]);
        assert!(frames[0].contains(r#""id":"kept""#));
        assert!(
            !frames[0].contains(r#""id":"erased""#),
            "delete-wins filtering must still drop tombstoned strokes"
        );
        assert_eq!(
            cost,
            ReplayCost {
                frames: 3,
                bytes: frames.iter().map(|frame| frame.len() as u64).sum(),
            }
        );
    }
}
