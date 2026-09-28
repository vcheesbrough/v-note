use std::net::SocketAddr;
use std::time::Instant;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use once_cell::sync::Lazy;
use opentelemetry::KeyValue;
use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use rand::Rng;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::FilterExt as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::ObservabilityConfig;

pub use protocol::REQUEST_ID_HEADER;
pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";
/// Metric/trace label form of [`protocol::PROTOCOL_VERSION`]. A bare `&str`
/// because both consumers want a `'static` label, so there is no compile-time
/// link to the canonical constant — `protocol_version_label_matches_protocol`
/// below is that link.
const PROTOCOL_VERSION: &str = "7";

/// Realtime frame and replay sizes: from a ~50 B `synced` up to a multi-MB
/// replay. Since #323 a replay is a single frame, so the top buckets now
/// measure one `page-replay` rather than a whole run of `stroke-batch`es (the
/// `DensePageSeeder` reference page replayed 7.30 MB before that change).
const REALTIME_BYTES_BUCKETS: [f64; 9] = [
    128.0, 512.0, 2048.0, 8192.0, 32768.0, 131072.0, 524288.0, 2097152.0, 8388608.0,
];
/// Frames per replay. Since #323 this is **1** — the whole snapshot is one
/// `page-replay` frame. The wide upper buckets are kept deliberately: they are
/// what makes a regression back towards per-batch frames visible rather than
/// saturating the top bucket. The dense reference page sent 1,267 before.
const REALTIME_REPLAY_FRAMES_BUCKETS: [f64; 12] = [
    1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0,
];
const REALTIME_REPLAY_DURATION_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
const REALTIME_HANDLING_BUCKETS: [f64; 12] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

static METRICS: Lazy<Metrics> = Lazy::new(Metrics::new);

pub fn metrics() -> &'static Metrics {
    &METRICS
}

pub struct Metrics {
    registry: Registry,
    http: HttpMetrics,
    auth_failures_total: IntCounterVec,
    page_mutations_total: IntCounterVec,
    client_telemetry_requests_total: IntCounterVec,
    thumbnails: ThumbnailMetrics,
    realtime: RealtimeMetrics,
    _build_info: IntGauge,
}

struct HttpMetrics {
    requests_total: IntCounterVec,
    request_duration_seconds: HistogramVec,
}

struct ThumbnailMetrics {
    generation_duration_seconds: HistogramVec,
    queue_depth: IntGauge,
    recoveries_total: IntCounterVec,
    artifact_bytes: Histogram,
}

struct RealtimeMetrics {
    events_total: IntCounterVec,
    active_connections: IntGauge,
    message_bytes: HistogramVec,
    replay_bytes: Histogram,
    replay_frames: Histogram,
    replay_duration_seconds: Histogram,
    message_handling_seconds: HistogramVec,
}

/// Registers `collector` and hands it back for recording. A duplicate name is a
/// programming error, so it panics at startup rather than failing quietly.
fn register<C>(registry: &Registry, collector: C) -> C
where
    C: prometheus::core::Collector + Clone + 'static,
{
    registry
        .register(Box::new(collector.clone()))
        .expect("metric should register once");
    collector
}

impl HttpMetrics {
    fn new(registry: &Registry) -> Self {
        Self {
            requests_total: register(
                registry,
                IntCounterVec::new(
                    Opts::new(
                        "v_note_http_requests_total",
                        "HTTP requests by route and status",
                    ),
                    &["method", "route", "status"],
                )
                .expect("http request counter should build"),
            ),
            request_duration_seconds: register(
                registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "v_note_http_request_duration_seconds",
                        "HTTP request latency by route and status",
                    ),
                    &["method", "route", "status"],
                )
                .expect("http request histogram should build"),
            ),
        }
    }
}

impl ThumbnailMetrics {
    fn new(registry: &Registry) -> Self {
        Self {
            generation_duration_seconds: register(
                registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "v_note_thumbnail_generation_duration_seconds",
                        "Thumbnail generation duration by result",
                    ),
                    &["result"],
                )
                .expect("thumbnail generation histogram should build"),
            ),
            queue_depth: register(
                registry,
                IntGauge::new(
                    "v_note_thumbnail_queue_depth",
                    "Thumbnail generations awaiting completion",
                )
                .expect("thumbnail queue gauge should build"),
            ),
            recoveries_total: register(
                registry,
                IntCounterVec::new(
                    Opts::new(
                        "v_note_thumbnail_recoveries_total",
                        "Thumbnail generation recovery attempts by result",
                    ),
                    &["result"],
                )
                .expect("thumbnail recovery counter should build"),
            ),
            artifact_bytes: register(
                registry,
                Histogram::with_opts(
                    HistogramOpts::new(
                        "v_note_thumbnail_artifact_bytes",
                        "Stored thumbnail PNG size in bytes",
                    )
                    .buckets(vec![
                        256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0,
                    ]),
                )
                .expect("thumbnail artifact histogram should build"),
            ),
        }
    }
}

