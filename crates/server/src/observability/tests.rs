use super::{Metrics, PROTOCOL_VERSION, normalized_route, request_span, trace_context_from_span};

/// `PROTOCOL_VERSION` is duplicated across five places, and this one is a
/// bare `&str` with no compile-time link to the canonical constant. Without
/// this test a bump silently leaves every metric and span labelled with the
/// previous protocol version.
#[test]
fn protocol_version_label_matches_protocol() {
    assert_eq!(PROTOCOL_VERSION, protocol::PROTOCOL_VERSION.to_string());
}

// The tests below build their own `Metrics` (its own registry) rather than
// reading the process-wide one, so exact counts hold under parallel tests.

/// Runs `check` with an OpenTelemetry layer installed, as in production, so
/// spans carry real trace context. The provider has no exporter.
fn with_otel_layer(check: impl FnOnce()) {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    tracing::subscriber::with_default(subscriber, check);
}

const TRACEPARENT_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const TRACEPARENT_SPAN_ID: &str = "00f067aa0ba902b7";

fn headers_with_traceparent(value: &'static str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("traceparent", axum::http::HeaderValue::from_static(value));
    headers
}

#[test]
fn request_span_joins_the_trace_in_traceparent() {
    with_otel_layer(|| {
        let headers =
            headers_with_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        let span = request_span(&axum::http::Method::GET, "/api/pages", "req_1", &headers);

        let context = trace_context_from_span(&span).expect("span should carry a trace");
        // Same trace as the edge (Traefik) span, but its own span id: a child.
        assert_eq!(context.trace_id, TRACEPARENT_TRACE_ID);
        assert_ne!(context.span_id, TRACEPARENT_SPAN_ID);
    });
}

#[test]
fn db_query_span_names_the_operation_and_call_site() {
    with_otel_layer(|| {
        let span = super::db_query_span!("COMMIT", "persist_batch");
        let metadata = span.metadata().expect("span should be enabled");
        assert_eq!(metadata.name(), "db.query");
        for field in ["db.system", "db.operation", "db.query_name"] {
            assert!(metadata.fields().field(field).is_some(), "missing {field}");
        }
    });
}

/// #343: every `db.query` span in Tempo pointed at `observability.rs`, because
/// `db_query_span` was a function and `tracing` stamps a span with the location
/// of the `info_span!` that built it. This file is not `observability.rs`, so a
/// span built here must say so — in the exported attributes, which is what
/// Tempo shows.
#[test]
fn db_query_span_is_exported_with_its_call_site_location() {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let call_site_line = tracing::subscriber::with_default(subscriber, || {
        let _span = super::db_query_span!("SELECT", "call_site_probe");
        line!() - 1
    });

    let spans = exporter.get_finished_spans().expect("spans should export");
    let [span] = spans.as_slice() else {
        panic!("expected one exported span, got {}", spans.len());
    };
    assert_eq!(span.name, "db.query");
    let attribute = |key: &str| {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .unwrap_or_else(|| panic!("missing {key}"))
            .value
            .to_string()
    };
    assert_eq!(attribute("code.file.path"), file!());
    assert_eq!(attribute("code.module.name"), module_path!());
    assert_eq!(
        attribute("code.line.number"),
        call_site_line.to_string(),
        "the span should report the line it was opened on; if `db_query_span!` \
         above no longer sits one line before the `line!()`, fix the offset",
    );
    assert_eq!(attribute("db.query_name"), "call_site_probe");
}

#[test]
fn result_size_counts_rows_total_and_widest_row() {
    let mut size = super::ResultSize::default();
    for row_bytes in [120, 4_096, 0, 512] {
        size.add_row(row_bytes);
    }
    assert_eq!(
        size,
        super::ResultSize {
            rows: 4,
            bytes: 4_728,
            max_row_bytes: 4_096,
            affected_rows: None,
        }
    );

    let mut write = super::ResultSize::default();
    write.add_affected(3);
    write.add_affected(0);
    assert_eq!(write.rows, 0);
    assert_eq!(write.affected_rows, Some(3));
}

