//! Client telemetry for the SPA: spans and levelled logs, exported as OTLP/JSON
//! to this environment's `otlp-collector-oidc` ingest (#439; built in #354).
//!
//! **No configuration, no telemetry.** Nothing is sent until the server has
//! answered `GET /api/telemetry/config` with an endpoint and a bearer token;
//! until then items wait in a small, short-lived buffer, and if the server
//! says nothing (ingest off, a failed fetch, no session) they are discarded and
//! OTLP is never initialised. The ingest accepts only a bearer, so the token
//! the SPA's `HttpOnly` cookie carries is handed to it by that route — the
//! deliberate cost of the reference ingest, recorded in `AGENTS.md`.
//!
//! Three rules hold everywhere in this module, because telemetry that costs the
//! product anything is worse than no telemetry:
//!
//! 1. **Nothing here can fail loudly.** Every entry point swallows its errors.
//!    There is no `?` that reaches a caller and no error that reaches the UI.
//!    Failures are said once, on the console, on a change of state only.
//! 2. **Nothing here blocks.** Exports are spawned, never awaited by the code
//!    being measured.
//! 3. **Nothing here is unbounded.** The queues are capped, retries are capped,
//!    and repeated failure stops telemetry for the page load.
//!
//! The pure parts live in [`otlp`] (the wire encoding) and [`outbox`] (queueing,
//! configuration and the send policy) and are unit-tested on the host; this
//! file is the part that needs a browser, and is covered by
//! `e2e/tests/client-telemetry.spec.ts`.

pub(crate) mod otlp;
pub(crate) mod outbox;
pub(crate) mod span;

use std::cell::RefCell;

use gloo_timers::future::TimeoutFuture;
use protocol::TelemetryConfigResponse;
use wasm_bindgen::JsCast as _;

pub(crate) use otlp::Severity;
use otlp::{ErrorStatus, KeyValue, LogRecord, Span, SpanId, SpanKind, TraceId, UnixNanos};
use outbox::{
    ConfigFetch, Credentials, ExportOutcome, ExportPolicy, Lifecycle, OffReason, Outbox, Signal,
    Transition,
};
use span::OpenSpan;
pub(crate) use span::Parent;

/// How often the exporter wakes. Long enough that a session makes a handful of
/// requests a minute; short enough that a failure is in Loki while someone is
/// still looking at the screen that produced it.
const TICK_MS: u32 = 5_000;

/// Where the configuration comes from. Same origin, cookie-authenticated.
const CONFIG_PATH: &str = "/api/telemetry/config";

thread_local! {
    /// The SPA is single-threaded (`wasm32-unknown-unknown`, no threads), so a
    /// thread-local `RefCell` is a global with no synchronisation cost. Every
    /// borrow below is short and non-reentrant: nothing inside a `with_state`
    /// closure calls back into this module.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// A batch that failed retryably, kept to be sent again.
struct Pending {
    body: String,
    items: usize,
    attempts: u32,
}

struct State {
    trace_id: TraceId,
    /// The span enclosing everything the current screen does. Route changes
    /// replace it, which is what keeps one trace per screen rather than one per
    /// page load. Read only by [`screen`] — nothing else consults "the current"
    /// anything; see `span.rs` for why.
    root: SpanId,
    spans: Outbox<Span>,
    logs: Outbox<LogRecord>,
    lifecycle: Lifecycle,
    policy: ExportPolicy,
    retry_spans: Option<Pending>,
    retry_logs: Option<Pending>,
    /// Items thrown away after a send failed — reported in the local lines.
    dropped_after_send: u64,
    /// Whether the outbox overflow has been said on the console yet.
    overflow_reported: bool,
    /// One export at a time: a slow ingest must not stack requests up.
    exporting: bool,
    /// One config fetch at a time.
    fetching_config: bool,
}

impl State {
    fn dropped(&self) -> u64 {
        self.spans.dropped() + self.logs.dropped() + self.dropped_after_send
    }

    fn discard_everything(&mut self) {
        self.spans.clear();
        self.logs.clear();
        self.retry_spans = None;
        self.retry_logs = None;
    }
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
/// span are covered. Collecting begins immediately into the small pre-config
/// buffer; nothing is *sent* until [`set_session`] has fetched configuration.
pub(crate) fn init() {
    let trace_id = TraceId::from_random(random_bytes());
    let root = SpanId::from_random(random_bytes());
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            trace_id,
            root,
            spans: Outbox::new(outbox::PRE_CONFIG_CAPACITY),
            logs: Outbox::new(outbox::PRE_CONFIG_CAPACITY),
            lifecycle: Lifecycle::new(now_ms()),
            policy: ExportPolicy::default(),
            retry_spans: None,
            retry_logs: None,
            dropped_after_send: 0,
            overflow_reported: false,
            exporting: false,
            fetching_config: false,
        });
    });
    wasm_bindgen_futures::spawn_local(export_loop());
}

