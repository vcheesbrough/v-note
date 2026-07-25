package link.desync.vnote.ink

/**
 * Page paper (rule lines) — the Kotlin mirror of `protocol::paper`.
 *
 * **This must stay identical to `crates/protocol/src/paper.rs`.**
 * `docs/PLAN.md` §Explicit non-goals forbids a shared cross-platform render
 * library ("two canvas implementations, one protocol spec"), so parity is
 * enforced the way the pressure→width curve is: one pure spec in Rust, mirrored
 * here, and pinned by the shared golden fixture
 * `contracts/fixtures/paper-geometry.json`, which `PaperGeometryTest` asserts
 * from this side and `crates/protocol/tests/contracts.rs` asserts from the
 * other. Change one without the other and both test suites fail.
 *
 * Marks are anchored at world 0 (`position = k * pitch`, `k` any integer
 * including negatives), so paper is fixed relative to the ink and rides the
 * ink's transform through pan and zoom. Every pitch is an integer, so positions
 * are exact integer multiples and the narrowing to `Float` is lossless while
 * `|position| < 2^24` ([MAX_EXACT_PAPER_WORLD_EXTENT]).
 */

internal const val RULE_SPACING_NARROW = 48.0
internal const val RULE_SPACING_WIDE = 72.0
internal const val GRID_SPACING_SMALL = 32.0
internal const val GRID_SPACING_LARGE = 64.0
internal const val MARGIN_X = 96.0

internal const val RULE_LINE_WIDTH = 1.5
internal const val MARGIN_LINE_WIDTH = 2.0

internal const val RULE_COLOR = "#B0C4DE"
internal const val MARGIN_COLOR = "#E06C6C"

/**
 * Minimum on-screen pitch in device pixels below which a mark *family* is culled
 * rather than aliased into a moiré wash. Evaluated per family, so coarse papers
 * survive further out than fine ones. The margin has no pitch to alias against
 * and is never culled.
 */
internal const val MIN_PAPER_MARK_DEVICE_PITCH = 4.0

/**
 * Minimum rendered mark width in device pixels — paper's own floor, deliberately
 * neither ink floor.
 */
internal const val MIN_PAPER_MARK_DEVICE_WIDTH = 1.0

/** Provably-unreachable per-axis safety net against a pathological viewport. */
internal const val MAX_PAPER_MARKS_PER_AXIS = 4096

/** `2^24` — the largest magnitude at which an integer world position survives narrowing to `Float`. */
internal const val MAX_EXACT_PAPER_WORLD_EXTENT = 16_777_216.0

/** Rows of rules a palette swatch aims to show; see [previewViewport]. */
private const val PREVIEW_ROWS = 4.0

/** Headroom over [MIN_PAPER_MARK_DEVICE_PITCH] applied by [previewViewport]. */
private const val PREVIEW_CULL_HEADROOM = 1.25

enum class Paper(
    val wireValue: String,
    val label: String,
    /** Pitch of the horizontal rule family in world units, or null when there is none. */
    val ruleSpacing: Double?,
    /** Pitch of the vertical grid family; only the squared papers have one. */
    val columnSpacing: Double?,
    /** True when this paper carries the single red margin rule at [MARGIN_X]. */
    val hasMargin: Boolean,
) {
    None("none", "None", null, null, false),
    RuledMarginNarrow("ruled-margin-narrow", "Ruled, margin, narrow", RULE_SPACING_NARROW, null, true),
    RuledMarginWide("ruled-margin-wide", "Ruled, margin, wide", RULE_SPACING_WIDE, null, true),
    RuledNarrow("ruled-narrow", "Ruled, narrow", RULE_SPACING_NARROW, null, false),
    RuledWide("ruled-wide", "Ruled, wide", RULE_SPACING_WIDE, null, false),
    SquaredSmall("squared-small", "Squared, small", GRID_SPACING_SMALL, GRID_SPACING_SMALL, false),
    SquaredLarge("squared-large", "Squared, large", GRID_SPACING_LARGE, GRID_SPACING_LARGE, false),
    ;

    companion object {
        /** Every choice, in palette order — the same order as `Paper::ALL` in Rust. */
        val ALL: List<Paper> = entries.toList()

        /**
         * Parse a wire value. Returns null for anything unrecognised, so a typo
         * is rejected rather than silently rendering as a blank page.
         */
        fun fromWire(value: String?): Paper? = ALL.firstOrNull { it.wireValue == value }
    }
}

internal enum class PaperMarkKind(
    val worldWidth: Double,
    val color: String,
    /** True for a full-width horizontal line (position is a world y). */
    val isHorizontal: Boolean,
) {
    Rule(RULE_LINE_WIDTH, RULE_COLOR, true),
    Column(RULE_LINE_WIDTH, RULE_COLOR, false),
    Margin(MARGIN_LINE_WIDTH, MARGIN_COLOR, false),
}

/** One paper line to draw, spanning the viewport perpendicular to [position]. */
internal data class PaperMark(
    val kind: PaperMarkKind,
    /** World y for [PaperMarkKind.Rule], world x otherwise. */
    val position: Double,
) {
    val worldWidth: Double get() = kind.worldWidth
}