impl RealtimeMetrics {
    fn new(registry: &Registry) -> Self {
        Self {
            events_total: register(
                registry,
                IntCounterVec::new(
                    Opts::new(
                        "v_note_realtime_events_total",
                        "Realtime WebSocket events by channel and result",
                    ),
                    &["channel", "result"],
                )
                .expect("realtime event counter should build"),
            ),
            active_connections: register(
                registry,
                IntGauge::new(
                    "v_note_realtime_active_connections",
                    "Currently open realtime WebSocket connections",
                )
                .expect("active realtime gauge should build"),
            ),
            // Cardinality budget: `channel` × `message_type`, both bounded enums.
            // Never add `page_id`, `session_id`, `owner_id` or `client_batch_id` —
            // those belong in span fields, not labels.
            message_bytes: register(
                registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "v_note_realtime_message_bytes",
                        "Serialized size of realtime frames sent, by channel and message type",
                    )
                    .buckets(REALTIME_BYTES_BUCKETS.to_vec()),
                    &["channel", "message_type"],
                )
                .expect("realtime message size histogram should build"),
            ),
            replay_bytes: register(
                registry,
                Histogram::with_opts(
                    HistogramOpts::new(
                        "v_note_realtime_replay_bytes",
                        "Total bytes sent for one page-channel subscribe replay",
                    )
                    .buckets(REALTIME_BYTES_BUCKETS.to_vec()),
                )
                .expect("realtime replay bytes histogram should build"),
            ),
            replay_frames: register(
                registry,
                Histogram::with_opts(
                    HistogramOpts::new(
                        "v_note_realtime_replay_frames",
                        "Frames sent for one page-channel subscribe replay, including the closing synced",
                    )
                    .buckets(REALTIME_REPLAY_FRAMES_BUCKETS.to_vec()),
                )
                .expect("realtime replay frames histogram should build"),
            ),
            replay_duration_seconds: register(
                registry,
                Histogram::with_opts(
                    HistogramOpts::new(
                        "v_note_realtime_replay_duration_seconds",
                        "Wall time from a subscribe being received to its synced being sent",
                    )
                    .buckets(REALTIME_REPLAY_DURATION_BUCKETS.to_vec()),
                )
                .expect("realtime replay duration histogram should build"),
            ),
            message_handling_seconds: register(
                registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "v_note_realtime_message_handling_seconds",
                        "Server-side handling time of inbound page-channel messages, by message type \
                         (not end-to-end latency)",
                    )
                    .buckets(REALTIME_HANDLING_BUCKETS.to_vec()),
                    &["message_type"],
                )
                .expect("realtime message handling histogram should build"),
            ),
        }
    }
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        let http = HttpMetrics::new(&registry);
        let auth_failures_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "v_note_auth_failures_total",
                    "Authentication failures by reason",
                ),
                &["reason"],
            )
            .expect("auth failure counter should build"),
        );
        let page_mutations_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "v_note_page_mutations_total",
                    "Page and ink mutations by operation/result",
                ),
                &["operation", "result"],
            )
            .expect("page mutation counter should build"),
        );
        let client_telemetry_requests_total = register(
            &registry,
            IntCounterVec::new(
                Opts::new(
                    "v_note_client_telemetry_requests_total",
                    "Client telemetry exports reaching the /otlp ingress, by client kind, signal and outcome",
                ),
                &["client", "signal", "outcome"],
            )
            .expect("client telemetry counter should build"),
        );
        let thumbnails = ThumbnailMetrics::new(&registry);
        let realtime = RealtimeMetrics::new(&registry);
        let build_info = IntGauge::with_opts(
            Opts::new("v_note_build_info", "v-note build and protocol metadata")
                .const_label("protocol", PROTOCOL_VERSION)
                .const_label("version", crate::app_version()),
        )
        .expect("build info gauge should build");
        build_info.set(1);
        let build_info = register(&registry, build_info);

        Self {
            registry,
            http,
            auth_failures_total,
            page_mutations_total,
            client_telemetry_requests_total,
            thumbnails,
            realtime,
            _build_info: build_info,
        }
    }

    pub fn record_auth_failure(&self, reason: &'static str) {
        self.auth_failures_total.with_label_values(&[reason]).inc();
    }

    /// All three labels are `&'static str` on purpose: the ingress passes values
    /// it has already matched against a closed set, never a path segment, so a
    /// caller cannot mint a time series by requesting `/otlp/<anything>/…`.
    pub fn record_client_telemetry(
        &self,
        client: &'static str,
        signal: &'static str,
        outcome: &'static str,
    ) {
        self.client_telemetry_requests_total
            .with_label_values(&[client, signal, outcome])
            .inc();
    }

    pub fn record_page_mutation(&self, operation: &'static str, result: &'static str) {
        self.page_mutations_total
            .with_label_values(&[operation, result])
            .inc();
    }

    pub fn record_thumbnail_generation(&self, result: &'static str, elapsed_seconds: f64) {
        self.thumbnails
            .generation_duration_seconds
            .with_label_values(&[result])
            .observe(elapsed_seconds);
    }

    pub fn thumbnail_generation_queued(&self) {
        self.thumbnails.queue_depth.inc();
    }

    pub fn thumbnail_generation_finished(&self) {
        self.thumbnails.queue_depth.dec();
    }

    pub fn record_thumbnail_recovery(&self, result: &'static str) {
        self.thumbnails
            .recoveries_total
            .with_label_values(&[result])
            .inc();
    }

    pub fn observe_thumbnail_artifact_bytes(&self, bytes: usize) {
        self.thumbnails.artifact_bytes.observe(bytes as f64);
    }

    pub fn record_realtime_event(&self, channel: &'static str, result: &'static str) {
        self.realtime
            .events_total
            .with_label_values(&[channel, result])
            .inc();
    }

    /// One realtime frame put on the wire. Both labels are `'static` so an id
    /// (always an owned `String`) cannot be passed as one by accident.
    pub fn observe_realtime_message_bytes(
        &self,
        channel: &'static str,
        message_type: &'static str,
        bytes: usize,
    ) {
        self.realtime
            .message_bytes
            .with_label_values(&[channel, message_type])
            .observe(bytes as f64);
    }

    /// One completed page-channel replay: every frame from the first
    /// `stroke-batch` to the closing `synced`.
    pub fn observe_realtime_replay(&self, frames: u64, bytes: u64, elapsed_seconds: f64) {
        self.realtime.replay_frames.observe(frames as f64);
        self.realtime.replay_bytes.observe(bytes as f64);
        self.realtime
            .replay_duration_seconds
            .observe(elapsed_seconds);
    }

    /// Server-side handling of one inbound page-channel message. Deliberately
    /// not end-to-end latency: that needs client timestamps (#154).
    pub fn observe_realtime_message_handling(
        &self,
        message_type: &'static str,
        elapsed_seconds: f64,
    ) {
        self.realtime
            .message_handling_seconds
            .with_label_values(&[message_type])
            .observe(elapsed_seconds);
    }

    #[cfg(test)]
    pub(crate) fn realtime_message_count(&self, channel: &str, message_type: &str) -> u64 {
        self.realtime
            .message_bytes
            .with_label_values(&[channel, message_type])
            .get_sample_count()
    }

    #[cfg(test)]
    pub(crate) fn realtime_event_count(&self, channel: &str, result: &str) -> u64 {
        self.realtime
            .events_total
            .with_label_values(&[channel, result])
            .get()
    }

    #[cfg(test)]
    pub(crate) fn thumbnail_queue_depth(&self) -> i64 {
        self.thumbnails.queue_depth.get()
    }

    pub fn realtime_connection_guard(&self) -> RealtimeConnectionGuard {
        self.realtime.active_connections.inc();
        RealtimeConnectionGuard
    }

    fn record_http(&self, method: &str, route: &str, status: StatusCode, elapsed: f64) {
        let status = status.as_u16().to_string();
        self.http
            .requests_total
            .with_label_values(&[method, route, &status])
            .inc();
        self.http
            .request_duration_seconds
            .with_label_values(&[method, route, &status])
            .observe(elapsed);
    }

    fn render(&self) -> Result<String, String> {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .map_err(|error| error.to_string())?;
        String::from_utf8(buffer).map_err(|error| error.to_string())
    }
}

