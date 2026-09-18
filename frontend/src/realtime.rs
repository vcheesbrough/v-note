//! The two WebSocket channels: the owner-wide library feed and the per-page ink
//! feed. Each loop reconnects forever; what an event *does* lives with the
//! screen that shows it (`library::apply_event`, `viewer::apply_page_event`).

use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use protocol::{LibraryEvent, PageServerMessage, PageSummary, Paper, StrokeBatch};

use crate::{api, library, viewer};

pub(crate) async fn library_realtime_loop(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    library_error: RwSignal<Option<String>>,
) {
    loop {
        if let Err(error) =
            library_realtime_once(pages, selected_page, pages_loaded, library_error).await
        {
            library_error.set(Some(error));
            TimeoutFuture::new(1_000).await;
        }
    }
}

async fn library_realtime_once(
    pages: RwSignal<Vec<PageSummary>>,
    selected_page: RwSignal<Option<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let ticket = api::realtime_ticket("realtime ticket").await?;

    api::load_pages(pages, pages_loaded, library_error).await?;

    let ws_url = api::realtime_url(&ticket.ticket)?;
    let mut socket = WebSocket::open(&ws_url)
        .map_err(|error| format!("opening realtime socket failed: {error:?}"))?;
    while let Some(message) = socket.next().await {
        let message = message.map_err(|error| format!("realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: LibraryEvent = serde_json::from_str(&text)
                .map_err(|error| format!("invalid realtime event: {error}"))?;
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
            Ok(()) => TimeoutFuture::new(500).await,
            Err(error) => {
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
    let ticket = api::realtime_ticket("page realtime ticket").await?;
    let ws_url = api::page_realtime_url(page_id, &ticket.ticket)?;
    let socket = WebSocket::open(&ws_url)
        .map_err(|error| format!("opening page realtime socket failed: {error:?}"))?;
    let (mut write, mut read) = socket.split();
    let from_seq = last_seq.get_untracked();
    write
        .send(Message::Text(
            serde_json::json!({ "type": "subscribe", "from_seq": from_seq }).to_string(),
        ))
        .await
        .map_err(|error| format!("page subscribe failed: {error:?}"))?;
    viewer_status.set("Subscribing".to_string());

    while let Some(message) = read.next().await {
        let message = message.map_err(|error| format!("page realtime socket failed: {error:?}"))?;
        if let Message::Text(text) = message {
            let event: PageServerMessage = serde_json::from_str(&text)
                .map_err(|error| format!("invalid page realtime event: {error}"))?;
            viewer::apply_page_event(event, batches, viewer_status, viewer_error, last_seq, paper);
        }
    }
    Err("page realtime socket closed".to_string())
}
