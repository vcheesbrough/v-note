use protocol::{LibraryEvent, Stroke, ThumbnailMetadata};
use sqlx::PgPool;
use std::time::Instant;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform,
};

use crate::AppState;

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;
const PADDING: f32 = 12.0;
const MIN_THUMBNAIL_STROKE_WIDTH: f32 = 20.0;

pub fn recover_pending(state: AppState) {
    let Some(pool) = state.db.clone() else {
        return;
    };
    tokio::spawn(async move {
        let pending = sqlx::query_as::<_, (String, String, i64)>(
            r#"SELECT t.page_id, p.owner_id, t.source_seq
               FROM page_thumbnails t
               JOIN pages p ON p.id = t.page_id
               WHERE t.status = 'generating'"#,
        )
        .fetch_all(&pool)
        .await;
        let pending = match pending {
            Ok(pending) => pending,
            Err(error) => {
                crate::observability::metrics().record_thumbnail_recovery("error");
                tracing::error!(%error, "could not recover pending thumbnail generation");
                return;
            }
        };
        for (page_id, owner_id, source_seq) in pending {
            let source_seq = source_seq as u64;
            crate::observability::metrics().record_thumbnail_recovery("queued");
            crate::observability::metrics().thumbnail_generation_queued();
            state.realtime.publish_library_event(
                &owner_id,
                LibraryEvent::PageThumbnailUpdated {
                    page_id: page_id.clone(),
                    thumbnail: ThumbnailMetadata::Generating { source_seq },
                },
            );
            enqueue(state.clone(), page_id, owner_id, source_seq);
        }
    });
}

pub fn enqueue(state: AppState, page_id: String, owner_id: String, source_seq: u64) {
    tokio::spawn(async move {
        let Some(pool) = state.db.as_ref() else {
            return;
        };
        let started = Instant::now();
        let result = generate(pool, &page_id, source_seq).await;
        let thumbnail = match result {
            Ok(()) => {
                crate::observability::metrics()
                    .record_page_mutation("generate_thumbnail", "success");
                crate::observability::metrics()
                    .record_thumbnail_generation("success", started.elapsed().as_secs_f64());
                ThumbnailMetadata::Available {
                    source_seq,
                    url: thumbnail_url(&page_id, source_seq),
                }
            }
            Err(error) => {
                crate::observability::metrics().record_page_mutation("generate_thumbnail", "error");
                crate::observability::metrics()
                    .record_thumbnail_generation("error", started.elapsed().as_secs_f64());
                tracing::error!(%error, %page_id, source_seq, "thumbnail generation failed");
                let _ = sqlx::query(
                    "UPDATE page_thumbnails SET status = 'failed', png = NULL WHERE page_id = $1 AND source_seq = $2",
                )
                .bind(&page_id)
                .bind(source_seq as i64)
                .execute(pool)
                .await;
                ThumbnailMetadata::Failed { source_seq }
            }
        };
        crate::observability::metrics().thumbnail_generation_finished();
        state.realtime.publish_library_event(
            &owner_id,
            LibraryEvent::PageThumbnailUpdated { page_id, thumbnail },
        );
    });
}

pub fn thumbnail_url(page_id: &str, source_seq: u64) -> String {
    format!("/api/pages/{page_id}/thumbnails/{source_seq}")
}