#[must_use]
pub struct RealtimeConnectionGuard;

impl Drop for RealtimeConnectionGuard {
    fn drop(&mut self) {
        metrics().realtime.active_connections.dec();
    }
}

#[derive(Clone, Debug)]
pub struct RequestId(pub String);

pub async fn request_observability_middleware(mut req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = normalized_route(req.uri().path());
    let request_id = request_id_from_headers(req.headers()).unwrap_or_else(generate_request_id);
    req.extensions_mut().insert(RequestId(request_id.clone()));

    let span = request_span(&method, &route, &request_id, req.headers());
    let started = Instant::now();
    let mut response = next.run(req).instrument(span.clone()).await;
    let elapsed = started.elapsed().as_secs_f64();
    let status = response.status();
    metrics().record_http(method.as_str(), &route, status, elapsed);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value.clone());
        response
            .headers_mut()
            .insert(HeaderName::from_static(CORRELATION_ID_HEADER), value);
    }

    // Emitted inside the request span, so `trace_id` / `span_id` come from the
    // active context like every other line's (`WithTraceContext` on stdout, the
    // SDK on the OTLP record) rather than being written out here by hand.
    span.in_scope(|| {
        tracing::info!(
            method = %method,
            route = %route,
            status = status.as_u16(),
            latency_ms = (elapsed * 1000.0),
            request_id = %request_id,
            "http request completed",
        );
    });

    response
}

