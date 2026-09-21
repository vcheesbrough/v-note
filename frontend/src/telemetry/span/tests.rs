//! The two failures the pre-review design had, as tests: overlapping spans
//! mis-parenting each other, and a span crossing a screen change switching
//! trace. Both were invisible to the e2e suite's "one trace, one root" checks.

use super::*;

fn trace(byte: u8) -> TraceId {
    TraceId::from_random([byte; 16])
}

fn span_id(byte: u8) -> SpanId {
    SpanId::from_random([byte; 8])
}

fn screen(trace_byte: u8, root_byte: u8) -> Parent {
    Parent {
        trace_id: trace(trace_byte),
        span_id: span_id(root_byte),
    }
}

fn open(parent: Parent, id: u8, name: &'static str) -> OpenSpan {
    OpenSpan::open(parent, span_id(id), name, SpanKind::Client, UnixNanos(1))
}

/// The page-load case exactly: two requests start under the screen, and end in
/// the order they started — not the reverse. Both must be the screen's
/// children, and neither the other's.
#[test]
fn overlapping_spans_are_both_children_of_what_they_were_opened_under() {
    let root = screen(0xa1, 0x01);
    let load_pages = open(root, 0x10, "http.client");
    let ticket = open(root, 0x20, "http.client");

    let load_pages = load_pages.finish(UnixNanos(2), None);
    let ticket = ticket.finish(UnixNanos(3), None);

    assert_eq!(load_pages.parent_span_id, Some(root.span_id));
    assert_eq!(ticket.parent_span_id, Some(root.span_id));
    assert_ne!(ticket.parent_span_id, Some(load_pages.span_id));
}

/// A span opened on one screen and finished after the user has moved to
/// another stays in the trace it started in — the one its `traceparent`
/// already named to the server.
#[test]
fn a_span_keeps_its_trace_across_a_screen_change() {
    let library = screen(0xa1, 0x01);
    let in_flight = open(library, 0x10, "http.client");
    let traceparent_trace = in_flight.context().trace_id;

    // The user opens a page; a new screen, a new trace.
    let page = screen(0xb2, 0x02);
    let on_new_screen = open(page, 0x20, "http.client").finish(UnixNanos(2), None);

    let finished = in_flight.finish(UnixNanos(3), None);
    assert_eq!(finished.trace_id, library.trace_id);
    assert_eq!(finished.trace_id, traceparent_trace);
    assert_eq!(finished.parent_span_id, Some(library.span_id));
    assert_eq!(on_new_screen.trace_id, page.trace_id);
}

/// Nesting is explicit: a child is opened under its parent's `context()`, and
/// lands in the parent's trace under the parent's id.
#[test]
fn a_child_opened_from_a_spans_context_nests_under_it() {
    let root = screen(0xa1, 0x01);
    let connect = open(root, 0x10, "realtime.connect");
    let subscribe = open(connect.context(), 0x20, "realtime.subscribe").finish(UnixNanos(2), None);

    assert_eq!(subscribe.trace_id, root.trace_id);
    assert_eq!(subscribe.parent_span_id, Some(connect.context().span_id));
}

/// `context()` is what a request inside the span puts in its `traceparent`, so
/// it has to name the span itself, not the span's parent.
#[test]
fn context_names_the_span_itself_in_its_parents_trace() {
    let root = screen(0xa1, 0x01);
    let span = open(root, 0x10, "http.client");

    assert_eq!(
        span.context(),
        Parent {
            trace_id: root.trace_id,
            span_id: span_id(0x10),
        }
    );
}

#[test]
fn attributes_and_status_survive_to_the_finished_span() {
    let mut span = open(screen(0xa1, 0x01), 0x10, "http.client");
    span.attr("http.response.status_code", AnyValue::Int(500));

    let finished = span.finish(UnixNanos(9), Some(ErrorStatus::new("HTTP 500")));
    assert_eq!(finished.attributes.len(), 1);
    assert_eq!(finished.status, Some(ErrorStatus::new("HTTP 500")));
    assert_eq!(finished.end_time_unix_nano, UnixNanos(9));
}
