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

use super::otlp::{AnyValue, ErrorStatus, KeyValue, Span, SpanId, SpanKind, TraceId, UnixNanos};

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
    pub(crate) fn open(
        parent: Parent,
        id: SpanId,
        name: &'static str,
        kind: SpanKind,
        start: UnixNanos,
    ) -> Self {
        Self {
            parent,
            id,
            name,
            kind,
            start,
            attributes: Vec::new(),
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
