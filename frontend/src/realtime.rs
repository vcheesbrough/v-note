//! The two WebSocket channels: the owner-wide library feed and the per-page ink
//! feed. Each loop reconnects forever; what an event *does* lives with the
//! screen that shows it (`library::apply_event`, `viewer::apply_page_event`).

use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use protocol::{LibraryEvent, PageServerMessage, PageSummary, Paper, StrokeBatch};

use crate::{api, library, telemetry, viewer};

pub(crate) async fn library_realtime_loop(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    library_error: RwSignal<Option<String>>,
) {
    // Counted rather than logged per attempt: a library left open through a
    // server restart reconnects until it succeeds, and a line each would be
    // noise. The count is what says whether this was a blip or a bad hour.
    let mut attempts: u32 = 0;
    loop {
        if let Err(error) =
            library_realtime_once(pages, selected_page, pages_loaded, library_error).await
        {
            attempts += 1;
            telemetry::log(
                telemetry::Severity::Warn,
                "library realtime connection lost; reconnecting",
                vec![
                    telemetry::attr("vnote.channel", "library"),
                    telemetry::attr("vnote.reconnect_attempt", i64::from(attempts)),
                    // Ours, never the server's: these strings are all built in
                    // this file and carry no page content.
                    telemetry::attr("vnote.reason", error.clone()),
                ],
            );
            library_error.set(Some(error));
            TimeoutFuture::new(1_000).await;
        } else {
            attempts = 0;
        }
    }
}

async fn library_realtime_once(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    // One parent per connection attempt, captured before anything awaits: a
    // reconnect after the user has changed screen belongs to the new screen,
    // and every part of *this* attempt stays together in the one it began in.
    let parent = telemetry::screen();
    let ticket = api::realtime_ticket(parent, "realtime ticket").await?;

    api::load_pages(parent, pages, pages_loaded, library_error).await?;

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
            return Err(message);
        }
    };
    telemetry::info_in(parent, "library realtime connected");
    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| format!("realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: LibraryEvent = serde_json::from_str(&text).map_err(|error| {
                // A message the SPA cannot read is a contract break between two
                // deployed things, which is worth an error even though the loop
                // recovers by reconnecting.
                telemetry::error_in(parent, "library realtime event did not parse");
                format!("invalid realtime event: {error}")
            })?;
            library::apply_event(pages, selected_page, event);
            library_error.set(None);
        }
    }

    Err("realtime socket closed".to_string())
}

pub(crate) async fn page_realtime_loop(
    page_id: String,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
) -> Result<(), String> {
    let mut attempts: u32 = 0;
    loop {
        match page_realtime_once(
            &page_id,
            batches,
            viewer_status,
            viewer_error,
            last_seq,
            paper,
        )
        .await
        {
            Ok(()) => {
                attempts = 0;
                TimeoutFuture::new(500).await;
            }
            Err(error) => {
                attempts += 1;
                telemetry::log(
                    telemetry::Severity::Warn,
                    "page realtime connection lost; reconnecting",
                    vec![
                        telemetry::attr("vnote.channel", "page"),
                        telemetry::attr("vnote.page_id", page_id.clone()),
                        telemetry::attr("vnote.reconnect_attempt", i64::from(attempts)),
                        telemetry::attr("vnote.reason", error.clone()),
                    ],
                );
                viewer_error.set(Some(error));
                viewer_status.set("Reconnecting".to_string());
                TimeoutFuture::new(1_000).await;
            }
        }
    }
}

async fn page_realtime_once(
    page_id: &str,
    batches: RwSignal<Vec<StrokeBatch>>,
    viewer_status: RwSignal<String>,
    viewer_error: RwSignal<Option<String>>,
    last_seq: RwSignal<u64>,
    paper: RwSignal<Paper>,
) -> Result<(), String> {
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
            return Err(message);
        }
    };
    let (mut write, mut read) = socket.split();
    let from_seq = last_seq.get_untracked();
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
        let message = format!("page subscribe failed: {error:?}");
        subscribe.fail(message.clone());
        return Err(message);
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
    viewer_status.set("Subscribing".to_string());

    while let Some(message) = read.next().await {
        let message = message.map_err(|error| format!("page realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: PageServerMessage = serde_json::from_str(&text).map_err(|error| {
                telemetry::error_in(parent, "page realtime event did not parse");
                format!("invalid page realtime event: {error}")
            })?;
            viewer::apply_page_event(event, batches, viewer_status, viewer_error, last_seq, paper);
        }
    }
    Err("page realtime socket closed".to_string())
}
