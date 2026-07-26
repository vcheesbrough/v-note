use std::fs;
use std::path::Path;

use protocol::{
    paper_marks, CreatePageRequest, HealthResponse, LibraryEvent, ListPagesResponse, MeResponse,
    MetaResponse, PageClientMessage, PageReplay, PageServerMessage, PageSummary, Paper,
    RealtimeTicketResponse, Stroke, StrokeBatch, StrokePoint, StrokeStyle, WorldViewport,
    DEFAULT_PEN_WIDTH, GRID_SPACING_LARGE, GRID_SPACING_SMALL, MARGIN_COLOR, MARGIN_LINE_WIDTH,
    MARGIN_X, MAX_PAPER_MARKS_PER_AXIS, MIN_PAPER_MARK_DEVICE_PITCH, MIN_PAPER_MARK_DEVICE_WIDTH,
    MIN_PRESSURE_WIDTH, PROTOCOL_VERSION, RULE_COLOR, RULE_LINE_WIDTH, RULE_SPACING_NARROW,
    RULE_SPACING_WIDE, SOLID_ROUND_PRESSURE_STYLE_VERSION,
};

fn fixture_path(path: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("contracts")
        .join("fixtures")
        .join(path)
}

fn fixture(path: &str) -> String {
    fs::read_to_string(fixture_path(path)).expect("fixture should be readable")
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
    assert_eq!(
        stroke.style.style_version,
        SOLID_ROUND_PRESSURE_STYLE_VERSION
    );
    assert!(stroke.style.is_pressure_sensitive());
    assert!(
        stroke.validate().is_ok(),
        "v2 stroke with pressure is valid"
    );
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
            StrokePoint {
                x: 0.0,
                y: 0.0,
                t: 0,
                pressure: Some(0.25),
            },
            StrokePoint {
                x: 1.0,
                y: 1.0,
                t: 8,
                pressure: None,
            },
        ],
    };
    let json = serde_json::to_string(&stroke).expect("serialize");
    assert!(
        !json.contains("null"),
        "absent pressure must be omitted, not null"
    );
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
    let point_with = |pressure: Option<f64>| StrokePoint {
        x: 0.0,
        y: 0.0,
        t: 0,
        pressure,
    };

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
        assert!(
            stroke.validate().is_err(),
            "pressure {bad} must be rejected"
        );
    }

    // v2 accepts the closed interval and absent pressure.
    for good in [Some(0.0), Some(1.0), Some(0.42), None] {
        let stroke = Stroke {
            id: "d".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: vec![point_with(good)],
        };
        assert!(
            stroke.validate().is_ok(),
            "pressure {good:?} must be accepted"
        );
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
    assert_eq!(page.paper, Paper::RuledMarginNarrow);

    let list: ListPagesResponse =
        serde_json::from_str(&fixture("pages.json")).expect("pages fixture should parse");
    assert_eq!(list.pages.len(), 1);
    assert_eq!(list.pages[0].paper, Paper::SquaredLarge);

    let create: CreatePageRequest = serde_json::from_str(&fixture("create-page.json"))
        .expect("create page fixture should parse");
    assert_eq!(create.title.as_deref(), Some("Meeting notes"));
    assert_eq!(create.paper, Paper::RuledWide);
}

