//! Client telemetry for the SPA (#354): spans and levelled logs, exported as
//! OTLP/JSON to `/otlp/spa` on the SPA's own origin.
//!
//! Same origin is the whole authentication story. The export is a same-origin
//! `fetch`, so the browser attaches the `HttpOnly` session cookie by itself, the
//! SPA never holds a token, and there is no CORS involved.
//!
//! Three rules hold everywhere in this module, because telemetry that costs the
//! product anything is worse than no telemetry:
//!
//! 1. **Nothing here can fail loudly.** Every entry point swallows its errors.
//!    There is no `?` that reaches a caller and no error that reaches the UI.
//! 2. **Nothing here blocks.** Exports are spawned, never awaited by the code
//!    being measured.
//! 3. **Nothing here is unbounded.** The queues are capped and the send rate is
//!    capped, whatever the app does.
//!
//! The pure parts live in [`otlp`] (the wire encoding) and [`outbox`] (queueing
//! and the send policy) and are unit-tested on the host; this file is the part
//! that needs a browser, and is covered by `e2e/tests/client-telemetry.spec.ts`.

pub(crate) mod otlp;
pub(crate) mod outbox;
pub(crate) mod span;

use std::cell::RefCell;

use gloo_timers::future::TimeoutFuture;

pub(crate) use otlp::Severity;
use otlp::{ErrorStatus, KeyValue, LogRecord, Span, SpanId, SpanKind, TraceId, UnixNanos};
use outbox::{ExportOutcome, ExportPolicy, Outbox};
use span::OpenSpan;
pub(crate) use span::Parent;

/// How often the exporter wakes. Long enough that a session makes a handful of
/// requests a minute; short enough that a failure is in Loki while someone is
/// still looking at the screen that produced it.
const TICK_MS: u32 = 5_000;

/// Version of this hand-rolled exporter, reported as `telemetry.sdk.version`.
/// Bump when the encoding changes, not when the app does — `service.version` is
/// the app's, and the sidecar supplies it.
const SDK_VERSION: &str = "1";

thread_local! {
    /// The SPA is single-threaded (`wasm32-unknown-unknown`, no threads), so a
    /// thread-local `RefCell` is a global with no synchronisation cost. Every
    /// borrow below is short and non-reentrant: nothing inside a `with_state`
    /// closure calls back into this module.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    trace_id: TraceId,
    /// Items dropped for lack of room that have already been reported, so the
    /// warning below is emitted on each new loss rather than every tick.
    reported_drops: u64,
    /// The span enclosing everything the current screen does. Route changes
    /// replace it, which is what keeps one trace per screen rather than one per
    /// page load. Read only by [`screen`] — nothing else consults "the current"
    /// anything; see `span.rs` for why.
    root: SpanId,
    spans: Outbox<Span>,
    logs: Outbox<LogRecord>,
    policy: ExportPolicy,
    enabled: bool,
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE
        .try_with(|state| {
            state
                .try_borrow_mut()
                .ok()
                .and_then(|mut state| state.as_mut().map(f))
        })
        .ok()
        .flatten()
}

/// Starts telemetry for this page load and spawns the export loop.
///
/// Called before anything else in `main`, so the panic hook and the first route
/// span are covered. Collecting begins immediately but nothing is *sent* until
/// [`set_session`] confirms a signed-in session, since the cookie is what
/// authorises an export.
pub(crate) fn init() {
    let trace_id = TraceId::from_random(random_bytes());
    let root = SpanId::from_random(random_bytes());
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            trace_id,
            reported_drops: 0,
            root,
            spans: Outbox::new(outbox::CAPACITY),
            logs: Outbox::new(outbox::CAPACITY),
            policy: ExportPolicy::default(),
            enabled: true,
        });
    });
    wasm_bindgen_futures::spawn_local(export_loop());
}

/// Whether there is a signed-in session. Until this says `true` nothing is sent.
pub(crate) fn set_session(signed_in: bool) {
    with_state(|state| state.policy.set_authenticated(signed_in));
}

// ---------------------------------------------------------------------------
// Spans
// ---------------------------------------------------------------------------

/// An open span. Finishing it is [`SpanHandle::end`] or [`SpanHandle::fail`];
/// dropping it without either records nothing, which is deliberate — a span
/// abandoned by a cancelled future never happened as far as the trace is
/// concerned.
///
/// Its trace and parent were fixed when it opened ([`span::OpenSpan`]); ending
/// it touches shared state only to queue the finished span.
#[must_use = "a span that is never ended is never recorded"]
pub(crate) struct SpanHandle(OpenSpan);

impl SpanHandle {
    pub(crate) fn attr(mut self, key: &'static str, value: impl Into<otlp::AnyValue>) -> Self {
        self.0.attr(key, value.into());
        self
    }