/// Whether there is a signed-in session. The config route needs one, so this
/// is what starts the fetch; a signed-out page never has telemetry.
pub(crate) fn set_session(signed_in: bool) {
    if !signed_in {
        stop(OffReason::SignedOut);
        return;
    }
    wasm_bindgen_futures::spawn_local(async {
        refresh_config().await;
    });
}

/// Fetches configuration and applies it. The first call turns telemetry on or
/// off; later calls are the token refresh — on expiry and after a `401`.
async fn refresh_config() {
    let Some(false) = with_state(|state| {
        let busy = state.fetching_config || state.lifecycle.is_off();
        if !busy {
            state.fetching_config = true;
        }
        busy
    }) else {
        return;
    };
    let fetch = fetch_config().await;
    let (configured, endpoint) = match &fetch {
        ConfigFetch::Configured(credentials) => (true, credentials.endpoint.clone()),
        ConfigFetch::Absent => (false, String::new()),
    };
    let first = with_state(|state| {
        state.fetching_config = false;
        let first = !matches!(state.lifecycle, Lifecycle::Configured(_));
        state.lifecycle.on_config(fetch);
        if configured {
            state.policy.refreshed();
            state.spans.set_capacity(outbox::CAPACITY);
            state.logs.set_capacity(outbox::CAPACITY);
        }
        first
    })
    .unwrap_or(false);
    if configured {
        if first {
            console(
                Severity::Info,
                &format!("client telemetry: exporting to {endpoint}"),
            );
        }
    } else {
        stop(OffReason::NotConfigured);
    }
}

/// `GET /api/telemetry/config`. Everything but a `200` with a parseable body is
/// "no configuration" — including a `404` from a server that predates the route.
async fn fetch_config() -> ConfigFetch {
    let Ok(response) = gloo_net::http::Request::get(CONFIG_PATH).send().await else {
        return ConfigFetch::Absent;
    };
    if response.status() != 200 {
        return ConfigFetch::Absent;
    }
    match response.json::<TelemetryConfigResponse>().await {
        Ok(config) if !config.endpoint.is_empty() => ConfigFetch::Configured(Credentials {
            endpoint: config.endpoint,
            access_token: config.access_token,
            expires_at: config.expires_at as f64,
        }),
        _ => ConfigFetch::Absent,
    }
}

