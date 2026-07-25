//! Page paper (rule lines) — the single shared geometry spec.
//!
//! `docs/PLAN.md` §Explicit non-goals forbids a shared cross-platform render
//! library, so paper parity is enforced the way the pressure→width curve is:
//! **one pure spec here, mirrored in Kotlin (`ink/Paper.kt`), pinned by the
//! golden fixture `contracts/fixtures/paper-geometry.json` asserted from both
//! sides.** Nothing in this module touches a canvas; every renderer
//! (thumbnails, SPA, Android) enumerates marks from here and draws them under
//! the *same transform it uses for ink*, so "identical relative size and
//! position" holds structurally rather than by coincidence.
//!
//! Marks are anchored at world **0** (`position = k * pitch`, `k` any integer
//! including negatives), so paper is fixed relative to the ink and rides the
//! ink's transform through pan and zoom. Every pitch is an integer, so
//! positions are exact integer multiples and the `f64 → f32` narrowing in
//! tiny_skia and Compose is lossless while `|position| < 2^24`
//! ([`MAX_EXACT_PAPER_WORLD_EXTENT`]) — that is the supported world extent.

use serde::{Deserialize, Serialize};

/// Horizontal rule pitch, world units — narrow rules (`ruled-narrow`,
/// `ruled-margin-narrow`).
pub const RULE_SPACING_NARROW: f64 = 48.0;
/// Horizontal rule pitch, world units — wide rules (`ruled-wide`,
/// `ruled-margin-wide`).
pub const RULE_SPACING_WIDE: f64 = 72.0;
/// Square pitch, world units — `squared-small` (both axes).
pub const GRID_SPACING_SMALL: f64 = 32.0;
/// Square pitch, world units — `squared-large` (both axes).
pub const GRID_SPACING_LARGE: f64 = 64.0;
/// World `x` of the single red margin rule on the `ruled-margin-*` papers.
pub const MARGIN_X: f64 = 96.0;

/// Rule/grid line width, world units.
pub const RULE_LINE_WIDTH: f64 = 1.5;
/// Margin line width, world units — deliberately heavier than a rule.
pub const MARGIN_LINE_WIDTH: f64 = 2.0;

/// Rule/grid colour, uppercase `#RRGGBB`. **Load-bearing:** see
/// [`is_ink_classified`] — it must not read as ink to any existing pixel test.
pub const RULE_COLOR: &str = "#B0C4DE";
/// [`RULE_COLOR`] as `[r, g, b]` components (tiny_skia wants components).
pub const RULE_COLOR_RGB: [u8; 3] = [0xB0, 0xC4, 0xDE];
/// Margin colour, uppercase `#RRGGBB`.
pub const MARGIN_COLOR: &str = "#E06C6C";
/// [`MARGIN_COLOR`] as `[r, g, b]` components.
pub const MARGIN_COLOR_RGB: [u8; 3] = [0xE0, 0x6C, 0x6C];

/// Minimum on-screen pitch, in the renderer's own device pixels, below which a
/// mark **family** is culled rather than aliased into a moiré wash.
///
/// Evaluated **per family**, so `squared-*` can lose its grid and keep its
/// rules, and coarse papers survive further out than fine ones. The margin has
/// no pitch to alias against and is never culled.
///
/// Why 4.0 and not 6.0: the Nyquist floor for a 1-px family is 2 px, so 4 px
/// keeps 2× headroom and 25% coverage (still discrete lines — 2 px is a flat
/// grey wash). More importantly paper must appear in 240×160 thumbnails, and
/// this threshold sets how wide a page can get before its rules vanish from the
/// preview; at 6.0 narrow rules would disappear from any page wider than
/// ~1730 world px, i.e. most real pages.
pub const MIN_PAPER_MARK_DEVICE_PITCH: f64 = 4.0;

/// Minimum rendered mark width in device pixels — a **dedicated** floor, not
/// one of the ink floors. Reusing the SPA/Android ink floor (0.75) lets paper
/// smear sub-pixel; reusing the thumbnail ink floor (20.0) would turn a 240×160
/// preview solid blue.
pub const MIN_PAPER_MARK_DEVICE_WIDTH: f64 = 1.0;