async fn generate(pool: &PgPool, page_id: &str, source_seq: u64) -> Result<(), String> {
    let batches = sqlx::query_scalar::<_, sqlx::types::Json<Vec<Stroke>>>(
        "SELECT strokes FROM stroke_batches WHERE page_id = $1 AND revision <= $2 ORDER BY revision",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    let tombstones: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND deleted_revision <= $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?
    .into_iter()
    .collect();
    let strokes: Vec<Stroke> = batches
        .into_iter()
        .flat_map(|batch| batch.0)
        .filter(|stroke| !tombstones.contains(&stroke.id))
        .collect();
    let png = render(&strokes)?;
    let png_bytes = png.len();
    sqlx::query(
        "UPDATE page_thumbnails SET status = 'available', png = $3 WHERE page_id = $1 AND source_seq = $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .bind(png)
    .execute(pool)
    .await
    .map_err(|error| error.to_string())?;
    crate::observability::metrics().observe_thumbnail_artifact_bytes(png_bytes);
    cleanup_best_effort(pool, page_id).await;
    Ok(())
}

fn render(strokes: &[Stroke]) -> Result<Vec<u8>, String> {
    let points: Vec<_> = strokes
        .iter()
        .flat_map(|stroke| stroke.points.iter())
        .collect();
    let (min_x, max_x, min_y, max_y) = points.iter().fold(
        (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ),
        |(min_x, max_x, min_y, max_y), point| {
            (
                min_x.min(point.x),
                max_x.max(point.x),
                min_y.min(point.y),
                max_y.max(point.y),
            )
        },
    );
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT).ok_or("could not allocate thumbnail")?;
    pixmap.fill(Color::WHITE);
    let bounds_w = (max_x - min_x) as f32;
    let bounds_h = (max_y - min_y) as f32;
    let content_w = bounds_w.max(1.0);
    let content_h = bounds_h.max(1.0);
    let scale = if bounds_w == 0.0 && bounds_h == 0.0 {
        1.0
    } else {
        ((WIDTH as f32 - PADDING * 2.0) / content_w)
            .min((HEIGHT as f32 - PADDING * 2.0) / content_h)
    };
    let content_min_x = min_x as f32
        - if bounds_w == 0.0 {
            content_w / 2.0
        } else {
            0.0
        };
    let content_min_y = min_y as f32
        - if bounds_h == 0.0 {
            content_h / 2.0
        } else {
            0.0
        };
    let offset_x = (WIDTH as f32 - content_w * scale) / 2.0 - content_min_x * scale;
    let offset_y = (HEIGHT as f32 - content_h * scale) / 2.0 - content_min_y * scale;
    for stroke in strokes {
        if stroke.validate().is_err() || stroke.points.is_empty() {
            continue;
        }
        let color = &stroke.style.parameters.color;
        let red = u8::from_str_radix(&color[1..3], 16).map_err(|error| error.to_string())?;
        let green = u8::from_str_radix(&color[3..5], 16).map_err(|error| error.to_string())?;
        let blue = u8::from_str_radix(&color[5..7], 16).map_err(|error| error.to_string())?;
        let mut paint = Paint::default();
        paint.set_color_rgba8(red, green, blue, 0xff);
        let transform = Transform::from_scale(scale, scale).post_translate(offset_x, offset_y);

        // Pressure-modulated (v2) strokes render as per-segment variable-width
        // ribbons; v1 keeps the single constant-width path below byte-identical.
        if stroke.style.is_pressure_sensitive() {
            render_pressure_stroke(&mut pixmap, stroke, &paint, transform, scale);
            continue;
        }

        let pen = SkiaStroke {
            // Fitting wide world-space ink must not turn the preview into hairlines.
            width: (stroke.style.parameters.width as f32 * scale).max(MIN_THUMBNAIL_STROKE_WIDTH),
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        if stroke.points.len() == 1 {
            let point = &stroke.points[0];
            if let Some(dot) =
                PathBuilder::from_circle(point.x as f32, point.y as f32, pen.width / (2.0 * scale))
            {
                pixmap.fill_path(&dot, &paint, FillRule::Winding, transform, None);
            }
            continue;
        }
        let mut path = PathBuilder::new();
        path.move_to(stroke.points[0].x as f32, stroke.points[0].y as f32);
        for point in stroke.points.iter().skip(1) {
            path.line_to(point.x as f32, point.y as f32);
        }
        if let Some(path) = path.finish() {
            pixmap.stroke_path(&path, &paint, &pen, transform, None);
        }
    }
    pixmap.encode_png().map_err(|error| error.to_string())
}

/// Rasterize one pressure-sensitive (`solid_round` v2) stroke as a chain of
/// round-capped segments, each drawn at the mean of its endpoints' pressure
/// widths. Round caps overlap consecutive segments so joins stay continuous.
/// The full-width preview floor ([`MIN_THUMBNAIL_STROKE_WIDTH`]) is applied as a
/// single per-stroke boost, so at full pressure the thickest part matches the v1
/// ceiling while lighter pressure narrows proportionally.
fn render_pressure_stroke(
    pixmap: &mut Pixmap,
    stroke: &Stroke,
    paint: &Paint,
    transform: Transform,
    scale: f32,
) {
    let style = &stroke.style;
    let full_width_px = style.parameters.width as f32 * scale;
    let boost = if full_width_px > 0.0 {
        (MIN_THUMBNAIL_STROKE_WIDTH / full_width_px).max(1.0)
    } else {
        1.0
    };
    // Screen-space nib diameter for a point, using the shared width curve.
    let nib_px = |pressure: Option<f64>| style.rendered_width(pressure) as f32 * scale * boost;

    if stroke.points.len() == 1 {
        let point = &stroke.points[0];
        let radius_world = nib_px(point.pressure) / (2.0 * scale);
        if let Some(dot) = PathBuilder::from_circle(point.x as f32, point.y as f32, radius_world) {
            pixmap.fill_path(&dot, paint, FillRule::Winding, transform, None);
        }
        return;
    }

    for pair in stroke.points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let seg_width = (nib_px(a.pressure) + nib_px(b.pressure)) / 2.0;
        let pen = SkiaStroke {
            width: seg_width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        let mut path = PathBuilder::new();
        path.move_to(a.x as f32, a.y as f32);
        path.line_to(b.x as f32, b.y as f32);
        if let Some(path) = path.finish() {
            pixmap.stroke_path(&path, paint, &pen, transform, None);
        }
    }
}

async fn cleanup(pool: &PgPool, page_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"DELETE FROM page_thumbnails
           WHERE page_id = $1
             AND source_seq <> (SELECT ink_revision FROM pages WHERE id = $1)
             AND (
             created_at < now() - interval '7 days' OR source_seq NOT IN (
               SELECT source_seq FROM page_thumbnails
               WHERE page_id = $1
             AND source_seq <> (SELECT ink_revision FROM pages WHERE id = $1)
                 AND created_at >= now() - interval '7 days'
               ORDER BY source_seq DESC LIMIT 10
             )
           )"#,
    )
    .bind(page_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn cleanup_best_effort(pool: &PgPool, page_id: &str) {
    match cleanup(pool, page_id).await {
        Ok(()) => {
            crate::observability::metrics().record_page_mutation("cleanup_thumbnails", "success");
        }
        Err(error) => {
            crate::observability::metrics().record_page_mutation("cleanup_thumbnails", "error");
            tracing::warn!(%error, %page_id, "thumbnail retention cleanup failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{StrokePoint, StrokeStyle};
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[test]
    fn renders_png_with_canonical_ink_colour() {
        let bytes = render(&[Stroke {
            id: "stroke_1".to_string(),
            style: StrokeStyle::default_solid_round(),
            points: vec![
                StrokePoint {
                    x: 0.0,
                    y: 0.0,
                    t: 0,
                    pressure: None,
                },
                StrokePoint {
                    x: 100.0,
                    y: 50.0,
                    t: 10,
                    pressure: None,
                },
            ],
        }])
        .expect("thumbnail should render");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(bytes.len() > 100);
    }

    #[test]
    fn renders_single_point_strokes_as_dots() {
        let bytes = render(&[Stroke {
            id: "stroke_1".to_string(),
            style: StrokeStyle::default_solid_round(),
            points: vec![StrokePoint {
                x: 50.0,
                y: 50.0,
                t: 0,
                pressure: None,
            }],
        }])
        .expect("thumbnail should render");
        let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");
        let (min_x, max_x, min_y, max_y) = ink_bounds(&pixmap);
        assert!((18..=22).contains(&(max_x - min_x + 1)));
        assert!((18..=22).contains(&(max_y - min_y + 1)));
        assert!((min_x + max_x).abs_diff(WIDTH - 1) <= 1);
        assert!((min_y + max_y).abs_diff(HEIGHT - 1) <= 1);
    }

    /// Green vertical extent (thickness in px) of the rasterized ink at column `x`.
    fn column_thickness(pixmap: &Pixmap, x: u32) -> u32 {
        let mut lo = HEIGHT;
        let mut hi = 0;
        let mut found = false;
        for y in 0..HEIGHT {
            let pixel = pixmap.pixel(x, y).expect("pixel should exist");
            if pixel.green() > pixel.red() && pixel.green() > pixel.blue() {
                lo = lo.min(y);
                hi = hi.max(y);
                found = true;
            }
        }
        if found {
            hi - lo + 1
        } else {
            0
        }
    }

    /// A v2 stroke whose pressure ramps 0 → 1 must be visibly thinner at the
    /// low-pressure end than the high-pressure end.
    #[test]
    fn pressure_stroke_widens_with_pressure() {
        // Many coalesced samples (as real ink produces) so each short segment
        // takes a progressively larger per-segment width across the ramp.
        let samples = 41;
        let points: Vec<StrokePoint> = (0..samples)
            .map(|i| {
                let f = i as f64 / (samples - 1) as f64;
                StrokePoint { x: f * 200.0, y: 50.0, t: i as i64, pressure: Some(f) }
            })
            .collect();
        let bytes = render(&[Stroke {
            id: "pressure_ramp".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points,
        }])
        .expect("thumbnail should render");
        let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");
        let (min_x, max_x, _, _) = ink_bounds(&pixmap);
        // Sample a few px inside each end to avoid the round-cap taper.
        let low = column_thickness(&pixmap, min_x + 5);
        let high = column_thickness(&pixmap, max_x - 5);
        assert!(
            high > low + 2,
            "high-pressure end must be thicker: low={low}px high={high}px"
        );
    }

    /// At full pressure a v2 stroke reaches the same nib width as the constant v1
    /// pen — the pressure model only *narrows* below the preset width.
    #[test]
    fn full_pressure_matches_v1_width() {
        let points = vec![StrokePoint { x: 50.0, y: 50.0, t: 0, pressure: Some(1.0) }];
        let v2 = render(&[Stroke {
            id: "v2_full".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: points.clone(),
        }])
        .expect("v2 thumbnail should render");
        let v1 = render(&[Stroke {
            id: "v1".to_string(),
            style: StrokeStyle::default_solid_round(),
            points: vec![StrokePoint { x: 50.0, y: 50.0, t: 0, pressure: None }],
        }])
        .expect("v1 thumbnail should render");
        let v2_dot = {
            let pm = Pixmap::decode_png(&v2).expect("decode");
            let (min_x, max_x, _, _) = ink_bounds(&pm);
            max_x - min_x + 1
        };
        let v1_dot = {
            let pm = Pixmap::decode_png(&v1).expect("decode");
            let (min_x, max_x, _, _) = ink_bounds(&pm);
            max_x - min_x + 1
        };
        assert!(
            v2_dot.abs_diff(v1_dot) <= 1,
            "full-pressure v2 dot {v2_dot}px should match v1 dot {v1_dot}px"
        );
    }

    /// A v2 stroke carrying a stray v1-style bare point (no pressure) is still
    /// valid and renders (that point at full width); a v1 stroke with a stray
    /// pressure value is rejected and skipped.
    #[test]
    fn rejects_pressure_on_v1_but_renders_mixed_v2() {
        let out = render(&[
            Stroke {
                id: "v1_with_pressure".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![
                    StrokePoint { x: 0.0, y: 0.0, t: 0, pressure: Some(0.5) },
                    StrokePoint { x: 40.0, y: 40.0, t: 5, pressure: Some(0.5) },
                ],
            },
            Stroke {
                id: "v2_mixed".to_string(),
                style: StrokeStyle::default_solid_round_pressure(),
                points: vec![
                    StrokePoint { x: 60.0, y: 10.0, t: 0, pressure: Some(0.2) },
                    StrokePoint { x: 120.0, y: 60.0, t: 5, pressure: None },
                ],
            },
        ])
        .expect("thumbnail should render");
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        // The invalid v1-with-pressure stroke is skipped; the valid v2 stroke draws.
        let pixmap = Pixmap::decode_png(&out).expect("decode");
        let (min_x, max_x, min_y, max_y) = ink_bounds(&pixmap);
        assert!(min_x <= max_x && min_y <= max_y, "v2 stroke should be drawn");
    }

    // A fixed v1 page (multi-point stroke + a dot) whose rasterization is pinned
    // as a golden so the untouched constant-width path can never silently drift.
    fn canonical_v1_page() -> Vec<Stroke> {
        vec![
            Stroke {
                id: "v1_line".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![
                    StrokePoint { x: 10.0, y: 20.0, t: 0, pressure: None },
                    StrokePoint { x: 120.0, y: 90.0, t: 8, pressure: None },
                    StrokePoint { x: 220.0, y: 30.0, t: 16, pressure: None },
                ],
            },
            Stroke {
                id: "v1_dot".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![StrokePoint { x: 180.0, y: 110.0, t: 0, pressure: None }],
            },
        ]
    }

    const V1_GOLDEN_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/v1_canonical_thumbnail.png");

    /// v1 rasterization must stay byte-identical. Compares decoded RGBA pixels
    /// (robust to PNG encoder differences) against the committed golden.
    #[test]
    fn v1_thumbnail_matches_golden() {
        let bytes = render(&canonical_v1_page()).expect("thumbnail should render");
        let rendered = Pixmap::decode_png(&bytes).expect("rendered thumbnail should decode");
        let golden_bytes = std::fs::read(V1_GOLDEN_PATH)
            .expect("v1 golden present; regenerate with `--ignored regenerate_v1_golden`");
        let golden = Pixmap::decode_png(&golden_bytes).expect("golden should decode");
        assert_eq!((rendered.width(), rendered.height()), (golden.width(), golden.height()));
        assert_eq!(
            rendered.data(),
            golden.data(),
            "v1 thumbnail rasterization drifted from the committed golden"
        );
    }

    /// Rewrites the committed v1 golden. Ignored by default; run with
    /// `cargo test -p server --lib regenerate_v1_golden -- --ignored` after an
    /// intentional v1 rendering change (review the image diff before committing).
    #[test]
    #[ignore = "regenerates the committed v1 golden image"]
    fn regenerate_v1_golden() {
        let bytes = render(&canonical_v1_page()).expect("thumbnail should render");
        std::fs::write(V1_GOLDEN_PATH, bytes).expect("golden should be writable");
    }

    #[tokio::test]
    async fn cleanup_failure_is_non_fatal() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://localhost/v_note")
            .expect("test pool URL should parse");
        pool.close().await;

        cleanup_best_effort(&pool, "page_cleanup_failure").await;
    }

    fn ink_bounds(pixmap: &Pixmap) -> (u32, u32, u32, u32) {
        let mut min_x = WIDTH;
        let mut max_x = 0;
        let mut min_y = HEIGHT;
        let mut max_y = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = pixmap.pixel(x, y).expect("pixel should exist");
                if pixel.green() > pixel.red() && pixel.green() > pixel.blue() {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        assert!(min_x <= max_x, "thumbnail should contain canonical ink");
        assert!(min_y <= max_y, "thumbnail should contain canonical ink");
        (min_x, max_x, min_y, max_y)
    }
}
