use protocol::{
    LibraryEvent, Paper, Stroke, ThumbnailMetadata, WorldViewport, paper_mark_device_width,
    visit_paper_marks,
};
use sqlx::PgPool;
use std::time::Instant;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform,
};

use tracing::Instrument as _;

use crate::AppState;
use crate::observability::{db_query_span, metered};

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;
const PADDING: f32 = 12.0;
// Preview-only width floor so heavily scaled-down ink stays visible without
// turning into hairlines. Was 20.0, which on a full page of handwriting (small
// scale, so the floor dominates every stroke) merged all ink into blobs.
const MIN_THUMBNAIL_STROKE_WIDTH: f32 = 4.0;

pub fn recover_pending(state: AppState) {
    let pool = state.db.clone();
    // Runs at startup, detached from any request, so it is a trace of its own.
    let span = tracing::info_span!(parent: None, "thumbnail.recover");
    tokio::spawn(
        async move {
            // No paper is carried here on purpose: `generate` reads it from the job
            // row it is about to render, so a job recovered after a restart can never
            // pick up a paper the page has moved on to since. See `generate`.
            let pending = sqlx::query_as::<_, (String, String, i64)>(
                r#"SELECT t.page_id, p.owner_id, t.source_seq
               FROM page_thumbnails t
               JOIN pages p ON p.id = t.page_id
               WHERE t.status = 'generating'"#,
            )
            .fetch_all(metered(&pool))
            .instrument(db_query_span!("SELECT", "recover_pending_thumbnails"))
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
                state.realtime.publish_library_event(
                    &owner_id,
                    LibraryEvent::PageThumbnailUpdated {
                        page_id: page_id.clone(),
                        thumbnail: ThumbnailMetadata::Generating { source_seq },
                    },
                );
                enqueue(state.clone(), page_id, owner_id, source_seq);
            }
        }
        .instrument(span),
    );
}

/// Starts rendering one thumbnail job. Counts it on the queue-depth gauge here,
/// paired with the `thumbnail_generation_finished` the job always reaches, so no
/// caller can queue a job without the gauge seeing it.
pub fn enqueue(state: AppState, page_id: String, owner_id: String, source_seq: u64) {
    crate::observability::metrics().thumbnail_generation_queued();
    // A detached job, so a trace of its own rather than a child of the request
    // that queued it (a commit, erase or paper change would otherwise stay open
    // until the render finished). The link keeps the two navigable in Tempo.
    let span = tracing::info_span!(
        parent: None,
        "thumbnail.generate",
        page_id = %page_id,
        source_seq = source_seq,
    );
    span.follows_from(tracing::Span::current());
    tokio::spawn(async move {
        let pool = &state.db;
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
                .execute(metered(pool))
                .instrument(db_query_span!("UPDATE", "thumbnail_mark_failed"))
                .await;
                ThumbnailMetadata::Failed { source_seq }
            }
        };
        crate::observability::metrics().thumbnail_generation_finished();
        state.realtime.publish_library_event(
            &owner_id,
            LibraryEvent::PageThumbnailUpdated { page_id, thumbnail },
        );
    }.instrument(span));
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
    .fetch_optional(metered(pool))
    .instrument(db_query_span!("SELECT", "thumbnail_job_paper"))
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
    .fetch_all(metered(pool))
    .instrument(db_query_span!("SELECT", "thumbnail_strokes"))
    .await
    .map_err(|error| error.to_string())?;
    let tombstones: std::collections::HashSet<String> = sqlx::query_scalar(
        "SELECT stroke_id FROM stroke_tombstones WHERE page_id = $1 AND deleted_revision <= $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .fetch_all(metered(pool))
    .instrument(db_query_span!("SELECT", "thumbnail_tombstones"))
    .await
    .map_err(|error| error.to_string())?
    .into_iter()
    .collect();
    let strokes: Vec<Stroke> = batches
        .into_iter()
        .flat_map(|batch| batch.0)
        .filter(|stroke| !tombstones.contains(&stroke.id))
        .collect();
    let png = tracing::info_span!("thumbnail.render", strokes = strokes.len())
        .in_scope(|| render(paper, &strokes))?;
    let png_bytes = png.len();
    sqlx::query(
        "UPDATE page_thumbnails SET status = 'available', png = $3 WHERE page_id = $1 AND source_seq = $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .bind(png)
    .execute(metered(pool))
    .instrument(db_query_span!("UPDATE", "thumbnail_store_png"))
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
    // (`maxOf(maxX - minX, maxY - minY) < maxWidth` in InkRenderer.kt) — a
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
    .execute(metered(pool))
    .instrument(db_query_span!("DELETE", "thumbnail_cleanup"))
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
mod tests;

#[cfg(all(test, feature = "postgres-tests"))]
mod postgres_tests;
