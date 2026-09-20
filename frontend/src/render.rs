//! Canvas2D painting of one page: white fill, paper grain and rules, then the
//! ink, all under the same world→CSS mapping. Pure functions over the
//! viewport; nothing here touches signals.

use protocol::{
    PAPER_TEXTURE_COLOR_RGB, PAPER_TEXTURE_TILE_SIZE, Paper, Stroke, StrokeBatch, WorldViewport,
    paper_has_texture, paper_mark_device_width, paper_texture_tile, visit_paper_marks,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasPattern, CanvasRenderingContext2d, HtmlCanvasElement};

/// The world-space rectangle currently on screen — the exact inverse of the
/// `world * scale + offset` mapping the stroke loop applies, so paper is
/// enumerated for precisely the area being painted.
///
/// `scale` here is CSS px per world unit: the DPR transform is applied outside
/// this function, so on a 2× display the *physical* pitch is double the cull
/// threshold. That is conservative — it can only ever cull too early, never too
/// late — and is deliberately left uncorrected.
pub(crate) fn world_viewport(
    css_width: f64,
    css_height: f64,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) -> WorldViewport {
    WorldViewport::new(
        (0.0 - offset_x) / scale,
        (0.0 - offset_y) / scale,
        (css_width - offset_x) / scale,
        (css_height - offset_y) / scale,
        scale,
    )
}

thread_local! {
    // The grain tile, built once per document and reused for every frame.
    //
    // Rebuilding it per frame would mean allocating a 64x64 `ImageData`,
    // blitting it and constructing a `CanvasPattern` on every pan and zoom
    // step. The inner `None` means the tile could not be built (no document, or
    // a 2d context refused) — the texture is then simply skipped, since it is
    // decoration and must never block the ink from drawing.
    static PAPER_TEXTURE_PATTERN: std::cell::RefCell<Option<Option<CanvasPattern>>> =
        const { std::cell::RefCell::new(None) };
}

/// Build the repeating grain pattern from the shared tile.
fn build_paper_texture_pattern() -> Option<CanvasPattern> {
    let size = PAPER_TEXTURE_TILE_SIZE as u32;
    let document = web_sys::window()?.document()?;
    let tile_canvas = document
        .create_element("canvas")
        .ok()?
        .dyn_into::<HtmlCanvasElement>()
        .ok()?;
    tile_canvas.set_width(size);
    tile_canvas.set_height(size);
    let tile_context = tile_canvas
        .get_context("2d")
        .ok()??
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;

    let alphas = paper_texture_tile();
    let [red, green, blue] = PAPER_TEXTURE_COLOR_RGB;
    let mut rgba = Vec::with_capacity(alphas.len() * 4);
    for alpha in &alphas {
        rgba.extend_from_slice(&[red, green, blue, *alpha]);
    }
    let image = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
        wasm_bindgen::Clamped(&rgba),
        size,
        size,
    )
    .ok()?;
    tile_context.put_image_data(&image, 0.0, 0.0).ok()?;
    context_pattern(&tile_canvas)
}

fn context_pattern(tile: &HtmlCanvasElement) -> Option<CanvasPattern> {
    let document = web_sys::window()?.document()?;
    let host = document
        .create_element("canvas")
        .ok()?
        .dyn_into::<HtmlCanvasElement>()
        .ok()?;
    let context = host
        .get_context("2d")
        .ok()??
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;
    context
        .create_pattern_with_html_canvas_element(tile, "repeat")
        .ok()?
}

/// Lay the faint paper grain over the whole canvas, under the rules and the ink.
///
/// Tiled in **CSS-pixel space** rather than world space, so the grain keeps a
/// constant perceptual size at every zoom. World-anchoring it would turn the
/// speckle into visible blocks when zoomed in and dissolve it when zoomed out.
fn draw_paper_texture(context: &CanvasRenderingContext2d, css_width: f64, css_height: f64) {
    PAPER_TEXTURE_PATTERN.with(|cell| {
        let mut cached = cell.borrow_mut();
        let pattern = cached.get_or_insert_with(build_paper_texture_pattern);
        if let Some(pattern) = pattern.as_ref() {
            context.set_fill_style_canvas_pattern(pattern);
            context.fill_rect(0.0, 0.0, css_width, css_height);
        }
    });
}