    /// This span as a parent — for a child span, or a log written inside it.
    pub(crate) fn context(&self) -> Parent {
        self.0.context()
    }

    pub(crate) fn end(self) {
        self.finish(None);
    }

    pub(crate) fn fail(self, message: impl Into<String>) {
        self.finish(Some(ErrorStatus::new(message)));
    }

    /// The `traceparent` for a request made inside this span, so the server's
    /// span becomes its child.
    pub(crate) fn traceparent(&self) -> String {
        let context = self.context();
        otlp::traceparent(context.trace_id, context.span_id)
    }

    fn finish(self, status: Option<ErrorStatus>) {
        let span = self.0.finish(now_unix_nanos(), status);
        with_state(|state| state.spans.push(span));
    }
}

/// The current screen's root, to parent work under. Capture it where the work
/// *starts* — at the top of a request, or of one connection attempt — and pass
/// it down; never re-read it later, or work that outlives a route change moves
/// traces halfway through.
///
/// Before [`init`] (or if the state is unavailable) this is a fresh, parentless
/// context, so a span opened then is merely orphaned rather than lost.
pub(crate) fn screen() -> Parent {
    with_state(|state| Parent {
        trace_id: state.trace_id,
        span_id: state.root,
    })
    .unwrap_or_else(|| Parent {
        trace_id: TraceId::from_random(random_bytes()),
        span_id: SpanId::from_random(random_bytes()),
    })
}

/// Opens a span under `parent`.
pub(crate) fn span(name: &'static str, parent: Parent) -> SpanHandle {
    open(name, SpanKind::Internal, parent)
}

/// Opens a span for an outbound request under `parent`. `Client` kind is what
/// makes a backend render it as the caller of the server's span rather than a
/// sibling.
pub(crate) fn client_span(name: &'static str, parent: Parent) -> SpanHandle {
    open(name, SpanKind::Client, parent)
}

fn open(name: &'static str, kind: SpanKind, parent: Parent) -> SpanHandle {
    SpanHandle(OpenSpan::open(
        parent,
        SpanId::from_random(random_bytes()),
        name,
        kind,
        now_unix_nanos(),
    ))
}

/// Starts a new trace rooted at `name`, ending the previous screen's.
///
/// One trace per screen rather than per page load: a session left open all day
/// would otherwise build a single unboundedly deep trace, which no backend
/// renders usefully.
pub(crate) fn start_screen(name: &'static str, route: &str) {
    let trace_id = TraceId::from_random(random_bytes());
    let root = SpanId::from_random(random_bytes());
    let start = now_unix_nanos();
    with_state(|state| {
        state.trace_id = trace_id;
        state.root = root;
        // A zero-length span standing for the screen itself, so the route's
        // spans have a named root to hang from even before anything finishes.
        state.spans.push(Span {
            trace_id,
            span_id: root,
            parent_span_id: None,
            name,
            kind: SpanKind::Internal,
            start_time_unix_nano: start,
            end_time_unix_nano: start,
            attributes: vec![KeyValue {
                key: "vnote.route",
                value: route.to_string().into(),
            }],
            status: None,
        });
    });
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// Records a log line correlated with the current screen.
///
/// **Never pass user content.** Page titles and stroke data are the user's; ids,
/// counts, statuses and our own error strings are not. This goes to a server
/// the user does not control, so the rule is the same as the server's own. The
/// one deliberate exception is the panic hook — see [`install_panic_hook`].
pub(crate) fn log(severity: Severity, message: impl Into<String>, attributes: Vec<KeyValue>) {
    log_in(screen(), severity, message, attributes);
}

/// As [`log`], correlated with `context` — the span the line is about, so Loki
/// links it to that request rather than to the screen as a whole.
pub(crate) fn log_in(
    context: Parent,
    severity: Severity,
    message: impl Into<String>,
    attributes: Vec<KeyValue>,
) {
    let time = now_unix_nanos();
    let message = message.into();
    with_state(|state| {
        state.logs.push(LogRecord {
            time,
            severity,
            body: message.clone(),
            attributes,
            context: Some((context.trace_id, context.span_id)),
        });
    });
    // The browser console stays the local view. It is what a developer with the
    // page open actually reads, and it keeps working when export is off.
    console(severity, &message);
}

/// One log attribute. `key` is `&'static str` so an attribute name can only
/// ever come from the code, never from data.
pub(crate) fn attr(key: &'static str, value: impl Into<otlp::AnyValue>) -> KeyValue {
    KeyValue {
        key,
        value: value.into(),
    }
}

pub(crate) fn info_in(context: Parent, message: impl Into<String>) {
    log_in(context, Severity::Info, message, Vec::new());
}

pub(crate) fn warn_in(context: Parent, message: impl Into<String>) {
    log_in(context, Severity::Warn, message, Vec::new());
}

pub(crate) fn error_in(context: Parent, message: impl Into<String>) {
    log_in(context, Severity::Error, message, Vec::new());
}

fn console(severity: Severity, message: &str) {
    let value = wasm_bindgen::JsValue::from_str(message);
    match severity {
        Severity::Debug => web_sys::console::debug_1(&value),
        Severity::Info => web_sys::console::info_1(&value),
        Severity::Warn => web_sys::console::warn_1(&value),
        Severity::Error => web_sys::console::error_1(&value),
    }
}

/// Routes WASM panics to OTLP as well as the console.
///
/// Installed instead of `console_error_panic_hook::set_once`, not alongside it:
/// this calls that hook itself, so the console output is unchanged and the
/// stack-trace machinery is still theirs.
pub(crate) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // The FULL panic message and its location — `info`'s `Display` is both.
        // This is the one deliberate exception to "never pass user content":
        // for `.unwrap()`/`.expect()` on an `Err` the message embeds that
        // error's `Debug`, and a serde error can quote the value it failed on,
        // which may be user data. Kept anyway (decided on PR #55) because a
        // panic is the rarest and most valuable thing this module reports, and
        // the location alone rarely says which of several `expect`s fired.
        log(
            Severity::Error,
            format!("wasm panic: {info}"),
            vec![KeyValue {
                key: "exception.type",
                value: "panic".into(),
            }],
        );
        // Flushed immediately: a panic usually means this page load is over, and
        // the next tick may never come.
        flush();
        console_error_panic_hook::hook(info);
    }));
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