/// Every Postgres call site gets a `db.query` span, or its time is invisible
/// in Tempo — which is how `get_thumbnail` and the whole realtime/thumbnail
/// write path went untraced. Counts call sites in the non-test source.
#[test]
fn every_postgres_call_site_has_a_db_span() {
    for (file, source) in [
        ("routes/pages.rs", include_str!("../routes/pages.rs")),
        ("realtime/store.rs", include_str!("../realtime/store.rs")),
        ("thumbnails.rs", include_str!("../thumbnails.rs")),
    ] {
        let code = source.split("#[cfg(test)]").next().unwrap_or(source);
        let tx_spans = code.matches("db_query_span!(\"BEGIN\"").count()
            + code.matches("db_query_span!(\"COMMIT\"").count();
        let query_spans = code.matches(".instrument(db_query_span!(").count() - tx_spans;
        let queries = code.matches("sqlx::query").count();
        assert_eq!(
            queries, query_spans,
            "{file}: {queries} queries, {query_spans} db spans"
        );
        let metered = code.matches("(metered(").count();
        assert_eq!(
            queries, metered,
            "{file}: {queries} queries, {metered} metered executors"
        );
        let transactions = code.matches(".begin()").count() + code.matches(".commit()").count();
        assert_eq!(
            transactions, tx_spans,
            "{file}: {transactions} BEGIN/COMMIT, {tx_spans} db spans"
        );
    }
}

#[test]
fn request_span_without_a_valid_traceparent_starts_a_new_trace() {
    with_otel_layer(|| {
        for headers in [
            axum::http::HeaderMap::new(),
            headers_with_traceparent("not-a-traceparent"),
            headers_with_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
        ] {
            let span = request_span(&axum::http::Method::GET, "/health", "req_2", &headers);
            let context = trace_context_from_span(&span).expect("span should carry a trace");
            assert_ne!(context.trace_id, TRACEPARENT_TRACE_ID);
            assert_ne!(context.trace_id, "00000000000000000000000000000000");
        }
    });
}

#[test]
fn realtime_message_bytes_are_bucketed_by_channel_and_message_type() {
    let metrics = Metrics::new();
    metrics.observe_realtime_message_bytes("page", "synced", 50);
    metrics.observe_realtime_message_bytes("page", "stroke-batch", 2_300);
    metrics.observe_realtime_message_bytes("library", "page-updated", 120);

    let synced = metrics
        .realtime
        .message_bytes
        .with_label_values(&["page", "synced"]);
    assert_eq!(synced.get_sample_count(), 1);
    assert_eq!(synced.get_sample_sum(), 50.0);
    assert_eq!(
        metrics
            .realtime
            .message_bytes
            .with_label_values(&["library", "page-updated"])
            .get_sample_count(),
        1
    );

    let text = metrics.render().expect("metrics should render");
    assert!(text.contains(
        r#"v_note_realtime_message_bytes_bucket{channel="page",message_type="synced",le="128"} 1"#
    ));
    assert!(text.contains(
        r#"v_note_realtime_message_bytes_bucket{channel="page",message_type="stroke-batch",le="2048"} 0"#
    ));
    assert!(text.contains(
        r#"v_note_realtime_message_bytes_bucket{channel="page",message_type="stroke-batch",le="8192"} 1"#
    ));
}

