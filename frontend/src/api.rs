//! HTTP calls and WebSocket URLs. Every request carries a request id the
//! server echoes, so a browser failure can be matched to a server log line, and
//! a `traceparent` (#354) so the server's span becomes a child of the browser's.
//!
//! The two are not redundant: `X-Request-Id` is what appears in a log line a
//! human greps for, `traceparent` is what a backend joins on. Neither replaced
//! the other.

use gloo_net::http::{Request, RequestBuilder};
use js_sys::Date;
use leptos::prelude::*;
use protocol::{
    ListPagesResponse, MeResponse, MetaResponse, PageSummary, REQUEST_ID_HEADER,
    RealtimeTicketResponse,
};

use crate::telemetry::{self, SpanHandle};

const TRACEPARENT_HEADER: &str = "traceparent";

fn request_id() -> String {
    let now = Date::now().round() as u64;
    let random = (js_sys::Math::random() * 1_000_000_000_000.0).round() as u64;
    format!("spa_{now:x}_{random:x}")
}

/// Opens the client span for one REST call and builds the request inside it, so
/// the `traceparent` names that span and the server's work nests under it.
///
/// `route` is the *template*, never the concrete path: a span attribute with an
/// id in it is fine, but this one is also how spans are grouped, and
/// `/api/pages/abc123` as a name makes every page its own operation.
fn start(method: &'static str, route: &'static str, url: &str) -> (RequestBuilder, SpanHandle) {
    let span = telemetry::client_span("http.client")
        .attr("http.request.method", method)
        .attr("url.template", route);
    let mut request = match method {
        "GET" => Request::get(url),
        "POST" => Request::post(url),
        "DELETE" => Request::delete(url),
        // No other verb is used here; treating an unknown one as GET would hide
        // the mistake, and the SPA is read-only so there is nothing to add.
        other => unreachable!("unsupported method {other}"),
    }
    .header(REQUEST_ID_HEADER, &request_id());
    if let Some(traceparent) = span.traceparent() {
        request = request.header(TRACEPARENT_HEADER, &traceparent);
    }
    (request, span)
}

/// Builds and sends. `RequestBuilder::build` fails only on a malformed header
/// value, which cannot happen for the two this file sets — but it is a `Result`,
/// so it is reported as the request failure it would be rather than unwrapped.
async fn send(request: RequestBuilder) -> Result<gloo_net::http::Response, gloo_net::Error> {
    request.build()?.send().await
}

/// `POST /api/realtime-ticket`: the short-lived credential both WebSocket
/// channels present, since a browser socket cannot carry a bearer header.
/// `context` names the caller in the error, as before the two copies merged.
///
/// The span matters more here than anywhere else in this file: a browser cannot
/// put a `traceparent` on a WebSocket upgrade, so the server stores *this*
/// request's trace with the ticket and parents the connection to it (#354).
pub(crate) async fn realtime_ticket(context: &str) -> Result<RealtimeTicketResponse, String> {
    let (request, span) = start("POST", "/api/realtime-ticket", "/api/realtime-ticket");
    let response = match send(request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("{context} failed: {error}");
            span.fail(message.clone());
            telemetry::warn(format!("realtime ticket request failed ({context})"));
            return Err(message);
        }
    };
    let span = span.attr("http.response.status_code", response.status());
    if !response.ok() {
        let message = format!("{context} failed: HTTP {}", response.status());
        span.fail(message.clone());
        telemetry::warn(format!(
            "realtime ticket refused ({context}): HTTP {}",
            response.status()
        ));
        return Err(message);
    }
    match response.json::<RealtimeTicketResponse>().await {
        Ok(ticket) => {
            span.end();
            Ok(ticket)
        }
        Err(error) => {
            let message = format!("invalid {context}: {error}");
            span.fail(message.clone());
            telemetry::error(format!(
                "realtime ticket response did not parse ({context})"
            ));
            Err(message)
        }
    }
}

