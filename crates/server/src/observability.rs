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
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::ObservabilityConfig;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";
/// Metric/trace label form of [`protocol::PROTOCOL_VERSION`]. A bare `&str`
/// because both consumers want a `'static` label, so there is no compile-time
/// link to the canonical constant — `protocol_version_label_matches_protocol`
/// below is that link.
const PROTOCOL_VERSION: &str = "5";

/// Realtime frame and replay sizes: from a ~50 B `synced` up to a multi-MB
/// replay (the `DensePageSeeder` reference page replays ~2.7 MB today).
const REALTIME_BYTES_BUCKETS: [f64; 9] = [
    128.0, 512.0, 2048.0, 8192.0, 32768.0, 131072.0, 524288.0, 2097152.0, 8388608.0,
];
/// Frames per replay: one per stored stroke batch and tombstone batch, plus the
/// closing `synced`. The dense reference page sends ~1200; #323 drives it to 1.
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
    http_requests_total: IntCounterVec,
    http_request_duration_seconds: HistogramVec,
    auth_failures_total: IntCounterVec,
    page_mutations_total: IntCounterVec,
    thumbnail_generation_duration_seconds: HistogramVec,
    thumbnail_queue_depth: IntGauge,
    thumbnail_recoveries_total: IntCounterVec,
    thumbnail_artifact_bytes: Histogram,
    realtime_events_total: IntCounterVec,
    realtime_active_connections: IntGauge,
    realtime_message_bytes: HistogramVec,
    realtime_replay_bytes: Histogram,
    realtime_replay_frames: Histogram,
    realtime_replay_duration_seconds: Histogram,
    realtime_message_handling_seconds: HistogramVec,
    _build_info: IntGauge,
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new();
        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "v_note_http_requests_total",
                "HTTP requests by route and status",
            ),
            &["method", "route", "status"],
        )
        .expect("http request counter should build");
        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "v_note_http_request_duration_seconds",
                "HTTP request latency by route and status",
            ),
            &["method", "route", "status"],
        )
        .expect("http request histogram should build");
        let auth_failures_total = IntCounterVec::new(
            Opts::new(
                "v_note_auth_failures_total",
                "Authentication failures by reason",
            ),
            &["reason"],
        )
        .expect("auth failure counter should build");
        let page_mutations_total = IntCounterVec::new(
            Opts::new(
                "v_note_page_mutations_total",
                "Page and ink mutations by operation/result",
            ),
            &["operation", "result"],
        )
        .expect("page mutation counter should build");
        let thumbnail_generation_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "v_note_thumbnail_generation_duration_seconds",
                "Thumbnail generation duration by result",
            ),
            &["result"],
        )
        .expect("thumbnail generation histogram should build");
        let thumbnail_queue_depth = IntGauge::new(
            "v_note_thumbnail_queue_depth",
            "Thumbnail generations awaiting completion",
        )
        .expect("thumbnail queue gauge should build");
        let thumbnail_recoveries_total = IntCounterVec::new(
            Opts::new(
                "v_note_thumbnail_recoveries_total",
                "Thumbnail generation recovery attempts by result",
            ),
            &["result"],
        )
        .expect("thumbnail recovery counter should build");
        let thumbnail_artifact_bytes = Histogram::with_opts(
            HistogramOpts::new(
                "v_note_thumbnail_artifact_bytes",
                "Stored thumbnail PNG size in bytes",
            )
            .buckets(vec![
                256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0,
            ]),
        )
        .expect("thumbnail artifact histogram should build");
        let realtime_events_total = IntCounterVec::new(
            Opts::new(
                "v_note_realtime_events_total",
                "Realtime WebSocket events by channel and result",
            ),
            &["channel", "result"],
        )
        .expect("realtime event counter should build");
        let realtime_active_connections = IntGauge::new(
            "v_note_realtime_active_connections",
            "Currently open realtime WebSocket connections",
        )
        .expect("active realtime gauge should build");
        // Cardinality budget: `channel` × `message_type`, both bounded enums.
        // Never add `page_id`, `session_id`, `owner_id` or `client_batch_id` —
        // those belong in span fields, not labels.
        let realtime_message_bytes = HistogramVec::new(
            HistogramOpts::new(
                "v_note_realtime_message_bytes",
                "Serialized size of realtime frames sent, by channel and message type",
            )
            .buckets(REALTIME_BYTES_BUCKETS.to_vec()),
            &["channel", "message_type"],
        )
        .expect("realtime message size histogram should build");
        let realtime_replay_bytes = Histogram::with_opts(
            HistogramOpts::new(
                "v_note_realtime_replay_bytes",
                "Total bytes sent for one page-channel subscribe replay",
            )
            .buckets(REALTIME_BYTES_BUCKETS.to_vec()),
        )
        .expect("realtime replay bytes histogram should build");
        let realtime_replay_frames = Histogram::with_opts(
            HistogramOpts::new(
                "v_note_realtime_replay_frames",
                "Frames sent for one page-channel subscribe replay, including the closing synced",
            )
            .buckets(REALTIME_REPLAY_FRAMES_BUCKETS.to_vec()),
        )
        .expect("realtime replay frames histogram should build");
        let realtime_replay_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "v_note_realtime_replay_duration_seconds",
                "Wall time from a subscribe being received to its synced being sent",
            )
            .buckets(REALTIME_REPLAY_DURATION_BUCKETS.to_vec()),
        )
        .expect("realtime replay duration histogram should build");
        let realtime_message_handling_seconds = HistogramVec::new(
            HistogramOpts::new(
                "v_note_realtime_message_handling_seconds",
                "Server-side handling time of inbound page-channel messages, by message type \
                 (not end-to-end latency)",
            )
            .buckets(REALTIME_HANDLING_BUCKETS.to_vec()),
            &["message_type"],
        )
        .expect("realtime message handling histogram should build");
        let build_info = IntGauge::with_opts(
            Opts::new("v_note_build_info", "v-note build and protocol metadata")
                .const_label("protocol", PROTOCOL_VERSION)
                .const_label("version", crate::app_version()),
        )
        .expect("build info gauge should build");
        build_info.set(1);

        for collector in [
            Box::new(http_requests_total.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(http_request_duration_seconds.clone()),
            Box::new(auth_failures_total.clone()),
            Box::new(page_mutations_total.clone()),
            Box::new(thumbnail_generation_duration_seconds.clone()),
            Box::new(thumbnail_queue_depth.clone()),
            Box::new(thumbnail_recoveries_total.clone()),
            Box::new(thumbnail_artifact_bytes.clone()),
            Box::new(realtime_events_total.clone()),
            Box::new(realtime_active_connections.clone()),
            Box::new(realtime_message_bytes.clone()),
            Box::new(realtime_replay_bytes.clone()),
            Box::new(realtime_replay_frames.clone()),
            Box::new(realtime_replay_duration_seconds.clone()),
            Box::new(realtime_message_handling_seconds.clone()),
            Box::new(build_info.clone()),
        ] {
            registry
                .register(collector)
                .expect("metric should register once");
        }

        Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            auth_failures_total,
            page_mutations_total,
            thumbnail_generation_duration_seconds,
            thumbnail_queue_depth,
            thumbnail_recoveries_total,
            thumbnail_artifact_bytes,
            realtime_events_total,
            realtime_active_connections,
            realtime_message_bytes,
            realtime_replay_bytes,
            realtime_replay_frames,
            realtime_replay_duration_seconds,
            realtime_message_handling_seconds,
            _build_info: build_info,
        }
    }

    pub fn record_auth_failure(&self, reason: &'static str) {
        self.auth_failures_total.with_label_values(&[reason]).inc();
    }

    pub fn record_page_mutation(&self, operation: &'static str, result: &'static str) {
        self.page_mutations_total
            .with_label_values(&[operation, result])
            .inc();
    }

    pub fn record_thumbnail_generation(&self, result: &'static str, elapsed_seconds: f64) {
        self.thumbnail_generation_duration_seconds
            .with_label_values(&[result])
            .observe(elapsed_seconds);
    }

    pub fn thumbnail_generation_queued(&self) {
        self.thumbnail_queue_depth.inc();
    }

    pub fn thumbnail_generation_finished(&self) {
        self.thumbnail_queue_depth.dec();
    }

    pub fn record_thumbnail_recovery(&self, result: &'static str) {
        self.thumbnail_recoveries_total
            .with_label_values(&[result])
            .inc();
    }

    pub fn observe_thumbnail_artifact_bytes(&self, bytes: usize) {
        self.thumbnail_artifact_bytes.observe(bytes as f64);
    }

    pub fn record_realtime_event(&self, channel: &'static str, result: &'static str) {
        self.realtime_events_total
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
        self.realtime_message_bytes
            .with_label_values(&[channel, message_type])
            .observe(bytes as f64);
    }

    /// One completed page-channel replay: every frame from the first
    /// `stroke-batch` to the closing `synced`.
    pub fn observe_realtime_replay(&self, frames: u64, bytes: u64, elapsed_seconds: f64) {
        self.realtime_replay_frames.observe(frames as f64);
        self.realtime_replay_bytes.observe(bytes as f64);
        self.realtime_replay_duration_seconds
            .observe(elapsed_seconds);
    }

    /// Server-side handling of one inbound page-channel message. Deliberately
    /// not end-to-end latency: that needs client timestamps (#154).
    pub fn observe_realtime_message_handling(
        &self,
        message_type: &'static str,
        elapsed_seconds: f64,
    ) {
        self.realtime_message_handling_seconds
            .with_label_values(&[message_type])
            .observe(elapsed_seconds);
    }

    #[cfg(test)]
    pub(crate) fn realtime_message_count(&self, channel: &str, message_type: &str) -> u64 {
        self.realtime_message_bytes
            .with_label_values(&[channel, message_type])
            .get_sample_count()
    }

    pub fn realtime_connection_guard(&self) -> RealtimeConnectionGuard {
        self.realtime_active_connections.inc();
        RealtimeConnectionGuard
    }

    fn record_http(&self, method: &str, route: &str, status: StatusCode, elapsed: f64) {
        let status = status.as_u16().to_string();
        self.http_requests_total
            .with_label_values(&[method, route, &status])
            .inc();
        self.http_request_duration_seconds
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
        metrics().realtime_active_connections.dec();
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
    let trace_context = trace_context_from_span(&span);

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value.clone());
        response
            .headers_mut()
            .insert(HeaderName::from_static(CORRELATION_ID_HEADER), value);
    }

    tracing::info!(
        method = %method,
        route = %route,
        status = status.as_u16(),
        latency_ms = (elapsed * 1000.0),
        request_id = %request_id,
        trace_id = trace_context
            .as_ref()
            .map(|context| context.trace_id.as_str())
            .unwrap_or(""),
        span_id = trace_context
            .as_ref()
            .map(|context| context.span_id.as_str())
            .unwrap_or(""),
        "http request completed",
    );

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
pub fn db_query_span(operation: &'static str, query_name: &'static str) -> tracing::Span {
    tracing::info_span!(
        "db.query",
        db.system = "postgresql",
        db.operation = operation,
        db.query_name = query_name,
        db.response.returned_rows = tracing::field::Empty,
        db.response.bytes = tracing::field::Empty,
        db.response.max_row_bytes = tracing::field::Empty,
        db.response.affected_rows = tracing::field::Empty,
    )
}

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

pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("OpenTelemetry shutdown failed: {error}");
        }
    }
}

pub fn init_tracing(observability: &ObservabilityConfig) -> TelemetryGuard {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("server=info,tower_http=info,axum=info"))
        .expect("default tracing filter should be valid");
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false);

    match build_tracer_provider(observability) {
        Ok(Some(provider)) => {
            let tracer = provider.tracer("v-note");
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .with(otel_layer)
                .init();
            TelemetryGuard {
                provider: Some(provider),
            }
        }
        Ok(None) => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .init();
            TelemetryGuard { provider: None }
        }
        Err(error) => {
            eprintln!("OpenTelemetry disabled: {error}");
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .init();
            TelemetryGuard { provider: None }
        }
    }
}

fn build_tracer_provider(
    observability: &ObservabilityConfig,
) -> Result<Option<SdkTracerProvider>, String> {
    // No OTLP endpoint configured → tracing export stays off.
    let Some(endpoint) = observability.otlp_endpoint.as_ref() else {
        return Ok(None);
    };

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.to_string())
        .with_timeout(observability.otlp_timeout())
        .build()
        .map_err(|error| error.to_string())?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(observability.service_name.clone())
        .with_attributes([
            KeyValue::new("deployment.environment", observability.environment.clone()),
            KeyValue::new("service.version", crate::app_version()),
            KeyValue::new("vnote.protocol", PROTOCOL_VERSION),
        ])
        .build();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    Ok(Some(provider))
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
    "/static/*".to_string()
}

#[cfg(test)]
mod tests;
