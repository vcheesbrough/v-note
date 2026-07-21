use std::env;
use std::net::SocketAddr;
use std::time::Duration;
use std::time::Instant;

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use once_cell::sync::Lazy;
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::SdkTracerProvider;
use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use rand::RngCore;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";
const PROTOCOL_VERSION: &str = "3";

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
        let build_info = IntGauge::with_opts(
            Opts::new("v_note_build_info", "v-note build and protocol metadata")
                .const_label("protocol", PROTOCOL_VERSION)
                .const_label("version", crate::app_version_from_env()),
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

    let span = tracing::info_span!(
        "http.request",
        method = %method,
        route = %route,
        request_id = %request_id,
    );
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

pub async fn run_metrics_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(addr) = metrics_addr() else {
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
        if let Some(provider) = self.provider.take() {
            if let Err(error) = provider.shutdown() {
                eprintln!("OpenTelemetry shutdown failed: {error}");
            }
        }
    }
}

pub fn init_tracing() -> TelemetryGuard {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("server=info,tower_http=info,axum=info"))
        .expect("default tracing filter should be valid");
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false);

    match build_tracer_provider() {
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

fn build_tracer_provider() -> Result<Option<SdkTracerProvider>, String> {
    let endpoint = match env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return Ok(None),
    };
    let protocol = env::var("OTEL_EXPORTER_OTLP_PROTOCOL").unwrap_or_else(|_| "grpc".to_string());
    if protocol != "grpc" {
        return Err(format!(
            "unsupported OTEL_EXPORTER_OTLP_PROTOCOL={protocol}; expected grpc"
        ));
    }

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(otel_export_timeout())
        .build()
        .map_err(|error| error.to_string())?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "v-note".to_string()))
        .with_attributes([
            KeyValue::new(
                "deployment.environment",
                env::var("APP_ENV").unwrap_or_else(|_| "dev".to_string()),
            ),
            KeyValue::new("service.version", crate::app_version_from_env()),
            KeyValue::new("vnote.protocol", PROTOCOL_VERSION),
        ])
        .build();
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    Ok(Some(provider))
}

fn otel_export_timeout() -> Duration {
    env::var("OTEL_EXPORTER_OTLP_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_secs(2))
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
    rand::thread_rng().fill_bytes(&mut buffer);
    format!(
        "req_{}",
        buffer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn metrics_addr() -> Option<SocketAddr> {
    match env::var("METRICS_ADDR") {
        Ok(value) if value.eq_ignore_ascii_case("disabled") || value.trim().is_empty() => None,
        Ok(value) => Some(value.parse().expect("METRICS_ADDR should be host:port")),
        Err(_) => Some("0.0.0.0:9090".parse().expect("default metrics addr")),
    }
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