/// The `http.request` span, parented to the W3C trace context in the request's
/// `traceparent` header. Traefik starts every trace at the edge and forwards that
/// header, so adopting it nests v-note's spans under Traefik's rather than
/// starting a disconnected trace. Without a valid `traceparent` (a request that
/// did not come through Traefik) the span is a new root.
fn request_span(
    method: &axum::http::Method,
    route: &str,
    request_id: &str,
    headers: &axum::http::HeaderMap,
) -> tracing::Span {
    let span = tracing::info_span!(
        "http.request",
        method = %method,
        route = %route,
        request_id = %request_id,
    );
    let parent = TraceContextPropagator::new().extract(&HeaderExtractor(headers));
    if parent.span().span_context().is_valid() {
        // Fails only when no OpenTelemetry layer is installed (export is off),
        // and then there is no trace to join.
        let _ = span.set_parent(parent);
    }
    span
}

/// A span for one Postgres round trip: a query, or a transaction's `BEGIN` or
/// `COMMIT`. `query_name` names the call site (e.g. `persist_batch_insert`), so a
/// slow query is identifiable in Tempo without recording SQL text or bound
/// values, which can carry user content. The `db.response.*` fields are filled
/// in by [`metered`] when the query finishes.
///
/// A macro rather than a function because `tracing` stamps a span with the
/// location of the `info_span!` that built it, and the OpenTelemetry layer
/// exports that as `code.file.path` / `code.module.name` / `code.line.number`.
/// Expanding at the call site makes those name the query, not this file (#343).
macro_rules! db_query_span {
    ($operation:expr, $query_name:expr $(,)?) => {{
        // The function this replaced took `&'static str`, which made a
        // runtime-built label a compile error. `const` keeps that guarantee:
        // both are low-cardinality Tempo attributes, never SQL text or values.
        const OPERATION: &str = $operation;
        const QUERY_NAME: &str = $query_name;
        ::tracing::info_span!(
            "db.query",
            db.system = "postgresql",
            db.operation = OPERATION,
            db.query_name = QUERY_NAME,
            db.response.returned_rows = ::tracing::field::Empty,
            db.response.bytes = ::tracing::field::Empty,
            db.response.max_row_bytes = ::tracing::field::Empty,
            db.response.affected_rows = ::tracing::field::Empty,
        )
    }};
}
pub(crate) use db_query_span;

/// Wraps a query's executor — `metered(pool)`, `metered(&mut *tx)` — so the
/// enclosing `db.query` span records the size of what came back:
///
/// - `db.response.returned_rows`: rows returned;
/// - `db.response.bytes` / `db.response.max_row_bytes`: Postgres wire bytes of
///   the returned column values, in total and for the widest row — the width of
///   the result (a `NULL` carries none);
/// - `db.response.affected_rows`: the command tag's row count, so an `INSERT`,
///   `UPDATE` or `DELETE` reports the rows it wrote. Absent for `fetch_optional`,
///   which does not surface it.
///
/// Only `fetch_many` and `fetch_optional` need wrapping: every other method sqlx
/// calls (`fetch_one`, `fetch_all`, `execute`, …) defaults to one of them. Totals
/// go on the span that is current when the query finishes, which is the
/// `db.query` span the call site instruments the query with.
#[derive(Debug)]
pub struct Metered<E>(E);

