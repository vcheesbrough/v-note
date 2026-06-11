use std::fs;
use std::path::Path;

use protocol::{
    CreatePageRequest, HealthResponse, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse,
    PageClientMessage, PageReplay, PageServerMessage, PageSummary, RealtimeTicketResponse, Stroke,
    StrokeBatch, PROTOCOL_VERSION,
};

fn fixture(path: &str) -> String {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("contracts")
        .join("fixtures")
        .join(path);
    fs::read_to_string(fixture_path).expect("fixture should be readable")
}

#[test]
fn deserializes_health_fixture() {
    let parsed: HealthResponse =
        serde_json::from_str(&fixture("health.json")).expect("health fixture should parse");
    assert_eq!(parsed.status, "ok");
}

#[test]
fn deserializes_meta_fixture() {
    let parsed: MetaResponse =
        serde_json::from_str(&fixture("meta.json")).expect("meta fixture should parse");
    assert_eq!(parsed.app_name, "v-note");
    assert_eq!(parsed.protocol_version, PROTOCOL_VERSION);
}

#[test]
fn deserializes_me_fixture() {
    let parsed: MeResponse =
        serde_json::from_str(&fixture("me.json")).expect("me fixture should parse");
    assert_eq!(parsed.sub, "v-note-test-service-account");
    assert_eq!(parsed.email.as_deref(), Some("test@example.com"));
}

#[test]
fn deserializes_stroke_fixtures() {
    let stroke: Stroke =
        serde_json::from_str(&fixture("stroke.json")).expect("stroke fixture should parse");
    assert_eq!(stroke.tool, "pen");
    assert_eq!(stroke.color, "#006400");
    assert_eq!(stroke.width, 2.0);
    assert_eq!(stroke.points.len(), 3);
    // MVP omits pressure entirely.
    assert!(stroke.points.iter().all(|point| point.pressure.is_none()));

    let batch: StrokeBatch = serde_json::from_str(&fixture("stroke-batch.json"))
        .expect("stroke batch fixture should parse");
    assert_eq!(batch.seq, 1);
    assert_eq!(batch.strokes.len(), 1);
}

/// Golden page-replay: bbox and point/stroke counts are pinned so every client
/// (Android Compose + SPA Canvas2D) renders identical geometry. A drift in
/// coordinate handling on any client breaks this contract.
#[test]
fn page_replay_golden_geometry() {
    let replay: PageReplay = serde_json::from_str(&fixture("page-replay.json"))
        .expect("page replay fixture should parse");

    // Sequences are strictly monotonic and match the declared head.
    assert_eq!(replay.last_seq, 2);
    let seqs: Vec<u64> = replay.batches.iter().map(|batch| batch.seq).collect();
    assert_eq!(seqs, vec![1, 2]);

    let strokes: Vec<&Stroke> = replay
        .batches
        .iter()
        .flat_map(|batch| batch.strokes.iter())
        .collect();
    assert_eq!(strokes.len(), 3, "stroke count");

    let point_count: usize = strokes.iter().map(|stroke| stroke.points.len()).sum();
    assert_eq!(point_count, 7, "total point count");

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for point in strokes.iter().flat_map(|stroke| stroke.points.iter()) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    assert_eq!(
        (min_x, min_y, max_x, max_y),
        (5.0, 8.0, 100.0, 90.0),
        "bbox"
    );
}

#[test]
fn deserializes_page_channel_fixtures() {
    let subscribe: PageClientMessage = serde_json::from_str(&fixture("page-client-subscribe.json"))
        .expect("subscribe fixture should parse");
    assert!(matches!(
        subscribe,
        PageClientMessage::Subscribe { from_seq: 0 }
    ));

    let commit: PageClientMessage = serde_json::from_str(&fixture("page-client-commit-batch.json"))
        .expect("commit-batch fixture should parse");
    assert!(matches!(commit, PageClientMessage::CommitBatch { .. }));

    let welcome: PageServerMessage = serde_json::from_str(&fixture("page-server-welcome.json"))
        .expect("welcome fixture should parse");
    assert!(matches!(welcome, PageServerMessage::Welcome { .. }));

    let server_batch: PageServerMessage =
        serde_json::from_str(&fixture("page-server-stroke-batch.json"))
            .expect("server stroke-batch fixture should parse");
    match server_batch {
        PageServerMessage::StrokeBatch(batch) => assert_eq!(batch.seq, 1),
        other => panic!("expected stroke-batch, got {other:?}"),
    }

    let denied: PageServerMessage = serde_json::from_str(&fixture("page-server-lease-denied.json"))
        .expect("lease-denied fixture should parse");
    assert!(matches!(denied, PageServerMessage::LeaseDenied { .. }));
}

#[test]
fn deserializes_page_fixtures() {
    let page: PageSummary =
        serde_json::from_str(&fixture("page.json")).expect("page fixture should parse");
    assert_eq!(page.title, "Meeting notes");

    let list: ListPagesResponse =
        serde_json::from_str(&fixture("pages.json")).expect("pages fixture should parse");
    assert_eq!(list.pages.len(), 1);

    let create: CreatePageRequest = serde_json::from_str(&fixture("create-page.json"))
        .expect("create page fixture should parse");
    assert_eq!(create.title.as_deref(), Some("Meeting notes"));
}

#[test]
fn deserializes_realtime_fixtures() {
    let ticket: RealtimeTicketResponse = serde_json::from_str(&fixture("realtime-ticket.json"))
        .expect("realtime ticket fixture should parse");
    assert!(ticket.ticket.starts_with("ticket_"));

    let created: LibraryEvent = serde_json::from_str(&fixture("library-event-page-created.json"))
        .expect("page-created event fixture should parse");
    assert!(matches!(created, LibraryEvent::PageCreated { .. }));

    let deleted: LibraryEvent = serde_json::from_str(&fixture("library-event-page-deleted.json"))
        .expect("page-deleted event fixture should parse");
    assert!(matches!(deleted, LibraryEvent::PageDeleted { .. }));
}
