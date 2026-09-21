//! The OTLP/HTTP **JSON** encoding of the two signals the SPA exports (#354):
//! spans and log records. Pure data and `serde` — no browser API is touched in
//! this file, which is what lets it be tested on the host.
//!
//! Hand-written rather than the OpenTelemetry SDK for a reason: the Rust SDK's
//! exporters need tonic or a Tokio runtime and do not build for `wasm32`, and the
//! JS SDK would cost bundle size and a `wasm-bindgen` shim. OTLP defines a JSON
//! encoding, the sidecar's receiver accepts it, and `serde_json` is already in
//! the bundle — so the whole exporter is the structs below.
//!
//! Three rules of that encoding are easy to get wrong, and each produces a
//! request the collector rejects or, worse, silently misreads:
//!
//! - trace and span ids are **lowercase hex**, not the base64 that protobuf's
//!   canonical JSON mapping would give a `bytes` field;
//! - 64-bit integers — every timestamp, and `intValue` — are **strings**;
//! - enums (`kind`, `status.code`, `severityNumber`) are **numbers**.

use serde::{Serialize, Serializer};

/// A W3C trace id. All-zero is reserved as "invalid" by the spec, so the only
/// constructor that accepts arbitrary bytes ([`TraceId::from_random`]) repairs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TraceId([u8; 16]);

/// A W3C span id. All-zero is likewise invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpanId([u8; 8]);

impl TraceId {
    pub(crate) fn from_random(mut bytes: [u8; 16]) -> Self {
        if bytes == [0; 16] {
            bytes[15] = 1;
        }
        Self(bytes)
    }

    pub(crate) fn to_hex(self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    pub(crate) fn from_random(mut bytes: [u8; 8]) -> Self {
        if bytes == [0; 8] {
            bytes[7] = 1;
        }
        Self(bytes)
    }

    pub(crate) fn to_hex(self) -> String {
        hex(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "{byte:02x}");
            out
        })
}

impl Serialize for TraceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl Serialize for SpanId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

/// The `traceparent` header for a request made inside `span_id`'s span.
///
/// Always flagged sampled (`-01`). The SPA does not head-sample, and must not
/// start to without care: Traefik's tracer is parent-based, so a `-00` here
/// would not merely drop the browser's span — it would switch off the Traefik
/// and server spans for that request, which exist today for every request.
pub(crate) fn traceparent(trace_id: TraceId, span_id: SpanId) -> String {
    format!("00-{}-{}-01", trace_id.to_hex(), span_id.to_hex())
}

/// Nanoseconds since the Unix epoch. A string on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnixNanos(pub(crate) u64);

impl Serialize for UnixNanos {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct KeyValue {
    pub(crate) key: &'static str,
    pub(crate) value: AnyValue,
}

/// serde's default externally-tagged form is exactly OTLP's `AnyValue`:
/// `{"stringValue":"…"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) enum AnyValue {
    #[serde(rename = "stringValue")]
    String(String),
    #[serde(rename = "intValue", serialize_with = "int_as_string")]
    Int(i64),
    #[serde(rename = "boolValue")]
    Bool(bool),
    #[serde(rename = "doubleValue")]
    Double(f64),
}

fn int_as_string<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

impl From<&str> for AnyValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for AnyValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<i64> for AnyValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<u16> for AnyValue {
    fn from(value: u16) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u32> for AnyValue {
    fn from(value: u32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<bool> for AnyValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<f64> for AnyValue {
    fn from(value: f64) -> Self {
        Self::Double(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpanKind {
    Internal,
    /// An outbound request: the span a server's span is the child of.
    Client,
}

impl Serialize for SpanKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(match self {
            Self::Internal => 1,
            Self::Client => 3,
        })
    }
}

/// Only ever serialized for a failure: an unset status is the absence of the
/// field, which is also what "fine" looks like to every OTLP backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ErrorStatus {
    code: StatusCodeError,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusCodeError;

impl Serialize for StatusCodeError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(2)
    }
}

impl ErrorStatus {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            code: StatusCodeError,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Span {
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: SpanId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_span_id: Option<SpanId>,
    pub(crate) name: &'static str,
    pub(crate) kind: SpanKind,
    pub(crate) start_time_unix_nano: UnixNanos,
    pub(crate) end_time_unix_nano: UnixNanos,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) attributes: Vec<KeyValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<ErrorStatus>,
}

/// The four levels the SPA logs at, with OTLP's `severityNumber` for each (the
/// first number of each level's range, as the SDKs emit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    fn number(self) -> u8 {
        match self {
            Self::Debug => 5,
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
        }
    }

    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LogRecord {
    pub(crate) time: UnixNanos,
    pub(crate) severity: Severity,
    pub(crate) body: String,
    pub(crate) attributes: Vec<KeyValue>,
    /// The span that was active, which is what makes Loki's line link to Tempo.
    pub(crate) context: Option<(TraceId, SpanId)>,
}

impl Serialize for LogRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire<'a> {
            time_unix_nano: UnixNanos,
            severity_number: u8,
            severity_text: &'static str,
            body: AnyValue,
            #[serde(skip_serializing_if = "<[KeyValue]>::is_empty")]
            attributes: &'a [KeyValue],
            #[serde(skip_serializing_if = "Option::is_none")]
            trace_id: Option<TraceId>,
            #[serde(skip_serializing_if = "Option::is_none")]
            span_id: Option<SpanId>,
        }
        Wire {
            time_unix_nano: self.time,
            severity_number: self.severity.number(),
            severity_text: self.severity.text(),
            body: AnyValue::String(self.body.clone()),
            attributes: &self.attributes,
            trace_id: self.context.map(|(trace_id, _)| trace_id),
            span_id: self.context.map(|(_, span_id)| span_id),
        }
        .serialize(serializer)
    }
}

/// What the SPA says about itself. **Every identity attribute is absent on
/// purpose**: the sidecar drops whatever a client claims for `service.name`,
/// `deployment.environment` and `service.version` and writes its own, so sending
/// them would only be sending something to be ignored. What is here is what the
/// sidecar's allow-list lets through.
fn resource(sdk_version: &str) -> serde_json::Value {
    serde_json::json!({
        "attributes": [
            KeyValue { key: "telemetry.sdk.name", value: "v-note-spa-otlp".into() },
            KeyValue { key: "telemetry.sdk.language", value: "rust".into() },
            KeyValue { key: "telemetry.sdk.version", value: sdk_version.into() },
        ]
    })
}

const SCOPE_NAME: &str = "v-note-spa";

/// An `ExportTraceServiceRequest` body.
pub(crate) fn traces_request(spans: &[Span], sdk_version: &str) -> String {
    serde_json::json!({
        "resourceSpans": [{
            "resource": resource(sdk_version),
            "scopeSpans": [{
                "scope": { "name": SCOPE_NAME, "version": sdk_version },
                "spans": spans,
            }],
        }],
    })
    .to_string()
}

/// An `ExportLogsServiceRequest` body.
pub(crate) fn logs_request(records: &[LogRecord], sdk_version: &str) -> String {
    serde_json::json!({
        "resourceLogs": [{
            "resource": resource(sdk_version),
            "scopeLogs": [{
                "scope": { "name": SCOPE_NAME, "version": sdk_version },
                "logRecords": records,
            }],
        }],
    })
    .to_string()
}

#[cfg(test)]
mod tests;