pub fn metered<E>(executor: E) -> Metered<E> {
    Metered(executor)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ResultSize {
    rows: u64,
    bytes: u64,
    max_row_bytes: u64,
    affected_rows: Option<u64>,
}

impl ResultSize {
    fn add_row(&mut self, row_bytes: u64) {
        self.rows += 1;
        self.bytes += row_bytes;
        self.max_row_bytes = self.max_row_bytes.max(row_bytes);
    }

    fn add_affected(&mut self, rows: u64) {
        *self.affected_rows.get_or_insert(0) += rows;
    }

    fn record_on_current_span(&self) {
        let span = tracing::Span::current();
        span.record("db.response.returned_rows", self.rows);
        span.record("db.response.bytes", self.bytes);
        span.record("db.response.max_row_bytes", self.max_row_bytes);
        if let Some(affected) = self.affected_rows {
            span.record("db.response.affected_rows", affected);
        }
    }
}

/// Wire bytes of one row's column values; a `NULL` carries none.
fn row_bytes(row: &sqlx::postgres::PgRow) -> u64 {
    use sqlx::Row as _;
    (0..row.len())
        .filter_map(|index| row.try_get_raw(index).ok())
        .filter_map(|value| value.as_bytes().ok().map(|bytes| bytes.len() as u64))
        .sum()
}

/// Accumulates over a result stream and records when the stream is dropped:
/// after its last item, or early if the caller stops reading.
struct StreamSize(ResultSize);

impl Drop for StreamSize {
    fn drop(&mut self) {
        self.0.record_on_current_span();
    }
}

impl<'c, E> sqlx::Executor<'c> for Metered<E>
where
    E: sqlx::Executor<'c, Database = sqlx::Postgres>,
{
    type Database = sqlx::Postgres;

    fn fetch_many<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> futures_util::stream::BoxStream<
        'e,
        Result<
            sqlx::Either<
                <Self::Database as sqlx::Database>::QueryResult,
                <Self::Database as sqlx::Database>::Row,
            >,
            sqlx::Error,
        >,
    >
    where
        'c: 'e,
        Q: 'q + sqlx::Execute<'q, Self::Database>,
    {
        use futures_util::StreamExt as _;
        let mut size = StreamSize(ResultSize::default());
        self.0
            .fetch_many(query)
            .inspect(move |step| match step {
                Ok(sqlx::Either::Left(result)) => size.0.add_affected(result.rows_affected()),
                Ok(sqlx::Either::Right(row)) => size.0.add_row(row_bytes(row)),
                Err(_) => {}
            })
            .boxed()
    }

    fn fetch_optional<'e, 'q: 'e, Q>(
        self,
        query: Q,
    ) -> futures_util::future::BoxFuture<
        'e,
        Result<Option<<Self::Database as sqlx::Database>::Row>, sqlx::Error>,
    >
    where
        'c: 'e,
        Q: 'q + sqlx::Execute<'q, Self::Database>,
    {
        let fetch = self.0.fetch_optional(query);
        Box::pin(async move {
            let row = fetch.await?;
            let mut size = ResultSize::default();
            if let Some(row) = &row {
                size.add_row(row_bytes(row));
            }
            size.record_on_current_span();
            Ok(row)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: sqlx::SqlStr,
        parameters: &'e [<Self::Database as sqlx::Database>::TypeInfo],
    ) -> futures_util::future::BoxFuture<
        'e,
        Result<<Self::Database as sqlx::Database>::Statement, sqlx::Error>,
    >
    where
        'c: 'e,
    {
        self.0.prepare_with(sql, parameters)
    }

    fn describe<'e>(
        self,
        sql: sqlx::SqlStr,
    ) -> futures_util::future::BoxFuture<'e, Result<sqlx::Describe<Self::Database>, sqlx::Error>>
    where
        'c: 'e,
    {
        self.0.describe(sql)
    }
}

/// The OpenTelemetry context of the current span, to carry across a channel to
/// work that happens later on another task (see `realtime::Fanout`). Invalid when
/// there is no current trace, and then ignored by [`set_remote_parent`].
pub fn current_span_context() -> opentelemetry::trace::SpanContext {
    tracing::Span::current()
        .context()
        .span()
        .span_context()
        .clone()
}

/// Parents `span` to a span context carried from elsewhere, so it appears inside
/// that trace. A no-op for an invalid context, which leaves `span` a root.
pub fn set_remote_parent(span: &tracing::Span, origin: &opentelemetry::trace::SpanContext) {
    if origin.is_valid() {
        // Fails only when no OpenTelemetry layer is installed (export is off),
        // and then there is no trace to join.
        let _ =
            span.set_parent(opentelemetry::Context::new().with_remote_span_context(origin.clone()));
    }
}

struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|name| name.as_str()).collect()
    }
}

struct TraceLogContext {
    trace_id: String,
    span_id: String,
}

#[cfg(test)]
fn trace_context_from_span(span: &tracing::Span) -> Option<TraceLogContext> {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    span_context.is_valid().then(|| TraceLogContext {
        trace_id: span_context.trace_id().to_string(),
        span_id: span_context.span_id().to_string(),
    })
}

pub async fn metrics_handler() -> Response {
    match metrics().render() {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(error = %error, "metrics render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics render failed").into_response()
        }
    }
}

pub async fn run_metrics_server(
    addr: Option<SocketAddr>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(addr) = addr else {
        return Ok(());
    };
    let app = Router::new().route("/metrics", get(metrics_handler));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "starting internal metrics listener");
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

/// What `init_tracing` installs when `RUST_LOG` is unset. `opentelemetry` at
/// `warn` is where the SDK reports a failed export, so a collector that is down
/// shows up in `docker logs` rather than nowhere.
pub(crate) const DEFAULT_LOG_FILTER: &str =
    "server=info,tower_http=info,axum=info,opentelemetry=warn";

