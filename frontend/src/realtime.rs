//! The two WebSocket channels: the owner-wide library feed and the per-page ink
//! feed. Each loop reconnects until the session is refused; what an event
//! *does* lives with the screen that shows it (`library::apply_event`,
//! `viewer::apply_page_event`). Each loop also keeps its channel's
//! [`SyncState`], which is what the screen uses to show or withhold its data.

use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use protocol::{LibraryEvent, PageServerMessage, PageSummary, Paper, StrokeBatch};

use crate::sync::{ChannelError, SyncState, parse_error, socket_error};
use crate::{api, library, telemetry, viewer};

/// Fixed rather than backing off: the SPA is a viewer someone is watching, and
/// PLAN.md's reconnect rule for it is one second (#279).
const RECONNECT_DELAY_MS: u32 = 1_000;

/// Record why a channel's connection ended. Returns `false` when the loop must
/// stop: a refused session will be refused again, so retrying every second
/// would only fill the server's log until the user signs in.
fn connection_lost(
    channel: &'static str,
    page_id: Option<&str>,
    attempts: u32,
    error: &ChannelError,
    sync: RwSignal<SyncState>,
) -> bool {
    let keep_trying = !matches!(error, ChannelError::Unauthorized(_));
    let mut attrs = vec![
        telemetry::attr("vnote.channel", channel),
        telemetry::attr("vnote.reconnect_attempt", i64::from(attempts)),
        // Never page content: see `ChannelError::detail`.
        telemetry::attr("vnote.reason", error.detail().to_string()),
    ];
    if let Some(page_id) = page_id {
        attrs.push(telemetry::attr("vnote.page_id", page_id.to_string()));
    }
    let message = if keep_trying {
        format!("{channel} realtime connection lost; reconnecting")
    } else {
        format!("{channel} realtime refused the session; stopped")
    };
    telemetry::log(telemetry::Severity::Warn, message, attrs);
    sync.set(error.state());
    keep_trying
}

pub(crate) async fn library_realtime_loop(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    sync: RwSignal<SyncState>,
) {
    // Counted rather than logged per attempt: a library left open through a
    // server restart reconnects until it succeeds, and a line each would be
    // noise. The count is what says whether this was a blip or a bad hour.
    let mut attempts: u32 = 0;
    loop {
        // A library socket ends only by failing or closing.
        let error = match library_connection(pages, selected_page, pages_loaded, sync).await {
            Ok(()) => ChannelError::Transport("realtime socket closed".to_string()),
            Err(error) => error,
        };
        attempts += 1;
        if !connection_lost("library", None, attempts, &error, sync) {
            return;
        }
        TimeoutFuture::new(RECONNECT_DELAY_MS).await;
    }
}

/// One connection, from ticket to close.
async fn library_connection(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    sync: RwSignal<SyncState>,
) -> Result<(), ChannelError> {
    // One parent per connection attempt, captured before anything awaits: a
    // reconnect after the user has changed screen belongs to the new screen,
    // and every part of *this* attempt stays together in the one it began in.
    let parent = telemetry::screen();
    let ticket = api::realtime_ticket(parent, "realtime ticket").await?;

    let ws_url = api::realtime_url(&ticket.ticket)?;
    // Ends as soon as the socket is open rather than spanning the connection:
    // this is the trace of *opening* it, and the connection outlives the trace
    // the ticket belongs to. The server's own connection span is parented to
    // that ticket, which is what links the two.
    let connect = telemetry::span("realtime.connect", parent).attr("vnote.channel", "library");
    let mut socket = match WebSocket::open(&ws_url) {
        Ok(socket) => {
            connect.end();
            socket
        }
        Err(error) => {
            let message = format!("opening realtime socket failed: {error:?}");
            connect.fail(message.clone());
            return Err(ChannelError::Transport(message));
        }
    };
    // Every (re)connect refetches the list, so what the library shows after a
    // drop is the server's, not what was on screen before it — and it does so
    // only once the socket is open. The server subscribes a library socket
    // before it answers the upgrade, so from here on every event is already
    // queued on the socket: one published while the list loads arrives after
    // it and is applied on top. Loading first, as before #631, left a window
    // between the snapshot and the subscription in which an event (a restore,
    // a delete from another device) was lost until the next reload.
    await_open(&socket).await?;
    api::load_pages(parent, pages, pages_loaded).await?;
    telemetry::info_in(parent, "library realtime connected");
    sync.set(SyncState::Live);
    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| socket_error("realtime socket", error))?;
        if let Message::Text(text) = message {
            let event: LibraryEvent = serde_json::from_str(&text).map_err(|error| {
                // A message the SPA cannot read is a contract break between two
                // deployed things, which is worth an error even though the loop
                // recovers by reconnecting.
                telemetry::error_in(parent, "library realtime event did not parse");
                parse_error("realtime event", &error)
            })?;
            library::apply_event(pages, selected_page, event);
        }
    }
    Ok(())
}