/// Turns telemetry off for the page load, discards what is queued, and says so
/// — once, whatever the reason and however many callers race to it.
fn stop(reason: OffReason) {
    let Some(Some(dropped)) = with_state(|state| {
        if state.lifecycle.is_off() && state.policy.stopped().is_some() {
            return None;
        }
        let was_off = state.lifecycle.is_off();
        state.lifecycle.turn_off(reason);
        let transition = state.policy.stop(reason);
        let dropped = state.dropped();
        state.discard_everything();
        (!was_off || transition.is_some()).then_some(dropped)
    }) else {
        return;
    };
    let level = match reason {
        OffReason::NotConfigured | OffReason::SignedOut | OffReason::ConfigTimedOut => {
            Severity::Info
        }
        OffReason::Unauthorized | OffReason::GaveUp => Severity::Warn,
    };
    console(
        level,
        &format!(
            "client telemetry off for this page: {}; {dropped} item(s) dropped",
            reason.describe()
        ),
    );
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
        with_state(|state| {
            if !state.lifecycle.is_off() {
                state.spans.push(span);
            }
        });
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
        if state.lifecycle.is_off() {
            return;
        }
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
        if !state.lifecycle.is_off() {
            state.logs.push(LogRecord {
                time,
                severity,
                body: message.clone(),
                attributes,
                context: Some((context.trace_id, context.span_id)),
            });
        }
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
        let now = now_ms();
        let Some(step) = with_state(|state| {
            if state.lifecycle.expire(now) {
                return Step::TimedOut;
            }
            if state.lifecycle.is_off() || state.policy.stopped().is_some() {
                return Step::Finished;
            }
            let Some(credentials) = state.lifecycle.credentials() else {
                return Step::Wait;
            };
            if state.policy.needs_refresh() || credentials.usable_token(now / 1_000.0).is_none() {
                return Step::Refresh;
            }
            Step::Export
        }) else {
            return;
        };
        report_overflow();
        match step {
            Step::TimedOut => stop(OffReason::ConfigTimedOut),
            // Nothing will be sent again this page load, so stop waking up.
            Step::Finished => {
                with_state(State::discard_everything);
                return;
            }
            Step::Wait => {}
            Step::Refresh => refresh_config().await,
            Step::Export => export_once().await,
        }
    }
}

enum Step {
    TimedOut,
    Finished,
    Wait,
    Refresh,
    Export,
}

/// The first time a queue overflows, say so on the console. Not into the
/// telemetry queue: when items are piling up, that is the channel that is
/// failing, and a report queued behind them only displaces one more.
fn report_overflow() {
    let Some(Some(dropped)) = with_state(|state| {
        let overflowed = state.spans.dropped() + state.logs.dropped() > 0;
        (overflowed && !state.overflow_reported).then(|| {
            state.overflow_reported = true;
            state.dropped()
        })
    }) else {
        return;
    };
    console(
        Severity::Warn,
        &format!("client telemetry queue full; {dropped} item(s) dropped so far"),
    );
}

/// What one signal's turn in an export found to send.
struct Batch {
    signal: Signal,
    url: String,
    token: String,
    body: String,
    items: usize,
    attempts: u32,
}

/// Takes the next batch for `signal` — a retry first, else fresh items — with
/// the credential as it is **now**: the token is read per request, so a
/// refreshed one is used by the very next export.
fn next_batch(state: &mut State, signal: Signal, now_s: f64) -> Option<Batch> {
    let credentials = state.lifecycle.credentials()?;
    let token = credentials.usable_token(now_s)?.to_string();
    let url = credentials.url(signal);
    let retry = match signal {
        Signal::Traces => state.retry_spans.take(),
        Signal::Logs => state.retry_logs.take(),
    };
    let (body, items, attempts) = match retry {
        Some(pending) => (pending.body, pending.items, pending.attempts),
        None => match signal {
            Signal::Traces => {
                let spans = state.spans.take_batch(outbox::MAX_BATCH);
                if spans.is_empty() {
                    return None;
                }
                (
                    otlp::traces_request(&spans, release_version()),
                    spans.len(),
                    0,
                )
            }
            Signal::Logs => {
                let logs = state.logs.take_batch(outbox::MAX_BATCH);
                if logs.is_empty() {
                    return None;
                }
                (otlp::logs_request(&logs, release_version()), logs.len(), 0)
            }
        },
    };
    Some(Batch {
        signal,
        url,
        token,
        body,
        items,
        attempts: attempts + 1,
    })
}