/// Appended to whatever filter the OTLP log layer runs, so that exporting a
/// batch can never itself produce a record to export: the SDK's own reports and
/// the gRPC stack under the exporter stay on stdout only. A crate name matches
/// the crate and its modules, never a longer crate name (`tower`, not
/// `tower_http`).
const EXPORTER_INTERNAL_CRATES: [&str; 5] = ["tonic", "h2", "hyper", "hyper_util", "tower"];

/// Whether `target` belongs to the OpenTelemetry SDK or the transport under the
/// OTLP exporter. Applied to the OTLP log layer as a filter of its own, ANDed
/// with `otlp-log-filter`, so no directive an operator writes — however
/// specific, e.g. `opentelemetry_sdk=debug` — can let a failing export produce a
/// record to export.
pub(crate) fn is_exporter_internal(target: &str) -> bool {
    let crate_name = target.split("::").next().unwrap_or(target);
    crate_name.starts_with("opentelemetry") || EXPORTER_INTERNAL_CRATES.contains(&crate_name)
}

/// Holds the OTLP providers for the life of the process. Dropping it shuts both
/// down, which flushes whatever spans and log records are still buffered.
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    /// Whether spans are exported over OTLP.
    pub fn exports_traces(&self) -> bool {
        self.tracer_provider.is_some()
    }

    /// Whether log records are exported over OTLP.
    pub fn exports_logs(&self) -> bool {
        self.logger_provider.is_some()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Logs first, so a record written while the spans flush is not the one
        // that misses the boat.
        if let Some(provider) = self.logger_provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("OpenTelemetry log shutdown failed: {error}");
        }
        if let Some(provider) = self.tracer_provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("OpenTelemetry trace shutdown failed: {error}");
        }
    }
}

/// The level filters of the three layers, as `EnvFilter` directives.
///
/// Stdout and the span layer share one (`RUST_LOG`, or [`DEFAULT_LOG_FILTER`]);
/// the OTLP log layer takes `observability.otlp-log-filter` when it is set, so
/// Loki can be quieter or louder than `docker logs` without touching either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogFilters {
    pub(crate) stdout: String,
    pub(crate) otlp_logs: String,
}

impl LogFilters {
    pub(crate) fn new(rust_log: Option<&str>, otlp_log_filter: Option<&str>) -> Self {
        let stdout = rust_log
            .map(str::trim)
            .filter(|value| !value.is_empty() && EnvFilter::try_new(value).is_ok())
            .unwrap_or(DEFAULT_LOG_FILTER)
            .to_string();
        let otlp_logs = otlp_log_filter
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map_or_else(|| stdout.clone(), str::to_string);
        Self { stdout, otlp_logs }
    }

    fn from_env(observability: &ObservabilityConfig) -> Self {
        let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).ok();
        Self::new(
            rust_log.as_deref(),
            observability.otlp_log_filter.as_deref(),
        )
    }

    fn stdout_filter(&self) -> EnvFilter {
        EnvFilter::try_new(&self.stdout).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER))
    }

    fn otlp_log_filter(&self) -> EnvFilter {
        // Config validation has already rejected a malformed directive; the
        // fallback only covers a filter built outside `load_group`.
        EnvFilter::try_new(&self.otlp_logs).unwrap_or_else(|_| self.stdout_filter())
    }
}

pub fn init_tracing(observability: &ObservabilityConfig) -> TelemetryGuard {
    let filters = LogFilters::from_env(observability);
    let guard = build_otlp_providers(observability);
    telemetry_subscriber(
        &filters,
        std::io::stdout,
        guard.tracer_provider.as_ref(),
        guard.logger_provider.as_ref(),
    )
    .init();
    guard
}

/// The whole subscriber stack, shared by [`init_tracing`] and the tests so they
/// assert on what production installs:
///
/// - the JSON `fmt` layer to `make_writer` (stdout in production), with the
///   active `trace_id` / `span_id` added to every line — see [`WithTraceContext`];
/// - the span layer, when a tracer provider exists;
/// - the OTLP log bridge, when a logger provider exists. The SDK attaches the
///   active trace context to each record itself.
///
/// Each layer carries its own filter rather than one global one, because the
/// log bridge is filtered independently (`otlp-log-filter`).
pub(crate) fn telemetry_subscriber<W>(
    filters: &LogFilters,
    make_writer: W,
    tracer_provider: Option<&SdkTracerProvider>,
    logger_provider: Option<&SdkLoggerProvider>,
) -> impl tracing::Subscriber + Send + Sync + for<'a> tracing_subscriber::registry::LookupSpan<'a>
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .event_format(WithTraceContext(
            tracing_subscriber::fmt::format()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false),
        ))
        .with_writer(make_writer)
        .with_filter(filters.stdout_filter());
    let span_layer = tracer_provider.map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("v-note"))
            .with_filter(filters.stdout_filter())
    });
    let log_layer = logger_provider.map(|provider| {
        opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(provider)
            .with_filter(
                filters
                    .otlp_log_filter()
                    .and(tracing_subscriber::filter::filter_fn(|metadata| {
                        !is_exporter_internal(metadata.target())
                    })),
            )
    });
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(span_layer)
        .with(log_layer)
}