/// Paint the page's paper behind the ink, under the same world→CSS mapping the
/// stroke loop uses, so it stays locked to the ink through pan and zoom.
fn draw_paper(
    context: &CanvasRenderingContext2d,
    paper: Paper,
    viewport: &WorldViewport,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    if paper == Paper::None {
        return;
    }
    // Butt caps: a round cap would bulge each line's ends past the viewport edge.
    // Restored to "round" for ink by the caller.
    context.set_line_cap("butt");

    // Batch by style instead of stroking each mark on its own. `visit_paper_marks`
    // yields marks grouped by kind, and rules and columns share a colour *and* a
    // width, so a whole grid collapses to one `begin_path` + one style pair + one
    // `stroke()`, with the margin as a second batch. Disjoint `move_to`/`line_to`
    // pairs are separate subpaths of that single path — this is exactly what
    // Canvas2D batching is for, and it turns dozens of JS/WASM boundary crossings
    // per frame into about three.
    let mut batch: Option<(&'static str, f64)> = None;
    visit_paper_marks(paper, viewport, |mark| {
        let style = (mark.kind.color(), mark.world_width());
        if batch != Some(style) {
            if batch.is_some() {
                context.stroke();
            }
            context.begin_path();
            context.set_stroke_style_str(style.0);
            context.set_line_width(paper_mark_device_width(style.1, scale));
            batch = Some(style);
        }
        if mark.kind.is_horizontal() {
            let y = mark.position * scale + offset_y;
            context.move_to(viewport.min_x * scale + offset_x, y);
            context.line_to(viewport.max_x * scale + offset_x, y);
        } else {
            let x = mark.position * scale + offset_x;
            context.move_to(x, viewport.min_y * scale + offset_y);
            context.line_to(x, viewport.max_y * scale + offset_y);
        }
    });
    if batch.is_some() {
        context.stroke();
    }
}

pub(crate) fn draw_canvas(
    canvas: &HtmlCanvasElement,
    batches: &[StrokeBatch],
    paper: Paper,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let rect = canvas.get_bounding_client_rect();
    let css_width = rect.width().max(1.0);
    let css_height = rect.height().max(1.0);
    let dpr = web_sys::window()
        .map(|window| window.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0);
    let backing_width = (css_width * dpr).round() as u32;
    let backing_height = (css_height * dpr).round() as u32;
    if canvas.width() != backing_width {
        canvas.set_width(backing_width);
    }
    if canvas.height() != backing_height {
        canvas.set_height(backing_height);
    }

    let Ok(Some(context)) = canvas.get_context("2d") else {
        return;
    };
    let Ok(context) = context.dyn_into::<CanvasRenderingContext2d>() else {
        return;
    };
    let _ = context.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    context.set_fill_style_str("#ffffff");
    context.fill_rect(0.0, 0.0, css_width, css_height);

    // Paper goes on after the white fill and before every stroke, so it can
    // never overpaint ink. The grain sits under the rules, so a rule crossing a
    // speckle still reads as an unbroken line.
    if paper_has_texture(paper) {
        draw_paper_texture(&context, css_width, css_height);
    }
    let viewport = world_viewport(css_width, css_height, offset_x, offset_y, scale);
    draw_paper(&context, paper, &viewport, offset_x, offset_y, scale);

    context.set_line_cap("round");
    context.set_line_join("round");

    let mut pen = Pen::default();
    for stroke in batches
        .iter()
        .flat_map(|batch| batch.strokes.iter())
        .filter(|stroke| stroke.points.len() >= 2)
        .filter(|stroke| stroke_may_be_visible(stroke, &viewport))
    {
        draw_stroke(&context, &mut pen, stroke, offset_x, offset_y, scale);
    }
}

const MIN_RENDERED_STROKE_WIDTH: f64 = 0.75;

/// Pressure widths are snapped to this many CSS px before segments are merged,
/// so a run of samples whose widths differ only sub-visibly shares one path.
const PRESSURE_WIDTH_STEP: f64 = 0.125;

/// The stroke style last sent to the context. Every setter is a JS/WASM
/// boundary crossing (and a colour string is re-encoded on each one), so a
/// value already in force is not sent again.
#[derive(Default)]
struct Pen {
    color: Option<String>,
    width: Option<f64>,
}

impl Pen {
    fn color(&mut self, context: &CanvasRenderingContext2d, color: &str) {
        if self.color.as_deref() != Some(color) {
            context.set_stroke_style_str(color);
            self.color = Some(color.to_string());
        }
    }

    fn width(&mut self, context: &CanvasRenderingContext2d, width: f64) {
        if self.width != Some(width) {
            context.set_line_width(width);
            self.width = Some(width);
        }
    }
}

/// Whether any of the stroke's ink can land inside the viewport. The points'
/// world bounds are padded by half the widest nib the stroke can render — the
/// preset width, which pressure only ever narrows — plus the on-screen floor,
/// so a stroke is only skipped when it is certainly off screen.
fn stroke_may_be_visible(stroke: &Stroke, viewport: &WorldViewport) -> bool {
    let Some(first) = stroke.points.first() else {
        return false;
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for point in &stroke.points[1..] {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    let pad = stroke.style.parameters.width / 2.0 + MIN_RENDERED_STROKE_WIDTH / viewport.scale;
    max_x + pad >= viewport.min_x
        && min_x - pad <= viewport.max_x
        && max_y + pad >= viewport.min_y
        && min_y - pad <= viewport.max_y
}

fn draw_stroke(
    context: &CanvasRenderingContext2d,
    pen: &mut Pen,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    pen.color(context, &stroke.style.parameters.color);

    // One replay path for every stroke. Ink migrated up from the retired v1
    // style carries no pressure, so all of its segments share the full-width
    // nib and the run-grouping below collapses it to a single polyline — the
    // same one draw call the old constant-width path made.
    draw_pressure_stroke(context, pen, stroke, offset_x, offset_y, scale);
}

/// Each segment's on-screen width: the mean of its endpoints' pressure widths
/// (the shared curve in `protocol`), floored and snapped to
/// [`PRESSURE_WIDTH_STEP`].
fn pressure_segment_widths(stroke: &Stroke, scale: f64) -> impl Iterator<Item = f64> + '_ {
    let style = &stroke.style;
    stroke.points.windows(2).map(move |pair| {
        let width =
            (style.rendered_width(pair[0].pressure) + style.rendered_width(pair[1].pressure)) / 2.0
                * scale;
        let snapped = (width / PRESSURE_WIDTH_STEP).round() * PRESSURE_WIDTH_STEP;
        snapped.max(MIN_RENDERED_STROKE_WIDTH)
    })
}

/// Replay one stroke as runs of consecutive segments sharing a width, one
/// round-joined polyline per run.
///
/// With round caps and joins a polyline covers exactly the union of its
/// segments stroked one by one with round caps, and stroke colours are opaque,
/// so the covered geometry matches per-segment stroking while crossing into JS
/// once per run rather than five times per segment. Coverage does differ at
/// hairline widths: separate strokes stacked antialiasing where caps overlapped,
/// so dense sub-pixel ink used to look darker than one path draws it. Zoomed
/// out, every segment sits on the width floor and a whole stroke becomes one
/// path.
fn draw_pressure_stroke(
    context: &CanvasRenderingContext2d,
    pen: &mut Pen,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let to_css = |index: usize| {
        let point = &stroke.points[index];
        (point.x * scale + offset_x, point.y * scale + offset_y)
    };
    let mut run_width = None;
    for (segment, width) in pressure_segment_widths(stroke, scale).enumerate() {
        if run_width != Some(width) {
            if run_width.is_some() {
                context.stroke();
            }
            context.begin_path();
            pen.width(context, width);
            let (x, y) = to_css(segment);
            context.move_to(x, y);
            run_width = Some(width);
        }
        let (x, y) = to_css(segment + 1);
        context.line_to(x, y);
    }
    if run_width.is_some() {
        context.stroke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `world_viewport` must be the exact inverse of the `world * scale + offset`
    /// mapping the stroke loop applies, or paper drifts against the ink.
    #[test]
    fn world_viewport_inverts_the_ink_transform() {
        let (css_w, css_h, offset_x, offset_y, scale) = (800.0, 600.0, 80.0, -45.0, 0.5);
        let viewport = world_viewport(css_w, css_h, offset_x, offset_y, scale);
        let to_css = |world: f64, offset: f64| world * scale + offset;
        assert!((to_css(viewport.min_x, offset_x) - 0.0).abs() < 1e-9);
        assert!((to_css(viewport.min_y, offset_y) - 0.0).abs() < 1e-9);
        assert!((to_css(viewport.max_x, offset_x) - css_w).abs() < 1e-9);
        assert!((to_css(viewport.max_y, offset_y) - css_h).abs() < 1e-9);
        assert_eq!(viewport.scale, scale);
    }

    fn pressure_stroke(width: f64, points: &[(f64, f64, f64)]) -> Stroke {
        let mut style = protocol::StrokeStyle::default_solid_round_pressure();
        style.parameters.width = width;
        Stroke {
            id: "stroke".to_string(),
            style,
            points: points
                .iter()
                .map(|&(x, y, pressure)| protocol::StrokePoint {
                    x,
                    y,
                    t: 0,
                    pressure: Some(pressure),
                })
                .collect(),
        }
    }

    /// A stroke is culled only when its padded bounds miss the viewport; one
    /// whose centreline is just off screen but whose nib reaches in still draws.
    #[test]
    fn strokes_are_culled_only_when_certainly_off_screen() {
        let viewport = world_viewport(400.0, 300.0, 0.0, 0.0, 1.0);
        let inside = pressure_stroke(8.0, &[(10.0, 10.0, 1.0), (50.0, 50.0, 1.0)]);
        let far_right = pressure_stroke(8.0, &[(900.0, 10.0, 1.0), (950.0, 50.0, 1.0)]);
        let far_above = pressure_stroke(8.0, &[(10.0, -900.0, 1.0), (50.0, -800.0, 1.0)]);
        // Centreline 3 world units left of the edge; a 32-wide nib reaches 16 in.
        let nib_reaches_in = pressure_stroke(32.0, &[(-3.0, 10.0, 1.0), (-3.0, 90.0, 1.0)]);
        // Bounds straddle the viewport even though no point is inside it.
        let spans_across = pressure_stroke(4.0, &[(-100.0, 150.0, 1.0), (600.0, 150.0, 1.0)]);
        assert!(stroke_may_be_visible(&inside, &viewport));
        assert!(!stroke_may_be_visible(&far_right, &viewport));
        assert!(!stroke_may_be_visible(&far_above, &viewport));
        assert!(stroke_may_be_visible(&nib_reaches_in, &viewport));
        assert!(stroke_may_be_visible(&spans_across, &viewport));

        // Panning the far stroke into view brings it back.
        let panned = world_viewport(400.0, 300.0, -700.0, 0.0, 1.0);
        assert!(stroke_may_be_visible(&far_right, &panned));
    }

    /// Zoomed out, every pressure segment sits on the on-screen floor, so the
    /// whole stroke collapses into a single run; zoomed in, widths still follow
    /// the shared pressure curve to within the snap step.
    #[test]
    fn pressure_segment_widths_snap_and_floor() {
        let points: Vec<_> = (0..=10)
            .map(|index| (index as f64 * 10.0, 0.0, index as f64 / 10.0))
            .collect();
        let stroke = pressure_stroke(8.0, &points);

        let zoomed_out: Vec<_> = pressure_segment_widths(&stroke, 0.08).collect();
        assert_eq!(zoomed_out.len(), 10);
        assert!(
            zoomed_out
                .iter()
                .all(|&width| width == MIN_RENDERED_STROKE_WIDTH)
        );

        let scale = 3.0;
        let zoomed_in: Vec<_> = pressure_segment_widths(&stroke, scale).collect();
        for (segment, width) in zoomed_in.iter().enumerate() {
            let exact = (stroke.style.rendered_width(Some(segment as f64 / 10.0))
                + stroke
                    .style
                    .rendered_width(Some((segment + 1) as f64 / 10.0)))
                / 2.0
                * scale;
            assert!((width - exact).abs() <= PRESSURE_WIDTH_STEP / 2.0 + 1e-9);
            assert_eq!(width % PRESSURE_WIDTH_STEP, 0.0);
        }
        assert!(zoomed_in.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// Panning moves the world window by exactly the inverse pan, so paper stays
    /// locked to the ink rather than sliding under it.
    #[test]
    fn world_viewport_tracks_pan_and_zoom() {
        let base = world_viewport(400.0, 300.0, 0.0, 0.0, 1.0);
        let panned = world_viewport(400.0, 300.0, 100.0, 50.0, 1.0);
        assert!((panned.min_x - (base.min_x - 100.0)).abs() < 1e-9);
        assert!((panned.min_y - (base.min_y - 50.0)).abs() < 1e-9);
        // Zooming in halves the world extent on screen.
        let zoomed = world_viewport(400.0, 300.0, 0.0, 0.0, 2.0);
        assert!(((zoomed.max_x - zoomed.min_x) * 2.0 - (base.max_x - base.min_x)).abs() < 1e-9);
    }
}