/// Resolves once `socket` has finished its handshake, or fails if it closed
/// instead. `gloo-net` opens without a future to await, so this polls the
/// ready state; the handshake is one round trip, so it takes a few ticks.
async fn await_open(socket: &WebSocket) -> Result<(), ChannelError> {
    use gloo_net::websocket::State;
    loop {
        match socket.state() {
            State::Open => return Ok(()),
            State::Connecting => TimeoutFuture::new(OPEN_POLL_MS).await,
            State::Closing | State::Closed => {
                return Err(ChannelError::Transport(
                    "realtime socket closed before it opened".to_string(),
                ));
            }
        }
    }
}

/// How often [`await_open`] looks at a connecting socket.
const OPEN_POLL_MS: u32 = 10;

/// The page channel's signals, as the viewer owns them.
#[derive(Clone, Copy)]
pub(crate) struct PageChannel {
    pub(crate) batches: RwSignal<Vec<StrokeBatch>>,
    pub(crate) status: RwSignal<String>,
    pub(crate) sync: RwSignal<SyncState>,
    pub(crate) last_seq: RwSignal<u64>,
    pub(crate) paper: RwSignal<Paper>,
}

/// Follow one page until the owning effect aborts the future, or until the
/// server refuses the session.
pub(crate) async fn page_realtime_loop(page_id: String, channel: PageChannel) {
    let mut attempts: u32 = 0;
    loop {
        let error = match page_connection(&page_id, channel).await {
            Ok(()) => ChannelError::Transport("page realtime socket closed".to_string()),
            Err(error) => error,
        };
        attempts += 1;
        channel.status.set("Reconnecting".to_string());
        if !connection_lost("page", Some(&page_id), attempts, &error, channel.sync) {
            return;
        }
        TimeoutFuture::new(RECONNECT_DELAY_MS).await;
    }
}

async fn page_connection(page_id: &str, channel: PageChannel) -> Result<(), ChannelError> {
    let parent = telemetry::screen();
    let ticket = api::realtime_ticket(parent, "page realtime ticket").await?;
    let ws_url = api::page_realtime_url(page_id, &ticket.ticket)?;
    let connect = telemetry::span("realtime.connect", parent)
        .attr("vnote.channel", "page")
        .attr("vnote.page_id", page_id.to_string());
    let socket = match WebSocket::open(&ws_url) {
        Ok(socket) => {
            connect.end();
            socket
        }
        Err(error) => {
            let message = format!("opening page realtime socket failed: {error:?}");
            connect.fail(message.clone());
            return Err(ChannelError::Transport(message));
        }
    };
    let (mut write, mut read) = socket.split();
    let from_seq = channel.last_seq.get_untracked();
    // `from_seq` is the viewer's cursor, so this span says how much of the page
    // a reconnect had to ask for — the difference between a cheap resume and a
    // full replay.
    let subscribe = telemetry::span("realtime.subscribe", parent)
        .attr("vnote.page_id", page_id.to_string())
        .attr("vnote.from_seq", from_seq as i64);
    if let Err(error) = write
        .send(Message::Text(
            serde_json::json!({ "type": "subscribe", "from_seq": from_seq }).to_string(),
        ))
        .await
    {
        let error = socket_error("page subscribe", error);
        subscribe.fail(error.detail().to_string());
        return Err(error);
    }
    subscribe.end();
    // `debug`, not `info`: one line per (re)connect is per-request detail, and a
    // flapping page channel would otherwise fill the log with it. The span above
    // carries the same `from_seq` for anyone looking at the trace.
    telemetry::log_in(
        parent,
        telemetry::Severity::Debug,
        format!("page realtime subscribed from seq {from_seq}"),
        Vec::new(),
    );
    channel.status.set("Subscribing".to_string());

    while let Some(message) = read.next().await {
        let message = message.map_err(|error| socket_error("page realtime socket", error))?;
        if let Message::Text(text) = message {
            let event: PageServerMessage = serde_json::from_str(&text).map_err(|error| {
                telemetry::error_in(parent, "page realtime event did not parse");
                parse_error("page realtime event", &error)
            })?;
            viewer::apply_page_event(event, channel)?;
        }
    }
    Ok(())
}