/// Wraps the JSON event format and appends the active OpenTelemetry
/// `trace_id` / `span_id` as top-level body fields, so a stdout line carries
/// the same correlation the OTLP record does and Grafana's derived field can
/// turn it into a Tempo link. Never labels: a trace id is unbounded.
///
/// The active context is the one the span layer attaches on span entry — the
/// same one the SDK reads for an exported log record — so both paths name the
/// same span. With no span layer installed, or outside any span, nothing is
/// added.
pub(crate) struct WithTraceContext<F>(F);

impl<S, N, F> tracing_subscriber::fmt::FormatEvent<S, N> for WithTraceContext<F>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
    F: tracing_subscriber::fmt::FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut line = String::new();
        self.0.format_event(
            ctx,
            tracing_subscriber::fmt::format::Writer::new(&mut line),
            event,
        )?;
        if let Some(context) = active_trace_context() {
            insert_trace_fields(&mut line, &context);
        }
        writer.write_str(&line)
    }
}

fn active_trace_context() -> Option<TraceLogContext> {
    opentelemetry::Context::map_current(|context| {
        let span_context = context.span().span_context().clone();
        span_context.is_valid().then(|| TraceLogContext {
            trace_id: span_context.trace_id().to_string(),
            span_id: span_context.span_id().to_string(),
        })
    })
}

/// Adds the two fields just inside the object's closing brace. Both values are
/// lowercase hex, so they need no escaping.
fn insert_trace_fields(line: &mut String, context: &TraceLogContext) {
    if let Some(close) = line.rfind('}') {
        line.insert_str(
            close,
            &format!(
                r#","trace_id":"{}","span_id":"{}""#,
                context.trace_id, context.span_id
            ),
        );
    }
}

/// One `Resource` for every signal the server exports, so `service.name`,
/// `deployment.environment`, `service.version` and `vnote.protocol` cannot
/// drift between a span and the log line written inside it.
///
/// `deployment.environment`, not semconv's `deployment.environment.name`: the
/// dashboard filter, the client Alloy fixture and stored queries all use this
/// name (see the telemetry deviation record in `AGENTS.md`).
pub(crate) fn telemetry_resource(observability: &ObservabilityConfig) -> Resource {
    Resource::builder()
        .with_service_name(observability.service_name.clone())
        .with_attributes([
            KeyValue::new("deployment.environment", observability.environment.clone()),
            KeyValue::new("service.version", crate::app_version()),
            KeyValue::new("vnote.protocol", PROTOCOL_VERSION),
        ])
        .build()
}

/// The OTLP providers for `observability`: both absent when no endpoint is
/// configured, and each built independently otherwise, so a log exporter that
/// fails to build costs the logs and nothing else. A failure is printed and
/// never fails startup.
fn build_otlp_providers(observability: &ObservabilityConfig) -> TelemetryGuard {
    build_providers(
        observability,
        |endpoint| {
            opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint.to_string())
                .with_timeout(observability.otlp_timeout())
                .build()
                .map_err(|error| error.to_string())
        },
        |endpoint| {
            opentelemetry_otlp::LogExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint.to_string())
                .with_timeout(observability.otlp_timeout())
                .build()
                .map_err(|error| error.to_string())
        },
    )
}

/// [`build_otlp_providers`] with the exporters injected, so the tests can hand
/// it in-memory ones, or a builder that fails.
pub(crate) fn build_providers<SE, LE>(
    observability: &ObservabilityConfig,
    span_exporter: impl FnOnce(&url::Url) -> Result<SE, String>,
    log_exporter: impl FnOnce(&url::Url) -> Result<LE, String>,
) -> TelemetryGuard
where
    SE: opentelemetry_sdk::trace::SpanExporter + 'static,
    LE: opentelemetry_sdk::logs::LogExporter + 'static,
{
    // No OTLP endpoint configured → both signals stay off.
    let Some(endpoint) = observability.otlp_endpoint.as_ref() else {
        return TelemetryGuard {
            tracer_provider: None,
            logger_provider: None,
        };
    };
    let resource = telemetry_resource(observability);

    let tracer_provider = match span_exporter(endpoint) {
        Ok(exporter) => Some(
            SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource.clone())
                .build(),
        ),
        Err(error) => {
            eprintln!("OpenTelemetry trace export disabled: {error}");
            None
        }
    };
    let logger_provider = match log_exporter(endpoint) {
        Ok(exporter) => Some(
            SdkLoggerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource)
                .build(),
        ),
        Err(error) => {
            eprintln!("OpenTelemetry log export disabled: {error}");
            None
        }
    };
    TelemetryGuard {
        tracer_provider,
        logger_provider,
    }
}