async fn export_loop() {
    loop {
        TimeoutFuture::new(TICK_MS).await;
        if with_state(|state| state.policy.is_switched_off()).unwrap_or(true) {
            // Nothing will be sent again this page load, so stop waking up.
            with_state(|state| {
                state.enabled = false;
                state.spans.clear();
                state.logs.clear();
            });
            return;
        }
        export_once().await;
    }
}

/// Telemetry that was thrown away is itself worth knowing about — otherwise a
/// gap in a trace looks like something that never happened rather than
/// something that was dropped. Queued as an ordinary log, so it rides out on
/// the next tick; it cannot recurse, since pushing a log never drops anything
/// except by displacing an older item.
fn report_drops() {
    let Some(Some((dropped, _))) = with_state(|state| {
        let dropped = state.spans.dropped() + state.logs.dropped();
        (dropped > state.reported_drops).then(|| {
            let previously = state.reported_drops;
            state.reported_drops = dropped;
            (dropped, previously)
        })
    }) else {
        return;
    };
    log(
        Severity::Warn,
        "client telemetry was dropped: the queue filled up",
        vec![attr("vnote.telemetry_dropped", dropped as i64)],
    );
}

async fn export_once() {
    report_drops();
    let Some(should) = with_state(|state| {
        state.policy.should_export() && !(state.spans.is_empty() && state.logs.is_empty())
    }) else {
        return;
    };
    if !should {
        return;
    }

    let spans = with_state(|state| state.spans.take_batch(outbox::MAX_BATCH)).unwrap_or_default();
    if !spans.is_empty() {
        let body = otlp::traces_request(&spans, SDK_VERSION);
        let outcome = post("/otlp/spa/v1/traces", body).await;
        with_state(|state| state.policy.record(outcome));
        if outcome == ExportOutcome::SwitchedOff {
            return;
        }
    }

    let logs = with_state(|state| state.logs.take_batch(outbox::MAX_BATCH)).unwrap_or_default();
    if !logs.is_empty() {
        let body = otlp::logs_request(&logs, SDK_VERSION);
        let outcome = post("/otlp/spa/v1/logs", body).await;
        with_state(|state| state.policy.record(outcome));
    }
}

/// Sends whatever is queued without waiting for the next tick, after a panic.
/// A normal fetch: the page usually survives a panic, and this path wants the
/// status back so the policy stays accurate. For the page going away, see
/// [`flush_on_pagehide`].
pub(crate) fn flush() {
    let Some(true) = with_state(|state| {
        state.enabled
            && !state.policy.is_switched_off()
            && !(state.spans.is_empty() && state.logs.is_empty())
    }) else {
        return;
    };
    wasm_bindgen_futures::spawn_local(export_once());
}

/// The most a `sendBeacon` body may be. Browsers queue beacons against a 64 KiB
/// budget per page and refuse one that would exceed it; this leaves room for
/// the two beacons (traces, logs) to share it.
const BEACON_MAX_BYTES: usize = 30 * 1024;

