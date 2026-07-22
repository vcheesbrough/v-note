use std::fs;
use std::path::Path;

use protocol::{
    CreatePageRequest, HealthResponse, LibraryEvent, ListPagesResponse, MeResponse, MetaResponse,
    PageClientMessage, PageReplay, PageServerMessage, PageSummary, RealtimeTicketResponse, Stroke,
    StrokeBatch, StrokePoint, StrokeStyle, DEFAULT_PEN_WIDTH, MIN_PRESSURE_WIDTH,
    PROTOCOL_VERSION, SOLID_ROUND_PRESSURE_STYLE_VERSION,
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
    assert_eq!(stroke.id, "stroke_fixture_1");
    assert_eq!(stroke.style.parameters.color, "#006400");
    assert_eq!(stroke.style.parameters.width, 4.0);
    assert!(stroke.style.validate().is_ok());
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

/// The v2 pressure fixture parses, carries per-point pressure, and validates —
/// while a point without pressure is allowed and renders at full width.
#[test]
fn deserializes_pressure_stroke_fixture() {
    let stroke: Stroke = serde_json::from_str(&fixture("stroke-pressure.json"))
        .expect("pressure stroke fixture should parse");
    assert_eq!(stroke.style.style_version, SOLID_ROUND_PRESSURE_STYLE_VERSION);
    assert!(stroke.style.is_pressure_sensitive());
    assert!(stroke.validate().is_ok(), "v2 stroke with pressure is valid");
    assert_eq!(stroke.points.len(), 4);
    assert_eq!(stroke.points[0].pressure, Some(0.0));
    assert_eq!(stroke.points[2].pressure, Some(1.0));
    // A v2 point may omit pressure; it renders at full width.
    assert_eq!(stroke.points[3].pressure, None);
    let full = stroke.style.parameters.width;
    assert_eq!(stroke.style.rendered_width(stroke.points[3].pressure), full);
}

/// Round-trip a v2 stroke: `pressure` survives serialization and absent
/// pressure stays absent (never serialized as `null`).
#[test]
fn pressure_survives_round_trip() {
    let stroke = Stroke {
        id: "rt".to_string(),
        style: StrokeStyle::default_solid_round_pressure(),
        points: vec![
            StrokePoint { x: 0.0, y: 0.0, t: 0, pressure: Some(0.25) },
            StrokePoint { x: 1.0, y: 1.0, t: 8, pressure: None },
        ],
    };
    let json = serde_json::to_string(&stroke).expect("serialize");
    assert!(!json.contains("null"), "absent pressure must be omitted, not null");
    let back: Stroke = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, stroke);
}

/// The shared pressure→width curve: full preset width at p=1, an absolute
/// MIN_PRESSURE_WIDTH floor at p=0, linear between; v1 ignores pressure.
#[test]
fn rendered_width_follows_shared_curve() {
    let v2 = StrokeStyle::default_solid_round_pressure();
    let w = DEFAULT_PEN_WIDTH; // 4.0
    let floor = MIN_PRESSURE_WIDTH; // 1.5, < 4.0
    assert_eq!(v2.rendered_width(Some(1.0)), w);
    assert_eq!(v2.rendered_width(None), w, "absent pressure = full width");
    assert!((v2.rendered_width(Some(0.0)) - floor).abs() < 1e-9);
    let mid = floor + (w - floor) * 0.5;
    assert!((v2.rendered_width(Some(0.5)) - mid).abs() < 1e-9);
    // Out-of-range pressure is clamped for *rendering* (validation rejects it
    // on the wire, but a renderer must stay in bounds defensively).
    assert_eq!(v2.rendered_width(Some(5.0)), w);
    assert!((v2.rendered_width(Some(-1.0)) - floor).abs() < 1e-9);

    // The floor is absolute, so a wide pen tapers far below its preset width.
    let mut wide = StrokeStyle::default_solid_round_pressure();
    wide.parameters.width = 32.0;
    assert!((wide.rendered_width(Some(0.0)) - floor).abs() < 1e-9);
    assert_eq!(wide.rendered_width(Some(1.0)), 32.0);

    // A pen thinner than the floor never exceeds its own preset (no taper range).
    let mut thin = StrokeStyle::default_solid_round_pressure();
    thin.parameters.width = 1.0;
    assert_eq!(thin.rendered_width(Some(0.0)), 1.0);
    assert_eq!(thin.rendered_width(Some(1.0)), 1.0);

    // v1 is constant regardless of pressure.
    let v1 = StrokeStyle::default_solid_round();
    assert_eq!(v1.rendered_width(Some(0.0)), w);
    assert_eq!(v1.rendered_width(Some(1.0)), w);
}

/// Pressure is legal only on v2 styles, and only when finite and in `0..=1`.
#[test]
fn stroke_validation_enforces_pressure_rules() {
    let point_with = |pressure: Option<f64>| StrokePoint { x: 0.0, y: 0.0, t: 0, pressure };

    // v1 must not carry pressure.
    let v1_with_pressure = Stroke {
        id: "a".to_string(),
        style: StrokeStyle::default_solid_round(),
        points: vec![point_with(Some(0.5))],
    };
    assert!(v1_with_pressure.validate().is_err());

    // v1 without pressure is fine.
    let v1_clean = Stroke {
        id: "b".to_string(),
        style: StrokeStyle::default_solid_round(),
        points: vec![point_with(None)],
    };
    assert!(v1_clean.validate().is_ok());

    // v2 rejects out-of-range and non-finite pressure (reject, don't clamp).
    for bad in [1.5_f64, -0.1, f64::NAN, f64::INFINITY] {
        let stroke = Stroke {
            id: "c".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: vec![point_with(Some(bad))],
        };
        assert!(stroke.validate().is_err(), "pressure {bad} must be rejected");
    }

    // v2 accepts the closed interval and absent pressure.
    for good in [Some(0.0), Some(1.0), Some(0.42), None] {
        let stroke = Stroke {
            id: "d".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: vec![point_with(good)],
        };
        assert!(stroke.validate().is_ok(), "pressure {good:?} must be accepted");
    }
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
fn page_error_correlates_tombstone_failures_and_accepts_legacy_errors() {
    let correlated = PageServerMessage::Error {
        code: "tombstone_failed".to_string(),
        message: "could not persist deleted strokes".to_string(),
        client_mutation_id: Some("erase_fixture_1".to_string()),
    };
    let json = serde_json::to_value(correlated).expect("page error should serialize");
    assert_eq!(json["client_mutation_id"], "erase_fixture_1");

    let legacy: PageServerMessage = serde_json::from_str(
        r#"{"type":"error","code":"commit_failed","message":"Commit failed"}"#,
    )
    .expect("page errors without a mutation id should remain compatible");
    assert!(matches!(
        legacy,
        PageServerMessage::Error {
            client_mutation_id: None,
            ..
        }
    ));
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

    let thumbnail: LibraryEvent =
        serde_json::from_str(&fixture("library-event-page-thumbnail-updated.json"))
            .expect("thumbnail event fixture should parse");
    assert!(matches!(
        thumbnail,
        LibraryEvent::PageThumbnailUpdated { .. }
    ));
}