/**
 * The world-space rectangle about to be painted, plus device pixels per world
 * unit. Derived by inverting the *same* transform applied to ink.
 */
internal data class PaperViewport(
    val minX: Double,
    val minY: Double,
    val maxX: Double,
    val maxY: Double,
    val scale: Double,
) {
    val isDrawable: Boolean
        get() =
            minX.isFinite() && minY.isFinite() && maxX.isFinite() && maxY.isFinite() &&
                scale.isFinite() && scale > 0.0 && maxX >= minX && maxY >= minY
}

/** Whether a family with [pitchWorld] spacing is dense enough to draw at [scale]. */
internal fun paperFamilyVisible(
    pitchWorld: Double,
    scale: Double,
): Boolean =
    pitchWorld.isFinite() && scale.isFinite() && pitchWorld > 0.0 && scale > 0.0 &&
        pitchWorld * scale >= MIN_PAPER_MARK_DEVICE_PITCH

/** Device-space stroke width for a mark, floored at [MIN_PAPER_MARK_DEVICE_WIDTH]. */
internal fun paperMarkDeviceWidth(
    worldWidth: Double,
    scale: Double,
): Double = maxOf(worldWidth * scale, MIN_PAPER_MARK_DEVICE_WIDTH)

/**
 * Enumerate the marks for [paper] over [viewport] without allocating a list.
 *
 * Order is deterministic and is the draw order: horizontal rules ascending, then
 * vertical grid ascending, then the margin last — all of it behind every stroke.
 */
internal inline fun visitPaperMarks(
    paper: Paper,
    viewport: PaperViewport,
    visit: (PaperMark) -> Unit,
) {
    if (paper == Paper.None || !viewport.isDrawable) {
        return
    }
    paper.ruleSpacing?.let { pitch ->
        if (paperFamilyVisible(pitch, viewport.scale)) {
            visitMultiples(pitch, viewport.minY, viewport.maxY) { visit(PaperMark(PaperMarkKind.Rule, it)) }
        }
    }
    paper.columnSpacing?.let { pitch ->
        if (paperFamilyVisible(pitch, viewport.scale)) {
            visitMultiples(pitch, viewport.minX, viewport.maxX) { visit(PaperMark(PaperMarkKind.Column, it)) }
        }
    }
    // A single line has no pitch to alias against, so the margin is never culled
    // — only clipped out when the viewport does not reach it.
    if (paper.hasMargin && viewport.minX <= MARGIN_X && MARGIN_X <= viewport.maxX) {
        visit(PaperMark(PaperMarkKind.Margin, MARGIN_X))
    }
}

/** Allocating form of [visitPaperMarks], for tests and non-hot paths. */
internal fun paperMarks(
    paper: Paper,
    viewport: PaperViewport,
): List<PaperMark> {
    val marks = mutableListOf<PaperMark>()
    visitPaperMarks(paper, viewport) { marks.add(it) }
    return marks
}

/**
 * A viewport for a palette swatch of [widthPx] × [heightPx] device pixels,
 * scaled so the paper is never culled — otherwise a 32×24.dp swatch of 48-unit
 * rules falls under the density cull and the icon renders blank.
 */
internal fun previewViewport(
    paper: Paper,
    widthPx: Double,
    heightPx: Double,
): PaperViewport {
    val width = if (widthPx.isFinite() && widthPx > 0.0) widthPx else 0.0
    val height = if (heightPx.isFinite() && heightPx > 0.0) heightPx else 0.0
    // Paper.None draws nothing; the narrow pitch is an arbitrary stand-in that
    // keeps the returned scale positive and finite for callers.
    val pitch = paper.ruleSpacing ?: RULE_SPACING_NARROW
    val natural = height / (pitch * PREVIEW_ROWS)
    val cullFloor = MIN_PAPER_MARK_DEVICE_PITCH * PREVIEW_CULL_HEADROOM / pitch
    val scale = maxOf(natural, cullFloor)
    // Half a pitch of lead-in so the first rule does not sit on the top edge.
    val minY = -pitch / 2.0
    return PaperViewport(
        minX = 0.0,
        minY = minY,
        maxX = width / scale,
        maxY = minY + height / scale,
        scale = scale,
    )
}

/** Visit every integer multiple of [pitch] within `[min, max]`, ascending, capped. */
internal inline fun visitMultiples(
    pitch: Double,
    min: Double,
    max: Double,
    visit: (Double) -> Unit,
) {
    if (!pitch.isFinite() || pitch <= 0.0 || !min.isFinite() || !max.isFinite() || max < min) {
        return
    }
    val first = kotlin.math.ceil(min / pitch)
    val last = kotlin.math.floor(max / pitch)
    if (!first.isFinite() || !last.isFinite() || last < first) {
        return
    }
    // Clamp the span before the integer conversion: a pathological viewport can
    // put `last - first` far beyond Int range.
    val span = minOf(last - first, MAX_PAPER_MARKS_PER_AXIS - 1.0)
    val count = span.toInt() + 1
    for (step in 0 until count) {
        visit((first + step) * pitch)
    }
}
