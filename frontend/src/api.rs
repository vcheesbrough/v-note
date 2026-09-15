//! HTTP calls and WebSocket URLs. Every request carries a request id the
//! server echoes, so a browser failure can be matched to a server log line.

use gloo_net::http::Request;
use js_sys::Date;
use leptos::prelude::*;
use protocol::{
    ListPagesResponse, MeResponse, MetaResponse, PageSummary, REQUEST_ID_HEADER,
    RealtimeTicketResponse,
};

fn request_id() -> String {
    let now = Date::now().round() as u64;
    let random = (js_sys::Math::random() * 1_000_000_000_000.0).round() as u64;
    format!("spa_{now:x}_{random:x}")
}

/// `POST /api/realtime-ticket`: the short-lived credential both WebSocket
/// channels present, since a browser socket cannot carry a bearer header.
/// `context` names the caller in the error, as before the two copies merged.
pub(crate) async fn realtime_ticket(context: &str) -> Result<RealtimeTicketResponse, String> {
    let response = Request::post("/api/realtime-ticket")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("{context} failed: {error}"))?;
    if !response.ok() {
        return Err(format!("{context} failed: HTTP {}", response.status()));
    }
    response
        .json::<RealtimeTicketResponse>()
        .await
        .map_err(|error| format!("invalid {context}: {error}"))
}

/// `GET /api/meta`.
pub(crate) async fn fetch_meta() -> Result<MetaResponse, String> {
    let response = Request::get("/api/meta")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("request failed: {error}"))?;
    response
        .json::<MetaResponse>()
        .await
        .map_err(|error| format!("invalid response JSON: {error}"))
}

/// `GET /api/me`. `Err(status)` for a non-2xx reply, `Err(0)` for a transport
/// error, `Err(500)` for a profile that does not parse.
pub(crate) async fn fetch_me() -> Result<MeResponse, u16> {
    let result = Request::get("/api/me")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await;
    match result {
        Ok(response) if response.ok() => response.json::<MeResponse>().await.map_err(|_| 500),
        Ok(response) => Err(response.status()),
        Err(_) => Err(0),
    }
}

pub(crate) async fn load_pages(
    pages: RwSignal<Vec<PageSummary>>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let response = Request::get("/api/pages")
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("loading pages failed: {error}"))?;
    if !response.ok() {
        return Err(format!("loading pages failed: HTTP {}", response.status()));
    }
    let body = response
        .json::<ListPagesResponse>()
        .await
        .map_err(|error| format!("invalid pages response: {error}"))?;
    pages.set(body.pages);
    library_error.set(None);
    Ok(())
}

/// `DELETE /api/pages/{id}`. The caller drops the page from the library; see
/// `library::remove_page`.
pub(crate) async fn delete_page(page_id: &str) -> Result<(), String> {
    let response = Request::delete(&format!("/api/pages/{page_id}"))
        .header(REQUEST_ID_HEADER, &request_id())
        .send()
        .await
        .map_err(|error| format!("deleting page failed: {error}"))?;
    if !response.ok() {
        return Err(format!("deleting page failed: HTTP {}", response.status()));
    }
    Ok(())
}

pub(crate) fn realtime_url(ticket: &str) -> Result<String, String> {
    websocket_url(&format!("/api/realtime?ticket={ticket}"))
}

pub(crate) fn page_realtime_url(page_id: &str, ticket: &str) -> Result<String, String> {
    websocket_url(&format!("/api/pages/{page_id}/realtime?ticket={ticket}"))
}

fn websocket_url(path_and_query: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "window unavailable".to_string())?;
    let location = window.location();
    let protocol = location
        .protocol()
        .map_err(|_| "location protocol unavailable".to_string())?;
    let host = location
        .host()
        .map_err(|_| "location host unavailable".to_string())?;
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    Ok(format!("{scheme}://{host}{path_and_query}"))
}
