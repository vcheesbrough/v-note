//! The encoding is only *proved* by a real collector parsing it, which is what
//! `e2e/tests/client-telemetry.spec.ts` does. These pin the rules that are easy
//! to break without noticing, because each failure mode is quiet: the collector
//! answers 400 and the SPA — correctly — swallows export failures.

use serde_json::{Value, json};

use super::*;

const TRACE: [u8; 16] = [
    0x5b, 0x8e, 0xff, 0xf7, 0x98, 0x03, 0x81, 0x03, 0xd2, 0x69, 0xb6, 0x33, 0x81, 0x3f, 0xc6, 0x0c,
];
const SPAN: [u8; 8] = [0xee, 0xe1, 0x9b, 0x7e, 0xc3, 0xc1, 0xb1, 0x74];
const PARENT: [u8; 8] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x0a];

fn span() -> Span {
    Span {
        trace_id: TraceId::from_random(TRACE),
        span_id: SpanId::from_random(SPAN),
        parent_span_id: None,
        name: "page.open",
        kind: SpanKind::Internal,
        start_time_unix_nano: UnixNanos(1_758_400_000_000_000_000),
        end_time_unix_nano: UnixNanos(1_758_400_000_500_000_000),
        attributes: Vec::new(),
        status: None,
    }
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).expect("an export body must be valid JSON")
}

fn first_span(body: &str) -> Value {
    parse(body)["resourceSpans"][0]["scopeSpans"][0]["spans"][0].clone()
}

