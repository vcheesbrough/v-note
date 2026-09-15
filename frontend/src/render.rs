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

    for stroke in batches
        .iter()
        .flat_map(|batch| batch.strokes.iter())
        .filter(|stroke| stroke.points.len() >= 2)
    {
        draw_stroke(&context, stroke, offset_x, offset_y, scale);
    }
}

const MIN_RENDERED_STROKE_WIDTH: f64 = 0.75;

fn draw_stroke(
    context: &CanvasRenderingContext2d,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    context.set_stroke_style_str(&stroke.style.parameters.color);

    // Pressure-modulated (v2) strokes replay as per-segment variable-width
    // paths; v1 keeps the single constant-width path below byte-identical.
    if stroke.style.is_pressure_sensitive() {
        draw_pressure_stroke(context, stroke, offset_x, offset_y, scale);
        return;
    }

    context.begin_path();
    context.set_line_width((stroke.style.parameters.width * scale).max(MIN_RENDERED_STROKE_WIDTH));
    if let Some(first) = stroke.points.first() {
        context.move_to(first.x * scale + offset_x, first.y * scale + offset_y);
        for point in stroke.points.iter().skip(1) {
            context.line_to(point.x * scale + offset_x, point.y * scale + offset_y);
        }
    }
    context.stroke();
}

/// Replay one v2 stroke as a chain of round-capped segments, each stroked at the
/// mean of its endpoints' pressure widths (the shared curve in `protocol`).
/// Round caps overlap consecutive segments so joins stay continuous.
fn draw_pressure_stroke(
    context: &CanvasRenderingContext2d,
    stroke: &Stroke,
    offset_x: f64,
    offset_y: f64,
    scale: f64,
) {
    let style = &stroke.style;
    for pair in stroke.points.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let width =
            (style.rendered_width(a.pressure) + style.rendered_width(b.pressure)) / 2.0 * scale;
        context.begin_path();
        context.set_line_width(width.max(MIN_RENDERED_STROKE_WIDTH));
        context.move_to(a.x * scale + offset_x, a.y * scale + offset_y);
        context.line_to(b.x * scale + offset_x, b.y * scale + offset_y);
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
