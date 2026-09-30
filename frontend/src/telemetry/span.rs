//! Span parenting, as pure data (#354).
//!
//! A span takes its parent **explicitly**, as a [`Parent`] captured when it
//! opens, and never reads shared state again. That is the whole design, and it
//! exists because the obvious alternative is wrong here: a global "current
//! span" that `open` pushes and `end` pops only works if spans end in the order
//! they opened. The SPA's futures interleave at every `await`, so they do not —
//! on a signed-in page load `load_pages` and the realtime ticket request run
//! concurrently, and a global stack parents one under the other and then
//! restores an already-finished span as "current" for the rest of the screen.
//! It also read the trace id when a span *ended*, so a request in flight across
//! a route change was sent under one trace and recorded in another.
//!
//! With the parent captured at open, both are impossible by construction: a
//! span's trace and parent are fixed the moment it exists, whatever else opens,
//! ends or changes screen meanwhile.
//!
//! No browser API in this file, so it is tested on the host.

use std::panic::{Location, PanicHookInfo};

use super::otlp::{AnyValue, ErrorStatus, KeyValue, Span, SpanId, SpanKind, TraceId, UnixNanos};

/// Where a span was opened or a log line written, as OpenTelemetry's `code.*`
/// attributes (#453) — the keys the server's `tracing` spans already carry, so
/// a client item in Tempo or Loki leads to its line of code the same way.
///
/// The location is the compiler's, via `#[track_caller]`: no stack walk, and
/// nothing a user typed. Every public entry point in `telemetry.rs` that opens
/// a span or writes a line is `#[track_caller]`, and so must be any helper that
/// wraps one, or the location names the helper. It does not pass through a
/// closure or an `async fn`: a call inside either is located at that call,
/// which is the right answer anyway. There is no `code.function.name`: a
/// `Location` has no function, and deriving one would cost a macro at every
/// call site for what the line already says.
pub(crate) fn code_location(location: &Location<'_>) -> [KeyValue; 2] {
    [
        KeyValue {
            key: "code.file.path",
            value: stable_path(location.file()).into(),
        },
        KeyValue {
            key: "code.line.number",
            value: i64::from(location.line()).into(),
        },
    ]
}

/// The panic hook's attributes: `exception.type`, and the panic's own
/// location — where it panicked, not the hook — when the runtime gives one.
pub(crate) fn panic_attributes(info: &PanicHookInfo<'_>) -> Vec<KeyValue> {
    let mut attributes = vec![KeyValue {
        key: "exception.type",
        value: "panic".into(),
    }];
    if let Some(location) = info.location() {
        attributes.extend(code_location(location));
    }
    attributes
}

/// Where cargo keeps dependency sources. Our own crate's paths are already
/// workspace-relative (`frontend/src/…`), but a location inside a dependency —
/// a panic in a library, mostly — is the absolute path on the machine that
/// built the bundle, e.g.
/// `/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/leptos-0.8.20/src/lib.rs`.
/// That changes with the build host and leaks its layout, so everything up to
/// the crate directory is dropped: `leptos-0.8.20/src/lib.rs` names the same
/// file on every build. Git dependencies likewise keep `<repo>-<hash>/<rev>/…`.
/// Standard-library paths (`/rustc/<commit>/library/…`) are already remapped
/// by rustc and are left alone.
const DEPENDENCY_ROOTS: [&str; 2] = ["/registry/src/", "/git/checkouts/"];

pub(crate) fn stable_path(file: &str) -> &str {
    for root in DEPENDENCY_ROOTS {
        if let Some((_, rest)) = file.split_once(root) {
            // A registry source dir has one more level, the index, before the
            // crate; a git checkout's first level is already the repository.
            return if root == "/registry/src/" {
                rest.split_once('/').map_or(rest, |(_, krate)| krate)
            } else {
                rest
            };
        }
    }
    file
}

/// Where a span hangs: a trace, and the span within it that is its parent.
/// `Copy`, so handing one to a spawned future costs nothing and cannot alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parent {
    pub(crate) trace_id: TraceId,
    pub(crate) span_id: SpanId,
}

/// A span that has started and not yet ended.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OpenSpan {
    parent: Parent,
    id: SpanId,
    name: &'static str,
    kind: SpanKind,
    start: UnixNanos,
    attributes: Vec<KeyValue>,
}

impl OpenSpan {
    /// Opens a span, located at `location` — the caller of whichever
    /// `telemetry` function opened it.
    pub(crate) fn open(
        parent: Parent,
        id: SpanId,
        name: &'static str,
        kind: SpanKind,
        start: UnixNanos,
        location: &Location<'_>,
    ) -> Self {
        Self {
            parent,
            id,
            name,
            kind,
            start,
            attributes: code_location(location).into(),
        }
    }

    pub(crate) fn attr(&mut self, key: &'static str, value: AnyValue) {
        self.attributes.push(KeyValue { key, value });
    }

    /// This span as a parent: for a child span, a log line, or the
    /// `traceparent` of a request made inside it.
    pub(crate) fn context(&self) -> Parent {
        Parent {
            trace_id: self.parent.trace_id,
            span_id: self.id,
        }
    }

    pub(crate) fn finish(self, end: UnixNanos, status: Option<ErrorStatus>) -> Span {
        Span {
            trace_id: self.parent.trace_id,
            span_id: self.id,
            parent_span_id: Some(self.parent.span_id),
            name: self.name,
            kind: self.kind,
            start_time_unix_nano: self.start,
            end_time_unix_nano: end,
            attributes: self.attributes,
            status,
        }
    }
}

#[cfg(test)]
mod tests;
