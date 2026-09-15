use super::{Metrics, PROTOCOL_VERSION, request_span, trace_context_from_span};

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
        let span = super::db_query_span("COMMIT", "persist_batch");
        let metadata = span.metadata().expect("span should be enabled");
        assert_eq!(metadata.name(), "db.query");
        for field in ["db.system", "db.operation", "db.query_name"] {
            assert!(metadata.fields().field(field).is_some(), "missing {field}");
        }
    });
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
        let tx_spans = code.matches("db_query_span(\"BEGIN\"").count()
            + code.matches("db_query_span(\"COMMIT\"").count();
        let query_spans = code.matches(".instrument(db_query_span(").count() - tx_spans;
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
    // The DensePageSeeder reference page as #323 measured it: 1200 batch
    // frames plus `synced`, 2.74 MB.
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