/// Pre-v5 payloads carry no `paper` and must keep parsing as blank pages, so an
/// older client's JSON is never rejected or mis-rendered.
#[test]
fn legacy_payloads_without_paper_default_to_none() {
    let page: PageSummary = serde_json::from_str(
        r#"{"id":"page_1","title":"t","created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z","thumbnail":{"status":"empty"}}"#,
    )
    .expect("pre-v5 page should parse");
    assert_eq!(page.paper, Paper::None);

    let create: CreatePageRequest =
        serde_json::from_str(r#"{"title":"t"}"#).expect("pre-v5 create should parse");
    assert_eq!(create.paper, Paper::None);

    let welcome: PageServerMessage =
        serde_json::from_str(r#"{"type":"welcome","session_id":"session_1","last_seq":0}"#)
            .expect("pre-v5 welcome should parse");
    match welcome {
        PageServerMessage::Welcome { paper, .. } => assert_eq!(paper, Paper::None),
        other => panic!("expected welcome, got {other:?}"),
    }

    // Unknown paper values are rejected outright rather than silently blanked —
    // a typo must never be persisted as "no paper".
    assert!(serde_json::from_str::<Paper>(r#""ruled-margin-huge""#).is_err());
}

#[test]
fn deserializes_paper_channel_fixtures() {
    let set: PageClientMessage = serde_json::from_str(&fixture("page-client-set-paper.json"))
        .expect("set-paper fixture should parse");
    match set {
        PageClientMessage::SetPaper {
            ref client_mutation_id,
            paper,
        } => {
            assert_eq!(client_mutation_id, "paper_fixture_1");
            assert_eq!(paper, Paper::RuledMarginNarrow);
        }
        other => panic!("expected set-paper, got {other:?}"),
    }
    let reserialized = serde_json::to_value(&set).expect("set-paper should serialize");
    assert_eq!(reserialized["type"], "set-paper");
    assert_eq!(reserialized["paper"], "ruled-margin-narrow");

    let changed: PageServerMessage =
        serde_json::from_str(&fixture("page-server-paper-changed.json"))
            .expect("paper-changed fixture should parse");
    match changed {
        PageServerMessage::PaperChanged { paper, revision } => {
            assert_eq!(paper, Paper::RuledMarginNarrow);
            assert_eq!(revision, 3);
        }
        other => panic!("expected paper-changed, got {other:?}"),
    }

    let welcome: PageServerMessage = serde_json::from_str(&fixture("page-server-welcome.json"))
        .expect("welcome fixture should parse");
    match welcome {
        PageServerMessage::Welcome { paper, .. } => assert_eq!(paper, Paper::SquaredSmall),
        other => panic!("expected welcome, got {other:?}"),
    }
}

// ---- Shared paper geometry golden ----------------------------------------
//
// `contracts/fixtures/paper-geometry.json` is the cross-language lock between
// `protocol::paper` and its Kotlin mirror (`ink/Paper.kt`): both sides assert
// the *same* file, so parity is proved rather than eyeballed. Regenerate with
// `cargo test -p protocol --test contracts regenerate_paper_geometry_golden -- --ignored`
// after an intentional geometry change, and update the Kotlin mirror in the
// same commit.

const PAPER_GEOMETRY_FIXTURE: &str = "paper-geometry.json";

/// Viewports chosen to pin the behaviours that are easy to get wrong in a
/// second implementation: negative coordinates, the per-family cull, the
/// never-culled margin, margin clipped out of view, and a thumbnail-scale view.
fn paper_geometry_cases() -> Vec<(&'static str, Paper, WorldViewport)> {
    let mut cases: Vec<(&'static str, Paper, WorldViewport)> = Paper::ALL
        .into_iter()
        .map(|paper| {
            (
                "origin-window",
                paper,
                WorldViewport::new(-200.0, -200.0, 400.0, 350.0, 1.0),
            )
        })
        .collect();
    cases.extend([
        (
            "offset-window",
            Paper::SquaredSmall,
            WorldViewport::new(37.0, -211.0, 154.0, -19.0, 2.0),
        ),
        // The cull is graded by pitch, so at one scale the fine paper vanishes
        // while the coarse one still draws. (A squared paper's two families
        // share a pitch, so they always cull together — the grading shows up
        // between papers, not between one paper's axes.)
        (
            "fine-paper-culled",
            Paper::SquaredSmall,
            WorldViewport::new(-500.0, -500.0, 500.0, 500.0, 0.035),
        ),
        (
            "coarse-paper-kept",
            Paper::SquaredLarge,
            WorldViewport::new(-500.0, -500.0, 500.0, 500.0, 0.035),
        ),
        (
            "rules-culled-margin-kept",
            Paper::RuledMarginNarrow,
            WorldViewport::new(-5000.0, -5000.0, 5000.0, 5000.0, 0.001),
        ),
        (
            "margin-out-of-view",
            Paper::RuledMarginWide,
            WorldViewport::new(500.0, -100.0, 900.0, 100.0, 1.0),
        ),
        (
            "margin-aligned-with-grid",
            Paper::SquaredSmall,
            WorldViewport::new(0.0, 0.0, 600.0, 200.0, 1.0),
        ),
        (
            "thumbnail-scale",
            Paper::RuledMarginWide,
            WorldViewport::new(-12.0, -8.0, 228.0, 152.0, 1.0),
        ),
    ]);
    cases
}

fn paper_geometry_golden() -> serde_json::Value {
    let constants = serde_json::json!({
        "rule_spacing_narrow": RULE_SPACING_NARROW,
        "rule_spacing_wide": RULE_SPACING_WIDE,
        "grid_spacing_small": GRID_SPACING_SMALL,
        "grid_spacing_large": GRID_SPACING_LARGE,
        "margin_x": MARGIN_X,
        "rule_line_width": RULE_LINE_WIDTH,
        "margin_line_width": MARGIN_LINE_WIDTH,
        "rule_color": RULE_COLOR,
        "margin_color": MARGIN_COLOR,
        "min_paper_mark_device_pitch": MIN_PAPER_MARK_DEVICE_PITCH,
        "min_paper_mark_device_width": MIN_PAPER_MARK_DEVICE_WIDTH,
        "max_paper_marks_per_axis": MAX_PAPER_MARKS_PER_AXIS,
    });
    let papers: Vec<serde_json::Value> = Paper::ALL
        .into_iter()
        .map(|paper| {
            serde_json::json!({
                "wire_value": paper.wire_value(),
                "label": paper.label(),
                "rule_spacing": paper.rule_spacing(),
                "column_spacing": paper.column_spacing(),
                "has_margin": paper.has_margin(),
            })
        })
        .collect();
    let cases: Vec<serde_json::Value> = paper_geometry_cases()
        .into_iter()
        .map(|(name, paper, viewport)| {
            let marks: Vec<serde_json::Value> = paper_marks(paper, &viewport)
                .into_iter()
                .map(|mark| {
                    serde_json::json!({
                        "kind": mark.kind,
                        "position": mark.position,
                        "world_width": mark.world_width(),
                        "color": mark.kind.color(),
                    })
                })
                .collect();
            serde_json::json!({
                "name": name,
                "paper": paper.wire_value(),
                "viewport": {
                    "min_x": viewport.min_x,
                    "min_y": viewport.min_y,
                    "max_x": viewport.max_x,
                    "max_y": viewport.max_y,
                    "scale": viewport.scale,
                },
                "marks": marks,
            })
        })
        .collect();
    // The grain is part of the shared spec, so the golden pins it as well.
    // Sampling plus a checksum keeps the fixture small while still failing on a
    // single divergent cell.
    let tile = protocol::paper_texture_tile();
    let checksum = tile.iter().enumerate().fold(0u64, |acc, (index, alpha)| {
        acc.wrapping_mul(31)
            .wrapping_add((index as u64) ^ u64::from(*alpha))
    });
    let samples: Vec<serde_json::Value> = [
        (0u32, 0u32),
        (1, 0),
        (7, 3),
        (13, 29),
        (31, 31),
        (32, 48),
        (63, 63),
    ]
    .into_iter()
    .map(|(x, y)| {
        serde_json::json!({ "x": x, "y": y, "alpha": protocol::paper_texture_alpha(x, y) })
    })
    .collect();
    let texture = serde_json::json!({
        "tile_size": protocol::PAPER_TEXTURE_TILE_SIZE,
        "color": protocol::PAPER_TEXTURE_COLOR,
        "max_alpha": protocol::PAPER_TEXTURE_MAX_ALPHA,
        "covered_cells": tile.iter().filter(|alpha| **alpha > 0).count(),
        // A string, not a number: the fold is a u64 and exceeds both
        // `Long.MAX_VALUE` and JavaScript's exact-integer range, so any JSON
        // consumer that parsed it as a number could silently round it.
        "checksum": checksum.to_string(),
        "samples": samples,
    });

    serde_json::json!({
        "constants": constants,
        "papers": papers,
        "cases": cases,
        "texture": texture,
    })
}

#[test]
fn paper_geometry_matches_golden() {
    let golden: serde_json::Value = serde_json::from_str(&fixture(PAPER_GEOMETRY_FIXTURE))
        .expect("paper geometry golden should parse");
    assert_eq!(
        paper_geometry_golden(),
        golden,
        "paper geometry drifted from the shared golden; if intentional, regenerate \
         it and update the Kotlin mirror in the same commit"
    );
    // The golden is only a lock if it actually pins marks.
    let total: usize = golden["cases"]
        .as_array()
        .expect("cases array")
        .iter()
        .map(|case| case["marks"].as_array().expect("marks array").len())
        .sum();
    assert!(total > 40, "golden should pin a meaningful number of marks");
}

#[test]
#[ignore = "regenerates the shared paper geometry golden"]
fn regenerate_paper_geometry_golden() {
    let json = serde_json::to_string_pretty(&paper_geometry_golden())
        .expect("paper geometry golden should serialize");
    fs::write(fixture_path(PAPER_GEOMETRY_FIXTURE), format!("{json}\n"))
        .expect("paper geometry golden should be writable");
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

    let updated: LibraryEvent = serde_json::from_str(&fixture("library-event-page-updated.json"))
        .expect("page-updated event fixture should parse");
    match &updated {
        LibraryEvent::PageUpdated {
            page_id,
            updated_at,
        } => {
            assert!(!page_id.is_empty());
            assert!(!updated_at.is_empty());
        }
        other => panic!("expected page-updated event, got {other:?}"),
    }
    let reserialized = serde_json::to_value(&updated).expect("page-updated should serialize");
    assert_eq!(reserialized["type"], "page-updated");
}