fn request_id_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    for header in [REQUEST_ID_HEADER, CORRELATION_ID_HEADER] {
        let Some(value) = headers.get(header).and_then(|value| value.to_str().ok()) else {
            continue;
        };
        let value = value.trim();
        if is_safe_request_id(value) {
            return Some(value.to_string());
        }
    }
    None
}

fn is_safe_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn generate_request_id() -> String {
    let mut buffer = [0u8; 16];
    rand::rng().fill_bytes(&mut buffer);
    format!(
        "req_{}",
        buffer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn normalized_route(path: &str) -> String {
    if path == "/metrics" {
        return "/metrics".to_string();
    }
    if path == "/health" || path == "/api/meta" || path == "/api/me" {
        return path.to_string();
    }
    if path == "/api/pages" {
        return path.to_string();
    }
    if path.starts_with("/api/pages/") && path.ends_with("/realtime") {
        return "/api/pages/{page_id}/realtime".to_string();
    }
    if path.starts_with("/api/pages/") {
        return "/api/pages/{page_id}".to_string();
    }
    if path.starts_with("/auth/") {
        return "/auth/*".to_string();
    }
    if path.starts_with("/.well-known/") {
        return "/.well-known/*".to_string();
    }
    // One bucket for the whole client telemetry ingress (#354), never one per
    // `{client}`/`{signal}`: those segments are caller-supplied, and the route
    // 404s an unknown one only *after* this label has been taken. The per-client
    // breakdown is `v_note_client_telemetry_requests_total`, labelled from
    // validated values. Without this the ingress would land in `/static/*` and
    // skew the static-asset latency series.
    if path == "/otlp" || path.starts_with("/otlp/") {
        return "/otlp/*".to_string();
    }
    "/static/*".to_string()
}

#[cfg(test)]
mod tests;

/// Captures the log events a test emits, so a test can assert that a code path
/// *says* something — the density regressions of #417 — without parsing stdout.
#[cfg(test)]
pub(crate) mod log_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt as _;

    /// One captured event: its level and every field as text, `message`
    /// included.
    #[derive(Clone, Debug)]
    pub(crate) struct Event {
        pub(crate) level: tracing::Level,
        pub(crate) fields: BTreeMap<String, String>,
    }

    impl Event {
        pub(crate) fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct Captured(Arc<Mutex<Vec<Event>>>);

    impl Captured {
        /// The events whose message is `message`.
        pub(crate) fn with_message(&self, message: &str) -> Vec<Event> {
            self.0
                .lock()
                .expect("capture lock")
                .iter()
                .filter(|event| event.field("message") == Some(message))
                .cloned()
                .collect()
        }

        /// The one event whose message is `message`.
        pub(crate) fn only(&self, message: &str) -> Event {
            let events = self.with_message(message);
            let [event] = events.as_slice() else {
                panic!("expected one {message:?} event, got {}", events.len());
            };
            event.clone()
        }
    }

    thread_local! {
        /// The capture the current test thread installed, if any.
        static ACTIVE: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
    }

    /// Forwards every event to the capturing test on the thread it fired on.
    struct Layer;

    struct Visitor<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let Some(captured) = ACTIVE.with(|active| active.borrow().clone()) else {
                return;
            };
            let mut fields = BTreeMap::new();
            event.record(&mut Visitor(&mut fields));
            captured.0.lock().expect("capture lock").push(Event {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    /// Stops capturing on this thread when dropped.
    pub(crate) struct CaptureGuard(());

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.borrow_mut().take());
        }
    }

    /// Captures the events this test thread emits until the guard drops. Hold
    /// it across the whole test: `#[tokio::test]` polls on the test's own
    /// thread, so awaits stay inside it.
    ///
    /// The subscriber is the process-wide *global* one, installed once, rather
    /// than a thread-local default. A thread-local default loses a race: a
    /// production callsite first reached by a parallel test while no subscriber
    /// was listening caches "never interested", and the line then never reaches
    /// the capture — a flake that looks exactly like the line being missing.
    /// The global subscriber is always interested, so no callsite is ever
    /// cached as disabled; events go to the capture of the thread they fired on.
    pub(crate) fn capture() -> (CaptureGuard, Captured) {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Layer))
                .expect("no other test installs a global subscriber");
        });
        let captured = Captured::default();
        ACTIVE.with(|active| *active.borrow_mut() = Some(captured.clone()));
        (CaptureGuard(()), captured)
    }
}
