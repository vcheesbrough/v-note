use protocol::{
    LibraryEvent, Paper, Stroke, ThumbnailMetadata, WorldViewport, paper_mark_device_width,
    visit_paper_marks,
};
use sqlx::PgPool;
use std::time::Instant;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform,
};

use crate::AppState;

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;
const PADDING: f32 = 12.0;
// Preview-only width floor so heavily scaled-down ink stays visible without
// turning into hairlines. Was 20.0, which on a full page of handwriting (small
// scale, so the floor dominates every stroke) merged all ink into blobs.
const MIN_THUMBNAIL_STROKE_WIDTH: f32 = 4.0;

pub fn recover_pending(state: AppState) {
    let Some(pool) = state.db.clone() else {
        return;
    };
    tokio::spawn(async move {
        // No paper is carried here on purpose: `generate` reads it from the job
        // row it is about to render, so a job recovered after a restart can never
        // pick up a paper the page has moved on to since. See `generate`.
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
    // Thumbnails are immutable per revision, so the paper comes from the *job
    // row* — the paper in force when this revision was minted — never from
    // `pages.paper`, which may already name a later choice. This is the only
    // place paper is read for rendering, so no caller can pass a stale value
    // (notably `recover_pending`, which re-queues jobs after a restart with no
    // in-memory context at all).
    let paper: String = sqlx::query_scalar(
        "SELECT paper FROM page_thumbnails WHERE page_id = $1 AND source_seq = $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .fetch_optional(pool)
    .await
    .map_err(|error| error.to_string())?
    .ok_or("thumbnail job row is missing")?;
    let paper =
        Paper::from_wire(&paper).ok_or_else(|| format!("unknown stored paper {paper:?}"))?;

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
    let png = render(paper, &strokes)?;
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

fn render(paper: Paper, strokes: &[Stroke]) -> Result<Vec<u8>, String> {
    // Bound only the strokes actually drawn below (the loop skips the same set):
    // a skipped invalid stroke with far-away points must not stretch the scale
    // and shrink the valid ink.
    let points: Vec<_> = strokes
        .iter()
        .filter(|stroke| !stroke.points.is_empty() && stroke.validate().is_ok())
        .flat_map(|stroke| stroke.points.iter())
        .collect();
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT).ok_or("could not allocate thumbnail")?;
    pixmap.fill(Color::WHITE);
    if points.is_empty() {
        // With no drawable points the bounds fold below yields ±INFINITY and the
        // derived scale/offsets are garbage. That was harmless while only the
        // (empty) stroke loop consumed them, but paper enumeration consumes them
        // too and would walk an infinite viewport — an all-erased page is a job
        // `persist_tombstones` really creates. A blank page has no ink to anchor
        // paper to, and the card forbids paper-only thumbnails, so return white.
        return pixmap.encode_png().map_err(|error| error.to_string());
    }
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
    // Paper shares the ink transform, so its size and position relative to the
    // ink match the SPA and Android exactly. Drawn after the white fill and
    // before every stroke, so it can never overpaint ink.
    draw_paper(&mut pixmap, paper, scale, offset_x, offset_y);
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

        // `stroke_path`/`fill_path` apply `transform` to width the same as to
        // geometry (confirmed empirically: a world-space width scales with the
        // transform's scale factor), so the floor must be expressed in world
        // units — dividing the device-space floor by `scale` — rather than
        // pre-multiplying by `scale` and letting the transform scale it again.
        let width_world =
            (stroke.style.parameters.width as f32).max(MIN_THUMBNAIL_STROKE_WIDTH / scale);
        let pen = SkiaStroke {
            width: width_world,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        if stroke.points.len() == 1 {
            let point = &stroke.points[0];
            if let Some(dot) =
                PathBuilder::from_circle(point.x as f32, point.y as f32, width_world / 2.0)
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

/// Lay the faint paper grain over the whole pixmap, under both the rules and
/// the ink.
///
/// Tiled in **device space**, so the grain keeps a constant size regardless of
/// how far the page's ink had to be scaled to fit the preview — exactly as it
/// does on the live canvases. Composited straight into the pixmap rather than
/// through a `Pattern` shader: the tile is small, the surface is 240×160, and a
/// direct blend keeps the alpha maths obvious and identical to the clients'.
fn draw_paper_texture(pixmap: &mut Pixmap, paper: Paper) {
    if !protocol::paper_has_texture(paper) {
        return;
    }
    let tile = protocol::paper_texture_tile();
    let size = protocol::PAPER_TEXTURE_TILE_SIZE;
    let [grain_r, grain_g, grain_b] = protocol::PAPER_TEXTURE_COLOR_RGB;
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    let pixels = pixmap.pixels_mut();

    for y in 0..height {
        for x in 0..width {
            let alpha = tile[(y % size) * size + (x % size)];
            if alpha == 0 {
                continue;
            }
            let index = y * width + x;
            let base = pixels[index];
            // Source-over with an opaque backdrop, so the result stays opaque
            // and the blend is a plain lerp toward the grain colour.
            let blend = |dst: u8, src: u8| -> u8 {
                let dst = u32::from(dst);
                let src = u32::from(src);
                let a = u32::from(alpha);
                ((src * a + dst * (255 - a) + 127) / 255) as u8
            };
            if let Some(mixed) = tiny_skia::ColorU8::from_rgba(
                blend(base.red(), grain_r),
                blend(base.green(), grain_g),
                blend(base.blue(), grain_b),
                255,
            )
            .premultiply()
            .into()
            {
                pixels[index] = mixed;
            }
        }
    }
}

/// Rasterize the page's paper behind the ink, under the *same* transform the
/// stroke loop uses. The world viewport is the exact inverse of that transform
/// over the whole `0..WIDTH × 0..HEIGHT` pixmap, so marks are enumerated for
/// precisely the area being painted.
///
/// Marks use `Butt` caps (a `Round` cap would bulge each line's ends past the
/// viewport edge) and the paper's own [`protocol::MIN_PAPER_MARK_DEVICE_WIDTH`]
/// floor — emphatically *not* the ink preview floor
/// [`MIN_THUMBNAIL_STROKE_WIDTH`], which would thicken the rules well past the
/// hairlines they are on the live canvases.
fn draw_paper(pixmap: &mut Pixmap, paper: Paper, scale: f32, offset_x: f32, offset_y: f32) {
    if paper == Paper::None || scale <= 0.0 || !scale.is_finite() {
        return;
    }
    draw_paper_texture(pixmap, paper);
    let to_world = |device: f32, offset: f32| ((device - offset) / scale) as f64;
    let viewport = WorldViewport::new(
        to_world(0.0, offset_x),
        to_world(0.0, offset_y),
        to_world(WIDTH as f32, offset_x),
        to_world(HEIGHT as f32, offset_y),
        scale as f64,
    );
    let transform = Transform::from_scale(scale, scale).post_translate(offset_x, offset_y);

    // Only two (colour, width) pairs exist across every mark, so build them once
    // rather than reallocating a Paint and a Stroke for each line.
    let style_for = |kind: protocol::PaperMarkKind| {
        let [red, green, blue] = kind.color_rgb();
        let mut paint = Paint::default();
        paint.set_color_rgba8(red, green, blue, 0xff);
        // The pen width is in world units (the transform scales it), so undo the
        // scale on the device-space floor.
        let pen = SkiaStroke {
            width: paper_mark_device_width(kind.world_width(), scale as f64) as f32 / scale,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            ..Default::default()
        };
        (paint, pen)
    };
    let (rule_paint, rule_pen) = style_for(protocol::PaperMarkKind::Rule);
    let (margin_paint, margin_pen) = style_for(protocol::PaperMarkKind::Margin);

    visit_paper_marks(paper, &viewport, |mark| {
        let (paint, pen) = if mark.kind == protocol::PaperMarkKind::Margin {
            (&margin_paint, &margin_pen)
        } else {
            (&rule_paint, &rule_pen)
        };
        let position = mark.position as f32;
        let mut path = PathBuilder::new();
        if mark.kind.is_horizontal() {
            path.move_to(viewport.min_x as f32, position);
            path.line_to(viewport.max_x as f32, position);
        } else {
            path.move_to(position, viewport.min_y as f32);
            path.line_to(position, viewport.max_y as f32);
        }
        if let Some(path) = path.finish() {
            pixmap.stroke_path(&path, paint, pen, transform, None);
        }
    });
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

    // Dot-like strokes (single point, or a tap whose extent is smaller than its
    // own nib) collapse the per-segment ribbon to nothing — render a dot at the
    // largest pressure width instead, matching the Android renderer.
    let (min_x, max_x, min_y, max_y) = stroke.points.iter().fold(
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
    // Axis-aligned max, matching Android's live-canvas heuristic
    // (`maxOf(maxX - minX, maxY - minY) < maxWidth` in PageCanvas.kt) — a
    // bounding-box diagonal would classify some short diagonal strokes
    // differently than Android does for the same ink, so the library preview
    // would disagree with the page itself about the shape of a stroke.
    let extent = (max_x - min_x).max(max_y - min_y) as f32;
    let max_nib = stroke
        .points
        .iter()
        .map(|point| nib_px(point.pressure))
        .fold(0.0_f32, f32::max);
    // Classify against the *unboosted* nib: "is the stroke shorter than its own
    // pen width" is a property of the ink, not of the preview floor. Comparing
    // against the boosted nib collapsed every letter-sized stroke on a dense
    // page (where the floor dominates) into a circle. The dot itself still
    // renders at the boosted width so genuine taps stay visible.
    if stroke.points.len() == 1 || extent * scale < max_nib / boost {
        let center_x = ((min_x + max_x) / 2.0) as f32;
        let center_y = ((min_y + max_y) / 2.0) as f32;
        if let Some(dot) = PathBuilder::from_circle(center_x, center_y, max_nib / (2.0 * scale)) {
            pixmap.fill_path(&dot, paint, FillRule::Winding, transform, None);
        }
        return;
    }

    for pair in stroke.points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // nib_px is a device-space diameter; stroke_path scales width by
        // `transform` the same as geometry, so convert to world space here
        // (matching the dot branch's `max_nib / (2.0 * scale)` above) instead
        // of letting the transform's scale apply a second time.
        let seg_width = (nib_px(a.pressure) + nib_px(b.pressure)) / (2.0 * scale);
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
        let bytes = render(
            Paper::None,
            &[Stroke {
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
            }],
        )
        .expect("thumbnail should render");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(bytes.len() > 100);
    }

    #[test]
    fn renders_single_point_strokes_as_dots() {
        let bytes = render(
            Paper::None,
            &[Stroke {
                id: "stroke_1".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![StrokePoint {
                    x: 50.0,
                    y: 50.0,
                    t: 0,
                    pressure: None,
                }],
            }],
        )
        .expect("thumbnail should render");
        let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");
        let (min_x, max_x, min_y, max_y) = ink_bounds(&pixmap);
        assert!((3..=6).contains(&(max_x - min_x + 1)));
        assert!((3..=6).contains(&(max_y - min_y + 1)));
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
        if found { hi - lo + 1 } else { 0 }
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
                StrokePoint {
                    x: f * 200.0,
                    y: 50.0,
                    t: i as i64,
                    pressure: Some(f),
                }
            })
            .collect();
        let bytes = render(
            Paper::None,
            &[Stroke {
                id: "pressure_ramp".to_string(),
                style: StrokeStyle::default_solid_round_pressure(),
                points,
            }],
        )
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

    /// A v2 tap (coincident down/up points) must still render as a dot, not
    /// collapse the per-segment ribbon to nothing.
    #[test]
    fn pressure_tap_renders_as_dot() {
        let bytes = render(
            Paper::None,
            &[Stroke {
                id: "v2_tap".to_string(),
                style: StrokeStyle::default_solid_round_pressure(),
                points: vec![
                    StrokePoint {
                        x: 50.0,
                        y: 50.0,
                        t: 0,
                        pressure: Some(1.0),
                    },
                    StrokePoint {
                        x: 50.0,
                        y: 50.0,
                        t: 8,
                        pressure: Some(1.0),
                    },
                ],
            }],
        )
        .expect("thumbnail should render");
        let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");
        let (min_x, max_x, min_y, max_y) = ink_bounds(&pixmap);
        // A visible, roughly round blob (not an empty thumbnail).
        assert!((max_x - min_x + 1) >= 3, "tap dot has visible width");
        assert!((max_y - min_y + 1) >= 3, "tap dot has visible height");
    }

    /// Ink bounds of everything drawn on a raw pixmap (any non-white pixel).
    fn drawn_bounds(pixmap: &Pixmap) -> (u32, u32, u32, u32) {
        let mut min_x = WIDTH;
        let mut max_x = 0;
        let mut min_y = HEIGHT;
        let mut max_y = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = pixmap.pixel(x, y).expect("pixel should exist");
                if pixel.red() < 255 || pixel.green() < 255 || pixel.blue() < 255 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        assert!(min_x <= max_x, "expected some ink to be drawn");
        (min_x, max_x, min_y, max_y)
    }

    /// A short **diagonal** v2 stroke (Δx and Δy each individually under the
    /// nib width) must collapse to a dot, matching Android's live-canvas
    /// `maxOf(Δx, Δy) < maxWidth` classifier exactly (`PageCanvas.kt`) — even
    /// though the true diagonal distance is technically larger. Using the
    /// bounding-box diagonal instead would make the library preview disagree
    /// with what the page itself renders for the same stroke.
    #[test]
    fn pressure_stroke_short_diagonal_matches_android_and_collapses_to_dot() {
        let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("thumbnail pixmap should allocate");
        pixmap.fill(Color::WHITE);
        let mut paint = Paint::default();
        paint.set_color_rgba8(0, 0, 0, 0xff);
        // Δx = Δy = 3 world units at scale 1: each axis alone is under the 4px
        // full-pressure nib, though the √2 diagonal (≈4.24) is over it.
        let stroke = Stroke {
            id: "diag".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: vec![
                StrokePoint {
                    x: 20.0,
                    y: 20.0,
                    t: 0,
                    pressure: Some(1.0),
                },
                StrokePoint {
                    x: 23.0,
                    y: 23.0,
                    t: 8,
                    pressure: Some(1.0),
                },
            ],
        };
        render_pressure_stroke(
            &mut pixmap,
            &stroke,
            &paint,
            Transform::from_scale(1.0, 1.0),
            1.0,
        );

        let (min_x, max_x, min_y, max_y) = drawn_bounds(&pixmap);
        let width = max_x - min_x + 1;
        let height = max_y - min_y + 1;
        // A dot is roughly as wide as tall (the 4px nib); a line would be
        // visibly elongated along the diagonal.
        assert!(
            width <= 6 && height <= 6,
            "short diagonal stroke should collapse to a dot, matching Android (width={width} height={height})"
        );
    }

    /// On a dense page (large world extent, so the preview scale is small and
    /// the width floor dominates), letter-sized strokes must still render as
    /// short lines. The old classification compared the stroke extent against
    /// the *boosted* nib, which collapsed every such stroke into a ~20px
    /// circle — a full page of handwriting became rows of circles.
    #[test]
    fn dense_page_short_strokes_stay_lines_not_circles() {
        // A long stroke across the top fixes the page bounds (scale ≈ 0.14);
        // a letter-sized horizontal stroke sits alone in the bottom half.
        let long = Stroke {
            id: "long".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: (0..=10)
                .map(|i| StrokePoint {
                    x: i as f64 * 150.0,
                    y: 0.0,
                    t: i as i64,
                    pressure: Some(0.7),
                })
                .collect(),
        };
        let short = Stroke {
            id: "short".to_string(),
            style: StrokeStyle::default_solid_round_pressure(),
            points: vec![
                StrokePoint {
                    x: 700.0,
                    y: 800.0,
                    t: 0,
                    pressure: Some(0.7),
                },
                StrokePoint {
                    x: 730.0,
                    y: 800.0,
                    t: 8,
                    pressure: Some(0.7),
                },
            ],
        };
        let bytes = render(Paper::None, &[long, short]).expect("thumbnail should render");
        let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");

        // Bounds of the short stroke only: scan the bottom half of the preview.
        // Track the darkest green channel seen too — a stroke rendered at
        // near-zero coverage (e.g. the width-double-scaling bug that made
        // lines ~2px and near-transparent) would still satisfy a bounds-only
        // check while being invisible to a user.
        let mut min_x = WIDTH;
        let mut max_x = 0;
        let mut min_y = HEIGHT;
        let mut max_y = 0;
        let mut darkest_green = 255u8;
        for y in HEIGHT / 2..HEIGHT {
            for x in 0..WIDTH {
                let pixel = pixmap.pixel(x, y).expect("pixel should exist");
                if pixel.green() > pixel.red() && pixel.green() > pixel.blue() {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                    darkest_green = darkest_green.min(pixel.green());
                }
            }
        }
        assert!(min_x <= max_x, "short stroke should render some ink");
        let width = max_x - min_x + 1;
        let height = max_y - min_y + 1;
        // The old collapse drew a ~20px circle here; the fixed renderer draws a
        // floored-width line: a few px tall, wider than tall.
        assert!(
            height <= 8,
            "short stroke should stay a thin line, not a circle (height={height})"
        );
        assert!(
            width > height,
            "short stroke should be elongated, not round (width={width} height={height})"
        );
        // Canonical ink (#006400) has green=100; require getting most of the
        // way there so a near-invisible wash (high green, low coverage) fails.
        assert!(
            darkest_green <= 150,
            "short stroke should reach near-full ink opacity, not a faint wash (darkest green channel = {darkest_green})"
        );
    }

    /// At full pressure a v2 stroke reaches the same nib width as the constant v1
    /// pen — the pressure model only *narrows* below the preset width.
    #[test]
    fn full_pressure_matches_v1_width() {
        let points = vec![StrokePoint {
            x: 50.0,
            y: 50.0,
            t: 0,
            pressure: Some(1.0),
        }];
        let v2 = render(
            Paper::None,
            &[Stroke {
                id: "v2_full".to_string(),
                style: StrokeStyle::default_solid_round_pressure(),
                points: points.clone(),
            }],
        )
        .expect("v2 thumbnail should render");
        let v1 = render(
            Paper::None,
            &[Stroke {
                id: "v1".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![StrokePoint {
                    x: 50.0,
                    y: 50.0,
                    t: 0,
                    pressure: None,
                }],
            }],
        )
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

    /// A skipped (invalid) stroke far off-canvas must not stretch the bounding
    /// box and shrink the valid ink that is actually drawn.
    #[test]
    fn invalid_strokes_excluded_from_thumbnail_bounds() {
        let valid = Stroke {
            id: "valid".to_string(),
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
                    y: 60.0,
                    t: 5,
                    pressure: None,
                },
            ],
        };
        // v1 style carrying pressure => rejected by stroke.validate(), skipped.
        let far_invalid = Stroke {
            id: "far_invalid".to_string(),
            style: StrokeStyle::default_solid_round(),
            points: vec![
                StrokePoint {
                    x: 5000.0,
                    y: 5000.0,
                    t: 0,
                    pressure: Some(0.5),
                },
                StrokePoint {
                    x: 5100.0,
                    y: 5100.0,
                    t: 5,
                    pressure: Some(0.5),
                },
            ],
        };
        let span = |strokes: &[Stroke]| {
            let bytes = render(Paper::None, strokes).expect("thumbnail should render");
            let pixmap = Pixmap::decode_png(&bytes).expect("thumbnail should decode");
            let (min_x, max_x, _, _) = ink_bounds(&pixmap);
            max_x - min_x + 1
        };
        let with_far = span(&[valid.clone(), far_invalid]);
        let only_valid = span(&[valid]);
        assert!(
            with_far.abs_diff(only_valid) <= 2,
            "far invalid stroke rescaled the valid ink: {with_far} vs {only_valid}"
        );
    }

    /// A v2 stroke carrying a stray v1-style bare point (no pressure) is still
    /// valid and renders (that point at full width); a v1 stroke with a stray
    /// pressure value is rejected and skipped.
    #[test]
    fn rejects_pressure_on_v1_but_renders_mixed_v2() {
        let out = render(
            Paper::None,
            &[
                Stroke {
                    id: "v1_with_pressure".to_string(),
                    style: StrokeStyle::default_solid_round(),
                    points: vec![
                        StrokePoint {
                            x: 0.0,
                            y: 0.0,
                            t: 0,
                            pressure: Some(0.5),
                        },
                        StrokePoint {
                            x: 40.0,
                            y: 40.0,
                            t: 5,
                            pressure: Some(0.5),
                        },
                    ],
                },
                Stroke {
                    id: "v2_mixed".to_string(),
                    style: StrokeStyle::default_solid_round_pressure(),
                    points: vec![
                        StrokePoint {
                            x: 60.0,
                            y: 10.0,
                            t: 0,
                            pressure: Some(0.2),
                        },
                        StrokePoint {
                            x: 120.0,
                            y: 60.0,
                            t: 5,
                            pressure: None,
                        },
                    ],
                },
            ],
        )
        .expect("thumbnail should render");
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        // The invalid v1-with-pressure stroke is skipped; the valid v2 stroke draws.
        let pixmap = Pixmap::decode_png(&out).expect("decode");
        let (min_x, max_x, min_y, max_y) = ink_bounds(&pixmap);
        assert!(
            min_x <= max_x && min_y <= max_y,
            "v2 stroke should be drawn"
        );
    }

    // A fixed v1 page (multi-point stroke + a dot) whose rasterization is pinned
    // as a golden so the untouched constant-width path can never silently drift.
    fn canonical_v1_page() -> Vec<Stroke> {
        vec![
            Stroke {
                id: "v1_line".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![
                    StrokePoint {
                        x: 10.0,
                        y: 20.0,
                        t: 0,
                        pressure: None,
                    },
                    StrokePoint {
                        x: 120.0,
                        y: 90.0,
                        t: 8,
                        pressure: None,
                    },
                    StrokePoint {
                        x: 220.0,
                        y: 30.0,
                        t: 16,
                        pressure: None,
                    },
                ],
            },
            Stroke {
                id: "v1_dot".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![StrokePoint {
                    x: 180.0,
                    y: 110.0,
                    t: 0,
                    pressure: None,
                }],
            },
        ]
    }

    const V1_GOLDEN_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/v1_canonical_thumbnail.png"
    );

    /// v1 rasterization must stay byte-identical. Compares decoded RGBA pixels
    /// (robust to PNG encoder differences) against the committed golden.
    #[test]
    fn v1_thumbnail_matches_golden() {
        let bytes = render(Paper::None, &canonical_v1_page()).expect("thumbnail should render");
        let rendered = Pixmap::decode_png(&bytes).expect("rendered thumbnail should decode");
        let golden_bytes = std::fs::read(V1_GOLDEN_PATH)
            .expect("v1 golden present; regenerate with `--ignored regenerate_v1_golden`");
        let golden = Pixmap::decode_png(&golden_bytes).expect("golden should decode");
        assert_eq!(
            (rendered.width(), rendered.height()),
            (golden.width(), golden.height())
        );
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
        let bytes = render(Paper::None, &canonical_v1_page()).expect("thumbnail should render");
        std::fs::write(V1_GOLDEN_PATH, bytes).expect("golden should be writable");
    }

    // ---- Paper ----------------------------------------------------------

    const RULED_GOLDEN_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/ruled_margin_narrow_thumbnail.png"
    );

    /// A page whose fitted viewport reaches past `MARGIN_X`, so the margin is
    /// actually in frame. `canonical_v1_page` spans only x 10..220 and fits to a
    /// viewport ending near x 232 — fine for the ink goldens it exists for, but
    /// with the margin now at 288 it would silently exercise none of the margin
    /// behaviour these tests are about.
    fn margin_reaching_page() -> Vec<Stroke> {
        vec![
            Stroke {
                id: "wide_line".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![
                    StrokePoint {
                        x: 0.0,
                        y: 0.0,
                        t: 0,
                        pressure: None,
                    },
                    StrokePoint {
                        x: 300.0,
                        y: 240.0,
                        t: 8,
                        pressure: None,
                    },
                    StrokePoint {
                        x: 600.0,
                        y: 60.0,
                        t: 16,
                        pressure: None,
                    },
                ],
            },
            Stroke {
                id: "wide_dot".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: vec![StrokePoint {
                    x: 460.0,
                    y: 290.0,
                    t: 0,
                    pressure: None,
                }],
            },
        ]
    }

    /// Rule/grid pixels blend toward white, which preserves their `b > g > r`
    /// channel ordering — and that ordering is disjoint from the green-dominant
    /// ink classifier.
    fn is_rule_pixel(pixel: tiny_skia::PremultipliedColorU8) -> bool {
        pixel.blue() > pixel.green() && pixel.green() > pixel.red()
    }

    /// Margin pixels blend toward white preserving `r > g` and `r > b`.
    fn is_margin_pixel(pixel: tiny_skia::PremultipliedColorU8) -> bool {
        pixel.red() > pixel.green() && pixel.red() > pixel.blue()
    }

    fn count_pixels(
        pixmap: &Pixmap,
        predicate: impl Fn(tiny_skia::PremultipliedColorU8) -> bool,
    ) -> u32 {
        let mut count = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                if predicate(pixmap.pixel(x, y).expect("pixel should exist")) {
                    count += 1;
                }
            }
        }
        count
    }

    fn rendered(paper: Paper, strokes: &[Stroke]) -> Pixmap {
        let bytes = render(paper, strokes).expect("thumbnail should render");
        Pixmap::decode_png(&bytes).expect("thumbnail should decode")
    }

    /// Paper rasterization is pinned as its own golden, so a drift in the mark
    /// geometry, colours or width floor cannot slip through unnoticed.
    #[test]
    fn ruled_margin_narrow_thumbnail_matches_golden() {
        let bytes = render(Paper::RuledMarginNarrow, &margin_reaching_page())
            .expect("thumbnail should render");
        let rendered = Pixmap::decode_png(&bytes).expect("rendered thumbnail should decode");
        let golden_bytes = std::fs::read(RULED_GOLDEN_PATH).expect(
            "ruled golden present; regenerate with `--ignored regenerate_ruled_margin_narrow_golden`",
        );
        let golden = Pixmap::decode_png(&golden_bytes).expect("golden should decode");
        assert_eq!(
            (rendered.width(), rendered.height()),
            (golden.width(), golden.height())
        );
        assert_eq!(
            rendered.data(),
            golden.data(),
            "ruled paper rasterization drifted from the committed golden"
        );
    }

    #[test]
    #[ignore = "regenerates the committed ruled paper golden image"]
    fn regenerate_ruled_margin_narrow_golden() {
        let bytes = render(Paper::RuledMarginNarrow, &margin_reaching_page())
            .expect("thumbnail should render");
        std::fs::write(RULED_GOLDEN_PATH, bytes).expect("golden should be writable");
    }

    /// Every paper family actually reaches the pixmap at thumbnail scale, and
    /// only the margin papers paint a margin.
    #[test]
    fn every_paper_draws_and_only_margin_papers_paint_a_margin() {
        for paper in Paper::ALL {
            let pixmap = rendered(paper, &margin_reaching_page());
            let rules = count_pixels(&pixmap, is_rule_pixel);
            let margin = count_pixels(&pixmap, is_margin_pixel);
            if paper == Paper::None {
                assert_eq!(rules, 0, "blank paper draws no rules");
                assert_eq!(margin, 0, "blank paper draws no margin");
                continue;
            }
            assert!(rules > 0, "{} drew no rules", paper.wire_value());
            if paper.has_margin() {
                assert!(margin > 0, "{} drew no margin", paper.wire_value());
            } else {
                assert_eq!(margin, 0, "{} should have no margin", paper.wire_value());
            }
        }
    }

    /// The grain reaches the pixmap, stays neutral, and stays subtle.
    ///
    /// Neutrality is the load-bearing part: the grain touches roughly a quarter
    /// of every pixel on the surface, so a colour cast would be picked up by the
    /// rule and margin classifiers everywhere and quietly invalidate every other
    /// paper assertion in this file.
    #[test]
    fn texture_covers_the_surface_without_tinting_it() {
        // Exercised in isolation, so antialiased rule edges cannot be mistaken
        // for grain.
        let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("pixmap should allocate");
        pixmap.fill(Color::WHITE);
        draw_paper_texture(&mut pixmap, Paper::RuledNarrow);

        let mut grain = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = pixmap.pixel(x, y).expect("pixel should exist");
                let (red, green, blue) = (pixel.red(), pixel.green(), pixel.blue());
                if (red, green, blue) == (255, 255, 255) {
                    continue;
                }
                grain += 1;
                assert_eq!(red, green, "grain must stay neutral at ({x}, {y})");
                assert_eq!(green, blue, "grain must stay neutral at ({x}, {y})");
                assert!(
                    red >= 240,
                    "grain must stay subtle, got {red} at ({x}, {y})"
                );
                assert!(!is_rule_pixel(pixel) && !is_margin_pixel(pixel));
            }
        }
        assert!(
            grain > 500,
            "grain should cover the surface, got {grain} cells"
        );

        // A blank page is left completely untouched.
        let mut blank = Pixmap::new(WIDTH, HEIGHT).expect("pixmap should allocate");
        blank.fill(Color::WHITE);
        draw_paper_texture(&mut blank, Paper::None);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = blank.pixel(x, y).expect("pixel should exist");
                assert_eq!(
                    (pixel.red(), pixel.green(), pixel.blue()),
                    (255, 255, 255),
                    "Paper::None must stay ungrained"
                );
            }
        }

        // And `render` really applies it — counting only strictly neutral
        // near-white pixels, which an antialiased (bluish) rule edge never is.
        let full = rendered(Paper::RuledNarrow, &margin_reaching_page());
        let mut neutral = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = full.pixel(x, y).expect("pixel should exist");
                let (red, green, blue) = (pixel.red(), pixel.green(), pixel.blue());
                if red == green && green == blue && (240..255).contains(&red) {
                    neutral += 1;
                }
            }
        }
        assert!(
            neutral > 500,
            "render should lay the grain, got {neutral} cells"
        );
    }

    /// Paper goes behind the ink: every fully-covered ink pixel (the stroke
    /// centreline, painted at the canonical colour with no blending) must be
    /// byte-identical with and without paper.
    ///
    /// Antialiased stroke *edges* are deliberately excluded — those are partial
    /// coverage, so paper legitimately shows through them. That is what "behind"
    /// means; if paper were drawn on top, the opaque core would change too.
    #[test]
    fn paper_never_overpaints_ink() {
        const INK: (u8, u8, u8) = (0x00, 0x64, 0x00);
        let blank = rendered(Paper::None, &canonical_v1_page());
        let ruled = rendered(Paper::SquaredSmall, &canonical_v1_page());
        let mut core_pixels = 0;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let before = blank.pixel(x, y).expect("pixel should exist");
                if (before.red(), before.green(), before.blue()) != INK {
                    continue;
                }
                core_pixels += 1;
                let after = ruled.pixel(x, y).expect("pixel should exist");
                assert_eq!(
                    (after.red(), after.green(), after.blue()),
                    INK,
                    "paper overpainted the ink core at ({x}, {y})"
                );
            }
        }
        assert!(
            core_pixels > 100,
            "the fixture page should contain a solid ink core to compare"
        );
    }

    /// Zoomed far out, a fine paper is culled rather than aliased into a wash.
    /// A page 3000 world units wide fits at ~0.045 device px per unit, well
    /// under the 4-px-pitch threshold for 32-unit squares.
    #[test]
    fn dense_scale_culls_paper_instead_of_aliasing() {
        let sprawling = vec![Stroke {
            id: "wide".to_string(),
            style: StrokeStyle::default_solid_round(),
            points: vec![
                StrokePoint {
                    x: 0.0,
                    y: 0.0,
                    t: 0,
                    pressure: None,
                },
                StrokePoint {
                    x: 6000.0,
                    y: 4000.0,
                    t: 10,
                    pressure: None,
                },
            ],
        }];
        let pixmap = rendered(Paper::SquaredSmall, &sprawling);
        assert_eq!(
            count_pixels(&pixmap, is_rule_pixel),
            0,
            "fine paper should be culled at this scale, not aliased"
        );
        // …while the never-culled margin of a margin paper still shows.
        let pixmap = rendered(Paper::RuledMarginNarrow, &sprawling);
        assert_eq!(count_pixels(&pixmap, is_rule_pixel), 0, "rules culled");
        assert!(
            count_pixels(&pixmap, is_margin_pixel) > 0,
            "a single margin line has no pitch to alias against and is never culled"
        );
    }

    /// The paper width floor is its own 1 px, not the thumbnail ink floor
    /// ([`MIN_THUMBNAIL_STROKE_WIDTH`]): if that floor leaked, each rule would
    /// be a multi-px band instead of a hairline.
    #[test]
    fn paper_runs_stay_hairline_thin() {
        let pixmap = rendered(Paper::RuledNarrow, &canonical_v1_page());
        let mut widest = 0;
        for x in 0..WIDTH {
            let mut run = 0;
            for y in 0..HEIGHT {
                if is_rule_pixel(pixmap.pixel(x, y).expect("pixel should exist")) {
                    run += 1;
                    widest = widest.max(run);
                } else {
                    run = 0;
                }
            }
        }
        assert!(widest > 0, "rules should be drawn at all");
        assert!(
            widest <= 3,
            "rule runs are {widest}px — the thumbnail ink floor leaked into paper"
        );
    }

    /// A page whose ink is all erased still produces a plain white thumbnail:
    /// no paper-only preview, and — critically — the enumeration terminates
    /// instead of walking the ±INFINITY viewport the bounds fold would yield.
    #[test]
    fn inkless_page_renders_blank_white_and_terminates() {
        for strokes in [
            Vec::new(),
            vec![Stroke {
                id: "empty".to_string(),
                style: StrokeStyle::default_solid_round(),
                points: Vec::new(),
            }],
        ] {
            let pixmap = rendered(Paper::SquaredSmall, &strokes);
            assert_eq!(count_pixels(&pixmap, is_rule_pixel), 0);
            assert_eq!(count_pixels(&pixmap, is_margin_pixel), 0);
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let pixel = pixmap.pixel(x, y).expect("pixel should exist");
                    assert_eq!(
                        (pixel.red(), pixel.green(), pixel.blue()),
                        (255, 255, 255),
                        "inkless page should be blank white at ({x}, {y})"
                    );
                }
            }
        }
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