#[test]
fn replay_cost_is_one_observation_per_replay() {
    let metrics = Metrics::new();
    // The DensePageSeeder reference page as #323 measured it *before*
    // coalescing: 1200 batch frames plus `synced`, 2.74 MB. Kept as the
    // regression shape — this is what the counters must still be able to show.
    metrics.observe_realtime_replay(1201, 2_740_000, 0.8);

    assert_eq!(metrics.realtime.replay_frames.get_sample_count(), 1);
    assert_eq!(metrics.realtime.replay_frames.get_sample_sum(), 1201.0);
    assert_eq!(metrics.realtime.replay_bytes.get_sample_sum(), 2_740_000.0);
    assert_eq!(
        metrics.realtime.replay_duration_seconds.get_sample_count(),
        1
    );

    // Today's dense replay lands below the top bucket, so its p95 is a real
    // number rather than +Inf — the before-number #323 needs.
    let text = metrics.render().expect("metrics should render");
    assert!(text.contains(r#"v_note_realtime_replay_bytes_bucket{le="2097152"} 0"#));
    assert!(text.contains(r#"v_note_realtime_replay_bytes_bucket{le="8388608"} 1"#));
    assert!(text.contains(r#"v_note_realtime_replay_frames_bucket{le="1000"} 0"#));
    assert!(text.contains(r#"v_note_realtime_replay_frames_bucket{le="2500"} 1"#));
}

/// After #323 every replay is one frame, so the `frames` histogram must be able
/// to say so exactly — a bottom bucket of `le="1"` is what distinguishes a
/// coalesced replay from a two-frame one.
#[test]
fn a_coalesced_replay_is_one_frame_in_the_bottom_bucket() {
    let metrics = Metrics::new();
    metrics.observe_realtime_replay(1, 7_296_892, 0.08);

    assert_eq!(metrics.realtime.replay_frames.get_sample_sum(), 1.0);
    let text = metrics.render().expect("metrics should render");
    assert!(text.contains(r#"v_note_realtime_replay_frames_bucket{le="1"} 1"#));
    // The bytes are unchanged by coalescing — the same ink, one envelope.
    assert!(text.contains(r#"v_note_realtime_replay_bytes_bucket{le="2097152"} 0"#));
    assert!(text.contains(r#"v_note_realtime_replay_bytes_bucket{le="8388608"} 1"#));
}

#[test]
fn message_handling_is_labelled_by_inbound_type_only() {
    let metrics = Metrics::new();
    metrics.observe_realtime_message_handling("commit-batch", 0.012);
    metrics.observe_realtime_message_handling("commit-batch", 0.003);
    metrics.observe_realtime_message_handling("subscribe", 0.2);

    let commit_batch = metrics
        .realtime
        .message_handling_seconds
        .with_label_values(&["commit-batch"]);
    assert_eq!(commit_batch.get_sample_count(), 2);
    assert!((commit_batch.get_sample_sum() - 0.015).abs() < 1e-9);

    let text = metrics.render().expect("metrics should render");
    assert!(
        text.contains(
            r#"v_note_realtime_message_handling_seconds_count{message_type="subscribe"} 1"#
        )
    );
}

#[test]
fn lagged_is_a_recorded_realtime_result() {
    let metrics = Metrics::new();
    metrics.record_realtime_event("page", "lagged");
    metrics.record_realtime_event("library", "lagged");

    assert_eq!(
        metrics
            .realtime
            .events_total
            .with_label_values(&["page", "lagged"])
            .get(),
        1
    );
    let text = metrics.render().expect("metrics should render");
    assert!(text.contains(r#"v_note_realtime_events_total{channel="library",result="lagged"} 1"#));
}

/// The `route` label is taken before routing, from the raw path — so for the
/// `/otlp` ingress (#354) it is taken before an unknown `{client}` has been
/// 404'd. Every path under it must therefore collapse to one value, or anyone
/// who can reach the server can mint a time series per request.
#[test]
fn every_otlp_path_shares_one_route_label() {
    for path in [
        "/otlp/spa/v1/traces",
        "/otlp/android/v1/logs",
        "/otlp/attacker-chosen-0001/v1/traces",
        "/otlp/spa/v1/attacker-chosen-0002",
        "/otlp/",
        "/otlp",
    ] {
        assert_eq!(normalized_route(path), "/otlp/*", "{path}");
    }
}

/// …and it must not swallow its neighbours. `/otlpx` is not the ingress, and
/// before #354 the ingress's own traffic was counted as static assets, which is
/// the series this keeps clean.
#[test]
fn the_otlp_route_label_does_not_capture_other_paths() {
    assert_eq!(normalized_route("/otlpx/spa/v1/traces"), "/static/*");
    assert_eq!(normalized_route("/assets/otlp/app.js"), "/static/*");
    assert_eq!(normalized_route("/api/pages"), "/api/pages");
}

// ---------------------------------------------------------------------------
// OTLP logs and trace correlation (#417)
// ---------------------------------------------------------------------------

mod otlp_logs {
    use std::sync::{Arc, Mutex};

    use opentelemetry::Key;
    use opentelemetry::logs::AnyValue;
    use opentelemetry::trace::TraceContextExt as _;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    use url::Url;

    use super::super::{
        DEFAULT_LOG_FILTER, LogFilters, PROTOCOL_VERSION, TelemetryGuard, build_providers,
        request_span, telemetry_resource, telemetry_subscriber,
    };
    use crate::config::ObservabilityConfig;

    fn observability(endpoint: Option<&str>) -> ObservabilityConfig {
        ObservabilityConfig {
            environment: "unit-test".to_string(),
            otlp_endpoint: endpoint.map(|value| Url::parse(value).expect("test endpoint")),
            otlp_protocol: "grpc".to_string(),
            otlp_timeout_ms: 2000,
            service_name: "v-note".to_string(),
            metrics_addr: "disabled".to_string(),
            otlp_log_filter: None,
        }
    }

    const ENDPOINT: &str = "http://collector.invalid:4317";

    /// Stdout as the `fmt` layer writes it: one JSON object per line.
    #[derive(Clone, Default)]
    struct Stdout(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Stdout {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("stdout lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Stdout {
        fn lines(&self) -> Vec<serde_json::Value> {
            let bytes = self.0.lock().expect("stdout lock").clone();
            String::from_utf8(bytes)
                .expect("stdout is utf-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
                .collect()
        }

        fn line(&self, message: &str) -> serde_json::Value {
            self.lines()
                .into_iter()
                .find(|line| line["message"] == message)
                .unwrap_or_else(|| panic!("no stdout line {message:?}"))
        }
    }

    /// The production stack (`telemetry_subscriber`) over in-memory exporters.
    struct Harness {
        guard: TelemetryGuard,
        stdout: Stdout,
        filters: LogFilters,
        logs: InMemoryLogExporter,
        spans: InMemorySpanExporter,
    }

    impl Harness {
        fn new(filters: LogFilters) -> Self {
            let logs = InMemoryLogExporter::default();
            let spans = InMemorySpanExporter::default();
            let guard = build_providers(
                &observability(Some(ENDPOINT)),
                {
                    let spans = spans.clone();
                    |_: &Url| Ok(spans)
                },
                {
                    let logs = logs.clone();
                    |_: &Url| Ok(logs)
                },
            );
            Self {
                guard,
                stdout: Stdout::default(),
                filters,
                logs,
                spans,
            }
        }

        fn exported_spans(&self) -> Vec<opentelemetry_sdk::trace::SpanData> {
            self.guard
                .tracer_provider
                .as_ref()
                .expect("tracer provider")
                .force_flush()
                .expect("spans should flush");
            self.spans
                .get_finished_spans()
                .expect("spans should export")
        }

        fn run<T>(&self, check: impl FnOnce() -> T) -> T {
            let stdout = self.stdout.clone();
            let subscriber = telemetry_subscriber(
                &self.filters,
                move || stdout.clone(),
                self.guard.tracer_provider.as_ref(),
                self.guard.logger_provider.as_ref(),
            );
            tracing::subscriber::with_default(subscriber, || {
                // Production callsites (the request-completed line) may have
                // been first reached by a parallel test with no subscriber, and
                // cached as uninteresting; recompute with this one installed.
                tracing::callsite::rebuild_interest_cache();
                check()
            })
        }

        fn exported_logs(&self) -> Vec<LogDataWithResource> {
            self.guard
                .logger_provider
                .as_ref()
                .expect("logger provider")
                .force_flush()
                .expect("logs should flush");
            self.logs.get_emitted_logs().expect("logs should export")
        }

        fn exported_bodies(&self) -> Vec<AnyValue> {
            self.exported_logs()
                .into_iter()
                .filter_map(|log| log.record.body().cloned())
                .collect()
        }

        fn exported_log(&self, message: &str) -> LogDataWithResource {
            self.exported_logs()
                .into_iter()
                .find(|log| log.record.body() == Some(&text(message)))
                .unwrap_or_else(|| panic!("no exported log record {message:?}"))
        }
    }

    fn text(message: &str) -> AnyValue {
        AnyValue::String(message.to_string().into())
    }

    fn default_filters() -> LogFilters {
        LogFilters::new(None, None)
    }

    fn ids(span: &tracing::Span) -> (String, String) {
        let context = span.context();
        let span_context = context.span().span_context().clone();
        (
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        )
    }

    fn a_request_span() -> tracing::Span {
        request_span(
            &axum::http::Method::POST,
            "/api/pages",
            "req_417",
            &axum::http::HeaderMap::new(),
        )
    }

    #[test]
    fn no_endpoint_builds_neither_provider() {
        let guard = build_providers(
            &observability(None),
            |_: &Url| -> Result<InMemorySpanExporter, String> {
                panic!("no span exporter without an endpoint")
            },
            |_: &Url| -> Result<InMemoryLogExporter, String> {
                panic!("no log exporter without an endpoint")
            },
        );
        assert!(!guard.exports_traces());
        assert!(!guard.exports_logs());
    }

    #[test]
    fn an_endpoint_builds_both_providers() {
        let harness = Harness::new(default_filters());
        assert!(harness.guard.exports_traces());
        assert!(harness.guard.exports_logs());
    }

    /// A log exporter that cannot be built costs the logs and nothing else: the
    /// trace exporter still exports and stdout still logs.
    #[test]
    fn a_failed_log_exporter_leaves_traces_and_stdout_working() {
        let spans = InMemorySpanExporter::default();
        let guard = build_providers(
            &observability(Some(ENDPOINT)),
            {
                let spans = spans.clone();
                |_: &Url| Ok(spans)
            },
            |_: &Url| -> Result<InMemoryLogExporter, String> {
                Err("collector said no".to_string())
            },
        );
        assert!(guard.exports_traces());
        assert!(!guard.exports_logs());

        let stdout = Stdout::default();
        let subscriber = telemetry_subscriber(
            &default_filters(),
            {
                let stdout = stdout.clone();
                move || stdout.clone()
            },
            guard.tracer_provider.as_ref(),
            guard.logger_provider.as_ref(),
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("work");
            span.in_scope(|| tracing::info!("still logging"));
        });

        assert_eq!(stdout.line("still logging")["level"], "INFO");
        guard
            .tracer_provider
            .as_ref()
            .expect("tracer provider")
            .force_flush()
            .expect("spans should flush");
        let spans = spans.get_finished_spans().expect("spans should export");
        assert!(spans.iter().any(|span| span.name == "work"));
    }

    /// …and the reverse: a span exporter that fails does not take logs down.
    #[test]
    fn a_failed_span_exporter_leaves_logs_working() {
        let guard = build_providers(
            &observability(Some(ENDPOINT)),
            |_: &Url| -> Result<InMemorySpanExporter, String> { Err("no".to_string()) },
            |_: &Url| Ok(InMemoryLogExporter::default()),
        );
        assert!(!guard.exports_traces());
        assert!(guard.exports_logs());
    }

    #[test]
    fn dropping_the_guard_shuts_down_both_providers() {
        let spans = InMemorySpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let guard = build_providers(
            &observability(Some(ENDPOINT)),
            {
                let spans = spans.clone();
                |_: &Url| Ok(spans)
            },
            {
                let logs = logs.clone();
                |_: &Url| Ok(logs)
            },
        );
        drop(guard);
        assert!(spans.is_shutdown_called(), "span provider not shut down");
        assert!(logs.is_shutdown_called(), "log provider not shut down");
    }

    /// Records the resource each provider hands its exporter, so the test can
    /// see what the SDK would put on the wire for *both* signals.
    #[derive(Clone, Debug, Default)]
    struct ResourceProbe(Arc<Mutex<Option<Resource>>>);

    impl ResourceProbe {
        fn resource(&self) -> Option<Resource> {
            self.0.lock().expect("probe lock").clone()
        }
    }

    impl opentelemetry_sdk::trace::SpanExporter for ResourceProbe {
        async fn export(&self, _batch: Vec<opentelemetry_sdk::trace::SpanData>) -> OTelSdkResult {
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.0.lock().expect("probe lock") = Some(resource.clone());
        }
    }

    impl opentelemetry_sdk::logs::LogExporter for ResourceProbe {
        async fn export(&self, _batch: opentelemetry_sdk::logs::LogBatch<'_>) -> OTelSdkResult {
            Ok(())
        }

        fn set_resource(&mut self, resource: &Resource) {
            *self.0.lock().expect("probe lock") = Some(resource.clone());
        }
    }

    #[test]
    fn tracer_and_logger_share_one_resource() {
        let config = observability(Some(ENDPOINT));
        let span_probe = ResourceProbe::default();
        let log_probe = ResourceProbe::default();
        let guard = build_providers(
            &config,
            {
                let probe = span_probe.clone();
                |_: &Url| Ok(probe)
            },
            {
                let probe = log_probe.clone();
                |_: &Url| Ok(probe)
            },
        );
        // The batch processors hand the resource over on their own threads; a
        // flush is a round trip through each, so it has arrived by the time
        // this returns.
        guard
            .tracer_provider
            .as_ref()
            .expect("tracer provider")
            .force_flush()
            .expect("spans should flush");
        guard
            .logger_provider
            .as_ref()
            .expect("logger provider")
            .force_flush()
            .expect("logs should flush");
        assert_eq!(span_probe.resource(), Some(telemetry_resource(&config)));
        assert_eq!(log_probe.resource(), span_probe.resource());
    }

    /// Asserted on the exported record, not on config: this is what a
    /// collector receives.
    #[test]
    fn exported_log_records_carry_the_resource_attributes() {
        let harness = Harness::new(default_filters());
        harness.run(|| tracing::info!("resource probe"));
        let log = harness.exported_log("resource probe");

        let attribute = |key: &'static str| {
            log.resource
                .get(&Key::from_static_str(key))
                .unwrap_or_else(|| panic!("missing resource attribute {key}"))
                .to_string()
        };
        assert_eq!(attribute("service.name"), "v-note");
        assert_eq!(attribute("service.version"), crate::app_version());
        assert_eq!(attribute("deployment.environment"), "unit-test");
        assert_eq!(attribute("vnote.protocol"), PROTOCOL_VERSION);
        // The path marker: the shared Alloy copies it into the indexed
        // `log_source`, and sets none of its own.
        assert_eq!(attribute("telemetry_source"), "otlp");
    }

    #[test]
    fn an_exported_log_inside_a_request_span_carries_its_trace_and_span_ids() {
        let harness = Harness::new(default_filters());
        let (trace_id, span_id) = harness.run(|| {
            let span = a_request_span();
            span.in_scope(|| tracing::info!("inside the request"));
            ids(&span)
        });

        let log = harness.exported_log("inside the request");
        let context = log
            .record
            .trace_context()
            .expect("a record written inside a span carries its context");
        assert_eq!(context.trace_id.to_string(), trace_id);
        assert_eq!(context.span_id.to_string(), span_id);
    }

    /// The real middleware, not a synthetic event: its request-completed line
    /// used to write `trace_id` / `span_id` by hand, and now takes them from
    /// the active context like every other line. Both paths must still name
    /// the exported `http.request` span.
    #[test]
    fn the_request_completed_line_names_its_http_request_span_on_both_paths() {
        use tower::ServiceExt as _;

        let harness = Harness::new(default_filters());
        harness.run(|| {
            let app = axum::Router::new()
                .route("/health", axum::routing::get(|| async { "ok" }))
                .layer(axum::middleware::from_fn(
                    super::super::request_observability_middleware,
                ));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime should build");
            let response = runtime
                .block_on(
                    app.oneshot(
                        axum::http::Request::builder()
                            .uri("/health")
                            .body(axum::body::Body::empty())
                            .expect("request should build"),
                    ),
                )
                .expect("request should succeed");
            assert_eq!(response.status(), axum::http::StatusCode::OK);
        });

        let spans = harness.exported_spans();
        let request = spans
            .iter()
            .find(|span| span.name == "http.request")
            .expect("an exported http.request span");
        let trace_id = request.span_context.trace_id().to_string();
        let span_id = request.span_context.span_id().to_string();

        let log = harness.exported_log("http request completed");
        let context = log
            .record
            .trace_context()
            .expect("the OTLP record is correlated");
        assert_eq!(context.trace_id.to_string(), trace_id);
        assert_eq!(context.span_id.to_string(), span_id);

        let line = harness.stdout.line("http request completed");
        assert_eq!(line["trace_id"], trace_id.as_str());
        assert_eq!(line["span_id"], span_id.as_str());
        assert_eq!(line["route"], "/health");
    }

    #[test]
    fn an_exported_log_outside_any_span_carries_no_trace_context() {
        let harness = Harness::new(default_filters());
        harness.run(|| tracing::info!("outside any span"));
        let log = harness.exported_log("outside any span");
        assert!(log.record.trace_context().is_none());
    }

    /// The stdout copy is a different pipeline from the OTLP one, so it gets
    /// its own proof: the same two cases against the JSON line.
    #[test]
    fn a_stdout_line_inside_a_request_span_carries_its_trace_and_span_ids() {
        let harness = Harness::new(default_filters());
        let (trace_id, span_id) = harness.run(|| {
            let span = a_request_span();
            span.in_scope(|| tracing::info!(page_id = "page_1", "inside the request"));
            ids(&span)
        });

        let line = harness.stdout.line("inside the request");
        assert_eq!(line["trace_id"], trace_id.as_str());
        assert_eq!(line["span_id"], span_id.as_str());
        // Still a flat object: the event's own fields and the span's
        // `request_id` survive the injection.
        assert_eq!(line["page_id"], "page_1");
        assert_eq!(line["span"]["request_id"], "req_417");
    }

    #[test]
    fn a_stdout_line_outside_any_span_carries_no_trace_ids() {
        let harness = Harness::new(default_filters());
        harness.run(|| tracing::info!("outside any span"));
        let line = harness.stdout.line("outside any span");
        assert!(line.get("trace_id").is_none(), "{line}");
        assert!(line.get("span_id").is_none(), "{line}");
    }

    /// `otlp-log-filter` governs the OTLP layer alone: Loki at `warn` while
    /// stdout stays at `info`, and the reverse.
    #[test]
    fn the_otlp_log_filter_is_independent_of_stdout() {
        let quieter = Harness::new(LogFilters::new(None, Some("server=warn")));
        quieter.run(|| {
            tracing::info!("routine");
            tracing::warn!("notable");
        });
        let exported = quieter.exported_bodies();
        assert!(exported.contains(&text("notable")));
        assert!(!exported.contains(&text("routine")));
        quieter.stdout.line("routine");
        quieter.stdout.line("notable");

        let louder = Harness::new(LogFilters::new(Some("server=info"), Some("server=debug")));
        louder.run(|| tracing::debug!("detail"));
        assert!(louder.exported_bodies().contains(&text("detail")));
        assert!(
            louder
                .stdout
                .lines()
                .iter()
                .all(|line| line["message"] != "detail")
        );
    }

    #[test]
    fn log_filters_default_to_the_stdout_filter() {
        let defaults = LogFilters::new(None, None);
        assert_eq!(defaults.stdout, DEFAULT_LOG_FILTER);
        assert_eq!(defaults.otlp_logs, defaults.stdout);

        let from_rust_log = LogFilters::new(Some("server=debug"), Some("  "));
        assert_eq!(from_rust_log.stdout, "server=debug");
        assert_eq!(from_rust_log.otlp_logs, "server=debug");

        let split = LogFilters::new(Some("server=debug"), Some("server=warn"));
        assert_eq!(split.otlp_logs, "server=warn");
    }

    /// Exporting a batch must never produce a record to export: the SDK's own
    /// reports and the transport's stay on stdout — even when the OTLP filter
    /// would admit everything, and even when an operator names them more
    /// specifically than any built-in directive could.
    #[test]
    fn the_exporters_own_events_never_reach_the_otlp_layer() {
        for otlp_filter in [
            "trace",
            "server=info,opentelemetry_sdk=debug,opentelemetry_otlp=trace,tonic=trace,tower=trace",
        ] {
            let harness = Harness::new(LogFilters::new(Some("trace"), Some(otlp_filter)));
            harness.run(|| {
                tracing::warn!(target: "opentelemetry_sdk", "sdk: export failed");
                tracing::warn!(target: "opentelemetry_otlp::exporter", "otlp: export failed");
                tracing::warn!(target: "tonic::transport", "tonic: connect failed");
                tracing::warn!(target: "tower::buffer", "tower: overloaded");
                tracing::warn!(target: "server::probe", "product line");
            });
            let exported = harness.exported_bodies();
            assert!(exported.contains(&text("product line")), "{otlp_filter}");
            for internal in [
                "sdk: export failed",
                "otlp: export failed",
                "tonic: connect failed",
                "tower: overloaded",
            ] {
                assert!(
                    !exported.contains(&text(internal)),
                    "{otlp_filter}: {internal}"
                );
                harness.stdout.line(internal);
            }
        }
    }

    #[test]
    fn exporter_internal_targets_are_matched_by_crate_not_prefix() {
        for target in [
            "opentelemetry",
            "opentelemetry_sdk::logs",
            "opentelemetry-otlp",
            "tonic::transport",
            "h2::proto",
            "hyper",
            "hyper_util::client",
            "tower::buffer",
        ] {
            assert!(super::super::is_exporter_internal(target), "{target}");
        }
        for target in ["tower_http::trace", "server::observability", "h2o", "axum"] {
            assert!(!super::super::is_exporter_internal(target), "{target}");
        }
    }
}