/// Hard per-axis cap on enumerated marks. Provably unreachable in practice —
/// 4096 marks at the ≥4 device-px minimum pitch needs a ≥16384-px viewport —
/// so this is a safety net against a pathological or corrupt viewport, not a
/// tuning parameter.
pub const MAX_PAPER_MARKS_PER_AXIS: usize = 4096;

/// `2^24`: the largest magnitude at which an integer world position survives
/// the `f64 → f32` narrowing every renderer performs, exactly.
pub const MAX_EXACT_PAPER_WORLD_EXTENT: f64 = 16_777_216.0;

/// Rows of rules a swatch aims to show; see [`preview_viewport`].
const PREVIEW_ROWS: f64 = 4.0;
/// Headroom over [`MIN_PAPER_MARK_DEVICE_PITCH`] applied by
/// [`preview_viewport`], so a swatch clears the cull with margin rather than
/// landing exactly on the boundary.
const PREVIEW_CULL_HEADROOM: f64 = 1.25;

/// The per-page paper choice. Serialized as its kebab-case wire value; absent
/// or legacy JSON defaults to [`Paper::None`].
///
/// Re-exported from the crate root by **name** — never glob-export the
/// variants, or `Paper::None` shadows `Option::None` at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Paper {
    /// Blank page — the historical behaviour and the default.
    #[default]
    None,
    RuledMarginNarrow,
    RuledMarginWide,
    RuledNarrow,
    RuledWide,
    SquaredSmall,
    SquaredLarge,
}

impl Paper {
    /// Every choice, in palette order. The Android palette and the docs both
    /// read this order, so it cannot drift between them.
    pub const ALL: [Paper; 7] = [
        Paper::None,
        Paper::RuledMarginNarrow,
        Paper::RuledMarginWide,
        Paper::RuledNarrow,
        Paper::RuledWide,
        Paper::SquaredSmall,
        Paper::SquaredLarge,
    ];

    /// The wire/storage value. Stored verbatim in Postgres, so this and
    /// [`Paper::from_wire`] are the only mapping in the system.
    pub const fn wire_value(self) -> &'static str {
        match self {
            Paper::None => "none",
            Paper::RuledMarginNarrow => "ruled-margin-narrow",
            Paper::RuledMarginWide => "ruled-margin-wide",
            Paper::RuledNarrow => "ruled-narrow",
            Paper::RuledWide => "ruled-wide",
            Paper::SquaredSmall => "squared-small",
            Paper::SquaredLarge => "squared-large",
        }
    }

    /// Parse a wire value. Returns `None` for anything unrecognised — callers
    /// reject rather than silently falling back, so a typo cannot be persisted
    /// as a blank page.
    pub fn from_wire(value: &str) -> Option<Paper> {
        Paper::ALL
            .into_iter()
            .find(|paper| paper.wire_value() == value)
    }

    /// Human label, shared so the Android palette wording cannot drift from
    /// the docs.
    pub const fn label(self) -> &'static str {
        match self {
            Paper::None => "None",
            Paper::RuledMarginNarrow => "Ruled, margin, narrow",
            Paper::RuledMarginWide => "Ruled, margin, wide",
            Paper::RuledNarrow => "Ruled, narrow",
            Paper::RuledWide => "Ruled, wide",
            Paper::SquaredSmall => "Squared, small",
            Paper::SquaredLarge => "Squared, large",
        }
    }

    /// Pitch of the horizontal rule family, world units, or `None` when this
    /// paper has no horizontal rules.
    pub const fn rule_spacing(self) -> Option<f64> {
        match self {
            Paper::None => Option::None,
            Paper::RuledMarginNarrow | Paper::RuledNarrow => Some(RULE_SPACING_NARROW),
            Paper::RuledMarginWide | Paper::RuledWide => Some(RULE_SPACING_WIDE),
            Paper::SquaredSmall => Some(GRID_SPACING_SMALL),
            Paper::SquaredLarge => Some(GRID_SPACING_LARGE),
        }
    }

    /// Pitch of the vertical grid family, world units. Only the squared papers
    /// have one; the `ruled-margin-*` vertical line is the margin, not a grid.
    pub const fn column_spacing(self) -> Option<f64> {
        match self {
            Paper::SquaredSmall => Some(GRID_SPACING_SMALL),
            Paper::SquaredLarge => Some(GRID_SPACING_LARGE),
            _ => Option::None,
        }
    }

    /// True when this paper carries the single red margin rule at [`MARGIN_X`].
    pub const fn has_margin(self) -> bool {
        matches!(self, Paper::RuledMarginNarrow | Paper::RuledMarginWide)
    }
}

