use protocol::{LibraryEvent, Stroke, ThumbnailMetadata, PEN_COLOR, PEN_WIDTH};
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
        "SELECT strokes FROM stroke_batches WHERE page_id = $1 AND seq <= $2 ORDER BY seq",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    let strokes: Vec<Stroke> = batches.into_iter().flat_map(|batch| batch.0).collect();
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
    let mut paint = Paint::default();
    paint.set_color_rgba8(0x00, 0x64, 0x00, 0xff);
    let pen = SkiaStroke {
        // Fitting wide world-space ink must not turn the preview into hairlines.
        width: (PEN_WIDTH as f32 * scale).max(MIN_THUMBNAIL_STROKE_WIDTH),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    for stroke in strokes {
        if stroke.color != PEN_COLOR || stroke.points.is_empty() {
            continue;
        }
        let transform = Transform::from_scale(scale, scale).post_translate(offset_x, offset_y);
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

async fn cleanup(pool: &PgPool, page_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"DELETE FROM page_thumbnails
           WHERE page_id = $1
             AND source_seq <> (SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1)
             AND (
             created_at < now() - interval '7 days' OR source_seq NOT IN (
               SELECT source_seq FROM page_thumbnails
               WHERE page_id = $1
                 AND source_seq <> (SELECT COALESCE(MAX(seq), 0) FROM stroke_batches WHERE page_id = $1)
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
    use protocol::{StrokePoint, PEN_TOOL};
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[test]
    fn renders_png_with_canonical_ink_colour() {
        let bytes = render(&[Stroke {
            tool: PEN_TOOL.to_string(),
            color: PEN_COLOR.to_string(),
            width: PEN_WIDTH,
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
            tool: PEN_TOOL.to_string(),
            color: PEN_COLOR.to_string(),
            width: PEN_WIDTH,
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