/// `GET /api/meta`.
pub(crate) async fn fetch_meta() -> Result<MetaResponse, String> {
    let (request, span) = start("GET", "/api/meta", "/api/meta");
    let response = match send(request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("request failed: {error}");
            span.fail(message.clone());
            return Err(message);
        }
    };
    let span = span.attr("http.response.status_code", response.status());
    match response.json::<MetaResponse>().await {
        Ok(meta) => {
            span.end();
            Ok(meta)
        }
        Err(error) => {
            let message = format!("invalid response JSON: {error}");
            span.fail(message.clone());
            telemetry::warn("meta response did not parse");
            Err(message)
        }
    }
}

/// `GET /api/me`. `Err(status)` for a non-2xx reply, `Err(0)` for a transport
/// error, `Err(500)` for a profile that does not parse.
pub(crate) async fn fetch_me() -> Result<MeResponse, u16> {
    let (request, span) = start("GET", "/api/me", "/api/me");
    let result = send(request).await;
    match result {
        Ok(response) if response.ok() => {
            let span = span.attr("http.response.status_code", response.status());
            match response.json::<MeResponse>().await {
                Ok(me) => {
                    span.end();
                    Ok(me)
                }
                Err(_) => {
                    span.fail("profile did not parse");
                    telemetry::error("profile response did not parse");
                    Err(500)
                }
            }
        }
        // 401/403 here is a signed-out browser, which is a normal state and not
        // a failure: `main` reads it to decide which screen to show.
        Ok(response) => {
            let status = response.status();
            span.attr("http.response.status_code", status).end();
            Err(status)
        }
        Err(error) => {
            span.fail(format!("request failed: {error}"));
            telemetry::warn("profile request failed");
            Err(0)
        }
    }
}

/// `GET /api/pages`. `pages_loaded` records that the library has answered at
/// all, which is what tells an id that is not in `pages` apart from one the
/// list has simply not reached yet. It lives here rather than at a call site
/// because every successful load establishes it, including the retrying one.
pub(crate) async fn load_pages(
    pages: RwSignal<Vec<PageSummary>>,
    pages_loaded: RwSignal<bool>,
    library_error: RwSignal<Option<String>>,
) -> Result<(), String> {
    let (request, span) = start("GET", "/api/pages", "/api/pages");
    let response = match send(request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("loading pages failed: {error}");
            span.fail(message.clone());
            telemetry::warn("loading pages failed: transport error");
            return Err(message);
        }
    };
    let span = span.attr("http.response.status_code", response.status());
    if !response.ok() {
        let message = format!("loading pages failed: HTTP {}", response.status());
        span.fail(message.clone());
        telemetry::error(format!("loading pages failed: HTTP {}", response.status()));
        return Err(message);
    }
    let body = match response.json::<ListPagesResponse>().await {
        Ok(body) => body,
        Err(error) => {
            let message = format!("invalid pages response: {error}");
            span.fail(message.clone());
            telemetry::error("pages response did not parse");
            return Err(message);
        }
    };
    // A count, not the pages: titles are the user's content.
    span.attr("vnote.page_count", body.pages.len() as i64).end();
    pages.set(body.pages);
    pages_loaded.set(true);
    library_error.set(None);
    Ok(())
}

/// `DELETE /api/pages/{id}`. The caller drops the page from the library; see
/// `library::remove_page`.
pub(crate) async fn delete_page(page_id: &str) -> Result<(), String> {
    let (request, span) = start(
        "DELETE",
        "/api/pages/{page_id}",
        &format!("/api/pages/{page_id}"),
    );
    // The page id is an opaque identifier, not user content, and it is the one
    // thing that makes this span answerable ("which delete failed?").
    let span = span.attr("vnote.page_id", page_id.to_string());
    let response = match send(request).await {
        Ok(response) => response,
        Err(error) => {
            let message = format!("deleting page failed: {error}");
            span.fail(message.clone());
            telemetry::warn("deleting page failed: transport error");
            return Err(message);
        }
    };
    let span = span.attr("http.response.status_code", response.status());
    if !response.ok() {
        let message = format!("deleting page failed: HTTP {}", response.status());
        span.fail(message.clone());
        telemetry::error(format!("deleting page failed: HTTP {}", response.status()));
        return Err(message);
    }
    span.end();
    telemetry::info("page deleted");
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