/// What a mark is, which fixes both its colour and its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaperMarkKind {
    /// Horizontal rule at world `y = position`.
    Rule,
    /// Vertical grid line at world `x = position`.
    Column,
    /// The single vertical red margin at world `x = position`.
    Margin,
}

impl PaperMarkKind {
    /// True for a full-width horizontal line (`position` is a world `y`);
    /// false for a full-height vertical one (`position` is a world `x`).
    pub const fn is_horizontal(self) -> bool {
        matches!(self, PaperMarkKind::Rule)
    }

    /// Line width in **world** units; convert with [`paper_mark_device_width`].
    pub const fn world_width(self) -> f64 {
        match self {
            PaperMarkKind::Rule | PaperMarkKind::Column => RULE_LINE_WIDTH,
            PaperMarkKind::Margin => MARGIN_LINE_WIDTH,
        }
    }

    /// `#RRGGBB` colour (Compose and Canvas2D take either form).
    pub const fn color(self) -> &'static str {
        match self {
            PaperMarkKind::Rule | PaperMarkKind::Column => RULE_COLOR,
            PaperMarkKind::Margin => MARGIN_COLOR,
        }
    }

    /// Colour as `[r, g, b]` components (tiny_skia takes components).
    pub const fn color_rgb(self) -> [u8; 3] {
        match self {
            PaperMarkKind::Rule | PaperMarkKind::Column => RULE_COLOR_RGB,
            PaperMarkKind::Margin => MARGIN_COLOR_RGB,
        }
    }
}

/// One paper line to draw, spanning the viewport perpendicular to `position`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PaperMark {
    pub kind: PaperMarkKind,
    /// World `y` for [`PaperMarkKind::Rule`], world `x` otherwise. Always an
    /// exact integer multiple of the family pitch (or [`MARGIN_X`]).
    pub position: f64,
}

impl PaperMark {
    pub const fn world_width(self) -> f64 {
        self.kind.world_width()
    }
}

/// The world-space rectangle a renderer is about to paint, plus the device
/// pixels per world unit it will paint at.
///
/// Every renderer derives this by inverting the *same* transform it applies to
/// ink. `scale` is in that renderer's own device space — CSS px on the SPA,
/// where the DPR transform is applied outside the draw call, so on a 2×
/// display the physical pitch is double the threshold. That is conservative
/// (it never culls too late), and is documented rather than corrected.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldViewport {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
    /// Device pixels per world unit.
    pub scale: f64,
}

impl WorldViewport {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64, scale: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
            scale,
        }
    }

    fn is_drawable(&self) -> bool {
        self.min_x.is_finite()
            && self.min_y.is_finite()
            && self.max_x.is_finite()
            && self.max_y.is_finite()
            && self.scale.is_finite()
            && self.scale > 0.0
            && self.max_x >= self.min_x
            && self.max_y >= self.min_y
    }
}

/// Whether a mark family with `pitch_world` world-unit spacing is dense enough
/// to draw at `scale` device px per world unit, or should be culled.
pub fn paper_family_visible(pitch_world: f64, scale: f64) -> bool {
    pitch_world.is_finite()
        && scale.is_finite()
        && pitch_world > 0.0
        && scale > 0.0
        && pitch_world * scale >= MIN_PAPER_MARK_DEVICE_PITCH
}

/// Device-space stroke width for a mark, floored at
/// [`MIN_PAPER_MARK_DEVICE_WIDTH`] so paper never smears sub-pixel.
pub fn paper_mark_device_width(world_width: f64, scale: f64) -> f64 {
    (world_width * scale).max(MIN_PAPER_MARK_DEVICE_WIDTH)
}

