//! The two failures the pre-review design had, as tests: overlapping spans
//! mis-parenting each other, and a span crossing a screen change switching
//! trace. Both were invisible to the e2e suite's "one trace, one root" checks.

use std::panic::Location;

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
    OpenSpan::open(
        parent,
        span_id(id),
        name,
        SpanKind::Client,
        UnixNanos(1),
        Location::caller(),
    )
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
    // The two location attributes, then the caller's own.
    assert_eq!(finished.attributes.len(), 3);
    assert_eq!(finished.attributes[2].key, "http.response.status_code");
    assert_eq!(finished.status, Some(ErrorStatus::new("HTTP 500")));
    assert_eq!(finished.end_time_unix_nano, UnixNanos(9));
}

// ---------------------------------------------------------------------------
// Source location (#453)
// ---------------------------------------------------------------------------

fn string_attr(attributes: &[KeyValue], key: &str) -> Option<String> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| match &kv.value {
            AnyValue::String(value) => value.clone(),
            other => panic!("{key} is not a string: {other:?}"),
        })
}

fn int_attr(attributes: &[KeyValue], key: &str) -> Option<i64> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| match kv.value {
            AnyValue::Int(value) => value,
            ref other => panic!("{key} is not an int: {other:?}"),
        })
}

/// The semconv keys, typed as OTLP expects: the file a string, the line an int.
#[test]
fn a_location_becomes_code_file_path_and_code_line_number() {
    let here = Location::caller();
    let attributes = code_location(here);

    assert_eq!(
        string_attr(&attributes, "code.file.path").as_deref(),
        Some(here.file())
    );
    assert_eq!(
        int_attr(&attributes, "code.line.number"),
        Some(i64::from(here.line()))
    );
    assert!(
        here.file().ends_with("telemetry/span/tests.rs"),
        "{}",
        here.file()
    );
}

/// The shape every entry point in `telemetry.rs` has: a `#[track_caller]`
/// function taking `Location::caller()` and handing it down.
#[track_caller]
fn entry_point(parent: Parent) -> OpenSpan {
    OpenSpan::open(
        parent,
        span_id(0x30),
        "realtime.connect",
        SpanKind::Internal,
        UnixNanos(1),
        Location::caller(),
    )
}

/// And the shape of a helper that wraps one, like `api::start`.
#[track_caller]
fn wrapper(parent: Parent) -> OpenSpan {
    entry_point(parent)
}

/// A span is located where application code asked for it — through any number
/// of `#[track_caller]` layers — not inside the telemetry module.
#[test]
fn a_span_is_located_at_the_outermost_caller() {
    let (span, line) = (wrapper(screen(0xa1, 0x01)), line!());
    let finished = span.finish(UnixNanos(2), None);

    assert_eq!(
        string_attr(&finished.attributes, "code.file.path").as_deref(),
        Some(file!())
    );
    assert_eq!(
        int_attr(&finished.attributes, "code.line.number"),
        Some(i64::from(line))
    );
}

/// What makes the above true for the real functions: they need a browser, so
/// cannot run here, and dropping `#[track_caller]` from one compiles fine and
/// silently locates every caller at the telemetry module. So the attribute is
/// checked on the source: every public function that opens a span or writes a
/// log line, and the one helper that wraps them (`api::start`). The e2e suite
/// checks the result on what Tempo and Loki stored.
#[test]
fn every_entry_point_that_records_is_track_caller() {
    let entry_points: [(&str, &str, &[&str]); 2] = [
        (
            "telemetry.rs",
            include_str!("../../telemetry.rs"),
            &[
                "span",
                "client_span",
                "start_screen",
                "log",
                "log_in",
                "info_in",
                "warn_in",
                "error_in",
            ],
        ),
        ("api.rs", include_str!("../../api.rs"), &["start"]),
    ];
    for (file, source, functions) in entry_points {
        for function in functions {
            let signature = source
                .lines()
                .position(|line| {
                    line.starts_with(&format!("pub(crate) fn {function}("))
                        || line.starts_with(&format!("fn {function}("))
                })
                .unwrap_or_else(|| panic!("{file}: no fn {function}"));
            let attribute = source.lines().nth(signature.saturating_sub(1));
            assert_eq!(
                attribute,
                Some("#[track_caller]"),
                "{file}: fn {function} must be #[track_caller] (#453)"
            );
        }
    }
}
