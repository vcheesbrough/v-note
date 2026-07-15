use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, Duration, Utc};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use protocol::{
    LibraryEvent, PageClientMessage, PageServerMessage, RealtimeTicketResponse, Stroke, StrokeBatch,
};
use rand::RngCore;
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::auth::{validate_jwt, Claims};
use crate::AppState;

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
        if let Some(existing) = leases.get(page_id) {
            if existing.expires_at <= now {
                leases.remove(page_id);
            }
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

async fn handle_library_socket(state: AppState, owner_id: String, socket: WebSocket) {
    let _connection_guard = crate::observability::metrics().realtime_connection_guard();
    crate::observability::metrics().record_realtime_event("library", "connected");
    let mut receiver = state.realtime.subscribe_library(&owner_id);
    let (mut sender, mut inbound) = socket.split();

    loop {
        tokio::select! {
            event = receiver.recv() => {
                let Ok(event) = event else {
                    break;
                };
                let Ok(payload) = serde_json::to_string(&event) else {
                    continue;
                };
                if sender.send(Message::Text(payload.into())).await.is_err() {
                    crate::observability::metrics().record_realtime_event("library", "send_error");
                    break;
                }
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
    rand::thread_rng().fill_bytes(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---- Page channel (per-`page_id` ink WSS) --------------------------------

/// Per-page ink socket. Authenticates like the library socket (Bearer for
/// Android, realtime ticket for SPA), then enforces owner-only access to the
/// page before upgrading. Bidirectional: gap-fill/snapshot, edit lease, and
/// coalesced stroke-batch commits fanned out to the owner's sibling sessions.
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

async fn handle_page_socket(state: AppState, pool: PgPool, page_id: String, socket: WebSocket) {
    let _connection_guard = crate::observability::metrics().realtime_connection_guard();
    crate::observability::metrics().record_realtime_event("page", "connected");
    let session_id = format!("session_{}", random_hex(16));
    let mut receiver = state.realtime.subscribe_page(&page_id);
    let (mut sender, mut inbound) = socket.split();

    let last_seq = max_seq(&pool, &page_id).await.unwrap_or(0);
    let lease_holder = state.realtime.current_lease_holder(&page_id);
    let welcome = PageServerMessage::Welcome {
        session_id: session_id.clone(),
        last_seq,
        lease_holder,
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
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

/// Returns false when the socket should close (send failure).
async fn handle_page_client_message(
    state: &AppState,
    pool: &PgPool,
    page_id: &str,
    session_id: &str,
    sender: &mut SplitSink<WebSocket, Message>,
    text: &str,
) -> bool {
    let message: PageClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(_) => {
            return send_page(
                sender,
                PageServerMessage::Error {
                    code: "bad_message".to_string(),
                    message: "could not parse client message".to_string(),
                },
            )
            .await;
        }
    };

    match message {
        PageClientMessage::Subscribe { from_seq } => {
            let batches = match load_batches_after(pool, page_id, from_seq).await {
                Ok(batches) => batches,
                Err(error) => {
                    tracing::error!(error = %error, "stroke replay failed");
                    crate::observability::metrics().record_realtime_event("page", "replay_error");
                    return send_page(
                        sender,
                        PageServerMessage::Error {
                            code: "replay_failed".to_string(),
                            message: "could not load page ink".to_string(),
                        },
                    )
                    .await;
                }
            };
            for batch in batches {
                if !send_page(sender, PageServerMessage::StrokeBatch(batch)).await {
                    return false;
                }
            }
            let last_seq = max_seq(pool, page_id).await.unwrap_or(from_seq);
            send_page(sender, PageServerMessage::Synced { last_seq }).await
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
            match persist_batch(pool, page_id, &client_batch_id, &strokes).await {
                Ok(persisted) => {
                    let seq = persisted.seq;
                    crate::observability::metrics().record_page_mutation("commit_batch", "success");
                    state.realtime.publish_page(
                        page_id,
                        PageServerMessage::StrokeBatch(StrokeBatch {
                            seq,
                            client_batch_id,
                            strokes,
                        }),
                    );
                    if persisted.thumbnail_job_created {
                        let page_id = page_id.to_string();
                        crate::observability::metrics().thumbnail_generation_queued();
                        state.realtime.publish_library_event(
                            &persisted.owner_id,
                            LibraryEvent::PageThumbnailUpdated {
                                page_id: page_id.clone(),
                                thumbnail: protocol::ThumbnailMetadata::Generating {
                                    source_seq: seq,
                                },
                            },
                        );
                        crate::thumbnails::enqueue(state.clone(), page_id, persisted.owner_id, seq);
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
                        },
                    )
                    .await
                }
            }
        }
    }
}

async fn send_page(sender: &mut SplitSink<WebSocket, Message>, message: PageServerMessage) -> bool {
    let Ok(payload) = serde_json::to_string(&message) else {
        return false;
    };
    sender.send(Message::Text(payload.into())).await.is_ok()
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

/// Persist a stroke batch with a per-page monotonic sequence. Idempotent by
/// `client_batch_id` so reconnect retries return the existing seq instead of
/// double-inserting. New batches atomically bump the page `updated_at` and
/// persist their thumbnail job before the commit is acknowledged.
struct PersistedBatch {
    seq: u64,
    owner_id: String,
    thumbnail_job_created: bool,
}

async fn persist_batch(
    pool: &PgPool,
    page_id: &str,
    client_batch_id: &str,
    strokes: &[Stroke],
) -> Result<PersistedBatch, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Serialize seq allocation for this page against concurrent commits.
    let owner_id =
        sqlx::query_scalar::<_, String>("SELECT owner_id FROM pages WHERE id = $1 FOR UPDATE")
            .bind(page_id)
            .fetch_one(&mut *tx)
            .await?;

    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT seq FROM stroke_batches WHERE page_id = $1 AND client_batch_id = $2",
    )
    .bind(page_id)
    .bind(client_batch_id)
    .fetch_optional(&mut *tx)
    .await?;
    let seq = if let Some(existing) = existing {
        existing
    } else {
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM stroke_batches WHERE page_id = $1",
        )
        .bind(page_id)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO stroke_batches (page_id, seq, client_batch_id, strokes) VALUES ($1, $2, $3, $4)",
        )
        .bind(page_id)
        .bind(next)
        .bind(client_batch_id)
        .bind(sqlx::types::Json(strokes))
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE pages SET updated_at = now() WHERE id = $1")
            .bind(page_id)
            .execute(&mut *tx)
            .await?;
        next
    };

    let thumbnail_job_created = sqlx::query(
        "INSERT INTO page_thumbnails (page_id, source_seq, status) VALUES ($1, $2, 'generating') ON CONFLICT (page_id, source_seq) DO NOTHING",
    )
    .bind(page_id)
    .bind(seq)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;

    tx.commit().await?;
    Ok(PersistedBatch {
        seq: seq as u64,
        owner_id,
        thumbnail_job_created,
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
}