/// Enumerate the marks for `paper` over `viewport` without allocating — the
/// hot path for the SPA and Android draw loops.
///
/// Order is deterministic and is the draw order: horizontal rules ascending,
/// then vertical grid ascending, then the margin last. All of it still goes
/// behind every stroke.
pub fn visit_paper_marks(paper: Paper, viewport: &WorldViewport, mut visit: impl FnMut(PaperMark)) {
    if paper == Paper::None || !viewport.is_drawable() {
        return;
    }

    if let Some(pitch) = paper.rule_spacing() {
        if paper_family_visible(pitch, viewport.scale) {
            visit_multiples(pitch, viewport.min_y, viewport.max_y, |position| {
                visit(PaperMark {
                    kind: PaperMarkKind::Rule,
                    position,
                })
            });
        }
    }

    if let Some(pitch) = paper.column_spacing() {
        if paper_family_visible(pitch, viewport.scale) {
            visit_multiples(pitch, viewport.min_x, viewport.max_x, |position| {
                visit(PaperMark {
                    kind: PaperMarkKind::Column,
                    position,
                })
            });
        }
    }

    // A single line has no pitch to alias against, so the margin is never
    // culled — only clipped out when the viewport does not reach it.
    if paper.has_margin() && viewport.min_x <= MARGIN_X && MARGIN_X <= viewport.max_x {
        visit(PaperMark {
            kind: PaperMarkKind::Margin,
            position: MARGIN_X,
        });
    }
}

/// Allocating form of [`visit_paper_marks`], for tests and the thumbnail path.
pub fn paper_marks(paper: Paper, viewport: &WorldViewport) -> Vec<PaperMark> {
    let mut marks = Vec::new();
    visit_paper_marks(paper, viewport, |mark| marks.push(mark));
    marks
}

/// A viewport for a small palette swatch of `width_px` × `height_px` device
/// pixels, scaled so the paper is never culled — otherwise a 32×24.dp swatch of
/// 48-unit rules falls under the density cull and the icon renders blank.
pub fn preview_viewport(paper: Paper, width_px: f64, height_px: f64) -> WorldViewport {
    let width_px = if width_px.is_finite() && width_px > 0.0 {
        width_px
    } else {
        0.0
    };
    let height_px = if height_px.is_finite() && height_px > 0.0 {
        height_px
    } else {
        0.0
    };
    // Paper::None draws nothing; the narrow pitch is an arbitrary stand-in that
    // keeps the returned scale positive and finite for callers.
    let pitch = paper.rule_spacing().unwrap_or(RULE_SPACING_NARROW);
    let natural = height_px / (pitch * PREVIEW_ROWS);
    let cull_floor = MIN_PAPER_MARK_DEVICE_PITCH * PREVIEW_CULL_HEADROOM / pitch;
    let scale = natural.max(cull_floor);
    // Half a pitch of lead-in so the first rule does not sit on the top edge.
    let min_y = -pitch / 2.0;
    WorldViewport {
        min_x: 0.0,
        min_y,
        max_x: width_px / scale,
        max_y: min_y + height_px / scale,
        scale,
    }
}

/// Would an `[r, g, b]` pixel be counted as ink by any of the repo's existing
/// pixel classifiers?
///
/// Four tests classify ink by pixel colour — `thumbnails.rs` `ink_bounds` and
/// `column_thickness`, Android `greenThickness`, and `e2e/tests/ink.spec.ts`.
/// The first three share the green-dominance predicate; the fourth is the
/// narrow dark-green band. Paper colours **must** fall outside all of them, or
/// drawing paper silently corrupts every existing ink assertion. Pinned by a
/// unit test so a future colour tweak fails fast.
pub fn is_ink_classified(rgb: [u8; 3]) -> bool {
    let [red, green, blue] = rgb;
    let green_dominant = green > red && green > blue;
    let dark_green_band = red < 20 && green > 70 && green < 130 && blue < 20;
    green_dominant || dark_green_band
}