async fn export_once() {
    let Some(true) = with_state(|state| {
        let go = !state.exporting && state.policy.should_export(now_ms());
        if go {
            state.exporting = true;
        }
        go
    }) else {
        return;
    };
    for signal in [Signal::Traces, Signal::Logs] {
        let Some(Some(batch)) = with_state(|state| next_batch(state, signal, now_ms() / 1_000.0))
        else {
            continue;
        };
        let outcome = post(&batch.url, &batch.token, batch.body.clone(), false).await;
        let accepted = outcome == ExportOutcome::Accepted;
        let reported = with_state(|state| {
            let decision =
                state
                    .policy
                    .record(outcome, batch.attempts, now_ms(), js_sys::Math::random());
            if decision.retry_batch {
                let pending = Some(Pending {
                    body: batch.body,
                    items: batch.items,
                    attempts: batch.attempts,
                });
                match batch.signal {
                    Signal::Traces => state.retry_spans = pending,
                    Signal::Logs => state.retry_logs = pending,
                }
            } else if !accepted {
                state.dropped_after_send += batch.items as u64;
            }
            decision
                .transition
                .map(|transition| (transition, state.dropped()))
        })
        .flatten();
        if let Some((transition, dropped)) = reported {
            report_transition(transition, outcome, &batch.url, dropped);
        }
        if !accepted {
            // One answer that is not "accepted" is enough for this tick: the
            // other signal waits for the policy, not for a second refusal.
            break;
        }
    }
    with_state(|state| state.exporting = false);
    if let Some(Some(reason)) = with_state(|state| state.policy.stopped()) {
        stop(reason);
    }
}

/// One console line per change of state (`client-export.md`, *Log transitions,
/// not batches*): the status or error kind, where, and how much was lost. Never
/// the token.
fn report_transition(transition: Transition, outcome: ExportOutcome, url: &str, dropped: u64) {
    match transition {
        Transition::StartedFailing => console(
            Severity::Warn,
            &format!(
                "client telemetry export failing: {} from {url}; {dropped} item(s) dropped so far",
                outcome.describe()
            ),
        ),
        Transition::Recovered => console(
            Severity::Warn,
            &format!(
                "client telemetry export recovered at {url}; {dropped} item(s) dropped while failing"
            ),
        ),
        // `stop` says the final line, with the reason, once.
        Transition::Stopped(_) => {}
    }
}

/// Sends whatever is queued without waiting for the next tick, after a panic.
/// A normal fetch: the page usually survives a panic, and this path wants the
/// status back so the policy stays accurate. For the page going away, see
/// [`flush_on_pagehide`].
pub(crate) fn flush() {
    let Some(true) = with_state(|state| {
        state.lifecycle.credentials().is_some()
            && !(state.spans.is_empty() && state.logs.is_empty())
    }) else {
        return;
    };
    wasm_bindgen_futures::spawn_local(export_once());
}

/// The most a pagehide body may be. Browsers queue `keepalive` requests against
/// a 64 KiB budget per page and refuse one that would exceed it; this leaves
/// room for the two (traces, logs) to share it.
const KEEPALIVE_MAX_BYTES: usize = 30 * 1024;

/// How many items a pagehide flush starts from, before trimming to fit.
const KEEPALIVE_MAX_ITEMS: usize = 64;

