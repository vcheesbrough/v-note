use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use protocol::{LibraryEvent, RealtimeTicketResponse};
use rand::RngCore;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::auth::{validate_jwt, Claims};
use crate::AppState;

const TICKET_TTL_SECONDS: i64 = 60;
const LIBRARY_CHANNEL_CAPACITY: usize = 64;

#[derive(Default)]
pub struct RealtimeHub {
    tickets: Mutex<HashMap<String, Ticket>>,
    library_channels: Mutex<HashMap<String, broadcast::Sender<LibraryEvent>>>,
}

#[derive(Clone)]
struct Ticket {
    owner_id: String,
    expires_at: DateTime<Utc>,
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
        Err(status) => return (status, "realtime authentication failed").into_response(),
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
                    break;
                }
            }
            message = inbound.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
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