/// Visit every integer multiple of `pitch` within `[min, max]`, ascending,
/// capped at [`MAX_PAPER_MARKS_PER_AXIS`].
fn visit_multiples(pitch: f64, min: f64, max: f64, mut visit: impl FnMut(f64)) {
    if !pitch.is_finite() || pitch <= 0.0 || !min.is_finite() || !max.is_finite() || max < min {
        return;
    }
    let first = (min / pitch).ceil();
    let last = (max / pitch).floor();
    if !first.is_finite() || !last.is_finite() || last < first {
        return;
    }
    // Clamp the span *before* the integer cast: a pathological viewport can put
    // `last - first` far beyond i64, which is UB-adjacent on a raw `as` cast.
    let span = (last - first).min(MAX_PAPER_MARKS_PER_AXIS as f64 - 1.0);
    let count = span as usize + 1;
    for step in 0..count {
        visit((first + step as f64) * pitch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_values_round_trip_and_default_is_none() {
        assert_eq!(Paper::default(), Paper::None);
        for paper in Paper::ALL {
            assert_eq!(Paper::from_wire(paper.wire_value()), Some(paper));
            let json = serde_json::to_string(&paper).expect("paper should serialize");
            assert_eq!(json, format!("\"{}\"", paper.wire_value()));
            let back: Paper = serde_json::from_str(&json).expect("paper should deserialize");
            assert_eq!(back, paper);
        }
        assert_eq!(Paper::from_wire("ruled-margin-huge"), Option::None);
        assert_eq!(Paper::from_wire(""), Option::None);
        assert_eq!(Paper::ALL[0], Paper::None, "None leads the palette");
    }

    #[test]
    fn geometry_constants_are_locked() {
        assert_eq!(Paper::RuledNarrow.rule_spacing(), Some(48.0));
        assert_eq!(Paper::RuledMarginNarrow.rule_spacing(), Some(48.0));
        assert_eq!(Paper::RuledWide.rule_spacing(), Some(72.0));
        assert_eq!(Paper::RuledMarginWide.rule_spacing(), Some(72.0));
        assert_eq!(Paper::SquaredSmall.rule_spacing(), Some(32.0));
        assert_eq!(Paper::SquaredLarge.rule_spacing(), Some(64.0));
        assert_eq!(Paper::None.rule_spacing(), Option::None);

        assert_eq!(Paper::SquaredSmall.column_spacing(), Some(32.0));
        assert_eq!(Paper::SquaredLarge.column_spacing(), Some(64.0));
        for paper in [
            Paper::None,
            Paper::RuledNarrow,
            Paper::RuledWide,
            Paper::RuledMarginNarrow,
            Paper::RuledMarginWide,
        ] {
            assert_eq!(paper.column_spacing(), Option::None);
        }

        let with_margin: Vec<Paper> = Paper::ALL
            .into_iter()
            .filter(|paper| paper.has_margin())
            .collect();
        assert_eq!(
            with_margin,
            vec![Paper::RuledMarginNarrow, Paper::RuledMarginWide]
        );
        assert_eq!(MARGIN_X, 96.0);
        assert_eq!(RULE_LINE_WIDTH, 1.5);
        assert_eq!(MARGIN_LINE_WIDTH, 2.0);
    }

    /// Paper must be invisible to every existing ink pixel classifier, or
    /// drawing it silently corrupts the ink assertions those tests make.
    #[test]
    fn paper_colours_are_not_classified_as_ink() {
        assert!(!is_ink_classified(RULE_COLOR_RGB));
        assert!(!is_ink_classified(MARGIN_COLOR_RGB));
        // The classifier itself is honest: canonical ink *is* classified.
        assert!(is_ink_classified([0x00, 0x64, 0x00]));
        // The two paper colours stay distinguishable from each other.
        assert_ne!(RULE_COLOR_RGB, MARGIN_COLOR_RGB);
        // Hex and component forms agree.
        let hex = |rgb: [u8; 3]| format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
        assert_eq!(hex(RULE_COLOR_RGB), RULE_COLOR);
        assert_eq!(hex(MARGIN_COLOR_RGB), MARGIN_COLOR);
        assert_eq!(PaperMarkKind::Rule.color(), RULE_COLOR);
        assert_eq!(PaperMarkKind::Column.color(), RULE_COLOR);
        assert_eq!(PaperMarkKind::Margin.color(), MARGIN_COLOR);
    }

    /// The graded cull table from the card, pinned at its boundaries.
    #[test]
    fn cull_threshold_is_graded_by_pitch() {
        for pitch in [
            RULE_SPACING_NARROW,
            RULE_SPACING_WIDE,
            GRID_SPACING_SMALL,
            GRID_SPACING_LARGE,
        ] {
            let boundary = MIN_PAPER_MARK_DEVICE_PITCH / pitch;
            assert!(paper_family_visible(pitch, boundary), "exactly 4 px draws");
            assert!(!paper_family_visible(pitch, boundary * 0.99));
            assert!(paper_family_visible(pitch, boundary * 1.01));
        }
        // At the SPA's minimum zoom the coarse families draw and the fine ones
        // are culled — the specified graded behaviour, not a bug.
        let min_canvas_scale = 0.08;
        assert!(paper_family_visible(RULE_SPACING_WIDE, min_canvas_scale));
        assert!(paper_family_visible(GRID_SPACING_LARGE, min_canvas_scale));
        assert!(!paper_family_visible(RULE_SPACING_NARROW, min_canvas_scale));
        assert!(!paper_family_visible(GRID_SPACING_SMALL, min_canvas_scale));
        // Degenerate scales never draw.
        assert!(!paper_family_visible(RULE_SPACING_WIDE, 0.0));
        assert!(!paper_family_visible(RULE_SPACING_WIDE, f64::NAN));
        assert!(!paper_family_visible(0.0, 1.0));
    }

    /// The width floor is the paper's own, not either ink floor.
    #[test]
    fn device_width_uses_the_paper_floor() {
        assert_eq!(MIN_PAPER_MARK_DEVICE_WIDTH, 1.0);
        // Far below the floor at a thumbnail-ish scale.
        assert_eq!(paper_mark_device_width(RULE_LINE_WIDTH, 0.05), 1.0);
        // Above the floor it is the honest scaled width.
        assert!((paper_mark_device_width(RULE_LINE_WIDTH, 4.0) - 6.0).abs() < 1e-12);
        assert!((paper_mark_device_width(MARGIN_LINE_WIDTH, 4.0) - 8.0).abs() < 1e-12);
        // Not the 20 px thumbnail ink floor, and not the 0.75 px render floor.
        assert!(paper_mark_device_width(RULE_LINE_WIDTH, 0.05) < 20.0);
        assert!(paper_mark_device_width(RULE_LINE_WIDTH, 0.05) > 0.75);
    }

    #[test]
    fn none_enumerates_nothing() {
        let viewport = WorldViewport::new(-1000.0, -1000.0, 1000.0, 1000.0, 1.0);
        assert!(paper_marks(Paper::None, &viewport).is_empty());
    }

    /// Draw order is horizontal rules ascending, then columns ascending, then
    /// the margin last — and positions are exact multiples anchored at world 0,
    /// including negatives.
    #[test]
    fn marks_are_ordered_anchored_and_cover_negatives() {
        let viewport = WorldViewport::new(-100.0, -100.0, 100.0, 100.0, 1.0);

        let ruled = paper_marks(Paper::RuledMarginNarrow, &viewport);
        let rules: Vec<f64> = ruled
            .iter()
            .filter(|mark| mark.kind == PaperMarkKind::Rule)
            .map(|mark| mark.position)
            .collect();
        assert_eq!(rules, vec![-96.0, -48.0, 0.0, 48.0, 96.0]);
        assert_eq!(
            ruled.last().map(|mark| (mark.kind, mark.position)),
            Some((PaperMarkKind::Margin, MARGIN_X)),
            "the margin is drawn last"
        );
        assert!(
            ruled.iter().all(|mark| mark.kind != PaperMarkKind::Column),
            "ruled papers have no vertical grid"
        );

        let squared = paper_marks(Paper::SquaredSmall, &viewport);
        let rule_end = squared
            .iter()
            .position(|mark| mark.kind == PaperMarkKind::Column)
            .expect("squared paper has columns");
        assert!(
            squared[..rule_end]
                .iter()
                .all(|mark| mark.kind == PaperMarkKind::Rule),
            "all rules precede all columns"
        );
        let columns: Vec<f64> = squared[rule_end..]
            .iter()
            .map(|mark| mark.position)
            .collect();
        assert_eq!(columns, vec![-96.0, -64.0, -32.0, 0.0, 32.0, 64.0, 96.0]);
        assert!(
            squared
                .iter()
                .all(|mark| mark.kind != PaperMarkKind::Margin),
            "squared paper has no margin"
        );

        // Positions are exact integer multiples, so f32 narrowing is lossless.
        for mark in paper_marks(Paper::RuledWide, &viewport) {
            assert_eq!(mark.position as f32 as f64, mark.position);
            assert_eq!(mark.position % RULE_SPACING_WIDE, 0.0);
        }
    }

    /// A family can be culled while its siblings survive: at a scale where 32
    /// world units fall under 4 device px, `squared-small` draws nothing but a
    /// margin paper's never-culled margin still draws.
    #[test]
    fn families_are_culled_independently_and_margin_survives() {
        // 0.1 device px per world unit: 32*0.1 = 3.2 px (culled),
        // 72*0.1 = 7.2 px (drawn).
        let viewport = WorldViewport::new(-500.0, -500.0, 500.0, 500.0, 0.1);
        assert!(paper_marks(Paper::SquaredSmall, &viewport).is_empty());
        assert!(!paper_marks(Paper::SquaredLarge, &viewport).is_empty());

        // Rules culled, margin still drawn — a single line cannot alias.
        let far = WorldViewport::new(-5000.0, -5000.0, 5000.0, 5000.0, 0.001);
        let marks = paper_marks(Paper::RuledMarginNarrow, &far);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].kind, PaperMarkKind::Margin);

        // …and clipped away entirely when the viewport does not reach it.
        let off_margin = WorldViewport::new(500.0, -100.0, 900.0, 100.0, 0.001);
        assert!(paper_marks(Paper::RuledMarginNarrow, &off_margin).is_empty());
    }

    /// A pathological viewport must terminate at the per-axis cap rather than
    /// enumerating for ever.
    #[test]
    fn pathological_viewport_hits_the_axis_cap() {
        let viewport = WorldViewport::new(-1e12, -1e12, 1e12, 1e12, 1.0);
        let marks = paper_marks(Paper::SquaredSmall, &viewport);
        assert_eq!(marks.len(), 2 * MAX_PAPER_MARKS_PER_AXIS);

        // Non-finite bounds enumerate nothing instead of looping.
        let broken = WorldViewport::new(f64::NEG_INFINITY, 0.0, f64::INFINITY, 10.0, 1.0);
        assert!(paper_marks(Paper::SquaredSmall, &broken).is_empty());
        let inverted = WorldViewport::new(100.0, 100.0, -100.0, -100.0, 1.0);
        assert!(paper_marks(Paper::RuledNarrow, &inverted).is_empty());
        let no_scale = WorldViewport::new(-100.0, -100.0, 100.0, 100.0, 0.0);
        assert!(paper_marks(Paper::RuledNarrow, &no_scale).is_empty());
    }

    /// Every paper at every plausible swatch size must clear the cull, or the
    /// palette icon renders blank.
    #[test]
    fn preview_viewport_never_culls_and_shows_marks() {
        for paper in Paper::ALL {
            for (width, height) in [
                (24.0, 18.0),
                (32.0, 24.0),
                (88.0, 66.0),
                (128.0, 96.0),
                (256.0, 192.0),
            ] {
                let viewport = preview_viewport(paper, width, height);
                assert!(viewport.scale > 0.0 && viewport.scale.is_finite());
                if paper == Paper::None {
                    assert!(paper_marks(paper, &viewport).is_empty());
                    continue;
                }
                for pitch in [paper.rule_spacing(), paper.column_spacing()]
                    .into_iter()
                    .flatten()
                {
                    assert!(
                        paper_family_visible(pitch, viewport.scale),
                        "{} culled in a {width}×{height} swatch",
                        paper.wire_value()
                    );
                }
                let marks = paper_marks(paper, &viewport);
                assert!(
                    !marks.is_empty(),
                    "{} swatch is blank at {width}×{height}",
                    paper.wire_value()
                );
                if paper.has_margin() {
                    assert!(
                        marks.iter().any(|mark| mark.kind == PaperMarkKind::Margin),
                        "{} swatch should show its margin",
                        paper.wire_value()
                    );
                }
            }
            // Degenerate sizes stay finite rather than producing NaN geometry.
            let degenerate = preview_viewport(paper, 0.0, 0.0);
            assert!(degenerate.scale.is_finite() && degenerate.scale > 0.0);
            assert!(paper_marks(paper, &degenerate).len() <= 1);
        }
    }
}