fn first_log(body: &str) -> Value {
    parse(body)["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0].clone()
}

/// Protobuf's canonical JSON mapping would base64 a `bytes` field. OTLP/JSON
/// overrides that for exactly these two fields, and a base64 id is a 400.
#[test]
fn ids_are_lowercase_hex_of_the_right_width() {
    let span = first_span(&traces_request(&[span()], "1.2.3"));

    assert_eq!(span["traceId"], "5b8efff798038103d269b633813fc60c");
    assert_eq!(span["spanId"], "eee19b7ec3c1b174");
    assert_eq!(span["traceId"].as_str().expect("string").len(), 32);
    assert_eq!(span["spanId"].as_str().expect("string").len(), 16);
}

/// Leading zero bytes must survive: `{:x}` on an integer would drop them and
/// produce an id of the wrong width.
#[test]
fn ids_keep_their_leading_zeros() {
    let mut span = span();
    span.parent_span_id = Some(SpanId::from_random(PARENT));
    let span = first_span(&traces_request(&[span], "1.2.3"));

    assert_eq!(span["parentSpanId"], "000102030405060a");
}

/// All-zero ids are "invalid" in W3C Trace Context; a server is required to
/// ignore a `traceparent` carrying one, which would silently unlink the trace.
#[test]
fn an_all_zero_random_draw_is_not_used_as_is() {
    assert_ne!(TraceId::from_random([0; 16]).to_hex(), "0".repeat(32));
    assert_ne!(SpanId::from_random([0; 8]).to_hex(), "0".repeat(16));
}

/// A JSON number cannot hold a nanosecond timestamp exactly (2^53 < 1.7e18), so
/// the encoding makes every 64-bit integer a string.
#[test]
fn timestamps_and_int_attributes_are_strings() {
    let mut span = span();
    span.attributes.push(KeyValue {
        key: "http.response.status_code",
        value: 200_u16.into(),
    });
    let span = first_span(&traces_request(&[span], "1.2.3"));

    assert_eq!(span["startTimeUnixNano"], "1758400000000000000");
    assert_eq!(span["endTimeUnixNano"], "1758400000500000000");
    assert_eq!(
        span["attributes"][0],
        json!({"key": "http.response.status_code", "value": {"intValue": "200"}})
    );
}

#[test]
fn enums_are_numbers() {
    let mut client = span();
    client.kind = SpanKind::Client;
    client.status = Some(ErrorStatus::new("HTTP 500"));

    assert_eq!(first_span(&traces_request(&[span()], "1.2.3"))["kind"], 1);
    let client = first_span(&traces_request(&[client], "1.2.3"));
    assert_eq!(client["kind"], 3);
    assert_eq!(client["status"], json!({"code": 2, "message": "HTTP 500"}));
}

/// An unset status, no parent and no attributes are all expressed by leaving the
/// field out. `"parentSpanId": null` in particular is not "no parent" to every
/// parser — and a root span is the thing the whole trace hangs off.
#[test]
fn absent_values_are_omitted_not_nulled() {
    let span = first_span(&traces_request(&[span()], "1.2.3"));
    let fields = span.as_object().expect("object");

    for absent in ["parentSpanId", "status", "attributes"] {
        assert!(!fields.contains_key(absent), "{absent} should be omitted");
    }
}

#[test]
fn every_attribute_type_uses_its_own_value_key() {
    let mut span = span();
    span.attributes = vec![
        KeyValue {
            key: "s",
            value: "text".into(),
        },
        KeyValue {
            key: "i",
            value: 7_i64.into(),
        },
        KeyValue {
            key: "b",
            value: true.into(),
        },
        KeyValue {
            key: "d",
            value: 1.5_f64.into(),
        },
    ];
    let span = first_span(&traces_request(&[span], "1.2.3"));

    assert_eq!(
        span["attributes"],
        json!([
            {"key": "s", "value": {"stringValue": "text"}},
            {"key": "i", "value": {"intValue": "7"}},
            {"key": "b", "value": {"boolValue": true}},
            {"key": "d", "value": {"doubleValue": 1.5}},
        ])
    );
}

/// The point of exporting logs through OTLP rather than anything simpler: Loki
/// stores these two fields as structured metadata and Grafana turns them into a
/// link to the trace.
#[test]
fn a_log_record_carries_the_span_it_was_written_in() {
    let record = LogRecord {
        time: UnixNanos(1_758_400_000_000_000_000),
        severity: Severity::Error,
        body: "loading pages failed: HTTP 500".to_string(),
        attributes: Vec::new(),
        context: Some((TraceId::from_random(TRACE), SpanId::from_random(SPAN))),
    };
    let log = first_log(&logs_request(&[record], "1.2.3"));

    assert_eq!(log["timeUnixNano"], "1758400000000000000");
    assert_eq!(log["severityNumber"], 17);
    assert_eq!(log["severityText"], "ERROR");
    assert_eq!(
        log["body"],
        json!({"stringValue": "loading pages failed: HTTP 500"})
    );
    assert_eq!(log["traceId"], "5b8efff798038103d269b633813fc60c");
    assert_eq!(log["spanId"], "eee19b7ec3c1b174");
}

/// A log written outside any span (a panic before the app mounts, say) has no
/// context, and must not invent one: an all-zero `traceId` string is a 400.
#[test]
fn a_log_record_outside_a_span_has_no_trace_fields() {
    let record = LogRecord {
        time: UnixNanos(1),
        severity: Severity::Warn,
        body: "x".to_string(),
        attributes: Vec::new(),
        context: None,
    };
    let log = first_log(&logs_request(&[record], "1.2.3"));
    let fields = log.as_object().expect("object");

    assert!(!fields.contains_key("traceId"));
    assert!(!fields.contains_key("spanId"));
}

#[test]
fn severities_are_ordered_and_numbered_per_the_spec() {
    let numbers: Vec<u8> = [
        Severity::Debug,
        Severity::Info,
        Severity::Warn,
        Severity::Error,
    ]
    .into_iter()
    .map(Severity::number)
    .collect();

    assert_eq!(numbers, [5, 9, 13, 17]);
    assert!(Severity::Debug < Severity::Info && Severity::Warn < Severity::Error);
}

/// The sidecar overwrites these three whatever a client sends, so the SPA
/// sending them could only ever be misleading — to a reader of this code, or to
/// anyone pointing the SPA at a collector that does *not* overwrite them.
#[test]
fn the_resource_claims_no_identity() {
    for body in [
        traces_request(&[span()], "1.2.3"),
        logs_request(&[], "1.2.3"),
    ] {
        assert!(!body.contains("service.name"), "{body}");
        assert!(!body.contains("deployment.environment"), "{body}");
        assert!(!body.contains("service.version"), "{body}");
        assert!(body.contains(r#""telemetry.sdk.name""#), "{body}");
    }
}

#[test]
fn traceparent_is_version_00_and_always_sampled() {
    assert_eq!(
        traceparent(TraceId::from_random(TRACE), SpanId::from_random(SPAN)),
        "00-5b8efff798038103d269b633813fc60c-eee19b7ec3c1b174-01"
    );
}