/// Sends what is queued as the page goes away.
///
/// An ordinary `fetch` started during `pagehide` is cancelled when the document
/// unloads; a `keepalive` one is not. `sendBeacon` would survive too, but it
/// cannot carry an `Authorization` header, and the ingest accepts nothing else
/// (#439). So this is `fetch(…, { keepalive: true })` with the bearer, within
/// keepalive's budget.
///
/// Fire-and-forget: nothing runs after unload to act on the answer, so the
/// backoff is not consulted and the policy is not updated; only the hard gates
/// apply — configured, not stopped, no refresh owed, and a usable token.
pub(crate) fn flush_on_pagehide() {
    let now_s = now_ms() / 1_000.0;
    let Some(Some((credentials, spans, logs))) = with_state(|state| {
        if state.policy.stopped().is_some() || state.policy.needs_refresh() {
            return None;
        }
        let credentials = state.lifecycle.credentials()?.clone();
        credentials.usable_token(now_s)?;
        Some((
            credentials,
            state.spans.take_batch(KEEPALIVE_MAX_ITEMS),
            state.logs.take_batch(KEEPALIVE_MAX_ITEMS),
        ))
    }) else {
        return;
    };
    let Some(token) = credentials.usable_token(now_s) else {
        return;
    };
    let version = release_version();
    if !spans.is_empty() {
        let body = fit_keepalive(spans, |items| otlp::traces_request(items, version));
        send_on_unload(&credentials.url(Signal::Traces), token, body);
    }
    if !logs.is_empty() {
        let body = fit_keepalive(logs, |items| otlp::logs_request(items, version));
        send_on_unload(&credentials.url(Signal::Logs), token, body);
    }
}

/// Encodes `items`, halving them until the body fits the keepalive budget. The
/// batch is oldest-first, so halving keeps the oldest and cuts the newest. What
/// is cut is lost — nothing runs after unload to send it — which only matters
/// when a single tick produced more than ~30 KiB, far beyond a normal session.
fn fit_keepalive<T>(mut items: Vec<T>, encode: impl Fn(&[T]) -> String) -> String {
    loop {
        let body = encode(&items);
        if body.len() <= KEEPALIVE_MAX_BYTES || items.len() <= 1 {
            return body;
        }
        items.truncate(items.len() / 2);
    }
}

fn send_on_unload(url: &str, token: &str, body: String) {
    // The request is handed to the browser synchronously by `fetch`; the
    // promise is dropped because nothing will be alive to read it.
    let _ = start_fetch(url, token, body, true);
}

/// Builds and starts the `fetch`. `credentials: omit` — the ingest takes a
/// bearer, and the session cookie has no business reaching it.
fn start_fetch(url: &str, token: &str, body: String, keepalive: bool) -> Option<js_sys::Promise> {
    let window = web_sys::window()?;
    let headers = web_sys::Headers::new().ok()?;
    headers.set("content-type", "application/json").ok()?;
    headers
        .set("authorization", &format!("Bearer {token}"))
        .ok()?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(&wasm_bindgen::JsValue::from_str(&body));
    init.set_credentials(web_sys::RequestCredentials::Omit);
    init.set_mode(web_sys::RequestMode::Cors);
    // web-sys exposes no setter for `keepalive`; it is a plain RequestInit
    // member, so it is set as one.
    if keepalive {
        js_sys::Reflect::set(&init, &"keepalive".into(), &true.into()).ok()?;
    }
    Some(window.fetch_with_str_and_init(url, &init))
}

/// One export `POST`. Returns what the exporter should conclude — never an
/// error, because there is no caller who could do anything with one.
async fn post(url: &str, token: &str, body: String, keepalive: bool) -> ExportOutcome {
    let Some(promise) = start_fetch(url, token, body, keepalive) else {
        return ExportOutcome::from_response(None, None);
    };
    let Ok(value) = wasm_bindgen_futures::JsFuture::from(promise).await else {
        return ExportOutcome::from_response(None, None);
    };
    let Ok(response) = value.dyn_into::<web_sys::Response>() else {
        return ExportOutcome::from_response(None, None);
    };
    let retry_after = response.headers().get("retry-after").ok().flatten();
    ExportOutcome::from_response(Some(response.status()), retry_after.as_deref())
}

/// The build, reported as `service.version`. The same constant the version
/// watermark shows.
fn release_version() -> &'static str {
    match option_env!("V_NOTE_RELEASE") {
        Some(release) if !release.is_empty() => release,
        _ => env!("CARGO_PKG_VERSION"),
    }
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

/// Wall-clock milliseconds, for the export policy's timers.
fn now_ms() -> f64 {
    js_sys::Date::now()
}
