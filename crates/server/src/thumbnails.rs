use protocol::{LibraryEvent, Stroke, ThumbnailMetadata, PEN_COLOR, PEN_WIDTH};
use sqlx::PgPool;
use tiny_skia::{
    Color, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkiaStroke, Transform,
};

use crate::AppState;

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;
const PADDING: f32 = 12.0;
const MIN_THUMBNAIL_STROKE_WIDTH: f32 = 20.0;

pub fn enqueue(state: AppState, page_id: String, owner_id: String, source_seq: u64) {
    tokio::spawn(async move {
        let Some(pool) = state.db.as_ref() else {
            return;
        };
        let result = generate(pool, &page_id, source_seq).await;
        let thumbnail = match result {
            Ok(()) => {
                crate::observability::metrics()
                    .record_page_mutation("generate_thumbnail", "success");
                ThumbnailMetadata::Available {
                    source_seq,
                    url: thumbnail_url(&page_id, source_seq),
                }
            }
            Err(error) => {
                crate::observability::metrics().record_page_mutation("generate_thumbnail", "error");
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
    sqlx::query(
        "UPDATE page_thumbnails SET status = 'available', png = $3 WHERE page_id = $1 AND source_seq = $2",
    )
    .bind(page_id)
    .bind(source_seq as i64)
    .bind(png)
    .execute(pool)
    .await
    .map_err(|error| error.to_string())?;
    cleanup(pool, page_id)
        .await
        .map_err(|error| error.to_string())
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
    let content_w = (max_x - min_x).max(1.0) as f32;
    let content_h = (max_y - min_y).max(1.0) as f32;
    let scale = ((WIDTH as f32 - PADDING * 2.0) / content_w)
        .min((HEIGHT as f32 - PADDING * 2.0) / content_h);
    let offset_x = (WIDTH as f32 - content_w * scale) / 2.0 - min_x as f32 * scale;
    let offset_y = (HEIGHT as f32 - content_h * scale) / 2.0 - min_y as f32 * scale;
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
        if stroke.color != PEN_COLOR || stroke.points.len() < 2 {
            continue;
        }
        let mut path = PathBuilder::new();
        path.move_to(stroke.points[0].x as f32, stroke.points[0].y as f32);
        for point in stroke.points.iter().skip(1) {
            path.line_to(point.x as f32, point.y as f32);
        }
        if let Some(path) = path.finish() {
            pixmap.stroke_path(
                &path,
                &paint,
                &pen,
                Transform::from_scale(scale, scale).post_translate(offset_x, offset_y),
                None,
            );
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

#[cfg(test)]
mod tests {
    use protocol::{StrokePoint, PEN_TOOL};

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
}