/// How many items a pagehide beacon starts from, before trimming to fit.
const BEACON_MAX_ITEMS: usize = 64;

/// Sends what is queued as the page goes away.
///
/// A normal `fetch` started during `pagehide` is cancelled when the document
/// unloads, so the previous version of this — which spawned an ordinary export —
/// delivered nothing, and the last few seconds before a tab close (exactly when
/// an error tends to arrive) were lost. `sendBeacon` is queued by the browser
/// and survives the unload. It is same-origin, so the session cookie
/// authenticates it at the ingress exactly as for a normal export.
///
/// Fire-and-forget: a beacon returns no status, and nothing is left to act on
/// one. So the backoff wait is not consumed and the policy is not updated;
/// only the two hard gates apply — the server has not switched ingest off, and
/// there is a session to send with.
pub(crate) fn flush_on_pagehide() {
    let Some((spans, logs)) = with_state(|state| {
        if !state.enabled || state.policy.is_switched_off() || !state.policy.is_authenticated() {
            return None;
        }
        Some((
            state.spans.take_batch(BEACON_MAX_ITEMS),
            state.logs.take_batch(BEACON_MAX_ITEMS),
        ))
    })
    .flatten() else {
        return;
    };
    if !spans.is_empty() {
        beacon(
            "/otlp/spa/v1/traces",
            fit_beacon(spans, |items| otlp::traces_request(items, SDK_VERSION)),
        );
    }
    if !logs.is_empty() {
        beacon(
            "/otlp/spa/v1/logs",
            fit_beacon(logs, |items| otlp::logs_request(items, SDK_VERSION)),
        );
    }
}

/// Encodes `items`, halving them until the body fits a beacon. The batch is
/// oldest-first, so halving keeps the oldest and cuts the newest. What is cut is
/// lost — nothing runs after unload to send it — which only matters when a
/// single tick produced more than ~30 KiB, far beyond a normal session.
fn fit_beacon<T>(mut items: Vec<T>, encode: impl Fn(&[T]) -> String) -> String {
    loop {
        let body = encode(&items);
        if body.len() <= BEACON_MAX_BYTES || items.len() <= 1 {
            return body;
        }
        items.truncate(items.len() / 2);
    }
}

fn beacon(path: &str, body: String) {
    let Some(navigator) = web_sys::window().map(|window| window.navigator()) else {
        return;
    };
    let parts = js_sys::Array::of1(&wasm_bindgen::JsValue::from_str(&body));
    let options = web_sys::BlobPropertyBag::new();
    // Without an explicit type the beacon goes as `text/plain`, and the OTLP
    // receiver only parses `application/json` as JSON.
    options.set_type("application/json");
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&parts, &options) else {
        return;
    };
    // `false` means the browser refused to queue it (over its budget). There is
    // no one to tell.
    let _ = navigator.send_beacon_with_opt_blob(path, Some(&blob));
}

/// A bare `fetch`, on the SPA's own origin so the session cookie rides along.
/// Returns what the exporter should conclude — never an error, because there is
/// no caller who could do anything with one.
async fn post(path: &str, body: String) -> ExportOutcome {
    let request = gloo_net::http::Request::post(path)
        .header("content-type", "application/json")
        .body(body);
    let status = match request {
        Ok(request) => request.send().await.ok().map(|response| response.status()),
        // The body could not be built. Not a transport failure, but the same
        // thing from here: this batch is gone.
        Err(_) => None,
    };
    ExportOutcome::from_status(status)
}

// ---------------------------------------------------------------------------
// Browser primitives
// ---------------------------------------------------------------------------

/// `N` bytes from the browser's CSPRNG, falling back to `Math.random` if it is
/// unavailable. Trace and span ids need to be unique, not unguessable, so the
/// fallback is a real fallback and not a security compromise.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0_u8; N];
    let filled = web_sys::window()
        .and_then(|window| window.crypto().ok())
        .is_some_and(|crypto| crypto.get_random_values_with_u8_array(&mut bytes).is_ok());
    if !filled {
        for byte in &mut bytes {
            *byte = (js_sys::Math::random() * 256.0) as u8;
        }
    }
    bytes
}

/// Wall-clock nanoseconds since the epoch.
///
/// From `Date::now`, not `performance.now()`: a span's timestamps have to be
/// comparable with the server's, and `performance.now()` is relative to the page
/// load. Millisecond resolution is all `Date::now` offers — the browser clamps
/// finer clocks for Spectre reasons anyway — so the nanosecond digits are zeros.
fn now_unix_nanos() -> UnixNanos {
    UnixNanos((js_sys::Date::now() as u64).saturating_mul(1_000_000))
}
